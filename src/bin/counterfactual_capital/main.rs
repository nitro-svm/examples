//! Counterfactual Capital: rewriting what a venue holds and how tightly it quotes, priced against the hops that already happened.

mod capture;
mod cli;
mod jsonl;
mod market;
mod plan;
mod report;
mod session;
mod vault;

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::{AccountData, RerouteStatsReport};
use simulator_client::{ManagedBacktestSession, ManagedEvent, backtest_ws_url};
use solana_account::Account;
use solana_address::Address;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use backtest_example::utils;

use crate::{
    cli::{ArmSpec, Cli},
    jsonl::{ArmRow, TierRow, VaultRow},
    plan::Plan,
    session::Arm,
};

/// Advance a session whose first pause the caller already consumed, printing slots as they replay.
/// An arm that advanced and reported no census priced nothing, which is an error rather than a
/// zero.
async fn advance(
    session: &mut ManagedBacktestSession,
    slot_count: u64,
) -> Result<RerouteStatsReport> {
    utils::session::resume_to_completion(session, slot_count, |event| {
        if let ManagedEvent::Slot(slot) = event {
            eprintln!("[slot] {slot}");
        }
    })
    .await?
    .context("the session completed without reroute stats, so the arm priced nothing")
}

/// `decimals` in the SPL mint layout, after a 36-byte COption mint authority and an 8-byte supply.
const MINT_DECIMALS_OFFSET: usize = 44;

/// A vault, its baseline, and what its mint calls the units.
struct Vault {
    address: Address,
    account: Account,
    mint: Address,
    decimals: u8,
}

/// The venue as it stood at the start slot: what every arm is a multiple of.
struct Venue {
    vaults: Vec<Vault>,
    state: Option<(Address, Account)>,
}

impl Venue {
    /// Every account an arm rewrites, keyed for threading through the trajectory.
    fn accounts(&self) -> HashMap<Address, Account> {
        self.vaults
            .iter()
            .map(|vault| (vault.address, vault.account.clone()))
            .chain(self.state.clone())
            .collect()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Cli::parse();
    let plan = Plan::read(&args.plan)?;
    let arms = args.arms()?;

    eprintln!(
        "[venue] {} via {}, pair {}",
        plan.direct_fill.venue,
        plan.direct_fill.aggregator.as_str(),
        plan.direct_fill.pair
    );
    eprintln!(
        "[range] {} + {} slots, {}",
        args.start_slot,
        args.slot_count,
        match args.no_replay {
            true => "executed",
            false => "replayed",
        }
    );
    eprintln!(
        "[arms] {} — {}",
        arms.len(),
        arms.iter()
            .map(ArmSpec::label)
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Before any session: a plan's mistakes cost a full replay per arm to discover otherwise.
    if args.dry_run {
        return report::dry_run(&arms, &plan);
    }

    // Every other arm is built from what this pass records: the venue's own state trajectory.
    let (venue, trajectory, reference) = capture_pass(&args, &plan).await?;

    // Written as each arm lands: a failure on the last must not discard every one before it.
    let mut rows = Vec::new();
    for arm in &arms {
        rows.push(run_arm(&args, &plan, &venue, &trajectory, *arm).await?);
        jsonl::write_rows(&args.out, &rows, reference.as_ref())?;
    }

    report::arms(&rows, reference.as_ref(), &venue);
    Ok(())
}

/// Read the venue at the start slot and record how it moved: the reference measurement, and the
/// source of every later arm's overrides.
///
/// Given a capture the range is not replayed here at all — the session opens only far enough to
/// read the baseline every arm is a multiple of, and the recorded trajectory stands in for the
/// pass. That trades the reference census, which the 1x arm is checked against, for a whole replay.
async fn capture_pass(
    args: &Cli,
    plan: &Plan,
) -> Result<(Venue, capture::Trajectory, Option<ArmRow>)> {
    let create = session::create_session(Arm {
        start_slot: args.start_slot,
        slot_count: args.slot_count,
        no_replay: args.no_replay,
        spec: plan.direct_fill.clone(),
        overrides: Vec::new(),
    })?;
    let mut open = ManagedBacktestSession::start(
        backtest_ws_url(&args.conn.url),
        args.conn.api_key.clone(),
        create,
    )
    .await?;
    let rpc_url = open.session_info().rpc_endpoint.clone();

    // Read at the pause: this session's RPC stops serving the moment the session completes, so a
    // read deferred until the pass has finished answers 502.
    utils::session::wait_for_first_pause(&mut open).await?;
    let read = session::read_accounts(&rpc_url, &plan.overridden()).await?;
    let venue = resolve(&rpc_url, plan, read).await?;
    report::baseline(&venue);

    let watching = plan.overridden();
    let reused = args.capture.as_deref().filter(|path| path.exists());
    if let Some(path) = reused {
        let trajectory = capture::load(path)?;
        capture::require_changes(&trajectory, &watching)?;
        eprintln!(
            "[capture] {} slots read from {} — the range is not replayed for the reference",
            trajectory.slots(),
            path.display()
        );
        open.shutdown().await;
        return Ok((venue, trajectory, None));
    }

    let capturing = capture::start(&rpc_url, &watching).await?;
    eprintln!("[arm] unfrozen (the venue priced as it actually moved — the reference)");
    let stats = advance(&mut open, args.slot_count).await?;
    let trajectory = capture::finish(capturing).await?;
    capture::require_changes(&trajectory, &watching)?;
    eprintln!(
        "[capture] {} slots carry a change to the venue's state — one override each",
        trajectory.slots()
    );
    if let Some(path) = &args.capture {
        capture::save(path, &trajectory)?;
        eprintln!("[capture] written to {}", path.display());
    }
    open.shutdown().await;

    Ok((
        venue,
        trajectory,
        Some(row(ArmSpec::CONTROL, false, None, stats)),
    ))
}

/// Pair each address read back with what it is, so the report can name units rather than bytes.
async fn resolve(rpc_url: &str, plan: &Plan, read: Vec<(Address, Account)>) -> Result<Venue> {
    let found = |address: &Address| {
        read.iter()
            .find(|(candidate, _)| candidate == address)
            .map(|(_, account)| account.clone())
            .with_context(|| format!("{address} was not read back at the start slot"))
    };

    let vault_accounts = plan
        .inventory
        .vaults
        .iter()
        .map(|address| Ok((*address, found(address)?)))
        .collect::<Result<Vec<_>>>()?;

    let mints = vault_accounts
        .iter()
        .map(|(address, account)| {
            let mint: [u8; 32] = account
                .data
                .get(0..32)
                .and_then(|slice| slice.try_into().ok())
                .with_context(|| format!("vault {address} is too short to name a mint"))?;
            Ok(Address::from(mint))
        })
        .collect::<Result<Vec<_>>>()?;
    let mint_accounts = RpcClient::new(rpc_url.to_string())
        .get_multiple_accounts(&mints)
        .await
        .context("reading the vaults' mints for their decimals")?;

    let vaults = vault_accounts
        .into_iter()
        .zip(mints)
        .zip(mint_accounts)
        .map(|(((address, account), mint), mint_account)| {
            let decimals = mint_account
                .and_then(|mint| mint.data.get(MINT_DECIMALS_OFFSET).copied())
                .with_context(|| format!("mint {mint} does not declare decimals"))?;
            Ok(Vault {
                address,
                account,
                mint,
                decimals,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let state = plan
        .inventory
        .state
        .as_ref()
        .map(|layout| Ok::<_, anyhow::Error>((layout.account, found(&layout.account)?)))
        .transpose()?;
    Ok(Venue { vaults, state })
}

/// What one arm posts, and what it changed.
struct Posted {
    /// One entry per slot the venue's state moved in, scaled.
    overrides: Vec<(u64, BTreeMap<Address, AccountData>)>,
    /// The start-slot view: what the venue held before the range ran.
    vaults: Vec<VaultRow>,
    tiers: Vec<TierRow>,
    /// The deepest tier this arm quotes, which is the ceiling on a single trade. `None` for a
    /// venue with no ladder, whose ceiling is its vault balance.
    ceiling: Option<u128>,
}

/// One snapshot of the venue, scaled: the bytes to post and what changed inside them.
struct Scaled {
    accounts: BTreeMap<Address, AccountData>,
    vaults: Vec<VaultRow>,
    tiers: Vec<TierRow>,
    ceiling: Option<u128>,
}

/// Takes the whole snapshot rather than the changed account alone: scaling the state account
/// asserts its balance mirrors against the vaults, which must be current at that slot.
fn scale_at(
    plan: &Plan,
    venue: &Venue,
    snapshot: &HashMap<Address, Account>,
    arm: ArmSpec,
) -> Result<Scaled> {
    let scaled_vaults = venue
        .vaults
        .iter()
        .map(|vault| {
            let account = snapshot
                .get(&vault.address)
                .with_context(|| format!("vault {} is missing from the snapshot", vault.address))?;
            vault::scale(account, arm.capital())
                .with_context(|| format!("scaling vault {} by {}", vault.address, arm.label()))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut accounts = venue
        .vaults
        .iter()
        .zip(&scaled_vaults)
        .map(|(vault, scaled)| (vault.address, scaled.account.clone()))
        .collect::<BTreeMap<_, _>>();

    let vaults = venue
        .vaults
        .iter()
        .zip(&scaled_vaults)
        .map(|(vault, scaled)| VaultRow {
            address: vault.address.to_string(),
            mint: vault.mint.to_string(),
            decimals: vault.decimals,
            before: scaled.before,
            after: scaled.after,
            native: scaled.native,
            lamports: scaled.lamports,
        })
        .collect();

    let Some((address, _)) = &venue.state else {
        return Ok(Scaled {
            accounts,
            vaults,
            tiers: Vec::new(),
            ceiling: None,
        });
    };
    let account = snapshot
        .get(address)
        .with_context(|| format!("state account {address} is missing from the snapshot"))?;
    let layout = plan
        .inventory
        .state
        .as_ref()
        .expect("state implies a layout");
    let amounts = scaled_vaults
        .iter()
        .map(|scaled| scaled.before)
        .collect::<Vec<_>>();
    let scaled = market::scale(
        account,
        layout,
        &amounts,
        arm.capital(),
        arm.depth(),
        arm.tighten_bps,
    )
    .with_context(|| format!("scaling the venue's curve by {}", arm.label()))?;

    let tiers =
        scaled
            .before
            .iter()
            .zip(&scaled.after)
            .enumerate()
            .flat_map(|(side, (before, after))| {
                before.tiers.iter().zip(&after.tiers).enumerate().map(
                    move |(tier, (before, after))| TierRow {
                        side,
                        tier,
                        price_before: before.price.to_string(),
                        price_after: after.price.to_string(),
                        size_before: before.size.to_string(),
                        size_after: after.size.to_string(),
                    },
                )
            })
            .collect();
    let ceiling = scaled.after.iter().map(market::Ladder::top_size).max();
    accounts.insert(*address, scaled.account);
    Ok(Scaled {
        accounts,
        vaults,
        tiers,
        ceiling,
    })
}

/// The whole schedule one arm posts: the venue's real trajectory, scaled slot by slot. With
/// `--pin-at-start` it collapses to one entry pinning the venue's state for the entire range.
fn schedule(
    args: &Cli,
    plan: &Plan,
    venue: &Venue,
    trajectory: &capture::Trajectory,
    arm: ArmSpec,
) -> Result<Posted> {
    let start = venue.accounts();
    let opening = scale_at(plan, venue, &start, arm)?;
    let mut overrides = vec![(args.start_slot, opening.accounts.clone())];

    // `active_at` folds every entry up to a slot, so an unchanged account is already standing.
    let mut unscalable = 0u64;
    for (slot, changed, snapshot) in capture::walk(&start, trajectory) {
        // Skipped rather than fatal: the previous override stays standing. Counted, because many
        // skips mean the arm is not measuring the trajectory it claims.
        let Ok(scaled) = scale_at(plan, venue, &snapshot, arm) else {
            unscalable += 1;
            continue;
        };
        let moved = changed
            .into_iter()
            .filter_map(|key| Some((key, scaled.accounts.get(&key)?.clone())))
            .collect::<BTreeMap<_, _>>();
        if moved.is_empty() {
            continue;
        }
        overrides.push((slot, moved));
    }
    if unscalable > 0 {
        eprintln!(
            "[warn] {unscalable} of {} captured slots could not be scaled and were skipped; the \
             override standing from the previous slot applied instead",
            trajectory.slots()
        );
    }

    Ok(Posted {
        overrides,
        vaults: opening.vaults,
        tiers: opening.tiers,
        ceiling: opening.ceiling,
    })
}

async fn run_arm(
    args: &Cli,
    plan: &Plan,
    venue: &Venue,
    trajectory: &capture::Trajectory,
    arm: ArmSpec,
) -> Result<ArmRow> {
    let posted = schedule(args, plan, venue, trajectory, arm)?;
    eprintln!(
        "[arm] {} — {} overrides",
        arm.label(),
        posted.overrides.len()
    );
    let create = session::create_session(Arm {
        start_slot: args.start_slot,
        slot_count: args.slot_count,
        no_replay: args.no_replay,
        spec: plan.direct_fill.clone(),
        overrides: posted.overrides.clone(),
    })?;
    let mut open = ManagedBacktestSession::start(
        backtest_ws_url(&args.conn.url),
        args.conn.api_key.clone(),
        create,
    )
    .await?;
    utils::session::wait_for_first_pause(&mut open).await?;
    let stats = advance(&mut open, args.slot_count).await?;
    open.shutdown().await;
    Ok(row(arm, true, Some(&posted), stats))
}

fn row(arm: ArmSpec, frozen: bool, posted: Option<&Posted>, stats: RerouteStatsReport) -> ArmRow {
    ArmRow {
        multiple: arm.multiple,
        tighten_bps: arm.tighten_bps,
        scale: arm.target,
        frozen,
        vaults: posted.map(|p| p.vaults.clone()).unwrap_or_default(),
        tiers: posted.map(|p| p.tiers.clone()).unwrap_or_default(),
        ceiling: posted.and_then(|p| p.ceiling).map(|top| top.to_string()),
        matched: stats.direct_fill_matched,
        built: stats.direct_fill_built,
        scored: stats.direct_fill_scored,
        bps_total: stats.direct_fill_bps_total,
        rejections: stats.direct_fill_rejections,
        outcomes: stats.direct_fill_outcomes,
    }
}

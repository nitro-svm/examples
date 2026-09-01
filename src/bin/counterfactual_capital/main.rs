//! Counterfactual Capital: rewriting what a venue holds and how tightly it quotes, priced against the hops that already happened.

mod capture;
mod cli;
mod jsonl;
mod market;
mod plan;
mod session;
mod vault;

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::{AccountData, RerouteStatsReport};
use simulator_client::ManagedBacktestSession;
use solana_account::Account;
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use crate::{
    cli::{ArmSpec, Cli},
    jsonl::{ArmRow, FILLED, TierRow, VaultRow},
    plan::Plan,
    session::Arm,
};

/// `decimals` in the SPL mint layout, after a 36-byte COption mint authority and an 8-byte supply.
const MINT_DECIMALS_OFFSET: usize = 44;

/// Above this, an arm that should have reproduced the reference exactly did not. Half the venue's
/// own half-spread.
const REFERENCE_GAP_WARN_BPS: f64 = 0.5;

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
        arms.iter().map(ArmSpec::label).collect::<Vec<_>>().join(", ")
    );

    // Before any session: a plan's mistakes cost a full replay per arm to discover otherwise.
    if args.dry_run {
        return report_dry_run(&arms, &plan);
    }

    // Every other arm is built from what this pass records: the venue's own state trajectory.
    let (venue, trajectory, reference) = capture_pass(&args, &plan).await?;
    let reference = Some(reference);

    // Written as each arm lands: a failure on the last must not discard every one before it.
    let mut rows = Vec::new();
    for arm in &arms {
        rows.push(run_arm(&args, &plan, &venue, &trajectory, *arm).await?);
        write_rows(&args, &rows, reference.as_ref())?;
    }

    report(&rows, reference.as_ref(), &venue);
    Ok(())
}

/// Replay the range once with nothing overridden: the reference measurement, and the source of
/// every later arm's overrides.
async fn capture_pass(args: &Cli, plan: &Plan) -> Result<(Venue, capture::Trajectory, ArmRow)> {
    let create = session::create_session(Arm {
        start_slot: args.start_slot,
        slot_count: args.slot_count,
        no_replay: args.no_replay,
        spec: plan.direct_fill.clone(),
        overrides: Vec::new(),
    })?;
    let mut open = ManagedBacktestSession::start(
        args.conn.websocket_url(),
        args.conn.api_key.clone(),
        create,
    )
    .await?;
    let rpc_url = args.conn.rpc_url(&open.session_info().rpc_endpoint);

    // Read at the pause: this session's RPC stops serving the moment the session completes, so a
    // read deferred until the pass has finished answers 502.
    session::wait_for_first_pause(&mut open).await?;
    let read = session::read_accounts(&rpc_url, &plan.overridden()).await?;
    let venue = resolve(&rpc_url, plan, read).await?;
    report_baseline(&venue);

    let watching = plan.overridden();
    let capturing = capture::start(&rpc_url, &watching).await?;
    eprintln!("[arm] unfrozen (the venue priced as it actually moved — the reference)");
    let stats = session::advance_to_completion(&mut open, args.slot_count).await?;

    let trajectory = capturing.finish().await?;
    capture::require_changes(&trajectory, &watching)?;
    eprintln!(
        "[capture] {} slots carry a change to the venue's state — one override each",
        trajectory.slots()
    );
    Ok((venue, trajectory, row(ArmSpec::CONTROL, false, None, stats)))
}

/// Pair each address read back with what it is, so the report can name units rather than bytes.
async fn resolve(
    rpc_url: &str,
    plan: &Plan,
    read: Vec<(Address, Account)>,
) -> Result<Venue> {
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
            Address::from(mint)
                .to_string()
                .parse::<Pubkey>()
                .map_err(anyhow::Error::new)
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
                mint: mint.to_string().parse()?,
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
        return Ok(Scaled { accounts, vaults, tiers: Vec::new(), ceiling: None });
    };
    let account = snapshot
        .get(address)
        .with_context(|| format!("state account {address} is missing from the snapshot"))?;
    let layout = plan.inventory.state.as_ref().expect("state implies a layout");
    let amounts = scaled_vaults.iter().map(|scaled| scaled.before).collect::<Vec<_>>();
    let scaled = market::scale(
        account,
        layout,
        &amounts,
        arm.capital(),
        arm.depth(),
        arm.tighten_bps,
    )
    .with_context(|| format!("scaling the venue's curve by {}", arm.label()))?;

    let tiers = scaled
        .before
        .iter()
        .zip(&scaled.after)
        .enumerate()
        .flat_map(|(side, (before, after))| {
            before
                .tiers
                .iter()
                .zip(&after.tiers)
                .enumerate()
                .map(move |(tier, (before, after))| TierRow {
                    side,
                    tier,
                    price_before: before.price.to_string(),
                    price_after: after.price.to_string(),
                    size_before: before.size.to_string(),
                    size_after: after.size.to_string(),
                })
        })
        .collect();
    let ceiling = scaled.after.iter().map(market::Ladder::top_size).max();
    accounts.insert(*address, scaled.account);
    Ok(Scaled { accounts, vaults, tiers, ceiling })
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
    eprintln!("[arm] {} — {} overrides", arm.label(), posted.overrides.len());
    let create = session::create_session(Arm {
        start_slot: args.start_slot,
        slot_count: args.slot_count,
        no_replay: args.no_replay,
        spec: plan.direct_fill.clone(),
        overrides: posted.overrides.clone(),
    })?;
    let mut open = ManagedBacktestSession::start(
        args.conn.websocket_url(),
        args.conn.api_key.clone(),
        create,
    )
    .await?;
    session::wait_for_first_pause(&mut open).await?;
    let stats = session::advance_to_completion(&mut open, args.slot_count).await?;
    Ok(row(arm, true, Some(&posted), stats))
}

fn row(
    arm: ArmSpec,
    frozen: bool,
    posted: Option<&Posted>,
    stats: RerouteStatsReport,
) -> ArmRow {
    ArmRow {
        multiple: arm.multiple,
        tighten_bps: arm.tighten_bps,
        scale: arm.target.as_str().to_owned(),
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

fn format_amount(base_units: u64, decimals: u8) -> String {
    let scaled = base_units as f64 / 10f64.powi(i32::from(decimals));
    match scaled >= 1000.0 {
        true => format!("{scaled:.0}"),
        false => format!("{scaled:.2}"),
    }
}

fn short(address: &str) -> String {
    match address.len() > 10 {
        true => format!("{}..{}", &address[..6], &address[address.len() - 4..]),
        false => address.to_string(),
    }
}

fn report_baseline(venue: &Venue) {
    for vault in &venue.vaults {
        // Read rather than indexed: nothing has validated this buffer as a token account yet.
        let Some(amount) = vault
            .account
            .data
            .get(64..72)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
        else {
            eprintln!(
                "[warn] {} is {} bytes, too short to be a token account",
                short(&vault.address.to_string()),
                vault.account.data.len()
            );
            continue;
        };
        eprintln!(
            "[baseline] {} holds {} of {}",
            short(&vault.address.to_string()),
            format_amount(amount, vault.decimals),
            short(&vault.mint.to_string())
        );
    }
}

/// What the run would do, without opening a session; the plan is validated before this prints.
fn report_dry_run(arms: &[ArmSpec], plan: &Plan) -> Result<()> {
    println!(
        "{} arms against {} via {}:",
        arms.len(),
        plan.direct_fill.venue,
        plan.direct_fill.aggregator.as_str()
    );
    for arm in arms {
        println!(
            "  {:>16}  vaults x{}  ladder sizes x{}  prices -{} bps",
            arm.label(),
            arm.capital(),
            arm.depth(),
            arm.tighten_bps
        );
    }
    println!();
    for vault in &plan.inventory.vaults {
        println!("would rewrite the balance of vault {vault}");
    }
    let Some(state) = &plan.inventory.state else {
        println!("no state account named: only the vaults would be rewritten");
        return Ok(());
    };
    println!(
        "would rewrite {} ladder(s) and {} balance mirror(s) in {}",
        state.ladders.len(),
        state.balance_mirrors.len(),
        state.account
    );
    Ok(())
}

fn write_rows(args: &Cli, rows: &[ArmRow], diagnostic: Option<&ArmRow>) -> Result<()> {
    let body = rows
        .iter()
        .chain(diagnostic)
        .map(|row| serde_json::to_string(row).map_err(anyhow::Error::new))
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    std::fs::write(&args.out, format!("{body}\n"))
        .with_context(|| format!("writing {}", args.out.display()))?;
    eprintln!(
        "[out] {} arms written to {}",
        rows.len() + usize::from(diagnostic.is_some()),
        args.out.display()
    );
    Ok(())
}

/// The table, and every guard that separates a real curve from a broken one.
fn report(rows: &[ArmRow], reference: Option<&ArmRow>, venue: &Venue) {
    let Some(control) = rows
        .iter()
        .find(|row| row.multiple == 1.0 && row.tighten_bps == 0.0)
    else {
        println!("No control arm ran, so nothing below is attributable.");
        return;
    };

    println!();
    println!(
        "[run] {} hops matched the venue's pair, {} were buildable — the denominator below",
        control.matched, control.built
    );
    println!();
    let base_decimals = venue.vaults.first().map_or(0, |vault| vault.decimals);
    println!(
        "  {:>16}  {:>24}  {:>13}  {:>7}  {:>7}  {:>9}  {:>9}",
        "arm", "vaults posted", "max trade", "won", "filled", "mean bps", "vs 1x"
    );
    for row in rows {
        let capital = row
            .vaults
            .iter()
            .map(|vault| format_amount(vault.after, vault.decimals))
            .collect::<Vec<_>>()
            .join(" / ");
        let mean = row
            .mean_bps()
            .map_or_else(|| "-".to_string(), |bps| format!("{bps:+.1}"));
        let delta = match (row.mean_bps(), control.mean_bps()) {
            (Some(bps), Some(base)) if row.multiple != 1.0 || row.tighten_bps != 0.0 => {
                format!("{:+.1}", bps - base)
            }
            _ => "-".to_string(),
        };
        let ceiling = row
            .ceiling
            .as_deref()
            .and_then(|top| top.parse::<u128>().ok())
            .and_then(|top| u64::try_from(top).ok())
            .map_or_else(|| "-".to_string(), |top| format_amount(top, base_decimals));
        println!(
            "  {:>16}  {capital:>24}  {ceiling:>13}  {:>7}  {:>6.1}%  {mean:>9}  {delta:>9}",
            label_of(row),
            row.scored,
            row.fill_rate() * 100.0
        );
    }
    println!();

    report_population(rows, control);
    report_refusals(rows);
    report_reference(control, reference);
    report_guards(rows, control, venue);
}

/// An arm's label, rebuilt from the row — including which knob the multiple turned.
fn label_of(row: &ArmRow) -> String {
    ArmSpec {
        multiple: row.multiple,
        target: cli::ScaleTarget::named(&row.scale),
        tighten_bps: row.tighten_bps,
    }
    .label()
}

/// Each arm's mean is taken over the hops that arm filled, so the arms score different populations.
fn report_population(rows: &[ArmRow], control: &ArmRow) {
    if rows.iter().all(|row| row.scored == control.scored) {
        return;
    }
    // The spread across arms: naming the control twice would print the caveat as a tautology.
    let (Some(fewest), Some(most)) = (
        rows.iter().min_by_key(|row| row.scored),
        rows.iter().max_by_key(|row| row.scored),
    ) else {
        return;
    };
    println!(
        "Read `mean bps` and `vs 1x` across arms with care: the arms score different populations \
         ({} hops at {}, {} at {}). A larger arm wins the hops a smaller one refused, and a venue \
         refuses what its curve prices worst — so the mean can fall while the flow won rises. The \
         `won` column is the like-for-like number.",
        fewest.scored,
        label_of(fewest),
        most.scored,
        label_of(most)
    );
    println!();
}

/// Why the missing flow was missed: the venue's own refusals behind the fill column.
fn report_refusals(rows: &[ArmRow]) {
    // No outcomes at all means no probe was ever simulated, not a venue that filled everything.
    if rows.iter().all(|row| row.outcomes.is_empty()) {
        println!("No probe was simulated, so the venue was never asked to fill anything.");
        return;
    }
    if rows.iter().all(|row| row.refusals().is_empty()) {
        println!("Every buildable hop was filled by every arm.");
        return;
    }
    println!("Why the rest were not filled:");
    for row in rows {
        let refusals = row.refusals();
        let summary = match refusals.is_empty() {
            true => "none — the venue filled everything it was offered".to_string(),
            false => refusals
                .iter()
                .map(|(key, count)| format!("{key} {count}"))
                .collect::<Vec<_>>()
                .join(", "),
        };
        println!("  {:>16}  {summary}", label_of(row));
    }
    println!();
}

/// The control rewrites the venue with its own bytes, so it must land on the reference's number;
/// a gap is the capture having missed a change, which every arm then inherits.
fn report_reference(control: &ArmRow, reference: Option<&ArmRow>) {
    let Some(reference) = reference else {
        return;
    };
    let (Some(measured), Some(reference)) = (control.mean_bps(), reference.mean_bps()) else {
        println!(
            "[warn] the control or the reference pass scored nothing, so the capture cannot be \
             checked. Read 'vs 1x'."
        );
        return;
    };
    let gap = measured - reference;
    println!(
        "[diag] reference: {reference:+.1} bps, 1x arm following the same trajectory: \
         {measured:+.1} — gap {gap:+.1} bps"
    );
    if gap.abs() > REFERENCE_GAP_WARN_BPS {
        println!(
            "[warn] a gap this size means the capture missed changes, and every arm inherits the \
             same hole — treat the table as unsound"
        );
    }
    println!();
}

fn report_guards(rows: &[ArmRow], control: &ArmRow, venue: &Venue) {
    // Detection does not read overrides, so the admitted population is a property of the range.
    if let Some(drift) = rows
        .iter()
        .find(|row| row.matched != control.matched || row.built != control.built)
    {
        println!(
            "[warn] the {} arm admitted {}/{} hops against the control's {}/{}; detection does not \
             read overrides, so the arms are not measuring one population",
            label_of(drift),
            drift.matched,
            drift.built,
            control.matched,
            control.built
        );
    }

    // A venue that quotes from a ladder does not move when only its vaults are funded, so a flat
    // line here can be the finding rather than a fault.
    if rows.len() > 1 && rows.iter().all(|row| row.outcomes == control.outcomes) {
        let vaults_only = rows
            .iter()
            .all(|row| row.multiple == 1.0 || row.scale == cli::ScaleTarget::Vaults.as_str());
        let remedy = match (vaults_only, venue.state.is_some()) {
            (true, true) => "this run scaled vaults only, and this venue does not price off its \
                             vault balances — that is the finding, not a fault. Re-run with \
                             --scale ladder or --scale all to move what it does price off",
            (true, false) => "this run scaled vaults only and the plan names no state account, so \
                              nothing that could change the venue's quotes was touched",
            (false, true) => "every knob was turned and nothing moved, so this venue prices from \
                              state the plan does not describe",
            (false, false) => "the plan scales vaults only. A venue that quotes from a ladder \
                               ignores its vault balances entirely — add its state layout",
        };
        println!("[warn] every arm's outcomes are identical to the control's: {remedy}.");
    }

    // The falsification arm: less capital that does not cost fills means the lever is inert.
    if let Some(down) = rows.iter().find(|row| row.multiple < 1.0)
        && down.scored >= control.scored
    {
        println!(
            "[warn] the {} arm filled {} against the control's {} — cutting the venue down did not \
             cost fills, so the lever is not live and no arm above the control is attributable",
            label_of(down),
            down.scored,
            control.scored
        );
    }

    // Titan writes a zero minimum-out, so a route can succeed and deliver nothing; the scorer
    // counts that as a fill at exactly -10000 bps.
    if let Some(empty) = rows
        .iter()
        .find(|row| row.mean_bps().is_some_and(|bps| (bps + 10_000.0).abs() < 1.0))
    {
        println!(
            "[warn] the {} arm's mean bps is -10000, the signature of routes that succeeded and \
             delivered nothing; the venue's mint ordering in the plan is probably reversed",
            label_of(empty)
        );
    }

    // The machine-readable answer: the smallest arm that filled everything, or 0. By value, not
    // row order — the control sorts first and would answer 1x whenever it saturates.
    let saturated = rows
        .iter()
        .filter(|row| row.built > 0 && row.outcome(FILLED) == row.built)
        .map(|row| row.multiple)
        .min_by(|a, b| a.partial_cmp(b).expect("finite multiples"))
        .unwrap_or(0.0);
    println!("{saturated}");
}

#[cfg(test)]
mod tests;

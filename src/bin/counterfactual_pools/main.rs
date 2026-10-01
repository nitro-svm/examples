//! Add a pool that never existed on mainnet and measure the taker flow the router would have sent
//! it. The pool's accounts are posted as an override at the start slot and offered to Metis as an
//! extra market, so every historical swap is requoted with the new pool available. With `--oracle`,
//! the pool's oracle is repriced every slot from Binance.

mod oracle;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    future::ready,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use backtest_example::utils::{
    self, connection::ConnectionArgs, pair::parse_pair, range::RangeArgs,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use simulator_api::{
    AccountData, AccountModifications, ActionAnchor, DiagnosticProbeParams, MintPair, RerouteFilter,
};
use simulator_client::{
    CreateSession, DiagnosticNotification, FULL_PERCENT, ReplacementNotification,
    RequoteNotification, account_data_from_ui, reroute_report::short_mint, subscribe_diagnostics,
    subscribe_replacements,
};
use solana_address::Address;

#[derive(Parser)]
#[command(about = "Add a pool that does not exist on mainnet and measure the flow it would win")]
struct Cli {
    #[command(flatten)]
    conn: ConnectionArgs,

    #[command(flatten)]
    range: RangeArgs,

    /// The new pool's market address, as the router would name it.
    #[arg(long)]
    pool: Address,

    /// An account the pool needs, as `solana account <address> --output json` writes it. Repeat
    /// for the pool itself and every account it reads: vaults, oracles, configs.
    #[arg(long = "account", value_name = "PATH", required = true)]
    accounts: Vec<PathBuf>,

    /// The pool's oracle, repriced every slot from `--binance`. Must be one of the `--account` files.
    #[arg(long, requires = "binance")]
    oracle: Option<Address>,

    /// Binance USDⓈ-M futures symbol whose 1m closes price the oracle, e.g. `SNDKUSDT`.
    #[arg(long, requires = "oracle")]
    binance: Option<String>,

    /// Mainnet RPC, read for the block times that line slots up with Binance minutes.
    #[arg(long, default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,

    /// Only requote swaps trading this pair, as `<base>,<quote>`, in both directions.
    #[arg(long, value_parser = parse_pair)]
    filter_pair: Vec<MintPair>,

    /// One row per leg the router sent through the pool.
    #[arg(long, default_value = "new-pool.jsonl")]
    out: PathBuf,
}

/// The shape `solana account --output json` writes.
#[derive(Deserialize)]
struct AccountFile {
    pubkey: String,
    account: serde_json::Value,
}

fn load_account(path: &Path) -> Result<(Address, AccountData)> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: AccountFile =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let address = file.pubkey.parse()?;
    let account = account_data_from_ui(&file.account)
        .with_context(|| format!("decoding the account in {}", path.display()))?;
    // The simulator refuses program overrides: the pool's program must already be deployed.
    ensure!(
        !account.executable,
        "{} is a program; only the pool's own accounts can be added",
        path.display()
    );
    Ok((address, account))
}

/// Slots between router probes of the new pool.
const PROBE_EVERY: u64 = 250;
/// Trade sizes, in USD, the router quotes the new pool at.
const PROBE_USD: [u32; 3] = [100, 1_000, 10_000];

/// What the router said when asked to quote the new pool directly.
#[derive(Default)]
struct Probes {
    quoted: u64,
    failed: u64,
    last_error: Option<String>,
    sample: Option<serde_json::Value>,
}

/// One leg the router sent through the pool.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolLeg {
    slot: u64,
    original_signature: String,
    input_mint: String,
    output_mint: String,
    amount: u64,
    /// Percent of the leg routed through the pool.
    share: u64,
    original_quoted_out: u64,
    quoted_out: u64,
    /// The requote against the original fill.
    improvement_bps: Option<f64>,
}

#[derive(Default)]
struct Direction {
    legs: u64,
    split: u64,
    bps_total: f64,
    scored: u64,
}

#[derive(Default)]
struct Tally {
    notifications: BTreeMap<&'static str, u64>,
    requoted_legs: u64,
    by_direction: BTreeMap<(String, String), Direction>,
    rows: Vec<PoolLeg>,
}

impl Tally {
    fn record(&mut self, requote: &RequoteNotification, pool: &str) {
        for leg in &requote.legs {
            self.requoted_legs += 1;
            let share = leg
                .route_plan
                .iter()
                .flat_map(|plan| plan.hops())
                .filter(|hop| hop.amm_key() == Some(pool))
                .map(|hop| hop.percent)
                .max()
                .unwrap_or(0);
            if share == 0 {
                continue;
            }
            let improvement_bps = (leg.original_quoted_out > 0).then(|| {
                (leg.metis_quoted_out as f64 - leg.original_quoted_out as f64)
                    / leg.original_quoted_out as f64
                    * 10_000.0
            });
            let direction = self
                .by_direction
                .entry((leg.input_mint.to_string(), leg.output_mint.to_string()))
                .or_default();
            direction.legs += 1;
            direction.split += u64::from(share < FULL_PERCENT);
            if let Some(bps) = improvement_bps {
                direction.bps_total += bps;
                direction.scored += 1;
            }
            self.rows.push(PoolLeg {
                slot: requote.core.slot,
                original_signature: requote.core.original_signature.to_string(),
                input_mint: leg.input_mint.to_string(),
                output_mint: leg.output_mint.to_string(),
                amount: leg.amount,
                share,
                original_quoted_out: leg.original_quoted_out,
                quoted_out: leg.metis_quoted_out,
                improvement_bps,
            });
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let args = Cli::parse();

    let accounts = args
        .accounts
        .iter()
        .map(|path| load_account(path))
        .collect::<Result<BTreeMap<_, _>>>()?;
    ensure!(
        accounts.contains_key(&args.pool),
        "no --account file holds the pool {}",
        args.pool
    );
    eprintln!(
        "[pool] {} with {} accounts, owned by {}",
        args.pool,
        accounts.len(),
        accounts[&args.pool].owner
    );

    let create = CreateSession::builder()
        .start_slot(args.range.start_slot)
        .slot_count(args.range.slot_count)
        .reroute_order_flow(true)
        .detect_failed_l1_swaps(true)
        .reroute_extra_markets(BTreeSet::from([args.pool]))
        .maybe_reroute_filter((!args.filter_pair.is_empty()).then(|| RerouteFilter {
            pairs: args.filter_pair.iter().copied().collect(),
        }))
        .replay_account_state(true)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .diagnostic_probes(vec![DiagnosticProbeParams {
            anchor: ActionAnchor::AfterEverySlot {
                every_n_slots: PROBE_EVERY.try_into()?,
            },
            markets: vec![args.pool],
            usd_values: PROBE_USD.to_vec(),
            label: None,
        }])
        .build();
    let mut overrides = BTreeMap::from([(args.range.start_slot, accounts.clone())]);
    if let (Some(address), Some(symbol)) = (args.oracle, &args.binance) {
        let account = accounts
            .get(&address)
            .context("--oracle must be one of the --account files")?;
        let entry = oracle::pool_entry(&accounts[&args.pool])?;
        let prices = oracle::schedule(
            account,
            entry,
            symbol,
            &args.rpc_url,
            args.range.start_slot,
            args.range.end_slot(),
        )
        .await?;
        eprintln!("[oracle] {address} entry {entry} repriced at {} slots from {symbol}", prices.len());
        for (slot, state) in prices {
            overrides.entry(slot).or_default().insert(address, state);
        }
    }
    let create = utils::session::with_overrides(
        create,
        overrides
            .into_iter()
            .map(|(slot, accounts)| (slot, AccountModifications(accounts))),
    );
    let mut session = utils::session::start(&args.conn, create.into_request()?).await?;

    let tally = Arc::new(Mutex::new(Tally::default()));
    let sink = tally.clone();
    let pool = args.pool.to_string();
    let handle = subscribe_replacements(
        &session.session_info().rpc_endpoint,
        move |notification: ReplacementNotification| {
            let mut tally = sink.lock().expect("tally");
            let kind = match &notification {
                ReplacementNotification::Requote(requote) => {
                    tally.record(requote, &pool);
                    "requote"
                }
                ReplacementNotification::DirectFill(_) => "directFill",
                ReplacementNotification::Original(_) => "original",
            };
            *tally.notifications.entry(kind).or_default() += 1;
            ready(())
        },
    )
    .await?;
    let probes = Arc::new(Mutex::new(Probes::default()));
    let probe_sink = probes.clone();
    let probe_handle = subscribe_diagnostics(
        &session.session_info().rpc_endpoint,
        move |sample: DiagnosticNotification| {
            let mut probes = probe_sink.lock().expect("probes");
            match (sample.err, sample.results) {
                (None, Some(results)) => {
                    probes.quoted += 1;
                    probes.sample.get_or_insert(results);
                }
                (err, _) => {
                    probes.failed += 1;
                    probes.last_error = err.or(Some("no results".to_string()));
                }
            }
            ready(())
        },
    )
    .await?;

    let funnel = utils::session::drive_to_completion(
        &mut session,
        args.range.slot_count,
        utils::session::log_slot,
    )
    .await?;
    handle.stop.send(true).ok();
    handle.join_handle.await??;
    probe_handle.stop.send(true).ok();
    probe_handle.join_handle.await??;
    session.shutdown().await;

    let tally = std::mem::take(&mut *tally.lock().expect("tally"));
    let mut out = BufWriter::new(fs::File::create(&args.out)?);
    for row in &tally.rows {
        writeln!(out, "{}", serde_json::to_string(row)?)?;
    }
    out.flush()?;

    if let Some(stats) = &funnel {
        println!(
            "{} swaps detected -> {} rerouted -> {} succeeded",
            stats.swaps_detected, stats.swaps_rerouted, stats.swaps_succeeded
        );
    }
    println!("notifications received: {:?}", tally.notifications);
    println!(
        "{} of {} requoted legs routed through the new pool",
        tally.rows.len(),
        tally.requoted_legs
    );
    for ((input, output), direction) in &tally.by_direction {
        let mean = match direction.scored {
            0 => "-".to_string(),
            n => format!("{:+.1}", direction.bps_total / n as f64),
        };
        println!(
            "  {}->{}: {} legs ({} split), {mean} bps vs the original fill",
            short_mint(input),
            short_mint(output),
            direction.legs,
            direction.split
        );
    }
    let probes = std::mem::take(&mut *probes.lock().expect("probes"));
    println!(
        "router probes of the pool: {} quoted, {} failed{}",
        probes.quoted,
        probes.failed,
        probes
            .last_error
            .map(|error| format!(" (last error: {error})"))
            .unwrap_or_default()
    );
    if let Some(sample) = probes.sample {
        println!("first quote: {sample}");
    }
    println!("legs written to {}", args.out.display());
    Ok(())
}

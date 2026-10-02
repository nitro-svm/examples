//! This example adds a new TaurusFi SNDK-USDC market and measures the flow it would've received
//! from Solana routers. The accounts are injected at the start of the test range:
//! - the oracle is set every slot based on Binance SNDK-USDT perp pricing
//! - the market is registered with Jupiter Metis and evaluated as part of its routing decisions
//!
//! The simulation reruns every historical SNDK swap with the new pool available.

mod diagnostic;
mod oracle;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    future::ready,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use backtest_example::utils::{self, connection::ConnectionArgs};
use clap::Parser;
use serde::{Deserialize, Serialize};
use simulator_api::{
    AccountData, AccountModifications, MintPair, RerouteAggregators, RerouteFilter, SwapAggregator,
};
use simulator_client::{
    CreateSession, FULL_PERCENT, ReplacementNotification, RequoteNotification,
    account_data_from_ui, reroute_report::short_mint, subscribe_replacements,
};
use solana_address::{Address, address};

/// 2026-09-29 17:35–18:20 UTC: the start slot of the test range in US market hours.
const START_SLOT: u64 = 451_710_501;
const SLOT_COUNT: u64 = 10_000;

const SNDK: Address = address!("SNDKbwMUQvZhnLnxLduradgLHG5KrPuKwpnrkkGRhfH");
const USDC: Address = address!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const POOL: Address = address!("HxAgMQYcqXZvNrUGCwJ6o8i8HsBpgRKUwbjGt7PHdU1a");
const ORACLE: Address = address!("ZPVuSaHzpmtipBrFeLSKajVSQwYp4dJ8Vq6eEyPYWL6");
/// The pool's slot in its oracle (offset 320 of the pool).
const ORACLE_ENTRY: usize = 0;

/// The SNDK pool, its vault, and its oracle, as `solana account --output json` writes them.
/// These don't yet exist on mainnet and are injected as part of the simulation.
/// (The USDC pool already exists, so it doesn't need to be added.)
const FIXTURES: [&str; 3] = [
    include_str!("fixtures/pool.json"),
    include_str!("fixtures/vault.json"),
    include_str!("fixtures/oracle.json"),
];

#[derive(Parser)]
#[command(about = "Add a TaurusFi SNDK/USDC pool and measure the flow the router would send it")]
struct Cli {
    #[command(flatten)]
    conn: ConnectionArgs,

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

fn load_account(json: &str) -> Result<(Address, AccountData)> {
    let file: AccountFile = serde_json::from_str(json)?;
    Ok((file.pubkey.parse()?, account_data_from_ui(&file.account)?))
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
    /// Requote notifications received; fewer than the summary's `rerouted` means some were dropped.
    requotes: u64,
    requoted_legs: u64,
    by_direction: BTreeMap<(String, String), Direction>,
    rows: Vec<PoolLeg>,
}

impl Tally {
    fn record(&mut self, requote: &RequoteNotification, pool: &str) {
        self.requotes += 1;
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

    // 1) Configure the simulation session
    let create = CreateSession::builder()
        .start_slot(START_SLOT)
        .slot_count(SLOT_COUNT)
        .reroute_order_flow(true)
        .detect_failed_l1_swaps(false)                 // If a swap failed on mainnet, don't try to reroute it here
        .reroute_extra_markets(BTreeSet::from([POOL])) // Register the new SNDK-USDC market for Jupiter Metis
        .reroute_filter(RerouteFilter {
            pairs: [MintPair::new(SNDK, USDC)].into(),
        })                                                // Only reroute SNDK-USDC swaps (replay other swaps exactly as they happened historically)
        .reroute_aggregators(RerouteAggregators::new([
            SwapAggregator::Jupiter,
            SwapAggregator::Okx,
            SwapAggregator::Titan,
            SwapAggregator::Dflow,
        ]))                                            // Reroute swaps from every router (only Jupiter is the default)
        .replay_account_state(true)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .build();

    // 2) Update the above config with the new accounts
    // Load the new pool, vault, and oracle accounts for SNDK and inject them
    let accounts = FIXTURES
        .into_iter()
        .map(load_account)
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut overrides = BTreeMap::from([(START_SLOT, accounts.clone())]);
    // Add a schedule to update the oracle every slot based on hardcoded Binance SNDK-USDT perp pricing
    let oracle = accounts.get(&ORACLE).context("the oracle fixture")?;
    for (slot, state) in oracle::schedule(oracle, ORACLE_ENTRY)? {
        overrides.entry(slot).or_default().insert(ORACLE, state);
    }
    let create = utils::session::with_overrides(
        create,
        overrides
            .into_iter()
            .map(|(slot, accounts)| (slot, AccountModifications(accounts))),
    );

    // 3) Start the session!
    let mut session = utils::session::start(&args.conn, create.into_request()?).await?;

    let tally = Arc::new(Mutex::new(Tally::default()));
    let sink = tally.clone();
    let pool = POOL.to_string();
    // 4) Subscribe to the Metis reroute event feed and record the results
    let handle = subscribe_replacements(
        &session.session_info().rpc_endpoint,
        move |notification: ReplacementNotification| {
            // The requote notification contains the new route versus the original fill
            // (Other notifications are skipped)
            if let ReplacementNotification::Requote(requote) = &notification {
                sink.lock().expect("tally").record(requote, &pool);
            }
            ready(())
        },
    )
    .await?;

    let funnel =
        utils::session::drive_to_completion(&mut session, SLOT_COUNT, utils::session::log_slot)
            .await?;
    handle.stop.send(true).ok();
    handle.join_handle.await??;
    session.shutdown().await;

    // 5) Print the results of the rerouting
    let tally = std::mem::take(&mut *tally.lock().expect("tally"));
    let mut out = BufWriter::new(fs::File::create(&args.out)?);
    for row in &tally.rows {
        writeln!(out, "{}", serde_json::to_string(row)?)?;
    }
    out.flush()?;

    if let Some(stats) = &funnel {
        println!(
            "{} swaps detected -> {} rerouted -> {} succeeded ({} requotes received)",
            stats.swaps_detected, stats.swaps_rerouted, stats.swaps_succeeded, tally.requotes
        );
    }
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
    println!("legs written to {}", args.out.display());
    Ok(())
}

//! Add a TaurusFi SNDK/USDC pool that never existed on mainnet and measure the taker flow the router
//! would have sent it. The pool's accounts are posted at the start slot and offered to Metis as an
//! extra market, and its oracle is repriced every slot from Binance, so every historical SNDK swap is
//! requoted with the new pool available.

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
    AccountData, AccountModifications, ActionAnchor, DiagnosticProbeParams, MintPair, RerouteFilter,
};
use simulator_client::{
    CreateSession, DiagnosticNotification, FULL_PERCENT, ReplacementNotification,
    RequoteNotification, account_data_from_ui, reroute_report::short_mint, subscribe_diagnostics,
    subscribe_replacements,
};
use solana_address::{Address, address};

/// 2026-09-29 17:35–18:20 UTC: the start of a recorded account-state bundle, in US market hours.
const START_SLOT: u64 = 451_710_501;
const SLOT_COUNT: u64 = 10_000;

const SNDK: Address = address!("SNDKbwMUQvZhnLnxLduradgLHG5KrPuKwpnrkkGRhfH");
const USDC: Address = address!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const POOL: Address = address!("HxAgMQYcqXZvNrUGCwJ6o8i8HsBpgRKUwbjGt7PHdU1a");
const ORACLE: Address = address!("ZPVuSaHzpmtipBrFeLSKajVSQwYp4dJ8Vq6eEyPYWL6");
/// The pool's slot in its oracle (offset 320 of the pool).
const ORACLE_ENTRY: usize = 0;

/// The pool, its SNDK vault and its oracle, as `solana account --output json` writes them. USDC
/// settles from the vault every TaurusFi pool shares, which already exists.
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

/// Slots between router probes of the pool.
const PROBE_EVERY: u64 = 250;
/// Trade sizes, in USD, the router quotes the pool at.
const PROBE_USD: [u32; 3] = [100, 1_000, 10_000];

/// The router's direct quotes of one pool in one direction.
#[derive(Default)]
struct Probe {
    quoted: u64,
    failed: u64,
    last_error: Option<String>,
    /// Output per unit of input at the smallest size quoted.
    last_rate: Option<f64>,
}

/// Probe results keyed by pool, then `(input, output)` mint.
type Probes = BTreeMap<String, BTreeMap<(String, String), Probe>>;

/// Fold one `/diagnostic` sample, whose results hold a `swapInfo` or a `quoteFailure` per direction.
fn record_probe(probes: &mut Probes, sample: DiagnosticNotification) {
    let pool = probes.entry(sample.market).or_default();
    for result in sample
        .results
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |value: &serde_json::Value, key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        if let Some(info) = result.get("swapInfo") {
            let probe = pool
                .entry((text(info, "inputMint"), text(info, "outputMint")))
                .or_default();
            probe.quoted += 1;
            let amount = |key| text(info, key).parse::<f64>().ok();
            if let (Some(input), Some(output)) = (amount("inAmount"), amount("outAmount")) {
                probe.last_rate = Some(output / input);
            }
        } else if let Some(failure) = result.get("quoteFailure") {
            let probe = pool
                .entry((text(failure, "input_mint"), text(failure, "output_mint")))
                .or_default();
            probe.failed += 1;
            probe.last_error = Some(text(failure, "error"));
        }
    }
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

    let accounts = FIXTURES
        .into_iter()
        .map(load_account)
        .collect::<Result<BTreeMap<_, _>>>()?;

    let create = CreateSession::builder()
        .start_slot(START_SLOT)
        .slot_count(SLOT_COUNT)
        .reroute_order_flow(true)
        .detect_failed_l1_swaps(true)
        .reroute_extra_markets(BTreeSet::from([POOL]))
        .reroute_filter(RerouteFilter {
            pairs: [MintPair::new(SNDK, USDC)].into(),
        })
        .replay_account_state(true)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .diagnostic_probes(vec![DiagnosticProbeParams {
            anchor: ActionAnchor::AfterEverySlot {
                every_n_slots: PROBE_EVERY.try_into()?,
            },
            markets: vec![POOL],
            usd_values: PROBE_USD.to_vec(),
            label: None,
        }])
        .build();
    let mut overrides = BTreeMap::from([(START_SLOT, accounts.clone())]);
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
    let mut session = utils::session::start(&args.conn, create.into_request()?).await?;

    let tally = Arc::new(Mutex::new(Tally::default()));
    let sink = tally.clone();
    let pool = POOL.to_string();
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
            record_probe(&mut probe_sink.lock().expect("probes"), sample);
            ready(())
        },
    )
    .await?;

    let funnel =
        utils::session::drive_to_completion(&mut session, SLOT_COUNT, utils::session::log_slot)
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
    for directions in probes.values() {
        println!("router quotes of the pool:");
        for ((input, output), probe) in directions {
            println!(
                "  {}->{}: {} quoted, {} failed{}{}",
                short_mint(input),
                short_mint(output),
                probe.quoted,
                probe.failed,
                probe
                    .last_rate
                    .map(|rate| format!(", rate {rate:.6}"))
                    .unwrap_or_default(),
                probe
                    .last_error
                    .as_ref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            );
        }
    }
    println!("legs written to {}", args.out.display());
    Ok(())
}

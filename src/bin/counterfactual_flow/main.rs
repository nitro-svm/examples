//! Pause at each batch that invokes the venue under test, apply a custom
//! quoting-parameter change against the frozen chain state, then measure its
//! effect on taker flow: every historical swap is re-quoted through Metis
//! (simulated only, never committed), so legs touching the venue show whether
//! the change would win more fills than it did originally.

mod discovery;

use backtest_example::utils::types::TxWithMeta;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{
    BacktestClient, CreateSession, DiscoveryStepResult, RerouteLegNotification,
    RerouteNotification, subscribe_reroutes,
};
use solana_address::Address;
use solana_transaction::versioned::VersionedTransaction;

use crate::discovery::is_program_upgrade;

#[derive(Parser)]
#[command(
    about = "Apply a quoting-parameter change at a discovered batch, then measure its effect on taker flow via Metis rerouting"
)]
struct Cli {
    /// Simulator base URL (no scheme), e.g. `staging.simulator.example.com`.
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,

    /// API key sent as the `X-API-Key` header.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 433838452)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 433838453)]
    end_slot: u64,

    /// The venue under test: batches invoking this program pause for inspection
    /// (to apply the parameter change), and rerouted legs are tailored to fills
    /// that touch it.
    #[arg(long)]
    program_id: Address,
}

/// Whether Metis's chosen route for `leg` touches `program_id`. `route_plan` is the raw
/// per-hop JSON (pool addresses, not necessarily program ids), so this also falls back to
/// a substring match against the human-readable `route_summary`.
fn leg_touches_venue(leg: &RerouteLegNotification, program_id: &Address) -> bool {
    let needle = program_id.to_string();
    leg.route_plan
        .as_deref()
        .is_some_and(|plan| plan.contains(&needle))
        || leg.route_summary.contains(&needle)
}

/// If `endpoint` is a relative path, resolve it against `base`.
fn resolve_url(base: &str, endpoint: &str) -> Result<String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }
    let path = endpoint.trim_start_matches('/');
    Ok(format!("{base}/{path}"))
}

/// Rerouted-leg counts for one side of the parameter change.
#[derive(Default)]
struct Tally {
    txs: AtomicU64,
    legs: AtomicU64,
    wins: AtomicU64,
}

impl Tally {
    fn report(&self, label: &str) {
        eprintln!(
            "{label}: transactions={} legs={} legs where metis quoted higher={}",
            self.txs.load(Ordering::Relaxed),
            self.legs.load(Ordering::Relaxed),
            self.wins.load(Ordering::Relaxed),
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let client = BacktestClient::builder()
        .url(format!("wss://{}/backtest", &cli.url))
        .api_key(cli.api_key)
        .build();

    eprintln!("[ws] connecting to wss://{}/backtest", &cli.url);

    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .reroute_order_flow(true)
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(cli.program_id)])
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));

    let rpc_endpoint = session
        .rpc_endpoint()
        .context("no rpc_endpoint")?
        .to_string();
    let rpc_url = resolve_url(&format!("https://{}", cli.url), &rpc_endpoint)?;
    eprintln!("[ws] rpc_endpoint: {rpc_url}");

    session.ensure_ready(Some(Duration::from_secs(120))).await?;
    eprintln!("[ws] ready — venue={}", cli.program_id);

    // Set once the parameter change has been applied; every reroute notification
    // received after that point is tallied separately from the baseline.
    let applied = Arc::new(AtomicBool::new(false));
    let before = Arc::new(Tally::default());
    let after = Arc::new(Tally::default());

    let applied_cb = applied.clone();
    let before_cb = before.clone();
    let after_cb = after.clone();
    let program_id = cli.program_id;

    let sub = subscribe_reroutes(&rpc_url, move |notification| {
        let applied = applied_cb.clone();
        let before = before_cb.clone();
        let after = after_cb.clone();
        async move {
            let RerouteNotification {
                slot,
                original_signature,
                legs,
                err,
                ..
            } = notification;

            let legs: Vec<_> = legs
                .iter()
                .filter(|leg| leg_touches_venue(leg, &program_id))
                .collect();
            if legs.is_empty() {
                return;
            }

            let tally = if applied.load(Ordering::Relaxed) {
                &after
            } else {
                &before
            };
            tally.txs.fetch_add(1, Ordering::Relaxed);

            if let Some(err) = &err {
                eprintln!("[reroute] slot={slot} sig={original_signature} simulation failed: {err}");
                return;
            }

            for leg in &legs {
                tally.legs.fetch_add(1, Ordering::Relaxed);
                let improvement_bps = (leg.metis_quoted_out as f64 - leg.original_quoted_out as f64)
                    / leg.original_quoted_out as f64
                    * 10_000.0;
                if leg.metis_quoted_out > leg.original_quoted_out {
                    tally.wins.fetch_add(1, Ordering::Relaxed);
                }
                eprintln!(
                    "[reroute] slot={slot} sig={original_signature} {}->{} amount={} original_out={} metis_out={} ({improvement_bps:+.2} bps) via {}",
                    leg.input_mint, leg.output_mint, leg.amount, leg.original_quoted_out, leg.metis_quoted_out, leg.route_summary,
                );
            }
        }
    })
    .await
    .context("subscribe to reroutes")?;
    eprintln!(
        "[sub] listening for rerouted swaps touching {}",
        cli.program_id
    );

    let timeout = Some(Duration::from_secs(120));
    let mut pause_count = 0u64;

    loop {
        match session.advance_to_discovery(Some(1), timeout).await? {
            DiscoveryStepResult::Paused(pause) => {
                pause_count += 1;

                let slot = pause.paused.slot;
                let batch = pause.paused.batch_index.unwrap_or(0);
                eprintln!("[pause #{pause_count}] slot={slot} batch={batch}");

                if applied.load(Ordering::Relaxed) {
                    continue;
                }

                let txs: Vec<TxWithMeta> = pause
                    .discovery
                    .transactions
                    .iter()
                    .filter_map(|bin| {
                        let bytes = bin.decode().ok()?;
                        bincode::deserialize(&bytes).ok()
                    })
                    .collect();

                for tx_with_meta in &txs {
                    let signature = tx_with_meta
                        .transaction
                        .signatures
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_default();

                    if is_program_upgrade(tx_with_meta) {
                        eprintln!("  [upgrade] sig={signature}");

                        // TODO: replace with the custom quoting-parameter update.
                        let custom_upgrade = VersionedTransaction::default();
                        session
                            .rpc()
                            .send_transaction(&custom_upgrade)
                            .await
                            .context("send tx failed")?;

                        applied.store(true, Ordering::Relaxed);
                        eprintln!(
                            "  [applied] parameter change is now live for the rest of the replay"
                        );
                        break;
                    }
                }
            }

            DiscoveryStepResult::Completed => {
                eprintln!("[done] session completed; total pauses: {pause_count}");
                break;
            }
        }
    }

    // Drain the subscription before closing.
    sub.stop.send(true).ok();
    eprintln!("[sub] draining subscription...");
    let mut join_handle = sub.join_handle;
    loop {
        tokio::select! {
            _ = &mut join_handle => break,
            _ = session.next_event(Some(Duration::from_secs(30))) => {}
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;

    eprintln!("=== Counterfactual summary (venue={}) ===", cli.program_id);
    before.report("before parameter change");
    after.report("after parameter change");
    // Bare post-change transaction count to stdout so it can be captured/piped.
    println!("{}", after.txs.load(Ordering::Relaxed));
    Ok(())
}

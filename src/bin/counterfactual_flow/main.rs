//! Apply a custom parameter change to a venue and measure its effect on taker flow:
//! every historical swap is requoted through Jupiter Metis (simulated only, never committed),
//! so legs touching the venue show whether the change would capture more fills than it did originally.

mod discovery;

use backtest_example::utils::types::TxWithMeta;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::{DiscoveryFilter, RerouteVenues, SwapVenue};
use simulator_client::{
    BacktestClient, CreateSession, DiscoveryStepResult, RerouteNotification, subscribe_reroutes,
};
use solana_address::Address;
use solana_transaction::versioned::VersionedTransaction;

use crate::discovery::{contains_venue, is_program_upgrade, resolve_venue_label};

#[derive(Parser)]
#[command(
    about = "Apply a quoting change at a discovered batch and measure its effect on taker flow via Metis rerouting"
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
    #[arg(long, default_value_t = 433838553)]
    end_slot: u64,

    /// The venue under test: batches invoking this program pause for inspection
    /// (to apply the parameter change). Its Jupiter/Metis route label is resolved
    /// automatically via `program-id-to-label` and used to match rerouted legs,
    /// since `route_plan`/`route_summary` carry pool addresses and display labels,
    /// never program ids.
    #[arg(long)]
    program_id: Address,
}

fn resolve_url(base: &str, endpoint: &str) -> Result<String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }
    let path = endpoint.trim_start_matches('/');
    Ok(format!("{base}/{path}"))
}

/// Rerouted-leg counts for the run.
#[derive(Default)]
struct Summary {
    // Number of transactions that Metis rerouted to the specified venue.
    txs: AtomicU64,
    // Number of legs (multiple legs per transactions) rerouted to venue.
    legs: AtomicU64,
    // Number of legs where new output > original output.
    improvements: AtomicU64,
}

impl Summary {
    fn report(&self, label: &str) {
        eprintln!(
            "{label}: transactions={} legs={} legs where metis quoted higher={}",
            self.txs.load(Ordering::Relaxed),
            self.legs.load(Ordering::Relaxed),
            self.improvements.load(Ordering::Relaxed),
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let venue_label = resolve_venue_label(&cli.program_id).await?;
    eprintln!("[jup] resolved venue label: {venue_label:?}");

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
                // The server re-quotes Jupiter order flow alone by default. A venue's share of
                // the book is contested across every aggregator that routes to it, so name them
                // all: dropping one drops the legs it carried, not just its label.
                .reroute_venues(RerouteVenues::new([
                    SwapVenue::Jupiter,
                    SwapVenue::Okx,
                    SwapVenue::Titan,
                    SwapVenue::Dflow,
                ]))
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(cli.program_id)])
                .build(),
        )
        .await?;

    eprintln!(
        "[ws] session: {}",
        session
            .session_id()
            .map_or_else(|| "?".to_string(), |id| id.to_string())
    );

    let rpc_endpoint = session
        .rpc_endpoint()
        .context("no rpc_endpoint")?
        .to_string();
    let rpc_url = resolve_url(&format!("https://{}", cli.url), &rpc_endpoint)?;
    eprintln!("[ws] rpc_endpoint: {rpc_url}");

    // No `ensure_ready()` here: it would consume the server's one-time `ReadyForContinue`
    // without replying to it, and `advance_to_discovery` below only sends the `Continue`
    // that kicks off execution in reaction to seeing that message — leaving the session
    // stuck waiting on a signal that already came and went.
    eprintln!("[ws] venue={} venue_label={venue_label:?}", cli.program_id);

    let summary = Arc::new(Summary::default());

    let summary_cb = summary.clone();
    let venue_label_cb = venue_label.clone();

    let sub = subscribe_reroutes(&rpc_url, move |notification| {
        let summary = summary_cb.clone();
        let venue_label = venue_label_cb.clone();
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
                .filter(|leg| contains_venue(leg, &venue_label))
                .collect();
            if legs.is_empty() {
                return;
            }

            summary.txs.fetch_add(1, Ordering::Relaxed);

            if let Some(err) = &err {
                eprintln!("[reroute] slot={slot} sig={original_signature} simulation failed: {err}");
                return;
            }

            for leg in &legs {
                summary.legs.fetch_add(1, Ordering::Relaxed);
                let improvement_bps = (leg.metis_quoted_out as f64 - leg.original_quoted_out as f64)
                    / leg.original_quoted_out as f64
                    * 10_000.0;
                if leg.metis_quoted_out > leg.original_quoted_out {
                    summary.improvements.fetch_add(1, Ordering::Relaxed);
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
    eprintln!("[sub] listening for rerouted swaps touching \"{venue_label}\"");

    // Covers the first step, which waits out session startup: a rerouting session also builds
    // the metis router's market cache before any batch executes, and that is minutes rather
    // than the seconds a plain replay needs.
    let timeout = Some(Duration::from_secs(900));
    let mut pause_count = 0u64;

    loop {
        // NOTE: if updates are frequent, this can also happen via WS instead of RPC.
        match session.advance_to_discovery(Some(1), timeout).await? {
            DiscoveryStepResult::Paused(pause) => {
                pause_count += 1;

                let slot = pause.paused.slot;
                let batch = pause.paused.batch_index.unwrap_or(0);
                eprintln!("[pause #{pause_count}] slot={slot} batch={batch}");

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

                        // TODO: replace with the custom parameter update.
                        let custom_upgrade = VersionedTransaction::default();
                        session
                            .rpc()
                            .send_transaction(&custom_upgrade)
                            .await
                            .context("send tx failed")?;
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
    summary.report("rerouted");
    // Bare transaction count to stdout so it can be captured/piped.
    println!("{}", summary.txs.load(Ordering::Relaxed));
    Ok(())
}

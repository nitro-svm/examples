use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::BacktestResponse;
use simulator_client::{
    BacktestClient, Continue, CreateSession, RerouteLegNotification, RerouteNotification,
    subscribe_reroutes,
};
use solana_address::Address;

#[derive(Parser)]
#[command(about = "Reroute historical taker flow through Metis and report how it compares")]
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

    /// Re-quote every detected taker swap through Metis and simulate the routed
    /// transaction (never committed) in place of the original.
    #[arg(long, default_value_t = false)]
    reroute_metis: bool,

    /// The venue to evaluate: only legs whose Metis-chosen route touches this
    /// program are counted/printed. Omit to see every rerouted leg.
    #[arg(long)]
    program_id: Option<Address>,
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
                .reroute_order_flow(cli.reroute_metis)
                .send_summary(true)
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));

    let rpc_endpoint = session.rpc_endpoint().context("no rpc_endpoint")?.to_string();
    let rpc_url = resolve_url(&format!("https://{}", cli.url), &rpc_endpoint)?;
    eprintln!("[ws] rpc_endpoint: {rpc_url}");

    session.ensure_ready(Some(Duration::from_secs(120))).await?;
    eprintln!(
        "[ws] ready — reroute_metis={} program_id={:?}",
        cli.reroute_metis, cli.program_id
    );

    // Subscribe to `rerouteSubscribe`: one notification per historical transaction
    // whose swap(s) were re-quoted through Metis and simulated (never committed) in
    // place of the original. When `--program-id` names a venue, every counter below
    // is tailored to legs Metis routed through that venue; i.e. fills it would win.
    let tx_count = Arc::new(AtomicU64::new(0));
    let leg_count = Arc::new(AtomicU64::new(0));
    let win_count = Arc::new(AtomicU64::new(0));

    let tx_count_cb = tx_count.clone();
    let leg_count_cb = leg_count.clone();
    let win_count_cb = win_count.clone();
    let program_id = cli.program_id;

    let sub = subscribe_reroutes(&rpc_url, move |notification| {
        let tx_count = tx_count_cb.clone();
        let leg_count = leg_count_cb.clone();
        let win_count = win_count_cb.clone();
        async move {
            let RerouteNotification {
                slot,
                original_signature,
                legs,
                err,
                realized_output_amount,
                original_realized_output_amount,
                ..
            } = notification;

            let legs: Vec<_> = legs
                .iter()
                .filter(|leg| program_id.is_none_or(|id| leg_touches_venue(leg, &id)))
                .collect();
            if legs.is_empty() {
                return;
            }

            tx_count.fetch_add(1, Ordering::Relaxed);

            if let Some(err) = &err {
                eprintln!("[reroute] slot={slot} sig={original_signature} simulation failed: {err}");
                return;
            }

            for leg in &legs {
                leg_count.fetch_add(1, Ordering::Relaxed);
                let improvement_bps = (leg.metis_quoted_out as f64 - leg.original_quoted_out as f64)
                    / leg.original_quoted_out as f64
                    * 10_000.0;
                if leg.metis_quoted_out > leg.original_quoted_out {
                    win_count.fetch_add(1, Ordering::Relaxed);
                }
                eprintln!(
                    "[reroute] slot={slot} sig={original_signature} {}->{} amount={} original_out={} metis_out={} ({improvement_bps:+.2} bps) via {}",
                    leg.input_mint, leg.output_mint, leg.amount, leg.original_quoted_out, leg.metis_quoted_out, leg.route_summary,
                );
            }

            if let (Some(realized), Some(original_realized)) =
                (realized_output_amount, original_realized_output_amount)
            {
                eprintln!(
                    "[reroute] slot={slot} sig={original_signature} realized: original={original_realized} metis={realized}"
                );
            }
        }
    })
    .await
    .context("subscribe to reroutes")?;
    eprintln!("[sub] listening for rerouted swaps");

    eprintln!("advancing slots {}..={}", cli.start_slot, cli.end_slot);
    // `advance`'s `ContinueResult` doesn't carry the session summary, so capture it
    // from the raw `Completed` event instead.
    let mut reroute_stats = None;
    session
        .advance(
            Continue::builder()
                .advance_count(cli.end_slot - cli.start_slot + 1)
                .build(),
            None,
            |event| {
                if let BacktestResponse::Completed { summary, .. } = event {
                    reroute_stats = summary.as_ref().and_then(|s| s.reroute_stats.clone());
                }
            },
        )
        .await?;
    eprintln!("all blocks processed");

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

    eprintln!("=== Reroute summary ===");
    if let Some(program_id) = cli.program_id {
        eprintln!("venue: {program_id}");
    }
    eprintln!("transactions rerouted: {}", tx_count.load(Ordering::Relaxed));
    eprintln!("legs compared: {}", leg_count.load(Ordering::Relaxed));
    eprintln!("legs where metis quoted higher: {}", win_count.load(Ordering::Relaxed));
    if let Some(stats) = reroute_stats {
        eprintln!(
            "server-side funnel: rerouted={} simulated={} succeeded={} requote_failures={}",
            stats.swaps_rerouted, stats.swaps_simulated, stats.swaps_succeeded, stats.requote_failures,
        );
    }
    // Bare transaction count to stdout so it can be captured/piped.
    println!("{}", tx_count.load(Ordering::Relaxed));
    Ok(())
}

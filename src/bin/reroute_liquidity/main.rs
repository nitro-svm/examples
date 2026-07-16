use backtest_example::utils::parse::JUPITER_V6;
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
use simulator_api::{
    AccountModifications, ActionAnchor, ActionKind, DiscoveryFilter, ScheduledAction,
};
use simulator_client::{
    BacktestClient, Continue, CreateSession, DiscoveryStepResult, LogSubscriptionHandle,
    subscribe_program_logs,
};
use solana_address::Address;
use solana_commitment_config::CommitmentConfig;
use solana_transaction::versioned::VersionedTransaction;

#[derive(Parser)]
#[command(about = "Pause before each discovered batch and inspect frozen chain state")]
struct Cli {
    /// Simulator base URL (no scheme), e.g. `staging.simulator.example.com`.
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,

    /// API key sent as the `X-API-Key` header.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 433_136_691)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 433_136_701)]
    end_slot: u64,

    /// Program to watch; defaults to Jupiter V6 aggregator.
    #[arg(long, default_value = JUPITER_V6)]
    program_id: String,

    #[arg(long, default_value_t = false)]
    reroute_metis: bool,
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
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));

    // Resolve the session's RPC endpoint so we can open a program-log
    // subscription against it (used below to count invocations).
    let rpc_endpoint = session.rpc_endpoint().context("no rpc_endpoint")?.to_string();
    let rpc_url = resolve_url(&format!("https://{}", cli.url), &rpc_endpoint)?;
    eprintln!("[ws] rpc_endpoint: {rpc_url}");

    session.ensure_ready(Some(Duration::from_secs(600))).await?;
    eprintln!("[ws] ready — scanning for {} batches", cli.program_id);

    // Subscribe to logs mentioning `program_id`. Each notification is one
    // transaction that invoked the program; within it we tally the
    // `Program <id> invoke` log lines, which count every invocation including
    // CPIs (a program invoked twice in one tx counts twice).
    let tx_count = Arc::new(AtomicU64::new(0));
    let invoke_count = Arc::new(AtomicU64::new(0));

    let tx_count_cb = tx_count.clone();
    let invoke_count_cb = invoke_count.clone();
    let program_label = cli.program_id.clone();
    let invoke_marker = format!("Program {} invoke", cli.program_id);

    let LogSubscriptionHandle { join_handle, stop } = subscribe_program_logs(
        &rpc_url,
        &cli.program_id,
        CommitmentConfig::confirmed(),
        move |notification| {
            let tx_count = tx_count_cb.clone();
            let invoke_count = invoke_count_cb.clone();
            let program_label = program_label.clone();
            let invoke_marker = invoke_marker.clone();
            async move {
                let invokes = notification
                    .value
                    .logs
                    .iter()
                    .filter(|line| line.starts_with(&invoke_marker))
                    .count() as u64;
                tx_count.fetch_add(1, Ordering::Relaxed);
                invoke_count.fetch_add(invokes, Ordering::Relaxed);
                eprintln!(
                    "[{program_label}] slot={} sig={} invokes={invokes}",
                    notification.context.slot, notification.value.signature,
                );
                for line in &notification.value.logs {
                    eprintln!("  {line}");
                }
            }
        },
    )
    .await
    .context("subscribe to program logs")?;
    eprintln!("[sub] counting invocations of {}", cli.program_id);

    eprintln!("advancing slots {}..={}", cli.start_slot, cli.end_slot);
    session
        .advance(
            Continue::builder()
                .advance_count(cli.end_slot - cli.start_slot + 1)
                .build(),
            None,
            |_| {},
        )
        .await?;
    eprintln!("all blocks processed");

    // Drain the subscription before closing — closing destroys the RPC state
    // the subscription relies on. Keep the control WS alive with next_event
    // while the subscription task winds down.
    stop.send(true).ok();
    eprintln!("[sub] draining subscription...");
    let mut join_handle = join_handle;
    loop {
        tokio::select! {
            _ = &mut join_handle => break,
            _ = session.next_event(Some(Duration::from_secs(30))) => {}
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;

    eprintln!("=== Invocations of {} ===", cli.program_id);
    eprintln!("invocations: {}", invoke_count.load(Ordering::Relaxed));
    // Bare transaction count to stdout so it can be captured/piped.
    println!("{}", tx_count.load(Ordering::Relaxed));
    Ok(())
}

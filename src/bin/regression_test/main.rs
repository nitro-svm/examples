//! Backtest session example using simulator-client.
//!
//! Demonstrates the full session lifecycle — connect, create, subscribe to
//! logs, advance, close. The control WebSocket (session lifecycle) is managed
//! by `simulator_client::BacktestClient`. RPC queries and log subscriptions
//! use the `RpcClient` and `PubsubClient` exposed directly on the session.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use simulator_client::balances::{SolChange, TokenChange};
use simulator_client::{Address, BacktestClient, Continue, CreateSession, backtest_ws_url};

mod logs;
use crate::logs::subscribe_logs;

// ── CLI ────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Backtest session example")]
struct Cli {
    /// Base URL for the backtest endpoint (no scheme).
    #[arg(long, default_value = "localhost:8900")]
    url: String,

    /// API key sent as the X-API-Key header on the control WebSocket.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 460_919_623)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 460_919_624)]
    end_slot: u64,

    /// File to write transaction logs to.
    #[arg(long, default_value = "logs.txt")]
    log_file: PathBuf,

    /// Program ID to filter logs on.
    #[arg(long)]
    program_id: Option<String>,

    /// Path to a compiled .so to deploy as PROGRAM_ID before the first slot.
    /// Build with: `solana program dump addr... program.so --url mainnet-beta`
    #[arg(long)]
    program_so: Option<PathBuf>,
}

/// All data captured for a single transaction.
struct Transaction {
    slot: u64,
    signature: String,
    success: bool,
    err: Option<String>,
    logs: Vec<String>,
    sol_changes: Vec<SolChange>,
    token_changes: Vec<TokenChange>,
}

// ── Stats ──────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    total: usize,
    successes: usize,
    failures: usize,
    /// Cumulative lamport delta per account across all transactions.
    sol_net: HashMap<Address, i128>,
    /// Cumulative raw-token delta per (pubkey, mint) pair; also stores decimals.
    token_net: HashMap<(Address, Address), (i128, u8)>,
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // ── 1. Connect and create a session ───────────────────────────────────────
    let ws_url = backtest_ws_url(&cli.url);
    let client = BacktestClient::builder()
        .url(&ws_url)
        .api_key(cli.api_key.clone())
        .build();

    eprintln!("[ws] connecting to {ws_url}");
    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .build(),
        )
        .await?;

    eprintln!(
        "[ws] session_id: {}",
        session
            .session_id()
            .map_or_else(|| "?".to_string(), |id| id.to_string())
    );
    eprintln!("[ws] rpc_endpoint: {}", session.grant().rpc_url());

    eprintln!("[ws] waiting for ReadyForContinue...");
    session.ensure_ready(Some(Duration::from_secs(600))).await?;
    eprintln!("[ws] ready");

    // ── 2. Query initial chain state via RpcClient ────────────────────────────
    let slot = session.rpc().get_slot().await?;
    println!("current slot:     {slot}");

    let blockhash = session.rpc().get_latest_blockhash().await?;
    println!("latest blockhash: {blockhash}");

    // ── 3. Subscribe to program logs (if --program-id supplied) ─────────
    let stats = Arc::new(Mutex::new(Stats::default()));
    let log_task = if let Some(program_id) = &cli.program_id {
        let (handle, stop_tx) = subscribe_logs(
            session.grant(),
            program_id,
            cli.log_file.clone(),
            Arc::clone(&stats),
        )
        .await?;
        Some((handle, stop_tx))
    } else {
        None
    };

    // ── 4. Build program injection (if --program-so supplied) ─────────────────
    let modifications = match &cli.program_so {
        Some(path) => {
            let id = cli
                .program_id
                .as_deref()
                .context("--program-so requires --program-id")?;
            let elf = std::fs::read(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            eprintln!("[inject] {} bytes from {}", elf.len(), path.display());
            session.modify_program(id, &elf).await?
        }
        None => BTreeMap::new(),
    };

    // ── 5. Advance through all blocks ─────────────────────────────────────────
    eprintln!("advancing slots {}..={}", cli.start_slot, cli.end_slot);
    session
        .advance(
            Continue::builder()
                .advance_count(cli.end_slot - cli.start_slot + 1)
                .modify_accounts(modifications)
                .build(),
            None,
            |_| {},
        )
        .await?;
    eprintln!("all blocks processed");

    // ── 6. Tear down ──────────────────────────────────────────────────────────
    // Signal the log task to drain remaining buffered notifications and exit.
    // Wait for it to finish (all getTransaction calls complete) BEFORE
    // closing the session, since closing destroys all RPC state.
    if let Some((handle, stop_tx)) = log_task {
        stop_tx.send(true).ok();
        eprintln!("[sub] waiting for log task to drain...");

        // Keep the control WS alive while waiting by draining any incoming
        // messages. next_event times out after 30s, keeping the loop active.
        let mut handle = handle;
        loop {
            tokio::select! {
                _ = &mut handle => break,
                _ = session.next_event(Some(Duration::from_secs(30))) => {}
            }
        }
    }
    session.close(Some(Duration::from_secs(10))).await?;

    // ── 7. Summary ────────────────────────────────────────────────────────────
    let s = stats.lock().unwrap();
    println!("\n=== Summary ===");
    println!("total:     {}", s.total);
    println!("successes: {}", s.successes);
    println!("failures:  {}", s.failures);
    println!("log file:  {}", cli.log_file.display());

    if !s.sol_net.is_empty() {
        println!("\n=== SOL P&L (all accounts, sorted by absolute change) ===");
        let mut sorted: Vec<_> = s.sol_net.iter().collect();
        sorted.sort_by_key(|(_, d)| -d.abs());
        for (pubkey, delta) in &sorted {
            println!(
                "  {}  {:+.9} SOL  ({:+} lamports)",
                pubkey,
                **delta as f64 / 1e9,
                delta,
            );
        }
    }

    if !s.token_net.is_empty() {
        println!("\n=== Token P&L (all ATAs, sorted by absolute change) ===");
        let mut sorted: Vec<_> = s.token_net.iter().collect();
        sorted.sort_by_key(|(_, (d, _))| -d.abs());
        for ((pubkey, mint), (delta, decimals)) in &sorted {
            println!(
                "  {}  {}  {:+.prec$}  ({:+} raw)",
                pubkey,
                mint,
                *delta as f64 / 10f64.powi(*decimals as i32),
                delta,
                prec = *decimals as usize,
            );
        }
    }

    Ok(())
}

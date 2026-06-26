//! Backtest session example using simulator-client.
//!
//! Demonstrates the full session lifecycle — connect, create, subscribe to
//! logs, advance, close. The control WebSocket (session lifecycle) is managed
//! by `simulator_client::BacktestClient`. RPC queries and log subscriptions
//! use the `RpcClient` and `PubsubClient` exposed directly on the session.

mod logs;
mod utils;

use std::{
    collections::HashMap,
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::Parser;
use simulator_client::{BacktestClient, Continue, CreateSession};
use solana_pubkey::Pubkey;

// ── CLI ────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Backtest session example")]
struct Cli {
    /// Base URL for the backtest endpoint (no scheme).
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,

    /// API key sent as the X-API-Key header on the control WebSocket.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 428_824_220)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 428_824_225)]
    end_slot: u64,

    /// File to write transaction logs to.
    #[arg(long, default_value = "logs.txt")]
    log_file: PathBuf,

    /// Program ID to filter logs on.
    #[arg(long)]
    program_id: Option<String>,

    // /// Account to subscribe to for state-diff notifications.
    // #[arg(long)]
    // account: Option<String>,

    /// Path to a compiled .so to deploy as PROGRAM_ID before the first slot.
    /// Build with: `solana program dump addr... program.so --url mainnet-beta`
    #[arg(long)]
    program_so: Option<PathBuf>,
}

// ── URL helpers ────────────────────────────────────────────────────────────────

/// If `endpoint` is a relative path, resolve it against `base`.
fn resolve_url(base: &str, endpoint: &str) -> Result<String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }
    let path = endpoint.trim_start_matches('/');
    Ok(format!("{base}/{path}"))
}

// ── Clock sysvar decoding ────────────────────────────────────────────────────────

/// Decoded fields of the Clock sysvar (40 bytes, all little-endian).
struct Clock {
    slot: u64,
    epoch_start_timestamp: i64,
    epoch: u64,
    leader_schedule_epoch: u64,
    unix_timestamp: i64,
}

impl std::fmt::Display for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "slot={} epoch={} leader_sched_epoch={} unix_ts={} epoch_start_ts={}",
            self.slot,
            self.epoch,
            self.leader_schedule_epoch,
            self.unix_timestamp,
            self.epoch_start_timestamp,
        )
    }
}

/// Decode a Clock from its 40-byte account data.
fn clock_from_bytes(bytes: &[u8]) -> Option<Clock> {
    if bytes.len() < 40 {
        return None;
    }
    let at = |o: usize| -> [u8; 8] { bytes[o..o + 8].try_into().unwrap() };
    Some(Clock {
        slot: u64::from_le_bytes(at(0)),
        epoch_start_timestamp: i64::from_le_bytes(at(8)),
        epoch: u64::from_le_bytes(at(16)),
        leader_schedule_epoch: u64::from_le_bytes(at(24)),
        unix_timestamp: i64::from_le_bytes(at(32)),
    })
}

/// Decode a Clock out of a `UiAccount` JSON value whose `data` is `[base64, "base64"]`.
fn clock_from_ui_account(account: &serde_json::Value) -> Option<Clock> {
    let b64 = account.get("data")?.get(0)?.as_str()?;
    let bytes = STANDARD.decode(b64).ok()?;
    clock_from_bytes(&bytes)
}

// ── Balance change types ───────────────────────────────────────────────────────

/// SOL balance change for a single account within a transaction.
struct SolAccount {
    pubkey: String,
    pre_lamports: u64,
    post_lamports: u64,
}

impl SolAccount {
    fn delta(&self) -> i64 {
        self.post_lamports as i64 - self.pre_lamports as i64
    }
}

/// SPL token balance change for a single ATA within a transaction.
struct TokenAccount {
    pubkey: String,
    mint: String,
    owner: String,
    pre_amount: u64,
    post_amount: u64,
    decimals: u8,
}

impl TokenAccount {
    fn delta(&self) -> i64 {
        self.post_amount as i64 - self.pre_amount as i64
    }
    fn to_ui(&self, raw: u64) -> f64 {
        raw as f64 / 10f64.powi(self.decimals as i32)
    }
}

/// All data captured for a single transaction.
struct Transaction {
    slot: u64,
    signature: String,
    success: bool,
    err: Option<String>,
    logs: Vec<String>,
    sol_changes: Vec<SolAccount>,
    token_changes: Vec<TokenAccount>,
}

// ── Stats ──────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    total: usize,
    successes: usize,
    failures: usize,
    /// Cumulative lamport delta per account across all transactions.
    sol_net: HashMap<String, i64>,
    /// Cumulative raw-token delta per (pubkey, mint) pair; also stores decimals.
    token_net: HashMap<(String, String), (i64, u8)>,
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // ── 1. Connect and create a session ───────────────────────────────────────
    let client = BacktestClient::builder()
        .url(format!("wss://{}/backtest", &cli.url))
        .api_key(cli.api_key.clone())
        .build();

    eprintln!("[ws] connecting to wss://{}/backtest", &cli.url);
    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .build(),
        )
        .await?;

    eprintln!("[ws] session_id: {}", session.session_id().unwrap_or("?"));
    let rpc_endpoint = session.rpc_endpoint().context("no rpc_endpoint")?.to_string();
    let rpc_url = resolve_url(&format!("https://{}", cli.url), &rpc_endpoint)?;
    eprintln!("[ws] rpc_endpoint: {rpc_url}");

    eprintln!("[ws] waiting for ReadyForContinue...");
    session.ensure_ready(Some(Duration::from_secs(600))).await?;
    eprintln!("[ws] ready");

    // ── 2. Query initial chain state via RpcClient ────────────────────────────
    let slot = session.rpc().get_slot().await?;
    println!("current slot:     {slot}");

    let blockhash = session.rpc().get_latest_blockhash().await?;
    println!("latest blockhash: {blockhash}");

    // ── 3. Subscribe to account diffs (if --account supplied) ─────────────────
    let clock = Pubkey::from_str("SysvarC1ock11111111111111111111111111111111").unwrap();
    eprintln!("[sub] subscribing to account diffs for {clock}");
    let handle = session
        .subscribe_account_diffs(&clock.to_string(), |n| async move {
            println!(
                "[diff] context.slot={} account={:?} sig={:?} tx_index={:?} block_time={:?}",
                n.context.slot, n.account, n.signature, n.tx_index, n.block_time,
            );
            if let Some(pre) = &n.pre {
                match clock_from_ui_account(pre) {
                    Some(c) => println!("       pre  clock {c}"),
                    None => println!("       pre={pre}"),
                }
            }
            if let Some(post) = &n.post {
                match clock_from_ui_account(post) {
                    Some(c) => println!("       post clock {c}"),
                    None => println!("       post={post}"),
                }
            }
            println!();
        })
        .await?;

    // tokio::time::sleep(Duration::from_secs(15)).await;

    // ── 5. Advance one slot at a time, dumping the Clock sysvar each step ──────
    // Stepping one slot per Continue lets us compare the RPC context slot
    // (get_slot) against the slot embedded in the decoded Clock sysvar, and
    // line both up against the context.slot on the account-diff notifications.
    for step in 0..(cli.end_slot - cli.start_slot) {
        // Inject the program ELF (if any) only on the first step.
        session
            .advance(
                Continue::builder()
                    .advance_count(1)
                    .build(),
                None,
                |_| {},
            )
            .await?;

        let context_slot = session.rpc().get_slot().await?;
        let acc = session.rpc().get_account(&clock).await;
        match acc.as_ref().ok().and_then(|a| clock_from_bytes(&a.data)) {
            Some(c) => println!("[step {step}] get_slot={context_slot} | {c}"),
            None => println!("[step {step}] get_slot={context_slot} clock_account={acc:?}"),
        }
        println!();
    }
    eprintln!("all blocks processed");

    // ── 6. Tear down ──────────────────────────────────────────────────────────
    // Signal the account-diff subscription to drain remaining buffered
    // notifications, then wait for it to finish BEFORE closing the session,
    // since closing destroys all RPC state.
    handle.stop.send(true).ok();
    eprintln!("[sub] waiting for account-diff task to drain...");
    handle.join_handle.await.ok();
    session.close(Some(Duration::from_secs(30))).await?;

    // ── 7. Summary ────────────────────────────────────────────────────────────
    // let s = stats.lock().unwrap();
    // println!("\n=== Summary ===");
    // println!("total:     {}", s.total);
    // println!("successes: {}", s.successes);
    // println!("failures:  {}", s.failures);
    // println!("log file:  {}", cli.log_file.display());

    // if !s.sol_net.is_empty() {
    //     println!("\n=== SOL P&L (all accounts, sorted by absolute change) ===");
    //     let mut sorted: Vec<_> = s.sol_net.iter().collect();
    //     sorted.sort_by_key(|(_, d)| -d.abs());
    //     for (pubkey, delta) in &sorted {
    //         println!(
    //             "  {}  {:+.9} SOL  ({:+} lamports)",
    //             pubkey,
    //             **delta as f64 / 1e9,
    //             delta,
    //         );
    //     }
    // }

    // if !s.token_net.is_empty() {
    //     println!("\n=== Token P&L (all ATAs, sorted by absolute change) ===");
    //     let mut sorted: Vec<_> = s.token_net.iter().collect();
    //     sorted.sort_by_key(|(_, (d, _))| -d.abs());
    //     for ((pubkey, mint), (delta, decimals)) in &sorted {
    //         println!(
    //             "  {}  {}  {:+.prec$}  ({:+} raw)",
    //             pubkey,
    //             mint,
    //             *delta as f64 / 10f64.powi(*decimals as i32),
    //             delta,
    //             prec = *decimals as usize,
    //         );
    //     }
    // }

    Ok(())
}

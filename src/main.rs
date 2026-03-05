//! Backtest session example.
//!
//! Demonstrates the full session lifecycle — connect, create, subscribe to
//! logs, advance, close. The control WebSocket (session lifecycle) is driven
//! manually because it uses a Nitro-specific protocol. State queries use
//! `solana_client::nonblocking::rpc_client::RpcClient` and log subscriptions
//! use `solana_client::nonblocking::pubsub_client::PubsubClient`.
//!
//! ## Protocol overview
//!
//! The backtest API has two layers:
//!
//! 1. **Control WebSocket** (`ws[s]://host/backtest`)
//!    Manages the session lifecycle. Messages are JSON with an internally-tagged
//!    enum format: `{"method": "...", "params": ...}`.
//!
//! 2. **RPC endpoint** (URL provided in the `SessionCreated` response)
//!    - HTTP POST: standard Solana JSON-RPC (used via `RpcClient`)
//!    - WebSocket: standard Solana PubSub (used via `PubsubClient`)
//!    This endpoint is unauthenticated.
//!
//! ## Typical lifecycle
//!
//! ```text
//! Client                                Server
//!   |-- CreateBacktestSession ---------->|  (control WS, X-API-Key header)
//!   |<-- SessionCreated (rpc_endpoint) --|
//!   |<-- ReadyForContinue ---------------|
//!   |
//!   | RpcClient::get_slot() ------------>|  (HTTP JSON-RPC)
//!   | PubsubClient::logs_subscribe() --->|  (WebSocket PubSub)
//!   |
//!   |-- Continue (advance_count=1) ----->|
//!   |<-- SlotNotification(slot) ---------|
//!   |<-- logsNotification (PubSub WS) ---|
//!   |<-- ReadyForContinue ---------------|
//!   |
//!   |-- CloseBacktestSession ----------->|
//!   |<-- Success ------------------------|
//! ```

mod logs;
mod utils;

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

use logs::subscribe_logs;
use utils::build_program_injection;

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
    #[arg(long, default_value_t = 399_834_992)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 399_834_997)]
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

// ── Control WebSocket message types ───────────────────────────────────────────
//
// These mirror `simulator-backtest-api::{BacktestRequest, BacktestResponse}` exactly.
// The serde configuration must match, or the server will reject the
// messages / we'll fail to parse its responses.

/// Requests sent to the server over the control WebSocket.
#[derive(Serialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
enum WsRequest {
    CreateBacktestSession(CreateParams),
    Continue(ContinueParams),
    CloseBacktestSession,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateParams {
    start_slot: u64,
    end_slot: u64,
    signer_filter: Vec<String>,
    preload_programs: Vec<String>,
    preload_account_bundles: Vec<String>,
    /// Seconds to keep the session alive after the control WS disconnects.
    /// Max 900. Set to max so the session survives while we drain getTransaction calls.
    disconnect_timeout_secs: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContinueParams {
    advance_count: u64,
    /// Base64-encoded transactions to inject before advancing.
    transactions: Vec<String>,
    /// Account state overrides applied before execution. Keys are base58 pubkeys.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    modify_account_states: AccountModifications,
}

// ── Account modification types ────────────────────────────────────────────────

/// Map of base58 pubkey → account data to inject/override before execution.
type AccountModifications = BTreeMap<String, AccountData>;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountData {
    data: EncodedBinary,
    executable: bool,
    lamports: u64,
    owner: String,
    space: u64,
}

#[derive(Clone, Serialize)]
struct EncodedBinary {
    data: String,
    encoding: &'static str,
}

/// Responses received from the server over the control WebSocket.
#[derive(Debug, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
enum WsResponse {
    SessionCreated {
        session_id: String,
        rpc_endpoint: String,
    },
    #[allow(dead_code)]
    SessionAttached {
        session_id: String,
        rpc_endpoint: String,
    },
    ReadyForContinue,
    SlotNotification(u64),
    Status { status: String },
    Completed { summary: Option<Value> },
    Success,
    Error(Value),
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

// ── Session ────────────────────────────────────────────────────────────────────

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Session {
    ws: WsStream,
    #[allow(dead_code)]
    session_id: String,
    /// HTTP URL for RpcClient; also used to derive the PubSub WebSocket URL.
    rpc_endpoint: String,
    rpc: RpcClient,
}

impl Session {
    async fn create(cli: &Cli) -> Result<Self> {
        let ws_url = format!("wss://{}/backtest", &cli.url);
        let http_url = format!("https://{}", &cli.url);

        let mut req = ws_url
            .as_str()
            .into_client_request()
            .context("invalid WebSocket URL")?;
        req.headers_mut().insert(
            "X-API-Key",
            HeaderValue::from_str(&cli.api_key).context("invalid API key")?,
        );

        eprintln!("[ws] connecting to {ws_url}");
        let (ws, _) = connect_async(req)
            .await
            .context("WebSocket handshake failed")?;
        eprintln!("[ws] connected");

        let mut ws = ws;

        ws_send(
            &mut ws,
            &WsRequest::CreateBacktestSession(CreateParams {
                start_slot: cli.start_slot,
                end_slot: cli.end_slot,
                signer_filter: vec![],
                preload_programs: vec![],
                preload_account_bundles: vec![],
                disconnect_timeout_secs: 900,
            }),
        )
        .await?;

        let (session_id, rpc_endpoint) = loop {
            match ws_recv(&mut ws).await? {
                WsResponse::SessionCreated {
                    session_id,
                    rpc_endpoint,
                } => {
                    let rpc_endpoint = resolve_url(&http_url, &rpc_endpoint)?;
                    eprintln!("[ws] session_id:   {session_id}");
                    eprintln!("[ws] rpc_endpoint: {rpc_endpoint}");
                    break (session_id, rpc_endpoint);
                }
                WsResponse::Error(e) => bail!("error creating session: {e}"),
                other => eprintln!("[ws] (setup) {other:?}"),
            }
        };

        let rpc = RpcClient::new_with_commitment(
            rpc_endpoint.clone(),
            CommitmentConfig::confirmed(),
        );

        let mut session = Self { ws, session_id, rpc_endpoint, rpc };

        eprintln!("[ws] waiting for ReadyForContinue...");
        if !session.drain_until_ready().await? {
            bail!("session completed during startup before any Continue");
        }
        eprintln!("[ws] ready");

        Ok(session)
    }

    /// Drain control messages until `ReadyForContinue` or `Completed`.
    /// Returns `true` if ready for another `Continue`, `false` if completed.
    async fn drain_until_ready(&mut self) -> Result<bool> {
        loop {
            let resp = timeout(Duration::from_secs(600), ws_recv(&mut self.ws))
                .await
                .context("timed out waiting for ReadyForContinue")??;

            match resp {
                WsResponse::ReadyForContinue => return Ok(true),
                WsResponse::Completed { summary } => {
                    eprintln!("[ws] Completed");
                    if let Some(s) = summary {
                        eprintln!("[ws] summary: {s}");
                    }
                    return Ok(false);
                }
                WsResponse::SlotNotification(slot) => eprintln!("[ws] slot {slot}"),
                WsResponse::Status { status } => eprintln!("[ws] status: {status}"),
                WsResponse::Error(e) => bail!("session error: {e}"),
                other => eprintln!("[ws] (drain) {other:?}"),
            }
        }
    }

    /// Advance `advance_count` blocks. Returns `true` if more blocks remain.
    async fn advance(
        &mut self,
        advance_count: u64,
        transactions: Vec<String>,
        modify_account_states: AccountModifications,
    ) -> Result<bool> {
        ws_send(
            &mut self.ws,
            &WsRequest::Continue(ContinueParams { advance_count, transactions, modify_account_states }),
        )
        .await?;
        self.drain_until_ready().await
    }

    async fn close(mut self) -> Result<()> {
        let _ = ws_send(&mut self.ws, &WsRequest::CloseBacktestSession).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or_default();
            match timeout(remaining, ws_recv(&mut self.ws)).await {
                Ok(Ok(WsResponse::Success | WsResponse::Completed { .. })) => break,
                Ok(Ok(other)) => eprintln!("[ws] (close) {other:?}"),
                Ok(Err(_)) | Err(_) => break,
            }
        }
        // Drop the WebSocket without waiting for the server's close echo.
        // The server may be in cleanup_runtime and not reading from the socket,
        // so ws.close(None) would block indefinitely. Dropping closes the TCP
        // connection, which is sufficient for the server to detect disconnect.
        eprintln!("[ws] closed");
        Ok(())
    }
}

// ── WebSocket helpers ──────────────────────────────────────────────────────────

async fn ws_send(ws: &mut WsStream, msg: &WsRequest) -> Result<()> {
    let json = serde_json::to_string(msg).context("serializing request")?;
    eprintln!("[ws ->] {json}");
    ws.send(Message::Text(json)).await.context("ws send failed")?;
    Ok(())
}

async fn ws_recv(ws: &mut WsStream) -> Result<WsResponse> {
    loop {
        let frame = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("WebSocket stream closed"))??;

        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8(b).context("non-UTF-8 binary frame")?,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(f) => bail!("WebSocket closed by server: {f:?}"),
        };

        eprintln!("[ws <-] {text}");
        return serde_json::from_str::<WsResponse>(&text)
            .with_context(|| format!("failed to parse response: {text}"));
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // ── 1. Connect and create a session ───────────────────────────────────────
    let mut session = Session::create(&cli).await?;

    // ── 2. Query initial chain state via RpcClient ────────────────────────────
    let slot = session.rpc.get_slot().await?;
    println!("current slot:     {slot}");

    let blockhash = session.rpc.get_latest_blockhash().await?;
    println!("latest blockhash: {blockhash}");

    // ── 3. Subscribe to program logs (if --program-id supplied) ─────────
    let stats = Arc::new(Mutex::new(Stats::default()));
    let log_task = if let Some(program_id) = &cli.program_id {
        let (handle, stop_tx) = subscribe_logs(
            &session.rpc_endpoint,
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
    let inject_mods = match &cli.program_so {
        Some(path) => {
            let id = cli.program_id.as_deref().context("--program-so requires --program-id")?;
            build_program_injection(id, path, &session.rpc, cli.start_slot).await?
        }
        None => AccountModifications::new(),
    };

    // ── 5. Advance through all blocks ─────────────────────────────────────────
    eprintln!("advancing slots {}..={}", cli.start_slot, cli.end_slot);
    session.advance(cli.end_slot - cli.start_slot + 1, vec![], inject_mods).await?;
    eprintln!("all blocks processed");

    // ── 6. Tear down ──────────────────────────────────────────────────────────
    // Signal the log task to drain remaining buffered notifications and exit.
    // Wait for it to finish (all getTransaction calls complete) BEFORE 
    // closing the session, since closing destroys all RPC state.
    if let Some((handle, stop_tx)) = log_task {
        stop_tx.send(true).ok();
        eprintln!("[sub] waiting for log task to drain...");

        // Keep the control WS alive while waiting:
        // - Send a ping every 30s so the TCP connection doesn't idle-timeout.
        // - Read and discard all incoming messages (pongs, etc.) so the client's
        //   TCP receive buffer stays drained. Without this, the server blocks
        //   trying to write pongs, stops reading its own receive buffer, and
        //   ws_send in close() hangs because the server's receive buffer is full.
        let mut ping_interval = tokio::time::interval(Duration::from_secs(30));
        ping_interval.tick().await; // consume the immediate first tick
        let mut handle = handle;
        loop {
            tokio::select! {
                _ = &mut handle => break,
                _ = ping_interval.tick() => {
                    let _ = session.ws.send(Message::Ping(vec![].into())).await;
                }
                // Drain server messages to prevent TCP receive-buffer deadlock.
                _ = session.ws.next() => {}
            }
        }
    }
    session.close().await?;

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

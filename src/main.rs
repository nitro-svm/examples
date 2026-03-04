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

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::prelude::*;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_client::nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient};
use solana_commitment_config::CommitmentConfig;
use solana_loader_v3_interface::get_program_data_address;
use solana_rpc_client_api::config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use tokio::{sync::oneshot, time::timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

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

/// http://host/path → ws://host/path  |  https → wss
fn http_url_to_ws(url: &str) -> Result<String> {
    let mut parsed = url::Url::parse(url).context("invalid RPC endpoint URL")?;
    let scheme = match parsed.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => bail!("unexpected scheme: {other}"),
    };
    parsed
        .set_scheme(scheme)
        .map_err(|()| anyhow!("failed to set WebSocket scheme"))?;
    Ok(parsed.to_string())
}

// ── Stats ──────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    total: usize,
    successes: usize,
    failures: usize,
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
        let _ = self.ws.close(None).await;
        eprintln!("[ws] closed");
        Ok(())
    }
}

// ── Log subscription ───────────────────────────────────────────────────────────

/// Subscribe to logs mentioning `program`, write every transaction to `log_file`,
/// and accumulate totals in `stats`.
///
/// Blocks until the subscription is confirmed by the server, so the caller can
/// safely start advancing blocks immediately after this returns.
///
/// Returns a `JoinHandle`. When done: call `abort()` then `await` the handle
/// (which returns once the task has actually stopped), then read `stats`.
async fn subscribe_logs(
    rpc_endpoint: &str,
    program: &str,
    log_file: PathBuf,
    stats: Arc<Mutex<Stats>>,
) -> Result<tokio::task::JoinHandle<()>> {
    let ws_url = http_url_to_ws(rpc_endpoint)?;
    let program = program.to_string();

    // Oneshot to signal back to the caller once the subscription is live.
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

    let handle = tokio::spawn(async move {
        eprintln!("[sub] connecting to {ws_url}");

        let file = match File::create(&log_file) {
            Ok(f) => f,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("failed to open {log_file:?}: {e}")));
                return;
            }
        };
        let mut writer = BufWriter::new(file);

        let client = match PubsubClient::new(&ws_url).await {
            Ok(c) => c,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("connect failed: {e}")));
                return;
            }
        };

        let (mut stream, _unsubscribe) = match client
            .logs_subscribe(
                RpcTransactionLogsFilter::Mentions(vec![program.clone()]),
                RpcTransactionLogsConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                },
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("logs_subscribe failed: {e}")));
                return;
            }
        };

        // Subscription is confirmed — unblock the caller.
        let _ = ready_tx.send(Ok(()));
        eprintln!("[sub] subscribed — writing logs to {log_file:?}");

        while let Some(resp) = stream.next().await {
            let slot = resp.context.slot;
            let sig = &resp.value.signature;
            let err = &resp.value.err;
            let success = err.is_none();

            // Update stats.
            {
                let mut s = stats.lock().unwrap();
                s.total += 1;
                if success { s.successes += 1; } else { s.failures += 1; }
            }

            // Print to stdout.
            let status = if success { "ok" } else { "FAIL" };
            println!("[{program}] slot={slot} {status} sig={sig}");

            // Write to file.
            let _ = writeln!(writer, "slot={slot} sig={sig} status={status}");
            if let Some(e) = err {
                let _ = writeln!(writer, "  err={e}");
            }
            for log in &resp.value.logs {
                println!("  {log}");
                let _ = writeln!(writer, "  {log}");
            }
            let _ = writeln!(writer); // blank line between transactions
            let _ = writer.flush();
        }

        eprintln!("[sub] stream ended");
    });

    // Wait until the subscription is confirmed (or fails) before returning.
    ready_rx
        .await
        .context("subscribe_logs task dropped before signalling ready")?
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(handle)
}

// ── Program injection ──────────────────────────────────────────────────────────

const BPF_LOADER_UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

/// Fetch the historical programdata account, splice in new ELF bytes, patch
/// the deployment slot, and return it as an account modification.
///
/// ProgramData layout (bincode):
///   [0..4]   variant = 3  (u32 LE)
///   [4..12]  deployment slot (u64 LE) — patched to start_slot
///   [12]     authority Option discriminant (1 = Some)
///   [13..45] upgrade authority pubkey (32 bytes)
///   [45..]   ELF bytecode  ← replaced with our .so
async fn build_program_injection(
    program_id_str: &str,
    so_path: &std::path::Path,
    rpc: &RpcClient,
    start_slot: u64,
) -> Result<AccountModifications> {
    let elf = std::fs::read(so_path)
        .with_context(|| format!("failed to read {}", so_path.display()))?;
    eprintln!("[inject] {} bytes from {}", elf.len(), so_path.display());

    let program_id: solana_pubkey::Pubkey =
        program_id_str.parse().context("invalid program id")?;
    let programdata_addr = get_program_data_address(&program_id);

    // Fetch programdata for its header.
    let programdata_account = rpc
        .get_account(&programdata_addr)
        .await
        .context("failed to fetch programdata account")?;

    if programdata_account.data.len() < 45 {
        bail!("programdata account too small ({} bytes)", programdata_account.data.len());
    }

    // Copy header (variant + slot + authority = 45 bytes), patch slot, then append new ELF.
    // Use start_slot - 1 so the program appears deployed *before* the first executed slot.
    // Patching to start_slot itself triggers the SVM's same-slot restriction, which marks the
    // program Unloaded (not yet executable) and caches that failure for all subsequent slots.
    let deploy_slot = start_slot.saturating_sub(1);
    let mut data = programdata_account.data[..45].to_vec();
    data[4..12].copy_from_slice(&deploy_slot.to_le_bytes());
    data.extend_from_slice(&elf);

    // Compute rent-exempt minimum for the new (possibly larger) programdata size.
    let programdata_lamports = rpc
        .get_minimum_balance_for_rent_exemption(data.len())
        .await
        .context("failed to compute rent-exempt minimum for programdata")?;

    eprintln!("[inject] programdata  {} ({} bytes, {} lamports)", programdata_addr, data.len(), programdata_lamports);

    let mut modifications = AccountModifications::new();

    // Inject programdata with the patched slot, new ELF, and correct rent-exempt lamports.
    modifications.insert(
        programdata_addr.to_string(),
        AccountData {
            space: data.len() as u64,
            data: EncodedBinary { data: BASE64_STANDARD.encode(&data), encoding: "base64" },
            executable: false,
            lamports: programdata_lamports,
            owner: BPF_LOADER_UPGRADEABLE.to_string(),
        },
    );

    Ok(modifications)
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
        Some(
            subscribe_logs(
                &session.rpc_endpoint,
                program_id,
                cli.log_file.clone(),
                Arc::clone(&stats),
            )
            .await?,
        )
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
    if let Some(task) = log_task {
        task.abort();
        let _ = task.await;
    }
    session.close().await?;

    let s = stats.lock().unwrap();
    println!("\n=== Summary ===");
    println!("total:     {}", s.total);
    println!("successes: {}", s.successes);
    println!("failures:  {}", s.failures);
    println!("log file:  {}", cli.log_file.display());

    Ok(())
}

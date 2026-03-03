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

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_client::nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

const PROGRAM_ID: &str = "...";

// ── CLI ────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Backtest session example")]
struct Cli {
    /// WebSocket URL for the backtest endpoint (ws:// or wss://).
    #[arg(long, default_value = "ws://localhost:8900/backtest")]
    url: String,

    /// API key sent as the X-API-Key header on the control WebSocket.
    #[arg(long, env = "SIMULATOR_API_KEY", default_value = "local-dev-key")]
    api_key: String,

    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 300_000_000)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 300_000_010)]
    end_slot: u64,
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

/// ws://host/path → http://host  |  wss://host/path → https://host
fn ws_url_to_http_base(ws_url: &str) -> Result<String> {
    let url = url::Url::parse(ws_url).context("invalid WebSocket URL")?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => bail!("unexpected WebSocket scheme: {other}"),
    };
    let host = url.host_str().context("WebSocket URL has no host")?;
    match url.port() {
        Some(port) => Ok(format!("{scheme}://{host}:{port}")),
        None => Ok(format!("{scheme}://{host}")),
    }
}

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
        let http_base = ws_url_to_http_base(&cli.url)?;

        let mut req = cli
            .url
            .as_str()
            .into_client_request()
            .context("invalid WebSocket URL")?;
        req.headers_mut().insert(
            "X-API-Key",
            HeaderValue::from_str(&cli.api_key).context("invalid API key")?,
        );

        eprintln!("[ws] connecting to {}", cli.url);
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
                    let rpc_endpoint = resolve_url(&http_base, &rpc_endpoint)?;
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
            let resp = timeout(Duration::from_secs(120), ws_recv(&mut self.ws))
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
    async fn advance(&mut self, advance_count: u64, transactions: Vec<String>) -> Result<bool> {
        ws_send(
            &mut self.ws,
            &WsRequest::Continue(ContinueParams { advance_count, transactions }),
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

/// Subscribe to logs mentioning `program` via `PubsubClient::logs_subscribe`.
///
/// The RPC endpoint accepts WebSocket connections on the same path as HTTP
/// (just swap the scheme). `PubsubClient` handles the `logsSubscribe` /
/// `logsNotification` wire protocol automatically.
///
/// Returns a `JoinHandle` — abort it when done.
async fn subscribe_logs(rpc_endpoint: &str, program: &str) -> Result<tokio::task::JoinHandle<()>> {
    let ws_url = http_url_to_ws(rpc_endpoint)?;
    let program = program.to_string();

    let handle = tokio::spawn(async move {
        eprintln!("[sub] connecting to {ws_url}");

        let client = match PubsubClient::new(&ws_url).await {
            Ok(c) => c,
            Err(e) => { eprintln!("[sub] connect failed: {e}"); return; }
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
            Err(e) => { eprintln!("[sub] logs_subscribe failed: {e}"); return; }
        };

        eprintln!("[sub] subscribed to logs for {program}");

        while let Some(resp) = stream.next().await {
            let slot = resp.context.slot;
            let sig = &resp.value.signature;
            let err = &resp.value.err;
            println!("[{program}] slot={slot} sig={sig} err={err:?}");
            for log in &resp.value.logs {
                println!("  {log}");
            }
        }

        eprintln!("[sub] stream ended");
    });

    Ok(handle)
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

    // ── 3. Subscribe to BisonFi program logs ──────────────────────────────────
    let log_task = subscribe_logs(&session.rpc_endpoint, PROGRAM_ID).await?;

    // ── 4. Advance through all blocks ─────────────────────────────────────────
    eprintln!(
        "advancing slots {}..={} (one block at a time)...",
        cli.start_slot, cli.end_slot
    );

    loop {
        let more_blocks = session.advance(1, vec![]).await?;
        if !more_blocks {
            eprintln!("all blocks processed");
            break;
        }
    }

    // ── 5. Tear down ──────────────────────────────────────────────────────────
    log_task.abort();
    session.close().await?;
    println!("done");

    Ok(())
}

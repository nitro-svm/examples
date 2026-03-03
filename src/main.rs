//! Raw backtest session example.
//!
//! Demonstrates the full session lifecycle — connect, create, query, advance,
//! close — using only external crates. No internal simulator libraries are
//! needed. The types defined here mirror `simulator-backtest-api` exactly and
//! can serve as a reference if you want to implement your own client.
//!
//! ## Protocol overview
//!
//! The backtest API has two layers:
//!
//! 1. **Control WebSocket** (`ws[s]://host/backtest`)
//!    Manages the session lifecycle. Messages are JSON with an internally-tagged
//!    enum format: `{"method": "...", "params": ...}`.
//!
//! 2. **HTTP JSON-RPC** (URL provided in the `SessionCreated` response)
//!    Queries the simulated chain state. Standard Solana JSON-RPC format.
//!
//! Both layers authenticate with an `X-API-Key` header.
//!
//! ## Typical lifecycle
//!
//! ```text
//! Client                                Server
//!   |-- CreateBacktestSession ---------->|
//!   |<-- SessionCreated (session_id,     |
//!   |         rpc_endpoint) -------------|
//!   |<-- Status* ----------------------->|  (prep messages, optional)
//!   |<-- ReadyForContinue ---------------|
//!   |
//!   | (optional) POST rpc_endpoint       |  query state via JSON-RPC
//!   |
//!   |-- Continue (advance_count=1) ----->|
//!   |<-- Status* ------------------------|
//!   |<-- SlotNotification(slot) ---------|
//!   |<-- ReadyForContinue ---------------|  (or Completed at end)
//!   |
//!   | ... repeat Continue/query loop ... |
//!   |
//!   |-- CloseBacktestSession ----------->|
//!   |<-- Success ------------------------|
//!   |-- ws close frame ----------------->|
//! ```
//!
//! ## Usage
//!
//! ```bash
//! # Against a local dev server
//! cargo run -p backtest-raw -- --start-slot 300000000 --end-slot 300000010
//!
//! # Against a remote server
//! cargo run -p backtest-raw -- \
//!   --url wss://simulator.example.com/backtest \
//!   --api-key YOUR_KEY \
//!   --start-slot 300000000 \
//!   --end-slot 300000010
//! ```

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

// ── CLI ────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Backtest session example — raw WebSocket + HTTP JSON-RPC")]
struct Cli {
    /// WebSocket URL for the backtest endpoint (ws:// or wss://).
    #[arg(long, default_value = "ws://localhost:8900/backtest")]
    url: String,

    /// API key sent as the X-API-Key header.
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
// These mirror `simulator-backtest-api::{BacktestRequest, BacktestResponse}`
// exactly. The serde configuration must match, or the server will reject the
// messages / we'll fail to parse its responses.

/// Requests sent to the server over the control WebSocket.
///
/// All variants serialize as `{"method": "<camelCase variant>", "params": ...}`.
/// Unit variants (e.g. `CloseBacktestSession`) emit no `params` field.
#[derive(Serialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
enum WsRequest {
    CreateBacktestSession(CreateParams),
    Continue(ContinueParams),
    CloseBacktestSession,
}

/// Parameters for `CreateBacktestSession`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateParams {
    /// First slot (inclusive) to replay.
    start_slot: u64,
    /// Last slot (inclusive) to replay.
    end_slot: u64,
    /// Skip transactions signed by these base-58 pubkeys.
    signer_filter: Vec<String>,
    /// Program IDs (base-58) to preload into the simulator before execution.
    preload_programs: Vec<String>,
    /// History clerk bundle IDs to preload before execution.
    preload_account_bundles: Vec<String>,
}

/// Parameters for `Continue`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContinueParams {
    /// Number of historical blocks to execute before pausing.
    advance_count: u64,
    /// Optional user-injected transactions (base64-encoded) to execute first.
    transactions: Vec<String>,
}

/// Responses received from the server over the control WebSocket.
///
/// Same tag/content/rename_all configuration as the server-side type.
#[derive(Debug, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
enum WsResponse {
    /// Session created; `rpc_endpoint` is the HTTP JSON-RPC URL to POST to.
    SessionCreated {
        session_id: String,
        rpc_endpoint: String,
    },
    /// Sent after `AttachBacktestSession` reconnects to an existing session.
    #[allow(dead_code)]
    SessionAttached {
        session_id: String,
        rpc_endpoint: String,
    },
    /// Server is ready to accept the next `Continue` request.
    ReadyForContinue,
    /// Historical block at this slot has been fully executed.
    SlotNotification(u64),
    /// High-level progress updates during a `Continue` call.
    Status { status: String },
    /// All blocks in the requested range have been executed.
    Completed { summary: Option<serde_json::Value> },
    /// Acknowledgement for `CloseBacktestSession`.
    Success,
    /// Server-side error.
    Error(Value),
}


// ── URL helpers ────────────────────────────────────────────────────────────────

/// Convert a WebSocket URL to its HTTP equivalent, keeping only scheme + host.
///   ws://host/path  → http://host
///   wss://host/path → https://host
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

/// Resolve `endpoint` against `base`. If `endpoint` is already absolute,
/// return it unchanged; otherwise prefix it with `base`.
fn resolve_url(base: &str, endpoint: &str) -> Result<String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }
    let path = endpoint.trim_start_matches('/');
    Ok(format!("{base}/{path}"))
}

// ── Session ────────────────────────────────────────────────────────────────────

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Session {
    ws: WsStream,
    /// Server-assigned session identifier, e.g. `backtest_<uuid>`.
    #[allow(dead_code)]
    session_id: String,
    /// Full HTTP URL for JSON-RPC queries, e.g. `http://host/backtest/backtest_<uuid>`.
    rpc_endpoint: String,
    http: reqwest::Client,
    api_key: String,
}

impl Session {
    /// Open the control WebSocket, send `CreateBacktestSession`, and wait for
    /// the first `ReadyForContinue` before returning.
    async fn create(cli: &Cli) -> Result<Self> {
        // ── 1. Build the WebSocket upgrade request ─────────────────────────────
        //
        // `into_client_request()` parses the URL and produces an HTTP/1.1
        // Upgrade request that tokio-tungstenite can use. We inject the API
        // key as a custom header.
        //
        // We also derive an HTTP base URL from the WebSocket URL so we can
        // resolve relative `rpc_endpoint` paths returned by the server:
        //   ws://host/...  → http://host
        //   wss://host/... → https://host
        let http_base = ws_url_to_http_base(&cli.url)?;

        let mut req = cli
            .url
            .as_str()
            .into_client_request()
            .context("invalid WebSocket URL")?;
        req.headers_mut().insert(
            "X-API-Key",
            HeaderValue::from_str(&cli.api_key).context("invalid API key header value")?,
        );

        eprintln!("[ws] connecting to {}", cli.url);
        let (ws, _http_resp) = connect_async(req)
            .await
            .context("WebSocket handshake failed")?;
        eprintln!("[ws] connected");

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        let mut ws = ws;

        // ── 2. Send CreateBacktestSession ──────────────────────────────────────
        //
        // Example wire JSON:
        // {
        //   "method": "createBacktestSession",
        //   "params": {
        //     "startSlot": 300000000,
        //     "endSlot": 300000010,
        //     "signerFilter": [],
        //     "preloadPrograms": [],
        //     "preloadAccountBundles": [],
        //     "sendSummary": true
        //   }
        // }
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

        // ── 3. Wait for SessionCreated ─────────────────────────────────────────
        //
        // The server may emit Status and ReadyForContinue before or alongside
        // SessionCreated; buffer them by looping.
        let (session_id, rpc_endpoint) = loop {
            match ws_recv(&mut ws).await? {
                WsResponse::SessionCreated {
                    session_id,
                    rpc_endpoint,
                } => {
                    // The server may return a relative path; resolve it
                    // against the HTTP base derived from the WebSocket URL.
                    let rpc_endpoint = resolve_url(&http_base, &rpc_endpoint)?;
                    eprintln!("[ws] session_id:    {session_id}");
                    eprintln!("[ws] rpc_endpoint:  {rpc_endpoint}");
                    break (session_id, rpc_endpoint);
                }
                WsResponse::Error(e) => bail!("error creating session: {e}"),
                other => eprintln!("[ws] (setup) {other:?}"),
            }
        };

        let mut session = Self {
            ws,
            session_id,
            rpc_endpoint,
            http,
            api_key: cli.api_key.clone(),
        };

        // ── 4. Wait for the initial ReadyForContinue ───────────────────────────
        eprintln!("[ws] waiting for initial ReadyForContinue...");
        let ready = session.drain_until_ready().await?;
        if !ready {
            bail!("session completed during startup before any Continue");
        }
        eprintln!("[ws] session is ready");

        Ok(session)
    }

    /// Read control messages until `ReadyForContinue` or `Completed`.
    ///
    /// Returns `true` if the session is ready for another `Continue`, or
    /// `false` if it completed (no more blocks).
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

    /// POST a JSON-RPC request to the session's HTTP endpoint.
    ///
    /// Wire request format:
    /// ```text
    /// POST {rpc_endpoint}
    /// Content-Type: application/json
    /// X-API-Key: {api_key}
    ///
    /// {"jsonrpc":"2.0","id":1,"method":"{method}","params":[...]}
    /// ```
    ///
    /// Wire response format:
    /// ```text
    /// {"jsonrpc":"2.0","id":1,"result": <value>}
    /// // or on error:
    /// {"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"..."}}
    /// ```
    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        let resp: Value = self
            .http
            .post(&self.rpc_endpoint)
            .header("X-API-Key", &self.api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("HTTP request to {} failed", self.rpc_endpoint))?
            .json()
            .await
            .context("failed to parse JSON-RPC response")?;

        if let Some(err) = resp.get("error") {
            bail!("JSON-RPC error from {method}: {err}");
        }

        Ok(resp["result"].clone())
    }

    /// Send `Continue` and wait for `ReadyForContinue` or `Completed`.
    ///
    /// `transactions` is an optional list of base64-encoded versioned
    /// transactions to inject and execute before the historical block TXs.
    ///
    /// Returns `true` if the session is ready for another `Continue`, `false`
    /// if all blocks in the range have been processed.
    async fn advance(&mut self, advance_count: u64, transactions: Vec<String>) -> Result<bool> {
        // Example wire JSON:
        // {"method":"continue","params":{"advanceCount":1,"transactions":[]}}
        ws_send(
            &mut self.ws,
            &WsRequest::Continue(ContinueParams {
                advance_count,
                transactions,
            }),
        )
        .await?;

        self.drain_until_ready().await
    }

    /// Send `CloseBacktestSession`, wait for acknowledgement, then close the WebSocket.
    async fn close(mut self) -> Result<()> {
        // Example wire JSON: {"method":"closeBacktestSession"}
        let _ = ws_send(&mut self.ws, &WsRequest::CloseBacktestSession).await;

        // Drain for up to 10 s waiting for Success/Completed/close.
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

// ── Low-level WebSocket helpers ────────────────────────────────────────────────

async fn ws_send(ws: &mut WsStream, msg: &WsRequest) -> Result<()> {
    let json = serde_json::to_string(msg).context("serializing request")?;
    eprintln!("[ws ->] {json}");
    ws.send(Message::Text(json))
        .await
        .context("WebSocket send failed")?;
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
            // Control frames — keep reading.
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

    // ── 2. Query initial chain state via JSON-RPC ──────────────────────────────
    //
    // All standard Solana JSON-RPC methods are available, scoped to the
    // simulated state at the current slot.

    // getSlot — the current simulated slot number.
    let slot = session.rpc("getSlot", json!([])).await?;
    println!("current slot:    {slot}");

    // getLatestBlockhash — needed when building and signing custom transactions.
    let blockhash_resp = session.rpc("getLatestBlockhash", json!([])).await?;
    let blockhash = &blockhash_resp["value"]["blockhash"];
    println!("latest blockhash: {blockhash}");

    // getAccountInfo — inspect any account's on-chain state.
    // Here we check the System Program (all zeros key).
    let system_program = "11111111111111111111111111111111";
    let account_resp = session
        .rpc(
            "getAccountInfo",
            json!([system_program, {"encoding": "base64"}]),
        )
        .await?;
    println!("system program:  {account_resp}");

    // getBalance — SOL balance of any account, in lamports.
    let balance = session.rpc("getBalance", json!([system_program])).await?;
    println!("system program balance: {balance} lamports");

    // ── 3. Advance through all blocks ─────────────────────────────────────────
    //
    // Each `Continue` call executes `advance_count` historical blocks inside
    // the simulator. The server sends `SlotNotification` for each slot and
    // `ReadyForContinue` when it's ready for the next call (or `Completed`
    // when there are no more blocks).
    eprintln!();
    eprintln!(
        "advancing slots {}..={} (one block at a time)...",
        cli.start_slot, cli.end_slot
    );

    loop {
        // Inject no custom transactions here; pass an empty vec.
        let more_blocks = session.advance(1, vec![]).await?;

        if !more_blocks {
            eprintln!("all blocks processed");
            break;
        }

        // After each advance we can query the updated state.
        let slot = session.rpc("getSlot", json!([])).await?;
        println!("slot after advance: {slot}");
    }

    // ── 4. Tear down ──────────────────────────────────────────────────────────
    session.close().await?;
    println!("session closed");

    Ok(())
}

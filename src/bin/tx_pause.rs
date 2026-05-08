//! Pause before each matching batch and inspect/simulate against frozen chain state.
//!
//! Creates a backtest session with a `ProgramExecuted` discovery filter for the
//! Jupiter V6 aggregator (or any program of your choice).  For each matching
//! batch the simulator pauses immediately before any of its transactions execute,
//! giving you a window to:
//!
//! - Inspect account state via `session.rpc()` — reads reflect the chain up to
//!   `batch_index - 1`, so the matched transactions have NOT yet run.
//! - Call `session.rpc().simulate_transaction(&your_tx)` to test your routing
//!   against exactly this chain state.  Nothing is committed on-chain.
//!
//! After your inspection, the session jumps directly to the next matching batch.

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{BacktestClient, CreateSession, DiscoveryStepResult};
use solana_address::Address;
use solana_transaction::versioned::VersionedTransaction;

const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const DART_TX_SIG: &str = "u6tf2YYLvDyG1HYfBUP9KqssUSZx3hebQMDJYh9Mug9CxDPKzTeNPgaoMZ92VPhwcCuByQqJeKqCTmo3fzgsohc";
const HELIUS_RPC: &str = "https://mainnet.helius-rpc.com/?api-key=b364b14c-d412-4660-a6c0-ff766b7b7441";

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
    #[arg(long, default_value_t = 417_811_170)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 417_811_190)]
    end_slot: u64,

    /// Program to watch; defaults to Jupiter V6 aggregator.
    #[arg(long, default_value = JUPITER_V6)]
    program_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let program_addr: Address = cli.program_id.parse().context("invalid --program-id")?;

    let dart_tx: VersionedTransaction = {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTransaction",
            "params": [DART_TX_SIG, {"encoding": "base64", "maxSupportedTransactionVersion": 0}]
        });
        let resp: serde_json::Value = reqwest::Client::new()
            .post(HELIUS_RPC)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        let encoded = resp["result"]["transaction"][0]
            .as_str()
            .context("dart tx not found")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("base64 decode dart tx")?;
        bincode::deserialize(&bytes).context("deserialize dart tx")?
    };
    eprintln!("[dart] loaded tx: {DART_TX_SIG}");

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
                // Register the discovery filter: the server announces each batch
                // containing a transaction that invokes this program.
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(program_addr)])
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));
    session.ensure_ready(Some(Duration::from_secs(600))).await?;
    eprintln!("[ws] ready — scanning for {} batches", cli.program_id);

    let timeout = Some(Duration::from_secs(120));
    let mut pause_count = 0u64;

    loop {
        match session.advance_to_discovery(timeout).await? {
            DiscoveryStepResult::Paused(pause) => {
                pause_count += 1;

                let slot = pause.paused.slot;
                let batch = pause.paused.batch_index.unwrap_or(0);
                eprintln!("[pause #{pause_count}] slot={slot} batch={batch}");

                // Filter to SOL→USDC swaps: tx must reference both mints.
                let sol_usdc_txs: Vec<VersionedTransaction> = pause
                    .discovery
                    .transactions
                    .iter()
                    .filter_map(|bin| {
                        let bytes = bin.decode().ok()?;
                        let tx: VersionedTransaction = bincode::deserialize(&bytes).ok()?;
                        let keys: Vec<String> = tx
                            .message
                            .static_account_keys()
                            .iter()
                            .map(|k| k.to_string())
                            .collect();
                        let has_wsol = keys.iter().any(|k| k == WSOL_MINT);
                        let has_usdc = keys.iter().any(|k| k == USDC_MINT);
                        if has_wsol && has_usdc { Some(tx) } else { None }
                    })
                    .collect();

                if sol_usdc_txs.is_empty() {
                    eprintln!("  [skip] no SOL→USDC swaps in this batch");
                    continue;
                }

                for tx in &sol_usdc_txs {
                    let sig = tx
                        .signatures
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    eprintln!("  [SOL→USDC] jup tx: {sig}");
                }

                let current = session.rpc().get_slot().await?;
                eprintln!("  rpc slot: {current}");

                for tx in &sol_usdc_txs {
                    let jup_result = session
                        .rpc()
                        .simulate_transaction(tx)
                        .await
                        .context("simulate jup tx failed")?;

                    let dart_result = session
                        .rpc()
                        .simulate_transaction(&dart_tx)
                        .await
                        .context("simulate dart tx failed")?;

                    let jup_return = jup_result.value.logs.as_ref()
                        .and_then(|logs| logs.iter().find(|l| l.starts_with("Program return:")))
                        .cloned()
                        .unwrap_or_else(|| "no return value".to_string());

                    let dart_return = dart_result.value.logs.as_ref()
                        .and_then(|logs| logs.iter().find(|l| l.starts_with("Program return:")))
                        .cloned()
                        .unwrap_or_else(|| "no return value".to_string());

                    eprintln!("  [compare]");
                    eprintln!("    jup:  {jup_return}");
                    eprintln!("    dart: {dart_return}");
                }

            }

            DiscoveryStepResult::Completed => {
                eprintln!("[done] session completed; total pauses: {pause_count}");
                break;
            }
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;
    Ok(())
}

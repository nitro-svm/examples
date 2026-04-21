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
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{BacktestClient, Continue, CreateSession, DiscoveryStepResult};
use solana_address::Address;

const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";

#[derive(Parser)]
#[command(about = "Pause before each discovered batch and inspect frozen chain state")]
struct Cli {
    /// Simulator base URL (no scheme), e.g. `staging.simulator.example.com`.
    #[arg(long, default_value = "localhost:8900")]
    url: String,

    /// API key sent as the `X-API-Key` header.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 399_834_992)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 399_835_010)]
    end_slot: u64,

    /// Program to watch; defaults to Jupiter V6 aggregator.
    #[arg(long, default_value = JUPITER_V6)]
    program_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let program_addr: Address = cli.program_id.parse().context("invalid --program-id")?;

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
    session
        .ensure_ready(Some(Duration::from_secs(600)))
        .await?;
    eprintln!("[ws] ready — scanning for {} batches", cli.program_id);

    let timeout = Some(Duration::from_secs(120));
    let mut pause_count = 0u64;

    loop {
        // Use u64::MAX so the session runs freely until the next discovery
        // or until the slot range is exhausted — no slot-by-slot polling.
        let cont = Continue::builder().advance_count(u64::MAX).build();

        match session.advance_to_discovery(cont, timeout).await? {
            DiscoveryStepResult::Paused(pause) => {
                pause_count += 1;

                let slot = pause.paused.slot;
                let batch = pause.paused.batch_index.unwrap_or(0);
                eprintln!("[pause #{pause_count}] slot={slot} batch={batch}");

                for sig in &pause.discovery.signatures {
                    eprintln!("  matched tx: {sig}");
                }

                // ── Inspect chain state ───────────────────────────────────────
                // State here reflects all committed transactions *before* this
                // batch.  The matched transactions have NOT run yet.
                //
                // Example: confirm the RPC reflects the pause point:
                let current = session.rpc().get_slot().await?;
                eprintln!("  rpc slot: {current}");

                // ── Simulate a custom transaction ─────────────────────────────
                // Build `my_tx` with your routing logic, then:
                //
                //   let result = session
                //       .rpc()
                //       .simulate_transaction(&my_tx)
                //       .await
                //       .context("simulate failed")?;
                //   eprintln!("  simulate logs: {:?}", result.value.logs);
                //
                // Nothing is sent on-chain; the session state is unchanged.
            }

            DiscoveryStepResult::Ready => {
                // Shouldn't happen with advance_count=u64::MAX, but handle
                // defensively in case the server caps advance windows.
                continue;
            }

            DiscoveryStepResult::Completed => {
                eprintln!("[done] session completed; total pauses: {pause_count}");
                break;
            }
        }
    }

    session.close(Some(Duration::from_secs(10))).await?;
    Ok(())
}

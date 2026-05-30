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

use backtest_example::utils::parse::{
    JUPITER_V6, TOKEN_PROGRAM, USDC_MINT, WSOL_MINT, derive_ata, extract_signer,
    get_titan_template_transaction, parse_jupiter_swap_result, parse_titan_sim_result,
    patch_titan_template_transaction,
};
use backtest_example::utils::{accounts::set_account_balance, types::TxWithMeta};

use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::{
    AccountData, AccountModifications, BinaryEncoding, DiscoveryFilter, EncodedBinary,
};
use simulator_client::{BacktestClient, BacktestSession, CreateSession, DiscoveryStepResult};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

const SOL_TO_USDC_TEMPLATE: &str =
    "24RysBDMt3gavdURB1H835C9KBC5ovsAdQ9AhdJ3HwccX9dvk29mNQkeUAKqUfHEC8UeqecoGkPqCKe2TViVF45Y";
const USDC_TO_SOL_TEMPLATE: &str =
    "2RtLqCUeYBVhRppiJ2DFZoyVcwuJtPWprauRFfocynoiREYrGeJoqbpLM8bKsJkSoYpgr4oLnYEwCvrpDpiEZZV8";
// "u6tf2YYLvDyG1HYfBUP9KqssUSZx3hebQMDJYh9Mug9CxDPKzTeNPgaoMZ92VPhwcCuByQqJeKqCTmo3fzgsohc";

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
    #[arg(long, default_value_t = 417_811_175)]
    end_slot: u64,

    /// CSV output file path.
    #[arg(long, default_value = "results.csv")]
    output: String,

    /// Program to watch; defaults to Jupiter V6 aggregator.
    #[arg(long, default_value = JUPITER_V6)]
    program_id: String,
}

struct SwapRecord {
    slot: u64,
    signature: String,
    input_mint: &'static str,
    output_mint: &'static str,
    input_amount: u64,
    jup_out: u64,
    jup_quote: Option<u64>,
    jup_venues: Vec<(String, u64)>,
    titan_out: u64,
    titan_venues: Vec<(String, u64)>,
}

struct Template {
    transaction: VersionedTransaction,
    signer: Pubkey,
    in_ata: Pubkey,
    in_mint: &'static str,
    out_mint: &'static str,
}

fn write_output(filename: &str, records: &[SwapRecord]) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(
        w,
        "slot,tx_sig,input_mint,output_mint,input_amount,jup_out,jup_quote,jup_venues,titan_out,titan_venues"
    )?;
    for r in records {
        let fmt = |v: &[(String, u64)]| -> String {
            v.iter()
                .map(|(p, a)| format!("{p}:{a}"))
                .collect::<Vec<_>>()
                .join("|")
        };
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{}",
            r.slot,
            r.signature,
            r.input_mint,
            r.output_mint,
            r.input_amount,
            r.jup_out,
            r.jup_quote.map_or(String::new(), |q| q.to_string()),
            fmt(&r.jup_venues),
            r.titan_out,
            fmt(&r.titan_venues),
        )?;
    }
    Ok(())
}

async fn get_template_transactions() -> Result<[Template; 2]> {
    let usdc_to_sol = {
        let tx = get_titan_template_transaction(USDC_TO_SOL_TEMPLATE).await?;
        let signer = extract_signer(&tx)?;
        let ata = derive_ata(&signer, USDC_MINT).context("derive USDC ATA")?;
        Template {
            transaction: tx,
            signer,
            in_ata: ata,
            in_mint: USDC_MINT,
            out_mint: WSOL_MINT,
        }
    };

    let sol_to_usdc = {
        let tx = get_titan_template_transaction(SOL_TO_USDC_TEMPLATE).await?;
        let signer = extract_signer(&tx)?;
        let ata = derive_ata(&signer, WSOL_MINT).context("derive WSOL ATA")?;
        Template {
            transaction: tx,
            signer,
            in_ata: ata,
            in_mint: WSOL_MINT,
            out_mint: USDC_MINT,
        }
    };

    Ok([usdc_to_sol, sol_to_usdc])
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

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
                .capacity_wait_timeout_secs(900u16)
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(program_addr)])
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));
    session.ensure_ready(Some(Duration::from_secs(600))).await?;
    eprintln!("[ws] ready — scanning for {} batches", cli.program_id);

    let timeout = Some(Duration::from_secs(120));
    let mut pause_count = 0u64;
    let mut records: Vec<SwapRecord> = Vec::new();
    let template_txs = get_template_transactions().await?;

    loop {
        match session.advance_to_discovery(timeout).await? {
            DiscoveryStepResult::Paused(pause) => {
                pause_count += 1;

                let slot = pause.paused.slot;
                let batch = pause.paused.batch_index.unwrap_or(0);
                eprintln!("[pause #{pause_count}] slot={slot} batch={batch}");

                let txs: Vec<TxWithMeta> = pause
                    .discovery
                    .transactions
                    .iter()
                    .filter_map(|bin| {
                        let bytes = bin.decode().ok()?;
                        bincode::deserialize(&bytes).ok()
                    })
                    .collect();

                for tx_with_meta in &txs {
                    let signature = tx_with_meta
                        .transaction
                        .signatures
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_default();

                    let (jup_swap, original_template) = if let Some(s) =
                        parse_jupiter_swap_result(tx_with_meta, USDC_MINT, WSOL_MINT)
                    {
                        (s, &template_txs[0])
                    } else if let Some(s) =
                        parse_jupiter_swap_result(tx_with_meta, WSOL_MINT, USDC_MINT)
                    {
                        (s, &template_txs[1])
                    } else {
                        continue;
                    };

                    eprintln!("  [compare] sig={signature} input={}", jup_swap.in_amount);
                    eprintln!(
                        "    jup:   out={} venues={:?}",
                        jup_swap.out_amount, jup_swap.venues
                    );

                    let original_balance = set_account_balance(
                        &session,
                        &original_template.signer,
                        original_template.in_mint,
                        jup_swap.in_amount,
                        true,
                    )
                    .await?;

                    let modified_template = patch_titan_template_transaction(
                        &original_template.transaction,
                        original_template.in_ata,
                        jup_swap.in_amount,
                    )
                    .context("patch_titan_template_transaction failed")?;

                    let titan_result = session
                        .rpc()
                        .simulate_transaction(&modified_template)
                        .await
                        .context("simulate titan tx failed")?;

                    let titan_swap = parse_titan_sim_result(&titan_result.value);
                    let titan_err = titan_result
                        .value
                        .logs
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .find(|l| l.contains("Error Message:"))
                        .cloned();
                    eprintln!(
                        "    titan: out={} venues={:?} err={:?}",
                        titan_swap.out_amount, titan_swap.venues, titan_err
                    );

                    set_account_balance(
                        &session,
                        &original_template.signer,
                        original_template.in_mint,
                        original_balance,
                        true,
                    )
                    .await?;

                    records.push(SwapRecord {
                        slot,
                        signature,
                        input_mint: original_template.in_mint,
                        output_mint: original_template.out_mint,
                        input_amount: jup_swap.in_amount,
                        jup_out: jup_swap.out_amount,
                        jup_quote: jup_swap.quote_amount,
                        jup_venues: jup_swap.venues,
                        titan_out: titan_swap.out_amount,
                        titan_venues: titan_swap.venues,
                    });
                }
            }

            DiscoveryStepResult::Completed => {
                eprintln!("[done] session completed; total pauses: {pause_count}");
                break;
            }
        }
    }

    write_output(&cli.output, &records)?;
    eprintln!("[done] wrote {} rows to {}", records.len(), cli.output);

    let _ = session.close(Some(Duration::from_secs(10))).await;
    Ok(())
}

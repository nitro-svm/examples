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
    USDC_MINT, WSOL_MINT, derive_ata, extract_signer,
    get_titan_template_transaction, parse_titan_sim_result,
    patch_titan_template_transaction,
};
use backtest_example::utils::{accounts::set_account_balance, types::TxWithMeta};

use std::io::{BufWriter, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{BacktestClient, BacktestSession, Continue, CreateSession, DiscoveryStepResult};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

const PROGRAM_ID: &str = "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi";

mod titan_template_v3 {
    pub const SOL_TO_USDC: &str =
        "24RysBDMt3gavdURB1H835C9KBC5ovsAdQ9AhdJ3HwccX9dvk29mNQkeUAKqUfHEC8UeqecoGkPqCKe2TViVF45Y";
    pub const USDC_TO_SOL: &str =
        "2RtLqCUeYBVhRppiJ2DFZoyVcwuJtPWprauRFfocynoiREYrGeJoqbpLM8bKsJkSoYpgr4oLnYEwCvrpDpiEZZV8";
}

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
    #[arg(long, default_value_t = 422_818_048)]
    start_slot: u64,

    /// Last slot (inclusive) to replay.
    #[arg(long, default_value_t = 422_818_148)]
    end_slot: u64,

    /// CSV output file path.
    #[arg(long, default_value = "results.csv")]
    output: String,

    /// Program to watch.
    #[arg(long, default_value = PROGRAM_ID)]
    program_id: String,

    #[arg(long, default_value = USDC_MINT)]
    quote_mint: String,

    #[arg(long, default_value = WSOL_MINT)]
    base_mint: String,

    /// Size should be 0.1-15 of pool depth. 
    /// (Too big would measure price impact instead of spread)
    #[arg(long, default_value_t = 5_000_000_000)]
    size: u64,
}

struct SpreadResult {
    slot: u64,
    quote_mint: String,
    base_mint: String,
    input_amount: u64,
    output_amount: u64,
    spread_bps: f64,
}

struct Template {
    quote_to_base: VersionedTransaction,
    base_to_quote: VersionedTransaction,
    quote_mint: Address,
    base_mint: Address,
    quote_signer: Pubkey,
    base_signer: Pubkey,
    quote_ata: Pubkey,
    base_ata: Pubkey,
}

fn write_output(filename: &str, records: &[SpreadResult]) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(w, "slot,quote_mint,base_mint,input_amount,output_amount,spread_bps")?;
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{}",
            r.slot, r.quote_mint, r.base_mint, r.input_amount, r.output_amount, r.spread_bps,
        )?;
    }
    Ok(())
}

async fn get_template() -> Result<Template> {
    let usdc_to_sol = get_titan_template_transaction(titan_template_v3::USDC_TO_SOL).await?;
    let sol_to_usdc = get_titan_template_transaction(titan_template_v3::SOL_TO_USDC).await?;
    let quote_signer = extract_signer(&usdc_to_sol)?;
    let base_signer = extract_signer(&sol_to_usdc)?;
    let quote_ata = derive_ata(&quote_signer, USDC_MINT).context("derive quote ATA")?;
    let base_ata = derive_ata(&base_signer, WSOL_MINT).context("derive base ATA")?;

    Ok(Template {
        quote_to_base: usdc_to_sol,
        base_to_quote: sol_to_usdc,
        quote_mint: Address::from_str_const(USDC_MINT),
        base_mint: Address::from_str_const(WSOL_MINT),
        quote_signer,
        base_signer,
        quote_ata,
        base_ata,
    })
}

const BPF_UPGRADEABLE_LOADER: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

fn is_program_upgrade(tx: &VersionedTransaction, program_id: &str) -> bool {
    let keys = tx.message.static_account_keys();
    let key_str: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    let Some(loader_idx) = key_str.iter().position(|k| k == BPF_UPGRADEABLE_LOADER) else {
        return false;
    };
    for ix in tx.message.instructions() {
        if ix.program_id_index as usize != loader_idx {
            continue;
        }
        // Upgrade discriminant = 3u32 LE
        if ix.data.get(..4) != Some(&[3, 0, 0, 0]) {
            continue;
        }
        // Account slot 1 in the Upgrade instruction is the program account
        if let Some(&prog_idx) = ix.accounts.get(1) {
            if key_str.get(prog_idx as usize).map(|s| s.as_str()) == Some(program_id) {
                return true;
            }
        }
    }
    false
}

async fn simulate_single_swap(session: &BacktestSession, tx: &VersionedTransaction) -> Result<u64> {
    let titan_result = session
        .rpc()
        .simulate_transaction(tx)
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

    Ok(titan_swap.out_amount)
}

async fn simulate_roundtrip_swap(
    session: &BacktestSession, 
    size: u64, 
    template: &Template, 
) -> Result<u64> {
    let quote_mint = template.quote_mint.to_string();
    let base_mint = template.base_mint.to_string();

    let original_quote = set_account_balance(
        &session,
        &template.quote_signer,
        &quote_mint,
        size,
        true,
    )
    .await?;
    let quote_to_base = patch_titan_template_transaction(&template.quote_to_base, template.quote_ata, size)?;
    let intermediate_out = simulate_single_swap(session, &quote_to_base).await?;

    let original_base = set_account_balance(
        &session,
        &template.base_signer,
        &base_mint,
        intermediate_out,
        true,
    )
    .await?;
    let base_to_quote = patch_titan_template_transaction(&template.base_to_quote, template.base_ata, intermediate_out)?;
    let final_out = simulate_single_swap(session, &base_to_quote).await?;

    let (r1, r2) = tokio::join!(
        set_account_balance(&session, &template.quote_signer, &quote_mint, original_quote, true),
        set_account_balance(&session, &template.base_signer, &base_mint, original_base, true),
    );
    r1?;
    r2?;

    Ok(final_out)
}

async fn run_discovery_session(
    client: BacktestClient, 
    program_addr: Address, 
    cli: Cli,
    template: &Template,
) -> Result<Vec<SpreadResult>> {
    let mut pause_count = 0u64;
    let timeout = Some(Duration::from_secs(120));
    let mut records: Vec<SpreadResult> = Vec::new();

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

                // if !txs.iter().any(|tx| is_program_upgrade(&tx.transaction, &cli.program_id)) {
                //     continue;
                // }

                let out_amount = simulate_roundtrip_swap(&session, cli.size, &template).await?;

                if out_amount == 0 {
                    continue;
                }

                records.push(SpreadResult {
                    slot,
                    quote_mint: template.quote_mint.to_string(),
                    base_mint: template.base_mint.to_string(),
                    input_amount: cli.size,
                    output_amount: out_amount,
                    spread_bps: (cli.size - out_amount) as f64 / cli.size as f64 * 10_000.0,
                });
            }

            DiscoveryStepResult::Completed => {
                eprintln!("[done] session completed; total pauses: {pause_count}");
                break;
            }
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;

    Ok(records)
}

async fn run_regular_session(client: BacktestClient, cli: Cli, template: &Template) -> Result<Vec<SpreadResult>> {
    let mut records: Vec<SpreadResult> = Vec::new();
    let timeout = Some(Duration::from_secs(120));

    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .build(),
        )
        .await?;

    session.ensure_ready(Some(Duration::from_secs(600))).await?;

    loop {
        let result = session
            .advance(Continue::builder().advance_count(1).build(), timeout, |_| {})
            .await?;

        let slot = result.last_slot.unwrap_or(0);
        let out_amount = simulate_roundtrip_swap(&session, cli.size, template).await?;

        if out_amount > 0 {
            records.push(SpreadResult {
                slot,
                quote_mint: template.quote_mint.to_string(),
                base_mint: template.base_mint.to_string(),
                input_amount: cli.size,
                output_amount: out_amount,
                spread_bps: (cli.size - out_amount) as f64 / cli.size as f64 * 10_000.0,
            });
        }

        if result.completed {
            break;
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;
    Ok(records)
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let template = get_template().await?;
    let cli = Cli::parse();
    let client = BacktestClient::builder()
        .url(format!("wss://{}/backtest", &cli.url))
        .api_key(cli.api_key.clone())
        .build();

    eprintln!("[ws] connecting to wss://{}/backtest", &cli.url);

    let output = cli.output.clone();
    let program_addr = cli.program_id.parse::<Address>().ok();
    let records = if let Some(program_addr) = program_addr {
        run_discovery_session(client, program_addr, cli, &template).await?
    } else {
        run_regular_session(client, cli, &template).await?
    };

    write_output(&output, &records)?;
    eprintln!("[done] wrote {} rows to {}", records.len(), output);
    
    Ok(())
}

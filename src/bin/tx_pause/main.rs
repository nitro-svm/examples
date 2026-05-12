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

mod utils;
use utils::{
    JUPITER_V6, TOKEN_PROGRAM, USDC_MINT, WSOL_MINT, derive_ata, extract_signer,
    get_titan_template_transaction, parse_jupiter_swap_result, parse_titan_sim_events,
    patch_titan_template_transaction,
};

use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use history_model::TxWithMeta;
use simulator_api::{
    AccountData, AccountModifications, BinaryEncoding, DiscoveryFilter, EncodedBinary,
};
use simulator_client::{BacktestClient, BacktestSession, CreateSession, DiscoveryStepResult};
use solana_address::Address;

const TEMPLATE_TX: &str =
    "u6tf2YYLvDyG1HYfBUP9KqssUSZx3hebQMDJYh9Mug9CxDPKzTeNPgaoMZ92VPhwcCuByQqJeKqCTmo3fzgsohc";

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
    jup_venues: Vec<(String, u64)>,
    titan_out: u64,
    titan_venues: Vec<(String, u64)>,
}

fn make_token_account(signer: &Address, mint: &str, amount: u64) -> Result<AccountData> {
    let mut data = [0u8; 165];
    data[0..32].copy_from_slice(mint.parse::<solana_pubkey::Pubkey>()?.as_ref());
    data[32..64].copy_from_slice(signer.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;

    Ok(AccountData {
        data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
        executable: false,
        lamports: 2_039_280,
        owner: TOKEN_PROGRAM.parse()?,
        space: 165,
    })
}

async fn set_account_modifications(
    session: &BacktestSession,
    owner: &solana_pubkey::Pubkey,
    mint: &str,
    new_balance: u64,
) -> Result<u64> {
    let ata = derive_ata(owner, mint).context("should derive ata")?;
    let owner_addr: Address = owner.to_string().parse()?;

    let original_balance = session
        .rpc()
        .get_account(&ata)
        .await
        .ok()
        .filter(|a| a.data.len() >= 72)
        .map(|a| u64::from_le_bytes(a.data[64..72].try_into().unwrap()))
        .unwrap_or(0);

    let modifications = AccountModifications(BTreeMap::from([(
        ata.to_string().parse::<Address>()?,
        make_token_account(&owner_addr, mint, new_balance)?,
    )]));
    session
        .modify_accounts(&modifications)
        .await
        .context("modify_accounts failed")?;
    Ok(original_balance)
}

fn write_output(filename: &str, records: &[SwapRecord]) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(
        w,
        "slot,tx_sig,input_mint,output_mint,input_amount,jup_out,jup_venues,titan_out,titan_venues"
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
            "{},{},{},{},{},{},{},{},{}",
            r.slot,
            r.signature,
            r.input_mint,
            r.output_mint,
            r.input_amount,
            r.jup_out,
            fmt(&r.jup_venues),
            r.titan_out,
            fmt(&r.titan_venues),
        )?;
    }
    Ok(())
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

    let base_template = get_titan_template_transaction(TEMPLATE_TX).await?;
    let signer = extract_signer(&base_template)?;
    let in_ata = derive_ata(&signer, USDC_MINT).context("could not derive signer's input ATA")?;

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

                eprintln!("  [batch] {} JUP txs discovered", txs.len());
                for tx_with_meta in &txs {
                    let signature = tx_with_meta
                        .transaction
                        .signatures
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_default();

                    let jup_swap =
                        match parse_jupiter_swap_result(tx_with_meta, USDC_MINT, WSOL_MINT) {
                            Some(s) => s,
                            None => continue,
                        };

                    eprintln!("  [compare] sig={signature} input={}", jup_swap.in_amount);
                    eprintln!(
                        "    jup:   out={} venues={:?}",
                        jup_swap.out_amount, jup_swap.venues
                    );

                    let original_balance =
                        set_account_modifications(&session, &signer, USDC_MINT, jup_swap.in_amount)
                            .await?;

                    let modified_template = patch_titan_template_transaction(
                        &base_template,
                        in_ata,
                        jup_swap.in_amount,
                    )
                    .context("patch_titan_template_transaction failed")?;

                    let titan_result = session
                        .rpc()
                        .simulate_transaction(&modified_template)
                        .await
                        .context("simulate titan tx failed")?;

                    let titan_swap = parse_titan_sim_events(&titan_result.value);
                    eprintln!(
                        "    titan: out={} venues={:?}",
                        titan_swap.out_amount, titan_swap.venues
                    );

                    set_account_modifications(&session, &signer, USDC_MINT, original_balance)
                        .await?;

                    records.push(SwapRecord {
                        slot,
                        signature,
                        input_mint: USDC_MINT,
                        output_mint: WSOL_MINT,
                        input_amount: jup_swap.in_amount,
                        jup_out: jup_swap.out_amount,
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

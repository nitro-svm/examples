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
    extract_signer_and_input_ata,
    get_titan_template, parse_jupiter_swap_result, parse_titan_sim_result,
    derive_ata, JUPITER_V6, USDC_MINT, WSOL_MINT,
};
use std::collections::BTreeMap;

use std::io::{BufWriter, Write as _};
use std::time::Duration;

use solana_transaction::versioned::VersionedTransaction;

use anyhow::{Context, Result};
use clap::Parser;
use history_model::TxWithMeta;
use simulator_api::{AccountData, AccountModifications, BinaryEncoding, DiscoveryFilter, EncodedBinary};
use simulator_client::{BacktestClient, CreateSession, DiscoveryStepResult};
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
    tx_sig: String,
    input_mint: &'static str,
    output_mint: &'static str,
    input_amount: u64,
    jup_out: u64,
    jup_venues: Vec<(String, u64)>,
    titan_out: u64,
    titan_venues: Vec<(String, u64)>,
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
            r.tx_sig,
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
    let base_template = get_titan_template(TEMPLATE_TX).await?;

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

    loop {
        match session.advance_to_discovery(timeout).await? {
            DiscoveryStepResult::Paused(pause) => {
                pause_count += 1;

                let slot = pause.paused.slot;
                let batch = pause.paused.batch_index.unwrap_or(0);
                eprintln!("[pause #{pause_count}] slot={slot} batch={batch}");

                let txs: Vec<VersionedTransaction> = pause
                    .discovery
                    .transactions
                    .iter()
                    .filter_map(|bin| {
                        let bytes = bin.decode().ok()?;
                        bincode::deserialize(&bytes).ok()
                    })
                    .collect();

                for tx_with_meta in &txs {
                    // let jup_swap =
                    //     match parse_jupiter_swap_result(tx_with_meta, USDC_MINT, WSOL_MINT) {
                    //         Some(s) => s,
                    //         None => continue,
                    //     };

                    // let (signer, in_ata) = match extract_signer_and_input_ata(tx_with_meta, USDC_MINT) {
                    //     Ok(x) => x,
                    //     Err(_) => continue,
                    // };

                    // Inject USDC input ATA and wSOL output ATA so the baseline tx has valid accounts.
                    {
                        let signer = base_template.message.static_account_keys()[0];
                        let in_ata_key = utils::template_in_ata_addr(&base_template)
                            .context("no in_ata in template")?;
                        let out_ata_key = utils::derive_ata(&signer, WSOL_MINT)
                            .context("could not derive out_ata")?;

                        let make_token_acct = |mint: &str, amount: u64| -> Result<AccountData> {
                            let mut data = [0u8; 165];
                            data[0..32].copy_from_slice(mint.parse::<solana_pubkey::Pubkey>()?.as_ref());
                            data[32..64].copy_from_slice(signer.as_ref());
                            data[64..72].copy_from_slice(&amount.to_le_bytes());
                            data[108] = 1;
                            Ok(AccountData {
                                data: EncodedBinary::from_bytes(&data, BinaryEncoding::Base64),
                                executable: false,
                                lamports: 2_039_280,
                                owner: utils::TOKEN_PROGRAM.parse()?,
                                space: 165,
                            })
                        };

                        let mods = AccountModifications(BTreeMap::from([
                            (in_ata_key.to_string().parse::<Address>()?, make_token_acct(USDC_MINT, 30_000_000_000)?),
                            (out_ata_key.to_string().parse::<Address>()?, make_token_acct(WSOL_MINT, 0)?),
                        ]));
                        session.modify_accounts(&mods).await.context("modify_accounts failed")?;
                    }
                    // let modified_template =
                    //     match patch_titan_in_amount(&base_template, in_amount)
                    //     // match patch_titan_template_transaction(&base_template, signer, in_ata, jup_swap.in_amount)
                    //     {
                    //         Some(t) => t,
                    //         None => {
                    //             eprintln!("  [skip] could not patch titan template");
                    //             continue;
                    //         }
                    //     };

                    for (i, key) in base_template.message.static_account_keys().iter().enumerate() {
                        match session.rpc().get_account(key).await {
                            Ok(acct) => eprintln!("  [acct {i}] {key} owner={}", acct.owner),
                            Err(_)   => eprintln!("  [acct {i}] {key} owner=<not found>"),
                        }
                    }

                    // let patched = utils::relax_titan_slippage(&base_template).context("relax_titan_slippage failed")?;
                    let titan_result = session
                        .rpc()
                        .simulate_transaction(&base_template)
                        .await
                        .context("simulate titan tx failed")?;

                    for log in titan_result.value.logs.as_deref().unwrap_or(&[]) {
                        eprintln!("  [log] {log}");
                    }

                    let (titan_out, titan_venues) = parse_titan_sim_result(
                        &base_template,
                        &titan_result.value,
                        USDC_MINT,
                        WSOL_MINT,
                    );

                    let events = utils::parse_titan_events(
                        titan_result.value.logs.as_deref().unwrap_or(&[])
                    );
                    let keys = base_template.message.static_account_keys();
                    for e in &events {
                        if let Some(key) = keys.get(e.split_idx as usize) {
                            match session.rpc().get_account(key).await {
                                Ok(account) => eprintln!("    [split {}] owner={}", e.split_idx, account.owner),
                                Err(err) => eprintln!("    [split {}] get_account err: {err}", e.split_idx),
                            }
                        }
                    }

                    // let tx_sig = tx_with_meta
                    //     .transaction
                    //     .signatures
                    //     .first()
                    //     .map(|s| s.to_string())
                    //     .unwrap_or_default();

                    // eprintln!("  [compare] sig={tx_sig} input={}", jup_swap.in_amount);
                    // eprintln!(
                    //     "    jup:   out={} venues={:?}",
                    //     jup_swap.out_amount, jup_swap.venues
                    // );
                    eprintln!("    titan: out={titan_out} venues={titan_venues:?}");

                    // records.push(SwapRecord {
                    //     slot,
                    //     tx_sig,
                    //     input_mint: USDC_MINT,
                    //     output_mint: WSOL_MINT,
                    //     input_amount: jup_swap.in_amount,
                    //     jup_out: jup_swap.out_amount,
                    //     jup_venues: jup_swap.venues,
                    //     titan_out,
                    //     titan_venues,
                    // });

                    break;
                }
                break;
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

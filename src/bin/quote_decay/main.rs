//! Track Jupiter swap output decay across slots.
//!
//! Phase 1 (start_slot): discover the first 10 Jupiter swaps and capture their
//!   transactions, signers, and on-chain amounts.
//! Phase 2 (start_slot..=start_slot+50): simulate each captured swap against
//!   each slot's frozen chain state and record the output token amount so you
//!   can track slippage decay over time.

use backtest_example::utils::parse::{
    JUPITER_V6, WSOL_MINT,
    derive_ata, extract_signer, parse_any_jupiter_swap,
};
use backtest_example::utils::accounts::set_account_balance;
use backtest_example::utils::types::TxWithMeta;

use std::io::{BufWriter, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{BacktestClient, BacktestSession, Continue, CreateSession, DiscoveryStepResult};
use solana_address::Address;
use solana_client::rpc_config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_rpc_client_api::response::{UiAccountData, UiAccountEncoding};
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use solana_pubkey::Pubkey;

const SLOT_ADVANCE: u64 = 50;
const MAX_SWAPS: usize = 5;

#[derive(Parser)]
#[command(about = "Track Jupiter swap output decay across 50 slots")]
struct Cli {
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,
    /// Starting slot: collect swaps here, then simulate over the next 50 slots.
    #[arg(long, default_value_t = 417_811_170)]
    start_slot: u64,
    #[arg(long, default_value = "results.csv")]
    output: String,
    #[arg(long, default_value = JUPITER_V6)]
    program_id: String,
}

struct OriginalSwap {
    slot: u64,
    signature: String,
    transaction: VersionedTransaction,
    signer: Pubkey,
    in_mint: String,
    out_mint: String,
    in_amount: u64,
    out_amount: u64,
}

struct SimulatedSwap {
    signature: String,
    in_mint: String,
    out_mint: String,
    in_amount: u64,
    original_slot: u64,
    original_out: u64,
    sim_slot: u64,
    sim_out: u64,
}

fn build_client(cli: &Cli) -> BacktestClient {
    BacktestClient::builder()
        .url(format!("wss://{}/backtest", &cli.url))
        .api_key(cli.api_key.clone())
        .build()
}

// ── phase 1: collect ─────────────────────────────────────────────────────────

async fn collect_swaps(cli: &Cli) -> Result<Vec<OriginalSwap>> {
    let program_addr: Address = cli.program_id.parse().context("invalid --program-id")?;
    let mut session = build_client(cli)
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.start_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(program_addr)])
                .build(),
        )
        .await?;

    eprintln!("[collect] session: {}", session.session_id().unwrap_or("?"));
    // Do NOT call ensure_ready here: it silently discards DiscoveryBatch events
    // (via `_ => {}`), which the server pre-emits for a single slot before
    // ReadyForContinue. advance_to_discovery's Phase 1 loop handles both
    // DiscoveryBatch and ReadyForContinue correctly, so we go straight to it.

    let timeout = Some(Duration::from_secs(120));
    let mut captured: Vec<OriginalSwap> = Vec::new();

    loop {
        if captured.len() >= MAX_SWAPS {
            break;
        }
        match session.advance_to_discovery(timeout).await? {
            DiscoveryStepResult::Paused(event) => {
                if event.paused.slot != cli.start_slot {
                    break;
                }
                let txs: Vec<TxWithMeta> = event
                    .discovery
                    .transactions
                    .iter()
                    .filter_map(|bin| {
                        let bytes = bin.decode().ok()?;
                        bincode::deserialize(&bytes).ok()
                    })
                    .collect();

                for tx_with_meta in &txs {
                    if captured.len() >= MAX_SWAPS {
                        break;
                    }
                    let Some((in_mint, out_mint, swap_data)) =
                        parse_any_jupiter_swap(tx_with_meta) else { continue };

                    let tx = tx_with_meta.transaction.clone();
                    let Ok(signer) = extract_signer(&tx) else { continue };
                    let signature = tx.signatures.first().map(|s| s.to_string()).unwrap_or_default();

                    eprintln!(
                        "  [collect] sig={} {}->{}  in={} out={}",
                        &signature[..16], in_mint, out_mint,
                        swap_data.in_amount, swap_data.out_amount
                    );
                    captured.push(OriginalSwap {
                        slot: event.paused.slot,
                        signature,
                        transaction: tx,
                        signer,
                        in_mint,
                        out_mint,
                        in_amount: swap_data.in_amount,
                        out_amount: swap_data.out_amount,
                    });
                }
            }
            DiscoveryStepResult::Completed => break,
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;
    Ok(captured)
}

// ── phase 2: simulate ────────────────────────────────────────────────────────

fn out_ata(swap: &OriginalSwap) -> Option<String> {
    if swap.out_mint == WSOL_MINT {
        Some(swap.signer.to_string())
    } else {
        derive_ata(&swap.signer, &swap.out_mint).map(|k| k.to_string())
    }
}

async fn out_balance(session: &BacktestSession, swap: &OriginalSwap) -> u64 {
    let Some(addr_str) = out_ata(swap) else { return 0 };
    let Ok(addr) = addr_str.parse::<Address>() else { return 0 };
    let Ok(acct) = session.rpc().get_account(&addr).await else { return 0 };
    if swap.out_mint == WSOL_MINT {
        acct.lamports
    } else if acct.data.len() >= 72 {
        u64::from_le_bytes(acct.data[64..72].try_into().unwrap())
    } else {
        0
    }
}

fn parse_post_balance(
    result_accounts: &[Option<solana_rpc_client_api::response::UiAccount>],
    out_mint: &str,
) -> u64 {
    let Some(Some(acct)) = result_accounts.first() else { return 0 };
    if out_mint == WSOL_MINT {
        acct.lamports
    } else {
        let b64 = match &acct.data {
            UiAccountData::Binary(s, UiAccountEncoding::Base64) => s,
            _ => return 0,
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else { return 0 };
        if bytes.len() >= 72 {
            u64::from_le_bytes(bytes[64..72].try_into().unwrap())
        } else {
            0
        }
    }
}

async fn simulate_swap(session: &BacktestSession, swap: &OriginalSwap) -> Result<u64> {
    let pre = out_balance(session, swap).await;
    let original_in =
        set_account_balance(session, &swap.signer, &swap.in_mint, swap.in_amount).await?;

    let out_addr = out_ata(swap).context("could not derive out address")?;
    let config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        encoding: Some(UiTransactionEncoding::Base64),
        accounts: Some(RpcSimulateTransactionAccountsConfig {
            encoding: Some(UiAccountEncoding::Base64),
            addresses: vec![out_addr],
        }),
        ..Default::default()
    };

    let result = session
        .rpc()
        .simulate_transaction_with_config(&swap.transaction, config)
        .await
        .context("simulate_transaction_with_config failed")?;

    if let Some(err) = &result.value.err {
        eprintln!("    [sim err] sig={} {:?}", &swap.signature[..16], err);
    }

    let sim_out = result
        .value
        .accounts
        .as_deref()
        .map(|accts| parse_post_balance(accts, &swap.out_mint).saturating_sub(pre))
        .unwrap_or(0);

    set_account_balance(session, &swap.signer, &swap.in_mint, original_in).await?;

    Ok(sim_out)
}

async fn run_simulations(mut session: BacktestSession, swaps: &[OriginalSwap], start_slot: u64) -> Result<Vec<SimulatedSwap>> {
    let timeout = Some(Duration::from_secs(120));
    let mut records: Vec<SimulatedSwap> = Vec::new();

    for offset in 0..=SLOT_ADVANCE {
        let slot = start_slot + offset;
        eprintln!("[simulate] slot {} ({}/{SLOT_ADVANCE})", slot, offset);

        for swap in swaps {
            let sim_out = simulate_swap(&session, swap).await?;
            eprintln!(
                "  sig={} orig={} sim={}",
                &swap.signature[..16], swap.out_amount, sim_out
            );
            records.push(SimulatedSwap {
                signature: swap.signature.clone(),
                in_mint: swap.in_mint.clone(),
                out_mint: swap.out_mint.clone(),
                in_amount: swap.in_amount,
                original_slot: swap.slot,
                original_out: swap.out_amount,
                sim_slot: slot,
                sim_out,
            });
        }

        if offset < SLOT_ADVANCE {
            let result = session
                .advance(Continue::builder().advance_count(1).build(), timeout, |_| {})
                .await?;
            if result.completed {
                eprintln!("[simulate] session completed early at slot {slot}");
                break;
            }
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;
    Ok(records)
}

// ── output ───────────────────────────────────────────────────────────────────

fn write_output(path: &str, records: &[SimulatedSwap]) -> Result<()> {
    let f = std::fs::File::create(path)?;
    let mut w = BufWriter::new(f);
    writeln!(w, "transaction,in_mint,out_mint,in_amount,original_slot,original_out,simulated_slot,simulated_out")?;
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{}",
            r.signature, r.in_mint, r.out_mint, r.in_amount,
            r.original_slot, r.original_out, r.sim_slot, r.sim_out
        )?;
    }
    Ok(())
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider().install_default().ok();
    let cli = Cli::parse();

    // Create both sessions concurrently so Phase 2 is ready the moment Phase 1 finishes.
    eprintln!("[phase1] collecting up to {MAX_SWAPS} swaps from slot {}", cli.start_slot);
    let (captured, sim_session) = tokio::try_join!(
        collect_swaps(&cli),
        async {
            let program_addr: Address = cli.program_id.parse().context("invalid --program-id")?;
            let _ = program_addr; // suppress unused warning — no filter on Phase 2 session
            let mut s = build_client(&cli)
                .create_session(
                    CreateSession::builder()
                        .start_slot(cli.start_slot)
                        .end_slot(cli.start_slot + SLOT_ADVANCE)
                        .disconnect_timeout_secs(900u16)
                        .capacity_wait_timeout_secs(900u16)
                        .build(),
                )
                .await?;
            eprintln!("[simulate] session: {}", s.session_id().unwrap_or("?"));
            s.ensure_ready(Some(Duration::from_secs(600))).await?;
            Ok::<_, anyhow::Error>(s)
        },
    )?;
    eprintln!("[phase1] collected {} swaps", captured.len());

    if captured.is_empty() {
        eprintln!("[warn] no Jupiter swaps found in slot {} — exiting", cli.start_slot);
        let _ = sim_session; // dropped, closes on its own timeout
        return Ok(());
    }

    eprintln!(
        "[phase2] simulating {} swaps over slots {}..={}",
        captured.len(), cli.start_slot, cli.start_slot + SLOT_ADVANCE
    );
    let records = run_simulations(sim_session, &captured, cli.start_slot).await?;

    write_output(&cli.output, &records)?;
    eprintln!("[done] wrote {} rows to {}", records.len(), cli.output);
    Ok(())
}

//! Track Jupiter swap output decay across slots.
//!
//! Phase 1 (start_slot): discover the first x Jupiter swaps and capture their
//!   transactions, signers, and on-chain amounts.
//! Phase 2 (start_slot..=start_slot+50): simulate each captured swap against
//!   each slot's frozen chain state and record the output token amount to
//!   track slippage decay over time.

use backtest_example::utils::accounts::{
    account_override, empty_token_override, native_override, native_seed_lamports,
};
use backtest_example::utils::chain::get_transactions;
use backtest_example::utils::parse::{
    JUPITER_V6, WSOL_MINT, derive_ata, extract_signer, parse_any_jupiter_swap, patch_jup_min_out,
};

use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Write as _};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::Parser;
use serde::Deserialize;
use simulator_api::{
    AccountData, AccountModifications, ActionAnchor, ActionKind, ContinueParams, ScheduledAction,
};
use simulator_client::{
    CreateSession, ManagedBacktestSession, ManagedEvent, ManagedSessionError, backtest_ws_url,
};
use solana_account_decoder::UiAccount;
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

#[derive(Parser)]
#[command(about = "Track Jupiter swap output decay across 50 slots")]
struct Cli {
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,
    /// Program id: router program to measure.
    #[arg(long, default_value = JUPITER_V6)]
    program_id: String,
    /// Starting slot: collect all swaps for the given `program_id` at this slot.
    #[arg(long, default_value_t = 437_249_802)]
    start_slot: u64,
    /// Slot count: simulate all swaps from `start_slot` for this many successive slots.
    #[arg(long, default_value_t = 50)]
    slot_count: u64,
    #[arg(long, default_value = "results.csv")]
    output: String,
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

// ── phase 1: collect ─────────────────────────────────────────────────────────

async fn collect_swaps(cli: &Cli) -> Result<Vec<OriginalSwap>> {
    let program_addr: Address = cli.program_id.parse().context("invalid --program-id")?;
    let program_id = program_addr.to_string();

    let txs = get_transactions(cli.start_slot).await?;
    let mut captured: Vec<OriginalSwap> = Vec::new();

    for tx_with_meta in &txs {
        let touches_program = tx_with_meta
            .transaction
            .message
            .static_account_keys()
            .iter()
            .any(|k| k.to_string() == program_id);
        if !touches_program {
            continue;
        }

        let Some((in_mint, out_mint, swap_data)) = parse_any_jupiter_swap(tx_with_meta) else {
            continue;
        };

        let tx = tx_with_meta.transaction.clone();
        let Ok(signer) = extract_signer(&tx) else {
            continue;
        };
        let signature = tx
            .signatures
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();

        eprintln!(
            "  [collect] sig={} {}->{}  in={} out={}",
            &signature[..16],
            in_mint,
            out_mint,
            swap_data.in_amount,
            swap_data.out_amount
        );
        captured.push(OriginalSwap {
            slot: cli.start_slot,
            signature,
            transaction: tx,
            signer,
            in_mint,
            out_mint,
            in_amount: swap_data.in_amount,
            out_amount: swap_data.out_amount,
        });
    }

    Ok(captured)
}

// ── phase 2: simulate ────────────────────────────────────────────────────────

/// Read the SPL token `amount` from a returned `UiAccount` JSON value.
fn token_amount(account: &serde_json::Value) -> Option<u64> {
    let data = UiAccount::deserialize(account).ok()?.data.decode()?;
    let amount = data.get(64..72)?;
    Some(u64::from_le_bytes(amount.try_into().ok()?))
}

/// Read the native lamport balance from a returned `UiAccount` JSON value.
fn native_lamports(account: &serde_json::Value) -> Option<u64> {
    Some(UiAccount::deserialize(account).ok()?.lamports)
}

fn contains_wsol_creation(swap: &OriginalSwap) -> bool {
    let ata = derive_ata(&swap.signer, WSOL_MINT)
        .map(|k| k.to_string())
        .unwrap_or_default();

    let keys = swap.transaction.message.static_account_keys();
    const SYS: &str = "11111111111111111111111111111111";

    keys.iter()
        .position(|k| k.to_string() == SYS)
        .is_some_and(|si| {
            swap.transaction.message.instructions().iter().any(|ix| {
                ix.program_id_index as usize == si
                    && ix
                        .accounts
                        .get(1)
                        .and_then(|&ai| keys.get(ai as usize))
                        .is_some_and(|k| k.to_string() == ata)
            })
        })
}

/// Rent-exempt minimum for a bare (non-native) token account; used as the
/// fallback baseline when reading back a wSOL ATA that a failed `CloseAccount`
/// left populated instead of unwrapping to native SOL.
const ATA_RENT_EXEMPT: u64 = 2_039_280;

/// One override per swap-mint side, plus the accounts to read back after the
/// simulated swap runs.
struct SwapAccounts {
    overrides: BTreeMap<Address, AccountData>,
    return_accounts: Vec<Address>,
}

/// Build the account overrides that fund `swap.signer` with `swap.in_amount`
/// of `in_mint`, and zero out the output side so the returned post-execution
/// balance *is* the swap's output (no separate "before" read needed).
async fn build_overrides(swap: &OriginalSwap) -> Result<SwapAccounts> {
    let mut overrides = BTreeMap::new();

    // Input side. For WSOL: either seed native SOL and let the tx wrap it
    // itself (wrap-and-swap pattern), or seed the wSOL ATA directly.
    let (in_addr, in_data) = account_override(
        &swap.signer,
        &swap.in_mint,
        swap.in_amount,
        contains_wsol_creation(swap),
    )
    .await?;
    overrides.insert(in_addr, in_data);

    // Output side, zeroed so the post-execution read is the swap output directly.
    // WSOL output always reads as native SOL, never an ATA.
    let (out_addr, out_data) = if swap.out_mint == WSOL_MINT {
        native_override(&swap.signer, 0)?
    } else {
        empty_token_override(&swap.signer, &swap.out_mint).await?
    };
    overrides.insert(out_addr, out_data);

    let return_accounts = if swap.out_mint == WSOL_MINT {
        let wsol_ata: Address = derive_ata(&swap.signer, WSOL_MINT)
            .context("derive wSOL output ATA")?
            .to_string()
            .parse()?;
        vec![out_addr, wsol_ata]
    } else {
        vec![out_addr]
    };

    Ok(SwapAccounts {
        overrides,
        return_accounts,
    })
}

/// One `ScheduledAction` per swap: the patched transaction repeated once per
/// slot in `start_slot..=start_slot + slot_count`, funded via `account_overrides`
/// so each firing re-simulates against a consistent, freshly-funded balance.
async fn build_action(
    swap: &OriginalSwap,
    start_slot: u64,
    slot_count: u64,
) -> Result<ScheduledAction> {
    let slots: Vec<u64> = (start_slot..=start_slot + slot_count).collect();
    let SwapAccounts {
        overrides,
        return_accounts,
    } = build_overrides(swap).await?;

    let patched = patch_jup_min_out(&swap.transaction);
    let tx = STANDARD.encode(bincode::serialize(&patched)?);

    Ok(ScheduledAction {
        anchor: ActionAnchor::AfterSlot {
            slots: slots.clone(),
        },
        kind: ActionKind::Simulate,
        transactions: vec![tx; slots.len()],
        account_overrides: AccountModifications(overrides),
        feeds_reroute: false,
        return_accounts,
        label: Some(swap.signature.clone()),
    })
}

/// Read the swap output from a `ScheduledAction`'s returned accounts,
/// matching the zeroed baseline `build_overrides` set on the output side.
fn read_sim_out(out_mint: &str, accounts: &[Option<serde_json::Value>]) -> u64 {
    if out_mint == WSOL_MINT {
        // accounts[0] = signer (native SOL, zeroed baseline), accounts[1] = wSOL ATA.
        // - Try native SOL first (CloseAccount ran, so the gain landed here).
        // - If it's 0, fall back to the wSOL ATA (CloseAccount didn't run).
        let native_gain = accounts
            .first()
            .and_then(|a| a.as_ref())
            .and_then(native_lamports)
            .map(|l| l.saturating_sub(native_seed_lamports(0)))
            .unwrap_or(0);
        if native_gain > 0 {
            native_gain
        } else {
            accounts
                .get(1)
                .and_then(|a| a.as_ref())
                .and_then(native_lamports)
                .map(|l| l.saturating_sub(ATA_RENT_EXEMPT))
                .unwrap_or(0)
        }
    } else {
        accounts
            .first()
            .and_then(|a| a.as_ref())
            .and_then(token_amount)
            .unwrap_or(0)
    }
}

async fn run_simulations(
    cli: &Cli,
    swaps: &[OriginalSwap],
    start_slot: u64,
    slot_count: u64,
) -> Result<Vec<SimulatedSwap>> {
    let mut actions = Vec::with_capacity(swaps.len());
    for swap in swaps {
        actions.push(build_action(swap, start_slot, slot_count).await?);
    }
    eprintln!("[phase2] registering {} actions", actions.len());

    let by_signature: HashMap<&str, &OriginalSwap> =
        swaps.iter().map(|s| (s.signature.as_str(), s)).collect();

    let create = CreateSession::builder()
        .start_slot(start_slot)
        .end_slot(start_slot + slot_count)
        .disconnect_timeout_secs(900u16)
        .capacity_wait_timeout_secs(900u16)
        .actions(actions)
        .build()
        .into_request()
        .context("building create-session request")?;

    let ws_url = backtest_ws_url(&cli.url);
    let mut session = ManagedBacktestSession::start(ws_url, cli.api_key.clone(), create)
        .await
        .context("starting managed session")?;
    session.subscribe_actions();

    let mut records: Vec<SimulatedSwap> = Vec::new();

    loop {
        match session.next_event().await {
            Ok(ManagedEvent::ReadyForContinue) => {
                let params = ContinueParams {
                    advance_count: slot_count,
                    transactions: Vec::new(),
                    modify_account_states: AccountModifications(Default::default()),
                };
                session
                    .send_continue(params)
                    .await
                    .context("send_continue")?;
            }
            Ok(ManagedEvent::ActionResult(notification)) => {
                let Some(signature) = notification.label.as_deref() else {
                    continue;
                };
                let Some(&swap) = by_signature.get(signature) else {
                    continue;
                };

                if let Some(outcome) = notification
                    .transaction_outcomes
                    .first()
                    .filter(|o| o.err.is_some())
                {
                    eprintln!(
                        "    [sim err] sig={} slot={} {}\n{}",
                        &signature[..16],
                        notification.slot,
                        outcome.err.as_deref().unwrap_or_default(),
                        outcome
                            .logs
                            .iter()
                            .map(|l| format!("      {l}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    continue;
                }

                let sim_out = read_sim_out(&swap.out_mint, &notification.accounts);
                eprintln!(
                    "  sig={} slot={} orig={} sim={}",
                    &signature[..16],
                    notification.slot,
                    swap.out_amount,
                    sim_out
                );
                records.push(SimulatedSwap {
                    signature: swap.signature.clone(),
                    in_mint: swap.in_mint.clone(),
                    out_mint: swap.out_mint.clone(),
                    in_amount: swap.in_amount,
                    original_slot: swap.slot,
                    original_out: swap.out_amount,
                    sim_slot: notification.slot,
                    sim_out,
                });
            }
            Ok(ManagedEvent::Completed { .. }) => break,
            Ok(ManagedEvent::Error(e)) => {
                session.shutdown().await;
                anyhow::bail!("simulator error: {e}");
            }
            Ok(_) => {}
            Err(ManagedSessionError::Cancelled) => break,
            Err(e) => {
                session.shutdown().await;
                return Err(anyhow::anyhow!("session failed: {e}"));
            }
        }
    }

    session.shutdown().await;
    Ok(records)
}

// ── output ───────────────────────────────────────────────────────────────────

fn write_output(path: &str, records: &[SimulatedSwap]) -> Result<()> {
    let f = std::fs::File::create(path)?;
    let mut w = BufWriter::new(f);
    writeln!(
        w,
        "transaction,in_mint,out_mint,in_amount,original_slot,original_out,simulated_slot,simulated_out"
    )?;
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{}",
            r.signature,
            r.in_mint,
            r.out_mint,
            r.in_amount,
            r.original_slot,
            r.original_out,
            r.sim_slot,
            r.sim_out
        )?;
    }
    Ok(())
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let cli = Cli::parse();

    eprintln!("[phase1] collecting swaps from slot {}", cli.start_slot);
    let captured = collect_swaps(&cli).await?;
    eprintln!("[phase1] collected {} swaps", captured.len());

    if captured.is_empty() {
        eprintln!("[warn] no swaps found in slot {} — exiting", cli.start_slot);
        return Ok(());
    }

    eprintln!(
        "[phase2] simulating {} swaps over slots {}..={}",
        captured.len(),
        cli.start_slot,
        cli.start_slot + cli.slot_count
    );
    let records = run_simulations(&cli, &captured, cli.start_slot, cli.slot_count).await?;

    write_output(&cli.output, &records)?;
    eprintln!("[done] wrote {} rows to {}", records.len(), cli.output);
    Ok(())
}

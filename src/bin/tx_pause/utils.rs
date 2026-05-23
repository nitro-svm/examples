use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine as _;
use history_model::{TransactionTokenBalanceSerde, TxWithMeta};
use solana_message::{VersionedMessage, compiled_instruction::CompiledInstruction};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::RpcSimulateTransactionResult;
use solana_transaction::versioned::VersionedTransaction;

pub const SOLANA_RPC: &str = "https://api.mainnet-beta.solana.com";
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

pub const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const JUP_FEE_AUTHORITY: &str = "45ruCyfdRkWpRNGEqWzjCiXRHkZs8WXCLQ67Pnpye7Hp";

pub const TITAN_PROGRAM: &str = "T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT";
const TITAN_SPLIT_EVENT_DISCRIMINANT: [u8; 8] = [0xb7, 0x1c, 0x17, 0x87, 0xad, 0x7f, 0x7c, 0xea];

pub struct SwapData {
    pub in_amount: u64,
    pub out_amount: u64,
    /// (program_address, input_token_amount_routed_through_venue)
    pub venues: Vec<(String, u64)>,
}

pub fn parse_jupiter_swap_result(
    tx_with_meta: &TxWithMeta,
    input: &str,
    output: &str,
) -> Option<SwapData> {
    let tx = &tx_with_meta.transaction;
    let sig = tx.signatures.first().map(|s| s.to_string()).unwrap_or_default();
    let keys: Vec<String> = tx
        .message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    keys.iter().position(|k| k == JUPITER_V6)?;

    let signer = &keys[0];
    let bd = tx_with_meta.balance_diffs.as_ref();
    let (pre, post) = bd.map(|b| b.token_balances_or_empty()).unwrap_or((&[], &[]));

    let input_consumed = token_delta(pre, post, signer, input, true).is_some_and(|d| d > 0);
    let has_input = input_consumed || match input {
        USDC_MINT => keys.iter().any(|k| k == USDC_MINT),
        WSOL_MINT => {
            let tokenkeg_idx = keys.iter().position(|k| k == TOKEN_PROGRAM).map(|i| i as u8);
            let has_sync_native = tokenkeg_idx.is_some_and(|idx| {
                tx.message.instructions().iter().any(|ix| ix.program_id_index == idx && ix.data == [17])
            });
            has_sync_native || signer_received(pre, post, signer, output).is_some()
        }
        _ => false,
    };
    if !has_input {
        return None;
    }

    // USDC→SOL JUP swaps often unwrap via CloseAccount so the wSOL ATA ends at 0;
    // fall back to the signer's native SOL gain at account[0].
    let out_amount = signer_received(pre, post, signer, output)
        .or_else(|| {
            if output == WSOL_MINT {
                bd.and_then(|b| {
                    b.post_balances.first().zip(b.pre_balances.first())
                        .and_then(|(&post_b, &pre_b)| post_b.checked_sub(pre_b))
                })
            } else {
                None
            }
        })
        .unwrap_or(0);
    if out_amount == 0 {
        eprintln!("  [jup filter] {sig}... out_amount=0");
        return None;
    }

    let venues = venue_amounts(pre, post, signer, input, &[JUPITER_V6, JUP_FEE_AUTHORITY]);
    let input_amount = if input == WSOL_MINT {
        token_delta(pre, post, signer, WSOL_MINT, true)
            .filter(|&d| d > 0)
            .or_else(|| {
                bd.and_then(|b| {
                    b.pre_balances.first().zip(b.post_balances.first())
                        .and_then(|(&pre_b, &post_b)| pre_b.checked_sub(post_b))
                })
            })
            .unwrap_or(0)
    } else {
        venues.iter().map(|(_, a)| a).sum()
    };

    Some(SwapData { in_amount: input_amount, out_amount, venues })
}

pub async fn get_titan_template_transaction(signature: &str) -> Result<VersionedTransaction> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getTransaction",
        "params": [signature, {"encoding": "base64", "maxSupportedTransactionVersion": 0}]
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(SOLANA_RPC)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let encoded = resp["result"]["transaction"][0]
        .as_str()
        .context("titan template tx not found")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("base64 decode titan template tx")?;
    eprintln!("[titan] loaded template: {signature}");
    bincode::deserialize(&bytes).context("deserialize titan template tx")
}

pub fn patch_titan_template_transaction(
    tx: &VersionedTransaction,
    in_ata: Pubkey,
    in_amount: u64,
) -> Result<VersionedTransaction> {
    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys
        .iter()
        .position(|k| k.to_string() == TITAN_PROGRAM)
        .context("titan program not in static keys")? as u8;
    let in_ata_key_idx = match &tx.message {
        VersionedMessage::Legacy(msg) => ix_acct(&msg.instructions, titan_idx, 3),
        VersionedMessage::V0(msg) => ix_acct(&msg.instructions, titan_idx, 3),
    }
    .context("could not find in_ata in titan ix")? as usize;

    let mut new_tx = tx.clone();
    let (keys, ixs) = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => (&mut msg.account_keys, &mut msg.instructions),
        VersionedMessage::V0(msg) => (&mut msg.account_keys, &mut msg.instructions),
    };

    *keys
        .get_mut(in_ata_key_idx)
        .context("in_ata_key_idx out of range")? = in_ata;

    let titan_ix = ixs
        .iter_mut()
        .find(|ix| ix.program_id_index == titan_idx)
        .context("titan ix not found")?;
    // v3 layout: [0]=disc(0x2a) [1]=config [2..10]=amount [10..18]=expected_amount_out [18..20]=slippage_threshold_bps
    anyhow::ensure!(titan_ix.data.len() >= 20, "titan ix data too short for v3");
    anyhow::ensure!(titan_ix.data[0] == 0x2a, "unexpected titan ix discriminator (not swap_route_v3)");
    titan_ix.data[2..10].copy_from_slice(&in_amount.to_le_bytes());
    // zero expected_amount_out so simulation never fails on stale slippage
    titan_ix.data[10..18].copy_from_slice(&0u64.to_le_bytes());

    Ok(new_tx)
}

// SwapRouteV3Details event layout (after 8-byte discriminator):
//   [8..16]  input_amount  (u64 LE)
//   [16..24] output_amount (u64 LE)
//   [24..32] expected_output, [32..40] fee_collected, [40..48] surplus_collected, [48..56] dynamic_allocation_fee_collected
const TITAN_V3_DETAILS_DISCRIMINANT: [u8; 8] = [79, 62, 249, 87, 62, 217, 136, 30];

pub fn parse_titan_sim_result(result: &RpcSimulateTransactionResult) -> SwapData {
    if let Some(err) = &result.err {
        eprintln!("  [titan sim error] {:?}", err);
    }

    let logs = result.logs.as_deref().unwrap_or(&[]);
    let mut in_amount = 0u64;
    let mut out_amount = 0u64;
    let mut last_invoke2: Option<String> = None;
    let mut venues: HashMap<String, u64> = HashMap::new();

    for log in logs {
        if let Some(prog) = log
            .strip_prefix("Program ")
            .and_then(|s| s.strip_suffix(" invoke [2]"))
        {
            last_invoke2 = Some(prog.to_string());
        } else if let Some(b64) = log.strip_prefix("Program data: ")
            && let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64)
        {
            if raw.len() >= 24 && raw[..8] == TITAN_V3_DETAILS_DISCRIMINANT {
                in_amount = u64::from_le_bytes(raw[8..16].try_into().unwrap());
                out_amount = u64::from_le_bytes(raw[16..24].try_into().unwrap());
            } else if raw.len() >= 41 && raw[..8] == TITAN_SPLIT_EVENT_DISCRIMINANT {
                let split_in = u64::from_le_bytes(raw[9..17].try_into().unwrap());
                if let Some(prog) = &last_invoke2 {
                    *venues.entry(prog.clone()).or_insert(0) += split_in;
                }
            }
        }
    }

    let mut venues: Vec<(String, u64)> = venues.into_iter().collect();
    venues.sort_by_key(|(_, a)| std::cmp::Reverse(*a));
    SwapData { in_amount, out_amount, venues }
}

pub fn parse_titan_sim_events(result: &RpcSimulateTransactionResult) -> SwapData {
    if let Some(err) = &result.err {
        eprintln!("  [titan sim error] {:?}", err);
    }

    let logs = result.logs.as_deref().unwrap_or(&[]);

    // Pair each split event with the last invoke [2] program seen before it.
    // Titan emits a Program data: event immediately after each DEX CPI completes.
    let mut last_invoke2: Option<String> = None;
    let mut venues: HashMap<String, u64> = HashMap::new();
    let mut in_amount = 0u64;
    let mut out_amount = 0u64;

    for log in logs {
        if let Some(prog) = log
            .strip_prefix("Program ")
            .and_then(|s| s.strip_suffix(" invoke [2]"))
        {
            last_invoke2 = Some(prog.to_string());
        } else if let Some(b64) = log.strip_prefix("Program data: ")
            && let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64)
            && raw.len() >= 41
            && raw[..8] == TITAN_SPLIT_EVENT_DISCRIMINANT
        {
            let split_idx = raw[8];
            let field_a = u64::from_le_bytes(raw[9..17].try_into().unwrap());
            let field_b = u64::from_le_bytes(raw[17..25].try_into().unwrap());
            let field_c = u64::from_le_bytes(raw[25..33].try_into().unwrap());
            let actual_out = u64::from_le_bytes(raw[33..41].try_into().unwrap());
            eprintln!(
                "    [titan event] split={split_idx} a={field_a} b={field_b} c={field_c} out={actual_out}"
            );
            let split_in = field_a;
            in_amount += split_in;
            out_amount += actual_out;
            if let Some(prog) = &last_invoke2 {
                *venues.entry(prog.clone()).or_insert(0) += split_in;
            }
        }
    }

    let mut venues: Vec<(String, u64)> = venues.into_iter().collect();
    venues.sort_by_key(|(_, a)| std::cmp::Reverse(*a));

    SwapData {
        in_amount,
        out_amount,
        venues,
    }
}

pub fn extract_signer(tx: &VersionedTransaction) -> Result<Pubkey> {
    let signer = tx
        .message
        .static_account_keys()
        .first()
        .context("no static account keys")?
        .to_owned();

    Ok(signer)
}

pub fn derive_ata(wallet: &Pubkey, mint_str: &str) -> Option<Pubkey> {
    let assoc: Pubkey = ASSOCIATED_TOKEN_PROGRAM.parse().ok()?;
    let token: Pubkey = TOKEN_PROGRAM.parse().ok()?;
    let mint: Pubkey = mint_str.parse().ok()?;
    let (ata, _) =
        Pubkey::find_program_address(&[wallet.as_ref(), token.as_ref(), mint.as_ref()], &assoc);
    Some(ata)
}

fn ix_acct(ixs: &[CompiledInstruction], prog: u8, pos: usize) -> Option<u8> {
    ixs.iter()
        .find(|ix| ix.program_id_index == prog)?
        .accounts
        .get(pos)
        .copied()
}

// Positive amount of `mint` received by `signer` in this tx.
// Handles both existing ATAs (pre→post delta) and new ATAs (no pre-entry → full post balance).
fn signer_received(
    pre: &[TransactionTokenBalanceSerde],
    post: &[TransactionTokenBalanceSerde],
    signer: &str,
    mint: &str,
) -> Option<u64> {
    if let Some(d) = token_delta(pre, post, signer, mint, false).filter(|&d| d > 0) {
        return Some(d);
    }
    // Pre-entry present but no increase → nothing received.
    if pre.iter().any(|b| b.owner.to_string() == signer && b.mint.to_string() == mint) {
        return None;
    }
    // No pre-entry: new ATA created in this tx; full post-balance is the received amount.
    post.iter()
        .find(|b| b.owner.to_string() == signer && b.mint.to_string() == mint)
        .and_then(|b| b.ui_token_amount.amount.parse().ok())
        .filter(|&a: &u64| a > 0)
}

// Signer's token delta for `mint`. `consumed=true` for input (pre > post), false for output.
fn token_delta(
    pre: &[TransactionTokenBalanceSerde],
    post: &[TransactionTokenBalanceSerde],
    signer: &str,
    mint: &str,
    consumed: bool,
) -> Option<u64> {
    let pre_e = pre
        .iter()
        .find(|b| b.owner.to_string() == signer && b.mint.to_string() == mint)?;
    let post_e = post
        .iter()
        .find(|b| b.account_index == pre_e.account_index)?;
    let pre_amt: u64 = pre_e.ui_token_amount.amount.parse().ok()?;
    let post_amt: u64 = post_e.ui_token_amount.amount.parse().ok()?;
    if consumed {
        pre_amt.checked_sub(post_amt)
    } else {
        post_amt.checked_sub(pre_amt)
    }
}

// Non-signer token accounts where input-mint balance increased are pool vaults receiving that
// token; their owner program is the venue. Results sorted descending by amount.
fn venue_amounts(
    pre: &[TransactionTokenBalanceSerde],
    post: &[TransactionTokenBalanceSerde],
    signer: &str,
    input_mint: &str,
    skip_owners: &[&str],
) -> Vec<(String, u64)> {
    let post_map: HashMap<u8, u64> = post
        .iter()
        .filter_map(|b| Some((b.account_index, b.ui_token_amount.amount.parse().ok()?)))
        .collect();

    let mut by_owner: HashMap<String, u64> = HashMap::new();
    for b in pre {
        let owner = b.owner.to_string();
        if b.mint.to_string() != input_mint
            || owner == signer
            || skip_owners.contains(&owner.as_str())
        {
            continue;
        }
        let pre_amt: u64 = match b.ui_token_amount.amount.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if let Some(&post_amt) = post_map.get(&b.account_index)
            && post_amt > pre_amt
        {
            *by_owner.entry(owner).or_insert(0) += post_amt - pre_amt;
        }
    }

    let mut result: Vec<(String, u64)> = by_owner.into_iter().collect();
    result.sort_by_key(|(_, a)| std::cmp::Reverse(*a));
    result
}

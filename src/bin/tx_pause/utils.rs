use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine as _;
use history_model::{TransactionTokenBalanceSerde, TxWithMeta};
use solana_message::VersionedMessage;
use solana_rpc_client_api::response::RpcSimulateTransactionResult;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::UiTransactionTokenBalance;

pub const SOLANA_RPC: &str = "https://mainnet-beta.solana.com";
pub const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const TITAN_PROGRAM: &str = "T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

pub const JUP_ROUTE_DISC: [u8; 8] = [229, 23, 203, 151, 122, 227, 173, 42];
pub const JUP_SHARED_DISC: [u8; 8] = [193, 32, 155, 51, 65, 214, 156, 129];
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

pub struct SwapData {
    pub input_amount: u64,
    pub out_amount: u64,
    /// (program_address, input_token_amount_routed_through_venue)
    pub venues: Vec<(String, u64)>,
}

// JUP V6 direction detection:
//   USDC input: USDC_MINT appears in static keys (SharedAccountsRoute pattern)
//   wSOL input: SyncNative instruction (tag=17) present in tx
//
// input_amount, out_amount: signer's pre/post token balance deltas.
// venues: non-signer token accounts where the input-mint balance increased are DEX pool vaults;
//         their owner program identifies the venue. JUP's own pass-through accounts net to zero
//         and are filtered out naturally.
pub fn parse_jupiter_swap(tx_with_meta: &TxWithMeta, input: &str, output: &str) -> Option<SwapData> {
    let tx = &tx_with_meta.transaction;
    let keys: Vec<String> = tx
        .message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();

    let jup_idx = keys.iter().position(|k| k == JUPITER_V6)? as u8;
    let jup_ix = tx
        .message
        .instructions()
        .iter()
        .find(|ix| ix.program_id_index == jup_idx)?;

    if jup_ix.data.len() < 19 {
        return None;
    }
    if !jup_ix.data.starts_with(&JUP_ROUTE_DISC) && !jup_ix.data.starts_with(&JUP_SHARED_DISC) {
        return None;
    }

    // Direction check: USDC input → USDC mint in static keys; wSOL input → SyncNative present
    let has_input = match input {
        USDC_MINT => keys.iter().any(|k| k == USDC_MINT),
        WSOL_MINT => {
            let tokenkeg_idx = keys.iter().position(|k| k == TOKEN_PROGRAM).map(|i| i as u8);
            tokenkeg_idx.is_some_and(|idx| {
                tx.message
                    .instructions()
                    .iter()
                    .any(|ix| ix.program_id_index == idx && ix.data == [17])
            })
        }
        _ => false,
    };
    if !has_input {
        return None;
    }

    let signer = &keys[0];
    let (input_amount, out_amount, venues) = tx_with_meta
        .balance_diffs
        .as_ref()
        .map(|bd| {
            let (pre, post) = bd.token_balances_or_empty();
            let input_amount = token_delta(pre, post, signer, input, true).unwrap_or(0);
            let out_amount = token_delta(pre, post, signer, output, false).unwrap_or(0);
            let venues = venue_amounts(pre, post, signer, input, &[JUPITER_V6]);
            (input_amount, out_amount, venues)
        })
        .unwrap_or_default();

    Some(SwapData { input_amount, out_amount, venues })
}

// Returns (out_amount, venues) for a simulated TITAN swap.
// out_amount: signer's output token delta from simulation pre/post token balances.
// venues: non-signer token accounts where input-mint balance increased; owner = venue program.
pub fn parse_titan_sim_result(
    tx: &VersionedTransaction,
    result: &RpcSimulateTransactionResult,
    input: &str,
    output: &str,
) -> (u64, Vec<(String, u64)>) {
    let signer = tx
        .message
        .static_account_keys()
        .first()
        .map(|k| k.to_string())
        .unwrap_or_default();

    let pre = result.pre_token_balances.as_deref().unwrap_or(&[]);
    let post = result.post_token_balances.as_deref().unwrap_or(&[]);

    let out_amount = ui_token_delta(pre, post, &signer, output, false).unwrap_or(0);
    let venues = ui_venue_amounts(pre, post, &signer, input, &[TITAN_PROGRAM]);

    (out_amount, venues)
}

pub async fn get_titan_template(signature: &str) -> Result<VersionedTransaction> {
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

// Returns a clone of `tx` with the Titan SwapRouteV3 in_amount patched to `amount`.
pub fn set_titan_input_amount(tx: &VersionedTransaction, amount: u64) -> Option<VersionedTransaction> {
    let keys = tx.message.static_account_keys();
    let titan_idx = keys
        .iter()
        .position(|k| k.to_string() == TITAN_PROGRAM)? as u8;

    let mut new_tx = tx.clone();

    let ix = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => msg
            .instructions
            .iter_mut()
            .find(|ix| ix.program_id_index == titan_idx)?,
        VersionedMessage::V0(msg) => msg
            .instructions
            .iter_mut()
            .find(|ix| ix.program_id_index == titan_idx)?,
    };

    if ix.data.len() < 10 {
        return None;
    }
    ix.data[2..10].copy_from_slice(&amount.to_le_bytes());

    Some(new_tx)
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
    let post_e = post.iter().find(|b| b.account_index == pre_e.account_index)?;
    let pre_amt: u64 = pre_e.ui_token_amount.amount.parse().ok()?;
    let post_amt: u64 = post_e.ui_token_amount.amount.parse().ok()?;
    if consumed { pre_amt.checked_sub(post_amt) } else { post_amt.checked_sub(pre_amt) }
}

// Same as token_delta but for UiTransactionTokenBalance (simulation result).
fn ui_token_delta(
    pre: &[UiTransactionTokenBalance],
    post: &[UiTransactionTokenBalance],
    signer: &str,
    mint: &str,
    consumed: bool,
) -> Option<u64> {
    let pre_e = pre.iter().find(|b| {
        b.owner.as_ref().map(|s| s.as_str() == signer).unwrap_or(false) && b.mint == mint
    })?;
    let post_e = post.iter().find(|b| b.account_index == pre_e.account_index)?;
    let pre_amt: u64 = pre_e.ui_token_amount.amount.parse().ok()?;
    let post_amt: u64 = post_e.ui_token_amount.amount.parse().ok()?;
    if consumed { pre_amt.checked_sub(post_amt) } else { post_amt.checked_sub(pre_amt) }
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
        if b.mint.to_string() != input_mint || owner == signer || skip_owners.contains(&owner.as_str()) {
            continue;
        }
        let pre_amt: u64 = match b.ui_token_amount.amount.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if let Some(&post_amt) = post_map.get(&b.account_index) {
            if post_amt > pre_amt {
                *by_owner.entry(owner).or_insert(0) += post_amt - pre_amt;
            }
        }
    }

    let mut result: Vec<(String, u64)> = by_owner.into_iter().collect();
    result.sort_by_key(|(_, a)| std::cmp::Reverse(*a));
    result
}

// Same as venue_amounts but for UiTransactionTokenBalance (simulation result).
fn ui_venue_amounts(
    pre: &[UiTransactionTokenBalance],
    post: &[UiTransactionTokenBalance],
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
        let Some(owner) = b.owner.as_ref().map(|s| s.clone()) else { continue };
        if b.mint != input_mint || owner == signer || skip_owners.contains(&owner.as_str()) {
            continue;
        }
        let pre_amt: u64 = match b.ui_token_amount.amount.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if let Some(&post_amt) = post_map.get(&b.account_index) {
            if post_amt > pre_amt {
                *by_owner.entry(owner).or_insert(0) += post_amt - pre_amt;
            }
        }
    }

    let mut result: Vec<(String, u64)> = by_owner.into_iter().collect();
    result.sort_by_key(|(_, a)| std::cmp::Reverse(*a));
    result
}

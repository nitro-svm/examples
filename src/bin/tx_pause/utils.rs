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

// JUP V6 direction detection:
//   USDC input: USDC_MINT appears in static keys (SharedAccountsRoute pattern)
//   wSOL input: SyncNative instruction (tag=17) present in tx
//
// input_amount, out_amount: signer's pre/post token balance deltas.
// venues: non-signer token accounts where the input-mint balance increased are DEX pool vaults;
//         their owner program identifies the venue. JUP's own pass-through accounts net to zero
//         and are filtered out naturally.
pub fn parse_jupiter_swap_result(
    tx_with_meta: &TxWithMeta,
    input: &str,
    output: &str,
) -> Option<SwapData> {
    let tx = &tx_with_meta.transaction;
    let sig = tx
        .signatures
        .first()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let keys: Vec<String> = tx
        .message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();

    keys.iter().position(|k| k == JUPITER_V6)?;
    // Direction check: prefer balance diffs (works even when mint is LUT-resolved);
    // fall back to static-key / SyncNative heuristics when diffs are absent.
    let signer = &keys[0];
    let has_input = tx_with_meta
        .balance_diffs
        .as_ref()
        .and_then(|bd| {
            let (pre, post) = bd.token_balances_or_empty();
            token_delta(pre, post, signer, input, true).map(|d| d > 0)
        })
        .unwrap_or_else(|| match input {
            USDC_MINT => keys.iter().any(|k| k == USDC_MINT),
            WSOL_MINT => {
                let tokenkeg_idx = keys
                    .iter()
                    .position(|k| k == TOKEN_PROGRAM)
                    .map(|i| i as u8);
                tokenkeg_idx.is_some_and(|idx| {
                    tx.message
                        .instructions()
                        .iter()
                        .any(|ix| ix.program_id_index == idx && ix.data == [17])
                })
            }
            _ => false,
        });
    if !has_input {
        eprintln!("  [jup filter] {sig}... input mint not detected");
        return None;
    }

    let (input_amount, out_amount, venues) = tx_with_meta
        .balance_diffs
        .as_ref()
        .map(|bd| {
            let (pre, post) = bd.token_balances_or_empty();
            // Many USDC→SOL JUP swaps use wSOL unwrap: create wSOL ATA → swap into it →
            // CloseAccount. The wSOL ATA ends at 0, so token_delta returns 0. Fall back
            // to the signer's native SOL increase (Account 0).
            let out_amount = token_delta(pre, post, signer, output, false)
                .filter(|&a| a > 0)
                .or_else(|| {
                    if output == WSOL_MINT {
                        bd.post_balances
                            .first()?
                            .checked_sub(*bd.pre_balances.first()?)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let venues = venue_amounts(pre, post, signer, input, &[JUPITER_V6, JUP_FEE_AUTHORITY]);
            let input_amount = venues.iter().map(|(_, a)| a).sum();
            (input_amount, out_amount, venues)
        })
        .unwrap_or_default();

    if out_amount == 0 {
        eprintln!("  [jup filter] {sig}... out_amount=0");
        return None;
    }

    Some(SwapData {
        in_amount: input_amount,
        out_amount,
        venues,
    })
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
    anyhow::ensure!(titan_ix.data.len() >= 10, "titan ix data too short");
    titan_ix.data[2..10].copy_from_slice(&in_amount.to_le_bytes());

    Ok(new_tx)
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
            let split_in = u64::from_le_bytes(raw[9..17].try_into().unwrap());
            let actual_out = u64::from_le_bytes(raw[33..41].try_into().unwrap());
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

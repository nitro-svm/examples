use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine as _;
use super::types::{TransactionTokenBalanceSerde, TxWithMeta};
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
    pub quote_amount: Option<u64>,
    /// (program_address, input_token_amount_routed_through_venue)
    pub venues: Vec<(String, u64)>,
}

// All discriminants are sha256("global:<instruction_name>")[..8].
//
// V1 suffix layout (route_plan is variable, so fixed fields are at fixed negative offsets):
//   regular:   [disc:8][route_plan:var][in:8][quoted_out:8][slippage:2][fee:1]  → quoted_out at [len-11..len-3]
//   exact_out: [disc:8][route_plan:var][out:8][quoted_in:8][slippage:2][fee:1]  → out at [len-19..len-11]
//
// V2 fixed layout (route_plan moved to end, fields at fixed positive offsets):
//   non-shared: [disc:8][in:8][quoted_out:8]        → quoted_out at 16
//   shared:     [disc:8][id:1][in:8][quoted_out:8]  → quoted_out at 17
//   exact_out variants: out_amount comes first at offset 8 or 9 (with id)
const JUP_V1_ROUTE_DISCRIMINANTS: &[[u8; 8]] = &[
    [0xe5, 0x17, 0xcb, 0x97, 0x7a, 0xe3, 0xad, 0x2a], // route
    [0xc1, 0x20, 0x9b, 0x33, 0x41, 0xd6, 0x9c, 0x81], // shared_accounts_route
    [0x96, 0x56, 0x47, 0x74, 0xa7, 0x5d, 0x0e, 0x68], // route_with_token_ledger
    [0xe6, 0x79, 0x8f, 0x50, 0x77, 0x9f, 0x6a, 0xaa], // shared_accounts_route_with_token_ledger
];
const JUP_V1_EXACT_OUT_DISCRIMINANTS: &[[u8; 8]] = &[
    [0xd0, 0x33, 0xef, 0x97, 0x7b, 0x2b, 0xed, 0x5c], // exact_out_route
    [0xb0, 0xd1, 0x69, 0xa8, 0x9a, 0x7d, 0x45, 0x3e], // shared_accounts_exact_out_route
];
// (discriminant, byte offset of the output amount field)
// non-shared route_v2:             [disc:8][in:8][quoted_out:8]        → 16
// shared_accounts_route_v2:        [disc:8][id:1][in:8][quoted_out:8]  → 17
// exact_out_route_v2:              [disc:8][out:8][quoted_in:8]        →  8
// shared_accounts_exact_out_route_v2: [disc:8][id:1][out:8][quoted_in:8] → 9
const JUP_V2_OFFSETS: &[([u8; 8], usize)] = &[
    ([0xbb, 0x64, 0xfa, 0xcc, 0x31, 0xc4, 0xaf, 0x14], 16), // route_v2
    ([0xd1, 0x98, 0x53, 0x93, 0x7c, 0xfe, 0xd8, 0xe9], 17), // shared_accounts_route_v2
    ([0x98, 0x0c, 0xa4, 0x79, 0x4a, 0x67, 0xad, 0xcd], 16), // route_with_token_ledger_v2
    ([0x79, 0xf7, 0xe0, 0x05, 0xd0, 0xea, 0x42, 0xfc], 17), // shared_accounts_route_with_token_ledger_v2
    ([0x9d, 0x8a, 0xb8, 0x52, 0x15, 0xf4, 0xf3, 0x24],  8), // exact_out_route_v2
    ([0x35, 0x60, 0xe5, 0xca, 0xd8, 0xbb, 0xfa, 0x18],  9), // shared_accounts_exact_out_route_v2
];

pub fn extract_jup_quoted_out(tx: &VersionedTransaction) -> Option<u64> {
    let keys = tx.message.static_account_keys();
    let jup_idx = keys.iter().position(|k| k.to_string() == JUPITER_V6)? as u8;
    for ix in tx.message.instructions().iter().filter(|ix| ix.program_id_index == jup_idx) {
        let data = ix.data.as_slice();
        let Ok(disc) = <[u8; 8]>::try_from(data.get(..8)?) else { continue };
        // Filter out quoted_out <= 0: senders often set 1 to bypass slippage checks.
        let filter = |q: u64| if q > 0 { Some(q) } else { None };
        if let Some(&(_, off)) = JUP_V2_OFFSETS.iter().find(|(d, _)| d == &disc) {
            return data.get(off..off + 8)?.try_into().ok().map(u64::from_le_bytes).and_then(filter);
        } else if JUP_V1_ROUTE_DISCRIMINANTS.contains(&disc) && data.len() >= 19 {
            let len = data.len();
            return filter(u64::from_le_bytes(data[len - 11..len - 3].try_into().unwrap()));
        } else if JUP_V1_EXACT_OUT_DISCRIMINANTS.contains(&disc) && data.len() >= 27 {
            let len = data.len();
            return filter(u64::from_le_bytes(data[len - 19..len - 11].try_into().unwrap()));
        }
    }
    None
}

fn has_sync_native(keys: &[String], tx: &VersionedTransaction) -> bool {
    let tokenkeg_idx = keys.iter().position(|k| k == TOKEN_PROGRAM).map(|i| i as u8);
    tokenkeg_idx.is_some_and(|idx| {
        tx.message.instructions().iter().any(|ix| ix.program_id_index == idx && ix.data == [17])
    })
}

pub fn parse_jupiter_swap_result(
    tx_with_meta: &TxWithMeta,
    in_mint: &str,
    out_mint: &str,
) -> Option<SwapData> {
    let tx = &tx_with_meta.transaction;
    let quote_amount = extract_jup_quoted_out(tx);
    // if quote_amount.is_none() {
    //     return None;
    // }

    let keys: Vec<String> = tx
        .message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    keys.iter().position(|k| k == JUPITER_V6)?;

    let signer = &keys[0];
    let balances = tx_with_meta.balance_diffs.as_ref();
    let (pre, post) = balances.map(|b| b.token_balances_or_empty()).unwrap_or((&[], &[]));

    let in_amount = if in_mint == WSOL_MINT && has_sync_native(&keys, tx) {
        // pre[0]-post[0] = swap_amount + fee. Subtract the 5000-lamport base fee to
        // approximate the wrapped amount; priority fees are not accounted for here.
        balances.and_then(|b| {
            b.pre_balances.first().zip(b.post_balances.first())
                .and_then(|(&pre_b, &post_b)| pre_b.checked_sub(post_b + 5_000))
        }).unwrap_or(0)
    } else {
        token_delta(pre, post, signer, in_mint, true).unwrap_or(0)
    };
    if in_amount == 0 {
        return None;
    }

    // X -> SOL swaps often unwrap via CloseAccount so the wSOL ATA ends at 0;
    // fall back to the signer's native SOL gain at account[0].
    let out_amount = signer_received(pre, post, signer, out_mint)
        .or_else(|| {
            if out_mint == WSOL_MINT {
                balances.and_then(|b| {
                    b.post_balances.first().zip(b.pre_balances.first())
                        .and_then(|(&post_b, &pre_b)| post_b.checked_sub(pre_b))
                })
            } else {
                None
            }
        })
        .unwrap_or(0);
    if out_amount == 0 {
        return None;
    }

    let venues: Vec<(String, u64)> = venue_amounts(pre, post, signer, in_mint, &[JUPITER_V6, JUP_FEE_AUTHORITY]);

    Some(SwapData { in_amount, out_amount, quote_amount, venues })
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
    SwapData { in_amount, out_amount, quote_amount: None, venues }
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
    if pre.iter().any(|b| b.owner == signer && b.mint == mint) {
        return None;
    }
    // No pre-entry: new ATA created in this tx; full post-balance is the received amount.
    post.iter()
        .find(|b| b.owner == signer && b.mint == mint)
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
        .find(|b| b.owner == signer && b.mint == mint)?;
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
        if b.mint != input_mint
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

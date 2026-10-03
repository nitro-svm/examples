use std::collections::HashMap;

use super::types::{TitanVenueDiscriminant, TransactionTokenBalanceSerde, TxWithMeta};
use anyhow::{Context, Result};
use base64::Engine as _;
use solana_message::{VersionedMessage, compiled_instruction::CompiledInstruction};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::RpcSimulateTransactionResult;
use solana_transaction::versioned::VersionedTransaction;

pub const SOLANA_RPC: &str = "https://api.mainnet-beta.solana.com";
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
pub const SPCX_MINT: &str = "SPCXxcqXj6e5dJDVNovHN8744zkbhM2bYudU45BimGb";

pub const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const JUP_FEE_AUTHORITY: &str = "45ruCyfdRkWpRNGEqWzjCiXRHkZs8WXCLQ67Pnpye7Hp";

pub const TITAN_PROGRAM: &str = "T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT";
const TITAN_SPLIT_EVENT_DISCRIMINANT: [u8; 8] = [0xb7, 0x1c, 0x17, 0x87, 0xad, 0x7f, 0x7c, 0xea];

// ── Titan v3 single-venue patching ──────────────────────────────────────────

struct ParsedSwap {
    data_start: usize,
    data_end: usize,
    n_accounts: u8,
    venue_disc: u8,
}

/// Advance `pos` past the venue-specific fields that follow the 1-byte discriminant.
/// SanctumDepositWithdraw (21) contains an Option<u32> so its size is data-dependent.
fn skip_venue_extra(disc: u8, data: &[u8], pos: &mut usize) -> Result<()> {
    let n: usize = match disc {
        // 0 extra bytes
        0 | 3 | 4 | 6 | 8 | 9 | 10 | 12 | 15 | 19 | 24 | 36 | 38 | 40 | 41 | 51 | 56 | 59 | 61
        | 62 | 70 => 0,
        // 1 extra byte (bool / u8 field)
        1 | 2 | 5 | 7 | 11 | 14 | 16 | 17 | 18 | 20 | 22 | 23 | 25 | 27 | 30 | 31 | 32 | 37
        | 39 | 43 | 44 | 45 | 49 | 52 | 54 | 55 | 57 | 58 | 60 | 63 | 64 | 65 | 68 | 69 | 71
        | 72 | 73 => 1,
        13 => 8,           // ZeroFi: expected_amount_out u64
        26 | 35 | 42 => 2, // ExponentAmm / GoonFi / Hylo
        28 => 9,           // HumidiFi: is_quote_to_base(1) + swap_id(8)
        29 => 17,          // Bonkswap: x_to_y(1) + price_limit(u128)
        33 => 121,         // HashFlow: txid(32)+from(8)+to(8)+expiry(8)+sig(64)+rec(1)
        34 => 65,          // VaultUnstakepool: split_at(5)+lst_amounts(40)+seeds(20)
        46 | 47 => 5,      // SanctumInf{Add,Remove}Liquidity
        48 => 10,          // SanctumInfSwap
        50 => 3,           // TitanLimitOrders: bumps [u8;3]
        53 => 17,          // Scorch: id(u128) + disc(u8)
        66 | 67 => 4,      // ExponentOB / ExponentClmm
        21 => {
            // SanctumDepositWithdraw: is_deposit(1) + Option<u32>(1 or 5) + split_at(1)
            *pos += 1;
            let tag = *data
                .get(*pos)
                .context("SanctumDepositWithdraw: truncated")?;
            *pos += 1;
            if tag != 0 {
                *pos += 4;
            }
            *pos += 1;
            return Ok(());
        }
        _ => anyhow::bail!("unknown venue discriminant {disc}"),
    };
    *pos += n;
    Ok(())
}

fn parse_titan_v3_swaps(data: &[u8]) -> Result<Vec<ParsedSwap>> {
    anyhow::ensure!(data.len() >= 28, "titan v3 data too short");
    anyhow::ensure!(data[0] == 0x2a, "not swap_route_v3");
    let num_swaps = u32::from_le_bytes(data[24..28].try_into().unwrap()) as usize;
    let mut swaps = Vec::with_capacity(num_swaps);
    let mut pos = 28;
    for _ in 0..num_swaps {
        let start = pos;
        let disc = *data.get(pos).context("truncated: disc")?;
        pos += 1;
        skip_venue_extra(disc, data, &mut pos)?;
        pos += 1 + 1 + 4; // from(u8) + to(u8) + weight_nanos(u32)
        let n_accounts = *data.get(pos).context("truncated: n_accounts")?;
        pos += 1;
        swaps.push(ParsedSwap {
            data_start: start,
            data_end: pos,
            n_accounts,
            venue_disc: disc,
        });
    }
    Ok(swaps)
}

/// Rewrite a swap_route_v3 transaction to route exclusively through `venue_disc`,
/// dropping all other swap entries and their remaining accounts.
/// Sets mesh_size=1 and weight_nanos=1_000_000_000 for the kept entry.
pub fn patch_titan_single_venue(
    tx: &VersionedTransaction,
    venue: &TitanVenueDiscriminant,
) -> Result<VersionedTransaction> {
    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys
        .iter()
        .position(|k| k.to_string() == TITAN_PROGRAM)
        .context("titan not in static keys")? as u8;

    let ixs = tx.message.instructions();
    let (titan_ix_pos, swaps) = ixs
        .iter()
        .enumerate()
        .find_map(|(i, ix)| {
            if ix.program_id_index != titan_idx {
                return None;
            }
            parse_titan_v3_swaps(&ix.data).ok().map(|s| (i, s))
        })
        .context("no swap_route_v3 found")?;

    let discriminant = *venue as u8;
    let target_pos = swaps
        .iter()
        .position(|s| s.venue_disc == discriminant)
        .with_context(|| format!("venue {discriminant} not in swaps"))?;
    let total_rem: usize = swaps.iter().map(|s| s.n_accounts as usize).sum();

    let old_ix = &ixs[titan_ix_pos];
    let old_data = &old_ix.data;

    // TRUE single-venue strip: keep ONLY the target swap (num_swaps=1).
    let target = &swaps[target_pos];
    let n_before: usize = swaps[..target_pos]
        .iter()
        .map(|s| s.n_accounts as usize)
        .sum();
    let prefix = old_ix.accounts.len() - total_rem;
    let venue_start = prefix + n_before;
    let venue_end = venue_start + target.n_accounts as usize;
    let mut new_accounts = old_ix.accounts[..prefix].to_vec();
    new_accounts.extend_from_slice(&old_ix.accounts[venue_start..venue_end]);

    // The last prefix slot (accounts[prefix-1]) holds the FIRST swap's market account
    // (ZeroFi's, in our template). When the target isn't swap 0, that first venue is stripped
    // and the slot becomes an orphan account. A native single-venue route puts the generic
    // Titan `atlas` marker there instead — the same account at accounts[2]. Repoint to match.
    // (If we're isolating the first venue itself, its market belongs there — leave it.)
    if target_pos != 0 {
        new_accounts[prefix - 1] = new_accounts[2];
    }

    // Data: header (slippage→100%, mesh_size→1), num_swaps=1, then only the target entry.
    let mut new_data = old_data[..24].to_vec();
    new_data[18..20].copy_from_slice(&10_000u16.to_le_bytes()); // slippage_threshold_bps
    new_data[23] = 1; // mesh_size
    new_data.extend_from_slice(&1u32.to_le_bytes()); // num_swaps = 1
    let mut swap_bytes = old_data[target.data_start..target.data_end].to_vec();
    let w_off = swap_bytes.len() - 5; // weight_nanos(4) + n_accounts(1) from end
    swap_bytes[w_off..w_off + 4].copy_from_slice(&1_000_000_000u32.to_le_bytes());
    new_data.extend_from_slice(&swap_bytes);

    let mut new_tx = tx.clone();
    let new_ixs = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => &mut msg.instructions,
        VersionedMessage::V0(msg) => &mut msg.instructions,
        VersionedMessage::V1(msg) => &mut msg.instructions,
    };
    new_ixs[titan_ix_pos].data = new_data;
    new_ixs[titan_ix_pos].accounts = new_accounts;

    Ok(new_tx)
}

/// Point Titan's `positive_slippage_fee_receiver` slot back at the "None" sentinel so the
/// venue's full output lands in the user's output token account.
pub fn patch_titan_disable_positive_slippage_fee(
    tx: &VersionedTransaction,
) -> Result<VersionedTransaction> {
    const POSITIVE_SLIPPAGE_FEE_RECEIVER: usize = 11;

    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys
        .iter()
        .position(|k| k.to_string() == TITAN_PROGRAM)
        .context("titan program not in static keys")? as u8;

    let mut new_tx = tx.clone();
    let ixs = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => &mut msg.instructions,
        VersionedMessage::V0(msg) => &mut msg.instructions,
        VersionedMessage::V1(msg) => &mut msg.instructions,
    };
    let titan_ix = ixs
        .iter_mut()
        .find(|ix| ix.program_id_index == titan_idx && ix.data.first() == Some(&0x2a))
        .context("no swap_route_v3 titan ix found")?;

    // "None" for an Anchor optional account == the program id, i.e. program_id_index.
    let none_sentinel = titan_ix.program_id_index;
    let slot = titan_ix
        .accounts
        .get_mut(POSITIVE_SLIPPAGE_FEE_RECEIVER)
        .with_context(|| {
            format!("titan ix has no account at position {POSITIVE_SLIPPAGE_FEE_RECEIVER}")
        })?;
    *slot = none_sentinel;

    Ok(new_tx)
}

/// Fetch an Address Lookup Table account and return its stored addresses.
/// ALT layout: 56-byte meta header (LOOKUP_TABLE_META_SIZE), then packed 32-byte pubkeys.
async fn fetch_alt_addresses(alt_pubkey: &str) -> Result<Vec<Pubkey>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [alt_pubkey, {"encoding": "base64"}]
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(SOLANA_RPC)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let encoded = resp["result"]["value"]["data"][0]
        .as_str()
        .context("alt account data not found")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    anyhow::ensure!(bytes.len() >= 56, "alt too short");
    Ok(bytes[56..]
        .chunks_exact(32)
        .map(|c| Pubkey::try_from(c).unwrap())
        .collect())
}

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
    ([0x9d, 0x8a, 0xb8, 0x52, 0x15, 0xf4, 0xf3, 0x24], 8),  // exact_out_route_v2
    ([0x35, 0x60, 0xe5, 0xca, 0xd8, 0xbb, 0xfa, 0x18], 9),  // shared_accounts_exact_out_route_v2
];

pub fn extract_jup_quoted_out(tx: &VersionedTransaction) -> Option<u64> {
    let keys = tx.message.static_account_keys();
    let jup_idx = keys.iter().position(|k| k.to_string() == JUPITER_V6)? as u8;
    for ix in tx
        .message
        .instructions()
        .iter()
        .filter(|ix| ix.program_id_index == jup_idx)
    {
        let data = ix.data.as_slice();
        let Ok(disc) = <[u8; 8]>::try_from(data.get(..8)?) else {
            continue;
        };
        // Filter out quoted_out <= 0: senders often set 1 to bypass slippage checks.
        let filter = |q: u64| if q > 0 { Some(q) } else { None };
        if let Some(&(_, off)) = JUP_V2_OFFSETS.iter().find(|(d, _)| d == &disc) {
            return data
                .get(off..off + 8)?
                .try_into()
                .ok()
                .map(u64::from_le_bytes)
                .and_then(filter);
        } else if JUP_V1_ROUTE_DISCRIMINANTS.contains(&disc) && data.len() >= 19 {
            let len = data.len();
            return filter(u64::from_le_bytes(
                data[len - 11..len - 3].try_into().unwrap(),
            ));
        } else if JUP_V1_EXACT_OUT_DISCRIMINANTS.contains(&disc) && data.len() >= 27 {
            let len = data.len();
            return filter(u64::from_le_bytes(
                data[len - 19..len - 11].try_into().unwrap(),
            ));
        }
    }
    None
}

/// Zero out the minimum-out (quoted_out) field in every Jupiter instruction so
/// that slippage checks never fail during resimulation. Mirrors what
/// `patch_titan_template_transaction` does for Titan's expected_amount_out.
pub fn patch_jup_min_out(tx: &VersionedTransaction) -> VersionedTransaction {
    let keys = tx.message.static_account_keys();
    let Some(jup_idx) = keys
        .iter()
        .position(|k| k.to_string() == JUPITER_V6)
        .map(|i| i as u8)
    else {
        return tx.clone();
    };

    let mut new_tx = tx.clone();
    let ixs = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => &mut msg.instructions,
        VersionedMessage::V0(msg) => &mut msg.instructions,
        VersionedMessage::V1(msg) => &mut msg.instructions,
    };

    for ix in ixs.iter_mut().filter(|ix| ix.program_id_index == jup_idx) {
        let data = &mut ix.data;
        let Ok(disc) = <[u8; 8]>::try_from(data.get(..8).unwrap_or(&[])) else {
            continue;
        };

        if let Some(&(_, off)) = JUP_V2_OFFSETS.iter().find(|(d, _)| d == &disc) {
            if data.len() >= off + 8 {
                data[off..off + 8].copy_from_slice(&0u64.to_le_bytes());
            }
        } else if JUP_V1_ROUTE_DISCRIMINANTS.contains(&disc) && data.len() >= 19 {
            let len = data.len();
            data[len - 11..len - 3].copy_from_slice(&0u64.to_le_bytes());
        } else if JUP_V1_EXACT_OUT_DISCRIMINANTS.contains(&disc) && data.len() >= 27 {
            let len = data.len();
            data[len - 19..len - 11].copy_from_slice(&0u64.to_le_bytes());
        }
    }
    new_tx
}

fn has_sync_native(keys: &[String], tx: &VersionedTransaction) -> bool {
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
    let (pre, post) = balances
        .map(|b| b.token_balances_or_empty())
        .unwrap_or((&[], &[]));

    let in_amount = if in_mint == WSOL_MINT && has_sync_native(&keys, tx) {
        // pre[0]-post[0] = swap_amount + fee. Subtract the 5000-lamport base fee to
        // approximate the wrapped amount; priority fees are not accounted for here.
        balances
            .and_then(|b| {
                b.pre_balances
                    .first()
                    .zip(b.post_balances.first())
                    .and_then(|(&pre_b, &post_b)| pre_b.checked_sub(post_b + 5_000))
            })
            .unwrap_or(0)
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
                    b.post_balances
                        .first()
                        .zip(b.pre_balances.first())
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

    let venues: Vec<(String, u64)> =
        venue_amounts(pre, post, signer, in_mint, &[JUPITER_V6, JUP_FEE_AUTHORITY]);

    Some(SwapData {
        in_amount,
        out_amount,
        quote_amount,
        venues,
    })
}

pub async fn get_titan_template_data(signature: &str) -> Result<VersionedTransaction> {
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
        VersionedMessage::V1(msg) => ix_acct(&msg.instructions, titan_idx, 3),
    }
    .context("could not find in_ata in titan ix")? as usize;

    let mut new_tx = tx.clone();
    let (keys, ixs) = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => (&mut msg.account_keys, &mut msg.instructions),
        VersionedMessage::V0(msg) => (&mut msg.account_keys, &mut msg.instructions),
        VersionedMessage::V1(msg) => (&mut msg.account_keys, &mut msg.instructions),
    };

    *keys
        .get_mut(in_ata_key_idx)
        .context("in_ata_key_idx out of range")? = in_ata;

    let titan_ix = ixs
        .iter_mut()
        .find(|ix| ix.program_id_index == titan_idx && ix.data.first() == Some(&0x2a))
        .context("swap_route_v3 ix not found")?;
    // v3 layout: [0]=disc(0x2a) [1]=config [2..10]=amount [10..18]=expected_amount_out
    //            [18..20]=slippage_threshold_bps [20]=mints [21..23]=fee_centi_bps
    anyhow::ensure!(titan_ix.data.len() >= 23, "titan ix data too short for v3");
    anyhow::ensure!(
        titan_ix.data[0] == 0x2a,
        "unexpected titan ix discriminator (not swap_route_v3)"
    );
    titan_ix.data[2..10].copy_from_slice(&in_amount.to_le_bytes());
    // zero expected_amount_out so simulation never fails on stale slippage
    titan_ix.data[10..18].copy_from_slice(&0u64.to_le_bytes());
    // bump dynamic-allocation slippage threshold to 10,000 bps (100%) so resimulation doesn't error
    titan_ix.data[18..20].copy_from_slice(&10_000u16.to_le_bytes());
    // zero Titan's routing fee (fee_centi_bps): we measure the venue's raw quote, not Titan's fee
    titan_ix.data[21..23].copy_from_slice(&0u16.to_le_bytes());

    Ok(new_tx)
}

/// Overwrite the *static* account key referenced at `position` of the Titan swap ix.
/// (account ordering: 0=payer, 1=user, 3=input_token_account, 4=output_token_account, 9=token_ledger, …).
/// Only valid when that position resolves to a static key used **solely** at that position;
/// the caller must ensure no aliasing, since overwriting the slot moves every other reference too.
pub fn repoint_titan_static_account(
    tx: &VersionedTransaction,
    position: usize,
    new_key: Pubkey,
) -> Result<VersionedTransaction> {
    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys
        .iter()
        .position(|k| k.to_string() == TITAN_PROGRAM)
        .context("titan program not in static keys")? as u8;
    let key_idx = match &tx.message {
        VersionedMessage::Legacy(msg) => ix_acct(&msg.instructions, titan_idx, position),
        VersionedMessage::V0(msg) => ix_acct(&msg.instructions, titan_idx, position),
        VersionedMessage::V1(msg) => ix_acct(&msg.instructions, titan_idx, position),
    }
    .with_context(|| format!("titan ix has no account at position {position}"))?
        as usize;
    anyhow::ensure!(
        key_idx < static_keys.len(),
        "position {position} resolves to an ALT key (idx {key_idx}); not statically repointable"
    );

    let mut new_tx = tx.clone();
    let keys = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => &mut msg.account_keys,
        VersionedMessage::V0(msg) => &mut msg.account_keys,
        VersionedMessage::V1(msg) => &mut msg.account_keys,
    };
    *keys.get_mut(key_idx).context("key idx out of range")? = new_key;
    Ok(new_tx)
}

/// Insert `key` as a fresh *writable* non-signer static key and point the Titan swap's
/// account at `position` to it. For slots whose current key is aliased, an
/// in-place rewrite via [`repoint_titan_static_account`] would move every reference;
/// inserting a new key repoints only the one slot. The key goes at the end of the
/// writable non-signer region, so every index at or past the insertion point shifts up.
pub fn repoint_titan_account_via_new_key(
    tx: &VersionedTransaction,
    position: usize,
    key: Pubkey,
) -> Result<VersionedTransaction> {
    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys
        .iter()
        .position(|k| k.to_string() == TITAN_PROGRAM)
        .context("titan program not in static keys")? as u8;
    let num_readonly = match &tx.message {
        VersionedMessage::Legacy(msg) => msg.header.num_readonly_unsigned_accounts,
        VersionedMessage::V0(msg) => msg.header.num_readonly_unsigned_accounts,
        VersionedMessage::V1(msg) => msg.header.num_readonly_unsigned_accounts,
    };
    let insert_idx = static_keys.len() as u8 - num_readonly;

    let mut new_tx = tx.clone();
    let (keys, ixs) = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => (&mut msg.account_keys, &mut msg.instructions),
        VersionedMessage::V0(msg) => (&mut msg.account_keys, &mut msg.instructions),
        VersionedMessage::V1(msg) => (&mut msg.account_keys, &mut msg.instructions),
    };
    keys.insert(insert_idx as usize, key);
    for ix in ixs.iter_mut() {
        if ix.program_id_index >= insert_idx {
            ix.program_id_index += 1;
        }
        for a in &mut ix.accounts {
            if *a >= insert_idx {
                *a += 1;
            }
        }
    }

    let titan_idx = if titan_idx >= insert_idx {
        titan_idx + 1
    } else {
        titan_idx
    };
    let titan_ix = ixs
        .iter_mut()
        .find(|ix| ix.program_id_index == titan_idx && ix.data.first() == Some(&0x2a))
        .context("swap_route_v3 ix not found")?;
    *titan_ix
        .accounts
        .get_mut(position)
        .with_context(|| format!("titan ix has no account at position {position}"))? = insert_idx;
    Ok(new_tx)
}

/// Append `ledger` as a read-only static key and point the Titan swap's`token_ledger` slot at it,
/// so the swap reads its input amount from the ledger (`input = balance(tracked) - ledger.amount`)
/// instead of the `amount` arg.
///
/// In our template txs, the `token_ledger` slot (and the `positive_slippage_fee_receiver` slot)
/// are omitted optional accounts, which Anchor encodes by pointing them at the program id.
/// That placeholder index is the same one the instruction uses as its `program_id_index`,
/// so it can't be overwritten in place — doing so would also redirect the program reference.
/// Instead we push a fresh key at the end of the static-key region and bump every instruction's
/// ALT-key reference up by one, since the new static key shifts the resolved ALT indices.
pub fn add_token_ledger(tx: &VersionedTransaction, ledger: Pubkey) -> Result<VersionedTransaction> {
    const TOKEN_LEDGER_POSITION: usize = 9;
    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys
        .iter()
        .position(|k| k.to_string() == TITAN_PROGRAM)
        .context("titan program not in static keys")? as u8;
    let old_static_len = static_keys.len() as u8;

    let mut new_tx = tx.clone();
    match &mut new_tx.message {
        VersionedMessage::V0(msg) => {
            for ix in &mut msg.instructions {
                if ix.program_id_index >= old_static_len {
                    ix.program_id_index += 1;
                }
                for a in &mut ix.accounts {
                    if *a >= old_static_len {
                        *a += 1;
                    }
                }
            }
            msg.account_keys.push(ledger);
            msg.header.num_readonly_unsigned_accounts += 1;
        }
        VersionedMessage::Legacy(msg) => {
            msg.account_keys.push(ledger);
            msg.header.num_readonly_unsigned_accounts += 1;
        }
        // V1 (SIMD-0385) has no address-table lookups,
        // so the index-bumping doesn't apply and wouldn't appear in history.
        VersionedMessage::V1(_) => anyhow::bail!("add_token_ledger: v1 messages unsupported"),
    }

    let ixs = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => &mut msg.instructions,
        VersionedMessage::V0(msg) => &mut msg.instructions,
        VersionedMessage::V1(msg) => &mut msg.instructions,
    };
    let titan_ix = ixs
        .iter_mut()
        .find(|ix| ix.program_id_index == titan_idx)
        .context("titan ix not found")?;
    *titan_ix
        .accounts
        .get_mut(TOKEN_LEDGER_POSITION)
        .context("titan ix has no token_ledger position")? = old_static_len;
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
    SwapData {
        in_amount,
        out_amount,
        quote_amount: None,
        venues,
    }
}

/// Detect any non-circular Jupiter swap by scanning the signer's token balance
/// changes. Returns `(in_mint, out_mint, SwapData)` or `None` if the tx is not
/// a Jupiter swap or is a circular arb.
pub fn parse_any_jupiter_swap(tx_with_meta: &TxWithMeta) -> Option<(String, String, SwapData)> {
    let tx = &tx_with_meta.transaction;
    let keys: Vec<String> = tx
        .message
        .static_account_keys()
        .iter()
        .map(|k| k.to_string())
        .collect();
    keys.iter().position(|k| k == JUPITER_V6)?;

    let signer = &keys[0];
    let balances = tx_with_meta.balance_diffs.as_ref()?;
    let (pre, post) = balances.token_balances_or_empty();

    // Signer's pre amounts keyed by mint.
    let signer_pre: HashMap<&str, u64> = pre
        .iter()
        .filter(|b| &b.owner == signer)
        .filter_map(|b| {
            b.ui_token_amount
                .amount
                .parse()
                .ok()
                .map(|a| (b.mint.as_str(), a))
        })
        .collect();

    // Largest token decrease → in_mint.
    let token_in = pre
        .iter()
        .filter(|b| &b.owner == signer)
        .filter_map(|b| {
            let pre_amt: u64 = b.ui_token_amount.amount.parse().ok()?;
            let post_amt: u64 = post
                .iter()
                .find(|p| p.account_index == b.account_index)
                .and_then(|p| p.ui_token_amount.amount.parse().ok())
                .unwrap_or(0);
            pre_amt
                .checked_sub(post_amt)
                .filter(|&d| d > 0)
                .map(|d| (b.mint.clone(), d))
        })
        .max_by_key(|(_, d)| *d);

    // Largest token increase → out_mint.
    let token_out = post
        .iter()
        .filter(|b| &b.owner == signer)
        .filter_map(|b| {
            let post_amt: u64 = b.ui_token_amount.amount.parse().ok()?;
            let pre_amt = signer_pre.get(b.mint.as_str()).copied().unwrap_or(0);
            post_amt
                .checked_sub(pre_amt)
                .filter(|&d| d > 0)
                .map(|d| (b.mint.clone(), d))
        })
        .max_by_key(|(_, d)| *d);

    // SOL as input: present when SyncNative wraps lamports.
    let has_sync = has_sync_native(&keys, tx);
    let sol_in = if has_sync {
        balances
            .pre_balances
            .first()
            .zip(balances.post_balances.first())
            .and_then(|(&pre_b, &post_b)| pre_b.checked_sub(post_b + 5_000))
            .unwrap_or(0)
    } else {
        0
    };

    // SOL as output: native balance increase (no SyncNative means SOL wasn't wrapped in).
    let sol_out = if !has_sync {
        balances
            .post_balances
            .first()
            .zip(balances.pre_balances.first())
            .and_then(|(&post_b, &pre_b)| post_b.checked_sub(pre_b))
            .unwrap_or(0)
    } else {
        0
    };

    let (in_mint, in_amount) =
        if has_sync && sol_in > 0 && token_in.as_ref().is_none_or(|(_, d)| sol_in >= *d) {
            (WSOL_MINT.to_string(), sol_in)
        } else {
            token_in?
        };

    let (out_mint, out_amount) =
        if sol_out > 0 && token_out.as_ref().is_none_or(|(_, d)| sol_out >= *d) {
            (WSOL_MINT.to_string(), sol_out)
        } else if let Some(t) = token_out {
            t
        } else if sol_out > 0 {
            (WSOL_MINT.to_string(), sol_out)
        } else {
            return None;
        };

    if in_mint == out_mint || in_amount == 0 || out_amount == 0 {
        return None;
    }

    let quote_amount = extract_jup_quoted_out(tx);
    let venues = venue_amounts(
        pre,
        post,
        signer,
        &in_mint,
        &[JUPITER_V6, JUP_FEE_AUTHORITY],
    );

    Some((
        in_mint,
        out_mint,
        SwapData {
            in_amount,
            out_amount,
            quote_amount,
            venues,
        },
    ))
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

/// ATA derivation seeded with the legacy Token program. Wrong for Token-2022 mints —
/// use [`derive_ata_with_program`] when the mint's owning program isn't already known
/// to be legacy Token.
pub fn derive_ata(wallet: &Pubkey, mint_str: &str) -> Option<Pubkey> {
    derive_ata_with_program(wallet, mint_str, TOKEN_PROGRAM)
}

/// ATA derivation seeded with `token_program` — must be the mint's real owning program
/// (legacy Token or Token-2022), since the ATA address depends on which one is used.
pub fn derive_ata_with_program(
    wallet: &Pubkey,
    mint_str: &str,
    token_program: &str,
) -> Option<Pubkey> {
    let assoc: Pubkey = ASSOCIATED_TOKEN_PROGRAM.parse().ok()?;
    let token: Pubkey = token_program.parse().ok()?;
    let mint: Pubkey = mint_str.parse().ok()?;
    let (ata, _) =
        Pubkey::find_program_address(&[wallet.as_ref(), token.as_ref(), mint.as_ref()], &assoc);
    Some(ata)
}

/// Titan's `swap_route_v3` instruction, not merely the first one Titan owns: a template
/// can carry several Titan instructions (a route plus setup/limit-order calls), and only
/// the route has the account layout and args the patchers rewrite. Selecting by program
/// id alone silently picks whichever comes first.
fn titan_route_ix(ixs: &[CompiledInstruction], prog: u8) -> Option<&CompiledInstruction> {
    ixs.iter()
        .find(|ix| ix.program_id_index == prog && ix.data.first() == Some(&0x2a))
}

fn ix_acct(ixs: &[CompiledInstruction], prog: u8, pos: usize) -> Option<u8> {
    titan_route_ix(ixs, prog)?.accounts.get(pos).copied()
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
    let pre_e = pre.iter().find(|b| b.owner == signer && b.mint == mint)?;
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
        if b.mint != input_mint || owner == signer || skip_owners.contains(&owner.as_str()) {
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

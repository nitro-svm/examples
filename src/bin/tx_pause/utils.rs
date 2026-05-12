use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine as _;
use history_model::{TransactionTokenBalanceSerde, TxWithMeta};
use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::RpcSimulateTransactionResult;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::UiTransactionTokenBalance;

pub const SOLANA_RPC: &str = "https://api.mainnet-beta.solana.com";
pub const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const TITAN_PROGRAM: &str = "T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// Anchor discriminants
// - route: user provides all ATAs
// - sharedAccountsRoute: uses a shared intermediate ATA to allow fee-free hops
pub const JUP_ROUTE_DISCRIMINANT: [u8; 8] = [229, 23, 203, 151, 122, 227, 173, 42];
pub const JUP_SHARED_DISCRIMINANT: [u8; 8] = [193, 32, 155, 51, 65, 214, 156, 129];
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

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
pub fn parse_jupiter_swap_result(tx_with_meta: &TxWithMeta, input: &str, output: &str) -> Option<SwapData> {
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

    // Jupiter swap instruction only has two entrypoints
    if !jup_ix.data.starts_with(&JUP_ROUTE_DISCRIMINANT) && !jup_ix.data.starts_with(&JUP_SHARED_DISCRIMINANT) {
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
            // Many USDC→SOL JUP swaps use wSOL unwrap: create wSOL ATA → swap into it →
            // CloseAccount. The wSOL ATA ends at 0, so token_delta returns 0. Fall back
            // to the signer's native SOL increase (Account 0).
            let out_amount = token_delta(pre, post, signer, output, false)
                .filter(|&a| a > 0)
                .or_else(|| {
                    if output == WSOL_MINT {
                        bd.post_balances.first()?.checked_sub(*bd.pre_balances.first()?)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let venues = venue_amounts(pre, post, signer, input, &[JUPITER_V6]);
            (input_amount, out_amount, venues)
        })
        .unwrap_or_default();

    if out_amount == 0 {
        return None;
    }

    Some(SwapData { in_amount: input_amount, out_amount, venues })
}

// discriminant for Titan's per-split swap event (b71c1787ad7f7cea)
const TITAN_SPLIT_EVENT_DISC: [u8; 8] = [0xb7, 0x1c, 0x17, 0x87, 0xad, 0x7f, 0x7c, 0xea];

pub struct TitanSplitEvent {
    pub split_idx: u8,
    pub in_amount: u64,
    pub expected_min_out: u64,
    pub actual_out: u64,
}

// event layout: disc(8) | split_idx(1) | in(8) | in(8) | expected_min(8) | actual_out(8) = 41 bytes
pub fn parse_titan_events(logs: &[String]) -> Vec<TitanSplitEvent> {
    logs.iter()
        .filter_map(|log| {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(log.strip_prefix("Program data: ")?).ok()?;
            if raw.len() < 41 || raw[..8] != TITAN_SPLIT_EVENT_DISC { return None; }
            Some(TitanSplitEvent {
                split_idx: raw[8],
                in_amount: u64::from_le_bytes(raw[9..17].try_into().ok()?),
                expected_min_out: u64::from_le_bytes(raw[25..33].try_into().ok()?),
                actual_out: u64::from_le_bytes(raw[33..41].try_into().ok()?),
            })
        })
        .collect()
}

pub fn parse_titan_sim_result(
    tx: &VersionedTransaction,
    result: &RpcSimulateTransactionResult,
    _input: &str,
    _output: &str,
) -> (u64, Vec<(String, u64)>) {
    if let Some(err) = &result.err {
        eprintln!("  [titan sim error] {:?}", err);
    }

    let logs = result.logs.as_deref().unwrap_or(&[]);

    // Pair each split event with the last invoke [2] program seen before it.
    // Titan emits a Program data: event immediately after each DEX CPI completes.
    let mut last_invoke2: Option<String> = None;
    let mut venues: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut out_amount = 0u64;

    for log in logs {
        if let Some(prog) = log.strip_prefix("Program ").and_then(|s| s.strip_suffix(" invoke [2]")) {
            last_invoke2 = Some(prog.to_string());
        } else if let Some(b64) = log.strip_prefix("Program data: ") {
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) {
                if raw.len() >= 41 && raw[..8] == TITAN_SPLIT_EVENT_DISC {
                    let in_amount  = u64::from_le_bytes(raw[9..17].try_into().unwrap());
                    let actual_out = u64::from_le_bytes(raw[33..41].try_into().unwrap());
                    out_amount += actual_out;
                    if let Some(prog) = &last_invoke2 {
                        *venues.entry(prog.clone()).or_insert(0) += in_amount;
                    }
                }
            }
        }
    }

    let mut venues: Vec<(String, u64)> = venues.into_iter().collect();
    venues.sort_by_key(|(_, a)| std::cmp::Reverse(*a));

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

pub fn extract_signer_and_input_ata(tx: &TxWithMeta, in_mint: &str) -> Result<(Pubkey, Pubkey)> {
    let accounts = tx.transaction.message.static_account_keys();
    let signer_str = accounts.first().context("no static account keys")?.to_string();

    let (pre, _) = tx
        .balance_diffs
        .as_ref()
        .context("no balance diffs")?
        .token_balances_or_empty();
    let input_entry = pre
        .iter()
        .find(|b| b.owner.to_string() == signer_str && b.mint.to_string() == in_mint)
        .context("signer has no input-mint token account in pre-balances")?;
    let in_ata: Pubkey = accounts
        .get(input_entry.account_index as usize)
        .context("input ATA account_index out of range")?
        .to_string()
        .parse()
        .context("invalid in_ata pubkey")?;

    Ok((signer_str.parse().context("invalid signer pubkey")?, in_ata))
}

fn titan_ix_acct(tx: &VersionedTransaction, pos: usize) -> Option<Pubkey> {
    use solana_message::compiled_instruction::CompiledInstruction as IX;
    fn ix_acct(ixs: &[IX], prog: u8, pos: usize) -> Option<u8> {
        ixs.iter().find(|ix| ix.program_id_index == prog)?.accounts.get(pos).copied()
    }
    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys.iter().position(|k| k.to_string() == TITAN_PROGRAM)? as u8;
    let idx = match &tx.message {
        VersionedMessage::Legacy(msg) => ix_acct(&msg.instructions, titan_idx, pos)?,
        VersionedMessage::V0(msg)     => ix_acct(&msg.instructions, titan_idx, pos)?,
    } as usize;
    Some(*static_keys.get(idx)?)
}

/// Returns the address of the input ATA in the template's Titan instruction (account slot 3).
pub fn template_in_ata_addr(tx: &VersionedTransaction) -> Option<Pubkey> {
    titan_ix_acct(tx, 3)
}

/// Returns the address of the output ATA in the template's Titan instruction (account slot 5).
pub fn template_out_ata_addr(tx: &VersionedTransaction) -> Option<Pubkey> {
    titan_ix_acct(tx, 5)
}

/// Bumps the per-split slippage tolerance byte (experimentally byte 23 = ~20 bps) to avoid
/// stale-price rejections when simulating the template at a different slot.
pub fn relax_titan_slippage(tx: &VersionedTransaction) -> Option<VersionedTransaction> {
    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys.iter().position(|k| k.to_string() == TITAN_PROGRAM)? as u8;
    let mut new_tx = tx.clone();
    let ixs = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => &mut msg.instructions,
        VersionedMessage::V0(msg)     => &mut msg.instructions,
    };
    let titan_ix = ixs.iter_mut().find(|ix| ix.program_id_index == titan_idx)?;
    if titan_ix.data.len() <= 23 { return None; }
    titan_ix.data[18] = (titan_ix.data[18] as u16 * 11 / 10) as u8;
    titan_ix.data[23] = (titan_ix.data[23] as u16 * 11 / 10) as u8;
    Some(new_tx)
}

const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

pub fn derive_ata(wallet: &Pubkey, mint_str: &str) -> Option<Pubkey> {
    let assoc: Pubkey = ASSOCIATED_TOKEN_PROGRAM.parse().ok()?;
    let token: Pubkey = TOKEN_PROGRAM.parse().ok()?;
    let mint: Pubkey = mint_str.parse().ok()?;
    let (ata, _) = Pubkey::find_program_address(
        &[wallet.as_ref(), token.as_ref(), mint.as_ref()],
        &assoc,
    );
    Some(ata)
}

/// Patches the template for a JUP-comparison simulation: replaces signer, in_ata, in_amount,
/// derives and injects the signer's wSOL output ATA, prepends CreateIdempotent, zeros slippage.
pub fn patch_titan_template_transaction(
    tx: &VersionedTransaction,
    signer: Pubkey,
    in_ata: Pubkey,
    in_amount: u64,
) -> Option<VersionedTransaction> {
    use solana_message::compiled_instruction::CompiledInstruction as IX;

    fn ix_acct(ixs: &[IX], prog: u8, pos: usize) -> Option<u8> {
        ixs.iter().find(|ix| ix.program_id_index == prog)?.accounts.get(pos).copied()
    }

    fn get_or_add(keys: &mut Vec<Pubkey>, ixs: &mut Vec<IX>, key: Pubkey) -> u8 {
        if let Some(i) = keys.iter().position(|k| k == &key) { return i as u8; }
        let idx = keys.len() as u8;
        for ix in ixs.iter_mut() {
            if ix.program_id_index >= idx { ix.program_id_index += 1; }
            for a in ix.accounts.iter_mut() { if *a >= idx { *a += 1; } }
        }
        keys.push(key);
        idx
    }

    let static_keys = tx.message.static_account_keys();
    let titan_idx = static_keys.iter().position(|k| k.to_string() == TITAN_PROGRAM)? as u8;
    let in_ata_key_idx = match &tx.message {
        VersionedMessage::Legacy(msg) => ix_acct(&msg.instructions, titan_idx, 3)?,
        VersionedMessage::V0(msg)     => ix_acct(&msg.instructions, titan_idx, 3)?,
    } as usize;

    let out_ata = derive_ata(&signer, WSOL_MINT)?;

    let mut new_tx = tx.clone();
    let (keys, ixs) = match &mut new_tx.message {
        VersionedMessage::Legacy(msg) => (&mut msg.account_keys, &mut msg.instructions),
        VersionedMessage::V0(msg)     => (&mut msg.account_keys, &mut msg.instructions),
    };

    *keys.get_mut(0)? = signer;
    *keys.get_mut(in_ata_key_idx)? = in_ata;

    let out_ata_idx = get_or_add(keys, ixs, out_ata);
    {
        let titan_ix = ixs.iter_mut().find(|ix| ix.program_id_index == titan_idx)?;
        if titan_ix.data.len() < 10 { return None; }
        titan_ix.data[2..10].copy_from_slice(&in_amount.to_le_bytes());
        titan_ix.data[10..].fill(0);
        if let Some(a) = titan_ix.accounts.get_mut(5) { *a = out_ata_idx; }
    }

    let assoc_idx = get_or_add(keys, ixs, ASSOCIATED_TOKEN_PROGRAM.parse().ok()?);
    let wsol_idx  = get_or_add(keys, ixs, WSOL_MINT.parse().ok()?);
    let sys_idx   = get_or_add(keys, ixs, SYSTEM_PROGRAM.parse().ok()?);
    let token_idx = get_or_add(keys, ixs, TOKEN_PROGRAM.parse().ok()?);

    let titan_pos = ixs.iter().position(|ix| ix.program_id_index == titan_idx)?;
    ixs.insert(titan_pos, IX {
        program_id_index: assoc_idx,
        accounts: vec![0, out_ata_idx, 0, wsol_idx, sys_idx, token_idx],
        data: vec![1],
    });

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

// Max increase across any token account with the given mint.
fn ui_any_output_delta(
    pre: &[UiTransactionTokenBalance],
    post: &[UiTransactionTokenBalance],
    mint: &str,
) -> Option<u64> {
    let pre_map: HashMap<u8, u64> = pre
        .iter()
        .filter(|b| b.mint == mint)
        .filter_map(|b| Some((b.account_index, b.ui_token_amount.amount.parse::<u64>().ok()?)))
        .collect();
    post.iter()
        .filter(|b| b.mint == mint)
        .filter_map(|b| {
            let post_amt: u64 = b.ui_token_amount.amount.parse().ok()?;
            let pre_amt = pre_map.get(&b.account_index).copied().unwrap_or(0);
            post_amt.checked_sub(pre_amt).filter(|&d| d > 0)
        })
        .max()
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

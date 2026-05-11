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

    if let Some(err) = &result.err {
        eprintln!("  [titan sim error] {:?}", err);
        if let Some(logs) = &result.logs {
            for log in logs {
                eprintln!("  [titan log] {}", log);
            }
        } else {
            eprintln!("  [titan sim error] (no logs — likely preflight failure)");
        }
        eprintln!("  [titan sim accounts]");
        for (i, k) in tx.message.static_account_keys().iter().enumerate() {
            eprintln!("    static[{i}] = {k}");
        }
        for ix in tx.message.instructions() {
            eprintln!("    ix prog={} accts={:?}", ix.program_id_index, ix.accounts);
        }
    }

    let pre = result.pre_token_balances.as_deref().unwrap_or(&[]);
    let post = result.post_token_balances.as_deref().unwrap_or(&[]);

    // Titan writes to the signer's wSOL ATA (created via CreateIdempotent).
    // Fall back to any wSOL increase in case the ATA owner isn't set in simulation metadata.
    let out_amount = ui_any_output_delta(pre, post, output).unwrap_or(0);
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

const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJe1bL";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

fn derive_ata(wallet: &Pubkey, mint_str: &str) -> Option<Pubkey> {
    let assoc: Pubkey = ASSOCIATED_TOKEN_PROGRAM.parse().ok()?;
    let token: Pubkey = TOKEN_PROGRAM.parse().ok()?;
    let mint: Pubkey = mint_str.parse().ok()?;
    let (ata, _) = Pubkey::find_program_address(
        &[wallet.as_ref(), token.as_ref(), mint.as_ref()],
        &assoc,
    );
    Some(ata)
}

/// Patches the template to simulate a swap for `signer` with their `in_ata` and `in_amount`.
/// Derives the signer's wSOL ATA, ensures it's created via CreateIdempotent, zeros slippage.
/// Requires ATP to be pre-loaded into the simulator session via getAccountInfo before use.
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

    // Appends `key` to static keys if absent, shifting all LUT-based account indices up by 1.
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

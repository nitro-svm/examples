use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use simulator_api::AccountData;
use simulator_client::build_program_injection;
use solana_address::Address;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_loader_v3_interface::get_program_data_address;
use solana_rpc_client_api::config::RpcTransactionConfig;
use solana_signature::Signature;
use solana_transaction_status::{
    EncodedTransaction, UiMessage, UiTransactionEncoding,
    option_serializer::OptionSerializer,
};

use super::{SolAccount, TokenAccount};

/// Reads `so_path`, constructs a patched `ProgramData` account, and returns
/// it as an account modification map ready for `Continue::modify_account_states`.
///
/// The ProgramData header is built from scratch (no mainnet fetch); the deploy
/// slot is set to `start_slot - 1` so the program appears deployed before the
/// first executed slot.
pub async fn build_program_injection_from_file(
    program_id_str: &str,
    so_path: &std::path::Path,
    rpc: &RpcClient,
    start_slot: u64,
) -> Result<BTreeMap<Address, AccountData>> {
    let elf = std::fs::read(so_path)
        .with_context(|| format!("failed to read {}", so_path.display()))?;
    eprintln!("[inject] {} bytes from {}", elf.len(), so_path.display());

    let program_id: solana_pubkey::Pubkey =
        program_id_str.parse().context("invalid program id")?;
    let programdata_addr = get_program_data_address(&program_id);
    let programdata_addr: Address = programdata_addr
        .to_string()
        .parse()
        .context("invalid programdata address")?;

    let deploy_slot = start_slot.saturating_sub(1);

    // 13 = 4 (variant) + 8 (slot) + 1 (None discriminant)
    let data_len = 13 + elf.len();
    let lamports = rpc
        .get_minimum_balance_for_rent_exemption(data_len)
        .await
        .context("failed to compute rent-exempt minimum for programdata")?;

    eprintln!(
        "[inject] programdata {} ({} bytes, {} lamports)",
        programdata_addr, data_len, lamports,
    );

    Ok(build_program_injection(programdata_addr, &elf, deploy_slot, None, lamports))
}

/// Call `getTransaction` for `signature` and extract per-account balance changes.
/// Returns `None` if the RPC call fails or the transaction has no meta.
pub async fn fetch_balance_changes(
    rpc: &RpcClient,
    signature: &str,
) -> Option<(Vec<SolAccount>, Vec<TokenAccount>)> {
    let signature: Signature = signature.parse().ok()?;
    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };

    let confirmed = rpc
        .get_transaction_with_config(&signature, config)
        .await
        .ok()?;

    let inner = confirmed.transaction;
    let meta = inner.meta.as_ref()?;

    // Build full account key list: static keys + ALT-loaded writable + readonly.
    let static_keys: Vec<String> = match &inner.transaction {
        EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            UiMessage::Raw(raw) => raw.account_keys.clone(),
            UiMessage::Parsed(parsed) => {
                parsed.account_keys.iter().map(|k| k.pubkey.clone()).collect()
            }
        },
        _ => return None,
    };

    let mut keys = static_keys;
    if let OptionSerializer::Some(addresses) = &meta.loaded_addresses {
        keys.extend(addresses.writable.iter().cloned());
        keys.extend(addresses.readonly.iter().cloned());
    }

    // SOL changes — only include accounts where the balance actually moved.
    let sol_changes: Vec<SolAccount> = keys
        .iter()
        .enumerate()
        .filter_map(|(i, pubkey)| {
            let pre = *meta.pre_balances.get(i)?;
            let post = *meta.post_balances.get(i)?;
            if pre == post {
                return None;
            }
            Some(SolAccount { pubkey: pubkey.clone(), pre_lamports: pre, post_lamports: post })
        })
        .collect();

    // Token changes — join pre/post by account_index, skip unchanged entries.
    let empty = vec![];
    let pre_tok = match &meta.pre_token_balances {
        OptionSerializer::Some(v) => v,
        _ => &empty,
    };
    let post_tok = match &meta.post_token_balances {
        OptionSerializer::Some(v) => v,
        _ => &empty,
    };

    let mut pre_map: HashMap<u8, (u64, String, String, u8)> = HashMap::new();
    for t in pre_tok {
        let amount = t.ui_token_amount.amount.parse::<u64>().unwrap_or(0);
        let owner = match &t.owner {
            OptionSerializer::Some(o) => o.clone(),
            _ => String::new(),
        };
        pre_map.insert(t.account_index, (amount, t.mint.clone(), owner, t.ui_token_amount.decimals));
    }

    let mut post_map: HashMap<u8, (u64, String, String, u8)> = HashMap::new();
    for t in post_tok {
        let amount = t.ui_token_amount.amount.parse::<u64>().unwrap_or(0);
        let owner = match &t.owner {
            OptionSerializer::Some(o) => o.clone(),
            _ => String::new(),
        };
        post_map.insert(t.account_index, (amount, t.mint.clone(), owner, t.ui_token_amount.decimals));
    }

    let mut all_indices: BTreeSet<u8> = BTreeSet::new();
    all_indices.extend(pre_map.keys().copied());
    all_indices.extend(post_map.keys().copied());

    let token_changes: Vec<TokenAccount> = all_indices
        .into_iter()
        .filter_map(|idx| {
            let pubkey = keys.get(idx as usize)?.clone();
            let (pre_amount, mint, owner, decimals) = if let Some(info) = pre_map.get(&idx) {
                (info.0, info.1.clone(), info.2.clone(), info.3)
            } else {
                let info = post_map.get(&idx)?;
                (0, info.1.clone(), info.2.clone(), info.3)
            };
            let post_amount = post_map.get(&idx).map(|i| i.0).unwrap_or(0);
            if pre_amount == post_amount {
                return None;
            }
            Some(TokenAccount { pubkey, mint, owner, pre_amount, post_amount, decimals })
        })
        .collect();

    Some((sol_changes, token_changes))
}

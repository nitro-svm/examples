use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcBlockConfig;
use solana_transaction_status::{TransactionDetails, UiTransactionEncoding};

use spl_token_2022_interface::extension::{BaseStateWithExtensions as _, ExtensionType, StateWithExtensions};
use spl_token_2022_interface::state::Mint;

use super::parse::{SOLANA_RPC, TOKEN_2022_PROGRAM};
use super::types::{BalanceDiffs, TransactionTokenBalanceSerde, TxWithMeta};

/// `SOLANA_RPC_URL` (e.g. a Helius endpoint) if set, otherwise the public mainnet RPC.
fn rpc_client() -> RpcClient {
    let url = std::env::var("SOLANA_RPC_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| SOLANA_RPC.to_string());
    RpcClient::new(url)
}

/// All transactions (with metadata) confirmed in `slot`, fetched from a public
/// Solana RPC node via `getBlock`.
pub async fn get_transactions(slot: u64) -> Result<Vec<TxWithMeta>> {
    let block = rpc_client()
        .get_block_with_config(
            slot,
            RpcBlockConfig {
                encoding: Some(UiTransactionEncoding::Base64),
                transaction_details: Some(TransactionDetails::Full),
                rewards: Some(false),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        )
        .await
        .with_context(|| format!("getBlock failed for slot {slot}"))?;

    let txs = block
        .transactions
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tx| {
            let transaction = tx.transaction.decode()?;
            let meta = tx.meta?;
            if meta.err.is_some() {
                return None;
            }
            let balance_diffs = BalanceDiffs {
                pre_balances: meta.pre_balances,
                post_balances: meta.post_balances,
                pre_token_balances: Option::from(meta.pre_token_balances)
                    .map(convert_token_balances),
                post_token_balances: Option::from(meta.post_token_balances)
                    .map(convert_token_balances),
            };
            Some(TxWithMeta {
                transaction,
                error: None,
                balance_diffs: Some(balance_diffs),
                logs: None,
                inner_instructions: None,
            })
        })
        .collect();

    Ok(txs)
}

fn convert_token_balances(
    balances: Vec<solana_transaction_status::UiTransactionTokenBalance>,
) -> Vec<TransactionTokenBalanceSerde> {
    balances
        .into_iter()
        .map(|b| TransactionTokenBalanceSerde {
            account_index: b.account_index,
            mint: b.mint,
            ui_token_amount: b.ui_token_amount,
            owner: Option::from(b.owner).unwrap_or_default(),
            program_id: Option::from(b.program_id).unwrap_or_default(),
        })
        .collect()
}

/// On-chain Unix timestamp (seconds) of `slot`, via Solana's `getBlockTime`.
pub async fn get_block_time(slot: u64) -> Result<i64> {
    rpc_client()
        .get_block_time(slot)
        .await
        .with_context(|| format!("getBlockTime failed for slot {slot}"))
}

/// The account at `pubkey`, or `None` if it doesn't exist.
pub async fn get_account_info(pubkey: &str) -> Result<Option<Account>> {
    let pubkey: Pubkey = pubkey.parse().context("parse pubkey")?;
    Ok(rpc_client()
        .get_account_with_commitment(&pubkey, CommitmentConfig::confirmed())
        .await
        .context("getAccountInfo failed")?
        .value)
}

/// What it takes to build a token account for a mint.
#[derive(Clone, Debug)]
pub struct MintInfo {
    /// The owning token program (legacy Token or Token-2022).
    pub token_program: String,
    /// Token-2022 extensions an ATA of this mint carries; empty for legacy mints.
    pub account_extensions: Vec<ExtensionType>,
}

/// [`MintInfo`] for `mint`. Cached per mint, since a mint's owner and extensions don't change
/// and callers look up the same few mints repeatedly.
pub async fn get_mint_info(mint: &str) -> Result<MintInfo> {
    static CACHE: OnceLock<Mutex<HashMap<String, MintInfo>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(info) = cache.lock().unwrap().get(mint) {
        return Ok(info.clone());
    }
    let account = get_account_info(mint)
        .await?
        .with_context(|| format!("mint {mint} not found"))?;
    let token_program = account.owner.to_string();
    let account_extensions = if token_program == TOKEN_2022_PROGRAM {
        let state = StateWithExtensions::<Mint>::unpack(&account.data)
            .map_err(|e| anyhow::anyhow!("unpack Token-2022 mint {mint}: {e:?}"))?;
        let mint_extensions = state
            .get_extension_types()
            .map_err(|e| anyhow::anyhow!("read extensions of mint {mint}: {e:?}"))?;
        #[allow(deprecated)]
        let mut extensions = ExtensionType::get_required_init_account_extensions(&mint_extensions);
        // The ATA program always initializes Token-2022 ATAs as immutable-owner.
        extensions.push(ExtensionType::ImmutableOwner);
        let mut unique = Vec::new();
        for extension in extensions {
            if !unique.contains(&extension) {
                unique.push(extension);
            }
        }
        unique
    } else {
        Vec::new()
    };
    let info = MintInfo { token_program, account_extensions };
    cache.lock().unwrap().insert(mint.to_string(), info.clone());
    Ok(info)
}

/// The SPL token program that owns `mint` (legacy Token or Token-2022).
pub async fn get_mint_token_program(mint: &str) -> Result<String> {
    Ok(get_mint_info(mint).await?.token_program)
}

use anyhow::{Context, Result};
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcBlockConfig;
use solana_transaction_status::{TransactionDetails, UiTransactionEncoding};

use super::parse::SOLANA_RPC;
use super::types::{BalanceDiffs, TransactionTokenBalanceSerde, TxWithMeta};

fn rpc_client() -> RpcClient {
    RpcClient::new(SOLANA_RPC.to_string())
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

/// The SPL token program that owns `mint` (legacy Token or Token-2022).
pub async fn get_mint_token_program(mint: &str) -> Result<String> {
    get_account_info(mint)
        .await?
        .map(|account| account.owner.to_string())
        .with_context(|| format!("mint {mint} not found"))
}

use anyhow::{Context, Result};
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use super::parse::SOLANA_RPC;

fn rpc_client() -> RpcClient {
    RpcClient::new(SOLANA_RPC.to_string())
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

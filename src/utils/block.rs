use anyhow::{Context, Result};

use super::parse::SOLANA_RPC;

/// On-chain Unix timestamp (seconds) of `slot`, via Solana's `getBlockTime`.
pub async fn get_block_time(slot: u64) -> Result<i64> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getBlockTime",
        "params": [slot]
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(SOLANA_RPC)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    resp["result"]
        .as_i64()
        .with_context(|| format!("block time not found for slot {slot}"))
}

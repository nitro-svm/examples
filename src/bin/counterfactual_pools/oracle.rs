//! Repricing a TaurusFi oracle every slot from a Binance futures price.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use simulator_api::{AccountData, EncodedBinary};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

/// TaurusFi oracle layout: 48-byte entries led by an f64 price, then the update's slot (twice) and
/// unix milliseconds.
const ENTRY_LEN: usize = 48;
const UPDATE_SLOTS: [usize; 2] = [480, 488];
const UPDATE_MILLIS: usize = 496;
/// Offset of the pool's oracle entry index.
const POOL_ENTRY: usize = 320;

const KLINES_URL: &str = "https://fapi.binance.com/fapi/v1/klines";
const MINUTE_MS: i64 = 60_000;

/// The pool's entry in its oracle.
pub fn pool_entry(pool: &AccountData) -> Result<usize> {
    let data = pool.data.decode()?;
    let bytes = data
        .get(POOL_ENTRY..POOL_ENTRY + 8)
        .context("the pool is too short to name an oracle entry")?;
    Ok(u64::from_le_bytes(bytes.try_into()?) as usize)
}

/// One oracle state per slot, priced at the last closed Binance minute.
pub async fn schedule(
    oracle: &AccountData,
    entry: usize,
    symbol: &str,
    rpc_url: &str,
    start: u64,
    end: u64,
) -> Result<Vec<(u64, AccountData)>> {
    let rpc = RpcClient::new(rpc_url.to_string());
    let (start_ms, end_ms) = (block_millis(&rpc, start).await?, block_millis(&rpc, end).await?);
    let closes = closes(symbol, start_ms - 2 * MINUTE_MS, end_ms).await?;
    let template = oracle.data.decode()?;
    ensure!(
        template.len() >= UPDATE_MILLIS + 8 && (entry + 1) * ENTRY_LEN <= UPDATE_SLOTS[0],
        "the oracle is not a TaurusFi oracle with entry {entry}"
    );

    (start..=end)
        .map(|slot| {
            // Slots are evenly spaced between the two block times.
            let millis =
                start_ms + (end_ms - start_ms) * (slot - start) as i64 / (end - start).max(1) as i64;
            let (_, price) = closes
                .range(..millis)
                .next_back()
                .with_context(|| format!("no {symbol} close before slot {slot}"))?;
            let mut data = template.clone();
            data[entry * ENTRY_LEN..entry * ENTRY_LEN + 8].copy_from_slice(&price.to_le_bytes());
            for at in UPDATE_SLOTS {
                data[at..at + 8].copy_from_slice(&slot.to_le_bytes());
            }
            data[UPDATE_MILLIS..UPDATE_MILLIS + 8].copy_from_slice(&millis.to_le_bytes());
            Ok((
                slot,
                AccountData {
                    data: EncodedBinary::from_bytes(&data, oracle.data.encoding),
                    ..oracle.clone()
                },
            ))
        })
        .collect()
}

/// A skipped slot has no block time, so take the next one that produced a block.
async fn block_millis(rpc: &RpcClient, slot: u64) -> Result<i64> {
    for candidate in slot..slot + 32 {
        if let Ok(seconds) = rpc.get_block_time(candidate).await {
            return Ok(seconds * 1000);
        }
    }
    anyhow::bail!("no block time near slot {slot}")
}

/// 1m closes keyed by close time.
async fn closes(symbol: &str, from_ms: i64, to_ms: i64) -> Result<BTreeMap<i64, f64>> {
    let mut closes = BTreeMap::new();
    let mut cursor = from_ms;
    while cursor < to_ms {
        let rows: Vec<Vec<serde_json::Value>> = reqwest::Client::new()
            .get(KLINES_URL)
            .query(&[
                ("symbol", symbol.to_string()),
                ("interval", "1m".to_string()),
                ("startTime", cursor.to_string()),
                ("endTime", to_ms.to_string()),
                ("limit", "1500".to_string()),
            ])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("fetching {symbol} klines from Binance"))?
            .json()
            .await?;
        let Some(last) = rows.last() else { break };
        cursor = last[6].as_i64().context("kline close time")? + 1;
        for row in &rows {
            let close_ms = row[6].as_i64().context("kline close time")?;
            let close = row[4]
                .as_str()
                .and_then(|close| close.parse().ok())
                .context("kline close")?;
            closes.insert(close_ms, close);
        }
    }
    ensure!(!closes.is_empty(), "Binance returned no {symbol} klines");
    Ok(closes)
}

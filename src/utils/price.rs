use anyhow::{Context, Result};

use super::parse::{SPCX_MINT, WSOL_MINT};

const BINANCE_API_URL: &str = "https://api.binance.com/api/v3/klines";

pub enum Ticker {
    SOL,
    SPCX,
}

impl Ticker {
    fn as_str(&self) -> &'static str {
        match self {
            Self::SOL => "SOLUSDC",
            Self::SPCX => "SPCXUSDC",
        }
    }

    /// The Binance ticker priced in USDC for a given base mint, if we track one.
    pub fn from_mint(mint: &str) -> Option<Self> {
        match mint {
            WSOL_MINT => Some(Self::SOL),
            SPCX_MINT => Some(Self::SPCX),
            _ => None,
        }
    }
}

/// `symbol`/USDC price (whole USDC per unit, rounded) at `unix_time`, from Binance's
/// public (no-API-key) klines endpoint.
pub async fn get_historical_binance_price_usdc(symbol: Ticker, unix_time: i64) -> Result<u64> {
    let symbol = symbol.as_str();
    let url = format!(
        "{BINANCE_API_URL}?symbol={symbol}&interval=1m&startTime={}&limit=1",
        unix_time * 1000
    );

    let resp: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .send()
        .await?
        .json()
        .await?;
    let close = resp[0][4]
        .as_str()
        .with_context(|| format!("{symbol} price not found in binance klines response"))?;

    let price: f64 = close
        .parse()
        .with_context(|| format!("parse {symbol} price"))?;
    Ok(price.round() as u64)
}

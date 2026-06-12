use serde::Deserialize;

#[derive(Deserialize)]
pub struct VaultMetricsHistory {
    pub timestamp: String,
    pub apy: String,
    #[serde(rename = "sharePrice")]
    pub share_price: String,
    pub reserves: Vec<VaultReserveMetrics>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultReserveMetrics {
    pub pubkey: String,
    pub supply_apy: String,
    pub allocation_ratio: String,
}

#[derive(Deserialize)]
pub struct ReserveHistory {
    pub reserve: String,
    pub history: Vec<HistoryEntry>,
}

#[derive(Deserialize)]
pub struct HistoryEntry {
    pub timestamp: String,
    pub metrics: ReserveMetrics,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ReserveMetrics {
    pub accumulated_protocol_fees: String,
    #[serde(rename = "assetOraclePriceUSD")]
    pub asset_oracle_price_usd: String,
    #[serde(rename = "assetPriceUSD")]
    pub asset_price_usd: String,
    pub borrow_curve: Vec<[f64; 2]>,
    pub borrow_factor: u64,
    #[serde(rename = "borrowInterestAPY")]
    pub borrow_interest_apy: f64,
    pub borrow_limit_against_collateral_in_elevation_groups: Vec<serde_json::Value>,
    pub borrow_limit_crossed_timestamp: u64,
    pub borrow_limit_outside_elevation_group: String,
    pub borrow_outside_elevation_group: String,
    #[serde(rename = "borrowTvl")]
    pub borrow_tvl: String,
    pub borrowed_against_collateral_in_elevation_groups: Vec<serde_json::Value>,
    pub decimals: u8,
    pub deposit_limit_crossed_timestamp: u64,
    #[serde(rename = "depositTvl")]
    pub deposit_tvl: String,
    pub exchange_rate: String,
    pub host_fixed_interest_rate: String,
    #[serde(rename = "isUIDeprecated", default)]
    pub is_ui_deprecated: bool,
    pub liquidation_threshold: f64,
    pub loan_to_value: f64,
    pub max_liquidation_bonus: f64,
    pub min_liquidation_bonus: f64,
    pub mint_address: String,
    pub mint_total_supply: String,
    pub protocol_take_rate: f64,
    pub reserve_borrow_limit: String,
    pub reserve_deposit_limit: String,
    pub status: String,
    #[serde(rename = "supplyInterestAPY")]
    pub supply_interest_apy: f64,
    pub symbol: String,
    pub total_borrows: String,
    pub total_liquidity: String,
    pub total_supply: String,
}

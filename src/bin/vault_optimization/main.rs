mod kamino;
use kamino::{ReserveHistory, VaultMetricsHistory};

use anyhow::Result;
use clap::Parser;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use std::collections::HashMap;
use std::io::{BufWriter, Write as _};

const SOLANA_RPC: &str = "https://api.mainnet-beta.solana.com";
const KAMINO_API: &str = "https://api.kamino.finance/kamino-market/{}/reserves/{}/metrics/history";
const SENTORA_PYUSD_VAULT: &str = "A2wsxhA7pF4B2UKVfXocb6TAAP9ipfPJam6oMKgDE5BK";
const KVAULT_API: &str = "https://api.kamino.finance/kvaults/vaults/{}/metrics/history";

#[derive(Parser)]
#[command(about = "Backtest Kamino vault PYUSD allocation strategies")]
struct Cli {
    #[arg(long, default_value = SOLANA_RPC)]
    rpc_url: String,
    #[arg(long, default_value_t = 422_818_048)]
    start_slot: u64,
    #[arg(long, default_value_t = 422_918_048)]
    end_slot: u64,
    #[arg(long)]
    output: String,
    #[arg(long, default_value = KAMINO_API)]
    kamino_url: String,
}

// Formulas
// Reserve exchange rate = total liquidity (available + borrowed − accumulated protocol fees) / cToken supply
// Vault share price = (vault's idle tokens + Σ cToken balances × exchange rates) / vault shares outstanding
// Realized yield = growth in share price over the simulated hour, annualized.

struct Portfolio {
    name: &'static str,
    main: f64,
    maple: f64,
    prime: f64,
    jlp: f64,
}

const MAPLE_MARKET: &str = "6WEGfej9B9wjxRs6t4BYpb9iCXd8CpTpJ8fVSNzHCC5y";
const MAPLE_PYUSD_RESERVE: &str = "92qeAka3ZzCGPfJriDXrE7tiNqfATVCAM6ZjjctR3TrS";

const MAIN_MARKET: &str = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF";
const MAIN_PYUSD_RESERVE: &str = "2gc9Dm1eB6UgVYFBUN9bWks6Kes9PbWSaPaa9DqyvEiN";

const PRIME_MARKET: &str = "CqAoLuqWtavaVE8deBjMKe8ZfSt9ghR6Vb8nfsyabyHA";
const PRIME_PYUSD_RESERVE: &str = "3ZUAwhEtK8XWfK4fy98z4yoptm4GeyeAu21L11HPXaZ5";

const JLP_MARKET: &str = "DxXdAyU3kCjnyggvHmY5nAwg5cRbbmdyX3npfDMjjMek";
const JLP_PYUSD_RESERVE: &str = "FswUCVjvfAuzHCgPDF95eLKscGsLHyJmD6hzkhq26CLe";

// market name -> (timestamp -> exchange_rate)
type MarketRates = HashMap<&'static str, HashMap<String, f64>>;

const MARKET_NAMES: &[&str] = &["maple", "main", "prime", "jlp"];

struct SimRow {
    timestamp: String,
    share_price: f64,
    annualized_yield: f64,
    // annualized yield per market, in MARKET_NAMES order
    market_yields: Vec<f64>,
}

// share_price is normalized to 1.0 at t=0.
// Weights are normalized to the sum of weights for markets that have data.
fn simulate_portfolio(portfolio: &Portfolio, market_rates: &MarketRates) -> Vec<SimRow> {
    let weights: &[(&str, f64)] = &[
        ("maple", portfolio.maple),
        ("main",  portfolio.main),
        ("prime", portfolio.prime),
        ("jlp",   portfolio.jlp),
    ];

    let active: Vec<(&str, f64)> = weights.iter()
        .filter(|(name, _)| market_rates.contains_key(*name))
        .copied()
        .collect();

    if active.is_empty() {
        return vec![];
    }

    let mut timestamps: Vec<String> = market_rates[active[0].0].keys().cloned().collect();
    timestamps.sort();

    if timestamps.is_empty() {
        return vec![];
    }

    let t0 = &timestamps[0];
    let total_weight: f64 = active.iter().map(|(_, w)| w).sum();

    let initial_rates: HashMap<&str, f64> = active.iter()
        .filter_map(|(name, _)| market_rates[*name].get(t0).map(|r| (*name, *r)))
        .collect();

    let mut results = vec![];
    let mut prev_price = 1.0_f64;
    let mut prev_market_rates: HashMap<&str, f64> = initial_rates.clone();

    for ts in &timestamps {
        // Vault share price: Σ (weight_i / total_weight) × (exchange_rate_i(t) / exchange_rate_i(t0))
        let share_price: f64 = active.iter()
            .filter_map(|(name, weight)| {
                let r0 = initial_rates.get(name)?;
                let rt = market_rates[*name].get(ts)?;
                Some((weight / total_weight) * (r0 / rt))
            })
            .sum();

        let annualized_yield = (share_price / prev_price - 1.0) * 8760.0;

        let market_yields: Vec<f64> = MARKET_NAMES.iter().map(|name| {
            let prev = prev_market_rates.get(name).copied().unwrap_or(f64::NAN);
            let curr = market_rates.get(name).and_then(|r| r.get(ts)).copied().unwrap_or(f64::NAN);
            (prev / curr - 1.0) * 8760.0
        }).collect();

        for name in &active {
            if let Some(&rt) = market_rates[name.0].get(ts) {
                prev_market_rates.insert(name.0, rt);
            }
        }

        results.push(SimRow { timestamp: ts.clone(), share_price, annualized_yield, market_yields });
        prev_price = share_price;
    }

    results
}

async fn get_historical_performance(
    client: &reqwest::Client,
    vault: &str,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<VaultMetricsHistory>> {
    let url = KVAULT_API.replace("{}", vault);
    let history = client
        .get(format!("{url}?env=mainnet-beta&start={start_time}&end={end_time}&frequency=hour"))
        .send()
        .await?
        .json::<Vec<VaultMetricsHistory>>()
        .await?;
    Ok(history)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let rpc = RpcClient::new_with_commitment(cli.rpc_url, CommitmentConfig::finalized());
    let start_time = rpc.get_block_time(cli.start_slot).await?;
    let end_time = rpc.get_block_time(cli.end_slot).await?;

    let kamino_client = reqwest::Client::new();
    let query_params = format!("?env=mainnet-beta&start={start_time}&end={end_time}&frequency=hour");

    let markets: &[(&'static str, &str, &str)] = &[
        ("maple", MAPLE_MARKET, MAPLE_PYUSD_RESERVE),
        ("main",  MAIN_MARKET,  MAIN_PYUSD_RESERVE),
        ("prime", PRIME_MARKET, PRIME_PYUSD_RESERVE),
        ("jlp",   JLP_MARKET,   JLP_PYUSD_RESERVE),
    ];

    let mut market_rates: MarketRates = HashMap::new();
    for (name, market, reserve) in markets {
        let url = cli.kamino_url.replacen("{}", market, 1).replacen("{}", reserve, 1);
        let history: ReserveHistory = kamino_client
            .get(format!("{}{}", url, query_params))
            .send()
            .await?
            .json()
            .await?;

        eprintln!("[{}] {} reserve={} {} snapshots", name, history.reserve, reserve, history.history.len());

        let rates: HashMap<String, f64> = history.history.into_iter()
            .filter_map(|e| e.metrics.exchange_rate.parse::<f64>().ok().map(|r| (e.timestamp, r)))
            .collect();

        market_rates.insert(name, rates);
    }

    let portfolios = vec![
        Portfolio { name: "portfolio_a", main: 0.45, maple: 0.35, prime: 0.25, jlp: 0.005 },
        Portfolio { name: "portfolio_b", main: 0.50, maple: 0.40, prime: 0.15, jlp: 0.005 },
    ];

    let f = std::fs::File::create(&cli.output)?;
    let mut w = BufWriter::new(f);
    let vault_history = get_historical_performance(&kamino_client, SENTORA_PYUSD_VAULT, start_time, end_time).await?;
    eprintln!("[vault] sentora_pyusd {} snapshots", vault_history.len());

    let market_headers = MARKET_NAMES.iter().map(|n| format!("{n}_yield_pct")).collect::<Vec<_>>().join(",");
    writeln!(w, "portfolio,timestamp,share_price,total_yield_pct,{market_headers}")?;

    // Baseline: actual vault performance
    let mut prev_vault_price = vault_history.first().and_then(|r| r.share_price.parse::<f64>().ok()).unwrap_or(1.0);
    for row in &vault_history {
        let price: f64 = row.share_price.parse().unwrap_or(f64::NAN);
        let ann_yield = (price / prev_vault_price - 1.0) * 8760.0 * 100.0;
        writeln!(w, "sentora_baseline,{},{:.8},{:.6},{}", row.timestamp, price, ann_yield, ",".repeat(MARKET_NAMES.len() - 1))?;
        prev_vault_price = price;
    }

    for portfolio in &portfolios {
        for row in simulate_portfolio(portfolio, &market_rates) {
            let market_cols = row.market_yields.iter().map(|y| format!("{:.6}", y * 100.0)).collect::<Vec<_>>().join(",");
            writeln!(w, "{},{},{:.8},{:.6},{market_cols}", portfolio.name, row.timestamp, row.share_price, row.annualized_yield * 100.0)?;
        }
    }

    eprintln!("wrote results to {}", cli.output);
    Ok(())
}

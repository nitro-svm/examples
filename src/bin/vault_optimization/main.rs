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
// Reserve exchange rate = kToken supply / total liquidity (decreases as interest accrues)
// 1 kToken is worth (1/exchange_rate) underlying; ratio r0/rt > 1 gives cumulative yield.
// Vault share price compounds each period's weighted return: Π (Σ w_i/W * r_{t-1,i}/r_{t,i})
// This keeps total_yield_pct == weighted_avg(market_yield_pct) every row.

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

// Maps vault reserve pubkeys to MARKET_NAMES entries for baseline per-market APY columns.
const RESERVE_TO_MARKET: &[(&str, &str)] = &[
    (MAPLE_PYUSD_RESERVE, "maple"),
    (MAIN_PYUSD_RESERVE,  "main"),
    (PRIME_PYUSD_RESERVE, "prime"),
    (JLP_PYUSD_RESERVE,   "jlp"),
];

struct SimRow {
    timestamp: String,
    share_price: f64,
    annualized_yield: f64,
    // annualized yield per market, in MARKET_NAMES order
    market_yields: Vec<f64>,
}

// share_price compounds period-by-period: share_price *= Σ (w_i/W) * (r_{t-1,i} / r_{t,i})
// This makes total_yield_pct == weighted_avg(market_yield_pct) exactly.
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

    let mut share_price = 1.0_f64;
    // Initialize prev rates to t=0 so first row yields 0%.
    let mut prev_market_rates: HashMap<&str, f64> = active.iter()
        .filter_map(|(name, _)| market_rates[*name].get(t0).map(|r| (*name, *r)))
        .collect();

    let mut results = vec![];

    for ts in &timestamps {
        // period_return = Σ (w_i/W) * (r_{t-1,i} / r_{t,i})
        let period_return: f64 = active.iter()
            .filter_map(|(name, weight)| {
                let prev = prev_market_rates.get(name)?;
                let curr = market_rates[*name].get(ts)?;
                Some((weight / total_weight) * (prev / curr))
            })
            .sum();

        share_price *= period_return;
        let annualized_yield = (period_return - 1.0) * 8760.0;

        let market_yields: Vec<f64> = MARKET_NAMES.iter().map(|name| {
            let prev = prev_market_rates.get(name).copied().unwrap_or(f64::NAN);
            let curr = market_rates.get(name).and_then(|r| r.get(ts)).copied().unwrap_or(f64::NAN);
            (prev / curr - 1.0) * 8760.0
        }).collect();

        for (name, _) in &active {
            if let Some(&rt) = market_rates[*name].get(ts) {
                prev_market_rates.insert(name, rt);
            }
        }

        results.push(SimRow { timestamp: ts.clone(), share_price, annualized_yield, market_yields });
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

fn csv_writer(path: &str) -> Result<BufWriter<std::fs::File>> {
    let f = std::fs::File::create(path)?;
    Ok(BufWriter::new(f))
}

fn output_prefix(output: &str) -> String {
    output.strip_suffix(".csv").unwrap_or(output).to_string()
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

    let vault_history = get_historical_performance(&kamino_client, SENTORA_PYUSD_VAULT, start_time, end_time).await?;
    eprintln!("[vault] sentora_pyusd {} snapshots", vault_history.len());

    let prefix = output_prefix(&cli.output);
    let header = {
        let market_headers = MARKET_NAMES.iter().map(|n| format!("{n}_yield_pct")).collect::<Vec<_>>().join(",");
        format!("timestamp,share_price,total_yield_pct,{market_headers}")
    };

    // Baseline: actual vault share price + per-reserve supply APYs from vault response.
    {
        let path = format!("{prefix}_baseline.csv");
        let mut w = csv_writer(&path)?;
        writeln!(w, "{header}")?;

        for row in &vault_history {
            let price: f64 = row.share_price.parse().unwrap_or(f64::NAN);
            let ann_yield: f64 = row.apy.parse().unwrap_or(f64::NAN) * 100.0;

            // Map vault reserve pubkeys to per-market supply APY columns.
            let reserve_apys: HashMap<&str, f64> = row.reserves.iter()
                .filter_map(|r| {
                    let market = RESERVE_TO_MARKET.iter()
                        .find(|(pk, _)| *pk == r.pubkey)
                        .map(|(_, name)| *name)?;
                    let apy: f64 = r.supply_apy.parse().ok()?;
                    Some((market, apy * 100.0))
                })
                .collect();

            let market_cols = MARKET_NAMES.iter()
                .map(|name| reserve_apys.get(name).map_or(String::new(), |v| format!("{v:.6}")))
                .collect::<Vec<_>>()
                .join(",");

            writeln!(w, "{},{:.8},{:.6},{market_cols}", row.timestamp, price, ann_yield)?;
        }

        eprintln!("wrote {path}");
    }

    // Portfolio simulations, one file each.
    for portfolio in &portfolios {
        let path = format!("{prefix}_{}.csv", portfolio.name);
        let mut w = csv_writer(&path)?;
        writeln!(w, "{header}")?;

        for row in simulate_portfolio(portfolio, &market_rates) {
            let market_cols = row.market_yields.iter()
                .map(|y| format!("{:.6}", y * 100.0))
                .collect::<Vec<_>>()
                .join(",");
            writeln!(w, "{},{:.8},{:.6},{market_cols}", row.timestamp, row.share_price, row.annualized_yield * 100.0)?;
        }

        eprintln!("wrote {path}");
    }

    Ok(())
}

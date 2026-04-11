//! Benchmark getAccount (sequential) vs getAccount (concurrent) vs getMultipleAccounts.
//!
//! Fetches pool accounts for the listed DEX programs from Helius, then runs
//! each benchmark variant against a backtest session.
//!
//! Usage:
//!   SIMULATOR_API_KEY=... HELIUS_API_KEY=... cargo run --bin bench-rpc

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use futures::future::join_all;
use simulator_client::{BacktestClient, CreateSession};
use solana_account_decoder::UiDataSliceConfig;
use solana_address::Address;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};

#[derive(Parser)]
#[command(about = "Benchmark getAccount vs getMultipleAccounts on a backtest session")]
struct Cli {
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,

    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    #[arg(long, default_value_t = 411_651_021)]
    start_slot: u64,

    #[arg(long, default_value_t = 411_651_021)]
    end_slot: u64,

    /// Helius API key for fetching program accounts.
    #[arg(long, env = "HELIUS_API_KEY")]
    helius_api_key: String,

    /// Max accounts to collect per DEX program (avoids fetching millions from large programs).
    #[arg(long, default_value_t = 25)]
    max_per_program: usize,
}

const DEX_PROGRAMS: [&str; 5] = [
    // "ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA", // AlphaQ
    // "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi", // BisonFi
    // "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j",  // GoonFi
    // "goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE",  // GoonFi V2
    // "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp", // Humidifi
    "obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y",  // Obric V2
    "SoLFiHG9TfgtdUXUjWAxi3LtvYuFyDLVhBWxdMZxyCe",  // SolFi
    "SV2EYYJyRz2YhfXwXnhNAevDEui5Q6yrfyo13WtupPF",  // SolFi V2
    "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY",  // ZeroFi
    "REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2",  // Byreal
    // "CarrotwivhMpDnm27EHmRLeQ683Z1PufuqEmBZvD282s",  // Carrot
    // "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB", // Meteora
    // "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG",  // Meteora DAMM v2
    // "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",  // Meteora DLMM
    // "DjVE6JNiYqPL2QXyCUUh8rNjHrbz9hXHNYt99MQ59qw1", // Orca V1
    // "9W959DqEETiGZocYWCQPaJ6sBmUzgfxXfqGeTEdp3aQP", // Orca V2
    // "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium
    // "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",  // Raydium CLMM
    // "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C",  // Raydium CP
    // "swapNyd8XiQwJ6ianp9snpu4brUqFxadzvHebnAXjJZ",   // Stabble Stable Swap
    // "swapFpHZwjELNnjvThjajtiVmkz3yPQEHjLtka2fwHW",   // Stabble Weighted Swap
    // "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",   // Whirlpool
];

async fn fetch_program_accounts(
    helius: &RpcClient,
    program: &str,
    max: usize,
) -> Result<Vec<Address>> {
    let program_addr: Address = program.parse().context("invalid program address")?;

    // Use dataSlice length=0 so we only get pubkeys, not account data.
    let config = RpcProgramAccountsConfig {
        account_config: RpcAccountInfoConfig {
            data_slice: Some(UiDataSliceConfig { offset: 0, length: 0 }),
            commitment: Some(CommitmentConfig::confirmed()),
            ..Default::default()
        },
        ..Default::default()
    };

    let accounts = helius
        .get_program_ui_accounts_with_config(&program_addr, config)
        .await
        .with_context(|| format!("getProgramAccounts failed for {program}"))?;

    Ok(accounts.into_iter().take(max).map(|(pk, _)| pk).collect())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // ── Fetch pubkeys from Helius ─────────────────────────────────────────────
    let helius_url = format!(
        "https://mainnet.helius-rpc.com/?api-key={}",
        cli.helius_api_key
    );
    let helius = RpcClient::new(helius_url);

    eprintln!("[helius] fetching pool accounts for {} DEX programs (max {} each)...", DEX_PROGRAMS.len(), cli.max_per_program);
    let mut pubkeys: Vec<Address> = Vec::new();
    for program in &DEX_PROGRAMS {
        match fetch_program_accounts(&helius, program, cli.max_per_program).await {
            Ok(addrs) => {
                eprintln!("  {program}: {} accounts", addrs.len());
                pubkeys.extend(addrs);
            }
            Err(e) => {
                eprintln!("  {program}: FAILED ({e})");
            }
        }
    }
    pubkeys.dedup();
    eprintln!("[helius] total accounts: {}", pubkeys.len());

    if pubkeys.is_empty() {
        anyhow::bail!("no accounts fetched — check your Helius API key");
    }

    // ── Connect and create backtest session ───────────────────────────────────
    let client = BacktestClient::builder()
        .url(format!("wss://{}/backtest", &cli.url))
        .api_key(cli.api_key.clone())
        .build();

    eprintln!("[ws] connecting...");
    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(300u16)
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));
    session.ensure_ready(Some(Duration::from_secs(120))).await?;
    eprintln!("[ws] ready");

    let rpc = session.rpc();

    // ── 1. Concurrent getAccount ──────────────────────────────────────────────
    eprintln!("\n[1/2] concurrent getAccount ({} calls)...", pubkeys.len());
    let t = Instant::now();
    let futs = pubkeys
        .iter()
        .map(|pk| rpc.get_account_with_commitment(pk, CommitmentConfig::confirmed()));
    let results = join_all(futs).await;
    for (pk, r) in pubkeys.iter().zip(results) {
        match r {
            Ok(_) => {}
            Err(e) if e.to_string().contains("AccountNotFound") => {
                eprintln!("  [not found] {pk}");
            }
            Err(e) => return Err(e).with_context(|| format!("getAccount failed for {pk}")),
        }
    }
    let concurrent_ms = t.elapsed().as_millis();
    println!("[concurrent]  total={concurrent_ms}ms  (wall clock, all in flight)");

    // ── 2. getMultipleAccounts ────────────────────────────────────────────────
    eprintln!("\n[2/2] getMultipleAccounts ({} accounts in one call)...", pubkeys.len());
    let t = Instant::now();
    rpc.get_multiple_accounts_with_commitment(&pubkeys, CommitmentConfig::confirmed())
        .await
        .context("getMultipleAccounts failed")?;
    let batch_ms = t.elapsed().as_millis();
    println!("[batch]       total={batch_ms}ms");

    println!("\n=== Summary ===");
    println!("  concurrent getAccount:  {concurrent_ms}ms");
    println!("  getMultipleAccounts:    {batch_ms}ms");

    session.close(Some(Duration::from_secs(10))).await?;
    Ok(())
}

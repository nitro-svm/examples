//! Emit the direct-fill market spec for an Orca Whirlpool, derived from the pool account.
//!
//! ```sh
//! cargo run --bin orca_market_spec -- --pool <pool> --start-slot <slot> > orca-sol-usdc.json
//! ```
//!
//! With `--start-slot` the pool is read through a session positioned at that slot, so the
//! tick-array window sits where the price was during the range being replayed. Read over plain RPC
//! instead and the window follows the pool *today*, which for a historical range puts all three
//! arrays nowhere near the swap and reverts every probe.

use std::time::Duration;

use anyhow::{Context, Result};
use backtest_example::utils::{
    connection::ConnectionArgs,
    whirlpool::{Whirlpool, direct_fill_params},
};
use clap::Parser;
use simulator_client::{BacktestClient, CreateSession};

#[derive(Parser)]
#[command(about = "Derive an Orca Whirlpool direct-fill market spec")]
struct Args {
    #[command(flatten)]
    connection: ConnectionArgs,

    /// The Whirlpool to price against. Prefer the deepest pool for the pair: a thin one reverts or
    /// fills far below the flow's real price, which reads as the venue losing when it is the pool.
    #[arg(long)]
    pool: String,

    /// First slot of the range the spec is for. The pool is read as of this slot, so the
    /// tick-array window sits where the price was rather than where it is now.
    #[arg(long)]
    start_slot: u64,

    #[arg(long, default_value_t = 50)]
    slippage_bps: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let pool = pool_at_slot(&args, args.start_slot).await?;
    let spec = direct_fill_params(&pool, pool.tick_current_index, args.slippage_bps)?;

    eprintln!(
        "pool {} tick_spacing={} tick={} ticks_per_array={} window_start={}",
        pool.address,
        pool.tick_spacing,
        pool.tick_current_index,
        pool.ticks_per_array(),
        pool.array_start(pool.tick_current_index),
    );
    println!("{}", serde_json::to_string_pretty(&spec)?);
    Ok(())
}

/// The pool as the replayed range saw it.
///
/// A one-slot session is enough. The read has to happen while the session is up: its RPC endpoint
/// stops serving the moment the session completes.
async fn pool_at_slot(args: &Args, slot: u64) -> Result<Whirlpool> {
    let url = if args.connection.url.contains("://") {
        format!("{}/backtest", args.connection.url.trim_end_matches('/'))
    } else {
        format!("wss://{}/backtest", args.connection.url)
    };
    eprintln!("[ws] reading {} at slot {slot} via {url}", args.pool);
    let client = BacktestClient::builder()
        .url(url)
        .api_key(args.connection.api_key.clone())
        .build();
    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(slot)
                .end_slot(slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .build(),
        )
        .await?;
    session.ensure_ready(Some(Duration::from_secs(600))).await?;

    let address = args.pool.parse()?;
    let account = session
        .rpc()
        .get_account(&address)
        .await
        .with_context(|| format!("no account at {} in the replayed range", args.pool));
    // Closed whatever the read did: a session left open holds capacity until it times out.
    let _ = session.close(Some(Duration::from_secs(10))).await;
    let account = account?;
    Whirlpool::decode(&address, &account.owner, &account.data)
}

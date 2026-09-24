//! Price a range's SOL/USDC flow against one Orca pool, end to end.
//!
//! ```sh
//! cargo run --bin orca_direct_fill -- --url ws://<host> \
//!   --pool Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE \
//!   --start-slot 438196108 --end-slot 438206107
//! ```
//!
//! Four steps, in this order for a reason:
//!
//! 1. Open a one-slot session at the range and read the pool through it. The tick-array window has
//!    to sit where the price *was*; mainnet's tick today can be many arrays away.
//! 2. Close it, so it stops holding capacity while the real run is set up.
//! 3. Open the run with that market as its direct-fill book. The session rebuilds every matching
//!    hop through the pool and prices it against what the hop actually filled.
//! 4. Report the census the session returns.
//!
//! Step 1 is what a harvested account run cannot give: a route names only the arrays its own swap
//! needed, which fills one way and reverts the other.

use anyhow::{Context, Result, bail};
use backtest_example::utils::{
    connection::ConnectionArgs,
    session::{drive_to_completion, wait_for_first_pause},
    whirlpool::{Whirlpool, direct_fill_params},
};
use clap::Parser;
use simulator_api::{RerouteAggregators, RerouteStatsReport, SwapAggregator};
use simulator_client::{CreateSession, ManagedBacktestSession, ManagedEvent, backtest_ws_url};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

/// Slots per `Continue` while driving the range.
const SLOT_STRIDE: u64 = 100;
/// The outcome the session records for a probe that traded, spelled as it serialises it.
const FILLED: &str = "filled";

/// The API admits Jupiter alone by default, and a swap whose router is outside the admitted set is
/// dropped before the direct-fill census sees it — so the pool would be priced against a fraction
/// of the pair's flow rather than all of it.
fn all_aggregators() -> RerouteAggregators {
    RerouteAggregators::new([
        SwapAggregator::Jupiter,
        SwapAggregator::Okx,
        SwapAggregator::Titan,
        SwapAggregator::Dflow,
    ])
}

#[derive(Parser)]
#[command(about = "Price a range's flow against one Orca Whirlpool")]
struct Args {
    #[command(flatten)]
    connection: ConnectionArgs,

    /// The Whirlpool to price against. Use the deepest pool for the pair — a thin one reverts most
    /// probes and fills the rest far below the flow's real price, which reads as the venue losing
    /// when it is the pool being empty.
    #[arg(long)]
    pool: String,

    #[arg(long)]
    start_slot: u64,
    #[arg(long)]
    end_slot: u64,

    #[arg(long, default_value_t = 50)]
    slippage_bps: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.end_slot < args.start_slot {
        bail!("end_slot precedes start_slot");
    }
    let ws_url = backtest_ws_url(&args.connection.url);

    let pool = read_pool(&ws_url, &args).await?;
    eprintln!(
        "[pool] {} tick_spacing={} tick={} window_start={}",
        pool.address,
        pool.tick_spacing,
        pool.tick_current_index,
        pool.array_start(pool.tick_current_index),
    );
    let market = direct_fill_params(&pool, pool.tick_current_index, args.slippage_bps)?;

    // Re-quoting off: no sidecar starts, so the probes are the only thing done with the flow.
    let mut session = ManagedBacktestSession::start(
        ws_url,
        args.connection.api_key.clone(),
        CreateSession::builder()
            .start_slot(args.start_slot)
            .end_slot(args.end_slot)
            .reroute_order_flow(true)
            .reroute_requote(false)
            .reroute_aggregators(all_aggregators())
            .reroute_direct_fill(Box::new(market))
            .send_summary(true)
            .disconnect_timeout_secs(900u16)
            .capacity_wait_timeout_secs(900u16)
            .build()
            .into_request()
            .context("building the direct-fill session request")?,
    )
    .await?;
    eprintln!(
        "[run] {}..={} ({} slots)",
        args.start_slot,
        args.end_slot,
        args.end_slot - args.start_slot + 1
    );

    let stats = drive_to_completion(&mut session, SLOT_STRIDE, |event| {
        if let ManagedEvent::Slot(slot) = event
            && slot.is_multiple_of(1_000)
        {
            eprintln!("[run] slot {slot}");
        }
    })
    .await?;
    session.shutdown().await;

    report(&stats.context("the session reported no reroute census")?);
    Ok(())
}

/// The pool as of `start_slot`, read through a session that is shut down before returning.
///
/// A session's RPC stops serving the moment the session completes, and the account is only the
/// range's once the session is positioned there — so the read belongs at the first pause and
/// nowhere else.
async fn read_pool(ws_url: &str, args: &Args) -> Result<Whirlpool> {
    let mut probe = ManagedBacktestSession::start(
        ws_url.to_string(),
        args.connection.api_key.clone(),
        CreateSession::builder()
            .start_slot(args.start_slot)
            .end_slot(args.start_slot)
            .disconnect_timeout_secs(900u16)
            .capacity_wait_timeout_secs(900u16)
            .build()
            .into_request()
            .context("building the pool-read session request")?,
    )
    .await?;
    let rpc_url = probe.session_info().rpc_endpoint.clone();
    wait_for_first_pause(&mut probe).await?;

    let address = args.pool.parse()?;
    let account = RpcClient::new(rpc_url)
        .get_account(&address)
        .await
        .with_context(|| format!("no account at {} at slot {}", args.pool, args.start_slot));
    // Shut down whatever the read did: a session left open holds capacity until it times out.
    probe.shutdown().await;
    let account = account?;
    Whirlpool::decode(&address, &account.owner, &account.data)
}

/// What the venue would have done with the flow, from the census the session returns.
fn report(stats: &RerouteStatsReport) {
    let filled = stats
        .direct_fill_outcomes
        .get(FILLED)
        .copied()
        .unwrap_or_default();
    let probes: u64 = stats.direct_fill_outcomes.values().sum();

    println!("\n=== SOL/USDC direct fill vs Orca ===");
    println!("hops matched : {}", stats.direct_fill_matched);
    println!("probes built : {}", stats.direct_fill_built);
    if !stats.direct_fill_rejections.is_empty() {
        println!("not built    : {:?}", stats.direct_fill_rejections);
    }
    println!("\noutcomes:");
    for (outcome, count) in &stats.direct_fill_outcomes {
        let share = *count as f64 / probes.max(1) as f64 * 100.0;
        println!("  {outcome:<28} {count:>7}  ({share:>5.1}%)");
    }

    if stats.direct_fill_scored == 0 {
        println!("\nnothing scored: no probe filled against a hop that recorded an output");
        return;
    }
    let mean = stats.direct_fill_bps_total as f64 / stats.direct_fill_scored as f64;
    println!(
        "\nscored {} of {filled} fills, mean {mean:+.1} bps vs the venue that traded",
        stats.direct_fill_scored,
    );
    // The sign is the whole answer: positive means this pool would have paid more.
    if mean >= 0.0 {
        println!("Orca would have beaten the winning venue on average over this range.");
    } else {
        println!(
            "Orca would have paid {:.1} bps less than the winning venue on average.",
            -mean
        );
    }
}

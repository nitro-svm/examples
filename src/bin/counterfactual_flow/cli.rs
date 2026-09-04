//! The clap argument surface, split out so `main` reads as the counterfactual and not its plumbing.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use simulator_api::{MintPair, RerouteAggregators, RerouteFilter};
use solana_address::Address;

#[derive(Parser)]
#[command(about = "Apply a parameter change to a venue and measure its effect on taker flow")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Record the account's state per slot to JSONL.
    Capture(CaptureArgs),
    /// Run one session, optionally with the parameter change applied.
    Run(RunArgs),
    /// Compare the parameter change against the control: the same range with no override.
    Compare(CompareArgs),
    /// Report what flowed through a venue or pool in a recorded run, on L1 and after the
    /// re-quote. Reads the file only, so it re-runs per pool for free.
    Report(ReportArgs),
}

#[derive(Args)]
pub(crate) struct ReportArgs {
    /// The recording written by `run` or `compare`.
    pub(crate) recording: PathBuf,

    /// The venue under test, resolved to its route label. Defaults to the venue the run named,
    /// so a report of that run needs no selector at all.
    #[arg(long)]
    pub(crate) program_id: Option<Address>,

    /// The route label, when you would rather give it than have `--program-id` resolve it.
    /// Enough on its own: both columns fall back to it when no program is given.
    #[arg(long)]
    pub(crate) label: Option<String>,

    /// Emit the report as JSON instead of a table.
    #[arg(long, default_value_t = false)]
    pub(crate) json: bool,
}

/// Shared with the other examples: one `--url` has to yield both the websocket and the
/// session's RPC endpoint, and the local-stack `ws://` case is easy to get subtly wrong.
pub(crate) use backtest_example::utils::connection::ConnectionArgs;

#[derive(Args, Clone)]
pub(crate) struct RangeArgs {
    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 433838452)]
    pub(crate) start_slot: u64,

    /// Slots to cover, as the inclusive range `[start, start + count]`.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) slot_count: u64,

    /// Execute transactions instead of replaying recorded account state (replay
    /// requires a recorded `.adlt` for the range).
    #[arg(long, default_value_t = false)]
    pub(crate) no_replay: bool,
}

#[derive(Args)]
pub(crate) struct CaptureArgs {
    #[command(flatten)]
    pub(crate) conn: ConnectionArgs,

    #[command(flatten)]
    pub(crate) range: RangeArgs,

    /// The account whose states to record: the venue's oracle, liquidity curve,
    /// fee account — whatever its quoting reads.
    #[arg(long)]
    pub(crate) account: Address,

    /// Output JSONL path.
    #[arg(long, default_value = "capture.jsonl")]
    pub(crate) out: PathBuf,
}

#[derive(Args, Clone)]
pub(crate) struct RunArgs {
    #[command(flatten)]
    pub(crate) conn: ConnectionArgs,

    #[command(flatten)]
    pub(crate) range: RangeArgs,

    /// The account the parameter change rewrites.
    #[arg(long)]
    pub(crate) account: Address,

    /// Capture JSONL produced by `capture`; required with `--price-shift-bps`.
    #[arg(long)]
    pub(crate) capture: Option<PathBuf>,

    /// The venue under test. Its Jupiter/Metis route label is resolved automatically
    /// via `program-id-to-label` and used to match rerouted legs, since
    /// `route_plan`/`route_summary` carry pool addresses and display labels, never
    /// program ids.
    #[arg(long)]
    pub(crate) program_id: Option<Address>,

    /// Restrict rerouting to swaps trading this pair, as `<base>,<quote>` — the book you
    /// quote, matched in both directions (repeatable).
    #[arg(long, value_parser = parse_pair)]
    pub(crate) filter_pair: Vec<MintPair>,

    /// Leave swaps whose L1 transaction failed out of the run entirely. They are tracked as
    /// their own population rather than mixed into the funnel, but excluding them keeps the
    /// rows to flow that actually filled.
    #[arg(long, default_value_t = false)]
    pub(crate) skip_l1_failures: bool,

    /// Also re-quote arbitrage cycles (same input and output mint), stitched leg by leg.
    #[arg(long, default_value_t = false)]
    pub(crate) circular_arbs: bool,

    /// Aggregators whose swaps to re-quote, comma-separated (server default: jupiter alone).
    /// Spelled `--reroute-venues` on the command line, the name it shipped under.
    #[arg(long = "reroute-venues")]
    pub(crate) reroute_aggregators: Option<RerouteAggregators>,

    /// Byte offset of a little-endian fixed-point price. Repeat for every field the venue stores
    /// the price in: moving only some leaves the pool inconsistent and its quotes rejected.
    #[arg(long)]
    pub(crate) price_field: Vec<usize>,

    /// Move `--price-field` by this many basis points; negative quotes lower. Relative, so the
    /// fixed-point scale needn't be known. Read the two trade directions apart: a mid shift is
    /// worse for one and better for the other.
    #[arg(long, allow_negative_numbers = true)]
    pub(crate) price_shift_bps: Option<f64>,

    /// Keep the simulation logs and the routed transaction in the recording. Off by default,
    /// since no report reads them and they dominate the file size. Turn it on when a specific
    /// fill will need explaining afterwards.
    #[arg(long, default_value_t = false)]
    pub(crate) record_full: bool,

    /// Reroute notifications JSONL output.
    #[arg(long, default_value = "reroute-out.jsonl")]
    pub(crate) out: PathBuf,
}

#[derive(Args)]
pub(crate) struct CompareArgs {
    #[command(flatten)]
    pub(crate) run: RunArgs,

    /// Per-leg delta report JSONL path. Defaults beside the two arm recordings, as
    /// `<--out stem>-report.<ext>`, so a comparison's three files stay together.
    #[arg(long)]
    pub(crate) report: Option<PathBuf>,
}

fn parse_pair(value: &str) -> Result<MintPair, String> {
    let (base, quote) = value
        .split_once(',')
        .ok_or("expected two base58 mints separated by a comma")?;
    let parse = |mint: &str| mint.trim().parse::<Address>().map_err(|e| e.to_string());
    Ok(MintPair::new(parse(base)?, parse(quote)?))
}

pub(crate) fn filter_from(args: &RunArgs) -> Option<RerouteFilter> {
    (!args.filter_pair.is_empty()).then(|| RerouteFilter {
        pairs: args.filter_pair.iter().copied().collect(),
    })
}

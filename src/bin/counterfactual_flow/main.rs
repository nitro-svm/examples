//! Apply a parameter change to a venue and measure its effect on taker flow.
//!
//! The change is an account override — the venue's oracle, liquidity curve, or fee
//! account, rewritten per slot — visible only to the router's counterfactual universe:
//! Metis re-quotes every historical swap against it and the routed transactions are
//! simulated, never committed, so the replayed chain stays untouched and other makers'
//! quotes and taker flow are held constant.
//!
//! `capture` records the account's real per-slot states. `run` executes one session with
//! those states time-shifted — `--lag K` posts slot `s-K`'s state at slot `s`, the venue
//! updating K slots slower, which prices the value of faster updates. `compare` runs an
//! unmodified baseline against the modified world and reports per-swap quote deltas plus
//! how many legs the venue captured in each.
//!
//! Sessions default to account-state replay, which requires a recorded `.adlt` for the
//! range; pass `--no-replay` to execute transactions instead.

mod venue;

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use simulator_api::{
    AccountData, AccountModifications, MintPair, RerouteFilter, RerouteStatsReport,
};
use simulator_client::{
    AccountDiffNotification, Continue, CreateSession, ManagedBacktestSession, ManagedEvent,
    RerouteLegNotification, RerouteNotification, account_data_from_ui, subscribe_account_diffs,
    subscribe_reroutes,
};
use solana_address::Address;

use crate::venue::{WHOLE_LEG, original_venue_share, resolve_venue_label, venue_share};

#[derive(Parser)]
#[command(about = "Apply a parameter change to a venue and measure its effect on taker flow")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    #[command(flatten)]
    conn: ConnectionArgs,
}

#[derive(Args, Clone)]
struct ConnectionArgs {
    /// Simulator URL: a bare host for the hosted deployment, or an explicit
    /// `ws://host:port` for a locally run stack.
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,

    /// API key sent as the `X-API-Key` header.
    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,
}

impl ConnectionArgs {
    /// A bare host is the hosted deployment over TLS; an explicit `ws(s)://` is used as
    /// given, so a local stack (plaintext) is reachable as `ws://localhost:8900`.
    fn websocket_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        let base = match base.split_once("://") {
            Some(("ws" | "wss", _)) => base.to_string(),
            _ => format!("wss://{base}"),
        };
        if base.ends_with("/backtest") {
            base
        } else {
            format!("{base}/backtest")
        }
    }

    /// Deployments report the session's RPC endpoint either absolute or as a path. A
    /// plaintext websocket implies a plaintext RPC endpoint on the same host.
    fn rpc_url(&self, endpoint: &str) -> String {
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            return endpoint.to_string();
        }
        let (scheme, host) = match self.url.trim_end_matches('/').split_once("://") {
            Some(("ws", host)) => ("http", host),
            Some((_, host)) => ("https", host),
            None => ("https", self.url.trim_end_matches('/')),
        };
        let host = host.trim_end_matches("/backtest");
        format!("{scheme}://{host}/{}", endpoint.trim_start_matches('/'))
    }
}

#[derive(Subcommand)]
enum Command {
    /// Record the account's state per slot to JSONL.
    Capture(CaptureArgs),
    /// Run one session, optionally with the parameter change applied.
    Run(RunArgs),
    /// Compare the parameter change against the null control: the same capture, same carrier,
    /// posted unmodified at each state's own slot.
    Compare(CompareArgs),
}

#[derive(Args, Clone)]
struct RangeArgs {
    /// First slot (inclusive) to replay.
    #[arg(long, default_value_t = 433838452)]
    start_slot: u64,

    /// Slots to cover, as the inclusive range `[start, start + count]`.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..))]
    slot_count: u64,

    /// Execute transactions instead of replaying recorded account state (replay
    /// requires a recorded `.adlt` for the range).
    #[arg(long, default_value_t = false)]
    no_replay: bool,
}

#[derive(Args)]
struct CaptureArgs {
    #[command(flatten)]
    range: RangeArgs,

    /// The account whose states to record: the venue's oracle, liquidity curve,
    /// fee account — whatever its quoting reads.
    #[arg(long)]
    account: Address,

    /// Output JSONL path.
    #[arg(long, default_value = "capture.jsonl")]
    out: PathBuf,
}

#[derive(Args, Clone)]
struct RunArgs {
    #[command(flatten)]
    range: RangeArgs,

    /// The account the parameter change rewrites.
    #[arg(long)]
    account: Address,

    /// Capture JSONL produced by `capture`; required with `--lag`/`--lead`/`--price-shift-bps`.
    #[arg(long)]
    capture: Option<PathBuf>,

    /// Post slot `s-K`'s captured state at slot `s`: the venue updating K slots
    /// slower. Omit for the unmodified baseline.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..), conflicts_with = "lead")]
    lag: Option<u64>,

    /// Post slot `s+K`'s captured state at slot `s`. Invalid for venues that stamp
    /// their own last-update slot into the account — a future slot drops them from
    /// routing entirely. Prefer `--lag`; see the README.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    lead: Option<u64>,

    /// The venue under test. Its Jupiter/Metis route label is resolved automatically
    /// via `program-id-to-label` and used to match rerouted legs, since
    /// `route_plan`/`route_summary` carry pool addresses and display labels, never
    /// program ids.
    #[arg(long)]
    program_id: Option<Address>,

    /// Restrict rerouting to swaps trading this pair, as `<base>,<quote>` — the book you
    /// quote, matched in both directions (repeatable).
    #[arg(long, value_parser = parse_pair)]
    filter_pair: Vec<MintPair>,

    /// Reroute notifications JSONL output.
    #[arg(long, default_value = "reroute-out.jsonl")]
    out: PathBuf,
}

impl RunArgs {
    /// Signed slot offset the schedule posts from: `--lag K` is `-K`, `--lead K` is `+K`.
    fn shift(&self) -> Option<i64> {
        self.lag
            .map(|lag| -(lag as i64))
            .or_else(|| self.lead.map(i64::try_from).transpose().ok().flatten())
    }
}

fn shift_label(shift: i64) -> String {
    match shift {
        ..0 => format!("lag {}", -shift),
        _ => format!("lead {shift}"),
    }
}

#[derive(Args)]
struct CompareArgs {
    #[command(flatten)]
    run: RunArgs,

    /// Per-leg delta report JSONL path.
    #[arg(long, default_value = "compare-report.jsonl")]
    report: PathBuf,
}

type LegKey = (String, usize);

#[derive(Clone, Debug)]
struct LegRecord {
    input_mint: String,
    output_mint: String,
    amount: u64,
    metis_quoted_out: u64,
    original_quoted_out: u64,
}

/// The venue's flow before and after re-quoting, over the legs this run actually saw.
#[derive(Default)]
struct VenueTally {
    txs: AtomicU64,
    /// Keyed by the leg's own `(input, output)` mints. A quote change is not symmetric — moving a
    /// price is better for one direction of a book and worse for the other — so summing the two
    /// lets a gain on one side hide a collapse on the other.
    by_direction: Mutex<HashMap<(String, String), VenueCounts>>,
}

/// Every field counts participation in a leg, not the share of it: a venue on a split route
/// counts the same as one holding the whole leg. `split` is the honest qualifier on that.
#[derive(Clone, Copy, Default)]
struct VenueCounts {
    legs: u64,
    l1_legs: u64,
    held: u64,
    improvements: u64,
    /// Legs where the venue took less than the whole route on either side. A venue dropping from
    /// all of a leg to a sliver of it is `held` with nothing else to show for the move.
    split: u64,
    /// Legs whose original route could not be recovered, so the L1 side of this direction is
    /// unknown for them. Counted rather than folded into `l1_legs`: treating them as "not the
    /// venue" would report them as legs it won.
    unresolved: u64,
}

impl VenueCounts {
    /// Legs the re-quote took from another venue — including every leg whose L1 route was never
    /// recovered, since only a recovered route can put a leg in `held`. `unresolved` bounds how
    /// much of this is an artifact of that.
    const fn won(self) -> u64 {
        self.legs.saturating_sub(self.held)
    }

    /// Legs the venue had on a recovered L1 route and lost to the re-quote.
    const fn lost(self) -> u64 {
        self.l1_legs.saturating_sub(self.held)
    }

    fn add(&mut self, other: Self) {
        self.legs += other.legs;
        self.l1_legs += other.l1_legs;
        self.held += other.held;
        self.improvements += other.improvements;
        self.split += other.split;
        self.unresolved += other.unresolved;
    }
}

/// The venue's flow per trade direction, plus the totals across them.
struct VenueReport {
    txs: u64,
    total: VenueCounts,
    /// Busiest direction first, so the side that moved most is read first.
    by_direction: Vec<((String, String), VenueCounts)>,
}

impl VenueTally {
    fn report(&self) -> VenueReport {
        let mut by_direction: Vec<_> = self
            .by_direction
            .lock()
            .expect("venue tally")
            .iter()
            .map(|(mints, counts)| (mints.clone(), *counts))
            .collect();
        by_direction.sort_unstable_by_key(|(_, counts)| std::cmp::Reverse(counts.legs));
        let total = by_direction
            .iter()
            .fold(VenueCounts::default(), |mut total, (_, counts)| {
                total.add(*counts);
                total
            });
        VenueReport {
            txs: self.txs.load(Ordering::Relaxed),
            total,
            by_direction,
        }
    }
}

/// `So11111111111111111111111111111111111111112` as `So1111..1112`: enough to tell two mints of
/// a pair apart without wrapping the line.
fn short_mint(mint: &str) -> String {
    match mint.len() > 12 {
        true => format!("{}..{}", &mint[..6], &mint[mint.len() - 4..]),
        false => mint.to_string(),
    }
}

struct RunOutput {
    funnel: Option<RerouteStatsReport>,
    legs: BTreeMap<LegKey, LegRecord>,
    venue: VenueReport,
}

/// The venue under test, named both ways its two route shapes name it. One value, since a run
/// that knows only one side reports every leg as won.
#[derive(Clone)]
struct Venue {
    /// Metis's display label, how its hops are named in the re-quoted route.
    label: String,
    /// The program, which is how its hops are named in the original's route.
    program: Address,
}

struct RunConfig {
    range: RangeArgs,
    overrides: Vec<(u64, AccountModifications)>,
    filter: Option<RerouteFilter>,
    venue: Option<Venue>,
    jsonl_out: Option<PathBuf>,
}

/// One line of the `capture` JSONL.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureRow {
    slot: u64,
    account: AccountData,
}

/// One line of the `run` reroute-notification JSONL.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RerouteRow<'a> {
    slot: u64,
    original_signature: &'a str,
    legs: Vec<RerouteLegRow<'a>>,
    err: Option<&'a str>,
    compute_units: u64,
    realized_out: Option<u64>,
    original_realized_out: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RerouteLegRow<'a> {
    input_mint: &'a str,
    output_mint: &'a str,
    amount: u64,
    original_quoted_out: u64,
    metis_quoted_out: u64,
    /// Metis's chosen route. The per-hop `ammKey`s are how you find the pool to capture
    /// and override.
    route_plan: Option<&'a str>,
    /// The route the original took on L1, so a reader can see what the re-quote displaced.
    original_route_plan: Option<&'a str>,
}

impl<'a> From<&'a RerouteLegNotification> for RerouteLegRow<'a> {
    fn from(leg: &'a RerouteLegNotification) -> Self {
        Self {
            input_mint: &leg.input_mint,
            output_mint: &leg.output_mint,
            amount: leg.amount,
            original_quoted_out: leg.original_quoted_out,
            metis_quoted_out: leg.metis_quoted_out,
            route_plan: leg.route_plan.as_deref(),
            original_route_plan: leg.original_route_plan.as_deref(),
        }
    }
}

/// One line of the `compare` report JSONL.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JoinedLeg {
    shift: i64,
    original_signature: String,
    leg_index: usize,
    input_mint: String,
    output_mint: String,
    amount: u64,
    original_quoted_out: u64,
    base_quoted_out: u64,
    quoted_out: u64,
    delta_bps: f64,
}

struct DeltaSummary {
    matched: usize,
    median_abs_bps: f64,
    mean_abs_bps: f64,
    p90_abs_bps: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    match cli.command {
        Command::Capture(args) => capture(cli.conn, args).await,
        Command::Run(args) => run(cli.conn, args).await.map(|_| ()),
        Command::Compare(args) => compare(cli.conn, args).await,
    }
}

async fn capture(conn: ConnectionArgs, args: CaptureArgs) -> Result<()> {
    let create = CreateSession::builder()
        .start_slot(args.range.start_slot)
        .slot_count(args.range.slot_count)
        .replay_account_state(!args.range.no_replay)
        .send_summary(true)
        .build()
        .into_request()?;
    let mut session =
        ManagedBacktestSession::start(conn.websocket_url(), conn.api_key.clone(), create).await?;

    let rows = Arc::new(Mutex::new(BTreeMap::<u64, AccountData>::new()));
    let conversion_error = Arc::new(Mutex::new(None::<anyhow::Error>));
    let first_pre_taken = Arc::new(AtomicBool::new(false));
    let start_slot = args.range.start_slot;
    let sink = rows.clone();
    let error_sink = conversion_error.clone();
    let handle = subscribe_account_diffs(
        &conn.rpc_url(&session.session_info().rpc_endpoint),
        &args.account.to_string(),
        move |diff: AccountDiffNotification| {
            let sink = sink.clone();
            let error_sink = error_sink.clone();
            let first_pre_taken = first_pre_taken.clone();
            async move {
                let insert = |slot, converted: Result<AccountData, _>, initial| match converted {
                    Ok(account) => {
                        let mut rows = sink.lock().unwrap();
                        if initial {
                            rows.entry(slot).or_insert(account);
                        } else {
                            rows.insert(slot, account);
                        }
                    }
                    Err(error) => {
                        let mut first = error_sink.lock().unwrap();
                        if first.is_none() {
                            *first = Some(anyhow::Error::new(error).context(format!(
                                "converting account diff at slot {}",
                                diff.context.slot
                            )));
                        }
                    }
                };
                // The first diff's `pre` is the account's state entering the range, so a
                // schedule has a value to post before the account's first in-range write.
                if !first_pre_taken.swap(true, Ordering::SeqCst)
                    && let Some(pre) = &diff.pre
                {
                    insert(start_slot, account_data_from_ui(pre), true);
                }
                if let Some(post) = diff.post_account_data() {
                    insert(diff.context.slot, post, false);
                }
            }
        },
    )
    .await?;

    drive_to_completion(&mut session, args.range.slot_count).await?;
    handle.stop.send(true).ok();
    handle.join_handle.await??;
    session.shutdown().await;
    if let Some(error) = conversion_error.lock().unwrap().take() {
        return Err(error);
    }

    let rows = Arc::try_unwrap(rows)
        .map_err(|_| anyhow!("captured rows are still shared"))?
        .into_inner()?;
    ensure!(
        !rows.is_empty(),
        "account {} never appeared in [{}, {}]",
        args.account,
        args.range.start_slot,
        args.range.start_slot + args.range.slot_count
    );
    let captured = rows.len();
    let mut out = io::BufWriter::new(fs::File::create(&args.out)?);
    for (slot, account) in rows {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&CaptureRow { slot, account })?
        )?;
    }
    out.flush()?;
    println!("captured {captured} states to {}", args.out.display());
    Ok(())
}

async fn drive_to_completion(
    session: &mut ManagedBacktestSession,
    slot_count: u64,
) -> Result<Option<RerouteStatsReport>> {
    loop {
        match session.next_event().await? {
            ManagedEvent::ReadyForContinue => {
                let advance = Continue::builder().advance_count(slot_count).build();
                session.send_continue(advance.into_params()).await?;
            }
            ManagedEvent::Slot(slot) => eprintln!("[slot] {slot}"),
            ManagedEvent::Completed { summary, .. } => {
                return Ok(summary.and_then(|summary| summary.reroute_stats.map(|stats| *stats)));
            }
            ManagedEvent::Error(error) => bail!("session error: {error}"),
            _ => {}
        }
    }
}

fn nearest_capture_at_or_before(
    captured: &BTreeMap<u64, AccountData>,
    slot: u64,
) -> Option<&AccountData> {
    captured
        .range(..=slot)
        .next_back()
        .map(|(_, account)| account)
}

/// One override per captured slot, each posting the state captured `shift` slots away:
/// negative lags the venue behind reality, positive runs it ahead.
fn build_shift_actions(
    shift: i64,
    account: Address,
    captured: &BTreeMap<u64, AccountData>,
    start: u64,
    end: u64,
) -> Vec<(u64, AccountModifications)> {
    captured
        .range(start..=end)
        .filter_map(|(slot, _)| {
            let target = slot.checked_add_signed(shift)?;
            nearest_capture_at_or_before(captured, target).map(|state| {
                (
                    *slot,
                    AccountModifications(BTreeMap::from([(account, state.clone())])),
                )
            })
        })
        .collect()
}

fn load_capture(path: &Path) -> Result<BTreeMap<u64, AccountData>> {
    let input = io::BufReader::new(
        fs::File::open(path).with_context(|| format!("opening capture {}", path.display()))?,
    );
    input
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let row: CaptureRow = serde_json::from_str(&line?)
                .with_context(|| format!("parsing {} line {}", path.display(), index + 1))?;
            Ok((row.slot, row.account))
        })
        .collect()
}

fn parse_pair(value: &str) -> Result<MintPair, String> {
    let (base, quote) = value
        .split_once(',')
        .ok_or("expected two base58 mints separated by a comma")?;
    let parse = |mint: &str| mint.trim().parse::<Address>().map_err(|e| e.to_string());
    Ok(MintPair::new(parse(base)?, parse(quote)?))
}

fn filter_from(args: &RunArgs) -> Option<RerouteFilter> {
    (!args.filter_pair.is_empty()).then(|| RerouteFilter {
        pairs: args.filter_pair.iter().copied().collect(),
    })
}

/// The venue under test, when one was named.
async fn venue_of(args: &RunArgs) -> Result<Option<Venue>> {
    let Some(program) = args.program_id else {
        return Ok(None);
    };
    let label = resolve_venue_label(&program).await?;
    eprintln!("[jup] resolved venue label: {label:?}");
    Ok(Some(Venue { label, program }))
}

async fn run(conn: ConnectionArgs, args: RunArgs) -> Result<RunOutput> {
    let overrides = match args.shift() {
        None => Vec::new(),
        Some(shift) => {
            let captured = load_capture(
                args.capture
                    .as_deref()
                    .ok_or_else(|| anyhow!("--capture is required with --lag/--lead"))?,
            )?;
            build_shift_actions(
                shift,
                args.account,
                &captured,
                args.range.start_slot,
                args.range.start_slot + args.range.slot_count,
            )
        }
    };
    let config = RunConfig {
        range: args.range.clone(),
        overrides,
        filter: filter_from(&args),
        venue: venue_of(&args).await?,
        jsonl_out: Some(args.out.clone()),
    };
    let output = run_once(conn, config).await?;
    report_run("run", &output);
    Ok(output)
}

/// A venue holding part of a leg but not all of it.
const fn is_split(share: u8) -> bool {
    share > 0 && share < WHOLE_LEG
}

/// One leg's contribution to the tally, or `None` when the venue is on neither side of it.
fn leg_counts(leg: &RerouteLegNotification, venue: &Venue) -> Option<VenueCounts> {
    let after = venue_share(leg, &venue.label);
    let before = original_venue_share(leg, &venue.program);
    let ran_before = before.is_some_and(|share| share > 0);
    (after > 0 || ran_before).then(|| VenueCounts {
        legs: u64::from(after > 0),
        l1_legs: u64::from(ran_before),
        held: u64::from(after > 0 && ran_before),
        improvements: u64::from(after > 0 && leg.metis_quoted_out > leg.original_quoted_out),
        split: u64::from(is_split(after) || before.is_some_and(is_split)),
        unresolved: u64::from(before.is_none()),
    })
}

async fn run_once(conn: ConnectionArgs, config: RunConfig) -> Result<RunOutput> {
    let create = config
        .overrides
        .into_iter()
        .fold(
            CreateSession::builder()
                .start_slot(config.range.start_slot)
                .slot_count(config.range.slot_count)
                .reroute_order_flow(true)
                .maybe_reroute_filter(config.filter)
                .replay_account_state(!config.range.no_replay)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .send_summary(true)
                .build(),
            |create, (slot, overrides)| create.add_override(slot, overrides),
        )
        .into_request()?;
    let mut session =
        ManagedBacktestSession::start(conn.websocket_url(), conn.api_key.clone(), create).await?;

    let legs = Arc::new(Mutex::new(BTreeMap::<LegKey, LegRecord>::new()));
    let venue = Arc::new(VenueTally::default());
    let write_error = Arc::new(Mutex::new(None::<io::Error>));
    let jsonl = config
        .jsonl_out
        .map(|path| fs::File::create(path).map(io::BufWriter::new))
        .transpose()?
        .map(|writer| Arc::new(Mutex::new(writer)));
    let sink = legs.clone();
    let tally = venue.clone();
    let error_sink = write_error.clone();
    let writer = jsonl.clone();
    let under_test = config.venue.clone();
    let handle = subscribe_reroutes(
        &conn.rpc_url(&session.session_info().rpc_endpoint),
        move |notification: RerouteNotification| {
            let sink = sink.clone();
            let tally = tally.clone();
            let jsonl = writer.clone();
            let error_sink = error_sink.clone();
            let under_test = under_test.clone();
            async move {
                {
                    let mut legs = sink.lock().unwrap();
                    for (index, leg) in notification.legs.iter().enumerate() {
                        legs.insert(
                            (notification.original_signature.clone(), index),
                            LegRecord {
                                input_mint: leg.input_mint.clone(),
                                output_mint: leg.output_mint.clone(),
                                amount: leg.amount,
                                metis_quoted_out: leg.metis_quoted_out,
                                original_quoted_out: leg.original_quoted_out,
                            },
                        );
                    }
                }

                if let Some(under_test) = &under_test {
                    // Both sides over the same legs, so won and lost are differences on one
                    // population rather than two counts from different runs.
                    let counted = notification
                        .legs
                        .iter()
                        .filter_map(|leg| {
                            let key = (leg.input_mint.clone(), leg.output_mint.clone());
                            leg_counts(leg, under_test).map(|counts| (key, counts))
                        })
                        .fold(
                            HashMap::<(String, String), VenueCounts>::new(),
                            |mut counted, (key, counts)| {
                                counted.entry(key).or_default().add(counts);
                                counted
                            },
                        );
                    let matched: u64 = counted.values().map(|counts| counts.legs).sum();
                    let mut by_direction = tally.by_direction.lock().expect("venue tally");
                    for (key, counts) in counted {
                        by_direction.entry(key).or_default().add(counts);
                    }
                    drop(by_direction);
                    if matched > 0 {
                        tally.txs.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if let Some(jsonl) = &jsonl {
                    let row = RerouteRow {
                        slot: notification.slot,
                        original_signature: &notification.original_signature,
                        legs: notification.legs.iter().map(RerouteLegRow::from).collect(),
                        err: notification.err.as_deref(),
                        compute_units: notification.compute_units_consumed,
                        realized_out: notification.realized_output_amount,
                        original_realized_out: notification.original_realized_output_amount,
                    };
                    let written = serde_json::to_string(&row)
                        .map_err(io::Error::from)
                        .and_then(|line| writeln!(jsonl.lock().unwrap(), "{line}"));
                    if let Err(error) = written {
                        let mut first = error_sink.lock().unwrap();
                        if first.is_none() {
                            *first = Some(error);
                        }
                    }
                }
            }
        },
    )
    .await?;

    let funnel = drive_to_completion(&mut session, config.range.slot_count).await?;
    handle.stop.send(true).ok();
    handle.join_handle.await??;
    session.shutdown().await;
    if let Some(error) = write_error.lock().unwrap().take() {
        return Err(error.into());
    }
    if let Some(jsonl) = &jsonl {
        jsonl.lock().unwrap().flush()?;
    }
    let legs = Arc::try_unwrap(legs)
        .map_err(|_| anyhow!("reroute results are still shared"))?
        .into_inner()?;
    Ok(RunOutput {
        funnel,
        legs,
        venue: venue.report(),
    })
}

fn report_run(label: &str, output: &RunOutput) {
    if let Some(stats) = &output.funnel {
        eprintln!(
            "[{label}] reroute: {} detected | {} filtered -> {} rerouted -> {} simulated -> {} succeeded | {} requote-fail",
            stats.swaps_detected,
            stats.swaps_filtered,
            stats.swaps_rerouted,
            stats.swaps_simulated,
            stats.swaps_succeeded,
            stats.requote_failures,
        );
    }
    let venue = &output.venue;
    let total = venue.total;
    eprintln!("[{label}] {} re-quoted legs seen", output.legs.len());
    if total.legs > 0 || total.l1_legs > 0 {
        eprintln!(
            "[{label}] venue on L1: legs={} | after re-quote: legs={} transactions={} (held={} won={} lost={} split={}) | legs where metis quoted higher={}",
            total.l1_legs,
            total.legs,
            venue.txs,
            total.held,
            total.won(),
            total.lost(),
            total.split,
            total.improvements
        );
        eprintln!(
            "[{label}] won/lost read only as a difference against the `--lag 0` control, which itself reports the venue losing most of its L1 legs with nothing changed — an absolute lost is not attributable"
        );
        // Participation, not share: a venue dropping from all of a leg to a sliver of it is held.
        if total.split > 0 {
            eprintln!(
                "[{label}] {} of the venue's legs were split routes it only partly held; held/won/lost count participation, not share",
                total.split
            );
        }
        // An unresolved leg cannot enter `held`, so it lands in `won` whether the venue took it
        // from anyone or not. Silently it would read as a clean gain.
        if total.unresolved > 0 {
            eprintln!(
                "[{label}] {} legs carried no recoverable L1 route; their before-side is unknown",
                total.unresolved
            );
        }
        // A price change moves the two directions of a book opposite ways, so the totals above
        // can net a collapse against a gain and read as neither.
        for ((input, output), counts) in &venue.by_direction {
            eprintln!(
                "[{label}]   {}->{}: L1={} after={} (held={} won={} lost={} split={}) improved={}",
                short_mint(input),
                short_mint(output),
                counts.l1_legs,
                counts.legs,
                counts.held,
                counts.won(),
                counts.lost(),
                counts.split,
                counts.improvements
            );
        }
    }
}

async fn compare(conn: ConnectionArgs, args: CompareArgs) -> Result<()> {
    let shift = args.run.shift().ok_or_else(|| {
        anyhow!(
            "--lag/--lead/--price-shift-bps is required for compare; its reference arm is the \
             null control, the same capture posted unmodified at each state's own slot"
        )
    })?;
    let label = shift_label(shift);
    let captured = load_capture(
        args.run
            .capture
            .as_deref()
            .ok_or_else(|| anyhow!("--capture is required for compare"))?,
    )?;
    let venue = venue_of(&args.run).await?;
    let make_config = |overrides| RunConfig {
        range: args.run.range.clone(),
        overrides,
        filter: filter_from(&args.run),
        venue: venue.clone(),
        jsonl_out: None,
    };

    eprintln!("[baseline] running unmodified...");
    let baseline = run_once(conn.clone(), make_config(Vec::new())).await?;
    report_run("baseline", &baseline);

    let overrides = build_shift_actions(
        shift,
        args.run.account,
        &captured,
        args.run.range.start_slot,
        args.run.range.start_slot + args.run.range.slot_count,
    );
    ensure!(
        !overrides.is_empty(),
        "--{label} built no overrides; does the capture cover the range?"
    );
    eprintln!("[modified] running with {label}...");
    let modified = run_once(conn.clone(), make_config(overrides)).await?;
    report_run("modified", &modified);

    let (joined, zero_baseline) = join_legs(shift, &baseline.legs, &modified.legs);
    let mut report = io::BufWriter::new(fs::File::create(&args.report)?);
    for row in &joined {
        writeln!(report, "{}", serde_json::to_string(row)?)?;
    }
    report.flush()?;

    let deltas = joined.iter().map(|row| row.delta_bps).collect::<Vec<_>>();
    let stats = delta_summary(&deltas);
    let moved = deltas.iter().filter(|delta| **delta != 0.0).count();
    println!("=== {label} vs the null control ===");
    println!(
        "legs matched: {} ({moved} moved){}",
        stats.matched,
        match zero_baseline {
            0 => String::new(),
            n => format!(", {n} legs excluded where the control quoted zero"),
        }
    );
    println!(
        "|delta| bps: median {:.3} | mean {:.3} | p90 {:.3}",
        stats.median_abs_bps, stats.mean_abs_bps, stats.p90_abs_bps
    );
    if args.run.program_id.is_some() {
        println!(
            "venue legs captured: control {} -> modified {}",
            baseline.venue.total.legs, modified.venue.total.legs
        );
    }
    println!("report written to {}", args.report.display());
    // Quoting is not deterministic run to run: re-run both sides and trust only the
    // legs that move consistently. See the README.
    eprintln!("[note] single-run deltas include router noise; repeat both runs before attributing");
    Ok(())
}

/// `None` when the baseline quoted zero out — a delta is meaningless there.
///
/// Fractional: router jitter routinely moves a quote by well under a basis point, and
/// integer bps would report every one of those as an unmoved leg.
fn delta_bps(base: u64, variant: u64) -> Option<f64> {
    (base != 0).then(|| (variant as f64 - base as f64) / base as f64 * 10_000.0)
}

/// Joins legs present in both runs; returns the rows plus the count of matched legs
/// excluded for a zero baseline quote.
fn join_legs(
    shift: i64,
    base: &BTreeMap<LegKey, LegRecord>,
    modified: &BTreeMap<LegKey, LegRecord>,
) -> (Vec<JoinedLeg>, usize) {
    let matched = base
        .iter()
        .filter_map(|(key, base)| modified.get(key).map(|modified| (key, base, modified)))
        .collect::<Vec<_>>();
    let rows = matched
        .iter()
        .filter_map(|(key, base, modified)| {
            delta_bps(base.metis_quoted_out, modified.metis_quoted_out).map(|delta_bps| JoinedLeg {
                shift,
                original_signature: key.0.clone(),
                leg_index: key.1,
                input_mint: base.input_mint.clone(),
                output_mint: base.output_mint.clone(),
                amount: base.amount,
                original_quoted_out: base.original_quoted_out,
                base_quoted_out: base.metis_quoted_out,
                quoted_out: modified.metis_quoted_out,
                delta_bps,
            })
        })
        .collect::<Vec<_>>();
    let zero_baseline = matched.len() - rows.len();
    (rows, zero_baseline)
}

fn delta_summary(deltas: &[f64]) -> DeltaSummary {
    let mut absolute = deltas.iter().map(|delta| delta.abs()).collect::<Vec<_>>();
    absolute.sort_by(f64::total_cmp);
    // Round-index quantiles: sorted[round((n-1)·p)].
    let at = |quantile: f64| {
        absolute
            .get(((absolute.len() as f64 - 1.0) * quantile).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    DeltaSummary {
        matched: absolute.len(),
        median_abs_bps: at(0.5),
        mean_abs_bps: if absolute.is_empty() {
            0.0
        } else {
            absolute.iter().sum::<f64>() / absolute.len() as f64
        },
        p90_abs_bps: at(0.9),
    }
}

#[cfg(test)]
mod tests {

    /// The totals are folded from the per-direction map, so a book whose two sides moved opposite
    /// ways must still report each side — summing them is what hid the collapse.
    #[test]
    fn a_report_keeps_each_direction_apart() {
        let tally = VenueTally::default();
        {
            let mut by_direction = tally.by_direction.lock().expect("venue tally");
            by_direction.insert(
                ("SOL".to_string(), "USDC".to_string()),
                VenueCounts {
                    legs: 29,
                    l1_legs: 515,
                    held: 20,
                    ..VenueCounts::default()
                },
            );
            by_direction.insert(
                ("USDC".to_string(), "SOL".to_string()),
                VenueCounts {
                    legs: 15581,
                    l1_legs: 741,
                    held: 300,
                    ..VenueCounts::default()
                },
            );
        }

        let report = tally.report();

        assert_eq!(report.total.legs, 15610);
        assert_eq!(report.total.l1_legs, 1256);
        let sides: Vec<_> = report
            .by_direction
            .iter()
            .map(|((input, _), counts)| (input.as_str(), counts.legs, counts.won(), counts.lost()))
            .collect();
        // Busiest first, and the collapsed side still visible with its own lost count.
        assert_eq!(
            sides,
            vec![("USDC", 15581, 15281, 441), ("SOL", 29, 9, 495)]
        );
    }
    use simulator_api::{BinaryEncoding, EncodedBinary};

    use super::*;

    fn account_key() -> Address {
        Address::from([9; 32])
    }

    fn state(slot: u64) -> AccountData {
        AccountData {
            data: EncodedBinary::new(String::new(), BinaryEncoding::Base64),
            executable: false,
            lamports: slot,
            owner: Address::from([8; 32]),
            space: 0,
        }
    }

    fn captured_at(slots: &[u64]) -> BTreeMap<u64, AccountData> {
        slots.iter().map(|slot| (*slot, state(*slot))).collect()
    }

    fn leg(quoted: u64) -> LegRecord {
        LegRecord {
            input_mint: "in".into(),
            output_mint: "out".into(),
            amount: 1,
            metis_quoted_out: quoted,
            original_quoted_out: quoted,
        }
    }

    /// (anchor slot, override payload lamports) per action.
    fn schedule_of(actions: &[(u64, AccountModifications)]) -> Vec<(u64, u64)> {
        actions
            .iter()
            .map(|(slot, overrides)| (*slot, overrides.0[&account_key()].lamports))
            .collect()
    }

    fn conn(url: &str) -> ConnectionArgs {
        ConnectionArgs {
            url: url.to_string(),
            api_key: String::new(),
        }
    }

    #[test]
    fn websocket_url_defaults_to_tls_and_honours_an_explicit_scheme() {
        for (url, expected) in [
            ("sim.example.com", "wss://sim.example.com/backtest"),
            ("ws://localhost:8900", "ws://localhost:8900/backtest"),
            (
                "ws://localhost:8900/backtest",
                "ws://localhost:8900/backtest",
            ),
            ("wss://sim.example.com/", "wss://sim.example.com/backtest"),
        ] {
            assert_eq!(conn(url).websocket_url(), expected, "{url}");
        }
    }

    #[test]
    fn rpc_url_follows_the_websocket_scheme_and_passes_absolute_endpoints_through() {
        for (url, endpoint, expected) in [
            (
                "sim.example.com",
                "/backtest/s1",
                "https://sim.example.com/backtest/s1",
            ),
            (
                "ws://localhost:8900",
                "backtest/s1",
                "http://localhost:8900/backtest/s1",
            ),
            (
                "ws://localhost:8900/backtest",
                "backtest/s1",
                "http://localhost:8900/backtest/s1",
            ),
            ("sim.example.com", "https://other/rpc", "https://other/rpc"),
        ] {
            assert_eq!(conn(url).rpc_url(endpoint), expected, "{url} + {endpoint}");
        }
    }

    #[test]
    fn positive_shift_runs_each_capture_forward_by_k() {
        let captured = captured_at(&[10, 12, 14]);
        let actions = build_shift_actions(2, account_key(), &captured, 10, 15);
        // Slot 10 sees the slot-12 capture, 12 sees 14; the tail carries the last capture.
        assert_eq!(schedule_of(&actions), vec![(10, 12), (12, 14), (14, 14)]);
    }

    #[test]
    fn negative_shift_lags_each_capture_behind_by_k() {
        let captured = captured_at(&[10, 12, 14]);
        let actions = build_shift_actions(-2, account_key(), &captured, 10, 15);
        // Slot 12 sees the slot-10 capture, 14 sees 12; slot 10 has nothing older to post.
        assert_eq!(schedule_of(&actions), vec![(12, 10), (14, 12)]);
    }

    #[test]
    fn empty_capture_builds_nothing() {
        assert!(build_shift_actions(1, account_key(), &BTreeMap::new(), 10, 15).is_empty());
    }

    #[test]
    fn shift_prefers_lag_and_labels_by_direction() {
        let args = |lag, lead| RunArgs {
            range: RangeArgs {
                start_slot: 1,
                slot_count: 1,
                no_replay: true,
            },
            account: account_key(),
            capture: None,
            lag,
            lead,
            program_id: None,
            filter_pair: Vec::new(),
            out: PathBuf::from("out.jsonl"),
        };
        assert_eq!(args(Some(10), None).shift(), Some(-10));
        assert_eq!(args(None, Some(10)).shift(), Some(10));
        assert_eq!(args(None, None).shift(), None);
        assert_eq!(shift_label(-10), "lag 10");
        assert_eq!(shift_label(10), "lead 10");
    }

    #[test]
    fn delta_bps_is_signed_scaled_and_undefined_on_zero_baseline() {
        assert_eq!(delta_bps(10_000, 10_100), Some(100.0));
        assert_eq!(delta_bps(10_000, 9_900), Some(-100.0));
        assert_eq!(delta_bps(10_000, 10_000), Some(0.0));
        assert_eq!(delta_bps(0, 42), None);
        // Sub-bps moves must survive: the router jitters well below one basis point.
        let sub_bps = delta_bps(10_000_000, 10_000_001).unwrap();
        assert!((sub_bps - 0.001).abs() < 1e-9, "{sub_bps}");
    }

    #[test]
    fn join_legs_pairs_common_keys_and_excludes_zero_baselines() {
        let base = BTreeMap::from([
            (("sig1".into(), 0), leg(100)),
            (("sig2".into(), 0), leg(200)),
            (("sig4".into(), 0), leg(0)),
        ]);
        let modified = BTreeMap::from([
            (("sig1".into(), 0), leg(110)),
            (("sig3".into(), 0), leg(999)),
            (("sig4".into(), 0), leg(7)),
        ]);
        let (joined, zero_baseline) = join_legs(-4, &base, &modified);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].delta_bps, 1000.0);
        assert_eq!(joined[0].shift, -4);
        assert_eq!(zero_baseline, 1);
    }

    #[test]
    fn summary_percentiles_over_abs_deltas() {
        // abs-sorted [0, 50, 100, 200]: round-index median = idx round(1.5) = 100,
        // p90 = idx round(2.7) = 200.
        let stats = delta_summary(&[0.0, -100.0, 50.0, 200.0]);
        assert_eq!(stats.matched, 4);
        assert_eq!(stats.median_abs_bps, 100.0);
        assert_eq!(stats.p90_abs_bps, 200.0);
        assert_eq!(stats.mean_abs_bps, 87.5);
    }
}

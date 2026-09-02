//! Change a venue's state and measure the taker flow it wins or loses.
//!
//! `capture` records the account's per-slot states; `run` posts them back modified, visible only
//! to the router. `--price-shift-bps` re-prices, `--lag`/`--lead` shifts in time.
//!
//! `--setup-transactions` carries a time shift as the venue's own update transaction re-executed
//! at the shifted slot. A venue that stamps its last-update slot needs this for `--lead`, since a
//! future snapshot is rejected; it requires a `--no-replay` capture.

mod cli;
mod jsonl;
mod session;
mod venue;

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    future::ready,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::Parser;
use simulator_api::{
    AccountData, AccountModifications, ActionAnchor, ActionKind, BinaryEncoding, EncodedBinary,
    RerouteAggregators, RerouteFilter, RerouteStatsReport, ScheduledAction,
};
use simulator_client::{
    AccountDiffNotification, CreateSession, FULL_PERCENT, ManagedBacktestSession,
    RerouteLegNotification, RerouteNotification,
    reroute_report::{Report, Target, short_mint},
    subscribe_account_diffs, subscribe_reroutes,
};
use solana_address::Address;

use crate::{
    cli::{
        CaptureArgs, Cli, Command, CompareArgs, ConnectionArgs, RangeArgs, ReportArgs, RunArgs,
        filter_from, shift_label,
    },
    jsonl::{
        CaptureRow, FORMAT_VERSION, HeaderKind, JoinedLeg, RunHeader, RunSummary,
        load_capture_rows, wire_transaction, write_capture,
    },
    session::{CaptureCollector, create_session, drive_to_completion},
    venue::{original_venue_share, resolve_venue_label, venue_share},
};

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
    txs: u64,
    by_direction: HashMap<(String, String), VenueCounts>,
}

/// Every field counts participation in a leg, not the share of it: a venue on a split route
/// counts the same as one holding the whole leg. `split` is the honest qualifier on that.
#[derive(Clone, Copy, Default)]
struct VenueCounts {
    legs: u64,
    l1_legs: u64,
    held: u64,
    improvements: u64,
    split: u64,
    unresolved: u64,
}

impl VenueCounts {
    /// Legs the re-quote took from another venue — including every leg whose L1 route was never
    /// recovered, since only a recovered route can put a leg in `held`.
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
    by_direction: Vec<((String, String), VenueCounts)>,
}

impl VenueTally {
    fn report(&self) -> VenueReport {
        let mut by_direction: Vec<_> = self
            .by_direction
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
            txs: self.txs,
            total,
            by_direction,
        }
    }
}

struct RunOutput {
    funnel: Option<RerouteStatsReport>,
    legs: BTreeMap<LegKey, LegRecord>,
    venue: VenueReport,
    scheduled: usize,
}

struct RunConfig {
    range: RangeArgs,
    schedule: Schedule,
    filter: Option<RerouteFilter>,
    venue: Option<Target>,
    jsonl_out: Option<PathBuf>,
    detect_failed_l1_swaps: bool,
    circular_arbs: bool,
    reroute_aggregators: Option<RerouteAggregators>,
    /// The arm, recorded in the file's header.
    shift: Option<i64>,
    price_shift_bps: Option<f64>,
    carrier: &'static str,
    record_full: bool,
}

impl RunConfig {
    fn header(&self) -> RunHeader {
        RunHeader {
            format_version: FORMAT_VERSION,
            kind: HeaderKind::CounterfactualFlowRun,
            start_slot: self.range.start_slot,
            end_slot: self.range.start_slot + self.range.slot_count,
            program_id: self
                .venue
                .as_ref()
                .and_then(Target::program)
                .map(|program| program.to_string()),
            label: self
                .venue
                .as_ref()
                .and_then(Target::label)
                .map(str::to_string),
            shift: self.shift,
            price_shift_bps: self.price_shift_bps,
            override_slots: self.schedule.entries(),
            carrier: self.carrier.to_string(),
            slim: !self.record_full,
            reroute_aggregators: self.reroute_aggregators.as_ref().map(ToString::to_string),
            filter_pairs: self
                .filter
                .iter()
                .flat_map(|filter| filter.pairs.iter())
                .map(|pair| {
                    let (base, quote) = pair.mints();
                    format!("{base},{quote}")
                })
                .collect(),
            circular_arbs: self.circular_arbs,
            detect_failed_l1_swaps: self.detect_failed_l1_swaps,
            replay_account_state: !self.range.no_replay,
        }
    }
}

/// Empty on both counts is the baseline; the carriers are alternatives, never
/// combined.
#[derive(Default)]
struct Schedule {
    /// Captured bytes, one entry per anchor slot.
    overrides: Vec<(u64, AccountModifications)>,
    /// The venue's own update transactions, simulated at their shifted slots; the account
    /// state each leaves behind is published in place of the bytes.
    setup: Option<ScheduledAction>,
}

impl Schedule {
    /// Anchor slots the schedule posts at, either way it carries them.
    fn entries(&self) -> usize {
        self.overrides.len()
            + self
                .setup
                .as_ref()
                .map_or(0, |action| match &action.anchor {
                    ActionAnchor::BeforeSlot { slots } => slots.len(),
                    _ => 0,
                })
    }
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
        Command::Capture(args) => capture(args).await,
        Command::Run(args) => run(args).await.map(|_| ()),
        Command::Compare(args) => compare(args).await,
        Command::Report(args) => report_recording(args).await,
    }
}

/// Reads the recording and nothing else. With no selector it measures the venue the run named.
async fn report_recording(args: ReportArgs) -> Result<()> {
    let recording = jsonl::read_recording(&args.recording)?;
    if let Some(header) = &recording.header
        && header.format_version != FORMAT_VERSION
    {
        bail!(
            "{} is format version {}, this build reads {FORMAT_VERSION}",
            args.recording.display(),
            header.format_version
        );
    }

    // `--program-id` names both sides: its label matches re-quoted hops, the program L1 ones.
    let target = match (&args.label, args.program_id) {
        (None, None) => recording
            .header
            .as_ref()
            .and_then(target_from_header)
            .ok_or_else(|| {
                anyhow!(
                    "{} names no venue, so there is no default to measure: \
                     pass --program-id or --label",
                    args.recording.display()
                )
            })?,
        (Some(label), program) => Target::new(Some(label.clone()), program),
        (None, Some(program)) => {
            Target::new(Some(resolve_venue_label(&program).await?), Some(program))
        }
    };

    let report = Report::from_notifications(target, &recording.notifications)?;
    match args.json {
        true => println!("{}", report.to_json()),
        false => println!(
            "{}",
            report.render(slot_range(&recording.header).as_deref())
        ),
    }
    Ok(())
}

/// The venue the run named, which a report with no selector measures.
fn target_from_header(header: &RunHeader) -> Option<Target> {
    let program = header
        .program_id
        .as_ref()
        .and_then(|program| program.parse().ok());
    let label = header.label.clone();
    (program.is_some() || label.is_some()).then(|| Target::new(label, program))
}

/// The range the recording knows and the report does not.
fn slot_range(header: &Option<RunHeader>) -> Option<String> {
    header
        .as_ref()
        .map(|header| format!("slots {}–{}", header.start_slot, header.end_slot))
}

async fn capture(args: CaptureArgs) -> Result<()> {
    let conn = &args.conn;
    let create = CreateSession::builder()
        .start_slot(args.range.start_slot)
        .slot_count(args.range.slot_count)
        .replay_account_state(!args.range.no_replay)
        .send_summary(true)
        .build()
        .into_request()?;
    let mut session =
        ManagedBacktestSession::start(conn.websocket_url(), conn.api_key.clone(), create).await?;

    session.subscribe_transactions(vec![args.account]);

    let collector = Arc::new(Mutex::new(CaptureCollector::default()));
    let start_slot = args.range.start_slot;
    let sink = collector.clone();
    let handle = subscribe_account_diffs(
        &conn.rpc_url(&session.session_info().rpc_endpoint),
        &args.account.to_string(),
        move |diff: AccountDiffNotification| {
            sink.lock()
                .expect("capture collector")
                .record_diff(&diff, start_slot);
            ready(())
        },
    )
    .await?;

    let mut wire = HashMap::new();
    let mut undecodable = 0u64;
    drive_to_completion(
        &mut session,
        args.range.slot_count,
        |transaction| match wire_transaction(&transaction.transaction) {
            Some((signature, encoded)) => {
                wire.insert(signature, encoded);
            }
            None => undecodable += 1,
        },
    )
    .await?;
    handle.stop.send(true).ok();
    handle.join_handle.await??;
    session.shutdown().await;

    let collected = std::mem::take(&mut *collector.lock().expect("capture collector"));
    if let Some(error) = collected.conversion_error {
        return Err(error);
    }
    let rows = collected
        .rows
        .into_values()
        .map(|row| CaptureRow {
            transaction: row
                .signature
                .as_ref()
                .and_then(|signature| wire.get(signature).cloned()),
            ..row
        })
        .collect::<Vec<_>>();
    ensure!(
        !rows.is_empty(),
        "account {} never appeared in [{}, {}]",
        args.account,
        args.range.start_slot,
        args.range.start_slot + args.range.slot_count
    );
    let resolved = rows.iter().filter(|row| row.transaction.is_some()).count();
    write_capture(&args.out, &rows)?;
    println!(
        "captured {} states ({resolved} with a transaction) to {}",
        rows.len(),
        args.out.display()
    );
    if undecodable > 0 {
        eprintln!("[capture] {undecodable} streamed transactions could not be re-encoded");
    }
    Ok(())
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

/// The transaction that ran at slot `t` is simulated at `t - shift` instead. Only `account`'s
/// post-state is published, so the rest of what the transaction touches stays out.
fn build_setup_action(
    shift: i64,
    account: Address,
    rows: &[CaptureRow],
    start: u64,
    end: u64,
) -> Option<ScheduledAction> {
    let anchored = rows
        .iter()
        .filter_map(|row| {
            let transaction = row.transaction.clone()?;
            let anchor = row.slot.checked_add_signed(shift.checked_neg()?)?;
            (start..=end)
                .contains(&anchor)
                .then_some((anchor, transaction))
        })
        // A slot named twice runs its transactions there in order, so the later capture wins.
        .fold(
            BTreeMap::<u64, Vec<String>>::new(),
            |mut by_slot, (anchor, tx)| {
                by_slot.entry(anchor).or_default().push(tx);
                by_slot
            },
        );
    let (slots, transactions): (Vec<u64>, Vec<String>) = anchored
        .into_iter()
        .flat_map(|(slot, txs)| txs.into_iter().map(move |tx| (slot, tx)))
        .unzip();
    (!slots.is_empty()).then(|| ScheduledAction {
        anchor: ActionAnchor::BeforeSlot { slots },
        kind: ActionKind::Simulate,
        transactions,
        account_overrides: AccountModifications::default(),
        feeds_reroute: true,
        return_accounts: vec![account],
        label: Some("venue update".to_string()),
    })
}

/// Move every `price_field` by `shift_bps`. Relative, so the fixed-point scale is irrelevant.
fn reprice(account: &mut AccountData, fields: &[usize], shift_bps: f64) -> Result<Vec<usize>> {
    let mut raw = account
        .data
        .decode()
        .context("decoding captured account data")?;
    let factor = 1.0 + shift_bps / 10_000.0;
    let written = fields.iter().try_fold(Vec::new(), |mut written, &offset| {
        let end = offset + 8;
        ensure!(
            end <= raw.len(),
            "--price-field {offset} is past the account's {} bytes",
            raw.len()
        );
        let value = i64::from_le_bytes(raw[offset..end].try_into().expect("eight bytes"));
        // A mis-aimed offset usually lands on padding; writing there would fake a re-priced venue.
        if value > 0 {
            let moved = (value as f64 * factor).round() as i64;
            raw[offset..end].copy_from_slice(&moved.to_le_bytes());
            written.push(offset);
        }
        Ok(written)
    })?;
    account.data = simulator_api::EncodedBinary::from_bytes(&raw, account.data.encoding);
    Ok(written)
}

/// The counterfactual's payload for the arm `args` asks for. Without `--lag`/`--lead`/
/// `--price-shift-bps` it is empty: the baseline replays the venue as it ran.
fn shifted_schedule(args: &RunArgs) -> Result<Schedule> {
    ensure!(
        args.price_field.is_empty() || args.price_shift_bps.is_some(),
        "--price-field moves a price; pass --price-shift-bps"
    );
    match args.shift() {
        Some(shift) => schedule_for(args, shift, args.price_shift_bps),
        None => {
            ensure!(
                !args.setup_transactions,
                "--setup-transactions carries a shift; pass --lag or --lead"
            );
            Ok(Schedule::default())
        }
    }
}

/// One arm's payload at an explicit `shift` and re-price, so the control can be built from
/// the same capture and the same carrier as the arm it is the reference for.
fn schedule_for(args: &RunArgs, shift: i64, price_shift_bps: Option<f64>) -> Result<Schedule> {
    // The setup carrier rebuilds the action from the captured transaction, which carries the
    // venue's real price: a re-price would be silently dropped and reported as applied.
    ensure!(
        !(args.setup_transactions && price_shift_bps.is_some()),
        "--price-shift-bps cannot be carried by --setup-transactions: the setup replays the \
         venue's own captured update, which re-prices itself. Drop one of the two."
    );
    let path = args
        .capture
        .as_deref()
        .ok_or_else(|| anyhow!("--capture is required with --lag/--lead/--price-shift-bps"))?;
    let mut rows = load_capture_rows(path)?;
    if let Some(shift_bps) = price_shift_bps {
        ensure!(
            !args.price_field.is_empty(),
            "--price-shift-bps needs at least one --price-field"
        );
        let written = rows.iter_mut().try_fold(
            BTreeMap::<usize, usize>::new(),
            |mut written, row| -> Result<_> {
                for offset in reprice(&mut row.account, &args.price_field, shift_bps)? {
                    *written.entry(offset).or_default() += 1;
                }
                Ok(written)
            },
        )?;
        eprintln!(
            "[price] moved {}/{} --price-field(s) by {shift_bps:+} bps over {} states, {} writes",
            written.len(),
            args.price_field.len(),
            rows.len(),
            written.values().sum::<usize>()
        );
        for offset in args
            .price_field
            .iter()
            .filter(|offset| !written.contains_key(offset))
        {
            eprintln!(
                "[price] --price-field {offset} held no positive value in any state and was never written — probably padding"
            );
        }
    }
    let start = args.range.start_slot;
    let end = start + args.range.slot_count;
    let schedule = if !args.setup_transactions {
        Schedule {
            overrides: build_shift_actions(
                shift,
                args.account,
                &rows
                    .into_iter()
                    .map(|row| (row.slot, row.account))
                    .collect(),
                start,
                end,
            ),
            setup: None,
        }
    } else {
        let unresolved = rows.iter().filter(|row| row.transaction.is_none()).count();
        ensure!(
            unresolved < rows.len(),
            "no captured state in {} carries a transaction; capture the range with --no-replay",
            path.display()
        );
        if unresolved > 0 {
            eprintln!(
                "[setup] {unresolved}/{} captured states have no transaction and are skipped",
                rows.len()
            );
        }
        Schedule {
            overrides: Vec::new(),
            setup: build_setup_action(shift, args.account, &rows, start, end),
        }
    };
    // A silently empty schedule runs as a plain baseline while reporting itself as the arm.
    ensure!(
        schedule.entries() > 0,
        "{} built no overrides; does {} cover [{start}, {end}]?",
        shift_label(shift),
        path.display()
    );
    Ok(schedule)
}

/// The venue under test, when one was named.
async fn venue_of(args: &RunArgs) -> Result<Option<Target>> {
    let Some(program) = args.program_id else {
        return Ok(None);
    };
    let label = resolve_venue_label(&program).await?;
    eprintln!("[jup] resolved venue label: {label:?}");
    Ok(Some(Target::new(Some(label), Some(program))))
}

async fn run(args: RunArgs) -> Result<RunOutput> {
    let schedule = shifted_schedule(&args)?;
    let carrier = carrier_of(&args);
    if let Some(shift) = args.shift() {
        eprintln!(
            "[{}] {} anchor slots, carried as {carrier}",
            shift_label(shift),
            schedule.entries(),
        );
    }
    let config = RunConfig {
        range: args.range.clone(),
        schedule,
        filter: filter_from(&args),
        venue: venue_of(&args).await?,
        jsonl_out: Some(args.out.clone()),
        detect_failed_l1_swaps: !args.skip_l1_failures,
        circular_arbs: args.circular_arbs,
        reroute_aggregators: args.reroute_aggregators.clone(),
        shift: args.shift(),
        price_shift_bps: args.price_shift_bps,
        carrier,
        record_full: args.record_full,
    };
    let output = run_once(&args.conn, config).await?;
    report_run("run", &output);
    Ok(output)
}

/// Fields are emptied rather than removed, so every row still reads as a `RerouteNotification`.
/// The header's `slim` flag is what tells a reader the emptiness was deliberate.
fn slimmed(notification: &RerouteNotification) -> RerouteNotification {
    RerouteNotification {
        logs: Vec::new(),
        routed_transaction: EncodedBinary::new(String::new(), BinaryEncoding::Base64),
        ..notification.clone()
    }
}

/// `reroute-out.jsonl` and arm `control` give `reroute-out-control.jsonl`, so the two arms of a
/// comparison sort together.
fn arm_path(base: &Path, arm: &str) -> PathBuf {
    let stem = base.file_stem().map_or_else(
        || base.as_os_str().to_string_lossy(),
        |s| s.to_string_lossy(),
    );
    let named = match base.extension() {
        Some(extension) => format!("{stem}-{arm}.{}", extension.to_string_lossy()),
        None => format!("{stem}-{arm}"),
    };
    base.with_file_name(named)
}

/// How the arm reaches the venue: as the captured bytes, as the venue's own update transaction
/// re-executed at the shifted slot, or not at all.
fn carrier_of(args: &RunArgs) -> &'static str {
    match (args.shift().is_some(), args.setup_transactions) {
        (false, _) => "none",
        (true, true) => "setup transactions",
        (true, false) => "account bytes",
    }
}

const fn is_split(share: u64) -> bool {
    share > 0 && share < FULL_PERCENT
}

/// One leg's contribution to the tally, or `None` when the venue is on neither side of it.
fn leg_counts(leg: &RerouteLegNotification, venue: &Target) -> Option<VenueCounts> {
    let after = venue_share(leg, venue);
    let before = original_venue_share(leg, venue);
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

/// Everything the reroute subscription accumulates, behind one lock.
#[derive(Default)]
struct RerouteCollector {
    venue: Option<Target>,
    record_full: bool,
    legs: BTreeMap<LegKey, LegRecord>,
    tally: VenueTally,
    jsonl: Option<io::BufWriter<fs::File>>,
    write_error: Option<io::Error>,
}

impl RerouteCollector {
    fn record_legs(&mut self, notification: &RerouteNotification) {
        self.legs
            .extend(notification.legs.iter().enumerate().map(|(index, leg)| {
                (
                    (notification.original_signature.clone(), index),
                    LegRecord {
                        input_mint: leg.input_mint.clone(),
                        output_mint: leg.output_mint.clone(),
                        amount: leg.amount,
                        metis_quoted_out: leg.metis_quoted_out,
                        original_quoted_out: leg.original_quoted_out,
                    },
                )
            }));
    }

    /// Both sides over the same legs, so won and lost are differences on one population rather
    /// than two counts from different runs.
    fn tally_venue(&mut self, notification: &RerouteNotification) {
        let Some(venue) = &self.venue else { return };
        let mut matched = 0;
        for (leg, counts) in notification
            .legs
            .iter()
            .filter_map(|leg| Some((leg, leg_counts(leg, venue)?)))
        {
            let direction = (leg.input_mint.clone(), leg.output_mint.clone());
            self.tally
                .by_direction
                .entry(direction)
                .or_default()
                .add(counts);
            matched += counts.legs;
        }
        if matched > 0 {
            self.tally.txs += 1;
        }
    }

    /// The wire type itself, with the unread fields emptied unless the run asked to keep them.
    /// A projection here would silently drop whatever it did not name.
    fn write_jsonl_row(&mut self, notification: &RerouteNotification) {
        if self.record_full {
            self.write_line(|| serde_json::to_string(notification));
            return;
        }
        let slim = slimmed(notification);
        self.write_line(|| serde_json::to_string(&slim));
    }

    fn write_line(&mut self, render: impl FnOnce() -> serde_json::Result<String>) {
        let Some(jsonl) = &mut self.jsonl else {
            return;
        };
        let written = render()
            .map_err(io::Error::from)
            .and_then(|line| writeln!(jsonl, "{line}"));
        if let Err(error) = written {
            self.write_error.get_or_insert(error);
        }
    }
}

async fn run_once(conn: &ConnectionArgs, config: RunConfig) -> Result<RunOutput> {
    let scheduled = config.schedule.entries();
    let slot_count = config.range.slot_count;
    let venue = config.venue.clone();
    let jsonl_out = config.jsonl_out.clone();
    let record_full = config.record_full;
    let header = config.header();
    let create = create_session(config)?;
    let mut session =
        ManagedBacktestSession::start(conn.websocket_url(), conn.api_key.clone(), create).await?;

    let jsonl = jsonl_out
        .map(|path| fs::File::create(path).map(io::BufWriter::new))
        .transpose()?;
    let mut collected = RerouteCollector {
        venue,
        jsonl,
        record_full,
        ..RerouteCollector::default()
    };
    // Written before the session can push a notification, so a truncated run still names its arm.
    collected.write_line(|| serde_json::to_string(&header));
    let collector = Arc::new(Mutex::new(collected));
    let sink = collector.clone();
    let handle = subscribe_reroutes(
        &conn.rpc_url(&session.session_info().rpc_endpoint),
        move |notification: RerouteNotification| {
            let mut collector = sink.lock().expect("reroute collector");
            collector.record_legs(&notification);
            collector.tally_venue(&notification);
            collector.write_jsonl_row(&notification);
            ready(())
        },
    )
    .await?;

    let funnel = drive_to_completion(&mut session, slot_count, |_| {}).await?;
    handle.stop.send(true).ok();
    handle.join_handle.await??;
    session.shutdown().await;

    let mut collected = std::mem::take(&mut *collector.lock().expect("reroute collector"));
    // A run that died before this point leaves no trailer, which is how a reader knows its
    // totals do not cover the whole range.
    if let Some(report) = &funnel {
        let summary = RunSummary::from_report(report);
        collected.write_line(|| serde_json::to_string(&summary));
    }
    if let Some(error) = collected.write_error {
        return Err(error.into());
    }
    if let Some(jsonl) = &mut collected.jsonl {
        jsonl.flush()?;
    }
    Ok(RunOutput {
        scheduled,
        funnel,
        legs: collected.legs,
        venue: collected.tally.report(),
    })
}

fn report_run(label: &str, output: &RunOutput) {
    if let Some(stats) = &output.funnel {
        eprintln!(
            "[{label}] reroute: {} detected -> {} rerouted -> {} simulated -> {} succeeded | {} requote-fail",
            stats.swaps_detected,
            stats.swaps_rerouted,
            stats.swaps_simulated,
            stats.swaps_succeeded,
            stats.requote_failures,
        );
        if stats.override_setup_failures > 0 {
            eprintln!(
                "[{label}] {}/{} scheduled actions failed and posted no state; those slots kept the previous override in force",
                stats.override_setup_failures, output.scheduled
            );
        }
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
            "[{label}] won/lost are differential: read against the `--lag 0` control, not against zero"
        );
        if total.split > 0 {
            eprintln!(
                "[{label}] {} of the venue's legs were split routes it only partly held; held/won/lost count participation, not share",
                total.split
            );
        }
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

async fn compare(args: CompareArgs) -> Result<()> {
    let shift = args.run.shift().ok_or_else(|| {
        anyhow!(
            "--lag/--lead/--price-shift-bps is required for compare; its reference arm is the \
             null control, the same capture posted unmodified at each state's own slot"
        )
    })?;
    let label = shift_label(shift);
    let venue = venue_of(&args.run).await?;
    // Each arm records to its own file, so `report` can read either one afterwards.
    let make_config = |schedule, shift, price_shift_bps, carrier, arm: &str| RunConfig {
        range: args.run.range.clone(),
        schedule,
        filter: filter_from(&args.run),
        venue: venue.clone(),
        jsonl_out: Some(arm_path(&args.run.out, arm)),
        detect_failed_l1_swaps: !args.run.skip_l1_failures,
        circular_arbs: args.run.circular_arbs,
        reroute_aggregators: args.run.reroute_aggregators.clone(),
        shift,
        price_shift_bps,
        carrier,
        record_full: args.run.record_full,
    };

    let schedule = shifted_schedule(&args.run)?;

    eprintln!("[control] running the reroute (no override)...");
    let baseline = run_once(
        &args.run.conn,
        make_config(Schedule::default(), None, None, "none", "control"),
    )
    .await?;
    report_run("control", &baseline);

    eprintln!("[modified] running with {label}...");
    let modified = run_once(
        &args.run.conn,
        make_config(
            schedule,
            Some(shift),
            args.run.price_shift_bps,
            carrier_of(&args.run),
            "modified",
        ),
    )
    .await?;
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

//! Change a venue's state and measure the taker flow it wins or loses.
//!
//! `capture` records the account's per-slot states; `run` posts them back re-priced by
//! `--price-shift-bps`, visible only to the router.

mod cli;
mod jsonl;
mod report;
mod schedule;
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

use anyhow::{Result, anyhow, ensure};
use backtest_example::utils::{
    self,
    capture::{CaptureRow, write_capture},
};
use clap::Parser;
use simulator_api::{RerouteAggregators, RerouteFilter, RerouteStatsReport};
use simulator_client::{
    AccountDiffNotification, CreateSession, FULL_PERCENT, ManagedBacktestSession, ManagedEvent,
    RerouteNotification, backtest_ws_url, reroute_report::Target, subscribe_account_diffs,
    subscribe_reroutes,
};

use crate::{
    cli::{
        CaptureArgs, Cli, Command, CompareArgs, ConnectionArgs, RangeArgs, RunArgs, filter_from,
    },
    jsonl::{FORMAT_VERSION, HeaderKind, RunHeader, RunSummary, wire_transaction},
    report::{
        LegRecord, RerouteCollector, VenueReport, delta_summary, join_legs, report_recording,
        report_run,
    },
    schedule::{Schedule, build_schedule},
    session::{CaptureCollector, create_session},
    venue::resolve_venue_label,
};

pub(crate) type LegKey = (String, usize);

pub(crate) struct RunOutput {
    pub(crate) funnel: Option<RerouteStatsReport>,
    pub(crate) legs: BTreeMap<LegKey, LegRecord>,
    pub(crate) venue: VenueReport,
    pub(crate) scheduled: usize,
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
    price_shift_bps: Option<f64>,
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
            price_shift_bps: self.price_shift_bps,
            override_slots: self.schedule.entries(),
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

async fn capture(args: CaptureArgs) -> Result<()> {
    let conn = &args.conn;
    let create = CreateSession::builder()
        .start_slot(args.range.start_slot)
        .slot_count(args.range.slot_count)
        .replay_account_state(!args.range.no_replay)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .build()
        .into_request()?;
    let mut session =
        ManagedBacktestSession::start(backtest_ws_url(&conn.url), conn.api_key.clone(), create)
            .await?;

    session.subscribe_transactions(vec![args.account]);

    let collector = Arc::new(Mutex::new(CaptureCollector::default()));
    let start_slot = args.range.start_slot;
    let sink = collector.clone();
    let handle = subscribe_account_diffs(
        &session.session_info().rpc_endpoint,
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
    utils::session::drive_to_completion(&mut session, args.range.slot_count, |event| match event {
        ManagedEvent::Slot(slot) => eprintln!("[slot] {slot}"),
        ManagedEvent::Transaction(transaction) => {
            match wire_transaction(&transaction.transaction) {
                Some((signature, encoded)) => {
                    wire.insert(signature, encoded);
                }
                None => undecodable += 1,
            }
        }
        _ => {}
    })
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
            address: Some(args.account),
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
    let schedule = build_schedule(&args)?;
    if let Some(bps) = args.price_shift_bps {
        eprintln!("[{bps:+} bps] {} anchor slots", schedule.entries(),);
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
        price_shift_bps: args.price_shift_bps,
        record_full: args.record_full,
    };
    let output = run_once(&args.conn, config).await?;
    report_run("run", &output);
    Ok(output)
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

pub(crate) const fn is_split(share: u64) -> bool {
    share > 0 && share < FULL_PERCENT
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
        ManagedBacktestSession::start(backtest_ws_url(&conn.url), conn.api_key.clone(), create)
            .await?;

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
        &session.session_info().rpc_endpoint,
        move |notification: RerouteNotification| {
            let mut collector = sink.lock().expect("reroute collector");
            collector.record_legs(&notification);
            collector.tally_venue(&notification);
            collector.write_jsonl_row(&notification);
            ready(())
        },
    )
    .await?;

    let funnel = utils::session::drive_to_completion(&mut session, slot_count, |event| {
        if let ManagedEvent::Slot(slot) = event {
            eprintln!("[slot] {slot}");
        }
    })
    .await?;
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

async fn compare(args: CompareArgs) -> Result<()> {
    let price_shift_bps = args.run.price_shift_bps.ok_or_else(|| {
        anyhow!(
            "--price-shift-bps is required for compare; its reference arm is the control, \
             the same range with no override"
        )
    })?;
    let label = format!("{price_shift_bps:+} bps");
    let venue = venue_of(&args.run).await?;
    // Each arm records to its own file, so `report` can read either one afterwards.
    let make_config = |schedule, price_shift_bps, arm: &str| RunConfig {
        range: args.run.range.clone(),
        schedule,
        filter: filter_from(&args.run),
        venue: venue.clone(),
        jsonl_out: Some(arm_path(&args.run.out, arm)),
        detect_failed_l1_swaps: !args.run.skip_l1_failures,
        circular_arbs: args.run.circular_arbs,
        reroute_aggregators: args.run.reroute_aggregators.clone(),
        price_shift_bps,
        record_full: args.run.record_full,
    };

    let schedule = build_schedule(&args.run)?;

    eprintln!("[control] running the reroute (no override)...");
    let baseline = run_once(
        &args.run.conn,
        make_config(Schedule::default(), None, "control"),
    )
    .await?;
    report_run("control", &baseline);

    eprintln!("[modified] running with {label}...");
    let modified = run_once(
        &args.run.conn,
        make_config(schedule, Some(price_shift_bps), "modified"),
    )
    .await?;
    report_run("modified", &modified);

    let (joined, zero_baseline) = join_legs(&baseline.legs, &modified.legs);
    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| arm_path(&args.run.out, "report"));
    let mut report = io::BufWriter::new(fs::File::create(&report_path)?);
    for row in &joined {
        writeln!(report, "{}", serde_json::to_string(row)?)?;
    }
    report.flush()?;

    let deltas = joined.iter().map(|row| row.delta_bps).collect::<Vec<_>>();
    let stats = delta_summary(&deltas);
    let moved = deltas.iter().filter(|delta| **delta != 0.0).count();
    println!("=== {label} vs the control ===");
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
    println!("report written to {}", report_path.display());
    // Quoting is not deterministic run to run: re-run both sides and trust only the
    // legs that move consistently. See the README.
    eprintln!("[note] single-run deltas include router noise; repeat both runs before attributing");
    Ok(())
}

//! What a run means once it has finished: the per-venue tally, the funnel it prints, and the
//! arm-against-arm comparison.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{self, Write},
    path::Path,
};

use anyhow::{Result, anyhow, bail, ensure};
use simulator_api::{BinaryEncoding, EncodedBinary, route_plan::RoutePlan};
use simulator_client::{
    OriginalNotification, ReplacementNotification, RequoteNotification, RerouteLegNotification,
    reroute_report::{self, LegRecord as FlowRecord, Report, SwapMode, Target, short_mint},
};

use crate::{
    LegKey, RunOutput,
    cli::{ReportArgs, TableArgs},
    is_split,
    jsonl::{FORMAT_VERSION, JoinedLeg, RunHeader, read_recording},
    venue::{original_venue_share, resolve_venue_label, venue_share},
};

#[derive(Clone, Debug)]
pub(crate) struct LegRecord {
    pub(crate) input_mint: String,
    pub(crate) output_mint: String,
    pub(crate) amount: u64,
    pub(crate) metis_quoted_out: u64,
    pub(crate) original_quoted_out: u64,
}

/// The venue's flow before and after re-quoting, over the legs this run actually saw.
#[derive(Default)]
pub(crate) struct VenueTally {
    pub(crate) txs: u64,
    pub(crate) by_direction: HashMap<(String, String), VenueCounts>,
}

/// Every field counts participation in a leg, not the share of it: a venue on a split route
/// counts the same as one holding the whole leg. `split` is the honest qualifier on that.
#[derive(Clone, Copy, Default)]
pub(crate) struct VenueCounts {
    pub(crate) legs: u64,
    pub(crate) l1_legs: u64,
    pub(crate) held: u64,
    pub(crate) improvements: u64,
    pub(crate) split: u64,
    pub(crate) unresolved: u64,
}

impl VenueCounts {
    /// Legs the re-quote took from another venue — including every leg whose L1 route was never
    /// recovered, since only a recovered route can put a leg in `held`.
    pub(crate) const fn won(self) -> u64 {
        self.legs.saturating_sub(self.held)
    }

    /// Legs the venue had on a recovered L1 route and lost to the re-quote.
    pub(crate) const fn lost(self) -> u64 {
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
pub(crate) struct VenueReport {
    pub(crate) txs: u64,
    pub(crate) total: VenueCounts,
    pub(crate) by_direction: Vec<((String, String), VenueCounts)>,
}

impl VenueTally {
    pub(crate) fn report(&self) -> VenueReport {
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

pub(crate) struct DeltaSummary {
    pub(crate) matched: usize,
    pub(crate) median_abs_bps: f64,
    pub(crate) mean_abs_bps: f64,
    pub(crate) p90_abs_bps: f64,
}

/// Reads the recording and nothing else. With no selector it measures the venue the run named.
pub(crate) async fn report_recording(args: ReportArgs) -> Result<()> {
    let recording = read_recording(&args.recording)?;
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

    if let Some(control) = &args.against {
        ensure!(
            !args.json,
            "--against renders the two arms as a table; it has no JSON form"
        );
        println!("{}", render_arms(control, &args.recording, &target)?);
        return Ok(());
    }

    let pairs = recording
        .header
        .as_ref()
        .map(|header| header.filter_pairs.clone())
        .unwrap_or_default();
    let baseline = l1_baseline(&recording.notifications, &target, &pairs);
    let report = reroute_report::from_notifications(target, &recording.notifications)?;
    match args.json {
        true => println!("{}", report.to_json()),
        false => {
            println!(
                "{}",
                report.render(slot_range(&recording.header).as_deref())
            );
            let covered = report
                .to_json()
                .get("total")
                .and_then(|total| total.get("l1"))
                .and_then(|l1| l1.get("swaps"))
                .and_then(serde_json::Value::as_u64);
            let coverage = covered
                .filter(|_| baseline.legs > 0)
                .map(|covered| {
                    format!(
                        "; the table's L1 column covers {covered} of them ({:.0}%)",
                        100.0 * covered as f64 / baseline.legs as f64
                    )
                })
                .unwrap_or_default();
            println!(
                "  venue on L1, over every original replayed: {} swaps in {} transactions{}{}\n",
                baseline.legs,
                baseline.transactions,
                match baseline.unresolved {
                    0 => String::new(),
                    n => format!(" ({n} with no recovered route)"),
                },
                coverage
            );
        }
    }
    Ok(())
}

/// The venue the run named, which a report with no selector measures.
pub(crate) fn target_from_header(header: &RunHeader) -> Option<Target> {
    let program = header
        .program_id
        .as_ref()
        .and_then(|program| program.parse().ok());
    let label = header.label.clone();
    (program.is_some() || label.is_some()).then(|| Target::new(label, program))
}

/// The range the recording knows and the report does not. The slot count carries the sense of
/// scale a bare pair of slot numbers does not.
pub(crate) fn slot_range(header: &Option<RunHeader>) -> Option<String> {
    header.as_ref().map(|header| {
        let slots = header
            .end_slot
            .saturating_sub(header.start_slot)
            .saturating_add(1);
        format!(
            "slots {}–{} ({slots} slots)",
            header.start_slot, header.end_slot
        )
    })
}

/// Both arms of a comparison, rendered together: the venue's own capture is the question a
/// comparison is asked, and reading it means the two arms side by side rather than two separate
/// `report` invocations.
pub(crate) fn render_arms(control: &Path, modified: &Path, target: &Target) -> Result<String> {
    let arm = |path: &Path| -> Result<(String, Option<f64>, L1Baseline, BTreeSet<String>)> {
        let recording = read_recording(path)?;
        let report = reroute_report::from_notifications(target.clone(), &recording.notifications)?;
        let requoted = requoted_usd(&report.to_json());
        Ok((
            report.render(slot_range(&recording.header).as_deref()),
            requoted,
            l1_baseline(
                &recording.notifications,
                target,
                &recording
                    .header
                    .as_ref()
                    .map(|header| header.filter_pairs.clone())
                    .unwrap_or_default(),
            ),
            requoted_signatures(&recording.notifications),
        ))
    };
    let (control_arm, control_usd, control_l1, control_sigs) = arm(control)?;
    let (modified_arm, modified_usd, modified_l1, modified_sigs) = arm(modified)?;
    let marginal = control_usd
        .zip(modified_usd)
        .map(|(control, modified)| marginal_line(control, modified))
        .unwrap_or_default();
    let footing = footing_line(control_l1, modified_l1, &control_sigs, &modified_sigs);
    Ok(format!(
        "\n  \u{2500}\u{2500} control \u{2500}\u{2500}{control_arm}\n  \u{2500}\u{2500} modified \u{2500}\u{2500}{modified_arm}{marginal}{footing}"
    ))
}

/// What the two arms actually share. The L1 baseline has to match or the arms replayed different
/// chains; the re-quoted sets do not, and the marginal above is blind to the difference.
fn footing_line(
    control: L1Baseline,
    modified: L1Baseline,
    control_sigs: &BTreeSet<String>,
    modified_sigs: &BTreeSet<String>,
) -> String {
    let union = control_sigs.union(modified_sigs).count();
    let shared = control_sigs.intersection(modified_sigs).count();
    let drift = union.saturating_sub(shared);
    let footing = match control == modified {
        true => format!(
            "  venue on L1: {} swaps in {} transactions, identical in both arms",
            control.legs, control.transactions
        ),
        false => format!(
            "  venue on L1 DIFFERS between arms: {} vs {} swaps — the arms did not replay the \
             same chain",
            control.legs, modified.legs
        ),
    };
    let coverage = match drift {
        0 => "  both arms re-quoted the same swaps".to_string(),
        _ => format!(
            "  {shared} of {union} re-quoted swaps are common to both arms; {drift} ({:.2}%) are \
             not, and the marginal above cannot see them",
            100.0 * drift as f64 / union.max(1) as f64
        ),
    };
    format!("\n{footing}\n{coverage}\n")
}

fn requoted_usd(json: &serde_json::Value) -> Option<f64> {
    json.get("total")?.get("requoted")?.get("usd")?.as_f64()
}

pub(crate) fn marginal_line(control: f64, modified: f64) -> String {
    let delta = modified - control;
    let ratio = modified / control;
    let times = match ratio.is_finite() {
        true => format!(" ({ratio:.2}x)"),
        false => String::new(),
    };
    format!(
        "\n  the change alone: re-quoted {} -> {}, {}{}{times}\n",
        money(control),
        money(modified),
        if delta >= 0.0 { "+" } else { "-" },
        money(delta.abs()),
    )
}

pub(crate) fn money(usd: f64) -> String {
    match usd {
        usd if usd >= 1e6 => format!("${:.2}M", usd / 1e6),
        usd if usd >= 1e3 => format!("${:.1}k", usd / 1e3),
        usd => format!("${usd:.0}"),
    }
}

pub(crate) fn original_record(notification: &OriginalNotification) -> Option<FlowRecord> {
    let plan = notification.l1_route_plan.as_ref()?;
    let hops = plan.hops();
    let input_mint = hops.first()?.swap_info.input_mint.as_deref()?;
    let output_mint = hops.last()?.swap_info.output_mint.as_deref()?;
    let amount = hops
        .iter()
        .filter(|hop| hop.swap_info.input_mint.as_deref() == Some(input_mint))
        .filter_map(|hop| hop.swap_info.in_amount.as_deref()?.parse::<u64>().ok())
        .sum();
    Some(FlowRecord {
        original_signature: notification.core.original_signature.to_string(),
        input_mint: input_mint.parse().ok()?,
        output_mint: output_mint.parse().ok()?,
        amount,
        swap_mode: SwapMode::ExactIn,
        failed: notification.core.error.is_some(),
        original_failed: notification.core.original_failed,
        realized_output_amount: notification.simulated_output_amount,
        original_realized_output_amount: notification.comparison.l1_fill,
        route_plan: notification.route_plan.clone(),
        original_route_plan: notification.l1_route_plan.clone(),
        realized_route_plan: None,
        template: None,
        quoted_output_amount: None,
        new_output_amount: None,
        error_category: None,
        output_bps: None,
    })
}

pub(crate) fn originals_report(
    notifications: &[ReplacementNotification],
    target: &Target,
    pairs: &[String],
) -> Result<(Report, usize)> {
    let wanted = pairs
        .iter()
        .filter_map(|pair| pair.split_once(','))
        .map(|(base, quote)| (base.to_string(), quote.to_string()))
        .collect::<Vec<_>>();
    let records = notifications
        .iter()
        .filter_map(|notification| match notification {
            ReplacementNotification::Original(original) => original_record(original),
            _ => None,
        })
        .filter(|record| {
            let (input, output) = (
                record.input_mint.to_string(),
                record.output_mint.to_string(),
            );
            wanted.is_empty()
                || wanted.iter().any(|(base, quote)| {
                    (input == *base && output == *quote) || (input == *quote && output == *base)
                })
        })
        .collect::<Vec<_>>();
    let report = Report::from_records(target.clone(), &records)?;
    Ok((report, records.len()))
}

/// The columns one recording contributes: its own arm, and — when `baselines` — the L1 and
/// quote-time columns the originals carry. Shared with `run`, so a table printed at the end of a
/// session and one rendered later from the same file are the same numbers by construction.
pub(crate) fn columns_for(
    recording: &crate::jsonl::Recording,
    target: &Target,
    baselines: bool,
) -> Result<Vec<Column>> {
    let mut columns = Vec::new();
    if baselines {
        let pairs = recording
            .header
            .as_ref()
            .map(|header| header.filter_pairs.clone())
            .unwrap_or_default();
        let (originals, population) = originals_report(&recording.notifications, target, &pairs)?;
        let total = originals.total();
        let share = total.share();
        let mut landed = Column::from_flow("L1 (landed)", &total.l1, share.map(|(l1, _)| l1));
        landed.detected_swaps = population as u64;
        let mut quoted = Column::from_flow(
            "original @ quote",
            &total.requoted,
            share.map(|(_, requoted)| requoted),
        );
        quoted.detected_swaps = population as u64;
        columns.push(landed);
        columns.push(quoted);
    }

    let arm = reroute_report::from_notifications(target.clone(), &recording.notifications)?;
    let total = arm.total();
    let share = total.share();
    let head = match recording.header.as_ref().and_then(|h| h.price_shift_bps) {
        None => "control".to_string(),
        Some(bps) => format!("{bps:+} bps"),
    };
    let mut column = Column::from_flow(&head, &total.requoted, share.map(|(_, requoted)| requoted));
    column.detected_swaps = requoted_signatures(&recording.notifications).len() as u64;
    columns.push(column);
    Ok(columns)
}

/// The table for a recording just written, read back off disk. Reading rather than tallying in
/// flight keeps one implementation: whatever `table` would say later, the run says now.
pub(crate) fn table_for(path: &Path, target: &Target) -> Result<String> {
    let recording = read_recording(path)?;
    Ok(render_table(&columns_for(&recording, target, true)?))
}

pub(crate) async fn report_table(args: TableArgs) -> Result<()> {
    let mut columns = Vec::new();
    for (index, path) in args.recordings.iter().enumerate() {
        let recording = read_recording(path)?;
        let target = match (&args.label, args.program_id) {
            (None, None) => recording
                .header
                .as_ref()
                .and_then(target_from_header)
                .ok_or_else(|| anyhow!("{} names no venue", path.display()))?,
            (Some(label), program) => Target::new(Some(label.clone()), program),
            (None, Some(program)) => {
                Target::new(Some(resolve_venue_label(&program).await?), Some(program))
            }
        };
        columns.extend(columns_for(&recording, &target, index == 0)?);
    }
    println!("{}", render_table(&columns));
    Ok(())
}

pub(crate) struct Column {
    pub(crate) head: String,
    pub(crate) detected_swaps: u64,
    pub(crate) detected_usd: Option<f64>,
    pub(crate) routed_swaps: u64,
    pub(crate) routed_usd: f64,
}

impl Column {
    fn from_flow(head: &str, flow: &reroute_report::Flow, share_pct: Option<f64>) -> Self {
        Self {
            head: head.to_string(),
            detected_swaps: 0,
            detected_usd: share_pct
                .filter(|pct| *pct > 0.0)
                .map(|pct| flow.usd / (pct / 100.0)),
            routed_swaps: flow.legs,
            routed_usd: flow.usd,
        }
    }

    fn swap_pct(&self) -> Option<f64> {
        (self.detected_swaps > 0)
            .then(|| 100.0 * self.routed_swaps as f64 / self.detected_swaps as f64)
    }

    fn volume_pct(&self) -> Option<f64> {
        self.detected_usd
            .filter(|usd| *usd > 0.0)
            .map(|usd| 100.0 * self.routed_usd / usd)
    }
}

pub(crate) fn render_table(columns: &[Column]) -> String {
    let width = 18;
    let cell = |text: String| format!("{text:>width$}");
    let row = |name: &str, values: Vec<String>| {
        format!(
            "  {:<22}{}\n",
            name,
            values.into_iter().map(cell).collect::<String>()
        )
    };
    let money = |usd: Option<f64>| usd.map_or_else(|| "—".to_string(), money_from);
    let pct = |value: Option<f64>| value.map_or_else(|| "—".to_string(), |v| format!("{v:.1}%"));
    let count = |n: u64| match n {
        0 => "—".to_string(),
        n => commas(n),
    };

    let mut out = String::from("\n");
    out.push_str(&row("", columns.iter().map(|c| c.head.clone()).collect()));
    out.push('\n');
    out.push_str(&row(
        "detected swaps",
        columns.iter().map(|c| count(c.detected_swaps)).collect(),
    ));
    out.push_str(&row(
        "detected volume",
        columns.iter().map(|c| money(c.detected_usd)).collect(),
    ));
    out.push_str(&row(
        "routed swaps",
        columns.iter().map(|c| count(c.routed_swaps)).collect(),
    ));
    out.push_str(&row(
        "routed volume",
        columns.iter().map(|c| money(Some(c.routed_usd))).collect(),
    ));
    out.push_str(&row(
        "% swaps to venue",
        columns.iter().map(|c| pct(c.swap_pct())).collect(),
    ));
    out.push_str(&row(
        "% volume to venue",
        columns.iter().map(|c| pct(c.volume_pct())).collect(),
    ));
    out
}

fn money_from(usd: f64) -> String {
    money(usd)
}

fn commas(n: u64) -> String {
    let digits = n.to_string();
    digits
        .chars()
        .enumerate()
        .flat_map(|(i, c)| {
            let sep = (i > 0 && (digits.len() - i).is_multiple_of(3)).then_some(',');
            sep.into_iter().chain(std::iter::once(c))
        })
        .collect()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct L1Baseline {
    pub(crate) transactions: u64,
    pub(crate) legs: u64,
    pub(crate) unresolved: u64,
}

pub(crate) fn l1_baseline(
    notifications: &[ReplacementNotification],
    venue: &Target,
    pairs: &[String],
) -> L1Baseline {
    let wanted = pairs
        .iter()
        .filter_map(|pair| pair.split_once(','))
        .map(|(base, quote)| (base.to_string(), quote.to_string()))
        .collect::<Vec<_>>();
    let in_filter = |hops: &[simulator_api::route_plan::RouteHop]| {
        let (Some(first), Some(last)) = (hops.first(), hops.last()) else {
            return false;
        };
        let (Some(input), Some(output)) = (first.input_mint(), last.output_mint()) else {
            return false;
        };
        wanted.iter().any(|(base, quote)| {
            (input == base && output == quote) || (input == quote && output == base)
        })
    };
    notifications
        .iter()
        .filter_map(|notification| match notification {
            ReplacementNotification::Original(original) => Some(original),
            _ => None,
        })
        .fold(L1Baseline::default(), |mut baseline, original| {
            let Some(hops) = original.l1_route_plan.as_ref().map(RoutePlan::hops) else {
                baseline.unresolved += 1;
                return baseline;
            };
            if !wanted.is_empty() && !in_filter(hops) {
                return baseline;
            }
            let legs = hops.iter().filter(|hop| venue.claims_fill(hop)).count() as u64;
            baseline.transactions += u64::from(legs > 0);
            baseline.legs += legs;
            baseline
        })
}

pub(crate) fn requoted_signatures(notifications: &[ReplacementNotification]) -> BTreeSet<String> {
    notifications
        .iter()
        .filter_map(|notification| match notification {
            ReplacementNotification::Requote(requote) => {
                Some(requote.core.original_signature.to_string())
            }
            _ => None,
        })
        .collect()
}

/// One leg's contribution to the tally, or `None` when the venue is on neither side of it.
pub(crate) fn leg_counts(leg: &RerouteLegNotification, venue: &Target) -> Option<VenueCounts> {
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

pub(crate) fn report_run(label: &str, output: &RunOutput) {
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
            "[{label}] won/lost are differential: read against the control, not against zero"
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

/// Joins legs present in both runs; returns the rows plus the count of matched legs
/// excluded for a zero baseline quote.
pub(crate) fn join_legs(
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

pub(crate) fn delta_summary(deltas: &[f64]) -> DeltaSummary {
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

/// `None` when the baseline quoted zero out — a delta is meaningless there.
pub(crate) fn delta_bps(base: u64, variant: u64) -> Option<f64> {
    (base != 0).then(|| (variant as f64 - base as f64) / base as f64 * 10_000.0)
}

/// Everything the replacement subscription accumulates, behind one lock.
#[derive(Default)]
pub(crate) struct RerouteCollector {
    pub(crate) venue: Option<Target>,
    pub(crate) record_full: bool,
    pub(crate) legs: BTreeMap<LegKey, LegRecord>,
    pub(crate) tally: VenueTally,
    pub(crate) jsonl: Option<io::BufWriter<fs::File>>,
    pub(crate) write_error: Option<io::Error>,
}

impl RerouteCollector {
    pub(crate) fn record_legs(&mut self, notification: &RequoteNotification) {
        self.legs
            .extend(notification.legs.iter().enumerate().map(|(index, leg)| {
                (
                    (notification.core.original_signature.to_string(), index),
                    LegRecord {
                        input_mint: leg.input_mint.to_string(),
                        output_mint: leg.output_mint.to_string(),
                        amount: leg.amount,
                        metis_quoted_out: leg.metis_quoted_out,
                        original_quoted_out: leg.original_quoted_out,
                    },
                )
            }));
    }

    /// Both sides over the same legs, so won and lost are differences on one population rather
    /// than two counts from different runs.
    pub(crate) fn tally_venue(&mut self, notification: &RequoteNotification) -> u64 {
        let Some(venue) = &self.venue else { return 0 };
        let mut matched = 0;
        for (leg, counts) in notification
            .legs
            .iter()
            .filter_map(|leg| Some((leg, leg_counts(leg, venue)?)))
        {
            let direction = (leg.input_mint.to_string(), leg.output_mint.to_string());
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
        matched
    }

    /// The wire type itself, with the unread fields emptied unless the run asked to keep them.
    /// A projection here would silently drop whatever it did not name — including the `kind`
    /// tag, without which the row does not read back as a notification at all.
    pub(crate) fn write_jsonl_row(&mut self, notification: &ReplacementNotification) {
        if self.record_full {
            self.write_line(|| serde_json::to_string(notification));
            return;
        }
        let slim = slimmed(notification);
        self.write_line(|| serde_json::to_string(&slim));
    }

    pub(crate) fn write_line(&mut self, render: impl FnOnce() -> serde_json::Result<String>) {
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

/// Fields are emptied rather than removed, so every row still reads as a
/// [`ReplacementNotification`]. The header's `slim` flag is what tells a reader the emptiness was
/// deliberate. Only a requote carries logs and a routed transaction; the other variants are
/// already slim and pass through untouched.
pub(crate) fn slimmed(notification: &ReplacementNotification) -> ReplacementNotification {
    match notification {
        ReplacementNotification::Requote(requote) => {
            ReplacementNotification::Requote(RequoteNotification {
                logs: Vec::new(),
                routed_transaction: EncodedBinary::new(String::new(), BinaryEncoding::Base64),
                ..requote.clone()
            })
        }
        other => other.clone(),
    }
}

//! What a run means once it has finished: the per-venue tally, the funnel it prints, and the
//! arm-against-arm comparison.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Write},
};

use anyhow::{Result, anyhow, bail};
use simulator_api::{BinaryEncoding, EncodedBinary};
use simulator_client::{
    RerouteLegNotification, RerouteNotification,
    reroute_report::{Report, Target, short_mint},
};

use crate::{
    LegKey, RunOutput,
    cli::ReportArgs,
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
pub(crate) fn target_from_header(header: &RunHeader) -> Option<Target> {
    let program = header
        .program_id
        .as_ref()
        .and_then(|program| program.parse().ok());
    let label = header.label.clone();
    (program.is_some() || label.is_some()).then(|| Target::new(label, program))
}

/// The range the recording knows and the report does not.
pub(crate) fn slot_range(header: &Option<RunHeader>) -> Option<String> {
    header
        .as_ref()
        .map(|header| format!("slots {}–{}", header.start_slot, header.end_slot))
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

/// Joins legs present in both runs; returns the rows plus the count of matched legs
/// excluded for a zero baseline quote.
pub(crate) fn join_legs(
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

/// Everything the reroute subscription accumulates, behind one lock.
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
    pub(crate) fn record_legs(&mut self, notification: &RerouteNotification) {
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
    pub(crate) fn tally_venue(&mut self, notification: &RerouteNotification) {
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
    pub(crate) fn write_jsonl_row(&mut self, notification: &RerouteNotification) {
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

/// Fields are emptied rather than removed, so every row still reads as a `RerouteNotification`.
/// The header's `slim` flag is what tells a reader the emptiness was deliberate.
pub(crate) fn slimmed(notification: &RerouteNotification) -> RerouteNotification {
    RerouteNotification {
        logs: Vec::new(),
        routed_transaction: EncodedBinary::new(String::new(), BinaryEncoding::Base64),
        ..notification.clone()
    }
}

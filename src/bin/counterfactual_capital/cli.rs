//! The clap surface, split out so `main` reads as the counterfactual and not its plumbing.

use std::path::PathBuf;

use anyhow::{Result, ensure};
use clap::Parser;
use serde::{Deserialize, Serialize};

pub(crate) use backtest_example::utils::connection::ConnectionArgs;

/// Capital multiples run when none are given. The arm below the venue's real book is the one that
/// can *lose* fills, which is the only proof the lever is connected at all.
const DEFAULT_MULTIPLES: &[f64] = &[0.1, 1.0, 2.0, 5.0, 10.0, 25.0, 100.0];

/// Ceiling on the arm matrix: each arm is a full replay of the range, so a cross product entered
/// by accident is an overnight run.
const MAX_ARMS: usize = 24;

/// What a capital multiple actually rewrites: `Vaults` funds settlement, `Ladder` widens what the
/// venue will quote, and only `All` is what a desk means by "N times bigger".
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    clap::ValueEnum,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScaleTarget {
    /// Vault balances, their mirrors, and every ladder tier's size.
    #[default]
    All,
    /// Vault balances and their mirrors only — settlement capital, quoting untouched.
    Vaults,
    /// Ladder tier sizes only — quoting capacity, settlement untouched.
    Ladder,
}

impl ScaleTarget {
    const fn label(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Vaults => " vaults",
            Self::Ladder => " ladder",
        }
    }
}

/// One arm: how much capital the venue commits, how much of it it quotes, and how tightly.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArmSpec {
    pub(crate) multiple: f64,
    pub(crate) target: ScaleTarget,
    pub(crate) tighten_bps: f64,
}

impl ArmSpec {
    pub(crate) const CONTROL: Self = Self {
        multiple: 1.0,
        target: ScaleTarget::All,
        tighten_bps: 0.0,
    };

    pub(crate) fn capital(&self) -> f64 {
        match self.target {
            ScaleTarget::All | ScaleTarget::Vaults => self.multiple,
            ScaleTarget::Ladder => 1.0,
        }
    }

    pub(crate) fn depth(&self) -> f64 {
        match self.target {
            ScaleTarget::All | ScaleTarget::Ladder => self.multiple,
            ScaleTarget::Vaults => 1.0,
        }
    }

    /// The triple that actually reaches the venue: two arms spelled differently but writing the
    /// same bytes are one arm, which is what equality and de-duplication key on.
    fn effect(&self) -> (u64, u64, u64) {
        let bits = |value: f64| value.to_bits();
        (
            bits(self.capital()),
            bits(self.depth()),
            bits(self.tighten_bps),
        )
    }

    pub(crate) fn is_control(&self) -> bool {
        self.effect() == Self::CONTROL.effect()
    }

    pub(crate) fn label(&self) -> String {
        if self.is_control() {
            return "1x".to_string();
        }
        let multiple = match self.multiple.fract() == 0.0 {
            true => format!("{:.0}x", self.multiple),
            false => format!("{}x", self.multiple),
        };
        let multiple = match self.multiple == 1.0 {
            true => String::new(),
            false => format!("{multiple}{}", self.target.label()),
        };
        match (multiple.is_empty(), self.tighten_bps == 0.0) {
            (_, true) => multiple,
            (true, false) => format!("-{}bps", self.tighten_bps),
            (false, false) => format!("{multiple} -{}bps", self.tighten_bps),
        }
    }
}

impl PartialEq for ArmSpec {
    fn eq(&self, other: &Self) -> bool {
        self.effect() == other.effect()
    }
}

#[derive(Parser)]
#[command(
    about = "Price a venue against real historical hops with its inventory and its quoting curve \
             scaled, and report the flow each change would have won"
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) conn: ConnectionArgs,

    /// First slot (inclusive) to replay. Required, since a stale default would silently price an
    /// empty population; `sim ranges` lists what exists.
    #[arg(long)]
    pub(crate) start_slot: u64,

    /// Slots to cover, as the inclusive range `[start, start + count]`.
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) slot_count: u64,

    /// Execute transactions instead of replaying recorded account state (replay requires a
    /// recorded `.adlt` for the range).
    #[arg(long, default_value_t = false)]
    pub(crate) no_replay: bool,

    /// The venue plan as JSON: its direct-fill spec (aggregator, venue, pair, account run) and
    /// where its capital and quoting curve live in bytes. See the README on building one.
    #[arg(long, value_name = "PATH")]
    pub(crate) plan: PathBuf,

    /// A capital multiple to run as its own arm: the venue's vaults, its balance mirrors and
    /// every tier of its ladder, scaled by this. Repeatable. Must include 1.0.
    #[arg(long = "multiple", value_name = "K")]
    pub(crate) multiples: Vec<f64>,

    /// What each multiple rewrites. `all` is a venue that is N times bigger; `vaults` funds
    /// settlement without quoting more; `ladder` quotes more without funding it. Repeatable and
    /// crossed with every multiple.
    #[arg(long = "scale", value_enum)]
    pub(crate) scales: Vec<ScaleTarget>,

    /// Basis points to tighten the venue's quoting curve by, moving each ladder tier toward the
    /// other side. Repeatable, and crossed with every capital multiple. `0` leaves the curve as
    /// the venue quoted it.
    #[arg(long = "tighten-bps", value_name = "BPS")]
    pub(crate) tighten_bps: Vec<f64>,

    /// Reuse a captured trajectory instead of replaying the range to record one.
    ///
    /// Absent, every invocation pays a full replay just to record what the venue did. Written when
    /// the file does not exist and read when it does, so a sweep of several targets records once
    /// and replays that recording. A capture is only valid for the range it was taken over.
    #[arg(long, value_name = "PATH")]
    pub(crate) capture: Option<PathBuf>,

    /// Per-arm output.
    #[arg(
        long,
        default_value = "counterfactual-capital.jsonl",
        value_name = "PATH"
    )]
    pub(crate) out: PathBuf,

    /// Print the plan, the arms and the state each would post, then exit without opening a
    /// session.
    #[arg(long, default_value_t = false)]
    pub(crate) dry_run: bool,
}

impl Cli {
    /// The arms to run: every capital multiple crossed with every curve adjustment, control first.
    pub(crate) fn arms(&self) -> Result<Vec<ArmSpec>> {
        let capitals = match self.multiples.is_empty() {
            true => DEFAULT_MULTIPLES.to_vec(),
            false => self.multiples.clone(),
        };
        ensure!(
            capitals.iter().all(|k| k.is_finite() && *k > 0.0),
            "every --multiple must be finite and positive, got {capitals:?}"
        );
        ensure!(
            capitals.contains(&1.0),
            "--multiple must include 1.0: it is the control every other arm is read against, and \
             without it no arm's difference is attributable"
        );

        let tightenings = match self.tighten_bps.is_empty() {
            true => vec![0.0],
            false => self.tighten_bps.clone(),
        };
        ensure!(
            tightenings
                .iter()
                .all(|bps| bps.is_finite() && (0.0..10_000.0).contains(bps)),
            "every --tighten-bps must be finite and within a whole unit, got {tightenings:?}"
        );
        ensure!(
            tightenings.contains(&0.0),
            "--tighten-bps must include 0: an arm that leaves the curve alone is what the \
             tightened arms are read against"
        );

        let targets = match self.scales.is_empty() {
            true => vec![ScaleTarget::All],
            false => self.scales.clone(),
        };

        // Borrowed before the closures so each inner `move` copies a reference, not the vector.
        let (targets, tightenings) = (&targets, &tightenings);
        let mut arms = capitals
            .iter()
            .flat_map(|multiple| {
                targets.iter().flat_map(move |target| {
                    tightenings.iter().map(move |bps| ArmSpec {
                        multiple: *multiple,
                        target: *target,
                        tighten_bps: *bps,
                    })
                })
            })
            .collect::<Vec<_>>();
        arms.sort_by(|a, b| {
            b.is_control()
                .cmp(&a.is_control())
                .then_with(|| a.multiple.total_cmp(&b.multiple))
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| a.tighten_bps.total_cmp(&b.tighten_bps))
        });
        // Every target collapses to the same bytes at a multiple of one.
        arms.dedup();
        ensure!(
            arms.len() <= MAX_ARMS,
            "{} arms is {} replays of the range; narrow --multiple or --tighten-bps (the ceiling \
             is {MAX_ARMS})",
            arms.len(),
            arms.len()
        );
        Ok(arms)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn cli(multiples: &[f64], tighten: &[f64]) -> Cli {
        scaled(multiples, tighten, &[])
    }

    /// `--multiple=<v>` rather than two tokens: a negative multiple is otherwise read as a flag.
    fn scaled(multiples: &[f64], tighten: &[f64], scales: &[&str]) -> Cli {
        Cli::parse_from(
            ["cf"]
                .into_iter()
                .map(String::from)
                .chain([
                    "--api-key=k".to_string(),
                    "--start-slot=1".to_string(),
                    "--plan=p".to_string(),
                ])
                .chain(multiples.iter().map(|m| format!("--multiple={m}")))
                .chain(tighten.iter().map(|b| format!("--tighten-bps={b}")))
                .chain(scales.iter().map(|s| format!("--scale={s}"))),
        )
    }

    #[test]
    fn the_control_sorts_first_so_the_table_reads_down_from_it() {
        let arms = cli(&[10.0, 1.0, 0.1], &[]).arms().expect("valid");
        assert!(arms[0].is_control());
        assert_eq!(arms[1].multiple, 0.1);
        assert_eq!(arms[2].multiple, 10.0);
    }

    #[rstest]
    #[case::everything(ScaleTarget::All, 10.0, 10.0)]
    #[case::settlement_only(ScaleTarget::Vaults, 10.0, 1.0)]
    #[case::quoting_only(ScaleTarget::Ladder, 1.0, 10.0)]
    fn a_scale_target_decides_which_knob_a_multiple_turns(
        #[case] target: ScaleTarget,
        #[case] capital: f64,
        #[case] depth: f64,
    ) {
        let arm = ArmSpec {
            multiple: 10.0,
            target,
            tighten_bps: 0.0,
        };
        assert_eq!(arm.capital(), capital);
        assert_eq!(arm.depth(), depth);
    }

    #[test]
    fn a_unit_multiple_is_the_same_arm_whichever_target_it_names() {
        let unit = |target| ArmSpec {
            multiple: 1.0,
            target,
            tighten_bps: 0.0,
        };
        assert_eq!(unit(ScaleTarget::Vaults), unit(ScaleTarget::Ladder));
        assert!(unit(ScaleTarget::Ladder).is_control());
    }

    #[test]
    fn curve_adjustments_cross_every_capital_multiple() {
        let arms = cli(&[1.0, 10.0], &[0.0, 2.0]).arms().expect("valid");
        assert_eq!(arms.len(), 4);
        assert_eq!(arms.iter().filter(|arm| arm.tighten_bps == 2.0).count(), 2);
    }

    #[rstest]
    #[case::no_unit_multiple(&[2.0, 10.0], &[], "must include 1.0")]
    #[case::no_zero_tightening(&[1.0], &[2.0], "must include 0")]
    #[case::negative_multiple(&[1.0, -1.0], &[], "finite and positive")]
    fn a_sweep_without_its_own_baseline_is_refused(
        #[case] multiples: &[f64],
        #[case] tighten: &[f64],
        #[case] expected: &str,
    ) {
        let error = cli(multiples, tighten).arms().expect_err("must be refused");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test]
    fn every_scale_target_crosses_the_multiples_and_shares_one_control() {
        let arms = scaled(&[1.0, 100.0], &[], &["all", "vaults", "ladder"])
            .arms()
            .expect("valid");
        assert_eq!(
            arms.len(),
            4,
            "{:?}",
            arms.iter().map(ArmSpec::label).collect::<Vec<_>>()
        );
        assert_eq!(arms.iter().filter(|arm| arm.is_control()).count(), 1);
        let labels = arms.iter().map(ArmSpec::label).collect::<Vec<_>>();
        assert_eq!(labels, ["1x", "100x", "100x vaults", "100x ladder"]);
    }

    #[test]
    fn an_accidental_cross_product_is_refused_before_it_runs_overnight() {
        let many = (1..=10).map(f64::from).collect::<Vec<_>>();
        let bps = [0.0, 1.0, 2.0, 3.0];
        let error = cli(&many, &bps)
            .arms()
            .expect_err("40 arms must be refused");
        assert!(
            error.to_string().contains("replays of the range"),
            "{error}"
        );
    }
}

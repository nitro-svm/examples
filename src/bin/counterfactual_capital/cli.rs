//! The clap surface, split out so `main` reads as the counterfactual and not its plumbing.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use simulator_api::DirectFillParams;
use solana_address::Address;

pub(crate) use backtest_example::utils::connection::ConnectionArgs;

/// The multiples run when none are given.
///
/// Spread across three orders of magnitude BELOW the venue's real book as well as above it,
/// because a venue that is not capital-constrained on the flow answers every arm above 1x
/// identically — the curve only bends where capital starts binding, and for a deep venue that is
/// far below what it actually holds.
const DEFAULT_MULTIPLES: &[f64] = &[0.001, 0.01, 0.1, 1.0, 10.0];

#[derive(Parser)]
#[command(
    about = "Price a venue against real historical hops with its inventory scaled up, and report \
             the flow more capital would have won"
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) conn: ConnectionArgs,

    /// First slot (inclusive) to replay. Required: a stale default silently prices an empty
    /// population, which reads like a result. `curl <url>/available-ranges` lists what exists.
    #[arg(long)]
    pub(crate) start_slot: u64,

    /// Slots to cover, as the inclusive range `[start, start + count]`.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) slot_count: u64,

    /// Execute transactions instead of replaying recorded account state (replay requires a
    /// recorded `.adlt` for the range).
    #[arg(long, default_value_t = false)]
    pub(crate) no_replay: bool,

    /// The venue's direct-fill spec as JSON: aggregator, venue, pair, slippageBps and the
    /// venue's own market (its mint ordering and account run). The same shape `sim run
    /// --direct-fill` takes. Nothing can derive the account run — see the README on harvesting
    /// it from a landed route.
    #[arg(long)]
    pub(crate) market: PathBuf,

    /// A venue account whose balance is its committed inventory, scaled by each multiple.
    /// Repeatable, and every one must appear in the spec's account run as writable.
    #[arg(long = "vault", required = true)]
    pub(crate) vaults: Vec<Address>,

    /// A capital multiple to run as its own arm. Repeatable. Must include 1.0, the control every
    /// other arm is read against.
    #[arg(long = "multiple")]
    pub(crate) multiples: Vec<f64>,

    /// Per-arm output.
    #[arg(long, default_value = "counterfactual-capital.jsonl")]
    pub(crate) out: PathBuf,

    /// Print the spec, the arms and the scaled amounts, then exit without opening a session.
    #[arg(long, default_value_t = false)]
    pub(crate) dry_run: bool,
}

impl Cli {
    /// The arms to run, control first so the table reads down from it, then ascending. A
    /// multiple below 1.0 is the falsification arm and stays where it sorts.
    pub(crate) fn arms(&self) -> Result<Vec<f64>> {
        let mut arms = match self.multiples.is_empty() {
            true => DEFAULT_MULTIPLES.to_vec(),
            false => self.multiples.clone(),
        };
        ensure!(
            arms.iter().all(|m| m.is_finite() && *m > 0.0),
            "every --multiple must be finite and positive, got {arms:?}"
        );
        ensure!(
            arms.contains(&1.0),
            "--multiple must include 1.0: it is the control every other arm is read against, and \
             without it no arm's difference is attributable"
        );
        arms.sort_by(|a, b| a.partial_cmp(b).expect("finite multiples compare"));
        arms.dedup();
        Ok(arms)
    }

    /// The venue spec, checked against the vaults named on the command line.
    ///
    /// Both checks fail the run before it opens a session, because both cost a whole replay to
    /// discover otherwise: an account outside the run is never loaded into the probe, and one the
    /// venue does not write reverts at execution rather than at build.
    pub(crate) fn spec(&self) -> Result<DirectFillParams> {
        let raw = std::fs::read_to_string(&self.market)
            .with_context(|| format!("reading the venue spec at {}", self.market.display()))?;
        let spec: DirectFillParams = serde_json::from_str(&raw)
            .with_context(|| format!("parsing the venue spec at {}", self.market.display()))?;

        for vault in &self.vaults {
            let account = spec
                .market
                .accounts
                .iter()
                .find(|account| account.address == *vault)
                .with_context(|| {
                    format!(
                        "--vault {vault} is not in the spec's account run, so the probe never \
                         loads it and scaling it would change nothing"
                    )
                })?;
            ensure!(
                account.writable,
                "--vault {vault} is read-only in the spec's account run; a venue that writes to \
                 it will revert at execution, so the run is mis-specified"
            );
        }
        Ok(spec)
    }
}

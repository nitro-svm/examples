//! Turning the arms' rows into the run's stdout: the table, the caveats that decide how to read
//! it, and the guards that separate a real curve from a broken plan.

use anyhow::Result;

use crate::{
    Venue,
    cli::{self, ArmSpec},
    jsonl::{ArmRow, FILLED},
    plan::Plan,
    vault,
};

/// Above this, an arm that should have reproduced the reference exactly did not. Half the venue's
/// own half-spread.
const REFERENCE_GAP_WARN_BPS: f64 = 0.5;

fn format_amount(base_units: u64, decimals: u8) -> String {
    let scaled = base_units as f64 / 10f64.powi(i32::from(decimals));
    match scaled >= 1000.0 {
        true => format!("{scaled:.0}"),
        false => format!("{scaled:.2}"),
    }
}

fn short(address: &str) -> String {
    match address.len() > 10 {
        true => format!("{}..{}", &address[..6], &address[address.len() - 4..]),
        false => address.to_string(),
    }
}

pub(crate) fn baseline(venue: &Venue) {
    for vault in &venue.vaults {
        // Read rather than indexed: nothing has validated this buffer as a token account yet.
        let Some(amount) = vault::amount(&vault.account) else {
            eprintln!(
                "[warn] {} is {} bytes, too short to be a token account",
                short(&vault.address.to_string()),
                vault.account.data.len()
            );
            continue;
        };
        eprintln!(
            "[baseline] {} holds {} of {}",
            short(&vault.address.to_string()),
            format_amount(amount, vault.decimals),
            short(&vault.mint.to_string())
        );
    }
}

/// What the run would do, without opening a session; the plan is validated before this prints.
pub(crate) fn dry_run(arms: &[ArmSpec], plan: &Plan) -> Result<()> {
    println!(
        "{} arms against {} via {}:",
        arms.len(),
        plan.direct_fill.venue,
        plan.direct_fill.aggregator.as_str()
    );
    for arm in arms {
        println!(
            "  {:>16}  vaults x{}  ladder sizes x{}  prices -{} bps",
            arm.label(),
            arm.capital(),
            arm.depth(),
            arm.tighten_bps
        );
    }
    println!();
    for vault in &plan.inventory.vaults {
        println!("would rewrite the balance of vault {vault}");
    }
    let Some(state) = &plan.inventory.state else {
        println!("no state account named: only the vaults would be rewritten");
        return Ok(());
    };
    println!(
        "would rewrite {} ladder(s) and {} balance mirror(s) in {}",
        state.ladders.len(),
        state.balance_mirrors.len(),
        state.account
    );
    Ok(())
}

/// The table, and every guard that separates a real curve from a broken one.
pub(crate) fn arms(rows: &[ArmRow], reference: Option<&ArmRow>, venue: &Venue) {
    let Some(control) = rows
        .iter()
        .find(|row| row.multiple == 1.0 && row.tighten_bps == 0.0)
    else {
        println!("No control arm ran, so nothing below is attributable.");
        return;
    };

    println!();
    println!(
        "[run] {} hops matched the venue's pair, {} were buildable — the denominator below",
        control.matched, control.built
    );
    println!();
    let base_decimals = venue.vaults.first().map_or(0, |vault| vault.decimals);
    println!(
        "  {:>16}  {:>24}  {:>13}  {:>7}  {:>7}  {:>9}  {:>9}",
        "arm", "vaults posted", "max trade", "won", "filled", "mean bps", "vs 1x"
    );
    for row in rows {
        let capital = row
            .vaults
            .iter()
            .map(|vault| format_amount(vault.after, vault.decimals))
            .collect::<Vec<_>>()
            .join(" / ");
        let mean = row
            .mean_bps()
            .map_or_else(|| "-".to_string(), |bps| format!("{bps:+.1}"));
        let delta = match (row.mean_bps(), control.mean_bps()) {
            (Some(bps), Some(base)) if row.multiple != 1.0 || row.tighten_bps != 0.0 => {
                format!("{:+.1}", bps - base)
            }
            _ => "-".to_string(),
        };
        let ceiling = row
            .ceiling
            .as_deref()
            .and_then(|top| top.parse::<u128>().ok())
            .and_then(|top| u64::try_from(top).ok())
            .map_or_else(|| "-".to_string(), |top| format_amount(top, base_decimals));
        println!(
            "  {:>16}  {capital:>24}  {ceiling:>13}  {:>7}  {:>6.1}%  {mean:>9}  {delta:>9}",
            label_of(row),
            row.scored,
            row.fill_rate() * 100.0
        );
    }
    println!();

    population(rows, control);
    refusals(rows);
    reference_check(control, reference);
    guards(rows, control, venue);
}

/// An arm's label, rebuilt from the row — including which knob the multiple turned.
fn label_of(row: &ArmRow) -> String {
    ArmSpec {
        multiple: row.multiple,
        target: row.scale,
        tighten_bps: row.tighten_bps,
    }
    .label()
}

/// Each arm's mean is taken over the hops that arm filled, so the arms score different populations.
fn population(rows: &[ArmRow], control: &ArmRow) {
    if rows.iter().all(|row| row.scored == control.scored) {
        return;
    }
    // The spread across arms: naming the control twice would print the caveat as a tautology.
    let (Some(fewest), Some(most)) = (
        rows.iter().min_by_key(|row| row.scored),
        rows.iter().max_by_key(|row| row.scored),
    ) else {
        return;
    };
    println!(
        "Read `mean bps` and `vs 1x` across arms with care: the arms score different populations \
         ({} hops at {}, {} at {}). A larger arm wins the hops a smaller one refused, and a venue \
         refuses what its curve prices worst — so the mean can fall while the flow won rises. The \
         `won` column is the like-for-like number.",
        fewest.scored,
        label_of(fewest),
        most.scored,
        label_of(most)
    );
    println!();
}

/// Why the missing flow was missed: the venue's own refusals behind the fill column.
fn refusals(rows: &[ArmRow]) {
    // No outcomes at all means no probe was ever simulated, not a venue that filled everything.
    if rows.iter().all(|row| row.outcomes.is_empty()) {
        println!("No probe was simulated, so the venue was never asked to fill anything.");
        return;
    }
    if rows.iter().all(|row| row.refusals().is_empty()) {
        println!("Every buildable hop was filled by every arm.");
        return;
    }
    println!("Why the rest were not filled:");
    for row in rows {
        let refusals = row.refusals();
        let summary = match refusals.is_empty() {
            true => "none — the venue filled everything it was offered".to_string(),
            false => refusals
                .iter()
                .map(|(key, count)| format!("{key} {count}"))
                .collect::<Vec<_>>()
                .join(", "),
        };
        println!("  {:>16}  {summary}", label_of(row));
    }
    println!();
}

/// The control rewrites the venue with its own bytes, so it must land on the reference's number;
/// a gap is the capture having missed a change, which every arm then inherits.
fn reference_check(control: &ArmRow, reference: Option<&ArmRow>) {
    let Some(reference) = reference else {
        println!(
            "[diag] unavailable: the trajectory was read from a capture, so this run has no \
             reference pass of its own to check the 1x arm against"
        );
        println!();
        return;
    };
    let (Some(measured), Some(reference)) = (control.mean_bps(), reference.mean_bps()) else {
        println!(
            "[warn] the control or the reference pass scored nothing, so the capture cannot be \
             checked. Read 'vs 1x'."
        );
        return;
    };
    let gap = measured - reference;
    println!(
        "[diag] reference: {reference:+.1} bps, 1x arm following the same trajectory: \
         {measured:+.1} — gap {gap:+.1} bps"
    );
    if gap.abs() > REFERENCE_GAP_WARN_BPS {
        println!(
            "[warn] a gap this size means the capture missed changes, and every arm inherits the \
             same hole — treat the table as unsound"
        );
    }
    println!();
}

fn guards(rows: &[ArmRow], control: &ArmRow, venue: &Venue) {
    // Detection does not read overrides, so the admitted population is a property of the range.
    if let Some(drift) = rows
        .iter()
        .find(|row| row.matched != control.matched || row.built != control.built)
    {
        println!(
            "[warn] the {} arm admitted {}/{} hops against the control's {}/{}; detection does not \
             read overrides, so the arms are not measuring one population",
            label_of(drift),
            drift.matched,
            drift.built,
            control.matched,
            control.built
        );
    }

    // A venue that quotes from a ladder does not move when only its vaults are funded, so a flat
    // line here can be the finding rather than a fault.
    if rows.len() > 1 && rows.iter().all(|row| row.outcomes == control.outcomes) {
        let vaults_only = rows
            .iter()
            .all(|row| row.multiple == 1.0 || row.scale == cli::ScaleTarget::Vaults);
        let remedy = match (vaults_only, venue.state.is_some()) {
            (true, true) => {
                "this run scaled vaults only, and this venue does not price off its \
                             vault balances — that is the finding, not a fault. Re-run with \
                             --scale ladder or --scale all to move what it does price off"
            }
            (true, false) => {
                "this run scaled vaults only and the plan names no state account, so \
                              nothing that could change the venue's quotes was touched"
            }
            (false, true) => {
                "every knob was turned and nothing moved, so this venue prices from \
                              state the plan does not describe"
            }
            (false, false) => {
                "the plan scales vaults only. A venue that quotes from a ladder \
                               ignores its vault balances entirely — add its state layout"
            }
        };
        println!("[warn] every arm's outcomes are identical to the control's: {remedy}.");
    }

    // The falsification arm: less capital that does not cost fills means the lever is inert.
    if let Some(down) = rows.iter().find(|row| row.multiple < 1.0)
        && down.scored >= control.scored
    {
        println!(
            "[warn] the {} arm filled {} against the control's {} — cutting the venue down did not \
             cost fills, so the lever is not live and no arm above the control is attributable",
            label_of(down),
            down.scored,
            control.scored
        );
    }

    // Titan writes a zero minimum-out, so a route can succeed and deliver nothing; the scorer
    // counts that as a fill at exactly -10000 bps.
    if let Some(empty) = rows.iter().find(|row| {
        row.mean_bps()
            .is_some_and(|bps| (bps + 10_000.0).abs() < 1.0)
    }) {
        println!(
            "[warn] the {} arm's mean bps is -10000, the signature of routes that succeeded and \
             delivered nothing; the venue's mint ordering in the plan is probably reversed",
            label_of(empty)
        );
    }

    // The machine-readable answer: the smallest arm that filled everything, or 0. By value, not
    // row order — the control sorts first and would answer 1x whenever it saturates.
    let saturated = rows
        .iter()
        .filter(|row| row.built > 0 && row.outcome(FILLED) == row.built)
        .map(|row| row.multiple)
        .min_by(f64::total_cmp)
        .unwrap_or(0.0);
    println!("{saturated}");
}

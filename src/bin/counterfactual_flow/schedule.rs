//! Turning a capture into the override schedule an arm posts: the re-priced state to publish at
//! each slot the venue wrote.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail, ensure};
use backtest_example::utils::capture::load_capture;
use simulator_api::{AccountData, AccountModifications};

use solana_address::Address;

use crate::cli::RunArgs;

/// Empty is the baseline: the venue replays as it ran.
#[derive(Default)]
pub(crate) struct Schedule {
    /// The re-priced bytes, one entry per slot the venue wrote.
    pub(crate) overrides: Vec<(u64, AccountModifications)>,
}

impl Schedule {
    /// Slots the schedule posts at.
    pub(crate) fn entries(&self) -> usize {
        self.overrides.len()
    }
}

/// One override per captured slot, each publishing that slot's own state.
pub(crate) fn build_overrides(
    account: Address,
    captured: &BTreeMap<u64, AccountData>,
    start: u64,
    end: u64,
) -> Vec<(u64, AccountModifications)> {
    captured
        .range(start..=end)
        .map(|(slot, state)| {
            (
                *slot,
                AccountModifications(BTreeMap::from([(account, state.clone())])),
            )
        })
        .collect()
}

/// Move every `price_field` by `shift_bps`. Relative, so the fixed-point scale is irrelevant.
pub(crate) fn reprice(
    account: &mut AccountData,
    fields: &[usize],
    shift_bps: f64,
) -> Result<Vec<usize>> {
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

/// The counterfactual's payload for the arm `args` asks for. Without `--price-shift-bps` it is
/// empty: the baseline replays the venue as it ran.
pub(crate) fn build_schedule(args: &RunArgs) -> Result<Schedule> {
    ensure!(
        args.price_field.is_empty() || args.price_shift_bps.is_some(),
        "--price-field moves a price; pass --price-shift-bps"
    );
    match args.price_shift_bps {
        Some(price_shift_bps) => schedule_for(args, price_shift_bps),
        None => Ok(Schedule::default()),
    }
}

/// The arm's payload: every captured state re-priced and published at its own slot.
pub(crate) fn schedule_for(args: &RunArgs, price_shift_bps: f64) -> Result<Schedule> {
    let path = args
        .capture
        .as_deref()
        .ok_or_else(|| anyhow!("--capture is required with --price-shift-bps"))?;
    let mut rows = load_capture(path)?;
    // A capture recorded before rows named their account has none to check, so it is taken on trust.
    if let Some(recorded) = rows.iter().find_map(|row| row.address)
        && recorded != args.account
    {
        bail!(
            "{} records account {recorded}, but this run rewrites {}. Posting one account's \
             states onto another prices the venue against bytes it never held",
            path.display(),
            args.account
        );
    }
    ensure!(
        !args.price_field.is_empty(),
        "--price-shift-bps needs at least one --price-field"
    );
    let written = rows.iter_mut().try_fold(
        BTreeMap::<usize, usize>::new(),
        |mut written, row| -> Result<_> {
            for offset in reprice(&mut row.account, &args.price_field, price_shift_bps)? {
                *written.entry(offset).or_default() += 1;
            }
            Ok(written)
        },
    )?;
    eprintln!(
        "[price] moved {}/{} --price-field(s) by {price_shift_bps:+} bps over {} states, {} writes",
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
    let start = args.range.start_slot;
    let end = start + args.range.slot_count;
    let schedule = Schedule {
        overrides: build_overrides(
            args.account,
            &rows
                .into_iter()
                .map(|row| (row.slot, row.account))
                .collect(),
            start,
            end,
        ),
    };
    // A silently empty schedule runs as a plain baseline while reporting itself as the arm.
    ensure!(
        schedule.entries() > 0,
        "{} built no overrides; does it cover [{start}, {end}]?",
        path.display()
    );
    Ok(schedule)
}

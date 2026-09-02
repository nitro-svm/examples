//! Turning a capture into the override schedule an arm posts: which state to anchor at each slot,
//! whether to carry it as bytes or as the venue's own update transaction, and the price rewrite.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail, ensure};
use backtest_example::utils::capture::{CaptureRow, load_capture};
use simulator_api::{AccountData, AccountModifications, ActionAnchor, ActionKind, ScheduledAction};

use solana_address::Address;

use crate::cli::{RunArgs, shift_label};

/// Empty on both counts is the baseline; the carriers are alternatives, never
/// combined.
#[derive(Default)]
pub(crate) struct Schedule {
    /// Captured bytes, one entry per anchor slot.
    pub(crate) overrides: Vec<(u64, AccountModifications)>,
    /// The venue's own update transactions, simulated at their shifted slots; the account
    /// state each leaves behind is published in place of the bytes.
    pub(crate) setup: Option<ScheduledAction>,
}

impl Schedule {
    /// Anchor slots the schedule posts at, either way it carries them.
    pub(crate) fn entries(&self) -> usize {
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

pub(crate) fn nearest_capture_at_or_before(
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
pub(crate) fn build_shift_actions(
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
pub(crate) fn build_setup_action(
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

/// The counterfactual's payload for the arm `args` asks for. Without `--lag`/`--lead`/
/// `--price-shift-bps` it is empty: the baseline replays the venue as it ran.
pub(crate) fn shifted_schedule(args: &RunArgs) -> Result<Schedule> {
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
pub(crate) fn schedule_for(
    args: &RunArgs,
    shift: i64,
    price_shift_bps: Option<f64>,
) -> Result<Schedule> {
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

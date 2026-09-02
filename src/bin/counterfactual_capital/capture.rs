//! Capturing the venue's real trajectory: the account-diff subscription, the per-slot changes it folds into, and the walk that turns them into one override snapshot per change.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use backtest_example::utils::capture::{
    CaptureRow, Collected, account_data, collect_account_diffs, load_capture, write_capture,
};
use solana_account::Account;
use solana_address::Address;
use std::path::Path;

/// One account's state after a change, and the slot it changed in.
type Change = (u64, Address, Account);

/// What the subscription folds into: the changes seen, and any diff it could not decode.
#[derive(Default)]
pub(crate) struct Seen {
    changes: Vec<Change>,
    undecodable: u64,
}

/// Every change to the venue's accounts over the range, in slot order.
#[derive(Debug, Default)]
pub(crate) struct Trajectory {
    pub(crate) changes: BTreeMap<u64, HashMap<Address, Account>>,
}

impl Trajectory {
    /// The number of overrides an arm will post: one per slot that carries a change.
    pub(crate) fn slots(&self) -> usize {
        self.changes.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Subscribe to every account the plan will rewrite, before the range advances.
///
/// A dropped websocket is fatal rather than reconnected: a missed change would ride on as a stale
/// override at every later slot, undetectably.
pub(crate) async fn start(rpc_url: &str, accounts: &[Address]) -> Result<Collected<Seen>> {
    collect_account_diffs(rpc_url, accounts, Seen::default(), |seen, routed| {
        let Some(Ok(data)) = routed.notification.post_account_data() else {
            seen.undecodable += 1;
            return;
        };
        let (Ok(account), Ok(address)) = (data.to_account(), routed.account.parse()) else {
            seen.undecodable += 1;
            return;
        };
        seen.changes
            .push((routed.notification.context.slot, address, account));
    })
    .await
}

/// Fold what the capture saw into a slot-keyed trajectory.
///
/// A diff that failed to decode is fatal rather than skipped: the change it carried would not be
/// posted, so the previous override would stay standing and every later slot would price against
/// a stale value.
pub(crate) async fn finish(capture: Collected<Seen>) -> Result<Trajectory> {
    let seen = capture.finish().await?;
    if seen.undecodable > 0 {
        bail!(
            "{} account diff(s) could not be decoded, so the captured trajectory is missing \
             changes and every arm built from it would price against stale state",
            seen.undecodable
        );
    }
    let mut trajectory = Trajectory::default();
    for (slot, address, account) in seen.changes {
        trajectory
            .changes
            .entry(slot)
            .or_default()
            .insert(address, account);
    }
    Ok(trajectory)
}

/// Each slot gets what changed there and the venue's full state after the change: the schedule
/// folds forward, so only the changed accounts need re-posting, but scaling the state account
/// asserts its balance mirrors against vaults that may not have moved in that slot.
pub(crate) fn walk<'a>(
    start: &HashMap<Address, Account>,
    trajectory: &'a Trajectory,
) -> impl Iterator<Item = (u64, Vec<Address>, HashMap<Address, Account>)> + 'a {
    let mut current = start.clone();
    trajectory.changes.iter().map(move |(slot, changed)| {
        current.extend(changed.iter().map(|(key, value)| (*key, value.clone())));
        (*slot, changed.keys().copied().collect(), current.clone())
    })
}

/// An empty trajectory posts no overrides and silently measures the control, and one naming an
/// account the plan does not own is a capture of some other venue: posting it would price this one
/// against bytes it never held.
pub(crate) fn require_changes(trajectory: &Trajectory, accounts: &[Address]) -> Result<()> {
    if trajectory.is_empty() {
        bail!(
            "the capture pass saw no change to any of the venue's {} accounts across the range. \
             Either the venue did not trade, or the subscription missed them — and an arm built \
             from an empty trajectory posts nothing and silently measures the control",
            accounts.len()
        );
    }
    if let Some(foreign) = trajectory
        .changes
        .values()
        .flat_map(HashMap::keys)
        .find(|address| !accounts.contains(address))
    {
        bail!(
            "the capture names account {foreign}, which this plan does not override. It was \
             recorded for a different venue, and replaying it would price this one against bytes \
             it never held"
        );
    }
    Ok(())
}

/// Persist a trajectory so a later sweep can replay it without paying for the reference pass.
pub(crate) fn save(path: &Path, trajectory: &Trajectory) -> Result<()> {
    let rows = trajectory
        .changes
        .iter()
        .flat_map(|(slot, accounts)| {
            accounts.iter().map(move |(address, account)| CaptureRow {
                slot: *slot,
                address: Some(*address),
                account: account_data(account),
                signature: None,
                transaction: None,
            })
        })
        .collect::<Vec<_>>();
    write_capture(path, &rows)
}

/// Read a trajectory back. Every row must name its account: a capture written for one account
/// cannot say which of this venue's several a state belongs to, and guessing would post the wrong
/// bytes.
pub(crate) fn load(path: &Path) -> Result<Trajectory> {
    let mut trajectory = Trajectory::default();
    for row in load_capture(path)? {
        let address = row.address.context(
            "a capture row names no account, so it cannot be replayed against a venue with several",
        )?;
        trajectory
            .changes
            .entry(row.slot)
            .or_default()
            .insert(address, row.account.to_account()?);
    }
    Ok(trajectory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::from([byte; 32])
    }

    fn account(lamports: u64) -> Account {
        Account {
            lamports,
            data: vec![lamports as u8],
            owner: addr(9),
            executable: false,
            rent_epoch: 0,
        }
    }

    fn trajectory(entries: &[(u64, &[(u8, u64)])]) -> Trajectory {
        Trajectory {
            changes: entries
                .iter()
                .map(|(slot, changed)| {
                    (
                        *slot,
                        changed
                            .iter()
                            .map(|(key, lamports)| (addr(*key), account(*lamports)))
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn a_snapshot_carries_accounts_that_did_not_change_in_that_slot() {
        let start = [(addr(1), account(10)), (addr(2), account(20))]
            .into_iter()
            .collect();
        let walked =
            walk(&start, &trajectory(&[(100, &[(1, 11)]), (200, &[(2, 22)])])).collect::<Vec<_>>();

        assert_eq!(walked[0].0, 100);
        assert_eq!(
            walked[0].2[&addr(1)].lamports,
            11,
            "the changed account moved"
        );
        assert_eq!(
            walked[0].2[&addr(2)].lamports,
            20,
            "the untouched one carried"
        );
        assert_eq!(
            walked[1].2[&addr(1)].lamports,
            11,
            "and stays carried forward"
        );
        assert_eq!(walked[1].2[&addr(2)].lamports, 22);
    }

    #[test]
    fn only_the_accounts_that_moved_are_named_for_that_slot() {
        let start = [(addr(1), account(10)), (addr(2), account(20))]
            .into_iter()
            .collect();
        let walked = walk(&start, &trajectory(&[(100, &[(1, 11)])])).collect::<Vec<_>>();
        assert_eq!(walked[0].1, vec![addr(1)]);
    }

    #[test]
    fn a_capture_of_another_venue_is_refused() {
        let error = require_changes(&trajectory(&[(100, &[(1, 11)])]), &[addr(2)])
            .expect_err("a trajectory naming an unowned account must be refused");
        assert!(error.to_string().contains("different venue"), "{error}");
    }

    #[test]
    fn an_empty_capture_is_refused_rather_than_measured() {
        let error = require_changes(&Trajectory::default(), &[addr(1)])
            .expect_err("an empty trajectory must be refused");
        assert!(
            error.to_string().contains("silently measures the control"),
            "{error}"
        );
    }
}

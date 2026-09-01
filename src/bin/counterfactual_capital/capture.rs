//! Capturing the venue's real trajectory: the account-diff subscription, the per-slot changes it folds into, and the walk that turns them into one override snapshot per change.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use simulator_client::{
    AccountDiffSubscriptionHandle, subscribe_account_diffs_many,
};
use solana_account::Account;
use solana_address::Address;

/// One account's state after a change, and the slot it changed in.
type Change = (u64, Address, Account);

/// A running capture. Notifications land here until [`Capture::finish`] takes them.
pub(crate) struct Capture {
    changes: Arc<Mutex<Vec<Change>>>,
    handle: AccountDiffSubscriptionHandle,
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
pub(crate) async fn start(rpc_url: &str, accounts: &[Address]) -> Result<Capture> {
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = changes.clone();
    let handle = subscribe_account_diffs_many(
        rpc_url,
        accounts.iter().map(Address::to_string),
        move |routed| {
            let sink = sink.clone();
            async move {
                let Some(decoded) = routed.notification.post_account_data() else {
                    return;
                };
                let Ok(account) = decoded.and_then(|data| {
                    data.data.decode().map(|bytes| (data, bytes)).map_err(Into::into)
                }) else {
                    return;
                };
                let (data, bytes) = account;
                let Ok(owner) = data.owner.to_string().parse() else {
                    return;
                };
                let Ok(address) = routed.account.parse() else {
                    return;
                };
                sink.lock()
                    .expect("the capture sink is never poisoned")
                    .push((
                        routed.notification.context.slot,
                        address,
                        Account {
                            lamports: data.lamports,
                            data: bytes,
                            owner,
                            executable: data.executable,
                            rent_epoch: 0,
                        },
                    ));
            }
        },
    )
    .await
    .context("subscribing to the venue's accounts")?;
    Ok(Capture { changes, handle })
}

impl Capture {
    /// Stop the subscription and fold what it saw into a trajectory keyed by slot; notifications
    /// for one slot arrive in any order.
    pub(crate) async fn finish(self) -> Result<Trajectory> {
        self.handle.stop.send(true).ok();
        self.handle
            .join_handle
            .await
            .context("the account subscription task panicked")?
            .context("the account subscription ended early, so the capture is incomplete")?;

        let drained = std::mem::take(
            &mut *self
                .changes
                .lock()
                .expect("the capture sink is never poisoned"),
        );
        let mut trajectory = Trajectory::default();
        for (slot, address, account) in drained {
            trajectory
                .changes
                .entry(slot)
                .or_default()
                .insert(address, account);
        }
        Ok(trajectory)
    }
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

/// An empty trajectory posts no overrides and silently measures the control.
pub(crate) fn require_changes(trajectory: &Trajectory, accounts: &[Address]) -> Result<()> {
    if trajectory.is_empty() {
        bail!(
            "the capture pass saw no change to any of the venue's {} accounts across the range. \
             Either the venue did not trade, or the subscription missed them — and an arm built \
             from an empty trajectory posts nothing and silently measures the control",
            accounts.len()
        );
    }
    Ok(())
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
        let walked = walk(&start, &trajectory(&[(100, &[(1, 11)]), (200, &[(2, 22)])]))
            .collect::<Vec<_>>();

        assert_eq!(walked[0].0, 100);
        assert_eq!(walked[0].2[&addr(1)].lamports, 11, "the changed account moved");
        assert_eq!(walked[0].2[&addr(2)].lamports, 20, "the untouched one carried");
        assert_eq!(walked[1].2[&addr(1)].lamports, 11, "and stays carried forward");
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
    fn an_empty_capture_is_refused_rather_than_measured() {
        let error = require_changes(&Trajectory::default(), &[addr(1)])
            .expect_err("an empty trajectory must be refused");
        assert!(error.to_string().contains("silently measures the control"), "{error}");
    }
}

//! Driving the simulator: the session request, the loop that pumps it to completion, and the account states it streams back.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use simulator_api::{AccountData, CreateBacktestSessionRequest, RerouteStatsReport};
use simulator_client::{
    AccountDiffNotification, Continue, CreateSession, ManagedBacktestSession, ManagedEvent,
    UiAccountConversionError, account_data_from_ui,
};
use solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta;

use crate::{RunConfig, jsonl::CaptureRow};

/// Every option the counterfactual turns on. The captured bytes ride as per-slot account
/// overrides; the setup carrier rides as a scheduled action instead. Both are visible only to
/// the router.
pub(crate) fn create_session(config: RunConfig) -> Result<CreateBacktestSessionRequest> {
    let create = CreateSession::builder()
        .start_slot(config.range.start_slot)
        .slot_count(config.range.slot_count)
        .reroute_order_flow(true)
        .detect_failed_l1_swaps(config.detect_failed_l1_swaps)
        .reroute_circular_arbs(config.circular_arbs)
        .maybe_reroute_aggregators(config.reroute_aggregators)
        .maybe_reroute_filter(config.filter)
        .actions(config.schedule.setup.into_iter().collect())
        .replay_account_state(!config.range.no_replay)
        .disconnect_timeout_secs(900u16)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .build();
    config
        .schedule
        .overrides
        .into_iter()
        .fold(create, |create, (slot, overrides)| {
            create.add_override(slot, overrides)
        })
        .into_request()
        .context("building the backtest session request")
}

/// `on_transaction` sees nothing unless the caller subscribed.
pub(crate) async fn drive_to_completion(
    session: &mut ManagedBacktestSession,
    slot_count: u64,
    mut on_transaction: impl FnMut(EncodedConfirmedTransactionWithStatusMeta),
) -> Result<Option<RerouteStatsReport>> {
    loop {
        match session.next_event().await? {
            ManagedEvent::ReadyForContinue => {
                let advance = Continue::builder().advance_count(slot_count).build();
                session.send_continue(advance.into_params()).await?;
            }
            ManagedEvent::Slot(slot) => eprintln!("[slot] {slot}"),
            ManagedEvent::Transaction(transaction) => on_transaction(*transaction),
            ManagedEvent::Completed { summary, .. } => {
                return Ok(summary.and_then(|summary| summary.reroute_stats.map(|stats| *stats)));
            }
            ManagedEvent::Error(error) => bail!("session error: {error}"),
            _ => {}
        }
    }
}

/// Everything the account-diff subscription accumulates, behind one lock.
#[derive(Default)]
pub(crate) struct CaptureCollector {
    pub(crate) rows: BTreeMap<u64, CaptureRow>,
    pub(crate) conversion_error: Option<anyhow::Error>,
    first_pre_taken: bool,
}

impl CaptureCollector {
    /// Only the very first diff carries a usable pre-state: it is the account as it stood at
    /// `start_slot`, before anything in the range touched it.
    pub(crate) fn record_diff(&mut self, diff: &AccountDiffNotification, start_slot: u64) {
        let slot = diff.context.slot;
        if !std::mem::replace(&mut self.first_pre_taken, true)
            && let Some(pre) = &diff.pre
        {
            match account_data_from_ui(pre) {
                Ok(account) => self.record_initial(start_slot, account),
                Err(error) => self.record_error(error, slot),
            }
        }
        match diff.post_account_data() {
            Some(Ok(account)) => self.record_state(slot, account, diff.signature.clone()),
            Some(Err(error)) => self.record_error(error, slot),
            None => {}
        }
    }

    /// A diff landing on `slot` is the real state there and wins over the pre-state.
    fn record_initial(&mut self, slot: u64, account: AccountData) {
        self.rows.entry(slot).or_insert(CaptureRow {
            slot,
            account,
            signature: None,
            transaction: None,
        });
    }

    fn record_state(&mut self, slot: u64, account: AccountData, signature: Option<String>) {
        self.rows.insert(
            slot,
            CaptureRow {
                slot,
                account,
                signature,
                transaction: None,
            },
        );
    }

    fn record_error(&mut self, error: UiAccountConversionError, slot: u64) {
        self.conversion_error.get_or_insert_with(|| {
            anyhow::Error::new(error).context(format!("converting account diff at slot {slot}"))
        });
    }
}

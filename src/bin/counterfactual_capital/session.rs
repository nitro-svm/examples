//! Driving the simulator: one arm's session request, the loop that pumps it to completion, and
//! reading the venue's own inventory at the slot the arm starts from.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use simulator_api::{
    AccountData, AccountModifications, CreateBacktestSessionRequest, DirectFillParams,
    RerouteAggregators, RerouteStatsReport, SwapAggregator,
};
use simulator_client::{Continue, CreateSession, ManagedBacktestSession, ManagedEvent};
use solana_account::Account;
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

/// One arm: the range, the venue book, and the inventory this arm posts before it starts.
pub(crate) struct Arm {
    pub(crate) start_slot: u64,
    pub(crate) slot_count: u64,
    pub(crate) no_replay: bool,
    pub(crate) spec: DirectFillParams,
    /// Empty for the arm that runs the venue's own book unmodified.
    pub(crate) overrides: BTreeMap<Address, AccountData>,
}

/// Every aggregator, named rather than defaulted.
///
/// `is_candidate` drops a transaction whose router is outside this set before the direct-fill
/// census runs at all, and the API's own default is Jupiter alone — so leaving it unset would
/// bound the population for a reason that has nothing to do with the venue under test.
fn all_aggregators() -> RerouteAggregators {
    RerouteAggregators::new([
        SwapAggregator::Jupiter,
        SwapAggregator::Okx,
        SwapAggregator::Titan,
        SwapAggregator::Dflow,
    ])
}

/// The session an arm runs in.
///
/// Re-quoting is off: direct fill prices the venue itself, and the router a re-quote would need is
/// a sidecar this run never starts. The inventory rides as a single override anchored at
/// `start_slot`, which the schedule carries forward for the rest of the range.
pub(crate) fn create_session(arm: Arm) -> Result<CreateBacktestSessionRequest> {
    let create = CreateSession::builder()
        .start_slot(arm.start_slot)
        .slot_count(arm.slot_count)
        .reroute_order_flow(true)
        .reroute_requote(false)
        .reroute_aggregators(all_aggregators())
        .reroute_direct_fill(Box::new(arm.spec))
        .replay_account_state(!arm.no_replay)
        .disconnect_timeout_secs(900u16)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .build();
    let create = match arm.overrides.is_empty() {
        true => create,
        false => create.add_override(arm.start_slot, AccountModifications(arm.overrides)),
    };
    create
        .into_request()
        .context("building the backtest session request")
}

/// Pump the session to completion and hand back the direct-fill census.
pub(crate) async fn drive_to_completion(
    session: &mut ManagedBacktestSession,
    slot_count: u64,
) -> Result<RerouteStatsReport> {
    loop {
        match session.next_event().await? {
            ManagedEvent::ReadyForContinue => {
                let advance = Continue::builder().advance_count(slot_count).build();
                session.send_continue(advance.into_params()).await?;
            }
            ManagedEvent::Slot(slot) => eprintln!("[slot] {slot}"),
            ManagedEvent::Completed { summary, .. } => {
                return summary
                    .and_then(|summary| summary.reroute_stats.map(|stats| *stats))
                    .context(
                        "the session completed without reroute stats, so the arm priced nothing",
                    );
            }
            ManagedEvent::Error(error) => bail!("session error: {error}"),
            _ => {}
        }
    }
}

/// The venue's inventory as it stood at the slot the session starts from.
///
/// Read through the session's own RPC rather than a mainnet endpoint: the multiple has to be
/// relative to what the venue held during the replayed range, not to what it holds today, or the
/// control arm is already a counterfactual and every difference is measured off the wrong base.
pub(crate) async fn read_vaults(
    rpc_url: &str,
    vaults: &[Address],
) -> Result<Vec<(Address, Account)>> {
    let keys = vaults
        .iter()
        .map(|vault| vault.to_string().parse::<Pubkey>().map_err(anyhow::Error::new))
        .collect::<Result<Vec<_>>>()?;
    let fetched = RpcClient::new(rpc_url.to_string())
        .get_multiple_accounts(&keys)
        .await
        .context("reading the venue's vaults at the session's start slot")?;
    vaults
        .iter()
        .zip(fetched)
        .map(|(vault, account)| {
            account
                .map(|account| (*vault, account))
                .with_context(|| format!("vault {vault} does not exist at the start slot"))
        })
        .collect()
}

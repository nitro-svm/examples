//! Driving the simulator: one arm's session request, the loop that pumps it to completion, and the venue's inventory at the slot the arm starts from.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use simulator_api::{
    AccountData, AccountModifications, CreateBacktestSessionRequest, RerouteAggregators,
    VenueTemplate,
};
use simulator_client::CreateSession;
use solana_account::Account;
use solana_address::Address;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use backtest_example::utils::{self, range::RangeArgs};

/// One arm: the range, the venue book, and the inventory this arm posts before it starts.
pub(crate) struct Arm {
    pub(crate) range: RangeArgs,
    pub(crate) no_replay: bool,
    pub(crate) template: VenueTemplate,
    /// One entry per slot the venue's state changed in, scaled so the venue follows its real
    /// trajectory at a different size. Empty for the capture pass.
    pub(crate) overrides: Vec<(u64, BTreeMap<Address, AccountData>)>,
}

/// Re-quoting is off: the venue template prices the venue itself, and the router a re-quote needs
/// is a sidecar this run never starts. The API defaults to Jupiter alone, and a swap whose router
/// is outside the aggregator set never reaches the template, so every aggregator is admitted.
///
/// Posting only the changed slots reproduces the whole trajectory, since
/// `OverrideSchedule::active_at` folds every entry up to a slot and lets the latest win.
pub(crate) fn create_session(arm: Arm) -> Result<CreateBacktestSessionRequest> {
    let create = CreateSession::builder()
        .start_slot(arm.range.start_slot)
        .slot_count(arm.range.slot_count)
        .reroute_order_flow(true)
        .reroute_requote(false)
        .reroute_aggregators(RerouteAggregators::all())
        .reroute_venue_templates(vec![arm.template])
        .replay_account_state(!arm.no_replay)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .build();
    let overrides = arm
        .overrides
        .into_iter()
        .map(|(slot, accounts)| (slot, AccountModifications(accounts)));
    utils::session::with_overrides(create, overrides)
        .into_request()
        .context("building the backtest session request")
}

/// Read through the session's own RPC rather than a mainnet endpoint: every arm is a multiple of
/// what the venue held *during the replayed range*, not of what it holds today.
pub(crate) async fn read_accounts(
    rpc: &RpcClient,
    addresses: &[Address],
) -> Result<Vec<(Address, Account)>> {
    let fetched = rpc
        .get_multiple_accounts(addresses)
        .await
        .context("reading the venue's accounts at the session's start slot")?;
    addresses
        .iter()
        .zip(fetched)
        .map(|(address, account)| {
            account
                .map(|account| (*address, account))
                .with_context(|| format!("{address} does not exist at the start slot"))
        })
        .collect()
}

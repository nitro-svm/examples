//! Driving the simulator: one arm's session request, the loop that pumps it to completion, and the venue's inventory at the slot the arm starts from.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use simulator_api::{
    AccountData, AccountModifications, CreateBacktestSessionRequest, DirectFillParams,
    RerouteAggregators, SwapAggregator,
};
use simulator_client::CreateSession;
use solana_account::Account;
use solana_address::Address;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

/// One arm: the range, the venue book, and the inventory this arm posts before it starts.
pub(crate) struct Arm {
    pub(crate) start_slot: u64,
    pub(crate) slot_count: u64,
    pub(crate) no_replay: bool,
    pub(crate) spec: DirectFillParams,
    /// One entry per slot the venue's state changed in, scaled so the venue follows its real
    /// trajectory at a different size. Empty for the capture pass.
    pub(crate) overrides: Vec<(u64, BTreeMap<Address, AccountData>)>,
}

/// The API defaults to Jupiter alone, and a transaction whose router is outside this set is
/// dropped before the direct-fill census runs at all.
fn all_aggregators() -> RerouteAggregators {
    RerouteAggregators::new([
        SwapAggregator::Jupiter,
        SwapAggregator::Okx,
        SwapAggregator::Titan,
        SwapAggregator::Dflow,
    ])
}

/// Re-quoting is off: direct fill prices the venue itself, and the router a re-quote needs is a
/// sidecar this run never starts. Posting only the changed slots reproduces the whole trajectory,
/// since `OverrideSchedule::active_at` folds every entry up to a slot and lets the latest win.
pub(crate) fn create_session(arm: Arm) -> Result<CreateBacktestSessionRequest> {
    let create = CreateSession::builder()
        .start_slot(arm.start_slot)
        .slot_count(arm.slot_count)
        .reroute_order_flow(true)
        .reroute_requote(false)
        .reroute_aggregators(all_aggregators())
        .reroute_direct_fill(Box::new(arm.spec))
        .replay_account_state(!arm.no_replay)
        .capacity_wait_timeout_secs(900u16)
        .send_summary(true)
        .build();
    let create = arm
        .overrides
        .into_iter()
        .fold(create, |create, (slot, accounts)| {
            create.add_override(slot, AccountModifications(accounts))
        });
    create
        .into_request()
        .context("building the backtest session request")
}

/// Read through the session's own RPC rather than a mainnet endpoint: every arm is a multiple of
/// what the venue held *during the replayed range*, not of what it holds today.
pub(crate) async fn read_accounts(
    rpc_url: &str,
    addresses: &[Address],
) -> Result<Vec<(Address, Account)>> {
    let fetched = RpcClient::new(rpc_url.to_string())
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

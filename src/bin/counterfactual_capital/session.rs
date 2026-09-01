//! Driving the simulator: one arm's session request, the loop that pumps it to completion, and the venue's inventory at the slot the arm starts from.

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
        .disconnect_timeout_secs(900u16)
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

/// Every chain read has to happen at this pause: the session is only positioned at its start slot
/// once it says so, and its RPC endpoint stops serving the moment the session completes.
pub(crate) async fn wait_for_first_pause(session: &mut ManagedBacktestSession) -> Result<()> {
    loop {
        match session.next_event().await? {
            ManagedEvent::ReadyForContinue => return Ok(()),
            ManagedEvent::Completed { .. } => {
                bail!("the session finished its range before it was ready to advance")
            }
            ManagedEvent::Error(error) => bail!("session error: {error}"),
            _ => {}
        }
    }
}

/// The caller has already consumed the first pause.
pub(crate) async fn advance_to_completion(
    session: &mut ManagedBacktestSession,
    slot_count: u64,
) -> Result<RerouteStatsReport> {
    let advance = Continue::builder().advance_count(slot_count).build();
    session.send_continue(advance.into_params()).await?;
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

/// Read through the session's own RPC rather than a mainnet endpoint: every arm is a multiple of
/// what the venue held *during the replayed range*, not of what it holds today.
pub(crate) async fn read_accounts(
    rpc_url: &str,
    addresses: &[Address],
) -> Result<Vec<(Address, Account)>> {
    let keys = addresses
        .iter()
        .map(|address| {
            address
                .to_string()
                .parse::<Pubkey>()
                .map_err(anyhow::Error::new)
        })
        .collect::<Result<Vec<_>>>()?;
    let fetched = RpcClient::new(rpc_url.to_string())
        .get_multiple_accounts(&keys)
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

//! Driving a managed session: the pump loop every example needs, and the census it ends with.

use anyhow::{Result, bail};
use simulator_api::{RerouteStatsReport, SessionSummary};
use simulator_client::{Continue, ManagedBacktestSession, ManagedEvent};

/// The reroute census a completed session reports, if it ran one. The wire shape is a nested
/// `Option<Box<_>>`; unwrapping it is not something an example should have to know.
pub fn reroute_stats(summary: Option<SessionSummary>) -> Option<RerouteStatsReport> {
    summary.and_then(|summary| summary.reroute_stats.map(|stats| *stats))
}

/// Pump until the session first reports ready to advance.
///
/// Chain reads belong at this moment and no other: the session is only positioned at its start
/// slot once it says so, and its RPC endpoint stops serving the moment the session completes.
pub async fn wait_for_first_pause(session: &mut ManagedBacktestSession) -> Result<()> {
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

/// Advance the session over its whole range. `on_event` sees every event the loop does not consume
/// itself — `Slot`, `Transaction`, and anything added later — because what to do with those is the
/// experiment, not the plumbing.
pub async fn drive_to_completion(
    session: &mut ManagedBacktestSession,
    slot_count: u64,
    mut on_event: impl FnMut(ManagedEvent),
) -> Result<Option<RerouteStatsReport>> {
    loop {
        match session.next_event().await? {
            ManagedEvent::ReadyForContinue => advance(session, slot_count).await?,
            ManagedEvent::Completed { summary, .. } => return Ok(reroute_stats(summary)),
            ManagedEvent::Error(error) => bail!("session error: {error}"),
            other => on_event(other),
        }
    }
}

/// [`drive_to_completion`] for a caller that already consumed the first pause (to read state at the
/// start slot), so the first `Continue` is sent rather than waited for.
pub async fn resume_to_completion(
    session: &mut ManagedBacktestSession,
    slot_count: u64,
    on_event: impl FnMut(ManagedEvent),
) -> Result<Option<RerouteStatsReport>> {
    advance(session, slot_count).await?;
    drive_to_completion(session, slot_count, on_event).await
}

async fn advance(session: &mut ManagedBacktestSession, slot_count: u64) -> Result<()> {
    let params = Continue::builder()
        .advance_count(slot_count)
        .build()
        .into_params();
    session.send_continue(params).await.map_err(Into::into)
}

//! Driving a managed session: the pump loop every example needs, and the census it ends with.

use anyhow::{Result, bail};
use simulator_api::{RerouteStatsReport, SessionSummary};
use simulator_client::{Continue, ManagedBacktestSession, ManagedEvent};

use crate::utils::progress::Progress;

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
///
/// `progress` carries the range, so the advance size and the line reporting it cannot disagree. It
/// also sees every `Slot` before `on_event` does; a caller that wants to do its own accounting
/// still gets the event.
pub async fn drive_to_completion(
    session: &mut ManagedBacktestSession,
    progress: &mut Progress,
    mut on_event: impl FnMut(ManagedEvent),
) -> Result<Option<RerouteStatsReport>> {
    loop {
        match session.next_event().await? {
            ManagedEvent::ReadyForContinue => advance(session, progress.slot_count()).await?,
            ManagedEvent::Completed { summary, .. } => {
                progress.finish();
                return Ok(reroute_stats(summary));
            }
            // Finished before bailing, so the failure message is not written over the line.
            ManagedEvent::Error(error) => {
                progress.finish();
                bail!("session error: {error}")
            }
            other => {
                match &other {
                    ManagedEvent::Slot(slot) => progress.observe(*slot),
                    // Before the first slot this is the only thing that moves. A cold sidecar can
                    // take minutes to come up, and the line has to say so rather than sit at zero.
                    ManagedEvent::Status(status) => progress.stage(status),
                    _ => {}
                }
                on_event(other)
            }
        }
    }
}

/// [`drive_to_completion`] for a caller that already consumed the first pause (to read state at the
/// start slot), so the first `Continue` is sent rather than waited for.
pub async fn resume_to_completion(
    session: &mut ManagedBacktestSession,
    progress: &mut Progress,
    on_event: impl FnMut(ManagedEvent),
) -> Result<Option<RerouteStatsReport>> {
    advance(session, progress.slot_count()).await?;
    drive_to_completion(session, progress, on_event).await
}

async fn advance(session: &mut ManagedBacktestSession, slot_count: u64) -> Result<()> {
    let params = Continue::builder()
        .advance_count(slot_count)
        .build()
        .into_params();
    session.send_continue(params).await.map_err(Into::into)
}

//! The line a session shows while it replays. One row, rewritten in place, so a 50,000-slot range
//! reads as motion instead of 50,000 lines of scrollback.
//!
//! Progress is derived from the slot *number*, never from counting notifications: a slot skipped
//! on-chain is never announced, so a counter would drift behind the range it is reporting.

use std::{
    fmt::Display,
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::task::JoinHandle;

/// How often the rewritten line repaints: fast enough to read as live, slow enough that the
/// terminal is never the bottleneck.
const TTY_REPAINT: Duration = Duration::from_millis(250);

/// A redirected stderr gets whole lines this far apart instead. `\r` into a log file is noise, but
/// a transcript still wants evidence the run is moving.
const PIPED_REPAINT: Duration = Duration::from_secs(15);

/// How often a permanent row is left behind amid the rewritten line, so a finished run has a
/// readable trail instead of one final frame.
const MILESTONE_SLOTS: u64 = 100;

/// Braille frames: one cell wide in every terminal, so the row never reflows as it turns.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Bringing a session up is dominated by the router's cold start, which is long enough that a
/// first-time reader assumes a hang. Say so once it has gone on long enough to worry about,
/// rather than on every run.
fn waiting_hint(elapsed: Duration) -> &'static str {
    match elapsed.as_secs() {
        0..=29 => "",
        30..=179 => " · the router loads its market cache first, which is the slow part",
        _ => " · still loading; a cold router can take minutes",
    }
}

/// A tally the experiment bumps and the line renders — re-quotes seen, legs the venue won. Cloning
/// shares the count, so the subscription task and the painter see the same number.
#[derive(Clone, Default)]
pub struct Counter(Arc<AtomicU64>);

impl Counter {
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

struct State {
    start_slot: u64,
    /// The range is inclusive — a session over `[start, start + count]` replays `count + 1` slots —
    /// so the denominator is one more than `--slot-count`.
    total: u64,
    /// `None` until the first slot arrives, which keeps the opening line honest about having
    /// replayed nothing rather than claiming slot zero.
    current: AtomicU64,
    seen: AtomicBool,
    /// What the session last said it was doing. Before the first slot this is the only thing
    /// moving — a cold sidecar can take minutes, and a frozen `0/50000` reads as a hang.
    stage: Mutex<Option<String>>,
    /// When the current stage was entered, and how many have gone by. A cold sidecar sits in one
    /// stage for minutes: the elapsed clock alone cannot say whether that is work or a hang, but
    /// a stage that keeps advancing can.
    stage_since: Mutex<Option<Instant>>,
    /// The server's id for this session, known as soon as the handshake returns. Printed so a
    /// slow startup can be traced to the session's own logs rather than guessed at.
    session: Mutex<Option<String>>,
    /// Highest milestone already left on screen, in units of [`MILESTONE_SLOTS`].
    milestone: AtomicU64,
    stages_seen: AtomicU64,
    /// Bumped every repaint so the waiting line animates. A still frame and a wedged session look
    /// the same; a turning one does not.
    tick: AtomicU64,
    counters: Vec<(&'static str, Counter)>,
    started: Instant,
    /// When the first slot landed. Rate and ETA are measured from here, not from `started`:
    /// bringing a session up takes as long as it takes, and averaging that wait into throughput
    /// understates the rate for the whole run and projects a wild ETA on the first frame.
    replaying_since: Mutex<Option<Instant>>,
    finished: AtomicBool,
}

impl State {
    fn done(&self) -> u64 {
        if !self.seen.load(Ordering::Relaxed) {
            return 0;
        }
        // Saturating: a session that pauses past its requested end still renders sanely.
        self.current
            .load(Ordering::Relaxed)
            .saturating_sub(self.start_slot)
            .saturating_add(1)
            .min(self.total)
    }

    /// ` · session <id>` once known, empty before. Short form: the tail is what distinguishes
    /// one session from another in a log.
    fn session_tag(&self) -> String {
        self.session
            .lock()
            .ok()
            .and_then(|held| held.clone())
            .map(|id| format!(" · session {}", &id[id.len().saturating_sub(12)..]))
            .unwrap_or_default()
    }

    fn stage_name(&self) -> Option<String> {
        self.stage.lock().ok().and_then(|stage| stage.clone())
    }

    /// The whole row, without the leading `\r` or a trailing newline — the caller decides which
    /// terminal this is going to.
    fn line(&self) -> String {
        let done = self.done();
        let elapsed = self.started.elapsed();
        // Nothing has replayed yet. Reporting the session's own stage beats a frozen `0/n`, which
        // is indistinguishable from a hang during a cold sidecar start.
        if done == 0 && !self.finished.load(Ordering::Relaxed) {
            // Not "connecting": the handshake is done and the session has an id. What is
            // outstanding is the server bringing it up, which it does not narrate.
            let stage = self
                .stage_name()
                .unwrap_or_else(|| "waiting for session startup".to_string());
            let in_stage = self
                .stage_since
                .lock()
                .ok()
                .and_then(|since| *since)
                .map_or_else(|| elapsed, |since| since.elapsed());
            let passed = self.stages_seen.load(Ordering::Relaxed);
            let steps = match passed {
                0 | 1 => String::new(),
                n => format!(" · step {n}"),
            };
            return format!(
                "[replay] {} {stage}{steps}{} · {} in stage · {} elapsed · 0/{} slots{}",
                SPINNER[(self.tick.fetch_add(1, Ordering::Relaxed) as usize) % SPINNER.len()],
                self.session_tag(),
                clock(in_stage),
                clock(elapsed),
                self.total,
                waiting_hint(elapsed),
            );
        }
        // Throughput is slots per second of *replaying*, so the session-start wait does not drag
        // it down. Below a threshold the sample is too short to divide by honestly.
        let replay_secs = self
            .replaying_since
            .lock()
            .ok()
            .and_then(|since| *since)
            .map_or(0.0, |since| since.elapsed().as_secs_f64());
        let rate = if replay_secs >= 0.25 {
            done as f64 / replay_secs
        } else {
            0.0
        };
        let percent = if self.total > 0 {
            done as f64 / self.total as f64 * 100.0
        } else {
            0.0
        };
        let mut line = format!(
            "[replay] {done}/{} slots · {percent:.1}% · {rate:.0} slot/s · {} elapsed",
            self.total,
            clock(elapsed),
        );
        // An ETA before the first slot would be a guess, and one after the last is noise.
        if rate > 0.0 && done < self.total {
            let left = (self.total - done) as f64 / rate;
            // `from_secs_f64` panics on a non-finite or negative input; both are impossible here
            // (`rate > 0.0` and `done < total`), but the clamp keeps that true if either changes.
            line.push_str(&format!(
                " · ~{} left",
                clock(Duration::from_secs_f64(
                    left.clamp(0.0, f64::from(u32::MAX))
                ))
            ));
        }
        for (label, counter) in &self.counters {
            line.push_str(&format!(" · {label} {}", counter.get()));
        }
        line
    }
}

/// `MM:SS`, or `HH:MM:SS` once a run has earned the third field.
fn clock(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, total % 3600 / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The progress line, and the range it is reporting on. Dropping it stops the painter, so a run
/// that bails early leaves no task behind.
pub struct Progress {
    state: Arc<State>,
    painter: Option<JoinHandle<()>>,
}

impl Progress {
    /// A live line over `[start_slot, start_slot + slot_count]`, labelled with whatever counters
    /// the experiment wants rendered alongside the slots.
    ///
    /// Spawns the painter, so it needs a Tokio runtime. Every session driver has one; a caller
    /// that does not wants [`Progress::silent`].
    pub fn new(start_slot: u64, slot_count: u64, counters: Vec<(&'static str, Counter)>) -> Self {
        let state = Arc::new(State {
            start_slot,
            total: slot_count.saturating_add(1),
            current: AtomicU64::new(start_slot),
            seen: AtomicBool::new(false),
            stage: Mutex::new(None),
            stage_since: Mutex::new(None),
            session: Mutex::new(None),
            milestone: AtomicU64::new(0),
            stages_seen: AtomicU64::new(0),
            tick: AtomicU64::new(0),
            counters,
            started: Instant::now(),
            replaying_since: Mutex::new(None),
            finished: AtomicBool::new(false),
        });
        let painter = Some(spawn_painter(state.clone()));
        Self { state, painter }
    }

    /// The same range with nothing drawn. For tests, and for any caller whose stderr is carrying
    /// something else — the line is a courtesy, not a dependency.
    pub fn silent(start_slot: u64, slot_count: u64) -> Self {
        Self {
            state: Arc::new(State {
                start_slot,
                total: slot_count.saturating_add(1),
                current: AtomicU64::new(start_slot),
                seen: AtomicBool::new(false),
                stage: Mutex::new(None),
                stage_since: Mutex::new(None),
                session: Mutex::new(None),
                milestone: AtomicU64::new(0),
                stages_seen: AtomicU64::new(0),
                tick: AtomicU64::new(0),
                counters: Vec::new(),
                started: Instant::now(),
                replaying_since: Mutex::new(None),
                finished: AtomicBool::new(false),
            }),
            painter: None,
        }
    }

    /// What the session was told to advance by. Held here so the range has one source of truth.
    pub fn slot_count(&self) -> u64 {
        self.state.total.saturating_sub(1)
    }

    /// Record what the session says it is doing, for the stretch before any slot has replayed.
    /// The server's id for this session. Printed once and carried on the waiting line, so a
    /// startup that takes minutes can be traced rather than guessed at.
    pub fn session(&self, id: impl Display) {
        let id = id.to_string();
        eprintln!("[session] {id}");
        if let Ok(mut held) = self.state.session.lock() {
            *held = Some(id);
        }
    }

    pub fn stage(&self, stage: impl Display) {
        let stage = stage.to_string();
        if let Ok(mut held) = self.state.stage.lock() {
            // A repeated status is not a new stage: resetting the clock on one would hide a
            // stage that has stopped making progress.
            if held.as_deref() == Some(stage.as_str()) {
                return;
            }
            *held = Some(stage);
            self.state.stages_seen.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut since) = self.state.stage_since.lock() {
                *since = Some(Instant::now());
            }
        }
    }

    /// Record the slot the session just replayed. The first one starts the throughput clock.
    pub fn observe(&self, slot: u64) {
        if !self.state.seen.swap(true, Ordering::Relaxed)
            && let Ok(mut since) = self.state.replaying_since.lock()
        {
            *since = Some(Instant::now());
        }
        self.state.current.store(slot, Ordering::Relaxed);
    }

    /// Stop painting and leave the finished line on screen. Idempotent, because a session can end
    /// by completing or by failing and both paths come through here.
    pub fn finish(&mut self) {
        if self.state.finished.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Some(painter) = self.painter.take() {
            painter.abort();
            let mut stderr = io::stderr();
            // Overwrite the rewritten row rather than scrolling past a half-width remnant of it.
            let _ = if stderr.is_terminal() {
                write!(stderr, "\r\x1b[2K{}\n", self.state.line())
            } else {
                writeln!(stderr, "{}", self.state.line())
            };
            let _ = stderr.flush();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The repaint loop. Split out because whether stderr is a terminal is decided once, at the top,
/// rather than on every tick.
fn spawn_painter(state: Arc<State>) -> JoinHandle<()> {
    let tty = io::stderr().is_terminal();
    let period = if tty { TTY_REPAINT } else { PIPED_REPAINT };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        // The first tick completes immediately; skipping it keeps an empty line off the screen
        // before the session has replayed anything.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if state.finished.load(Ordering::Relaxed) {
                return;
            }
            let mut stderr = io::stderr();
            // A milestone is the same row, kept. Writing it from the painter rather than from
            // the event loop keeps every terminal write on one thread, so a permanent row can
            // never land halfway through a rewritten one.
            let reached = state.done() / MILESTONE_SLOTS;
            let passed = state.milestone.swap(reached, Ordering::Relaxed);
            let _ = if tty {
                if reached > passed && state.done() > 0 {
                    let _ = writeln!(stderr, "\r\x1b[2K{}", state.line());
                }
                write!(stderr, "\r\x1b[2K{}", state.line())
            } else {
                writeln!(stderr, "{}", state.line())
            };
            let _ = stderr.flush();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_counts_both_ends_of_the_inclusive_range() {
        let progress = Progress::silent(100, 9);
        assert_eq!(progress.state.total, 10);
        assert_eq!(progress.slot_count(), 9);
    }

    #[test]
    fn nothing_is_done_until_a_slot_arrives() {
        let progress = Progress::silent(100, 9);
        assert_eq!(progress.state.done(), 0);
        progress.observe(100);
        assert_eq!(progress.state.done(), 1);
    }

    #[test]
    fn a_skipped_slot_still_advances_progress() {
        let progress = Progress::silent(100, 9);
        // 101 and 102 were skipped on-chain and never announced.
        progress.observe(103);
        assert_eq!(progress.state.done(), 4);
    }

    #[test]
    fn progress_past_the_requested_end_is_clamped() {
        let progress = Progress::silent(100, 9);
        progress.observe(200);
        assert_eq!(progress.state.done(), 10);
    }

    /// Async because the live constructor spawns the painter; the silent one takes no counters.
    #[tokio::test]
    async fn counters_render_after_the_slots() {
        let requotes = Counter::default();
        requotes.add(7);
        let mut progress = Progress::new(100, 9, vec![("requotes", requotes.clone())]);
        progress.observe(104);
        let line = progress.state.line();
        assert!(line.contains("5/10 slots"), "{line}");
        assert!(line.contains("requotes 7"), "{line}");
        // Stops the painter before the runtime goes away.
        progress.finish();
    }

    #[test]
    fn before_the_first_slot_the_line_reports_the_session_stage() {
        let progress = Progress::silent(100, 49_999);
        // No stage yet: still says what it is doing, not "0 slot/s". The session is already
        // established at this point, so the line must not claim to be connecting.
        let line = progress.state.line();
        assert!(line.contains("waiting for session startup"), "{line}");
        assert!(!line.contains("connecting"), "{line}");
        assert!(!line.contains("slot/s"), "{line}");

        progress.stage("starting runtime");
        let line = progress.state.line();
        assert!(line.contains("starting runtime"), "{line}");
        assert!(line.contains("in stage"), "{line}");
        assert!(line.contains("0/50000 slots"), "{line}");

        // Once slots replay, the stage gives way to real throughput.
        progress.observe(120);
        let line = progress.state.line();
        assert!(line.contains("21/50000 slots"), "{line}");
        assert!(!line.contains("in stage"), "{line}");
    }

    /// The spinner is the only part of a waiting line that moves when nothing else does, so a
    /// still frame reads as a hang. It has to advance on every repaint.
    #[test]
    fn the_waiting_line_animates_between_repaints() {
        let progress = Progress::silent(100, 49_999);
        let frames: Vec<String> = (0..3).map(|_| progress.state.line()).collect();
        assert_ne!(frames[0], frames[1], "{frames:?}");
        assert_ne!(frames[1], frames[2], "{frames:?}");
    }

    /// A status repeated by the server is not progress. Restarting the in-stage clock on one
    /// would make a stage that has stopped advancing look like it just began.
    #[test]
    fn a_repeated_status_does_not_restart_the_stage_clock() {
        let progress = Progress::silent(100, 49_999);
        progress.stage("starting runtime");
        let first = *progress.state.stage_since.lock().unwrap();
        progress.stage("starting runtime");
        assert_eq!(
            *progress.state.stage_since.lock().unwrap(),
            first,
            "the same status twice is one stage"
        );
        assert_eq!(progress.state.stages_seen.load(Ordering::Relaxed), 1);

        progress.stage("program accounts loaded");
        assert_eq!(progress.state.stages_seen.load(Ordering::Relaxed), 2);
        assert!(progress.state.line().contains("step 2"), "counts the steps");
    }

    #[test]
    fn the_throughput_clock_starts_at_the_first_slot_and_never_restarts() {
        let progress = Progress::silent(100, 9);
        assert!(
            progress.state.replaying_since.lock().unwrap().is_none(),
            "a session still coming up has replayed nothing to measure"
        );
        progress.observe(100);
        let first = *progress.state.replaying_since.lock().unwrap();
        assert!(first.is_some());
        progress.observe(105);
        assert_eq!(
            *progress.state.replaying_since.lock().unwrap(),
            first,
            "a later slot must not restart the clock, or the rate would keep resetting"
        );
    }

    #[test]
    fn clock_grows_a_third_field_only_when_earned() {
        assert_eq!(clock(Duration::from_secs(59)), "0:59");
        assert_eq!(clock(Duration::from_secs(600)), "10:00");
        assert_eq!(clock(Duration::from_secs(3661)), "1:01:01");
    }

    /// The id is what ties a slow startup to the session's own server-side logs, so it has to
    /// reach the waiting line and not just scrollback.
    #[test]
    fn the_waiting_line_names_the_session() {
        let progress = Progress::silent(100, 49_999);
        // The default stage text also contains the word, so match the tag's own separator.
        assert!(!progress.state.line().contains("· session "), "none yet");

        progress.session("backtest_01a0ce07cba077c18a1902ca2b4c20d9");
        let line = progress.state.line();
        assert!(line.contains("· session "), "{line}");
        // Short form: the tail distinguishes sessions, the prefix is the same on every one.
        assert!(line.contains("ca2b4c20d9"), "{line}");
        assert!(!line.contains("backtest_01a0"), "{line}");
    }

    /// Milestones are struck off by slot count, so a run leaves one permanent row per
    /// `MILESTONE_SLOTS` rather than one per repaint.
    #[test]
    fn milestones_advance_once_per_block_of_slots() {
        let progress = Progress::silent(1_000, 9_999);
        progress.observe(1_099);
        assert_eq!(progress.state.done() / MILESTONE_SLOTS, 1);
        progress.observe(1_198);
        assert_eq!(progress.state.done() / MILESTONE_SLOTS, 1, "same block");
        progress.observe(1_200);
        assert_eq!(progress.state.done() / MILESTONE_SLOTS, 2, "next block");
    }
}

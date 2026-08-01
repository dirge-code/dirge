//! Progress monitor — the "busy but not converging" signal (dirge-uw2l.3).
//!
//! Every other loop guard keys on ERRORS. The storm breaker needs an
//! identical repeated call ([`super::storm`]); the failure tracker needs
//! errored results ([`super::failure_tracker`]); the file-touch tracker
//! needs the same files touched over and over
//! ([`super::context_depth`]). A model making *successful*, *varied*,
//! useless tool calls trips none of them — it just burns the run until
//! `max_turns` hard-stops it with a truncation notice.
//!
//! That failure mode is the one the DS1 Remote Agent post-mortem singles
//! out. The dominant late problem with its planner was "PS operating
//! correctly but being unable to find a plan within the allocated time
//! limit since its search was *thrashing*" — not an error, a non-result
//! within budget. RAX's answers were a hard search bound and, where the
//! residual risk couldn't be designed out, a contingency procedure
//! prepared in advance.
//!
//! Two signals, both cheap and both bounded:
//!
//!   - **stall** — `stall_threshold` turn boundaries pass with no
//!     progress event → one checkpoint asking the model to name what is
//!     blocking and either change approach or narrow the goal.
//!   - **budget** — the run crosses 60% and 85% of its turn cap → one
//!     notice each, stating what is left. RAX operated against measured
//!     resource envelopes (32 MB of RAM, 45% of the CPU, a peak of 29 MB
//!     actually observed); dirge enforces `max_turns` but never tells the
//!     model, so a silent hard stop can't prompt triage the way a visible
//!     countdown can.
//!
//! A *progress event* is any of: a todo item closed, a file mutated that
//! was never mutated before, or verification going green. Re-editing one
//! file is deliberately NOT progress — that is exactly the thrash being
//! watched for (and [`super::context_depth`] already covers the narrower
//! same-file case).
//!
//! **The stall counter arms only after the first progress event.** A run
//! that opens with twenty reads is exploring, not stalling, and must not
//! be nudged for it; a run that produced something and then stopped
//! producing is the real signal. Without this the monitor would fire on
//! every research task.
//!
//! Self-contained — no rig/LLM state, no globals. Lives behind
//! `LoopConfig.progress`; when `None` the loop behaves exactly as before.

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::sync::{Arc, Mutex};

use super::message::{LoopMessage, UserMessage};

/// Display tag prefixing the stall checkpoint. The UI keys on this to
/// attribute the message to the system rather than the user — it is
/// injected as a user-role message so the model acts on it, but it isn't
/// user input (same scheme as `[track]` / `[verify-before-done]`).
pub const STALL_TAG: &str = "[stall]";

/// Display tag prefixing the budget notice. See [`STALL_TAG`].
pub const BUDGET_TAG: &str = "[budget]";

/// Upper bound on stall checkpoints per run. The tracker re-arms after
/// each one (another full `stall_threshold` of barren turns), so this
/// caps total noise at two messages however long the run goes.
const MAX_STALL_NUDGES: u8 = 2;

/// Fractions of the turn cap at which a budget notice fires, as
/// (numerator, denominator) to keep the arithmetic integral. 60% is early
/// enough that narrowing scope is still possible; 85% is the last point
/// at which finishing something small still fits.
const BUDGET_MARKS: &[(usize, usize)] = &[(60, 100), (85, 100)];

/// A cheap snapshot of run state, taken at a turn boundary. Each field is
/// a scalar the loop already tracks, so the tracker never has to diff
/// collections — it only compares against the previous boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    /// Unfinished items on the active board (open + in progress). Progress
    /// is a DECREASE — the todo mirror drops terminal items, so a closed
    /// item shows up as one fewer unfinished rather than as a "closed"
    /// counter. An increase is planning, not progress: writing more todos
    /// must not read as getting work done.
    pub todos_unfinished: usize,
    /// Distinct files mutated so far. Progress is an INCREASE — a file
    /// never touched before is new ground. Re-editing one file leaves this
    /// flat, which is the thrash being watched for.
    pub files_touched: usize,
    /// Whether verification is currently green. Progress is the false→true
    /// edge only; staying green is not repeated progress.
    pub verified_green: bool,
}

/// Per-session progress tracker. `Mutex<Inner>` so the turn-boundary poll
/// can reach it without `&mut LoopConfig` plumbing — mirrors
/// [`super::context_depth::FileTouchTracker`].
#[derive(Debug)]
pub struct ProgressTracker {
    inner: Mutex<Inner>,
    stall_threshold: usize,
}

#[derive(Debug, Default)]
struct Inner {
    /// Last observed snapshot, for the strict-increase comparison.
    last: ProgressSnapshot,
    /// Whether any progress event has been seen yet. Until it has, the
    /// run is exploring and the stall counter stays disarmed.
    armed: bool,
    /// Turn boundaries since the last progress event.
    barren_turns: usize,
    /// Stall checkpoints already spent.
    stall_nudges: u8,
    /// Budget marks already announced, as an index into [`BUDGET_MARKS`].
    budget_marks_fired: usize,
}

impl ProgressTracker {
    /// `stall_threshold` is the number of barren turn boundaries before a
    /// checkpoint fires. Clamped to at least 2 — a threshold of 0 or 1
    /// would fire on ordinary back-to-back work.
    pub fn new(stall_threshold: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            stall_threshold: stall_threshold.max(2),
        })
    }

    /// Record a turn boundary. Returns a stall checkpoint when the run has
    /// gone `stall_threshold` boundaries with no progress event since the
    /// last one. Any progress event resets the counter and returns `None`.
    pub fn record_turn(&self, snap: ProgressSnapshot) -> Option<LoopMessage> {
        let mut inner = self.inner.lock_ignore_poison();
        let progressed = snap.todos_unfinished < inner.last.todos_unfinished
            || snap.files_touched > inner.last.files_touched
            || (snap.verified_green && !inner.last.verified_green);
        inner.last = snap;
        if progressed {
            inner.armed = true;
            inner.barren_turns = 0;
            return None;
        }
        if !inner.armed {
            // Still in the exploration prologue — nothing has been
            // produced yet, so there is no stall to detect.
            return None;
        }
        inner.barren_turns += 1;
        if inner.barren_turns < self.stall_threshold || inner.stall_nudges >= MAX_STALL_NUDGES {
            return None;
        }
        inner.stall_nudges += 1;
        // Re-arm: another full threshold must pass before the next one.
        inner.barren_turns = 0;
        Some(stall_message(self.stall_threshold))
    }

    /// Budget notice when the run crosses a [`BUDGET_MARKS`] fraction of
    /// its turn cap. One message per mark, in order. `max_turns == 0` (no
    /// cap configured) never fires.
    pub fn poll_budget(&self, turns_used: usize, max_turns: usize) -> Option<LoopMessage> {
        if max_turns == 0 {
            return None;
        }
        let mut inner = self.inner.lock_ignore_poison();
        let (num, den) = *BUDGET_MARKS.get(inner.budget_marks_fired)?;
        // Integer compare, no float rounding: used/max >= num/den.
        if turns_used * den < max_turns * num {
            return None;
        }
        inner.budget_marks_fired += 1;
        Some(budget_message(turns_used, max_turns))
    }
}

/// The stall checkpoint. Asks for a diagnosis and then a decision —
/// change approach or narrow the goal. Dropping an unachievable
/// low-priority goal is a legitimate outcome, not a failure: rejecting
/// one was an explicit validation objective for the RAX planner, which
/// dropped asteroid imaging targets that didn't fit the observation
/// window rather than failing the whole plan.
fn stall_message(threshold: usize) -> LoopMessage {
    LoopMessage::User(UserMessage::text(format!(
        "{STALL_TAG} {threshold} turns have passed without finishing a task item, touching a new \
         file, or getting a green check. The calls are succeeding but the work isn't converging. \
         Before another one: state in one line what is actually blocking progress, then either \
         change approach or cut scope — if part of this can't be done, say which part and why, \
         and finish the rest. Continuing the same way is the one option that isn't working."
    )))
}

/// The budget notice. States the position and invites triage — the
/// information RAX operators had from their measured resource envelopes
/// and dirge's model, until now, did not.
fn budget_message(turns_used: usize, max_turns: usize) -> LoopMessage {
    let remaining = max_turns.saturating_sub(turns_used);
    LoopMessage::User(UserMessage::text(format!(
        "{BUDGET_TAG} You've used {turns_used} of {max_turns} turns; {remaining} remain, and the \
         run stops when they're gone. Check what's left against that: finish the highest-value \
         work first, and drop or hand off anything that won't fit rather than being cut off \
         mid-way. If everything left fits comfortably, ignore this."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `todos` is the UNFINISHED count — progress is a decrease.
    fn snap(todos: usize, files: usize, green: bool) -> ProgressSnapshot {
        ProgressSnapshot {
            todos_unfinished: todos,
            files_touched: files,
            verified_green: green,
        }
    }

    fn text(msg: LoopMessage) -> String {
        match msg {
            LoopMessage::User(u) => u.text_joined(),
            _ => panic!("expected a user message"),
        }
    }

    /// The exploration prologue must never fire. A run that has produced
    /// nothing yet is reading, not thrashing — this is the guard against
    /// nagging every research task.
    #[test]
    fn exploration_prologue_never_stalls() {
        let t = ProgressTracker::new(3);
        for _ in 0..20 {
            assert!(t.record_turn(snap(0, 0, false)).is_none());
        }
    }

    /// Once something has been produced, barren turns count — and the
    /// checkpoint fires exactly at the threshold, not before.
    #[test]
    fn stall_fires_at_threshold_after_arming() {
        let t = ProgressTracker::new(3);
        // Arm: first file touched.
        assert!(t.record_turn(snap(0, 1, false)).is_none());
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "1 barren");
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "2 barren");
        let msg = t
            .record_turn(snap(0, 1, false))
            .expect("3 barren turns hits the threshold");
        let body = text(msg);
        assert!(body.contains(STALL_TAG), "carries the tag: {body}");
        assert!(body.contains("blocking"), "asks for a diagnosis: {body}");
    }

    /// Each kind of progress event independently resets the counter.
    #[test]
    fn any_progress_event_resets_the_counter() {
        for (label, progressed) in [
            ("new file", snap(0, 2, false)),
            ("went green", snap(0, 1, true)),
        ] {
            let t = ProgressTracker::new(3);
            assert!(t.record_turn(snap(0, 1, false)).is_none(), "arm");
            assert!(t.record_turn(snap(0, 1, false)).is_none());
            assert!(t.record_turn(snap(0, 1, false)).is_none());
            // Progress on the turn that would otherwise have tripped it.
            assert!(
                t.record_turn(progressed).is_none(),
                "{label} must reset, not fire"
            );
            // …and the counter really did restart.
            assert!(t.record_turn(progressed).is_none(), "{label} +1");
            assert!(t.record_turn(progressed).is_none(), "{label} +2");
        }
    }

    /// Closing a todo is progress: the mirror drops terminal items, so a
    /// completed item shows up as one FEWER unfinished.
    #[test]
    fn closing_a_todo_is_progress() {
        let t = ProgressTracker::new(2);
        assert!(t.record_turn(snap(3, 1, false)).is_none(), "arm");
        assert!(t.record_turn(snap(3, 1, false)).is_none(), "1 barren");
        // Item closed: 3 unfinished → 2.
        assert!(
            t.record_turn(snap(2, 1, false)).is_none(),
            "a closed item resets the counter"
        );
        assert!(
            t.record_turn(snap(2, 1, false)).is_none(),
            "counter restarted"
        );
    }

    /// Writing MORE todos is planning, not progress. A model that answers
    /// "do the work" with another `write_todo_list` must not clear the
    /// stall counter by doing so.
    #[test]
    fn adding_todos_is_not_progress() {
        let t = ProgressTracker::new(2);
        assert!(t.record_turn(snap(1, 1, false)).is_none(), "arm");
        // Board grows: 1 unfinished → 5. Planning, not progress.
        assert!(t.record_turn(snap(5, 1, false)).is_none(), "1 barren");
        assert!(
            t.record_turn(snap(9, 1, false)).is_some(),
            "growing the board doesn't count as getting work done"
        );
    }

    /// Re-editing the SAME file is not progress — that's the thrash being
    /// watched for. `files_touched` is a distinct-file count, so a flat
    /// value across turns means no new ground.
    #[test]
    fn re_editing_one_file_is_not_progress() {
        let t = ProgressTracker::new(3);
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "arm");
        assert!(t.record_turn(snap(0, 1, false)).is_none());
        assert!(t.record_turn(snap(0, 1, false)).is_none());
        assert!(
            t.record_turn(snap(0, 1, false)).is_some(),
            "same file count across turns is a stall"
        );
    }

    /// A run that keeps the suite green while re-editing ONE file and
    /// closing nothing is the exact DS1 thrash case — correct operation,
    /// no convergence. It must still stall.
    ///
    /// This is a regression pin for a real interaction bug: the caller
    /// used to derive `verified_green` from the tier-aware status, whose
    /// staleness rule flips green→false on every post-green edit. That
    /// made each edit→test cycle produce a fresh false→true edge, resetting
    /// the counter forever and silently disabling the monitor for exactly
    /// the case it exists to catch. The caller must feed the LATCHED green.
    #[test]
    fn green_suite_thrash_on_one_file_still_stalls() {
        let t = ProgressTracker::new(3);
        // Arm: first file touched, suite green.
        assert!(t.record_turn(snap(2, 1, true)).is_none(), "arm");
        // Now edit→test→edit→test on the same file, board unchanged. With a
        // latched green these are all barren.
        assert!(t.record_turn(snap(2, 1, true)).is_none(), "1 barren");
        assert!(t.record_turn(snap(2, 1, true)).is_none(), "2 barren");
        assert!(
            t.record_turn(snap(2, 1, true)).is_some(),
            "green-but-not-converging must still stall"
        );
    }

    /// Green latches: once verification has been green, going green again
    /// after a red is progress, but staying green is not.
    #[test]
    fn staying_green_is_not_repeated_progress() {
        let t = ProgressTracker::new(2);
        assert!(t.record_turn(snap(0, 1, true)).is_none(), "arm + green");
        assert!(t.record_turn(snap(0, 1, true)).is_none(), "1 barren");
        assert!(
            t.record_turn(snap(0, 1, true)).is_some(),
            "still green isn't new progress"
        );
    }

    /// Bounded: at most `MAX_STALL_NUDGES` per run, and it re-arms for a
    /// full threshold between them so it can't spam.
    #[test]
    fn stall_is_bounded_and_re_arms() {
        let t = ProgressTracker::new(2);
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "arm");
        // First checkpoint.
        assert!(t.record_turn(snap(0, 1, false)).is_none());
        assert!(t.record_turn(snap(0, 1, false)).is_some(), "first");
        // Re-armed: needs another full threshold.
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "re-arm gap");
        assert!(t.record_turn(snap(0, 1, false)).is_some(), "second");
        // Budget spent — silent forever after.
        for _ in 0..10 {
            assert!(t.record_turn(snap(0, 1, false)).is_none(), "bounded");
        }
    }

    /// A threshold below 2 would fire on ordinary consecutive work.
    #[test]
    fn threshold_is_clamped_to_two() {
        let t = ProgressTracker::new(0);
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "arm");
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "1 barren");
        assert!(t.record_turn(snap(0, 1, false)).is_some(), "2 barren");
    }

    /// Budget notices fire once each, in order, at 60% and 85%.
    #[test]
    fn budget_marks_fire_once_each_in_order() {
        let t = ProgressTracker::new(3);
        assert!(t.poll_budget(50, 100).is_none(), "below the first mark");
        let first = text(t.poll_budget(60, 100).expect("60% mark"));
        assert!(first.contains(BUDGET_TAG), "carries the tag: {first}");
        assert!(first.contains("60 of 100"), "states position: {first}");
        assert!(first.contains("40 remain"), "states remaining: {first}");
        // Between marks: silent.
        assert!(t.poll_budget(70, 100).is_none());
        assert!(t.poll_budget(84, 100).is_none());
        let second = text(t.poll_budget(85, 100).expect("85% mark"));
        assert!(second.contains("85 of 100"), "{second}");
        // Both marks spent.
        assert!(t.poll_budget(99, 100).is_none());
    }

    /// A run that jumps straight past both marks still gets them one at a
    /// time — never two messages from one poll.
    #[test]
    fn budget_marks_never_double_fire_in_one_poll() {
        let t = ProgressTracker::new(3);
        assert!(t.poll_budget(90, 100).is_some(), "first mark");
        assert!(t.poll_budget(90, 100).is_some(), "then the second");
        assert!(t.poll_budget(90, 100).is_none(), "and no more");
    }

    /// User steering resets the loop's turn counter to give a fresh budget
    /// (dirge-st8r). The marks are deliberately NOT re-armed by that: the
    /// notice is a once-per-run orientation, and re-announcing the same
    /// thresholds every time the user types would be pure noise.
    #[test]
    fn budget_marks_do_not_re_arm_after_a_turn_counter_reset() {
        let t = ProgressTracker::new(3);
        assert!(t.poll_budget(60, 100).is_some(), "60% mark");
        assert!(t.poll_budget(85, 100).is_some(), "85% mark");
        // Steering reset the counter — turns climb through both marks again.
        for used in [0, 60, 85, 99] {
            assert!(
                t.poll_budget(used, 100).is_none(),
                "spent marks stay spent across a reset (used={used})"
            );
        }
    }

    /// No turn cap configured → the budget signal is meaningless and must
    /// stay silent rather than dividing by zero.
    #[test]
    fn budget_silent_without_a_cap() {
        let t = ProgressTracker::new(3);
        assert!(t.poll_budget(1000, 0).is_none());
    }

    /// The two signals hold independent state: spending the stall budget
    /// must not affect budget notices, and vice versa.
    #[test]
    fn stall_and_budget_budgets_are_independent() {
        let t = ProgressTracker::new(2);
        assert!(t.record_turn(snap(0, 1, false)).is_none(), "arm");
        assert!(t.record_turn(snap(0, 1, false)).is_none());
        assert!(t.record_turn(snap(0, 1, false)).is_some(), "stall fired");
        assert!(t.poll_budget(60, 100).is_some(), "budget still available");
    }
}

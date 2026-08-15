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
//! Three signals, all cheap and all bounded:
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
//!   - **prologue** — the run has produced NOTHING at all and has passed
//!     `prologue_cap` barren boundaries (or `PROLOGUE_TOOL_MULTIPLE` times
//!     that many barren tool calls) → one checkpoint pushing for the
//!     smallest possible first write. See the arming note below.
//!
//! A *progress event* is any of: a todo item closed, a file mutated that
//! was never mutated before, or verification going green. Re-editing one
//! file is deliberately NOT progress — that is exactly the thrash being
//! watched for (and [`super::context_depth`] already covers the narrower
//! same-file case).
//!
//! **A boundary is only judged when the turn made a successful tool call**
//! (dirge-hwk9.7). Read the first paragraph again: this monitor exists for
//! *successful*, varied, useless calls, and the other guards own everything
//! else. It nevertheless counted every boundary, including turns that called
//! nothing and turns whose calls all failed — so a run's ENDGAME, where the
//! todos are closed, the files are touched and the green is latched, was
//! barren by definition on every boundary. A long enough run was guaranteed to
//! be told it had stalled while it was finishing successfully; measured twice,
//! on different models, both within 0.1s of the end.
//!
//! **The stall counter arms only after the first progress event.** A run
//! that opens with twenty reads is exploring, not stalling, and must not
//! be nudged for it; a run that produced something and then stopped
//! producing is the real signal. Without this the monitor would fire on
//! every research task.
//!
//! That rule is right, but before dirge-t5dh the prologue it creates had no
//! upper bound — so a run that produced nothing NEVER armed, and this
//! monitor was structurally incapable of reporting the one case it most
//! needed to. Observed: 60 turns and eight minutes of successful, varied
//! grep/read calls with nothing written, `progress_stall_threshold` set and
//! on, and no other guard able to see it (storm needs identical repeats,
//! the failure tracker needs errors, safe-state needs a failure streak).
//! The prologue signal bounds it: exploring is fine, exploring forever is
//! the thrash. Its message is deliberately distinct from the stall one —
//! "you have not produced anything yet" is a different diagnosis from "you
//! were producing and stopped", and collapsing them would tell a run that
//! had written files that it had written none.
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

/// Display tag prefixing the prologue checkpoint (dirge-t5dh). Distinct from
/// [`STALL_TAG`]: a stall means "you were producing and stopped", the prologue
/// means "you have not produced anything at all yet". See [`STALL_TAG`] for the
/// attribution scheme the UI keys on.
pub const PROLOGUE_TAG: &str = "[prologue]";

/// Upper bound on prologue checkpoints per run. The situation the message
/// describes ("nothing written yet") does not change between nudges, so one per
/// run is enough: the run either writes something (ending the prologue) or hits
/// the turn cap.
const MAX_PROLOGUE_NUDGES: u8 = 1;

/// PROVISIONAL default for the prologue boundary cap, applied in ONE place (the
/// config-to-tracker wiring) when `progress_prologue_cap` is absent.
/// dirge-5mtx.7 will replace this flat default by deriving the cap from
/// observed capability signals (turns and tool calls without a progress event,
/// weighted by tier). Chosen generously: the observed legitimate-research
/// ceiling is roughly twenty barren turns, so 24 leaves margin below the
/// 60-turn reconnaissance burn that motivated the fix.
pub const DEFAULT_PROLOGUE_CAP: usize = 24;

/// The prologue also counts barren TOOL CALLS, not just barren turn boundaries:
/// a turn that batches many grep/read calls is one boundary but many calls (the
/// granularity bug that hid the observed thrash, and the same class as the
/// verifier's batched-edit bug, 25d05324). This multiplies the boundary cap into
/// a tool-call cap so the finer signal trips first when a model thrashes inside
/// few turns.
const PROLOGUE_TOOL_MULTIPLE: usize = 4;

/// Which checkpoint the tracker is offering (dirge-hwk9.7). The two answer
/// different questions — "you were producing and stopped" versus "you have not
/// produced anything at all" — and the caller needs to know which without
/// re-deriving it from the message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointKind {
    /// Armed run, `stall_threshold` barren boundaries. See [`stall_message`].
    Stall,
    /// Unarmed run past `prologue_cap`. See [`prologue_message`].
    Prologue,
}

/// A checkpoint the tracker is OFFERING, not one it has spent.
///
/// [`ProgressTracker::record_turn`] used to do both at once, which made the
/// budget an account of *attempts* rather than of *deliveries*. The boundary
/// arbiter declines an offer for two reasons — a more specific guard owns the
/// situation, or the boundary belongs to the finalization arbiter — and in
/// both cases the model receives nothing, so a run's two stall checkpoints
/// were being spent on silence. The arbiter calls
/// [`ProgressTracker::commit`] only when the message is actually delivered;
/// a declined offer stands and is re-offered at the next boundary.
///
/// Same rule the completeness gate had to learn (dirge-2m68): a check that
/// could not run is not a check that ran.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub kind: CheckpointKind,
    pub message: LoopMessage,
}

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
    /// Cumulative SUCCESSFUL tool calls made this run.
    ///
    /// Two jobs. The prologue bound (dirge-t5dh) watches these in addition to
    /// barren turn boundaries: a turn that batches forty grep/read calls is one
    /// boundary but forty calls, so a turn-only counter would score the
    /// observed thrash as a single barren turn (same class as the verifier's
    /// batched-edit bug, 25d05324).
    ///
    /// And, since dirge-hwk9.7, it is what makes a boundary the monitor's to
    /// judge at all — see [`ProgressTracker::record_turn`]. That is why it
    /// counts SUCCESSFUL calls rather than all of them: this module's whole
    /// premise, stated at the top, is that it watches the failure mode no other
    /// guard can see — *successful*, varied, useless calls. Errors are the
    /// failure tracker's and the storm breaker's territory.
    pub successful_tool_calls: usize,
}

/// Per-session progress tracker. `Mutex<Inner>` so the turn-boundary poll
/// can reach it without `&mut LoopConfig` plumbing — mirrors
/// [`super::context_depth::FileTouchTracker`].
#[derive(Debug)]
pub struct ProgressTracker {
    inner: Mutex<Inner>,
    stall_threshold: usize,
    /// Barren turn boundaries (or, times [`PROLOGUE_TOOL_MULTIPLE`], barren
    /// tool calls) allowed before the prologue checkpoint fires on a run that
    /// has produced nothing yet. Constructor-set so dirge-5mtx.7 can derive it
    /// from observed signals.
    prologue_cap: usize,
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
    /// Barren turn boundaries elapsed while still in the prologue.
    prologue_boundaries: usize,
    /// Barren tool calls elapsed while still in the prologue.
    prologue_tool_calls: usize,
    /// Prologue checkpoints already spent this run.
    prologue_nudges: u8,
}

impl ProgressTracker {
    /// `stall_threshold` is the number of barren turn boundaries before a
    /// checkpoint fires. Clamped to at least 2 — a threshold of 0 or 1
    /// would fire on ordinary back-to-back work.
    pub fn new(stall_threshold: usize, prologue_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            stall_threshold: stall_threshold.max(2),
            // A cap of 0 would fire on the first barren boundary; clamp so a
            // misconfiguration never nudges opening research.
            prologue_cap: prologue_cap.max(1),
        })
    }

    /// Record a turn boundary. Returns a stall checkpoint when the run has
    /// gone `stall_threshold` boundaries with no progress event since the
    /// last one. Any progress event resets the counter and returns `None`.
    ///
    /// A boundary is only judged when the turn made at least one SUCCESSFUL
    /// tool call (dirge-hwk9.7). The module's premise, stated at the top, is
    /// "*successful*, *varied*, useless tool calls" — the one failure mode no
    /// other guard can see. Until now the counters advanced on every boundary,
    /// including two kinds that are not that:
    ///
    ///   - a turn with NO tool calls. The model wrote prose; there is nothing
    ///     to thrash. In practice this is the final answer, which is why a
    ///     successful run ended up being told it had stalled — twice, on
    ///     different models, both within 0.1s of the run finishing.
    ///   - a turn whose calls all FAILED. That is the failure tracker's and the
    ///     storm breaker's territory, and scoring it here both double-counts
    ///     and produces a message that asserts something false: the stall text
    ///     says "the calls are succeeding but the work isn't converging".
    ///     Measured after the fix above: the model was told to re-run its tests
    ///     without a pipe, the re-run was denied by the permission layer, and
    ///     the resulting barren boundary tripped the stall — so the run's last
    ///     words were spent arguing that nothing was blocking it.
    ///
    /// This does not weaken the case the monitor exists for. A model thrashing
    /// on one file runs edits and tests that SUCCEED, so those boundaries are
    /// still judged and still stall — pinned by
    /// `green_suite_thrash_on_one_file_still_stalls`.
    pub fn record_turn(&self, snap: ProgressSnapshot) -> Option<Checkpoint> {
        let mut inner = self.inner.lock_ignore_poison();
        let progressed = snap.todos_unfinished < inner.last.todos_unfinished
            || snap.files_touched > inner.last.files_touched
            || (snap.verified_green && !inner.last.verified_green);
        // Successful calls made during THIS boundary = run-total delta since
        // the last one. record_turn is called once per turn boundary, so a turn
        // that batches forty grep/read calls counts as forty here, not one.
        let tool_delta = snap
            .successful_tool_calls
            .saturating_sub(inner.last.successful_tool_calls);
        inner.last = snap;
        // Not this monitor's boundary to judge: no work happened on it that
        // could have converged. Update the snapshot (done above) and leave
        // every counter alone — advancing them would just defer the same wrong
        // message to the next boundary, which is what standing down at the
        // boundary alone was measured doing.
        if !progressed && tool_delta == 0 {
            return None;
        }
        if progressed {
            inner.armed = true;
            inner.barren_turns = 0;
            // Producing ends the prologue.
            inner.prologue_boundaries = 0;
            inner.prologue_tool_calls = 0;
            return None;
        }
        if !inner.armed {
            // Exploration prologue (dirge-t5dh). The arming rule is right —
            // a run that opens with twenty reads is exploring, and nagging
            // it would fire on every research task — but before this the
            // prologue had NO upper bound, so a run that never produced
            // anything could never be reported at all. That is the case
            // this catches: 60 turns, ~40 successful reads, nothing written.
            inner.prologue_boundaries += 1;
            inner.prologue_tool_calls += tool_delta;
            if inner.prologue_nudges >= MAX_PROLOGUE_NUDGES {
                return None;
            }
            // Either counter can trip it. The tool-call arm exists because
            // record_turn is called once per BOUNDARY: a turn batching forty
            // reads is one barren boundary but forty barren calls, and the
            // models that batch hardest are the ones that thrash.
            let by_boundaries = inner.prologue_boundaries >= self.prologue_cap;
            let by_tool_calls =
                inner.prologue_tool_calls >= self.prologue_cap * PROLOGUE_TOOL_MULTIPLE;
            if !by_boundaries && !by_tool_calls {
                return None;
            }
            // OFFERED, not spent. `commit` resets these counters and charges
            // the budget when the arbiter actually delivers the message; until
            // then the offer stands and re-appears at the next boundary.
            return Some(Checkpoint {
                kind: CheckpointKind::Prologue,
                message: prologue_message(),
            });
        }
        inner.barren_turns += 1;
        if inner.barren_turns < self.stall_threshold || inner.stall_nudges >= MAX_STALL_NUDGES {
            return None;
        }
        Some(Checkpoint {
            kind: CheckpointKind::Stall,
            message: stall_message(self.stall_threshold),
        })
    }

    /// Spend the budget for a checkpoint the arbiter actually DELIVERED, and
    /// re-arm its counter so the next one needs a full threshold again.
    ///
    /// See [`Checkpoint`] for why this is separate from the offer.
    pub fn commit(&self, kind: CheckpointKind) {
        let mut inner = self.inner.lock_ignore_poison();
        match kind {
            CheckpointKind::Stall => {
                inner.stall_nudges += 1;
                inner.barren_turns = 0;
            }
            CheckpointKind::Prologue => {
                inner.prologue_nudges += 1;
                inner.prologue_boundaries = 0;
                inner.prologue_tool_calls = 0;
            }
        }
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

/// The prologue checkpoint (dirge-t5dh). Distinct from [`stall_message`]:
/// this fires when the run has produced NOTHING yet (never armed), not when it
/// produced and then stopped. The wording pushes toward the smallest possible
/// first write -- the goal is to get something on disk and iterate, not to
/// analyse further.
fn prologue_message() -> LoopMessage {
    LoopMessage::User(UserMessage::text(format!(
        "{PROLOGUE_TAG} You've been reading and calling tools for a while without writing a \
         file, closing a task, or getting a green check. At this point more analysis is the \
         failure mode, not the way out of it. Pick the smallest piece of the goal and put it \
         on disk now — a stub, a first test, anything concrete — then iterate. You can refine \
         what's written; you can't refine what isn't. If you genuinely can't start because \
         something is missing, say what it is and stop rather than reading further."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives boundaries the way a run does.
    ///
    /// Every helper except [`Driver::idle`] puts at least one SUCCESSFUL tool
    /// call on the boundary, because since dirge-hwk9.7 that is what makes a
    /// boundary this monitor's to judge at all — a turn that called nothing,
    /// or whose calls all failed, belongs to some other guard. The cumulative
    /// count lives here rather than in each test so a test reads as "another
    /// working turn happened" rather than as arithmetic.
    struct Driver {
        t: Arc<ProgressTracker>,
        calls: std::cell::Cell<usize>,
    }

    impl Driver {
        fn new(stall_threshold: usize, prologue_cap: usize) -> Self {
            Self {
                t: ProgressTracker::new(stall_threshold, prologue_cap),
                calls: std::cell::Cell::new(0),
            }
        }

        /// `todos` is the UNFINISHED count — progress is a decrease.
        fn snap(&self, todos: usize, files: usize, green: bool, calls: usize) -> ProgressSnapshot {
            self.calls.set(self.calls.get() + calls);
            ProgressSnapshot {
                todos_unfinished: todos,
                files_touched: files,
                verified_green: green,
                successful_tool_calls: self.calls.get(),
            }
        }

        /// One ordinary working boundary, DELIVERING whatever it offers — the
        /// arbiter's normal path. The behaviour tests read against this.
        fn boundary(&self, todos: usize, files: usize, green: bool) -> Option<Checkpoint> {
            self.deliver(self.snap(todos, files, green, 1))
        }

        /// A boundary that batched `calls` successful tool calls.
        fn batched(
            &self,
            todos: usize,
            files: usize,
            green: bool,
            calls: usize,
        ) -> Option<Checkpoint> {
            self.deliver(self.snap(todos, files, green, calls))
        }

        /// A boundary with NO successful tool call — the model answered, or
        /// every call it made failed.
        fn idle(&self, todos: usize, files: usize, green: bool) -> Option<Checkpoint> {
            self.deliver(self.snap(todos, files, green, 0))
        }

        /// A working boundary whose offer the arbiter DECLINES: the message is
        /// not sent, so its budget must not be charged.
        fn declined(&self, todos: usize, files: usize, green: bool) -> Option<Checkpoint> {
            self.t.record_turn(self.snap(todos, files, green, 1))
        }

        fn deliver(&self, snap: ProgressSnapshot) -> Option<Checkpoint> {
            let offer = self.t.record_turn(snap);
            if let Some(c) = &offer {
                self.t.commit(c.kind);
            }
            offer
        }
    }

    fn text(msg: LoopMessage) -> String {
        match msg {
            LoopMessage::User(u) => u.text_joined(),
            _ => panic!("expected a user message"),
        }
    }

    fn is_prologue(cp: &Checkpoint) -> bool {
        cp.kind == CheckpointKind::Prologue
    }

    // ── the exploration prologue (dirge-t5dh) ────────────────────────────

    /// The exploration prologue must not fire BELOW the cap. A run that has
    /// produced nothing yet is reading, not thrashing — this is the guard
    /// against nagging every research task.
    #[test]
    fn exploration_prologue_does_not_stall_below_the_cap() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        for _ in 0..(DEFAULT_PROLOGUE_CAP - 1) {
            assert!(d.boundary(0, 0, false).is_none());
        }
    }

    /// Fires exactly AT the cap, never before.
    #[test]
    fn prologue_fires_at_the_cap_and_not_before() {
        let d = Driver::new(3, 5);
        for i in 1..5 {
            assert!(
                d.boundary(0, 0, false).is_none(),
                "barren boundary {i} is still under the cap"
            );
        }
        let msg = d
            .boundary(0, 0, false)
            .expect("the 5th barren boundary hits the cap");
        assert!(is_prologue(&msg), "must carry the prologue tag");
    }

    /// A run that produced early and then stalled gets the STALL message,
    /// never the prologue one. The two answer different questions and must
    /// not collapse into one.
    #[test]
    fn produced_then_stalled_is_a_stall_not_a_prologue() {
        let d = Driver::new(2, 5);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none(), "1 barren");
        let msg = d
            .declined(0, 1, false)
            .expect("2 barren turns hits the stall threshold");
        assert!(
            !is_prologue(&msg),
            "an armed run stalls; it is not in the prologue"
        );
        match &msg.message {
            LoopMessage::User(u) => assert!(u.text_joined().starts_with(STALL_TAG)),
            _ => panic!("expected a user message"),
        }
    }

    /// The tool-call arm trips independently of the boundary arm. This is the
    /// case that mattered: the observed thrash batched 40+ calls into ONE
    /// turn, so a boundary-only counter scored it as a single barren turn.
    /// Same granularity bug as the verifier's batched-edit miss (25d05324).
    #[test]
    fn prologue_trips_on_batched_tool_calls_within_few_boundaries() {
        let d = Driver::new(3, 10);
        // Two boundaries only — far below the cap of 10 — but each batches a
        // pile of reads, crossing 10 * PROLOGUE_TOOL_MULTIPLE tool calls.
        assert!(
            d.batched(0, 0, false, 20).is_none(),
            "20 calls is under 10*{PROLOGUE_TOOL_MULTIPLE}"
        );
        let msg = d
            .batched(0, 0, false, 20)
            .expect("40 barren tool calls crosses the tool-call arm");
        assert!(is_prologue(&msg));
    }

    /// Bounded per run: it cannot spam a run that keeps reading.
    #[test]
    fn prologue_is_bounded_per_run() {
        let d = Driver::new(3, 2);
        let mut fired = 0;
        for _ in 0..40 {
            if let Some(m) = d.boundary(0, 0, false)
                && is_prologue(&m)
            {
                fired += 1;
            }
        }
        assert_eq!(
            fired, MAX_PROLOGUE_NUDGES as usize,
            "prologue checkpoints must be bounded"
        );
    }

    /// Producing something ends the prologue: the counters reset, so a later
    /// barren stretch is judged as a stall (armed) rather than re-triggering
    /// the "you haven't written anything" message, which would then be false.
    #[test]
    fn producing_ends_the_prologue() {
        let d = Driver::new(3, 4);
        assert!(d.boundary(0, 0, false).is_none());
        assert!(d.boundary(0, 0, false).is_none());
        // A file lands — the run has produced.
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        // Now go barren well past the prologue cap; every message must be a
        // stall, never a prologue.
        for _ in 0..12 {
            if let Some(m) = d.boundary(0, 1, false) {
                assert!(
                    !is_prologue(&m),
                    "a run that produced can never be told it produced nothing"
                );
            }
        }
    }

    // ── the stall signal ─────────────────────────────────────────────────

    /// Once something has been produced, barren turns count — and the
    /// checkpoint fires exactly at the threshold, not before.
    #[test]
    fn stall_fires_at_threshold_after_arming() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none(), "1 barren");
        assert!(d.boundary(0, 1, false).is_none(), "2 barren");
        let msg = d
            .boundary(0, 1, false)
            .expect("3 barren turns hits the threshold");
        let body = text(msg.message);
        assert!(body.contains(STALL_TAG), "carries the tag: {body}");
        assert!(body.contains("blocking"), "asks for a diagnosis: {body}");
    }

    /// Each kind of progress event independently resets the counter.
    #[test]
    fn any_progress_event_resets_the_counter() {
        for (label, todos, files, green) in [("new file", 0, 2, false), ("went green", 0, 1, true)]
        {
            let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
            assert!(d.boundary(0, 1, false).is_none(), "arm");
            assert!(d.boundary(0, 1, false).is_none());
            assert!(d.boundary(0, 1, false).is_none());
            // Progress on the turn that would otherwise have tripped it.
            assert!(
                d.boundary(todos, files, green).is_none(),
                "{label} must reset, not fire"
            );
            // …and the counter really did restart.
            assert!(d.boundary(todos, files, green).is_none(), "{label} +1");
            assert!(d.boundary(todos, files, green).is_none(), "{label} +2");
        }
    }

    /// Closing a todo is progress: the mirror drops terminal items, so a
    /// completed item shows up as one FEWER unfinished.
    #[test]
    fn closing_a_todo_is_progress() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(3, 1, false).is_none(), "arm");
        assert!(d.boundary(3, 1, false).is_none(), "1 barren");
        // Item closed: 3 unfinished → 2.
        assert!(
            d.boundary(2, 1, false).is_none(),
            "a closed item resets the counter"
        );
        assert!(d.boundary(2, 1, false).is_none(), "counter restarted");
    }

    /// Writing MORE todos is planning, not progress. A model that answers
    /// "do the work" with another `write_todo_list` must not clear the
    /// stall counter by doing so.
    #[test]
    fn adding_todos_is_not_progress() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(1, 1, false).is_none(), "arm");
        // Board grows: 1 unfinished → 5. Planning, not progress.
        assert!(d.boundary(5, 1, false).is_none(), "1 barren");
        assert!(
            d.boundary(9, 1, false).is_some(),
            "growing the board doesn't count as getting work done"
        );
    }

    /// Re-editing the SAME file is not progress — that's the thrash being
    /// watched for. `files_touched` is a distinct-file count, so a flat
    /// value across turns means no new ground.
    #[test]
    fn re_editing_one_file_is_not_progress() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none());
        assert!(d.boundary(0, 1, false).is_none());
        assert!(
            d.boundary(0, 1, false).is_some(),
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
    ///
    /// It is ALSO the control for dirge-hwk9.7: those edit→test turns make
    /// SUCCESSFUL calls, so narrowing the monitor to boundaries that did work
    /// leaves this case exactly where it was.
    #[test]
    fn green_suite_thrash_on_one_file_still_stalls() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        // Arm: first file touched, suite green.
        assert!(d.boundary(2, 1, true).is_none(), "arm");
        // Now edit→test→edit→test on the same file, board unchanged. With a
        // latched green these are all barren.
        assert!(d.boundary(2, 1, true).is_none(), "1 barren");
        assert!(d.boundary(2, 1, true).is_none(), "2 barren");
        assert!(
            d.boundary(2, 1, true).is_some(),
            "green-but-not-converging must still stall"
        );
    }

    /// Green latches: once verification has been green, going green again
    /// after a red is progress, but staying green is not.
    #[test]
    fn staying_green_is_not_repeated_progress() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, true).is_none(), "arm + green");
        assert!(d.boundary(0, 1, true).is_none(), "1 barren");
        assert!(
            d.boundary(0, 1, true).is_some(),
            "still green isn't new progress"
        );
    }

    /// Bounded: at most `MAX_STALL_NUDGES` per run, and it re-arms for a
    /// full threshold between them so it can't spam.
    #[test]
    fn stall_is_bounded_and_re_arms() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        // First checkpoint.
        assert!(d.boundary(0, 1, false).is_none());
        assert!(d.boundary(0, 1, false).is_some(), "first");
        // Re-armed: needs another full threshold.
        assert!(d.boundary(0, 1, false).is_none(), "re-arm gap");
        assert!(d.boundary(0, 1, false).is_some(), "second");
        // Budget spent — silent forever after.
        for _ in 0..10 {
            assert!(d.boundary(0, 1, false).is_none(), "bounded");
        }
    }

    /// A threshold below 2 would fire on ordinary consecutive work.
    #[test]
    fn threshold_is_clamped_to_two() {
        let d = Driver::new(0, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none(), "1 barren");
        assert!(d.boundary(0, 1, false).is_some(), "2 barren");
    }

    // ── dirge-hwk9.7: which boundaries are this monitor's to judge ───────
    //
    // The module's premise is "*successful*, *varied*, useless tool calls" —
    // the failure mode no other guard can see. The counters advanced on every
    // boundary regardless, including boundaries where no work happened at all.
    // A run's endgame is made of those: the todos are closed (cannot decrease),
    // the files are touched (cannot increase) and the green is latched (no
    // fresh edge), so every endgame boundary is barren by definition and a
    // long enough run was GUARANTEED to be told it had stalled as it finished.
    // Measured on two models, both within 0.1s of the run ending.

    /// A turn that made no successful call is not a stalled turn. The model
    /// wrote prose — in practice, its final answer.
    #[test]
    fn a_turn_with_no_successful_call_is_not_a_barren_turn() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none(), "1 barren");
        // Twenty answer-only boundaries. Under the old contract the second one
        // would have tripped the threshold.
        for i in 0..20 {
            assert!(
                d.idle(0, 1, false).is_none(),
                "idle boundary {i} must not count toward a stall"
            );
        }
        // …and the evidence that WAS earned is still there: one more working
        // boundary trips it, so this narrows the monitor rather than muting it.
        assert!(d.boundary(0, 1, false).is_some(), "real work still counts");
    }

    /// A turn whose calls all FAILED is the failure tracker's and the storm
    /// breaker's business. Scoring it here double-counts, and the stall text
    /// would assert something false — it says the calls are succeeding.
    ///
    /// Measured: the harness told a model to re-run its tests without a pipe,
    /// the permission layer denied the re-run, and the resulting barren
    /// boundary tripped the stall.
    #[test]
    fn a_turn_whose_calls_all_failed_is_not_a_barren_turn() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none(), "1 barren");
        // An errored call never reaches `successful_tool_calls`, so the
        // boundary looks the same as an idle one to this monitor.
        for i in 0..10 {
            assert!(
                d.idle(0, 1, false).is_none(),
                "all-errored boundary {i} belongs to the failure tracker"
            );
        }
    }

    /// The prologue narrows the same way: a run that has written nothing AND
    /// called nothing is not thrashing on reads, it is answering.
    #[test]
    fn idle_boundaries_do_not_advance_the_prologue() {
        let d = Driver::new(3, 3);
        for i in 0..20 {
            assert!(
                d.idle(0, 0, false).is_none(),
                "idle boundary {i} must not advance the prologue"
            );
        }
        // Three boundaries of real reading still trips it.
        assert!(d.boundary(0, 0, false).is_none());
        assert!(d.boundary(0, 0, false).is_none());
        assert!(
            d.boundary(0, 0, false).is_some_and(|c| is_prologue(&c)),
            "reading without producing is still the prologue"
        );
    }

    // ── dirge-hwk9.7: an offer is not a delivery ─────────────────────────
    //
    // The arbiter declines a progress checkpoint for two reasons: a more
    // specific guard owns the situation (the masked-verification decline), or
    // the boundary belongs to the finalization arbiter (a concluding turn).
    // In both cases the model receives NOTHING. Spending the budget there
    // meant a run's two stall checkpoints could be consumed entirely by
    // silence — measured on a live run, where the first stall was suppressed
    // by the masked-decline rule and the second landed 1.3s before the end.

    /// An offer the arbiter never commits keeps its budget, and stands: the
    /// same barren stretch re-offers at the next boundary rather than being
    /// consumed by a boundary that could not carry it.
    #[test]
    fn an_offer_the_arbiter_declines_keeps_its_budget() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.declined(0, 1, false).is_none(), "arm");
        assert!(d.declined(0, 1, false).is_none(), "1 barren");
        // Every boundary from here offers, and every one is declined. Under
        // the old spend-on-offer contract the budget ran out after two.
        for i in 0..8 {
            assert!(
                d.declined(0, 1, false)
                    .is_some_and(|c| c.kind == CheckpointKind::Stall),
                "boundary {i}: a declined offer must not spend the budget"
            );
        }
    }

    /// …and committing DOES spend it: two deliveries and the run is silent,
    /// which is the bound `MAX_STALL_NUDGES` exists to enforce.
    #[test]
    fn committing_an_offer_spends_the_budget() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none(), "1 barren");
        assert!(d.boundary(0, 1, false).is_some(), "first delivered");
        assert!(d.boundary(0, 1, false).is_none(), "re-arm gap");
        assert!(d.boundary(0, 1, false).is_some(), "second delivered");
        for _ in 0..8 {
            assert!(
                d.boundary(0, 1, false).is_none(),
                "the budget is spent by deliveries, and it is spent"
            );
        }
    }

    /// A commit re-arms: the next checkpoint needs another full threshold, so
    /// delivering one cannot produce a second on the very next boundary.
    #[test]
    fn committing_re_arms_the_counter() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none(), "1 barren");
        assert!(d.boundary(0, 1, false).is_none(), "2 barren");
        assert!(d.boundary(0, 1, false).is_some(), "3 barren fires");
        assert!(d.boundary(0, 1, false).is_none(), "re-armed: 1");
        assert!(d.boundary(0, 1, false).is_none(), "re-armed: 2");
        assert!(d.boundary(0, 1, false).is_some(), "re-armed: 3");
    }

    /// The prologue offer behaves the same way — a declined one keeps its
    /// single shot rather than burning it on a boundary that stayed silent.
    #[test]
    fn a_declined_prologue_offer_keeps_its_budget() {
        let d = Driver::new(3, 2);
        assert!(d.declined(0, 0, false).is_none(), "1 barren");
        for i in 0..6 {
            assert!(
                d.declined(0, 0, false)
                    .is_some_and(|c| c.kind == CheckpointKind::Prologue),
                "boundary {i}: a declined prologue offer must not spend its shot"
            );
        }
        // Delivering it once spends the only one there is.
        d.t.commit(CheckpointKind::Prologue);
        for _ in 0..6 {
            assert!(d.declined(0, 0, false).is_none(), "spent");
        }
    }

    /// Progress still clears a standing offer: a run that starts converging
    /// again must not be handed a stall diagnosis it earned three turns ago.
    #[test]
    fn progress_clears_a_standing_offer() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.declined(0, 1, false).is_none(), "arm");
        assert!(d.declined(0, 1, false).is_none(), "1 barren");
        assert!(d.declined(0, 1, false).is_some(), "offer stands");
        // A new file lands — the run is converging again.
        assert!(d.declined(0, 2, false).is_none(), "progress clears");
        assert!(d.declined(0, 2, false).is_none(), "counter restarted");
    }

    // ── the budget countdown ─────────────────────────────────────────────

    /// The two signals hold independent state: spending the stall budget
    /// must not affect budget notices, and vice versa.
    #[test]
    fn stall_and_budget_budgets_are_independent() {
        let d = Driver::new(2, DEFAULT_PROLOGUE_CAP);
        assert!(d.boundary(0, 1, false).is_none(), "arm");
        assert!(d.boundary(0, 1, false).is_none());
        assert!(d.boundary(0, 1, false).is_some(), "stall fired");
        assert!(d.t.poll_budget(60, 100).is_some(), "budget still available");
    }

    /// Budget notices fire once each, in order, at 60% and 85%.
    #[test]
    fn budget_marks_fire_once_each_in_order() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        assert!(d.t.poll_budget(50, 100).is_none(), "below the first mark");
        let first = text(d.t.poll_budget(60, 100).expect("60% mark"));
        assert!(first.contains(BUDGET_TAG), "carries the tag: {first}");
        assert!(first.contains("60 of 100"), "states position: {first}");
        assert!(first.contains("40 remain"), "states remaining: {first}");
        // Between marks: silent.
        assert!(d.t.poll_budget(70, 100).is_none());
        assert!(d.t.poll_budget(84, 100).is_none());
        let second = text(d.t.poll_budget(85, 100).expect("85% mark"));
        assert!(second.contains("85 of 100"), "{second}");
        // Both marks spent.
        assert!(d.t.poll_budget(99, 100).is_none());
    }

    /// A run that jumps straight past both marks still gets them one at a
    /// time — never two messages from one poll.
    #[test]
    fn budget_marks_never_double_fire_in_one_poll() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        assert!(d.t.poll_budget(90, 100).is_some(), "first mark");
        assert!(d.t.poll_budget(90, 100).is_some(), "then the second");
        assert!(d.t.poll_budget(90, 100).is_none(), "and no more");
    }

    /// User steering resets the loop's turn counter to give a fresh budget
    /// (dirge-st8r). The marks are deliberately NOT re-armed by that: the
    /// notice is a once-per-run orientation, and re-announcing the same
    /// thresholds every time the user types would be pure noise.
    #[test]
    fn budget_marks_do_not_re_arm_after_a_turn_counter_reset() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        assert!(d.t.poll_budget(60, 100).is_some(), "60% mark");
        assert!(d.t.poll_budget(85, 100).is_some(), "85% mark");
        // Steering reset the counter — turns climb through both marks again.
        for used in [0, 60, 85, 99] {
            assert!(
                d.t.poll_budget(used, 100).is_none(),
                "spent marks stay spent across a reset (used={used})"
            );
        }
    }

    /// No turn cap configured → the budget signal is meaningless and must
    /// stay silent rather than dividing by zero.
    #[test]
    fn budget_silent_without_a_cap() {
        let d = Driver::new(3, DEFAULT_PROLOGUE_CAP);
        assert!(d.t.poll_budget(1000, 0).is_none());
    }
}

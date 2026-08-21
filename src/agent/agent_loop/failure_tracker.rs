//! Cross-turn failure recovery — a reflection nudge for repeated,
//! *distinct* tool errors.
//!
//! The storm breaker ([`super::storm`]) catches a model stuck repeating
//! the SAME call. It does nothing for a model that fails *differently*
//! every turn — edit-miss, then wrong path, then a bad argument, then
//! another edit-miss. Each call is unique, so storm never trips, and
//! weaker models can burn a long run thrashing without ever stepping
//! back to diagnose.
//!
//! `FailureTracker` counts *consecutive* errored tool results (across
//! turns — it is NOT reset at turn boundaries like the storm window).
//! When the streak reaches `threshold`, it injects one structured
//! "recovery checkpoint" asking the model to name the shared root cause
//! and try a DIFFERENT approach before retrying. The literature on
//! tool-call repair for smaller models (structured-reflection work,
//! arXiv:2509.18847 / 2509.25238) finds the gains concentrate over the
//! first few corrective attempts, so the nudge fires early (default 3)
//! and re-arms every further `threshold` failures rather than spamming
//! once per errored call. Any successful tool result clears the streak.
//!
//! The threshold is read at every poll, not fixed at construction
//! (dirge-z85a): [`super::capability::CapabilityTier::Struggling`] scales it
//! down so a visibly failing run gets the checkpoint sooner (3 → 2).
//! `Nominal` and `Strong` are bit-identical to the base. This is the one
//! guard whose input and trigger are the same observation — the estimator is
//! built from failure counts and streaks, and this fires on consecutive
//! errored results — which is what licenses the derivation at all.
//!
//! Self-contained — no rig/LLM state. Lives as a local in
//! [`super::run`]; when the loop never wires it, behaviour is
//! unchanged.

use std::sync::{Arc, Mutex};

use crate::sync_util::LockExt;

use super::capability::CapabilityTier;
use super::message::{LoopMessage, UserMessage};

/// How many recent failures to quote back in the checkpoint body.
const MAX_QUOTED: usize = 5;
/// Per-error excerpt cap (single line) so the nudge stays compact.
const EXCERPT_CAP: usize = 160;
/// Floor on the effective threshold, tier or no tier (dirge-z85a). Mirrors the
/// `>= 2` construction invariant: at 1 the checkpoint fires on the FIRST
/// errored call, which contradicts this module's premise — *repeated*, distinct
/// failures — and would nudge on every isolated transient.
const MIN_EFFECTIVE_THRESHOLD: usize = 2;

/// Per-session consecutive-failure tracker. `Mutex<Inner>` so the
/// record hook (tool dispatch) and the poll hook (turn boundary) can
/// both reach it without `&mut` plumbing — mirrors
/// [`super::context_depth::FileTouchTracker`].
#[derive(Debug)]
pub struct FailureTracker {
    inner: Mutex<Inner>,
    threshold: usize,
}

#[derive(Debug)]
struct Inner {
    /// Consecutive errored tool *calls*, reset by any success. Drives
    /// the "{n} tool calls in a row have failed" wording — a truthful
    /// call count, distinct from the weighted escalation below.
    consecutive: usize,
    /// Weighted streak score: a plain error adds 1, a timeout adds 2.
    /// The nudge fires off this, not the raw count, so expensive
    /// failures (a command that burned its whole time budget) escalate
    /// sooner — two timeouts trip a threshold-3 tracker, where two plain
    /// errors would not. For error-only streaks escalation == consecutive,
    /// so the legacy behaviour is unchanged.
    escalation: usize,
    /// Timeouts within the current streak, used to add a targeted line to
    /// the checkpoint ("retrying a hung command won't help").
    timeouts: usize,
    /// `(tool_name, excerpt)` for the most recent failures in the
    /// current streak, bounded to `MAX_QUOTED`.
    recent: Vec<(String, String)>,
    /// dirge-61sv: the recovery class of each failure in `recent`, in the same
    /// order and bounded the same way. Kept parallel rather than folded into
    /// the tuple so the excerpt list stays the shape `recent_excerpts` already
    /// hands to the safe-state replan.
    recent_classes: Vec<super::tool_error_class::ErrorClass>,
    /// Escalation score at the last emitted checkpoint; 0 = none emitted
    /// for this streak. Re-arm only after another `threshold` of
    /// escalation so a stubborn streak gets periodic — not per-call —
    /// nudges.
    last_emitted_at: usize,
    /// dirge-iwwq: consecutive permission/approval denials, a SEPARATE
    /// streak from the mechanical one above. A denial is a policy wall the
    /// model cannot retry around, so it must not feed `escalation` (which
    /// drives the "try a DIFFERENT approach" nudge). Reset by any success,
    /// like the mechanical streak; untouched by mechanical errors.
    denials: usize,
    /// `(tool_name, excerpt)` for recent denials, bounded to `MAX_QUOTED`,
    /// quoted in the permission checkpoint.
    recent_denials: Vec<(String, String)>,
    /// Denial-streak score at the last emitted permission checkpoint;
    /// re-arm mirrors `last_emitted_at`.
    last_denial_emitted_at: usize,
}

impl FailureTracker {
    /// Build a tracker that nudges once a streak of `threshold`
    /// consecutive failures is reached. `threshold` must be >= 2.
    pub fn new(threshold: usize) -> Arc<Self> {
        assert!(
            threshold >= 2,
            "failure tracker threshold must be >= 2 (got {threshold})"
        );
        Arc::new(Self {
            threshold,
            inner: Mutex::new(Inner {
                consecutive: 0,
                escalation: 0,
                timeouts: 0,
                recent: Vec::new(),
                recent_classes: Vec::new(),
                last_emitted_at: 0,
                denials: 0,
                recent_denials: Vec::new(),
                last_denial_emitted_at: 0,
            }),
        })
    }

    /// Record one tool result by [`Outcome`]. A success clears the
    /// streak; an error or timeout extends it (a timeout counting double
    /// toward the escalation score) and remembers a short excerpt for the
    /// checkpoint.
    pub fn record(&self, outcome: super::activity::Outcome, tool_name: &str, excerpt: &str) {
        use super::activity::Outcome;
        let mut inner = self.inner.lock_ignore_poison();
        match outcome {
            Outcome::Ok => {
                inner.consecutive = 0;
                inner.escalation = 0;
                inner.timeouts = 0;
                inner.recent.clear();
                inner.recent_classes.clear();
                inner.last_emitted_at = 0;
                inner.denials = 0;
                inner.recent_denials.clear();
                inner.last_denial_emitted_at = 0;
                return;
            }
            // dirge-iwwq: a denial is a policy wall, tracked on its own
            // streak. It neither extends nor resets the mechanical streak —
            // routing the model toward "a DIFFERENT approach" here is the
            // bug. It gets its own permission checkpoint instead.
            Outcome::Denied => {
                inner.denials += 1;
                inner
                    .recent_denials
                    .push((tool_name.to_string(), condense(excerpt)));
                if inner.recent_denials.len() > MAX_QUOTED {
                    let drop = inner.recent_denials.len() - MAX_QUOTED;
                    inner.recent_denials.drain(0..drop);
                }
                return;
            }
            Outcome::Error => {
                inner.consecutive += 1;
                inner.escalation += 1;
            }
            Outcome::Timeout => {
                inner.consecutive += 1;
                inner.escalation += 2;
                inner.timeouts += 1;
            }
        }
        // dirge-61sv: remember WHAT KIND of failure this was, not just that it
        // was one. A streak of "no such file" and a streak of schema
        // rejections want opposite advice, and the generic checkpoint gives
        // neither. Bounded by the same MAX_QUOTED window as the excerpts so a
        // long streak cannot grow this without limit.
        inner
            .recent_classes
            .push(super::tool_error_class::classify(tool_name, excerpt));
        if inner.recent_classes.len() > MAX_QUOTED {
            let drop = inner.recent_classes.len() - MAX_QUOTED;
            inner.recent_classes.drain(0..drop);
        }
        inner
            .recent
            .push((tool_name.to_string(), condense(excerpt)));
        if inner.recent.len() > MAX_QUOTED {
            let drop = inner.recent.len() - MAX_QUOTED;
            inner.recent.drain(0..drop);
        }
    }

    /// Back-compat shim: record a result by its error flag (no timeout
    /// distinction). Kept for call sites / tests that only know
    /// success-vs-error; prefer [`FailureTracker::record`] where the
    /// outcome is classified.
    #[cfg(test)]
    pub fn record_result(&self, is_error: bool, tool_name: &str, excerpt: &str) {
        use super::activity::Outcome;
        let outcome = if is_error {
            Outcome::Error
        } else {
            Outcome::Ok
        };
        self.record(outcome, tool_name, excerpt);
    }

    /// The threshold this poll evaluates at (dirge-z85a).
    ///
    /// Read per evaluation rather than baked in at construction. The tracker is
    /// built at run start, where the estimator is always `Nominal` by warm-up
    /// (`MIN_CALLS_FOR_ESTIMATE` tool calls), so deriving it at the
    /// construction site would read the neutral tier every time and be inert.
    ///
    /// Of the loop's movable constants this is the one whose *signal* and
    /// *trigger* are the same thing: the estimator is built from failure counts
    /// and streaks, and this guard fires on consecutive errored results. That
    /// match is the whole justification — see `docs/verification-discipline.md`
    /// ("a signal may only tune a guard that fires on the same thing the signal
    /// measures").
    ///
    /// `Nominal` and `Strong` return the base bit-identically; only
    /// `Struggling` moves it, and only earlier (base 3 → 2), floored at
    /// [`MIN_EFFECTIVE_THRESHOLD`].
    fn effective_threshold(&self, tier: CapabilityTier) -> usize {
        let base = u32::try_from(self.threshold).unwrap_or(u32::MAX);
        (tier.scale_threshold(base) as usize).max(MIN_EFFECTIVE_THRESHOLD)
    }

    /// Poll hook: returns one recovery-checkpoint message when the
    /// streak has reached the tier's effective threshold and we haven't
    /// nudged since the last such interval; otherwise empty.
    ///
    /// `tier` scales the MECHANICAL streak only. The permission checkpoint
    /// below keeps the base threshold: a denial streak is a policy wall, and
    /// nothing [`super::capability::CapabilityCounters`] measures says anything
    /// about how often the user's rules block a call.
    pub fn poll_reflection(
        &self,
        tier: CapabilityTier,
    ) -> Vec<(LoopMessage, super::gate_tally::BoundaryNudge)> {
        let threshold = self.effective_threshold(tier);
        let mut inner = self.inner.lock_ignore_poison();
        let mut out = Vec::new();

        // dirge-iwwq: permission denials first — their own streak, re-armed
        // like the mechanical one. Distinct message: this is a wall the
        // model can't retry around, so don't send it back to "diagnose and
        // try a different approach".
        if inner.denials >= self.threshold {
            let due = inner.last_denial_emitted_at == 0
                || inner.denials.saturating_sub(inner.last_denial_emitted_at) >= self.threshold;
            if due {
                inner.last_denial_emitted_at = inner.denials;
                let body = format_permission_checkpoint(inner.denials, &inner.recent_denials);
                out.push((
                    LoopMessage::User(UserMessage::text(body)),
                    super::gate_tally::BoundaryNudge::PermissionCheckpoint,
                ));
            }
        }

        if inner.escalation >= threshold {
            // First crossing, or another full `threshold` of escalation since
            // the last nudge. Keyed on the weighted score so timeouts pull
            // the nudge forward.
            let due = inner.last_emitted_at == 0
                || inner.escalation.saturating_sub(inner.last_emitted_at) >= threshold;
            if due {
                inner.last_emitted_at = inner.escalation;
                let body = format_checkpoint(
                    inner.consecutive,
                    inner.timeouts,
                    &inner.recent,
                    &inner.recent_classes,
                );
                out.push((
                    LoopMessage::User(UserMessage::text(body)),
                    super::gate_tally::BoundaryNudge::ReflectionCheckpoint,
                ));
            }
        }
        out
    }

    /// Whether the failure streak has reached 2× the BASE recovery-checkpoint
    /// threshold (dirge-uw2l.4). This is the safe-state abort rung's
    /// escalation signal — it fires only when the model is deep in a failing
    /// streak that the rung-2 checkpoint has already nudged and failed to
    /// break. Uses the weighted `escalation` score (timeouts count double), so
    /// a stuck, timeout-heavy run trips it sooner. Read-only: it never mutates
    /// the tracker and never spends a checkpoint, so the boundary poll can
    /// consult it cheaply every iteration. Knowledge of `threshold` stays
    /// here (the tracker owns it; the safe-state engine never sees the raw
    /// number).
    ///
    /// Deliberately NOT tier-scaled (dirge-z85a), unlike
    /// [`Self::poll_reflection`]. The tier's one-directional rule is that it
    /// may add support, never take latitude away, and this rung is not
    /// support: it spends one of two hard-capped aborts per run and, in
    /// `auto` mode, restores files on the tree. Pulling that forward for a
    /// struggling model is a different decision from nudging it sooner, and
    /// there is no evidence for it.
    pub fn safe_state_due(&self) -> bool {
        let inner = self.inner.lock_ignore_poison();
        inner.escalation >= self.threshold.saturating_mul(2)
    }

    /// The `(tool, excerpt)` pairs for the most recent failures in the current
    /// streak (dirge-uw2l.4). Cloned out (bounded to `MAX_QUOTED`) so the
    /// safe-state replan message can quote what already failed without the
    /// re-plan repeating it. Empty once a success clears the streak.
    pub fn recent_excerpts(&self) -> Vec<(String, String)> {
        self.inner.lock_ignore_poison().recent.clone()
    }

    /// Current consecutive errored-tool-result streak — the mechanical,
    /// unweighted call count, not the escalation score. Feeds the
    /// capability tally's high-water mark (dirge-5mtx.1).
    pub fn consecutive(&self) -> usize {
        self.inner.lock_ignore_poison().consecutive
    }
}

/// Collapse an excerpt to a single bounded line for the checkpoint.
fn condense(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > EXCERPT_CAP {
        let kept: String = one_line.chars().take(EXCERPT_CAP).collect();
        format!("{kept}…")
    } else {
        one_line
    }
}

/// Build the recovery-checkpoint body. Free fn so tests pin the wording.
fn format_checkpoint(
    consecutive: usize,
    timeouts: usize,
    recent: &[(String, String)],
    classes: &[super::tool_error_class::ErrorClass],
) -> String {
    let mut s = format!("[Recovery checkpoint] {consecutive} tool calls in a row have failed:\n");
    for (tool, excerpt) in recent {
        s.push_str(&format!("  - {tool}: {excerpt}\n"));
    }
    // Timeouts are a distinct failure mode from "wrong arguments": the
    // command ran to its time limit. Re-issuing it verbatim just burns
    // the budget again, so call it out specifically.
    if timeouts > 0 {
        s.push_str(&format!(
            "{timeouts} of these timed out — the command ran out its time budget, it didn't \
             fail on bad input. Re-running it unchanged will hang again: narrow the work, fix \
             why it hangs, or raise the timeout deliberately — don't just retry.\n",
        ));
    }
    // dirge-61sv: when the streak has a single character, say so and give the
    // instruction that fits it. Placed BEFORE the generic list because the
    // specific direction is the useful part; the generic questions stay as the
    // fallback for a streak with no dominant class.
    if let Some(class) = super::tool_error_class::dominant_class(classes)
        && let Some(guidance) = class.guidance()
    {
        s.push_str(&format!(
            "Most of these are {}. {}\n",
            class.label(),
            guidance
        ));
    }
    s.push_str(
        "Stop and diagnose before retrying — this is a system checkpoint, not a new task:\n\
         1. What root cause do these share — wrong arguments, wrong tool, or wrong approach?\n\
         2. If you've already tried a fix twice, it isn't working. Change the approach; don't tweak it.\n\
         3. If you're missing information, gather it first (read the file, list the directory,\n\
            re-read the exact error) before acting again.\n\
         Name the root cause in one sentence, then take a DIFFERENT next step.",
    );
    // When one tool dominates the streak, point the model straight at its
    // contract. The tool's full description + parameter schema are already
    // in context (the tool definitions), so re-reading them is cheaper and
    // more reliable than the model guessing again (cf. arXiv:2510.17874,
    // tool-doc re-grounding on repeated failure).
    if let Some(tool) = dominant_tool(recent) {
        s.push_str(&format!(
            "\nEvery one of these was `{tool}`. Re-read its description and parameter \
             schema in your tool definitions before calling it again — or use a different \
             tool to make progress.",
        ));
    }
    s
}

/// Build the permission-checkpoint body (dirge-iwwq). Deliberately
/// shares NO wording with [`format_checkpoint`]: a denial is not a
/// mechanical failure to diagnose and retry differently, it is a policy
/// wall only the user can lift. The message says so and forbids the
/// workaround a "try a different approach" nudge otherwise invites
/// (writing a script to do the blocked action, moving the work elsewhere).
fn format_permission_checkpoint(denials: usize, recent: &[(String, String)]) -> String {
    let mut s = format!(
        "[Permission checkpoint] {denials} tool calls in a row were blocked by the \
         permission system:\n"
    );
    for (tool, excerpt) in recent {
        s.push_str(&format!("  - {tool}: {excerpt}\n"));
    }
    s.push_str(
        "This is a policy block, not a tool error. Retrying, rephrasing, or switching to \
         another tool will not clear it, and you must NOT try to work around it — do not write \
         a script to perform the blocked action, move the work to an allowed path, or otherwise \
         route around the guardrail. Only the user can permit this. Either ask the user to \
         approve it (they can run `/allow add <tool> <pattern>`, e.g. `/allow add write \
         ~/dir/**`), or stop and report plainly what is blocked and what you would do once it \
         is allowed.",
    );
    s
}

/// The single tool name shared by every recent failure, or `None` if
/// the streak spans more than one tool.
fn dominant_tool(recent: &[(String, String)]) -> Option<String> {
    let first = recent.first()?.0.as_str();
    if recent.iter().all(|(t, _)| t == first) {
        Some(first.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Poll at the neutral tier — what every test predating dirge-z85a
    /// asserts, and the property `Nominal` must keep: bit-identical to the
    /// base threshold.
    fn poll(t: &FailureTracker) -> Vec<(LoopMessage, super::super::gate_tally::BoundaryNudge)> {
        t.poll_reflection(CapabilityTier::Nominal)
    }

    fn content_of(msgs: &[(LoopMessage, super::super::gate_tally::BoundaryNudge)]) -> String {
        match msgs.first() {
            Some((LoopMessage::User(u), _)) => u.text_joined(),
            _ => panic!("expected one User message"),
        }
    }

    /// dirge-61sv: a streak with one character must SAY so, and say the thing
    /// that fits it. The generic checkpoint asks "wrong arguments, wrong tool,
    /// or wrong approach?" — for a run whose calls all name files that are not
    /// there, none of those three is the answer, and the useful instruction is
    /// to stop calling and go look.
    ///
    /// This is the case measured in dirge-e31n: control runs burned 17 and 26
    /// varied, well-formed tool calls that storm, scavenge and repair all
    /// correctly ignored, and the only nudge they got was the generic one.
    #[test]
    fn checkpoint_names_a_dominant_missing_info_streak() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for path in ["/a/one.rs", "/a/two.rs", "/a/three.rs"] {
            t.record(
                Outcome::Error,
                "read",
                &format!("No such file or directory: {path}"),
            );
        }
        let msgs = t.poll_reflection(CapabilityTier::Nominal);
        let body = content_of(&msgs);
        assert!(
            body.contains("things that aren't there"),
            "checkpoint did not name the dominant class:\n{body}"
        );
        assert!(
            body.contains("stop calling and go look"),
            "checkpoint named the class but gave no class-specific direction:\n{body}"
        );
        // The generic advice still ships — the class line supplements it.
        assert!(body.contains("Stop and diagnose before retrying"));
    }

    /// The other side, and the one that makes the test above evidence: a
    /// DIFFERENT dominant class must produce different direction. A checkpoint
    /// that printed the missing-info line for every streak would pass the test
    /// above and be useless.
    #[test]
    fn a_different_dominant_class_gives_different_direction() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record(
                Outcome::Error,
                "edit",
                "invalid arguments: missing required field `old_text`",
            );
        }
        let body = content_of(&t.poll_reflection(CapabilityTier::Nominal));
        assert!(
            body.contains("malformed calls"),
            "misuse streak was not named:\n{body}"
        );
        assert!(
            !body.contains("things that aren't there"),
            "a misuse streak got missing-info direction:\n{body}"
        );
    }

    /// A mixed streak has no single character, so the checkpoint must NOT
    /// pick one. Confident direction about a minority of the failures is
    /// worse than the honest generic questions.
    #[test]
    fn a_mixed_streak_names_no_class() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Error, "read", "No such file or directory: /a");
        t.record(Outcome::Error, "edit", "invalid arguments: bad schema");
        t.record(Outcome::Error, "bash", "make: *** Error 1");
        let body = content_of(&t.poll_reflection(CapabilityTier::Nominal));
        assert!(body.contains("Stop and diagnose before retrying"));
        for label in [
            "things that aren't there",
            "malformed calls",
            "transient failures",
        ] {
            assert!(
                !body.contains(label),
                "a mixed streak claimed a dominant class ({label}):\n{body}"
            );
        }
    }

    /// Unrecognised errors must behave exactly as they did before this
    /// existed — the acceptance criterion for not regressing today's runs.
    #[test]
    fn unclassified_streak_is_unchanged_by_classification() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record(Outcome::Error, "bash", "make: *** [target] Error 1");
        }
        let body = content_of(&t.poll_reflection(CapabilityTier::Nominal));
        assert!(body.contains("Stop and diagnose before retrying"));
        assert!(
            !body.contains("Most of these are"),
            "an unclassified streak claimed a class:\n{body}"
        );
    }

    /// A success clears the class history with the rest of the streak state,
    /// so a later unrelated streak cannot inherit the earlier one's character.
    #[test]
    fn success_clears_the_class_history() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record(Outcome::Error, "read", "No such file or directory: /a");
        }
        t.record(Outcome::Ok, "read", "");
        for _ in 0..3 {
            t.record(Outcome::Error, "edit", "invalid arguments: bad schema");
        }
        let body = content_of(&t.poll_reflection(CapabilityTier::Nominal));
        assert!(
            !body.contains("things that aren't there"),
            "the cleared streak's class leaked into a new one:\n{body}"
        );
        assert!(body.contains("malformed calls"));
    }

    #[test]
    fn below_threshold_is_silent() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "no match");
        t.record_result(true, "edit", "no match either");
        assert!(poll(&t).is_empty(), "2 < threshold 3");
    }

    #[test]
    fn distinct_failures_trip_at_threshold() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "old_string not found");
        t.record_result(true, "read", "file not found");
        t.record_result(true, "bash", "command failed");
        let msgs = poll(&t);
        assert_eq!(msgs.len(), 1, "streak of 3 distinct errors nudges");
        let body = content_of(&msgs);
        assert!(body.contains("Recovery checkpoint"));
        assert!(body.contains("3 tool calls in a row have failed"));
        // Quotes the failing tools + excerpts.
        assert!(body.contains("edit: old_string not found"));
        assert!(body.contains("read: file not found"));
        // Asks for a different approach, not a retry.
        assert!(body.contains("DIFFERENT next step"));
        // Mixed tools → no single-tool re-grounding line.
        assert!(!body.contains("Re-read its description"));
    }

    #[test]
    fn one_tool_dominating_points_at_its_contract() {
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record_result(true, "edit", "old_string not found");
        }
        let body = content_of(&poll(&t));
        assert!(
            body.contains("Every one of these was `edit`"),
            "single-tool streak should name the tool: {body}"
        );
        assert!(body.contains("Re-read its description"));
    }

    #[test]
    fn two_timeouts_trip_a_threshold_three_tracker() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Timeout, "bash", "Command timed out after 120s");
        // One timeout (escalation 2) is below threshold 3 — still silent.
        assert!(
            poll(&t).is_empty(),
            "single timeout (weight 2) < threshold 3"
        );
        t.record(Outcome::Timeout, "bash", "Command timed out after 120s");
        // Two timeouts (escalation 4) cross threshold 3 after only 2 calls,
        // where two plain errors (weight 2) would not have.
        let msgs = poll(&t);
        assert_eq!(msgs.len(), 1, "two timeouts escalate past threshold");
        let body = content_of(&msgs);
        // Truthful call count, not the weighted score.
        assert!(body.contains("2 tool calls in a row have failed"), "{body}");
    }

    #[test]
    fn timeout_checkpoint_calls_out_the_timeout() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(2);
        t.record(Outcome::Timeout, "bash", "Command timed out after 120s");
        let body = content_of(&poll(&t));
        assert!(
            body.contains("timed out") && body.contains("time budget"),
            "checkpoint should name the timeout failure mode: {body}"
        );
    }

    #[test]
    fn error_then_timeout_reaches_threshold_three() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Error, "edit", "no match");
        assert!(poll(&t).is_empty(), "escalation 1 < 3");
        t.record(Outcome::Timeout, "bash", "Command timed out after 5s");
        // 1 (error) + 2 (timeout) = 3 → trips.
        assert_eq!(poll(&t).len(), 1, "error+timeout escalate to 3");
    }

    #[test]
    fn success_clears_the_streak() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "miss");
        t.record_result(true, "edit", "miss");
        t.record_result(false, "read", "ok"); // success resets
        t.record_result(true, "edit", "miss");
        assert!(
            poll(&t).is_empty(),
            "one success reset the counter; only 1 error since"
        );
    }

    #[test]
    fn nudges_once_per_streak_not_per_call() {
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record_result(true, "edit", "miss");
        }
        assert_eq!(poll(&t).len(), 1, "first crossing nudges");
        // A 4th failure shouldn't re-nudge — not yet another full threshold.
        t.record_result(true, "edit", "miss");
        assert!(
            poll(&t).is_empty(),
            "streak 4, last emitted at 3 — not due again"
        );
    }

    #[test]
    fn re_arms_after_another_threshold() {
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record_result(true, "edit", "miss");
        }
        assert_eq!(poll(&t).len(), 1);
        // Three more failures (streak now 6) re-arms the nudge.
        for _ in 0..3 {
            t.record_result(true, "edit", "miss");
        }
        let msgs = poll(&t);
        assert_eq!(msgs.len(), 1, "streak of 6 re-arms");
        assert!(content_of(&msgs).contains("6 tool calls in a row"));
    }

    #[test]
    fn poll_is_idempotent_within_a_streak() {
        let t = FailureTracker::new(2);
        t.record_result(true, "edit", "miss");
        t.record_result(true, "edit", "miss");
        assert_eq!(poll(&t).len(), 1);
        assert!(
            poll(&t).is_empty(),
            "second poll with no new failures stays silent"
        );
    }

    #[test]
    fn excerpt_is_condensed_to_one_bounded_line() {
        let t = FailureTracker::new(2);
        let noisy = format!("line one\n  line two\t{}", "x".repeat(400));
        t.record_result(true, "bash", &noisy);
        t.record_result(true, "bash", "second");
        let body = content_of(&poll(&t));
        assert!(!body.contains('\t'), "tabs collapsed");
        // The 400-x run must be truncated with an ellipsis.
        assert!(body.contains('…'));
        assert!(
            !body.contains(&"x".repeat(200)),
            "excerpt capped well under the raw length"
        );
    }

    #[test]
    fn only_last_five_failures_quoted() {
        let t = FailureTracker::new(3);
        for i in 0..7 {
            t.record_result(true, "edit", &format!("err{i}"));
        }
        let body = content_of(&poll(&t));
        assert!(!body.contains("err0"), "oldest dropped beyond MAX_QUOTED");
        assert!(!body.contains("err1"));
        assert!(body.contains("err2"));
        assert!(body.contains("err6"));
    }

    // dirge-iwwq: permission denials are a policy wall, not a mechanical
    // failure. They get their own checkpoint and must never feed the
    // "try a DIFFERENT approach" mechanical nudge — that nudge is exactly
    // what drives a model to route around the guardrail.

    #[test]
    fn denial_streak_emits_permission_checkpoint_not_mechanical() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record(
                Outcome::Denied,
                "edit",
                "Permission denied: writes outside project",
            );
        }
        let msgs = poll(&t);
        assert_eq!(msgs.len(), 1, "denial streak nudges once");
        let body = content_of(&msgs);
        // NOT the mechanical checkpoint — none of its tells.
        assert!(!body.contains("DIFFERENT next step"), "{body}");
        assert!(!body.contains("Re-read its description"), "{body}");
        assert!(!body.contains("wrong arguments, wrong tool"), "{body}");
        assert!(!body.contains("tool calls in a row have failed"), "{body}");
        // Permission-specific: names the block, points at /allow, and
        // forbids routing around it.
        assert!(body.contains("Permission checkpoint"), "{body}");
        assert!(body.contains("/allow"), "{body}");
        let lc = body.to_lowercase();
        assert!(
            lc.contains("work around") || lc.contains("route around"),
            "must forbid the workaround: {body}"
        );
        // Quotes the blocked tool + reason.
        assert!(body.contains("edit: Permission denied"), "{body}");
    }

    #[test]
    fn denials_do_not_inflate_the_mechanical_streak() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Error, "edit", "old_string not found");
        t.record(Outcome::Error, "read", "file not found");
        // If this denial wrongly fed the mechanical streak, escalation
        // would reach 3 and the mechanical checkpoint would fire.
        t.record(
            Outcome::Denied,
            "write",
            "Permission denied: outside project",
        );
        assert!(
            poll(&t).is_empty(),
            "2 mechanical errors + 1 denial: neither streak at threshold"
        );
    }

    #[test]
    fn denial_does_not_reset_the_mechanical_streak() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Error, "edit", "no match");
        t.record(Outcome::Error, "edit", "no match");
        // A denial between errors is neither a success (no reset) nor a
        // mechanical failure (no increment) — the error streak survives it.
        t.record(Outcome::Denied, "write", "Permission denied: x");
        t.record(Outcome::Error, "edit", "no match");
        let msgs = poll(&t);
        assert_eq!(msgs.len(), 1, "3 mechanical errors across a denial trip");
        assert!(content_of(&msgs).contains("3 tool calls in a row have failed"));
    }

    #[test]
    fn success_clears_the_denial_streak() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Denied, "write", "Permission denied: x");
        t.record(Outcome::Denied, "write", "Permission denied: x");
        t.record(Outcome::Ok, "read", "ok");
        t.record(Outcome::Denied, "write", "Permission denied: x");
        assert!(
            poll(&t).is_empty(),
            "success reset the denial streak; 1 denial < threshold"
        );
    }

    // dirge-uw2l.4: the safe-state abort rung fires off the failure tracker's
    // 2× threshold. Pure read-only signal — never mutates the tracker, never
    // spends a checkpoint.
    #[test]
    fn safe_state_due_false_below_double_threshold() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3); // 2× = 6
        // At the rung-2 checkpoint (escalation 3) the rung-3 signal is NOT due.
        for _ in 0..3 {
            t.record(Outcome::Error, "edit", "no match");
        }
        assert!(!t.safe_state_due(), "at threshold, not yet 2x");
        // Climb to one short of 2x.
        for _ in 0..2 {
            t.record(Outcome::Error, "edit", "no match");
        }
        assert!(!t.safe_state_due(), "5 < 2x threshold 6");
    }

    #[test]
    fn safe_state_due_true_at_double_threshold() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..6 {
            t.record(Outcome::Error, "edit", "no match");
        }
        assert!(t.safe_state_due(), "6 == 2x threshold");
        // A timeout-heavy streak reaches 2x sooner (timeouts count double).
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record(Outcome::Timeout, "bash", "Command timed out after 120s");
        }
        assert!(
            t.safe_state_due(),
            "3 timeouts == escalation 6 == 2x threshold"
        );
    }

    #[test]
    fn safe_state_due_resets_on_success() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..6 {
            t.record(Outcome::Error, "edit", "no match");
        }
        assert!(t.safe_state_due());
        t.record(Outcome::Ok, "read", "ok");
        assert!(!t.safe_state_due(), "a success clears the streak");
    }

    #[test]
    fn safe_state_due_re_arms_after_success() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..6 {
            t.record(Outcome::Error, "edit", "no match");
        }
        t.record(Outcome::Ok, "read", "ok");
        // A fresh streak must re-climb the full 2x — the signal does not carry
        // over from the cleared streak.
        for _ in 0..5 {
            t.record(Outcome::Error, "edit", "no match");
        }
        assert!(!t.safe_state_due(), "5 < 6 after re-arm");
        t.record(Outcome::Error, "edit", "no match");
        assert!(t.safe_state_due(), "6 re-trips after a fresh streak");
    }

    #[test]
    fn safe_state_due_ignores_denial_streak() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        // Permission denials are a policy wall, not a mechanical failure
        // (dirge-iwwq) — they must NOT trip the safe-state abort, which is
        // about a plan that's mechanically failing, not one the user blocked.
        for _ in 0..6 {
            t.record(Outcome::Denied, "write", "Permission denied: x");
        }
        assert!(!t.safe_state_due(), "denials never feed escalation");
    }

    // dirge-z85a: the recovery-checkpoint threshold is read at every poll, not
    // baked in at construction. The tracker is built at run start, when the
    // estimator is always `Nominal` by warm-up (MIN_CALLS_FOR_ESTIMATE), so a
    // threshold derived at the construction site would read the neutral tier
    // every time and be inert by construction.

    #[test]
    fn struggling_reflects_sooner_than_nominal() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "old_string not found");
        t.record_result(true, "read", "file not found");
        assert!(
            t.poll_reflection(CapabilityTier::Nominal).is_empty(),
            "2 < base threshold 3"
        );
        let msgs = t.poll_reflection(CapabilityTier::Struggling);
        assert_eq!(msgs.len(), 1, "struggling reflects at the scaled 2");
        assert!(content_of(&msgs).contains("2 tool calls in a row have failed"));
    }

    #[test]
    fn tier_flip_mid_streak_moves_the_guard_both_ways() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "a");
        t.record_result(true, "edit", "b");
        // Struggling: fires at 2.
        assert_eq!(t.poll_reflection(CapabilityTier::Struggling).len(), 1);
        // Flipped back to Nominal, the re-arm interval is the BASE threshold
        // again — escalation 4 is only 2 past the last emit, so it stays quiet
        // until 5.
        t.record_result(true, "edit", "c");
        t.record_result(true, "edit", "d");
        assert!(
            t.poll_reflection(CapabilityTier::Nominal).is_empty(),
            "4 - 2 = 2 < base 3"
        );
        t.record_result(true, "edit", "e");
        assert_eq!(
            t.poll_reflection(CapabilityTier::Nominal).len(),
            1,
            "5 - 2 = 3 re-arms at the base interval"
        );
    }

    #[test]
    fn nominal_and_strong_are_bit_identical() {
        for tier in [CapabilityTier::Nominal, CapabilityTier::Strong] {
            let t = FailureTracker::new(3);
            t.record_result(true, "edit", "a");
            t.record_result(true, "edit", "b");
            assert!(t.poll_reflection(tier).is_empty(), "{tier:?}: 2 < 3");
            t.record_result(true, "edit", "c");
            assert_eq!(t.poll_reflection(tier).len(), 1, "{tier:?}: 3 == 3");
        }
    }

    #[test]
    fn struggling_re_arms_at_the_scaled_interval() {
        let t = FailureTracker::new(3);
        for _ in 0..2 {
            t.record_result(true, "edit", "miss");
        }
        assert_eq!(
            t.poll_reflection(CapabilityTier::Struggling).len(),
            1,
            "fires at 2"
        );
        t.record_result(true, "edit", "miss");
        assert!(
            t.poll_reflection(CapabilityTier::Struggling).is_empty(),
            "3 - 2 = 1 < 2"
        );
        t.record_result(true, "edit", "miss");
        assert_eq!(
            t.poll_reflection(CapabilityTier::Struggling).len(),
            1,
            "4 - 2 = 2 re-arms"
        );
    }

    #[test]
    fn scaled_threshold_respects_the_two_failure_floor() {
        let t = FailureTracker::new(2);
        t.record_result(true, "edit", "one");
        assert!(
            t.poll_reflection(CapabilityTier::Struggling).is_empty(),
            "base 2 scales to 1, clamped back to 2 — never nudge on a single error"
        );
        t.record_result(true, "edit", "two");
        assert_eq!(t.poll_reflection(CapabilityTier::Struggling).len(), 1);
    }

    #[test]
    fn permission_checkpoint_stays_on_the_base_threshold() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Denied, "write", "Permission denied: x");
        t.record(Outcome::Denied, "write", "Permission denied: x");
        // A denial streak is a policy wall, and nothing the estimator counts
        // measures it — the tier has no standing to pull this one forward.
        assert!(
            t.poll_reflection(CapabilityTier::Struggling).is_empty(),
            "2 denials < base threshold 3, tier notwithstanding"
        );
        t.record(Outcome::Denied, "write", "Permission denied: x");
        assert_eq!(t.poll_reflection(CapabilityTier::Struggling).len(), 1);
    }

    #[test]
    fn safe_state_signal_stays_on_the_base_threshold() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        for _ in 0..4 {
            t.record(Outcome::Error, "edit", "no match");
        }
        // Struggling pulls the rung-2 checkpoint forward (2, not 3). It must
        // NOT drag the rung-3 abort with it: that rung spends a hard-capped
        // budget and, in auto mode, writes to the tree.
        assert!(!t.safe_state_due(), "2x stays 6 regardless of tier");
    }

    #[test]
    fn recent_excerpts_exposed_for_replan() {
        use super::super::activity::Outcome;
        let t = FailureTracker::new(3);
        t.record(Outcome::Error, "edit", "old_string not found");
        t.record(Outcome::Error, "bash", "command failed");
        let excerpts = t.recent_excerpts();
        assert_eq!(excerpts.len(), 2);
        assert_eq!(excerpts[0].0, "edit");
        assert!(excerpts[0].1.contains("old_string not found"));
        assert_eq!(excerpts[1].0, "bash");
        // A success clears the streak and its excerpts.
        t.record(Outcome::Ok, "read", "ok");
        assert!(t.recent_excerpts().is_empty(), "success clears recent");
    }
}

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
//! Self-contained — no rig/LLM state. Lives as a local in
//! [`super::run`]; when the loop never wires it, behaviour is
//! unchanged.

use std::sync::{Arc, Mutex};

use crate::sync_util::LockExt;

use super::message::{LoopMessage, UserMessage};

/// How many recent failures to quote back in the checkpoint body.
const MAX_QUOTED: usize = 5;
/// Per-error excerpt cap (single line) so the nudge stays compact.
const EXCERPT_CAP: usize = 160;

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
    /// Escalation score at the last emitted checkpoint; 0 = none emitted
    /// for this streak. Re-arm only after another `threshold` of
    /// escalation so a stubborn streak gets periodic — not per-call —
    /// nudges.
    last_emitted_at: usize,
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
                last_emitted_at: 0,
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
                inner.last_emitted_at = 0;
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

    /// Poll hook: returns one recovery-checkpoint message when the
    /// streak has reached `threshold` and we haven't nudged since the
    /// last `threshold`-failure interval; otherwise empty.
    pub fn poll_reflection(&self) -> Vec<LoopMessage> {
        let mut inner = self.inner.lock_ignore_poison();
        if inner.escalation < self.threshold {
            return Vec::new();
        }
        // First crossing, or another full `threshold` of escalation since
        // the last nudge. Keyed on the weighted score so timeouts pull
        // the nudge forward.
        let due = inner.last_emitted_at == 0
            || inner.escalation.saturating_sub(inner.last_emitted_at) >= self.threshold;
        if !due {
            return Vec::new();
        }
        inner.last_emitted_at = inner.escalation;
        let body = format_checkpoint(inner.consecutive, inner.timeouts, &inner.recent);
        vec![LoopMessage::User(UserMessage { content: body })]
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
fn format_checkpoint(consecutive: usize, timeouts: usize, recent: &[(String, String)]) -> String {
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

    fn content_of(msgs: &[LoopMessage]) -> String {
        match msgs.first() {
            Some(LoopMessage::User(u)) => u.content.clone(),
            _ => panic!("expected one User message"),
        }
    }

    #[test]
    fn below_threshold_is_silent() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "no match");
        t.record_result(true, "edit", "no match either");
        assert!(t.poll_reflection().is_empty(), "2 < threshold 3");
    }

    #[test]
    fn distinct_failures_trip_at_threshold() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "old_string not found");
        t.record_result(true, "read", "file not found");
        t.record_result(true, "bash", "command failed");
        let msgs = t.poll_reflection();
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
        let body = content_of(&t.poll_reflection());
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
            t.poll_reflection().is_empty(),
            "single timeout (weight 2) < threshold 3"
        );
        t.record(Outcome::Timeout, "bash", "Command timed out after 120s");
        // Two timeouts (escalation 4) cross threshold 3 after only 2 calls,
        // where two plain errors (weight 2) would not have.
        let msgs = t.poll_reflection();
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
        let body = content_of(&t.poll_reflection());
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
        assert!(t.poll_reflection().is_empty(), "escalation 1 < 3");
        t.record(Outcome::Timeout, "bash", "Command timed out after 5s");
        // 1 (error) + 2 (timeout) = 3 → trips.
        assert_eq!(t.poll_reflection().len(), 1, "error+timeout escalate to 3");
    }

    #[test]
    fn success_clears_the_streak() {
        let t = FailureTracker::new(3);
        t.record_result(true, "edit", "miss");
        t.record_result(true, "edit", "miss");
        t.record_result(false, "read", "ok"); // success resets
        t.record_result(true, "edit", "miss");
        assert!(
            t.poll_reflection().is_empty(),
            "one success reset the counter; only 1 error since"
        );
    }

    #[test]
    fn nudges_once_per_streak_not_per_call() {
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record_result(true, "edit", "miss");
        }
        assert_eq!(t.poll_reflection().len(), 1, "first crossing nudges");
        // A 4th failure shouldn't re-nudge — not yet another full threshold.
        t.record_result(true, "edit", "miss");
        assert!(
            t.poll_reflection().is_empty(),
            "streak 4, last emitted at 3 — not due again"
        );
    }

    #[test]
    fn re_arms_after_another_threshold() {
        let t = FailureTracker::new(3);
        for _ in 0..3 {
            t.record_result(true, "edit", "miss");
        }
        assert_eq!(t.poll_reflection().len(), 1);
        // Three more failures (streak now 6) re-arms the nudge.
        for _ in 0..3 {
            t.record_result(true, "edit", "miss");
        }
        let msgs = t.poll_reflection();
        assert_eq!(msgs.len(), 1, "streak of 6 re-arms");
        assert!(content_of(&msgs).contains("6 tool calls in a row"));
    }

    #[test]
    fn poll_is_idempotent_within_a_streak() {
        let t = FailureTracker::new(2);
        t.record_result(true, "edit", "miss");
        t.record_result(true, "edit", "miss");
        assert_eq!(t.poll_reflection().len(), 1);
        assert!(
            t.poll_reflection().is_empty(),
            "second poll with no new failures stays silent"
        );
    }

    #[test]
    fn excerpt_is_condensed_to_one_bounded_line() {
        let t = FailureTracker::new(2);
        let noisy = format!("line one\n  line two\t{}", "x".repeat(400));
        t.record_result(true, "bash", &noisy);
        t.record_result(true, "bash", "second");
        let body = content_of(&t.poll_reflection());
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
        let body = content_of(&t.poll_reflection());
        assert!(!body.contains("err0"), "oldest dropped beyond MAX_QUOTED");
        assert!(!body.contains("err1"));
        assert!(body.contains("err2"));
        assert!(body.contains("err6"));
    }
}

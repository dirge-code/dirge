//! Goal gate: an opt-in, user-defined natural-language stop condition for
//! autonomous runs (`--loop`, the MCP delegate). At the finalization
//! boundary an independent judge decides whether the stated goal is
//! actually met; if not, its reason re-enters the loop and the run
//! continues, bounded by [`MAX_GOAL_REACT`] so a mis-stated or
//! unsatisfiable goal can't loop forever. OFF unless a goal is set AND a
//! judge (the critic provider) is configured — no cost on a default
//! session.
//!
//! Mirrors [`super::critic`]: it reuses the [`CriticFn`] judge callback
//! built in the provider layer, and owns the prompt, verdict parsing, and
//! loop-message wiring here so they're unit-testable without a model. The
//! difference from the critic is intent and cardinality — the critic is a
//! one-shot "is this correct/complete" review; the goal gate persists
//! across finalizations until the user's explicit stop condition holds.

use super::critic::CriticFn;
use super::message::{LoopMessage, UserMessage};

/// Max times the goal gate re-enters the loop before giving up and letting
/// the run finalize anyway. A natural-language goal can be mis-stated or
/// genuinely unsatisfiable; this bound (MiMo's `MAX_GOAL_REACT`) stops the
/// loop from spinning on it indefinitely.
pub const MAX_GOAL_REACT: u8 = 12;

/// Tag prefixed onto the goal gate's re-entry message. The loop re-enters
/// it as a user-role message (so the model acts on it); the UI keys on the
/// tag to render it under a distinct handle rather than as user input —
/// same scheme as [`super::critic::CRITIC_TAG`].
pub const GOAL_TAG: &str = "[goal]";

/// System preamble establishing the judge's role and a calibrated stance.
/// Like the critic, it must respect the agent's own constraints so it never
/// demands a forbidden action, and it must judge ONLY the stated stop
/// condition — not invent extra requirements.
const GOAL_PREAMBLE: &str = "\
You are a completion judge for an autonomous coding agent. You are given the agent's own \
instructions and constraints, a single natural-language STOP CONDITION the user set for this run, \
and a transcript of what the agent has done so far. Decide ONLY whether the stop condition is now \
satisfied.\n\
\n\
Hard rules:\n\
- Judge against the STOP CONDITION as written — nothing more, nothing less. Do not add scope or \
\"nice to haves\".\n\
- RESPECT the agent's instructions. Never require an action the instructions forbid or defer (e.g. \
if told not to push, a missing push does NOT make the goal unmet).\n\
- Treat the condition as MET when the transcript shows it plainly satisfied. When genuinely \
unsure, answer MET — the run is already bounded, and a false UNMET wastes a whole turn.";

/// Response-format instruction, kept beside the transcript in the user
/// prompt so the verdict shape sits next to the material being judged.
const GOAL_FORMAT: &str = "\
Respond in EXACTLY this format and nothing else:\n\
On the first line, either `GOAL: MET` or `GOAL: UNMET`.\n\
If UNMET, follow with a short bullet list of exactly what remains for the stop condition to hold.";

/// Cap on the constraints block fed to the judge so a large system prompt
/// doesn't balloon the call. Mirrors the critic's bound.
const MAX_RULES_CHARS: usize = 16_000;

/// Build the judge prompt: the agent's constraints, the stop condition, the
/// transcript, and the response format. The compaction summary is stripped
/// from `rules` first (same stale-state guard as the critic — a resumed
/// session's `## Active Task` describes already-done work, not the goal),
/// then it is truncated to [`MAX_RULES_CHARS`] with a note when elided.
pub fn build_goal_prompt(goal: &str, rules: &str, transcript: &str) -> String {
    let rules = super::critic::strip_compaction_summary(rules);
    let (rules, elided) = if rules.chars().count() > MAX_RULES_CHARS {
        let head: String = rules.chars().take(MAX_RULES_CHARS).collect();
        (head, "\n[…constraints truncated…]")
    } else {
        (rules.to_string(), "")
    };
    format!(
        "{GOAL_PREAMBLE}\n\n\
         === AGENT INSTRUCTIONS / CONSTRAINTS ===\n{rules}{elided}\n\n\
         === STOP CONDITION ===\n{goal}\n\n\
         === TRANSCRIPT ===\n{transcript}\n\n\
         {GOAL_FORMAT}"
    )
}

/// Parse the judge's verdict. `Some(remaining)` means the goal is NOT yet
/// met (with the outstanding work); `None` means met. An empty or
/// unparseable verdict resolves to `None` (met) — failing toward
/// finalization so a flaky judge can't trap the loop; the re-entry bound is
/// the backstop for the opposite mistake.
pub fn parse_goal_verdict(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first_line = trimmed.lines().next().unwrap_or("").to_ascii_uppercase();
    if first_line.contains("GOAL: UNMET") || first_line.contains("GOAL:UNMET") {
        // Everything after the first line is the remaining-work detail.
        let detail = trimmed
            .split_once('\n')
            .map(|(_, rest)| rest.trim())
            .unwrap_or("");
        let detail = if detail.is_empty() {
            "(no detail given)".to_string()
        } else {
            detail.to_string()
        };
        Some(detail)
    } else {
        // MET, or ambiguous → fail toward done.
        None
    }
}

/// Run the goal gate over a run transcript. Returns a one-element vec with a
/// [`GOAL_TAG`]-prefixed re-entry message when the stop condition is not yet
/// met; empty otherwise (met, or the judge call errored — fail open). Never
/// panics on a judge error. The caller enforces [`MAX_GOAL_REACT`].
pub async fn run_goal_gate(
    judge: &CriticFn,
    goal: &str,
    rules: &str,
    transcript: &str,
) -> Vec<LoopMessage> {
    let prompt = build_goal_prompt(goal, rules, transcript);
    let response = match judge(prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "dirge::goal", error = %e, "goal-gate judge call failed; finalizing without it");
            return Vec::new();
        }
    };
    match parse_goal_verdict(&response) {
        Some(remaining) => vec![LoopMessage::User(UserMessage {
            content: format!(
                "{GOAL_TAG} The stop condition for this run is not satisfied yet: \"{goal}\". \
                 Outstanding:\n{remaining}\n\
                 Keep working until it holds, or — if it can't be met (out of scope, blocked, or \
                 something you were told not to do) — say so explicitly and stop."
            ),
        })],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn parse_met_returns_none() {
        assert!(parse_goal_verdict("GOAL: MET").is_none());
        assert!(parse_goal_verdict("goal: met\nlooks good").is_none());
    }

    #[test]
    fn parse_unmet_returns_remaining() {
        let r = parse_goal_verdict("GOAL: UNMET\n- tests still failing\n- not committed");
        let detail = r.expect("unmet → Some");
        assert!(detail.contains("tests still failing"));
        assert!(detail.contains("not committed"));
    }

    #[test]
    fn parse_unmet_without_detail_is_still_unmet() {
        let r = parse_goal_verdict("GOAL: UNMET");
        assert_eq!(r.as_deref(), Some("(no detail given)"));
    }

    #[test]
    fn parse_empty_or_ambiguous_fails_toward_met() {
        assert!(parse_goal_verdict("").is_none());
        assert!(parse_goal_verdict("   \n ").is_none());
        assert!(parse_goal_verdict("probably done?").is_none());
    }

    #[test]
    fn prompt_embeds_goal_rules_transcript_and_format() {
        let p = build_goal_prompt(
            "all tests pass and changes committed",
            "RULE: never push to remote.",
            "user asked X; assistant ran the tests",
        );
        assert!(p.contains("all tests pass and changes committed"));
        assert!(p.contains("never push to remote"));
        assert!(p.contains("assistant ran the tests"));
        assert!(p.contains("GOAL: MET"));
    }

    /// Same stale-state guard as the critic (dirge-wp0e): after a resume the
    /// merged system prompt carries the `[CONTEXT COMPACTION — REFERENCE
    /// ONLY]` summary, whose `## Active Task` describes already-completed
    /// work. It must not reach the goal judge, or the gate would re-demand
    /// superseded work as if it were the stop condition.
    #[test]
    fn build_goal_prompt_drops_the_compaction_summary_from_rules() {
        let rules = format!(
            "RULE: never push to remote.\n\n{} \
             ## Active Task\nFinish Phase 3: wire the Janet loader and add tests.",
            crate::agent::compression::COMPACTION_MARKER,
        );
        let p = build_goal_prompt("all tests pass", &rules, "assistant ran the tests");
        assert!(
            p.contains("never push to remote"),
            "real rules must survive"
        );
        assert!(
            !p.contains("Active Task") && !p.contains("Phase 3") && !p.contains("Janet"),
            "the compaction summary must be stripped from the judge's rules",
        );
        assert!(
            !p.contains(crate::agent::compression::COMPACTION_MARKER),
            "the compaction marker itself must be stripped",
        );
    }

    #[tokio::test]
    async fn unmet_judge_yields_a_tagged_reentry() {
        let judge: CriticFn = Arc::new(|_p| {
            Box::pin(async { Ok("GOAL: UNMET\n- still need to commit".to_string()) })
        });
        let msgs = run_goal_gate(&judge, "commit the work", "", "edited foo.rs").await;
        assert_eq!(msgs.len(), 1);
        let LoopMessage::User(UserMessage { content }) = &msgs[0] else {
            panic!("goal gate must re-enter as a user-role message");
        };
        assert!(content.starts_with(GOAL_TAG));
        assert!(content.contains("commit the work"));
        assert!(content.contains("still need to commit"));
    }

    #[tokio::test]
    async fn met_judge_yields_no_reentry() {
        let judge: CriticFn = Arc::new(|_p| Box::pin(async { Ok("GOAL: MET".to_string()) }));
        let msgs = run_goal_gate(&judge, "commit the work", "", "committed").await;
        assert!(msgs.is_empty(), "a met goal must let the run finalize");
    }

    #[tokio::test]
    async fn judge_error_fails_open() {
        let judge: CriticFn = Arc::new(|_p| Box::pin(async { anyhow::bail!("provider down") }));
        let msgs = run_goal_gate(&judge, "commit the work", "", "x").await;
        assert!(msgs.is_empty(), "a judge error must not trap the loop");
    }
}

//! Bounded in-loop LLM critic (F6 tier 3).
//!
//! When a `critic_provider` is configured, the verifier gate can escalate
//! from cheap signals to a single LLM judgement at the finalization
//! boundary: given the user's request and the work done this run, is the
//! task actually complete and correct? If the critic says no, its
//! concrete issues are injected as a follow-up and the loop continues;
//! otherwise the run finalizes. Bounded to one call per run (the caller
//! enforces this) and OFF unless a critic provider is configured — so it
//! never adds latency or cost to a default session.
//!
//! The actual LLM call is a [`CriticFn`] callback (mirrors
//! `compression::SummarizeFn`) built in the provider layer; this module
//! owns the prompt, the verdict parsing, and the loop-message wiring so
//! they're unit-testable without a model.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::message::{LoopMessage, UserMessage};
use super::verifier::VerificationStatus;

/// Parsed critic verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Work is done, or fail-open (empty/ambiguous response).
    Complete,
    /// Concrete issues that must be addressed.
    Incomplete(String),
    /// Cannot verify from spec/evidence available — missing info or test.
    Abstain(String),
}

/// Truncate `rules` to at most `max` CHARS (not bytes), appending `note`
/// when truncation happens. Counting by chars stops a multibyte system prompt
/// from tripping a byte-based cap and being needlessly shortened — the old
/// per-site `.len() > MAX` gates truncated strings whose char count was
/// already under the cap. Returns the input borrowed when within the cap.
pub(crate) fn truncate_rules<'a>(rules: &'a str, max: usize, note: &str) -> Cow<'a, str> {
    if rules.chars().count() > max {
        let head: String = rules.chars().take(max).collect();
        Cow::Owned(format!("{head}{note}"))
    } else {
        Cow::Borrowed(rules)
    }
}

/// Wall-clock bound on a single judge LLM call (critic / goal-gate /
/// code-review). A provider that opens a stream then stalls without
/// erroring would otherwise freeze finalization forever; on expiry the
/// judge fails OPEN (same as an error), mirroring COMPACTION_SUMMARY_TIMEOUT
/// (dirge-ax46).
pub(crate) const JUDGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Run a judge-style completion, failing open on error. Centralizes the
/// `Ok(r) => r, Err(e) => { warn + return default }` shape shared by the
/// critic, goal-gate, and code-review passes. `target` must be a string
/// literal (tracing callsite metadata is static); the expansion `return`s
/// `default` from the enclosing function on error.
macro_rules! run_judge {
    ($judge:expr, $prompt:expr, $target:literal, $msg:literal, $default:expr) => {
        match ::tokio::time::timeout(
            $crate::agent::agent_loop::critic::JUDGE_TIMEOUT,
            $judge($prompt),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(target: $target, error = %e, $msg);
                return $default;
            }
            Err(_) => {
                tracing::warn!(
                    target: $target,
                    timeout_secs = $crate::agent::agent_loop::critic::JUDGE_TIMEOUT.as_secs(),
                    "judge call timed out; failing open"
                );
                return $default;
            }
        }
    };
}
pub(crate) use run_judge;

/// One-shot critic call: takes a fully-built prompt, returns the model's
/// raw verdict text. Mirrors `compression::SummarizeFn` so the provider
/// layer can build it from any configured model.
pub type CriticFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> + Send + Sync,
>;

/// Tag prefixed onto the critic's injected follow-up message. The agent
/// loop re-enters it as a user-role message (so the model acts on it); the
/// UI keys on this tag to render it under a distinct `<critic>` handle and
/// color instead of the user's. Shared so producer and renderer agree.
pub const CRITIC_TAG: &str = "[critic]";

/// System preamble for the critic: establishes its role and a calibrated —
/// not trigger-happy — stance. Passed as the LLM system prompt by
/// `build_judge_fn` so the model knows what it is BEFORE it sees the
/// transcript. The response FORMAT lives in [`build_prompt`] instead —
/// right next to the material being judged.
///
/// dirge-bedj: the stance was over-aggressive ("be skeptical", everything
/// "NOT complete") and constraint-blind, so it demanded actions the agent
/// was explicitly told not to take (e.g. pushing). It now (a) respects the
/// agent's own instructions and (b) blocks only on concrete, in-scope gaps.
pub const CRITIC_PREAMBLE: &str = "\
You are a code-review critic for an autonomous coding agent. You are given the instructions and \
constraints the assistant operates under, plus a transcript of what it just did to satisfy the \
user's request. Judge ONLY whether the task is actually complete and correct within those \
constraints — not style.\n\
\n\
Hard rules:\n\
- RESPECT the assistant's instructions. NEVER flag the absence of an action the instructions \
forbid or defer (e.g. if it was told not to push/commit/deploy, do NOT ask it to). Treat anything \
the instructions place out of scope as correctly omitted.\n\
- Block only on CONCRETE, in-scope incompleteness with evidence (e.g. the user asked for X and X \
is missing; a change was made but never built/tested when verification was expected).\n\
- A tool result tagged `[DENIED]` (or whose text begins `Permission denied` / `Auto-approval \
denied`) is a PERMISSION block, not a failure to fix. Treat that capability as out of scope: \
never demand the assistant retry it, route around it, or accomplish the blocked action some \
other way. Judge the rest of the work as if that action were correctly deferred to the user.\n\
- A block marked `[CONTEXT COMPACTION — REFERENCE ONLY]` (or a `## Active Task` lifted from one) \
describes ALREADY-COMPLETED prior work — never treat it as an outstanding requirement. Judge only \
the latest request and the transcript.\n\
- Do NOT invent new requirements, scope, or \"nice to haves\". If you cannot determine correctness from \
the spec and evidence available, ABSTAIN — say what's missing (e.g. no test covering this change, \
unclear acceptance criteria). An abstention is safer than a false pass. If you are unsure whether \
there's a real gap, PASS — a false block wastes a whole turn.";

/// Response-format instruction. Kept in the user prompt (not the system
/// preamble) so the verdict shape sits directly beside the transcript.
const CRITIC_FORMAT: &str = "\
Respond in EXACTLY this format and nothing else:\n\
On the first line, one of: `VERDICT: COMPLETE`, `VERDICT: INCOMPLETE`, or `VERDICT: ABSTAIN`.\n\
- COMPLETE: the work is done and correct.\n\
- INCOMPLETE: concrete, in-scope gaps remain. Follow with a short bullet list.\n\
- ABSTAIN: the spec or evidence available is insufficient to judge correctness. \
Say what test or spec detail is missing (e.g. no test covering the change, \
acceptance criteria unclear). Do NOT pass: an ABSTAIN is still a block, \
but one the assistant resolves by adding evidence rather than fixing gaps.";

/// Cap on the instructions/constraints block fed to the critic, so a large
/// system prompt (tool docs + project context) doesn't balloon the critic
/// call. Generous — the constraints that matter (AGENTS.md, prompt-mode
/// rules) sit early; a truncation note tells the critic more was elided.
const MAX_RULES_CHARS: usize = 16_000;

/// Drop the context-compaction summary from the critic's `rules`. The rules
/// are the agent's merged system prompt, built as `preamble + "\n\n" + history`
/// (`provider::spawn`), so the summary — a `[CONTEXT COMPACTION — REFERENCE
/// ONLY]` System message — always lands AFTER the genuine constraints.
/// Truncating at the marker keeps the real rules (identity, tool docs,
/// AGENTS.md, prompt-mode scope) and discards the stale summary, whose
/// `## Active Task` describes already-completed work the critic would
/// otherwise demand again (the stale-state bug). Returns the input unchanged
/// when no summary is present.
///
/// Shared with the sibling goal gate ([`super::goal`]), which feeds the same
/// merged system prompt to the same judge and needs the same protection.
pub(crate) fn strip_compaction_summary(rules: &str) -> &str {
    match rules.find(crate::agent::compression::COMPACTION_MARKER) {
        Some(idx) => rules[..idx].trim_end(),
        None => rules,
    }
}

/// Render the verification-status block for the critic prompt (dirge-6q3w).
/// Empty unless code was edited this run — that's the precondition that
/// keeps the critic from nagging about tests on a no-code-change turn.
/// When code WAS edited, it gives the critic the concrete signal the
/// cheap verifier gate already computed, plus a calibrated instruction so
/// it treats an unverified/red change as a real, in-scope gap rather than
/// inventing busywork.
fn verification_block(verification: Option<VerificationStatus>) -> &'static str {
    match verification {
        Some(VerificationStatus::Unverified) => {
            "\n\n--- verification status ---\n\
             Code was edited this run but no build/test/lint was detected. If one is runnable \
             here and not forbidden, flag the unverified change as a concrete gap and name the \
             command to run. This is a NUDGE, not a hard rule: if there is nothing to run, the \
             change isn't testable (docs, config, scaffolding), or the assistant already verified \
             another way and said so, treat it as COMPLETE — never force a test that can't be \
             run.\n--- end verification status ---"
        }
        Some(VerificationStatus::VerifiedRed) => {
            "\n\n--- verification status ---\n\
             Code was edited and the most recent build/test FAILED. Don't pass a red build — this \
             is INCOMPLETE — UNLESS the assistant explicitly said the failure is pre-existing, \
             expected, or unrelated to the change.\n--- end verification status ---"
        }
        Some(VerificationStatus::VerifiedGreen) => {
            "\n\n--- verification status ---\n\
             Code was edited and a build/test passed. Sanity-check only that the verification was \
             RELEVANT to the change (e.g. tests covering the edited area, not just an unrelated \
             build); don't manufacture extra requirements.\n--- end verification status ---"
        }
        // No code edited (precondition not met) or no gate configured →
        // add nothing, so the critic behaves exactly as before.
        Some(VerificationStatus::NoCodeEdited) | None => "",
    }
}

/// Build the critic prompt. `rules` is the assistant's own system prompt /
/// instructions (so the critic judges against the SAME constraints the
/// agent had — dirge-bedj), minus any compaction summary (see
/// [`strip_compaction_summary`]); `transcript` is what the agent did;
/// `verification` is the run's compile/lint/test signal (dirge-6q3w),
/// `None` when no verifier gate is configured. The role lives in
/// [`CRITIC_PREAMBLE`]; this carries the format + bodies.
pub fn build_prompt(
    rules: &str,
    transcript: &str,
    verification: Option<VerificationStatus>,
) -> String {
    let rules = strip_compaction_summary(rules).trim();
    let rules_block = if rules.is_empty() {
        "(no special constraints provided)".to_string()
    } else {
        truncate_rules(rules, MAX_RULES_CHARS, "\n…(instructions truncated)").into_owned()
    };
    format!(
        "{CRITIC_FORMAT}\n\n\
         --- assistant instructions & constraints (judge within these; never demand a \
         forbidden/out-of-scope action) ---\n{rules_block}\n--- end instructions ---\n\n\
         --- transcript ---\n{transcript}\n--- end transcript ---{}",
        verification_block(verification)
    )
}

/// Parse the critic's raw response into a verdict. `Verdict::Complete` means
/// the work is done — or the response was empty/ambiguous, in which case we
/// fail OPEN (don't block finalization on a confused critic).
/// `Verdict::Incomplete(issues)` means concrete gaps to fix.
/// `Verdict::Abstain(missing)` means the critic cannot verify from available
/// spec/evidence — the model should write a held-out test or clarify the spec.
///
/// ORDER of precedence on the first non-empty line: INCOMPLETE > ABSTAIN >
/// Complete (fail-open). A line containing both INCOMPLETE and ABSTAIN
/// resolves to Incomplete (the safer, action-forcing state).
pub fn parse_verdict(response: &str) -> Verdict {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Verdict::Complete;
    }
    // Look at the first non-empty line for the verdict token.
    let first = trimmed.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let upper = first.to_ascii_uppercase();
    if upper.contains("INCOMPLETE") {
        let rest = trimmed
            .split_once('\n')
            .map(|(_, x)| x)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed);
        Verdict::Incomplete(rest.to_string())
    } else if upper.contains("ABSTAIN") || upper.contains("INSUFFICIENT") {
        let rest = trimmed
            .split_once('\n')
            .map(|(_, x)| x)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed);
        Verdict::Abstain(rest.to_string())
    } else {
        // "COMPLETE", or anything that isn't a clear INCOMPLETE/ABSTAIN → pass.
        Verdict::Complete
    }
}

/// Run the critic over a run transcript. `rules` is the assistant's own
/// system prompt / instructions, passed so the critic judges within the
/// SAME constraints the agent had (dirge-bedj); `verification` is the
/// run's compile/lint/test signal so the critic can be pickier about
/// unverified changes (dirge-6q3w). Returns a one-element vec with a
/// [`CRITIC_TAG`]-prefixed follow-up message when the critic judged the
/// work incomplete; empty otherwise (complete, or the call errored — fail
/// open). Never panics on a critic error.
pub async fn run_critic(
    critic: &CriticFn,
    rules: &str,
    transcript: &str,
    verification: Option<VerificationStatus>,
) -> Vec<LoopMessage> {
    let prompt = build_prompt(rules, transcript, verification);
    let response = run_judge!(
        critic,
        prompt,
        "dirge::critic",
        "critic call failed; finalizing without it",
        Vec::new()
    );
    match parse_verdict(&response) {
        Verdict::Complete => Vec::new(),
        Verdict::Incomplete(issues) => vec![LoopMessage::User(UserMessage::text(format!(
            "{CRITIC_TAG} A review of your work found it may not be done yet. Address these \
             before reporting complete, or explain why they don't apply (e.g. they're out of \
             scope or something you were told not to do):\n{issues}"
        )))],
        Verdict::Abstain(missing) => vec![LoopMessage::User(UserMessage::text(format!(
            "{CRITIC_TAG} A review could not confirm this is correct from the spec and \
             evidence available. Rather than assume it's done, add a focused test (or state \
             the missing spec detail) that would prove it, then continue.\n{missing}"
        )))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_rules_counts_chars_not_bytes() {
        use std::borrow::Cow;
        // ASCII within cap → borrowed, untouched.
        assert!(matches!(
            truncate_rules("abc", 10, "…(instructions truncated)"),
            Cow::Borrowed(_)
        ));
        // ASCII over cap → truncated to `max` chars + note.
        assert_eq!(
            truncate_rules("abcdefghij", 4, "|NOTE").into_owned(),
            "abcd|NOTE"
        );
        // 6 × 4-byte chars = 24 bytes but only 6 chars. A byte-based gate
        // (`.len() > MAX`) would truncate this even though it's under the
        // char cap; the helper must count chars and leave it untouched.
        let mb = "🦀🦀🦀🦀🦀🦀";
        assert_eq!(mb.len(), 24);
        assert!(matches!(truncate_rules(mb, 10, "|NOTE"), Cow::Borrowed(_)));
        // Multibyte over the CHAR cap → truncated to `max` chars + note.
        let over = "🦀🦀🦀🦀"; // 4 chars, 16 bytes
        assert_eq!(truncate_rules(over, 2, "|NOTE").into_owned(), "🦀🦀|NOTE");
    }

    #[test]
    fn parse_complete_returns_complete() {
        assert_eq!(parse_verdict("VERDICT: COMPLETE"), Verdict::Complete);
        assert_eq!(
            parse_verdict("verdict: complete\n(looks good)"),
            Verdict::Complete
        );
    }

    #[test]
    fn parse_incomplete_returns_incomplete_with_issues() {
        let v = parse_verdict("VERDICT: INCOMPLETE\n- missing test\n- error path unhandled");
        match v {
            Verdict::Incomplete(issues) => {
                assert!(issues.contains("missing test"));
                assert!(issues.contains("error path"));
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_or_ambiguous_returns_complete() {
        assert_eq!(parse_verdict(""), Verdict::Complete);
        assert_eq!(parse_verdict("   \n  "), Verdict::Complete);
        assert_eq!(
            parse_verdict("I think it's probably fine?"),
            Verdict::Complete
        );
    }

    #[test]
    fn parse_abstain_returns_abstain_with_detail() {
        let v = parse_verdict("VERDICT: ABSTAIN\nNo test covers the retry-on-timeout path.");
        match v {
            Verdict::Abstain(detail) => {
                assert!(detail.contains("retry-on-timeout"));
            }
            other => panic!("expected Abstain, got {other:?}"),
        }
        // INSUFFICIENT is an accepted synonym.
        let v2 = parse_verdict("VERDICT: INSUFFICIENT\nSpec unclear on error format.");
        match v2 {
            Verdict::Abstain(detail) => {
                assert!(detail.contains("error format"));
            }
            other => panic!("expected Abstain, got {other:?}"),
        }
    }

    #[test]
    fn parse_incomplete_before_abstain_priority() {
        // First line mentions both — INCOMPLETE wins.
        let v = parse_verdict("VERDICT: INCOMPLETE, or perhaps ABSTAIN\nmissing tests");
        match v {
            Verdict::Incomplete(issues) => {
                assert!(issues.contains("missing tests"));
            }
            other => panic!("expected Incomplete (priority over ABSTAIN), got {other:?}"),
        }
    }

    #[test]
    fn prompt_embeds_transcript_format_and_rules() {
        let p = build_prompt(
            "RULE: never push to remote.",
            "user asked X; assistant edited foo.rs",
            None,
        );
        assert!(p.contains("VERDICT: COMPLETE"));
        assert!(p.contains("VERDICT: INCOMPLETE"));
        assert!(p.contains("VERDICT: ABSTAIN"));
        assert!(p.contains("edited foo.rs"));
        // dirge-bedj: the agent's own constraints are included so the
        // critic judges within them.
        assert!(p.contains("never push to remote"), "rules must be embedded");
        assert!(
            p.to_lowercase().contains("forbidden") || p.to_lowercase().contains("out-of-scope"),
            "prompt must instruct the critic to respect constraints",
        );
    }

    #[test]
    fn empty_rules_render_a_placeholder_not_blank() {
        let p = build_prompt("", "did stuff", None);
        assert!(p.contains("no special constraints"));
    }

    /// dirge: the critic's `rules` is the agent's merged system prompt, which
    /// after a compaction carries the `[CONTEXT COMPACTION — REFERENCE ONLY]`
    /// summary describing ALREADY-COMPLETED prior work. The critic must judge
    /// against the agent's real constraints, not a stale summary's
    /// `## Active Task` — else it blocks finalization on superseded work
    /// (e.g. demanding an old "Phase 3" that's already done).
    #[test]
    fn build_prompt_drops_the_compaction_summary_from_rules() {
        let rules = format!(
            "RULE: never push to remote.\n\n{} \
             ## Active Task\nFinish Phase 3: wire the Janet loader and add tests.",
            crate::agent::compression::COMPACTION_MARKER,
        );
        let p = build_prompt(&rules, "user asked X; assistant edited foo.rs", None);
        // The agent's genuine constraint (it precedes the summary) survives…
        assert!(
            p.contains("never push to remote"),
            "real rules must survive"
        );
        // …but the stale summary's contents must NOT reach the critic.
        assert!(
            !p.contains("Active Task") && !p.contains("Phase 3") && !p.contains("Janet"),
            "the compaction summary must be stripped from the critic's rules",
        );
        assert!(
            !p.contains(crate::agent::compression::COMPACTION_MARKER),
            "the compaction marker itself must be stripped",
        );
    }

    /// Defense-in-depth: even if a summary block reaches the critic by some
    /// other path, the preamble tells it to discount reference-only material.
    #[test]
    fn preamble_discounts_reference_only_blocks() {
        let lower = CRITIC_PREAMBLE.to_ascii_lowercase();
        assert!(
            lower.contains("reference") || lower.contains("compaction"),
            "preamble must tell the critic to ignore reference-only/compaction blocks",
        );
    }

    #[test]
    fn build_prompt_caps_large_rules() {
        let huge = "x".repeat(MAX_RULES_CHARS + 5_000);
        let p = build_prompt(&huge, "t", None);
        assert!(p.contains("instructions truncated"));
        // The rules block is bounded (cap + the transcript/format scaffold,
        // well under the untruncated size).
        assert!(p.len() < MAX_RULES_CHARS + 4_000);
    }

    /// The system preamble states the critic's ROLE, keeps FORMAT out, and
    /// (dirge-bedj) instructs it to respect the agent's constraints.
    #[test]
    fn preamble_is_calibrated_and_constraint_aware() {
        let lower = CRITIC_PREAMBLE.to_ascii_lowercase();
        assert!(lower.contains("critic"), "preamble must name the role");
        assert!(!lower.contains("summarizer"));
        // Format lives in the prompt, not the system preamble.
        assert!(!CRITIC_PREAMBLE.contains("VERDICT:"));
        assert!(build_prompt("", "t", None).contains("VERDICT:"));
        // Must not demand forbidden actions, and must respect instructions.
        assert!(
            lower.contains("respect"),
            "must say to respect instructions"
        );
        assert!(
            lower.contains("never flag the absence") || lower.contains("forbid"),
            "must forbid demanding disallowed actions",
        );
        assert!(lower.contains("unsure"), "must keep the fail-open guidance");
    }

    // dirge-6q3w: verification-status block.

    /// No gate configured → prompt is byte-identical to the pre-feature
    /// behavior (no verification block at all).
    #[test]
    fn no_verification_status_adds_no_block() {
        let p = build_prompt("rules", "did stuff", None);
        assert!(!p.contains("verification status"));
    }

    /// Precondition: no code edited this run → no verification pressure,
    /// even though the gate is present. The critic shouldn't nag about
    /// tests on a read-only / Q&A turn.
    #[test]
    fn no_code_edited_adds_no_block() {
        let p = build_prompt("rules", "did stuff", Some(VerificationStatus::NoCodeEdited));
        assert!(!p.contains("verification status"));
    }

    #[test]
    fn unverified_block_pushes_to_run_a_check() {
        let p = build_prompt(
            "rules",
            "edited foo.rs",
            Some(VerificationStatus::Unverified),
        );
        assert!(p.contains("verification status"));
        let lower = p.to_lowercase();
        assert!(lower.contains("no build/test/lint was detected"));
        assert!(lower.contains("concrete"), "must frame it as a real gap");
    }

    /// The unverified block must stay a soft nudge with an explicit escape
    /// hatch, so the model never fabricates a test that can't be run.
    #[test]
    fn unverified_block_is_a_soft_nudge() {
        let p = build_prompt(
            "rules",
            "edited foo.rs",
            Some(VerificationStatus::Unverified),
        );
        let lower = p.to_lowercase();
        assert!(lower.contains("nudge"), "must call itself a nudge");
        assert!(
            lower.contains("isn't testable") || lower.contains("nothing to run"),
            "must offer a not-testable escape",
        );
        assert!(
            lower.contains("never force a test that can't be run"),
            "must forbid fabricating an unrunnable test",
        );
    }

    #[test]
    fn red_block_forbids_passing_a_red_build() {
        let p = build_prompt(
            "rules",
            "edited foo.rs",
            Some(VerificationStatus::VerifiedRed),
        );
        let lower = p.to_lowercase();
        assert!(lower.contains("failed"));
        assert!(lower.contains("incomplete"));
    }

    #[test]
    fn green_block_stays_calibrated() {
        let p = build_prompt(
            "rules",
            "edited foo.rs",
            Some(VerificationStatus::VerifiedGreen),
        );
        let lower = p.to_lowercase();
        assert!(lower.contains("passed"));
        // Must not manufacture new requirements on a green run.
        assert!(lower.contains("relevant"));
    }

    #[tokio::test]
    async fn run_critic_threads_verification_into_prompt() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let critic: CriticFn = Arc::new(move |prompt: String| {
            *seen2.lock().unwrap() = prompt;
            Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) })
        });
        let _ = run_critic(
            &critic,
            "rules",
            "edited foo.rs",
            Some(VerificationStatus::Unverified),
        )
        .await;
        assert!(
            seen.lock().unwrap().contains("verification status"),
            "the verification signal must reach the critic prompt",
        );
    }

    #[tokio::test]
    async fn run_critic_injects_followup_when_incomplete() {
        let critic: CriticFn = Arc::new(|_prompt| {
            Box::pin(async { Ok("VERDICT: INCOMPLETE\n- the test was never run".to_string()) })
        });
        let msgs = run_critic(&critic, "rules", "did stuff", None).await;
        assert_eq!(msgs.len(), 1);
        let content = match &msgs[0] {
            LoopMessage::User(u) => u.text_joined(),
            _ => panic!("expected user message"),
        };
        assert!(content.starts_with(CRITIC_TAG));
        assert!(content.contains("test was never run"));
    }

    #[tokio::test]
    async fn run_critic_abstain_injects_test_request_nudge() {
        let critic: CriticFn = Arc::new(|_prompt| {
            Box::pin(async {
                Ok("VERDICT: ABSTAIN\nNo test covers the retry-on-timeout path.".to_string())
            })
        });
        let msgs = run_critic(&critic, "rules", "did stuff", None).await;
        assert_eq!(msgs.len(), 1);
        let content = match &msgs[0] {
            LoopMessage::User(u) => u.text_joined(),
            _ => panic!("expected user message"),
        };
        assert!(
            content.starts_with(CRITIC_TAG),
            "abstain nudge must use CRITIC_TAG"
        );
        assert!(
            content.contains("could not confirm"),
            "abstain nudge must say 'could not confirm', got: {content}"
        );
        assert!(
            content.contains("retry-on-timeout"),
            "abstain nudge must carry the critic's detail, got: {content}"
        );
    }

    #[tokio::test]
    async fn run_critic_passes_rules_into_prompt() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let critic: CriticFn = Arc::new(move |prompt: String| {
            *seen2.lock().unwrap() = prompt;
            Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) })
        });
        let _ = run_critic(&critic, "RULE: do not deploy", "did stuff", None).await;
        assert!(
            seen.lock().unwrap().contains("do not deploy"),
            "the agent's constraints must reach the critic prompt",
        );
    }

    #[tokio::test]
    async fn run_critic_silent_when_complete() {
        let critic: CriticFn =
            Arc::new(|_p| Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) }));
        assert!(
            run_critic(&critic, "rules", "did stuff", None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn run_critic_fails_open_on_error() {
        let critic: CriticFn = Arc::new(|_p| Box::pin(async { anyhow::bail!("provider down") }));
        assert!(
            run_critic(&critic, "rules", "did stuff", None)
                .await
                .is_empty(),
            "a critic error must not block finalization"
        );
    }
}

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

use super::code_review::{Finding, parse_findings, partition_findings};
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
    // dirge-kjzg: share the one char-based head truncator. `note` is a fixed
    // suffix here (not parameterized by the dropped count), so ignore it.
    crate::text::truncate_head(rules, max, |_| note.to_string())
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
/// transcript. The response FORMAT lives in [`UNIFIED_FORMAT`] instead —
/// right next to the material being judged. dirge-8v98: when `code_review`
/// is on, `build.rs` appends the reviewer's role to this preamble so the one
/// judge covers both completeness and diff review.
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

// ── Unified finalization judge (dirge-8v98) ───────────────────────────────
//
// One judge call that does BOTH the critic's completeness check AND the
// diff-aware code review, returning a single consolidated follow-up. Replaces
// the two separate judge calls; the reviewer's role instructions are appended
// to the (possibly custom) critic preamble at arm time in `build.rs`, and the
// combined output format below rides in the prompt.

/// Combined response-format instruction for the unified judge: a completeness
/// verdict followed by a `FINDINGS:` section reviewing the diff. Carried in the
/// prompt (not the preamble) so it sits beside the material and a custom
/// `critic_preamble` still receives it. [`parse_unified`] keys on the
/// `VERDICT:` first line and the `FINDINGS:` marker.
const UNIFIED_FORMAT: &str = "\
Respond in EXACTLY this structure and nothing else.\n\
\n\
First line — a verdict, one of `VERDICT: COMPLETE`, `VERDICT: INCOMPLETE`, or `VERDICT: ABSTAIN`:\n\
- COMPLETE: the work is done and correct.\n\
- INCOMPLETE: concrete, in-scope gaps remain. Follow with a short bullet list of the gaps.\n\
- ABSTAIN: the spec or evidence available is insufficient to judge correctness. Say what test or \
spec detail is missing. An ABSTAIN still blocks, but is resolved by adding evidence, not fixes.\n\
\n\
Then a line reading exactly `FINDINGS:` followed by any defects in the diff below — each on its \
own bullet leading with a severity word (critical/high/medium/low), then the narrowest file/line \
location, the concrete harm if left unfixed, and a suggested fix. Separate multiple findings with \
`---` on its own line. If the diff is clean or none is shown, write `FINDINGS: none`.";

/// Marker separating the completeness verdict from the diff findings in the
/// unified response. Matched case-insensitively on its first occurrence.
const FINDINGS_MARKER: &str = "FINDINGS:";

/// Build the unified judge prompt: the completeness question always, plus the
/// run's `diff` to review when `Some`. `rules` is the assistant's own system
/// prompt (so the judge reasons within the same constraints); `verification`
/// is the run's compile/lint/test signal.
pub fn build_unified_prompt(
    rules: &str,
    transcript: &str,
    diff: Option<&str>,
    verification: Option<VerificationStatus>,
    // dirge-9b2k R2: findings a prior Blocking reaction raised. The judge is told
    // to re-raise one only if it's still present AND the model neither fixed nor
    // justified it (blindly re-emitting a declined finding is the duplicate bug;
    // silently dropping an unaddressed one is the opposite failure). `None`/blank
    // = no section.
    prior_findings: Option<&str>,
) -> String {
    let rules = strip_compaction_summary(rules).trim();
    let rules_block = if rules.is_empty() {
        "(no special constraints provided)".to_string()
    } else {
        truncate_rules(rules, MAX_RULES_CHARS, "\n…(instructions truncated)").into_owned()
    };
    let diff_block = match diff {
        Some(d) if !d.trim().is_empty() => format!(
            "\n\n--- diff under review (review for defects; report them in FINDINGS) ---\n{}\n--- end diff ---",
            d.trim()
        ),
        _ => String::new(),
    };
    let prior_findings_block = match prior_findings {
        Some(p) if !p.trim().is_empty() => format!(
            "\n\n--- findings raised on an earlier review (the assistant's response is in the \
             transcript above) ---\n{}\n--- end prior findings ---\n\
             For each: if the assistant fixed it, or gave a sound reason for leaving it, do NOT \
             re-raise it. If it is still present and the assistant neither fixed nor justified \
             it, DO re-raise it. Only re-raise a justified finding when the justification is \
             factually wrong — then explain why it fails.",
            p.trim()
        ),
        _ => String::new(),
    };
    format!(
        "{UNIFIED_FORMAT}\n\n\
         --- assistant instructions & constraints (judge within these; never demand a \
         forbidden/out-of-scope action) ---\n{rules_block}\n--- end instructions ---\n\n\
         --- transcript ---\n{transcript}\n--- end transcript ---{diff_block}{prior_findings_block}{}",
        verification_block(verification)
    )
}

/// Split the unified response into its completeness verdict and its diff
/// findings, parsing each with the existing single-purpose parsers. Findings
/// are severity-sorted (highest first).
pub fn parse_unified(response: &str) -> (Verdict, Vec<Finding>) {
    let (head, tail) = split_on_findings_marker(response);
    let verdict = parse_verdict(head);
    let mut findings = parse_findings(tail);
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    (verdict, findings)
}

/// Split at the first case-insensitive `FINDINGS:` marker → `(verdict head,
/// findings tail)`. When the marker is absent the whole response is the verdict
/// head and the findings tail is empty (a diff-less completeness-only run).
fn split_on_findings_marker(response: &str) -> (&str, &str) {
    let lower = response.to_ascii_lowercase();
    match lower.find(&FINDINGS_MARKER.to_ascii_lowercase()) {
        Some(idx) => (&response[..idx], &response[idx + FINDINGS_MARKER.len()..]),
        None => (response, ""),
    }
}

/// Join finding bodies with the `---` separator, each led by its severity.
fn render_findings(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| format!("[{}] {}", f.severity.label(), f.body.trim()))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Build the single consolidated finalization follow-up from the unified
/// judge's verdict + findings (dirge-8v98). Completeness gaps and high/critical
/// findings are must-address; medium/low ride along as optional notes so the
/// model can knock them out in the same pass. Returns `None` only when the work
/// is COMPLETE and the diff is clean — nothing to send, the loop finalizes.
pub fn build_unified_followup(verdict: Verdict, findings: Vec<Finding>) -> Option<LoopMessage> {
    let gaps = match &verdict {
        Verdict::Complete => None,
        Verdict::Incomplete(issues) => Some(("the task may not be done yet", issues.clone())),
        Verdict::Abstain(missing) => Some((
            "correctness couldn't be confirmed — add a focused test or state the missing spec detail",
            missing.clone(),
        )),
    };
    let (blocking, advisory) = partition_findings(findings);
    if gaps.is_none() && blocking.is_empty() && advisory.is_empty() {
        return None;
    }
    let mut sections: Vec<String> = Vec::new();
    if let Some((label, body)) = gaps {
        sections.push(format!("Completeness — {label}:\n{}", body.trim()));
    }
    if !blocking.is_empty() {
        sections.push(format!(
            "Bugs to fix (high severity):\n{}",
            render_findings(&blocking)
        ));
    }
    if !advisory.is_empty() {
        sections.push(format!(
            "Lower-priority notes (optional; address if quick, else say why you're leaving them):\n{}",
            render_findings(&advisory)
        ));
    }
    let body = sections.join("\n\n");
    Some(LoopMessage::User(UserMessage::text(format!(
        "{CRITIC_TAG} A review of your work found things to address before you report complete. \
         Fix each, or explain why it doesn't apply (out of scope, intended, or something you were \
         told not to do):\n\n{body}"
    ))))
}

/// Run the unified finalization judge: ONE call that judges completeness AND
/// reviews the run's diff (when `diff` is `Some`), returning at most one
/// consolidated [`CRITIC_TAG`] follow-up. Replaces the separate critic +
/// code-review calls (dirge-8v98). Fail-open: a judge error/timeout finalizes
/// without blocking.
pub async fn run_unified_review(
    judge: &CriticFn,
    rules: &str,
    transcript: &str,
    diff: Option<&str>,
    verification: Option<VerificationStatus>,
    prior_findings: Option<&str>,
) -> (Vec<LoopMessage>, Option<String>) {
    let prompt = build_unified_prompt(rules, transcript, diff, verification, prior_findings);
    let response = run_judge!(
        judge,
        prompt,
        "dirge::critic",
        "unified review call failed; finalizing without it",
        (Vec::new(), None)
    );
    let (verdict, findings) = parse_unified(&response);
    // dirge-9b2k R2: surface the rendered findings so the caller can hand them
    // to the next reaction's judge prompt (Blocking path in run.rs).
    let raised = if findings.is_empty() {
        None
    } else {
        Some(render_findings(&findings))
    };
    let msgs = build_unified_followup(verdict, findings)
        .into_iter()
        .collect();
    (msgs, raised)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test shim (dirge-8v98): the old `build_prompt` was folded into
    /// `build_unified_prompt` with a `None` diff (completeness-only). The prompt
    /// tests below still exercise the shared rules/compaction/verification/format
    /// behavior through it.
    fn build_prompt(
        rules: &str,
        transcript: &str,
        verification: Option<VerificationStatus>,
    ) -> String {
        build_unified_prompt(rules, transcript, None, verification, None)
    }

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
    async fn unified_review_threads_verification_and_rules_into_prompt() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let judge: CriticFn = Arc::new(move |prompt: String| {
            *seen2.lock().unwrap() = prompt;
            Box::pin(async { Ok("VERDICT: COMPLETE\nFINDINGS: none".to_string()) })
        });
        let _ = run_unified_review(
            &judge,
            "RULE: do not deploy",
            "edited foo.rs",
            None,
            Some(VerificationStatus::Unverified),
            None,
        )
        .await;
        let prompt = seen.lock().unwrap().clone();
        assert!(
            prompt.contains("verification status"),
            "the verification signal must reach the judge prompt"
        );
        assert!(
            prompt.contains("do not deploy"),
            "the agent's constraints must reach the judge prompt"
        );
    }

    #[tokio::test]
    async fn unified_review_silent_when_complete_and_clean() {
        let judge: CriticFn =
            Arc::new(|_p| Box::pin(async { Ok("VERDICT: COMPLETE\nFINDINGS: none".to_string()) }));
        assert!(
            run_unified_review(&judge, "rules", "did stuff", None, None, None)
                .await
                .0
                .is_empty()
        );
    }

    // ── Unified finalization judge (dirge-8v98) ──

    use crate::agent::agent_loop::code_review::Severity;

    fn msg_text(m: &LoopMessage) -> String {
        match m {
            LoopMessage::User(u) => u.text_joined(),
            _ => panic!("expected a user follow-up message"),
        }
    }

    #[test]
    fn parse_unified_complete_and_clean() {
        let (v, f) = parse_unified("VERDICT: COMPLETE\n\nFINDINGS: none");
        assert!(matches!(v, Verdict::Complete));
        assert!(f.is_empty());
    }

    #[test]
    fn parse_unified_incomplete_with_findings_severity_sorted() {
        let resp = "VERDICT: INCOMPLETE\n- the --skill arg is never parsed\n\n\
                    FINDINGS:\n- low: inconsistent spacing in warnings.\n---\n\
                    - high: line 834 missing closing paren -> SyntaxError. Fix: add ).";
        let (v, f) = parse_unified(resp);
        match v {
            Verdict::Incomplete(gaps) => assert!(gaps.contains("--skill"), "gaps: {gaps}"),
            other => panic!("expected incomplete, got {other:?}"),
        }
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].severity, Severity::High, "sorted highest-first");
        assert_eq!(f[1].severity, Severity::Low);
    }

    #[test]
    fn parse_unified_marker_case_insensitive_and_absent() {
        // Marker absent (a diff-less completeness run) → verdict only.
        let (v, f) = parse_unified("VERDICT: COMPLETE");
        assert!(matches!(v, Verdict::Complete));
        assert!(f.is_empty());
        // Lowercase marker still splits verdict from findings.
        let (v2, f2) =
            parse_unified("VERDICT: INCOMPLETE\n- gap\n\nfindings:\n- critical: rce in exec path.");
        assert!(matches!(v2, Verdict::Incomplete(_)));
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].severity, Severity::Critical);
    }

    #[test]
    fn unified_followup_none_when_complete_and_clean() {
        assert!(build_unified_followup(Verdict::Complete, Vec::new()).is_none());
    }

    #[test]
    fn unified_followup_reenters_for_high_finding_even_when_complete() {
        // Completeness passed but the diff has a showstopper — must still
        // re-enter. This is the exact case the display-only advisory swallowed.
        let findings = parse_findings("- high: missing closing paren -> SyntaxError.");
        let msg = build_unified_followup(Verdict::Complete, findings).expect("some");
        let text = msg_text(&msg);
        assert!(text.starts_with(CRITIC_TAG));
        assert!(text.contains("Bugs to fix"));
        assert!(text.to_lowercase().contains("syntaxerror"));
        assert!(
            !text.contains("Completeness"),
            "no completeness section when verdict was COMPLETE"
        );
    }

    #[test]
    fn unified_followup_reenters_for_low_only_finding() {
        // "Re-enter once for any finding": a nitpick-only result still reaches
        // the model (as optional), never a user-only wall.
        let findings = parse_findings("- low: inconsistent spacing in warnings.");
        let msg = build_unified_followup(Verdict::Complete, findings).expect("some");
        let text = msg_text(&msg);
        assert!(text.contains("Lower-priority notes"));
        assert!(!text.contains("Bugs to fix"));
    }

    #[test]
    fn unified_followup_combines_completeness_and_findings() {
        let findings = parse_findings("- critical: auth bypass.\n---\n- medium: dup logic.");
        let msg =
            build_unified_followup(Verdict::Incomplete("- X is missing".to_string()), findings)
                .expect("some");
        let text = msg_text(&msg);
        assert!(text.contains("Completeness"));
        assert!(text.contains("X is missing"));
        assert!(text.contains("Bugs to fix"));
        assert!(text.contains("Lower-priority notes"));
    }

    #[test]
    fn unified_prompt_includes_diff_only_when_present() {
        let with = build_unified_prompt(
            "rules",
            "did stuff",
            Some("@@ -1 +1 @@\n-a\n+b"),
            None,
            None,
        );
        assert!(with.contains("diff under review"));
        assert!(with.contains("+b"));
        let without = build_unified_prompt("rules", "did stuff", None, None, None);
        assert!(!without.contains("diff under review"));
        // Both carry the combined verdict+findings format contract.
        assert!(with.contains("FINDINGS:"));
        assert!(without.contains("FINDINGS:"));
    }

    #[test]
    fn unified_prompt_omits_prior_findings_section_when_none() {
        // dirge-9b2k R2: no prior findings → no new section (identical to the
        // pre-R2 prompt).
        let p = build_unified_prompt(
            "rules",
            "did stuff",
            Some("@@ -1 +1 @@\n-a\n+b"),
            None,
            None,
        );
        assert!(!p.contains("earlier review"));
        assert!(p.contains("diff under review"));
    }

    #[test]
    fn unified_prompt_omits_prior_findings_section_when_blank() {
        // A whitespace-only string carries no real findings — still no section.
        let p = build_unified_prompt("rules", "did stuff", Some("diff"), None, Some("  \n "));
        assert!(!p.contains("earlier review"));
    }

    #[test]
    fn unified_prompt_includes_prior_findings_section_when_present() {
        let p = build_unified_prompt(
            "rules",
            "did stuff",
            Some("diff"),
            None,
            Some("- High — sql injection"),
        );
        assert!(
            p.contains("earlier review"),
            "prior-findings section must appear"
        );
        assert!(
            p.contains("- High — sql injection"),
            "the prior findings must be inlined"
        );
        assert!(
            p.contains("do NOT re-raise"),
            "the suppress-if-addressed instruction must be present"
        );
        assert!(
            p.contains("neither fixed nor justified"),
            "the re-raise-if-unaddressed instruction must be present"
        );
    }

    #[tokio::test]
    async fn run_unified_review_fails_open_on_error() {
        let judge: CriticFn = Arc::new(|_p| Box::pin(async { anyhow::bail!("provider down") }));
        assert!(
            run_unified_review(&judge, "rules", "did stuff", Some("diff"), None, None)
                .await
                .0
                .is_empty(),
            "a judge error must not block finalization"
        );
    }
}

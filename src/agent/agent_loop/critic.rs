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
use std::sync::LazyLock;

use regex::Regex;

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

/// Judge callback that returns the INDEX of a chosen option, never prose
/// (dirge-5mtx.3). The caller supplies a closed answer set as a `&'static`
/// slice; the judge is constrained to emit exactly one member, and the result
/// comes back as an index — so there is no free-text verdict to misread. This
/// is the §7 split (classify and respond as separate calls) applied to a
/// genuinely binary question: the next consumer is dirge-5mtx.4's
/// blocked-vs-next-step gate.
pub type ClassifyFn = Arc<
    dyn Fn(
            String,
            &'static [&'static str],
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<usize>> + Send>>
        + Send
        + Sync,
>;

/// Classify a judge response against a fixed answer set. Matches each option
/// as a WHOLE WORD, case-insensitively — the same regex/word-boundary
/// discipline the verdict tokens use (dirge-5mtx), so an option named `MET`
/// does NOT match inside `UNMET`. Bare `contains` is exactly the trap that
/// produced dirge-5mtx.3 and is deliberately not used.
///
/// Returns the index of the single option that matched, or:
/// - `None` when NO option matched (no answer), and
/// - `None` when MORE THAN ONE distinct option matched (ambiguous).
///
/// Ambiguity is a non-answer on purpose: a confused judge must be visible to
/// the caller as a non-answer so it can fall back, never silently resolved by
/// position or first-match. Repetition of the SAME option is not ambiguity.
///
/// # Precondition
/// No option may be a (case-insensitive) substring of another — overlapping
/// answer sets are the hazard this function exists to remove. Checked by a
/// `debug_assert` so a competing set fails loudly in tests rather than
/// silently misclassifying. Pick non-nesting members (e.g. `MET / SHORT`, not
/// `MET / UNMET`).
pub fn parse_choice(response: &str, options: &[&str]) -> Option<usize> {
    debug_assert!(
        !options_overlap(options),
        "parse_choice: answer-set members must not be substrings of one another (dirge-5mtx.3)"
    );
    let mut found: Option<usize> = None;
    for (i, opt) in options.iter().enumerate() {
        if whole_word_present(response, opt) {
            if found.is_some() {
                return None; // a second DISTINCT option matched → ambiguous, not first-wins
            }
            found = Some(i);
        }
    }
    found
}

/// Whether `needle` appears in `haystack` as a whole word, case-insensitively.
/// `\b` stops `MET` from matching inside `UNMET`; `(?i)` matches any casing
/// without upper-casing both sides. Escaped so option text (e.g. `next-step`)
/// is matched literally, not as a regex.
fn whole_word_present(haystack: &str, needle: &str) -> bool {
    let pattern = format!(r"(?i)\b{}\b", regex::escape(needle));
    // `regex::escape` guarantees a valid pattern; the only fixed wrapper is
    // `\b`, so this cannot fail.
    Regex::new(&pattern)
        .expect("escaped word-boundary pattern is always valid")
        .is_match(haystack)
}

/// `true` when some option is a (case-insensitive) substring of another — i.e.
/// the answer set nests, which is a caller bug (dirge-5mtx.3). Substring
/// subsumes whole-word match, so this single check covers both failure modes.
fn options_overlap(options: &[&str]) -> bool {
    for (i, a) in options.iter().enumerate() {
        for (j, b) in options.iter().enumerate() {
            if i != j && a.to_lowercase().contains(&b.to_lowercase()) {
                return true;
            }
        }
    }
    false
}

/// System preamble for the classify judge (dirge-5mtx.3). Minimal on purpose:
/// the constraint that actually matters — answer with exactly one option word
/// — lives in the user prompt ([`classify_prompt`]), right next to the
/// question. A heavy system role would invite the very prose this exists to
/// suppress. Passed as the LLM system prompt by `build_classify_fn`.
pub const CLASSIFY_PREAMBLE: &str = "\
You answer a single question by choosing exactly one option from a fixed set. Reply with that one \
option word and nothing else — no reasoning, no punctuation, no extra text.";

/// Build the constrained classify user prompt: states the question, names every
/// option, and demands the answer be EXACTLY one of them, nothing else. Kept
/// short — a long preamble invites prose, and prose is what [`parse_choice`]
/// then has to dig an option word out of (dirge-5mtx.3).
pub fn classify_prompt(question: &str, options: &[&str]) -> String {
    let list = options
        .iter()
        .map(|o| format!("`{o}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{question}\n\nAnswer with EXACTLY ONE of these words and nothing else: {list}.\n\
         Do not add explanation, punctuation, or any other text."
    )
}

/// The retry prompt: a terser restatement used when [`parse_choice`] already
/// failed once (the model hedged in prose or emitted no option word). The
/// question context already reached the model on the first call, so the retry
/// only needs to re-deny prose and re-state the options.
pub(crate) fn classify_retry_prompt(options: &[&str]) -> String {
    let list = options
        .iter()
        .map(|o| format!("`{o}`"))
        .collect::<Vec<_>>()
        .join(" / ");
    format!("Reply with a single word — one of: {list}. Nothing else.")
}

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
- If the assistant ended by asking the user a question or presenting options and is waiting on their \
decision, that is a CORRECT stopping point, not incompleteness — never tell it to proceed anyway, \
pick a default, or guess. Judge only the work done up to the question.\n\
- The `--- evidence ... ---` block in the prompt is a factual record of this run. Check the \
assistant's claims against it: a claim that names a file it changed, a command it ran, a result \
it produced, or a source it consulted (a fetched page, docs, an external service), where the \
evidence shows no such thing — e.g. claiming to have checked a web page while no fetch/search \
tool appears in `tools invoked` — is an UNSUPPORTED claim — flag it with the concrete mismatch. \
Never invent a mismatch the evidence does not show, and never flag a claim the evidence supports.\n\
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
        // dirge-uw2l.2: cheap tier green, slow tier never seen green. Same
        // calibrated escape hatch as `Unverified` — the point is to ask for
        // the suite once, not to invent a requirement where none can run.
        Some(VerificationStatus::FastGreenOnly) => {
            "\n\n--- verification status ---\n\
             Code was edited and the fast checks (typecheck/lint/a targeted test) passed, but the \
             full test suite never ran this run. If a broader suite is runnable here and not \
             forbidden, flag that as a concrete gap and name the command. This is a NUDGE, not a \
             hard rule: if there is no broader suite, it can't run here, or the assistant verified \
             end-to-end another way and said so, treat it as COMPLETE.\n\
             --- end verification status ---"
        }
        // No code edited (precondition not met) or no gate configured →
        // add nothing, so the critic behaves exactly as before.
        Some(VerificationStatus::NoCodeEdited) | None => "",
    }
}

/// Deterministic evidence about THIS run, rendered into the critic prompt so
/// the judge can check the assistant's factual claims against what actually
/// happened (dirge-d0e5.3). Complements the aggregate
/// [`VerificationStatus`] block: this names files, commands, and counts, so
/// a claim like "I applied the two awk fixes" is checkable against the file
/// list rather than only against "unverified".
#[derive(Debug, Default, Clone)]
pub struct Evidence {
    /// Paths the tracker recorded as mutated since the run's epoch.
    pub files_mutated: Vec<String>,
    /// Verification commands observed this run, each with whether it failed,
    /// latest-first (see [`crate::agent::agent_loop::verifier`]).
    pub observed_commands: Vec<(String, bool)>,
    /// Tool-result messages in this finalization's message list.
    pub tool_calls: usize,
    /// Distinct tool names invoked this run. Kept raw here; the renderer
    /// sorts and dedups so the prompt block is compact and stable across
    /// runs (dirge-lavc GAP 4). This makes a sourcing claim checkable the
    /// same way a file claim is: "checked the X page" with no fetch/search
    /// tool in the list is UNSUPPORTED.
    pub tool_names: Vec<String>,
}

/// Render the evidence block for the critic prompt (dirge-d0e5.3). Empty
/// when there is no evidence to show. The block renders even when the lists
/// are empty — `(none)` is the honest signal that a claim has nothing to
/// stand on, and absence is as informative as presence.
fn evidence_block(evidence: Option<&Evidence>) -> String {
    let Some(e) = evidence else {
        return String::new();
    };
    let files = if e.files_mutated.is_empty() {
        "(none)".to_string()
    } else {
        e.files_mutated.join(", ")
    };
    let commands = if e.observed_commands.is_empty() {
        "(none observed)".to_string()
    } else {
        e.observed_commands
            .iter()
            .map(|(cmd, failed)| format!("{cmd} — {}", if *failed { "FAILED" } else { "passed" }))
            .collect::<Vec<_>>()
            .join("; ")
    };
    // dirge-lavc GAP 4: sort + dedup here so the rendered set is compact
    // and stable across runs, whatever the construction site passes.
    let mut tool_names = e.tool_names.clone();
    tool_names.sort();
    tool_names.dedup();
    let tools = if tool_names.is_empty() {
        "(none)".to_string()
    } else {
        tool_names.join(", ")
    };
    format!(
        "\n\n--- evidence of what happened this run (check the assistant's claims against this; \
         a claim naming a file, a command, a consulted source, or an outcome that is absent here \
         is UNSUPPORTED) ---\n\
         files mutated: {files}\n\
         verification commands observed: {commands}\n\
         tool calls: {}\n\
         tools invoked: {tools}\n--- end evidence ---",
        e.tool_calls
    )
}

/// Classified verdict signal, strongest (most action-forcing) first. Shared by
/// the critic and goal parsers so both judge the same surface form the same way
/// (dirge-5mtx).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictSignal {
    /// Not done / gaps remain / goal unmet.
    Negative,
    /// Cannot verify from the spec/evidence available.
    Abstain,
    /// Done / goal met.
    Positive,
    /// No verdict token anywhere in the head — fail open.
    None,
}

// dirge-5mtx: whole-word verdict classification. `COMPLETE` is a proper
// substring of `INCOMPLETE` and `MET` of `UNMET`, so a naive `contains` lets
// the answer sets compete — whichever branch the code tests first wins,
// independent of what the judge meant. Word boundaries stop that; an explicit
// `NOT` before a positive word (`NOT COMPLETE`, `NOT MET`) flips it negative
// rather than letting the embedded positive token win. `FINISHED` is only
// meaningful negated (`NOT FINISHED`), so it appears in the negation set but
// NOT in the positive set. All four run against upper-cased text.
static NEGATED_POSITIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bNOT\s+(?:COMPLETE|DONE|MET|SATISFIED|FINISHED)\b").unwrap());
static NEGATIVE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:INCOMPLETE|GAPS|SHORT|UNMET)\b").unwrap());
static ABSTAIN_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:ABSTAIN|INSUFFICIENT|UNSURE)\b").unwrap());
static POSITIVE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:COMPLETE|DONE|MET|SATISFIED)\b").unwrap());

/// Whether a single (upper-cased) line bears any verdict token — used to find
/// where the verdict line ends and the remaining-work detail begins.
fn line_bears_verdict(line_upper: &str) -> bool {
    NEGATED_POSITIVE.is_match(line_upper)
        || NEGATIVE_TOKEN.is_match(line_upper)
        || ABSTAIN_TOKEN.is_match(line_upper)
        || POSITIVE_TOKEN.is_match(line_upper)
}

/// Classify the verdict in a judge response. Finds the first line that bears a
/// whole-word verdict token (the verdict line — scanning past any prose
/// preamble) and classifies THAT line alone, honouring an explicit `NOT`
/// negation of a positive word. Precedence is Negative > Abstain > Positive:
/// when the verdict line mixes tokens the safer, action-forcing signal wins.
/// `None` (no verdict token on any line) is the deliberate fail-open input —
/// the caller maps it to its own pass variant.
///
/// Only the verdict line is classified — not the whole response — so common
/// English in the remaining-work detail (e.g. "a few short comments, no gaps in
/// logic") can't compete with the verdict above it.
pub(crate) fn classify_verdict_head(trimmed: &str) -> VerdictSignal {
    for line in trimmed.split('\n') {
        let upper = line.to_ascii_uppercase();
        if !line_bears_verdict(&upper) {
            continue;
        }
        if NEGATED_POSITIVE.is_match(&upper) || NEGATIVE_TOKEN.is_match(&upper) {
            return VerdictSignal::Negative;
        }
        if ABSTAIN_TOKEN.is_match(&upper) {
            return VerdictSignal::Abstain;
        }
        return VerdictSignal::Positive;
    }
    VerdictSignal::None
}

/// Non-empty remaining-work detail AFTER the first line that bears a verdict
/// token, or `None` when the verdict is its own last line (no separate detail).
/// Splitting on `\n` (not `.lines()`) keeps the byte offsets exact under both
/// `\n` and `\r\n` endings. The caller picks the fallback for the no-detail
/// case — the critic reuses the whole response, the goal substitutes a note.
pub(crate) fn detail_after_verdict(trimmed: &str) -> Option<&str> {
    let mut after_verdict_byte = None;
    let mut cursor = 0usize;
    for line in trimmed.split('\n') {
        if after_verdict_byte.is_none() && line_bears_verdict(&line.to_ascii_uppercase()) {
            // Detail begins right after this line's content (before its `\n`).
            after_verdict_byte = Some(cursor + line.len());
            break;
        }
        cursor += line.len() + 1; // +1 for the '\n' separator.
    }
    let off = after_verdict_byte?;
    let rest = trimmed[off..].trim();
    (!rest.is_empty()).then_some(rest)
}

/// Parse the critic's raw response into a verdict. `Verdict::Complete` means
/// the work is done — or the response carried NO verdict token anywhere, in
/// which case we fail OPEN (don't block finalization on a confused critic).
/// `Verdict::Incomplete(issues)` means concrete gaps to fix.
/// `Verdict::Abstain(missing)` means the critic cannot verify from available
/// spec/evidence — the model should write a held-out test or clarify the spec.
///
/// Classification is whole-word with explicit-negation handling
/// ([`classify_verdict_head`]); precedence is Negative > Abstain > Positive, so
/// a line mixing INCOMPLETE and ABSTAIN resolves to Incomplete. The detail is
/// [`detail_after_verdict`], falling back to the whole response when the bare
/// verdict has nothing after it.
pub fn parse_verdict(response: &str) -> Verdict {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Verdict::Complete;
    }
    match classify_verdict_head(trimmed) {
        VerdictSignal::Negative => {
            Verdict::Incomplete(detail_after_verdict(trimmed).unwrap_or(trimmed).to_string())
        }
        VerdictSignal::Abstain => {
            Verdict::Abstain(detail_after_verdict(trimmed).unwrap_or(trimmed).to_string())
        }
        VerdictSignal::Positive | VerdictSignal::None => Verdict::Complete,
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
    evidence: Option<&Evidence>,
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
         --- transcript ---\n{transcript}\n--- end transcript ---{diff_block}{prior_findings_block}{}{}",
        verification_block(verification),
        evidence_block(evidence)
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
/// Outcome of one unified review.
///
/// `judged` distinguishes "the judge ran and found nothing" from "the judge
/// call failed and we failed open" — which the old `(Vec, Option<String>)`
/// return could not (dirge-q7vw). Both produced an empty vec and a `None`, so
/// the Blocking caller recorded the reviewed-diff fingerprint after a
/// transient judge error and then skipped re-review of that same diff forever:
/// the diff never got code-reviewed at all, silently.
pub struct ReviewOutcome {
    pub messages: Vec<LoopMessage>,
    pub raised_findings: Option<String>,
    /// True only when a judge response was actually parsed.
    pub judged: bool,
}

pub async fn run_unified_review(
    judge: &CriticFn,
    rules: &str,
    transcript: &str,
    diff: Option<&str>,
    verification: Option<VerificationStatus>,
    prior_findings: Option<&str>,
    evidence: Option<&Evidence>,
) -> ReviewOutcome {
    let prompt = build_unified_prompt(
        rules,
        transcript,
        diff,
        verification,
        prior_findings,
        evidence,
    );
    let response = run_judge!(
        judge,
        prompt,
        "dirge::critic",
        "unified review call failed; finalizing without it",
        ReviewOutcome {
            messages: Vec::new(),
            raised_findings: None,
            judged: false,
        }
    );
    let (verdict, findings) = parse_unified(&response);
    // dirge-9b2k R2: surface the rendered findings so the caller can hand them
    // to the next reaction's judge prompt (Blocking path in run.rs).
    let raised = if findings.is_empty() {
        None
    } else {
        Some(render_findings(&findings))
    };
    let messages = build_unified_followup(verdict, findings)
        .into_iter()
        .collect();
    ReviewOutcome {
        messages,
        raised_findings: raised,
        judged: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dirge-d0e5.3 test 8: the evidence block must reach the critic prompt
    /// with REAL values — mutated file names, observed verification commands
    /// with outcomes, and the tool-call count — so the judge can check the
    /// assistant's factual claims against what actually happened.
    #[test]
    fn evidence_block_reaches_prompt_with_real_values() {
        let evidence = Evidence {
            files_mutated: vec!["src/agent/loop.rs".to_string(), "Cargo.toml".to_string()],
            observed_commands: vec![
                ("cargo test".to_string(), false),
                ("cargo clippy".to_string(), true),
            ],
            tool_calls: 9,
            tool_names: vec![
                "edit".to_string(),
                "bash".to_string(),
                "edit".to_string(), // duplicates must collapse
                "webfetch".to_string(),
            ],
        };
        let p = build_unified_prompt("", "t", None, None, None, Some(&evidence));
        assert!(
            p.contains("src/agent/loop.rs"),
            "mutated file must be named"
        );
        assert!(p.contains("Cargo.toml"), "mutated file must be named");
        assert!(p.contains("cargo test"), "observed command must be named");
        assert!(p.contains("FAILED"), "a failed command must be marked");
        assert!(p.contains("passed"), "a passing command must be marked");
        assert!(
            p.contains("tool calls: 9"),
            "tool-call count must be present"
        );
        assert!(
            p.contains("tools invoked: bash, edit, webfetch"),
            "tool names must render sorted and deduped — duplicates collapse"
        );
        assert!(
            p.contains("UNSUPPORTED"),
            "the block must say what an absent fact means"
        );
        // No evidence → no block; empty-but-present evidence renders the
        // honest `(none)` markers rather than a gap.
        assert_eq!(evidence_block(None), "");
        let empty = build_unified_prompt("", "t", None, None, None, Some(&Evidence::default()));
        assert!(
            empty.contains("(none)"),
            "empty evidence must be marked, not elided"
        );
        assert!(
            empty.contains("tools invoked: (none)"),
            "absent tools must render as (none) — absence is informative for sourcing claims"
        );
    }

    /// dirge-d0e5.3 test 9: the system preamble must ask the critic to check
    /// the assistant's claims against the evidence block. Style mirrors
    /// `preamble_is_calibrated_and_constraint_aware`.
    #[test]
    fn preamble_asks_for_claim_check_against_evidence() {
        let lower = CRITIC_PREAMBLE.to_ascii_lowercase();
        assert!(
            lower.contains("evidence"),
            "preamble must point the critic at the evidence block"
        );
        assert!(
            lower.contains("unsupported") || lower.contains("does not show"),
            "preamble must tell the critic to flag unsupported claims"
        );
        assert!(
            lower.contains("source") && lower.contains("fetch/search"),
            "preamble must make consulting a source without a fetch/search tool unsupported"
        );
    }

    /// dirge-uw2l.2: the fast-green-only block must name the gap (the full
    /// suite never ran) while keeping the same calibrated escape hatch as
    /// the `Unverified` block — it is a nudge, not a hard rule, so a project
    /// with no broader suite is never blocked on running one.
    #[test]
    fn critic_verification_block_fast_green_only() {
        let block = verification_block(Some(VerificationStatus::FastGreenOnly));
        assert!(!block.is_empty());
        assert!(block.contains("full test suite"), "{block}");
        assert!(block.contains("NUDGE, not a hard rule"), "{block}");
        assert!(
            block.contains("COMPLETE"),
            "escape hatch preserved: {block}"
        );

        // Unchanged: no code edited / no gate configured stay silent.
        assert_eq!(
            verification_block(Some(VerificationStatus::NoCodeEdited)),
            ""
        );
        assert_eq!(verification_block(None), "");
    }

    /// Test shim (dirge-8v98): the old `build_prompt` was folded into
    /// `build_unified_prompt` with a `None` diff (completeness-only). The prompt
    /// tests below still exercise the shared rules/compaction/verification/format
    /// behavior through it.
    fn build_prompt(
        rules: &str,
        transcript: &str,
        verification: Option<VerificationStatus>,
    ) -> String {
        build_unified_prompt(rules, transcript, None, verification, None, None)
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
    fn parse_verdict_corpus() {
        // dirge-5mtx: a judge's verdict surface form competes with itself.
        // `COMPLETE` is a substring of `INCOMPLETE`; a bare positive word is
        // embedded in its own negation (`NOT COMPLETE`). The parser must
        // classify whole words, honour an explicit `NOT`, scan past a prose
        // preamble to the verdict line, and only fail open when NO verdict
        // token is present anywhere in the head. Most rows reproduced a bug on
        // the old first-line / substring parser.
        // (input, expected verdict class)
        let rows: &[(&str, &str)] = &[
            // bare tokens
            ("COMPLETE", "complete"),
            ("DONE", "complete"),
            ("INCOMPLETE", "incomplete"),
            ("GAPS", "incomplete"),
            ("SHORT", "incomplete"),
            // VERDICT: prefix
            ("VERDICT: COMPLETE", "complete"),
            ("VERDICT: INCOMPLETE", "incomplete"),
            ("VERDICT: DONE", "complete"),
            ("VERDICT: GAPS", "incomplete"),
            ("VERDICT: UNSURE", "abstain"),
            ("VERDICT: ABSTAIN", "abstain"),
            // negated forms — `COMPLETE`/`DONE` appear as substrings but mean
            // NOT done. The INCOMPLETE/COMPLETE trap, negated order.
            ("NOT COMPLETE", "incomplete"),
            ("NOT DONE", "incomplete"),
            ("NOT FINISHED", "incomplete"),
            ("NOT SATISFIED", "incomplete"),
            // preamble line before the verdict token
            (
                "I reviewed the work.\nINCOMPLETE\n- tests still failing",
                "incomplete",
            ),
            ("After checking, VERDICT: DONE", "complete"),
            // mixed case
            ("verdict: incomplete\n- x", "incomplete"),
            ("Verdict: UNSURE", "abstain"),
            // tokens mid-sentence (first-line substring handling, both ways)
            ("The task is NOT COMPLETE yet", "incomplete"),
            ("Overall the work is COMPLETE", "complete"),
            // the INCOMPLETE/COMPLETE substring trap in both orders — negative
            // wins whenever both tokens are present on the verdict line.
            ("COMPLETE, not INCOMPLETE", "incomplete"),
            ("INCOMPLETE then COMPLETE", "incomplete"),
            // common English in the DETAIL must NOT compete with the verdict
            // line above it — only the verdict line is classified.
            (
                "DONE\nThe diff is clean; a few short comments, no gaps in logic.",
                "complete",
            ),
            (
                "COMPLETE\naddressing the remaining gaps next sprint is out of scope.",
                "complete",
            ),
            // no verdict token anywhere in the head → fail open (deliberate)
            ("", "complete"),
            ("probably looks fine", "complete"),
        ];
        for &(input, want) in rows {
            let got = match parse_verdict(input) {
                Verdict::Complete => "complete",
                Verdict::Incomplete(_) => "incomplete",
                Verdict::Abstain(_) => "abstain",
            };
            assert_eq!(got, want, "parse_verdict({input:?})");
        }

        // The preamble row must carry the remaining-work detail through.
        match parse_verdict("I reviewed the work.\nINCOMPLETE\n- tests still failing") {
            Verdict::Incomplete(issues) => assert!(
                issues.contains("tests still failing"),
                "detail lost: {issues:?}"
            ),
            other => panic!("expected Incomplete with detail, got {other:?}"),
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

    // dirge-g2ex: an assistant that stopped to ask the user a question is at a
    // CORRECT stopping point — the preamble must tell the judge never to treat
    // that as incompleteness or push it to guess/proceed.

    /// The preamble carries the awaiting-question carve-out: it names the
    /// scenario (asking the user / awaiting a decision), marks it a correct
    /// stop, and forbids proceeding/guessing/picking a default.
    #[test]
    fn preamble_treats_awaiting_user_question_as_correct_stop() {
        let lower = CRITIC_PREAMBLE.to_ascii_lowercase();
        assert!(
            lower.contains("asking the user a question") || lower.contains("presenting options"),
            "must name the awaiting-question / options scenario"
        );
        assert!(
            lower.contains("correct stopping point"),
            "must mark a pending question a correct stop"
        );
        assert!(
            lower.contains("guess"),
            "must forbid pushing the assistant to guess"
        );
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
            run_unified_review(&judge, "rules", "did stuff", None, None, None, None)
                .await
                .messages
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
            None,
        );
        assert!(with.contains("diff under review"));
        assert!(with.contains("+b"));
        let without = build_unified_prompt("rules", "did stuff", None, None, None, None);
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
            None,
        );
        assert!(!p.contains("earlier review"));
        assert!(p.contains("diff under review"));
    }

    #[test]
    fn unified_prompt_omits_prior_findings_section_when_blank() {
        // A whitespace-only string carries no real findings — still no section.
        let p = build_unified_prompt(
            "rules",
            "did stuff",
            Some("diff"),
            None,
            Some("  \n "),
            None,
        );
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
            None,
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
            run_unified_review(&judge, "rules", "did stuff", Some("diff"), None, None, None)
                .await
                .messages
                .is_empty(),
            "a judge error must not block finalization"
        );
    }

    /// Mechanical guard on the hazard itself, rather than on a fixed list of
    /// phrasings: for EVERY positive token and EVERY negative token that
    /// embeds it, a line bearing the negative must classify negative.
    ///
    /// The emitted answer set is deliberately left as COMPLETE/INCOMPLETE/
    /// ABSTAIN rather than switched to non-overlapping words. Changing what
    /// the prompt asks the judge for is a behavioural change, and run-to-run
    /// variance makes behavioural changes to the judge unverifiable at any
    /// sample size we can afford (dirge-5mtx.6, FM-5). The parser handling
    /// both vocabularies is the structural fix, and it is checkable at n=1 —
    /// which is what this test does. If someone later does change the
    /// prompt, this still holds.
    #[test]
    fn negative_always_beats_the_positive_it_embeds() {
        // (negative form, embedded positive token, is that token also a
        // standalone positive?). FINISHED is deliberately negation-only: it
        // is recognised in `NOT FINISHED` but bare `FINISHED` is not a
        // verdict token, so it falls through to fail-open. Harmless, because
        // fail-open and Positive both resolve to the same verdict — but the
        // asymmetry is real and is pinned here rather than left to surprise
        // someone.
        let traps: &[(&str, &str, bool)] = &[
            ("INCOMPLETE", "COMPLETE", true),
            ("NOT COMPLETE", "COMPLETE", true),
            ("NOT DONE", "DONE", true),
            ("NOT MET", "MET", true),
            ("NOT SATISFIED", "SATISFIED", true),
            ("NOT FINISHED", "FINISHED", false),
        ];
        for &(negative, positive, positive_is_standalone) in traps {
            assert!(
                negative.contains(positive),
                "test data error: {negative} should embed {positive}"
            );
            assert_eq!(
                classify_verdict_head(negative),
                VerdictSignal::Negative,
                "{negative} embeds {positive} and must still classify negative"
            );
            // The fix must not have been achieved by simply never returning
            // Positive — bare positives still read positive where they are
            // part of the vocabulary at all.
            if positive_is_standalone {
                assert_eq!(
                    classify_verdict_head(positive),
                    VerdictSignal::Positive,
                    "bare {positive} must still classify positive"
                );
            } else {
                assert_eq!(
                    classify_verdict_head(positive),
                    VerdictSignal::None,
                    "{positive} is negation-only; bare use falls through to fail-open"
                );
            }
        }
    }

    // ── dirge-5mtx.3: ClassifyFn answer-set matching ──────────────────────

    /// Exactly one option present → its index, for EACH position in the set.
    #[test]
    fn parse_choice_single_match_each_index() {
        let opts = ["BLOCKED", "NEXT", "DONE"];
        assert_eq!(parse_choice("BLOCKED", &opts), Some(0));
        assert_eq!(
            parse_choice("the run is BLOCKED on the user", &opts),
            Some(0)
        );
        assert_eq!(parse_choice("NEXT", &opts), Some(1));
        assert_eq!(parse_choice("offering a NEXT step", &opts), Some(1));
        assert_eq!(parse_choice("DONE", &opts), Some(2));
    }

    /// Case-insensitive and whole-word: `MET` must NOT match inside `UNMET`.
    /// This is the exact hazard that produced dirge-5mtx.3 — the overlap is
    /// why a non-nesting answer set (`MET / SHORT`) is required in practice.
    #[test]
    fn parse_choice_case_insensitive_and_whole_word() {
        let opts = ["MET", "SHORT"];
        assert_eq!(parse_choice("met", &opts), Some(0));
        assert_eq!(parse_choice("Met.", &opts), Some(0));
        assert_eq!(parse_choice("goal is met", &opts), Some(0));
        assert_eq!(parse_choice("SHORT", &opts), Some(1));
        assert_eq!(parse_choice("short by a lot", &opts), Some(1));
        assert_eq!(parse_choice("the goal is not satisfied", &opts), None);
        // Demonstrate the trap the precondition guards against: with an
        // overlapping set, `MET` would match inside `UNMET`. We assert the
        // debug-check catches it rather than asserting parse_choice behaviour
        // on an invalid set, so the test documents the precondition.
        let bad = ["MET", "UNMET"];
        assert!(options_overlap(&bad));
    }

    /// Two DIFFERENT options present → `None` (ambiguous, not first-wins).
    #[test]
    fn parse_choice_two_distinct_options_is_ambiguous() {
        let opts = ["BLOCKED", "NEXT", "DONE"];
        assert_eq!(parse_choice("BLOCKED and then DONE", &opts), None);
        assert_eq!(parse_choice("NEXT not DONE", &opts), None);
    }

    /// No option present → `None`.
    #[test]
    fn parse_choice_no_match() {
        let opts = ["BLOCKED", "NEXT"];
        assert_eq!(parse_choice("", &opts), None);
        assert_eq!(parse_choice("the assistant is waiting", &opts), None);
        assert_eq!(parse_choice("COMPLETELY unrelated prose", &opts), None);
    }

    /// An option appearing twice → still that index (repetition is not
    /// ambiguity).
    #[test]
    fn parse_choice_repeated_option_not_ambiguous() {
        let opts = ["BLOCKED", "NEXT"];
        assert_eq!(parse_choice("BLOCKED, yes BLOCKED", &opts), Some(0));
        assert_eq!(parse_choice("NEXT NEXT NEXT", &opts), Some(1));
    }

    /// The prompt builder names EVERY option, so the judge sees the full set.
    #[test]
    fn classify_prompt_names_every_option() {
        let prompt = classify_prompt("Is the agent blocked?", &["BLOCKED", "NEXT"]);
        assert!(prompt.contains("BLOCKED"));
        assert!(prompt.contains("NEXT"));
        assert!(prompt.contains("Is the agent blocked?"));
        assert!(prompt.contains("EXACTLY ONE"));
    }

    /// The retry prompt also names every option (the last chance to steer the
    /// model back to the set before the callback errors).
    #[test]
    fn classify_retry_prompt_names_every_option() {
        let prompt = classify_retry_prompt(&["BLOCKED", "NEXT"]);
        assert!(prompt.contains("BLOCKED"));
        assert!(prompt.contains("NEXT"));
    }

    // ── dirge-q7vw: a failed judge call must be distinguishable from a
    // clean review. Both produce no messages and no findings, and the
    // Blocking caller used that to decide whether to record the
    // reviewed-diff fingerprint. Recording it after an ERROR made the next
    // reaction skip the same unchanged diff, so it never got reviewed.

    #[tokio::test]
    async fn judge_error_is_not_reported_as_judged() {
        let judge: CriticFn = Arc::new(|_p| Box::pin(async { anyhow::bail!("provider down") }));
        let out = run_unified_review(&judge, "", "did stuff", Some("diff"), None, None, None).await;
        assert!(!out.judged, "a failed call must not claim to have judged");
        assert!(
            out.messages.is_empty(),
            "fail-open still yields no follow-up"
        );
        assert!(out.raised_findings.is_none());
    }

    #[tokio::test]
    async fn clean_review_is_reported_as_judged() {
        let judge: CriticFn =
            Arc::new(|_p| Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) }));
        let out = run_unified_review(&judge, "", "did stuff", Some("diff"), None, None, None).await;
        assert!(out.judged, "a real response must count as judged");
        assert!(
            out.messages.is_empty(),
            "COMPLETE with no findings still yields no follow-up"
        );
    }

    /// The pair is the actual invariant: the two cases are indistinguishable
    /// by messages/findings alone, which is exactly why `judged` exists.
    #[tokio::test]
    async fn error_and_clean_review_differ_only_in_judged() {
        let err: CriticFn = Arc::new(|_p| Box::pin(async { anyhow::bail!("down") }));
        let ok: CriticFn = Arc::new(|_p| Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) }));
        let a = run_unified_review(&err, "", "t", Some("d"), None, None, None).await;
        let b = run_unified_review(&ok, "", "t", Some("d"), None, None, None).await;
        assert_eq!(a.messages.len(), b.messages.len());
        assert_eq!(a.raised_findings, b.raised_findings);
        assert_ne!(a.judged, b.judged, "only `judged` separates them");
    }
}

//! Diff-aware code reviewer (dirge-iyf5).
//!
//! A sibling to the completeness [`critic`](super::critic): where the
//! critic judges "is the task done?" from the transcript, this reviewer
//! judges "is the changed CODE correct?" from the actual diff, and emits
//! structured, severity-ranked [`Finding`]s. It reuses the critic's judge
//! plumbing ([`CriticFn`](super::critic::CriticFn) + the shared
//! `critic_provider` client) — no new provider config — so it's the same
//! opt-in with a different preamble and a findings pipeline.
//!
//! The prompt craft and the verdict/finding model are ported from roborev
//! (`internal/prompt/templates/default_review.md.gotmpl`,
//! `default_security.md.gotmpl`, and `internal/storage/verdict.go`); the
//! daemon/queue/sqlite infrastructure around them is not relevant to an
//! in-loop reviewer and is deliberately left behind.
//!
//! This module (R1) is the PURE core: the preambles, the [`Severity`] /
//! [`Finding`] types, and the parser. The finalization wiring, the diff
//! capture, the two-pass verify, and the severity gate land in later
//! rounds — until then several items here have no non-test caller.
#![allow(dead_code)]

/// Finding severity, ported from roborev's four-level model. Declared in
/// ASCENDING order so the derived [`Ord`] makes `Critical` the greatest —
/// `findings.sort_by(|a, b| b.severity.cmp(&a.severity))` yields
/// highest-first. The gate (R5) blocks on `High`/`Critical` and treats
/// `Medium`/`Low` as advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// The lowercase label used in review output and prompts.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// Parse a leading severity word (case-insensitive). Returns the
    /// severity when `word` begins with one of the four level names.
    fn from_prefix(word: &str) -> Option<Severity> {
        // Order doesn't matter — the four prefixes don't overlap.
        const LEVELS: [(&str, Severity); 4] = [
            ("critical", Severity::Critical),
            ("high", Severity::High),
            ("medium", Severity::Medium),
            ("low", Severity::Low),
        ];
        let lower = word.trim().to_ascii_lowercase();
        LEVELS
            .iter()
            .find(|(name, _)| lower.starts_with(name))
            .map(|(_, sev)| *sev)
    }
}

/// One review finding: a severity plus the finding's text block and, when
/// the model provided one, a narrowest-location hint. `body` is the raw
/// block (minus the `---` delimiters) so the surfacing/feedback code can
/// show the model's own wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Best-effort file/line reference lifted from the block (a
    /// `Location:`/`File:` field or a `path:line` token). `None` when the
    /// model gave no locatable reference.
    pub location: Option<String>,
    /// The finding block verbatim (trimmed).
    pub body: String,
}

/// Tag prefixed onto the reviewer's injected follow-up message. Distinct
/// from `[critic]` / `[verify-before-done]` so the UI can attribute and
/// color it independently. The agent loop re-enters it as a user-role
/// message (so the model acts on it).
pub const CODE_REVIEW_TAG: &str = "[code-review]";

/// System preamble for the general code review pass. Establishes the
/// reviewer's role, what to check, and — critically — the evidence
/// discipline and don't-report list that keep it from generating noise.
/// Ported from roborev's `default_review.md.gotmpl`, adapted from
/// "review this commit" to "review the diff this run produced" (dirge's
/// reviewer runs in-loop, not per-commit) and given the critic's
/// constraint-awareness so it never demands a forbidden action. The
/// output FORMAT lives in [`REVIEW_FORMAT`], carried beside the diff.
pub const REVIEW_PREAMBLE: &str = "\
You are a code reviewer for an autonomous coding agent. You are given a unified diff of the code \
changes the assistant just made, the user's request, and a transcript of what the assistant did. \
Review the DIFF for defects.\n\
\n\
Read the request and transcript to understand intent, then check whether the diff correctly and \
completely achieves it — gaps between stated intent and actual implementation are high-value \
findings. If intent is vague, infer it from the diff itself and skip the intent-alignment check.\n\
\n\
Check for:\n\
1. Intent-implementation gaps: does the diff actually accomplish what was asked?\n\
2. Bugs: logic errors, off-by-one errors, null/None issues, race conditions.\n\
3. Security: injection, auth issues, data exposure.\n\
4. Testing gaps: missing unit tests, edge cases not covered.\n\
5. Regressions: changes that might break existing functionality.\n\
6. Code quality: duplication that should be refactored, overly complex logic, unclear naming.\n\
\n\
Do not report issues without specific evidence in the diff. In particular, do NOT report:\n\
- Hypothetical issues in code not shown in the diff.\n\
- Style preferences or naming opinions that do not affect correctness.\n\
- \"Missing tests\" unless the change introduces testable behavior with no coverage.\n\
- Patterns that are consistent with the codebase conventions visible in context.\n\
- The absence of an action the assistant was explicitly told not to take (commit, push, deploy, \
etc.). Treat anything out of scope as correctly omitted — never demand it.\n\
\n\
Judge whether a feature or API exists from the project's toolchain and dependency manifests \
(Cargo.toml, package.json, go.mod, pyproject.toml, …), not your own memory, which may be stale. \
Do not flag valid recent APIs as broken, and do not miss calls to APIs that genuinely do not \
exist for the project's versions.";

/// System preamble for the security-focused pass. Ported from roborev's
/// `default_security.md.gotmpl` — the "exploitability burden of proof"
/// stance plus its long don't-report list, which is what keeps a security
/// pass from drowning the user in defense-in-depth noise. Used when the
/// reviewer runs in security stance (R3+); the format still comes from
/// [`REVIEW_FORMAT`].
pub const SECURITY_PREAMBLE: &str = "\
You are a security code reviewer with an exploitability burden of proof. Review the diff for \
concrete vulnerabilities, material weakening of security controls, and newly reachable attack \
surface.\n\
\n\
Report an issue only when the changed code affects a real trust boundary, security decision, \
secret-bearing path, privileged operation, or externally/user-controlled input path. The finding \
does not need a turnkey exploit, but it must identify a realistic attacker capability, the \
weakened boundary or control, and the concrete asset or privilege at risk.\n\
\n\
Prefer NO finding over generic hardening advice, local-only paranoia, or best-practice \
commentary without a changed security outcome. Focus on injection, broken auth/authorization, \
credential exposure, path traversal, unsafe deserialization/patterns, dependency risks the diff \
introduces, CI/CD workflow injection, sensitive-data handling, security-relevant races/TOCTOU, \
and information leakage — but do not produce one finding per category.\n\
\n\
Only report vulnerabilities with a plausible exploit path visible in the diff. Do NOT report:\n\
- Theoretical vulnerabilities in code not touched by this change.\n\
- Generic hardening unrelated to the specific code under review.\n\
- Missing validation unless the value is attacker-controlled and reaches a security-sensitive \
sink.\n\
- Missing encryption/hashing/signing/rate-limiting/audit-logging unless the reviewed code \
handles sensitive assets and the absence creates a concrete abuse path.\n\
- Process environment variables being readable by local same-user code, child processes, or \
same-user tooling. Local same-user access is not an attacker boundary by itself — a finding must \
involve a weaker actor gaining access they did not already have.\n\
\n\
Before reporting, verify: who is the attacker or less-privileged actor? What do they control? \
What boundary or control changed? What can they now access, modify, trigger, or bypass that they \
could not before? Is this risk introduced or materially worsened by the diff? Drop the finding if \
those answers are vague or rely only on \"this could be more secure.\"";

/// Response-format instruction, carried in the user prompt beside the diff
/// (mirrors the critic's split: role in the preamble, format next to the
/// material). Ported from the tail of roborev's review template. The
/// `---`-on-its-own-line separator and the four severity definitions are
/// load-bearing: [`parse_findings`] keys on both.
pub const REVIEW_FORMAT: &str = "\
Respond with a brief one-line summary of what the diff does, then any issues found. For each \
finding, on its own bullet, lead with the severity word, then the details:\n\
- Severity, using these definitions:\n\
  - critical: actively exploitable — remote code execution, auth bypass, or data exfiltration.\n\
  - high: will cause data loss, security breach, crash, or incorrect results in production.\n\
  - medium: degraded behavior under specific conditions, or blocks future maintainability.\n\
  - low: minor improvement with no immediate functional impact.\n\
- File and line reference where possible (the narrowest applicable location).\n\
- What specifically goes wrong if this is not fixed (concrete harm, not \"violates best \
practices\").\n\
- A suggested fix.\n\
Separate multiple findings with `---` on its own line.\n\
\n\
Before finalizing, verify: every finding references the narrowest applicable location, the \
severity matches the impact you described, and no two findings contradict each other. Drop any \
finding that fails these checks.\n\
\n\
If you find no issues, state \"No issues found.\" on its own line after the summary.";

// ── Parser (ported from roborev internal/storage/verdict.go) ──────────

/// Parse review output into structured findings. Splits on `---`
/// delimiter lines (the format's finding separator) and, for each block,
/// extracts the severity via the same line-scan roborev's `hasSeverityLabel`
/// uses — a block with no severity label is narration/summary and yields no
/// finding. Returns findings in document order; callers sort by severity.
///
/// Divergence from roborev's `ParseVerdict`, which defaults ambiguous
/// prose to FAIL: here "no severity-labeled block" means "no finding", so
/// vague narration never fabricates a finding. That default is right for
/// this context — a finding can BLOCK the loop (R5), whereas roborev's
/// fail only posts a PR comment — and the prompt's format contract makes
/// real findings severity-labeled. [`verdict_is_pass`] keeps the faithful
/// boolean port for the pass-2 verify step, where a clean/dirty verdict is
/// the right shape.
pub fn parse_findings(output: &str) -> Vec<Finding> {
    split_finding_blocks(output)
        .into_iter()
        .filter_map(|block| {
            detect_block_severity(&block).map(|severity| Finding {
                severity,
                location: extract_location(&block),
                body: block.trim().to_string(),
            })
        })
        .collect()
}

/// Split output into candidate finding blocks on lines that are exactly
/// `---` (after trimming). A single-block output (no separators) comes
/// back as one element.
fn split_finding_blocks(output: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    for line in output.lines() {
        if line.trim() == "---" {
            blocks.push(std::mem::take(&mut current));
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    blocks.push(current);
    blocks
        .into_iter()
        .filter(|b| !b.trim().is_empty())
        .collect()
}

/// Detect the severity label for a single finding block. Mirrors roborev's
/// `hasSeverityLabel` line scan (bullet/number strip, markdown strip,
/// severity-word-then-separator, and the `Severity: <level>` field form)
/// but returns the matched [`Severity`] instead of a bool, and skips lines
/// that look like a severity legend/rubric entry.
fn detect_block_severity(block: &str) -> Option<Severity> {
    let lower = block.to_ascii_lowercase();
    let lines: Vec<&str> = lower.lines().collect();

    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Strip a leading bullet/number marker, then markdown.
        let first = trimmed.as_bytes()[0];
        let has_bullet = first == b'-'
            || first == b'*'
            || first.is_ascii_digit()
            || trimmed.starts_with('\u{2022}'); // •
        let mut check = if has_bullet {
            trimmed
                .trim_start_matches(['-', '*', '\u{2022}', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', ')', ' '])
                .to_string()
        } else {
            trimmed.to_string()
        };
        check = strip_markdown(&check);

        // Branch 1: the text starts with a severity word + separator.
        if let Some(sev) = severity_word_with_separator(&check)
            && !is_legend_entry(&lines, i)
        {
            return Some(sev);
        }

        // Branch 2: a "severity: <level>" field (e.g. "**Severity**: High").
        if let Some(rest) = check.strip_prefix("severity") {
            let rest = rest.trim_start();
            let has_sep = rest.starts_with([':', '|', '—', '–'])
                || rest.starts_with("- ");
            if has_sep {
                let level = rest.trim_start_matches([':', '-', '–', '—', '|', ' ']);
                if let Some(sev) = Severity::from_prefix(level)
                    && !is_legend_entry(&lines, i)
                {
                    return Some(sev);
                }
            }
        }
    }
    None
}

/// If `check` (already lowercased, bullet/markdown-stripped) starts with a
/// severity word directly followed by a valid separator (em/en dash,
/// colon, pipe, or `- ` with a space), return that severity. The
/// space-after-hyphen rule avoids matching "high-level overview".
fn severity_word_with_separator(check: &str) -> Option<Severity> {
    for (name, sev) in [
        ("critical", Severity::Critical),
        ("high", Severity::High),
        ("medium", Severity::Medium),
        ("low", Severity::Low),
    ] {
        let Some(rest) = check.strip_prefix(name) else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }
        let valid_sep = rest.starts_with('—')
            || rest.starts_with('–')
            || rest.starts_with(':')
            || rest.starts_with('|')
            || rest.starts_with("- ");
        if valid_sep {
            return Some(sev);
        }
    }
    None
}

/// True when the line at `i` looks like a severity legend/rubric entry
/// rather than a real finding — the nearest preceding non-empty line (up
/// to 10 back) is a header ending in `:` that names a legend/scale/rubric.
/// Ported from roborev's `isLegendEntry`. `lines` are already lowercased.
fn is_legend_entry(lines: &[&str], i: usize) -> bool {
    let start = i.saturating_sub(10);
    for j in (start..i).rev() {
        let prev = lines[j].trim();
        if prev.is_empty() {
            continue;
        }
        let prev = strip_markdown(&strip_list_marker(prev));
        if prev.ends_with(':') || prev.ends_with('：') {
            const INDICATORS: [&str; 7] =
                ["severity", "level", "legend", "priority", "rubric", "rating", "scale"];
            if INDICATORS.iter().any(|w| prev.contains(w)) {
                return true;
            }
        }
        // Keep scanning back (roborev's isLegendEntry): severity lines and
        // description lines can sit between a legend header and this entry.
    }
    false
}

/// Best-effort location hint from a finding block: a `Location:`/`File:`
/// field value, else the first `path:line`-looking token. `None` when
/// nothing locatable is present.
fn extract_location(block: &str) -> Option<String> {
    for raw in block.lines() {
        let line = strip_markdown(&strip_list_marker(raw.trim()));
        let lower = line.to_ascii_lowercase();
        for label in ["location:", "file:"] {
            if lower.starts_with(label) {
                let val = line[label.len()..].trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

// ── Faithful ParseVerdict boolean port (for the pass-2 verify step) ───

/// Faithful port of roborev's `ParseVerdict`, returning `true` for a clean
/// (pass) verdict. Used by the two-pass verify step (R4), where the model
/// re-checks findings and may report "all findings addressed" / "no
/// verified findings remain" — a clean/dirty boolean is the right shape
/// there. Deterministic: a severity label means dirty; a clear pass phrase
/// means clean; otherwise dirty.
pub fn verdict_is_pass(output: &str) -> bool {
    // A severity label anywhere means there are real findings.
    if split_finding_blocks(output)
        .iter()
        .any(|b| detect_block_severity(b).is_some())
    {
        return false;
    }
    for line in output.lines() {
        let normalized = normalize_verdict_line(line);
        if normalized == "pass" || is_no_finding_line(&normalized) || has_pass_prefix(&normalized) {
            return true;
        }
    }
    false
}

fn normalize_verdict_line(line: &str) -> String {
    let lowered = line
        .trim()
        .to_ascii_lowercase()
        .replace(['\u{2018}', '\u{2019}'], "'");
    let stripped = strip_markdown(&lowered);
    let stripped = strip_list_marker(&stripped);
    strip_field_label(&stripped)
}

fn has_pass_prefix(line: &str) -> bool {
    const PREFIXES: [&str; 5] = [
        "no issues",
        "no findings",
        "i didn't find any issues",
        "i did not find any issues",
        "i found no issues",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

fn is_no_finding_line(line: &str) -> bool {
    let line = line.trim_end_matches(['.', '!', '?']);
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        line.as_str(),
        "all previous findings have been addressed"
            | "all findings have been resolved"
            | "no verified findings remain"
            | "no findings remain"
            | "no remaining findings"
            | "0 findings"
            | "0 findings remain"
            | "0 verified findings"
            | "0 verified findings remain"
            | "zero findings"
            | "zero findings remain"
            | "zero verified findings"
            | "zero verified findings remain"
    )
}

/// Strip leading markdown headers and bold/italic markers. Ported from
/// roborev's `stripMarkdown`.
fn strip_markdown(s: &str) -> String {
    let mut s = s.trim_start_matches('#').trim().to_string();
    s = s.replace("**", "").replace("__", "");
    s.trim().to_string()
}

/// Strip a single leading bullet or numbered-list marker. Ported from
/// roborev's `stripListMarker`.
fn strip_list_marker(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return s.to_string();
    }
    if bytes[0] == b'-' || bytes[0] == b'*' {
        return s[1..].trim().to_string();
    }
    // Numbered list: leading digits then a `.`/`)`/`:` terminator.
    for (i, b) in bytes.iter().enumerate() {
        if b.is_ascii_digit() {
            continue;
        }
        if i > 0 && (*b == b'.' || *b == b')' || *b == b':') {
            return s[i + 1..].trim().to_string();
        }
        break;
    }
    s.to_string()
}

/// Strip a known leading field label ("Findings:", "Verdict:", …). Ported
/// from roborev's `stripFieldLabel`.
fn strip_field_label(s: &str) -> String {
    const LABELS: [&str; 6] = [
        "review findings",
        "findings",
        "review result",
        "result",
        "verdict",
        "review",
    ];
    for label in LABELS {
        if let Some(rest) = s.strip_prefix(label)
            && let Some(after) = rest.strip_prefix(':')
        {
            return after.trim().to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Severity ──────────────────────────────────────────────────

    #[test]
    fn severity_orders_critical_highest() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
    }

    #[test]
    fn severity_from_prefix_matches_leading_word() {
        assert_eq!(Severity::from_prefix("Critical — x"), Some(Severity::Critical));
        assert_eq!(Severity::from_prefix("HIGH"), Some(Severity::High));
        assert_eq!(Severity::from_prefix("medium issue"), Some(Severity::Medium));
        assert_eq!(Severity::from_prefix("nope"), None);
    }

    // ── Preambles carry the ported discipline ─────────────────────

    #[test]
    fn review_preamble_has_evidence_discipline() {
        let p = REVIEW_PREAMBLE.to_ascii_lowercase();
        assert!(p.contains("without specific evidence in the diff"));
        assert!(p.contains("do not report"));
        // Constraint-awareness (never demand a forbidden action).
        assert!(p.contains("told not to take"));
        // Toolchain-from-manifest rule survived the port.
        assert!(p.contains("manifest"));
    }

    #[test]
    fn security_preamble_has_burden_of_proof() {
        let p = SECURITY_PREAMBLE.to_ascii_lowercase();
        assert!(p.contains("exploitability burden of proof"));
        assert!(p.contains("prefer no finding"));
        // The same-user env-var carve-out is the highest-value noise filter.
        assert!(p.contains("same-user"));
    }

    #[test]
    fn review_format_defines_all_four_severities_and_separator() {
        let f = REVIEW_FORMAT;
        for level in ["critical", "high", "medium", "low"] {
            assert!(f.contains(level), "missing severity def: {level}");
        }
        assert!(f.contains("`---`"), "must document the finding separator");
        assert!(f.contains("No issues found."));
    }

    // ── parse_findings ────────────────────────────────────────────

    #[test]
    fn parse_findings_empty_on_clean_output() {
        assert!(parse_findings("No issues found.").is_empty());
        assert!(parse_findings("Summary: the diff renames a field. No issues found.").is_empty());
        // Vague narration is NOT a finding (divergence from roborev fail-default).
        assert!(parse_findings("The commit looks mostly fine but could use cleanup.").is_empty());
    }

    #[test]
    fn parse_findings_extracts_single_severity_block() {
        let out = "Summary line.\n\n- High — auth check skipped in login().";
        let f = parse_findings(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert!(f[0].body.contains("auth check skipped"));
    }

    #[test]
    fn parse_findings_splits_on_delimiter() {
        let out = "\
- High — SQL injection in query builder.\n\
---\n\
- Low: unclear variable name `x`.\n\
---\n\
Medium — missing error handling on read.";
        let f = parse_findings(out);
        assert_eq!(f.len(), 3, "three delimited findings");
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[1].severity, Severity::Low);
        assert_eq!(f[2].severity, Severity::Medium);
    }

    #[test]
    fn parse_findings_reads_severity_field_form() {
        let out = "- **Severity**: Critical\n- **Location**: src/auth.rs:42\n- **Problem**: token leak.";
        let f = parse_findings(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(f[0].location.as_deref(), Some("src/auth.rs:42"));
    }

    #[test]
    fn parse_findings_ignores_a_severity_legend() {
        // A rubric block must not be mistaken for findings.
        let out = "\
Severity levels:\n\
- high: breaks prod\n\
- low: cosmetic\n\
\n\
No issues found.";
        assert!(
            parse_findings(out).is_empty(),
            "legend entries must not become findings"
        );
    }

    #[test]
    fn parse_findings_does_not_match_high_level_prose() {
        // "High-level" has no separator after "high" → not a severity label.
        let out = "This is a high-level overview of the change. No issues found.";
        assert!(parse_findings(out).is_empty());
    }

    #[test]
    fn parse_findings_sortable_highest_first() {
        let out = "- Low: nit.\n---\n- Critical — data loss.\n---\n- Medium — perf.";
        let mut f = parse_findings(out);
        f.sort_by(|a, b| b.severity.cmp(&a.severity));
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(f[1].severity, Severity::Medium);
        assert_eq!(f[2].severity, Severity::Low);
    }

    // ── verdict_is_pass: ported from roborev's verdict_test.go ─────

    #[test]
    fn verdict_pass_phrases() {
        for out in [
            "No issues found.",
            "**No issues found.**",
            "## No issues found",
            "__No issues found.__",
            "No issues found; no tests failed.",
            "No issues found. This update prevents crashes when input is nil.",
            "I didn't find any issues in this commit.",
            "I didn\u{2019}t find any issues in this commit.",
            "I did not find any issues with the code.",
            "I found no issues.",
            "**Verdict**: PASS",
            "**Verdict**:No issues found.",
            "2. **Review Findings**:No issues found.",
        ] {
            assert!(verdict_is_pass(out), "should be pass: {out:?}");
        }
    }

    #[test]
    fn verdict_no_finding_remaining_phrases_pass() {
        for out in [
            "All previous findings have been addressed.",
            "No verified findings remain.",
            "0 findings",
            "Zero findings remain.",
        ] {
            assert!(verdict_is_pass(out), "should be pass: {out:?}");
        }
    }

    #[test]
    fn verdict_fail_cases() {
        for out in [
            "",
            "The commit looks mostly fine but could use some cleanup.",
            "The code has issues.",
            "**Verdict**: FAIL",
            "Medium - Security issue\nOtherwise no issues found.",
            "**Findings**\n- Medium — Possible regression in deploy.\nNo issues found beyond the notes above.",
            "- Low: Minor style issue.\nOtherwise no issues.",
            "* High - Security vulnerability found.\nNo issues found.",
            "- Critical — Data loss possible.\nNo issues otherwise.",
            "Critical — Data loss possible.\nNo issues otherwise.",
            "High: Security vulnerability in auth module.\nNo issues found.",
            "- **Severity**: High\n- **Location**: file.go\n- **Problem**: Bug found.",
            "Severity: High\nLocation: file.go\nProblem: Bug found.",
            "Severity - High\nLocation: file.go\nProblem: Bug found.",
        ] {
            assert!(!verdict_is_pass(out), "should be fail: {out:?}");
        }
    }

    // ── ported string helpers ─────────────────────────────────────

    #[test]
    fn strip_markdown_removes_headers_and_bold() {
        assert_eq!(strip_markdown("## No issues found"), "No issues found");
        assert_eq!(strip_markdown("**bold**"), "bold");
        assert_eq!(strip_markdown("__x__"), "x");
    }

    #[test]
    fn strip_list_marker_handles_bullets_and_numbers() {
        assert_eq!(strip_list_marker("- item"), "item");
        assert_eq!(strip_list_marker("* item"), "item");
        assert_eq!(strip_list_marker("1. item"), "item");
        assert_eq!(strip_list_marker("99) item"), "item");
        assert_eq!(strip_list_marker("plain"), "plain");
    }

    #[test]
    fn strip_field_label_removes_known_labels() {
        assert_eq!(strip_field_label("findings: no issues found."), "no issues found.");
        assert_eq!(strip_field_label("verdict: fail"), "fail");
        assert_eq!(strip_field_label("something else"), "something else");
    }
}

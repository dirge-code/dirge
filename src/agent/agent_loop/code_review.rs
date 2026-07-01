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
- Dependency pinning, stale-dependency, or typosquatting concerns unless this diff introduces or \
changes the dependency and there is specific evidence of risk.\n\
- Error-message leakage unless the leaked value is sensitive and reachable by an unauthorized \
actor.\n\
- CI permission concerns unless untrusted code or input can influence the privileged workflow.\n\
- Environment-variable exposure unless the reviewed code newly exposes env vars across a trust \
boundary — returning them in an HTTP response, writing them to user-visible logs, embedding them \
in generated artifacts, sending them to third-party services, or making them readable by \
less-privileged users.\n\
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
                .trim_start_matches([
                    '-', '*', '\u{2022}', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.',
                    ')', ' ',
                ])
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
            let has_sep = rest.starts_with([':', '|', '—', '–']) || rest.starts_with("- ");
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
            const INDICATORS: [&str; 7] = [
                "severity", "level", "legend", "priority", "rubric", "rating", "scale",
            ];
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
    // `•` (U+2022) as well as ASCII bullets — `detect_block_severity`
    // strips it inline, so the shared helper must too, else a `•`-bulleted
    // legend header or `No issues found.` line slips past the callers that
    // route through here (extract_location, is_legend_entry, verdict_is_pass).
    if let Some(rest) = s.strip_prefix('\u{2022}') {
        return rest.trim().to_string();
    }
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

// ── Run-diff capture (R2) ─────────────────────────────────────────────

use std::path::Path;
use std::process::Command;

/// Upper bound on the diff fed to the reviewer, so a large refactor can't
/// balloon the judge call. Generous — the material that matters (the
/// changed hunks) usually fits — but bounded, with a truncation note so
/// the model knows more was elided. Kept beside the critic's own
/// `MAX_RULES_CHARS` sizing philosophy.
const MAX_DIFF_BYTES: usize = 64_000;

/// Capture the run's uncommitted changes as a single unified diff, ready
/// for the reviewer: tracked edits (`git diff HEAD`) plus any new
/// untracked files, exclude-filtered and size-capped. Returns `None` when
/// there is nothing to review (clean tree, not a git repo, or git absent)
/// — the gate treats `None` as "no diff, skip".
///
/// Thin git glue on purpose: the formatting/filtering/capping below is
/// pure and unit-tested; this function is the one impure seam.
pub fn capture_run_diff(repo: &Path) -> Option<String> {
    let raw = raw_uncommitted_diff(repo);
    let formatted = format_run_diff(&raw);
    if formatted.trim().is_empty() {
        None
    } else {
        Some(formatted)
    }
}

/// Combine tracked and untracked changes into one raw diff string.
/// Tracked: `git diff HEAD` (staged + unstaged vs the last commit),
/// falling back to `git diff` + `git diff --cached` when there is no HEAD
/// yet (a repo with no commits). Untracked: each `--others` file rendered
/// as an addition via `git diff --no-index`.
fn raw_uncommitted_diff(repo: &Path) -> String {
    let mut out = String::new();

    // `--no-ext-diff --no-color` force canonical unified output regardless
    // of the user's git config — many devs set `diff.external` (e.g.
    // difftastic), whose output is not a parseable unified diff.
    match git_out(repo, &["diff", "--no-ext-diff", "--no-color", "HEAD"]) {
        Some(d) if !d.trim().is_empty() => out.push_str(&d),
        Some(_) => {}
        // No HEAD (no commits yet) — union of unstaged and staged.
        None => {
            for args in [
                &["diff", "--no-ext-diff", "--no-color"][..],
                &["diff", "--no-ext-diff", "--no-color", "--cached"][..],
            ] {
                if let Some(d) = git_out(repo, args)
                    && !d.trim().is_empty()
                {
                    out.push_str(&d);
                }
            }
        }
    }

    // Untracked files: render each as an addition diff. `--no-index`
    // exits non-zero when files differ (the normal case), so its stdout
    // is captured regardless of exit status.
    if let Some(list) = git_out(repo, &["ls-files", "--others", "--exclude-standard"]) {
        for path in list.lines().filter(|l| !l.trim().is_empty()) {
            if should_exclude(path) {
                continue;
            }
            if let Some(d) = git_stdout_allow_fail(
                repo,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-color",
                    "--no-index",
                    "--",
                    "/dev/null",
                    path,
                ],
            ) && !d.trim().is_empty()
            {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&d);
            }
        }
    }

    out
}

/// Run git in `repo`, returning trimmed-nonempty stdout only on success.
/// `None` on non-zero exit or spawn failure (git missing / not a repo).
fn git_out(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Like [`git_out`] but returns stdout even on a non-zero exit — for
/// `git diff --no-index`, which signals "files differ" with exit 1.
fn git_stdout_allow_fail(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    if s.trim().is_empty() { None } else { Some(s) }
}

/// Filter excluded files out of a raw diff, then size-cap it. Pure.
fn format_run_diff(raw: &str) -> String {
    cap_diff(&filter_diff_excludes(raw), MAX_DIFF_BYTES)
}

/// Drop per-file sections whose path is noise (lockfiles, generated
/// output, vendored/build trees). Splits the diff on `diff --git`
/// boundaries and keeps only sections whose path survives
/// [`should_exclude`].
fn filter_diff_excludes(diff: &str) -> String {
    let sections = split_diff_sections(diff);
    if sections.is_empty() {
        return String::new();
    }
    let mut kept: Vec<String> = Vec::new();
    for section in sections {
        match section_path(&section) {
            Some(path) if should_exclude(&path) => {}
            _ => kept.push(section),
        }
    }
    kept.join("\n")
}

/// Split a unified diff into per-file sections, each starting at a
/// `diff --git ` line. Any preamble before the first such line is dropped
/// (git diffs don't have one, but a `--no-index` concat might).
fn split_diff_sections(diff: &str) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if let Some(sec) = current.take() {
                sections.push(sec.trim_end().to_string());
            }
            current = Some(format!("{line}\n"));
        } else if let Some(cur) = current.as_mut() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    if let Some(sec) = current {
        sections.push(sec.trim_end().to_string());
    }
    sections
}

/// Extract the file path a diff section applies to. Prefers the new-side
/// (`+++ b/…`) path; falls back to the old-side (`--- a/…`) when the new
/// side is `/dev/null` (a deletion), then to the `diff --git` header.
fn section_path(section: &str) -> Option<String> {
    let mut header_path: Option<String> = None;
    let mut old_path: Option<String> = None;
    for line in section.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let p = strip_diff_path_prefix(rest.trim());
            if p != "/dev/null" && !p.is_empty() {
                return Some(p);
            }
        } else if let Some(rest) = line.strip_prefix("--- ") {
            let p = strip_diff_path_prefix(rest.trim());
            if p != "/dev/null" && !p.is_empty() {
                old_path = Some(p);
            }
        } else if let Some(rest) = line.strip_prefix("diff --git ")
            && header_path.is_none()
        {
            // "a/x b/y" — take the b-side token.
            header_path = rest
                .split_whitespace()
                .next_back()
                .map(strip_diff_path_prefix);
        }
    }
    old_path.or(header_path)
}

/// Strip a leading `a/` or `b/` (git diff path prefix) and any trailing
/// tab-quoted metadata. `/dev/null` passes through unchanged.
fn strip_diff_path_prefix(p: &str) -> String {
    let p = p.split('\t').next().unwrap_or(p);
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
        .to_string()
}

/// True when a path is review noise: dependency lockfiles, minified /
/// generated output, and vendored/build directories. Excluding these
/// keeps the reviewer focused on human-authored change and out of
/// machine-generated churn (roborev applies the same idea via its
/// configurable exclude patterns).
fn should_exclude(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);

    const LOCKFILES: [&str; 10] = [
        "cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "gemfile.lock",
        "composer.lock",
        "go.sum",
        "flake.lock",
        "uv.lock",
    ];
    if LOCKFILES.contains(&name) {
        return true;
    }

    const SUFFIXES: [&str; 4] = [".min.js", ".min.css", ".map", ".snap"];
    if SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return true;
    }

    const DIR_MARKERS: [&str; 6] = [
        "node_modules/",
        "/target/",
        "/dist/",
        "/build/",
        "/vendor/",
        "/.git/",
    ];
    DIR_MARKERS.iter().any(|d| lower.contains(d))
        || lower.starts_with("target/")
        || lower.starts_with("dist/")
        || lower.starts_with("vendor/")
}

/// Truncate a diff to `max` bytes on a char boundary, appending a note so
/// the reviewer knows the tail was elided. Returns the input untouched
/// when it already fits.
fn cap_diff(diff: &str, max: usize) -> String {
    if diff.len() <= max {
        return diff.to_string();
    }
    let mut end = max;
    while end > 0 && !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… (diff truncated — {} of {} bytes shown)",
        &diff[..end],
        end,
        diff.len()
    )
}

// ── Prompt + orchestration (R3) ───────────────────────────────────────

use super::critic::CriticFn;
use super::message::{LoopMessage, UserMessage};

/// Cap on the instructions/constraints block fed to the reviewer. Smaller
/// than the critic's (16k) because the diff and transcript also compete for
/// the judge's budget; the constraints that matter (AGENTS.md, prompt-mode
/// rules) sit early in the system prompt.
const MAX_RULES_CHARS: usize = 12_000;

/// How many finalizations the reviewer may re-engage within one run. Like
/// the goal gate's `MAX_GOAL_REACT`, this persists across finalization
/// boundaries (so the agent can fix findings and be re-reviewed) but is
/// bounded so a stubborn or unsatisfiable finding can't loop forever.
pub const MAX_REVIEW_REACT: u8 = 3;

/// Build the reviewer prompt: the response format, the assistant's own
/// constraints (so the reviewer judges within them and never demands a
/// forbidden action), the diff to review, and a transcript for intent.
/// Mirrors the critic's `build_prompt` split — role in the preamble,
/// format beside the material. Reuses the critic's compaction-summary
/// stripper so a stale `## Active Task` can't leak in.
pub fn build_review_prompt(rules: &str, diff: &str, transcript: &str) -> String {
    let rules = super::critic::strip_compaction_summary(rules).trim();
    let rules_block = if rules.is_empty() {
        "(no special constraints provided)".to_string()
    } else if rules.len() > MAX_RULES_CHARS {
        let head: String = rules.chars().take(MAX_RULES_CHARS).collect();
        format!("{head}\n…(instructions truncated)")
    } else {
        rules.to_string()
    };
    let transcript = if transcript.trim().is_empty() {
        "(no transcript)"
    } else {
        transcript.trim()
    };
    format!(
        "{REVIEW_FORMAT}\n\n\
         --- assistant instructions & constraints (judge within these; never demand a \
         forbidden/out-of-scope action) ---\n{rules_block}\n--- end instructions ---\n\n\
         --- diff (the code changes to review) ---\n{diff}\n--- end diff ---\n\n\
         --- transcript (what the assistant did, for intent) ---\n{transcript}\n\
         --- end transcript ---"
    )
}

/// Verify/dedupe instructions for the second pass, carried in the user
/// prompt beside the candidate findings and the diff. Ported from
/// roborev's `VerifyDedupePreamble` (`internal/review/synthesis.go`),
/// adapted from its agentic "search the codebase" step to a toolless
/// judgment "against the diff shown" — dirge's reviewer judge is a
/// single-shot call with no tools, so verification is an adversarial
/// re-read of the diff rather than a fresh codebase search. It keeps
/// roborev's VERIFIED / FALSE_POSITIVE + consolidation contract and the
/// `---`/severity output format so [`parse_findings`] re-parses the
/// survivors.
const VERIFY_INSTRUCTIONS: &str = "\
You are verifying a set of candidate code-review findings against the diff that produced them. \
Work only from the diff shown — do not assume code you cannot see. Be skeptical: a plausible-but-\
unsupported finding wastes the author's time.\n\
\n\
1. Verify each candidate against the diff:\n\
   - Keep it (VERIFIED) only if the diff clearly supports it — the cited location is in the diff \
and the described problem is real.\n\
   - Drop it (FALSE_POSITIVE) if it misreads the code, cites a location not in the diff, is \
contradicted by another hunk, or is speculation about code not shown.\n\
2. Consolidate: merge candidates that describe the same underlying issue into one finding.\n\
3. Re-emit every SURVIVING finding using the output format below. If every candidate was a false \
positive, state \"No issues found.\" on its own line and nothing else.";

/// Run the diff-aware reviewer and return verified findings, highest
/// severity first. Two passes (dirge chose two-pass from the start):
///
///   1. review — the reviewer reads the diff and emits candidate findings.
///   2. verify — the same judge re-reads each candidate against the diff,
///      drops false positives, and merges duplicates.
///
/// Then a mechanical [`dedupe_findings`] pass removes any exact duplicates
/// the verify step missed. Fails OPEN throughout: a judge error in pass 1
/// yields no findings; an error/ambiguous result in pass 2 falls back to
/// the (deduped) pass-1 findings rather than silently dropping real work.
pub async fn run_code_review(
    review_fn: &CriticFn,
    rules: &str,
    diff: &str,
    transcript: &str,
) -> Vec<Finding> {
    let candidates = review_pass(review_fn, rules, diff, transcript).await;
    if candidates.is_empty() {
        return Vec::new();
    }
    let verified = verify_pass(review_fn, diff, &candidates).await;
    dedupe_findings(verified)
}

/// Pass 1: the reviewer reads the diff and emits candidate findings.
async fn review_pass(
    review_fn: &CriticFn,
    rules: &str,
    diff: &str,
    transcript: &str,
) -> Vec<Finding> {
    let prompt = build_review_prompt(rules, diff, transcript);
    let response = match review_fn(prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "dirge::code_review", error = %e, "reviewer call failed; finalizing without it");
            return Vec::new();
        }
    };
    let mut findings = parse_findings(&response);
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    findings
}

/// Pass 2: re-read each candidate against the diff, dropping false
/// positives and merging duplicates. Fail-SAFE: on a judge error, or when
/// the verify output is empty but doesn't read as a clean pass (a
/// malformed re-emit), fall back to the pass-1 candidates rather than
/// silently dropping them.
async fn verify_pass(review_fn: &CriticFn, diff: &str, candidates: &[Finding]) -> Vec<Finding> {
    let prompt = build_verify_prompt(candidates, diff);
    let response = match review_fn(prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "dirge::code_review", error = %e, "verify pass failed; keeping unverified findings");
            return candidates.to_vec();
        }
    };
    let mut survivors = parse_findings(&response);
    if survivors.is_empty() && !verdict_is_pass(&response) {
        // Ambiguous verify output (no parseable findings, no clean-pass
        // phrase) — don't silently drop; keep the pass-1 candidates.
        tracing::debug!(target: "dirge::code_review", "verify output ambiguous; keeping pass-1 findings");
        return candidates.to_vec();
    }
    survivors.sort_by_key(|f| std::cmp::Reverse(f.severity));
    survivors
}

/// Build the verify-pass user prompt: the ported verify/dedupe
/// instructions, the candidate findings, the diff to check them against,
/// and the review output format so survivors are re-emitted parseably.
pub fn build_verify_prompt(candidates: &[Finding], diff: &str) -> String {
    let listed = candidates
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}. [{}] {}", i + 1, f.severity.label(), f.body.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "{VERIFY_INSTRUCTIONS}\n\n\
         --- candidate findings ---\n{listed}\n--- end candidates ---\n\n\
         --- diff ---\n{diff}\n--- end diff ---\n\n\
         --- output format ---\n{REVIEW_FORMAT}"
    )
}

/// Drop exact-ish duplicate findings the verify pass may have missed.
/// Keeps the first occurrence keyed by (severity, location-or-body-head),
/// normalized to alphanumerics — a cheap mechanical backstop to the LLM's
/// consolidation, order-preserving.
fn dedupe_findings(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(findings.len());
    for f in findings {
        let key_src = f
            .location
            .clone()
            .unwrap_or_else(|| f.body.chars().take(80).collect());
        let sig = format!(
            "{}:{}",
            f.severity.label(),
            key_src
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        );
        if seen.insert(sig) {
            out.push(f);
        }
    }
    out
}

/// Split findings into the blocking channel (`High`/`Critical` — the agent
/// must fix or justify them before finalizing) and the advisory channel
/// (`Medium`/`Low` — surfaced to the user, never blocks). This is the
/// severity gate: it mirrors roborev's severity model, but where roborev
/// only ranks a PR comment, dirge acts on the split — high/critical
/// re-enter the loop, medium/low are FYI.
pub fn partition_findings(findings: Vec<Finding>) -> (Vec<Finding>, Vec<Finding>) {
    findings
        .into_iter()
        .partition(|f| matches!(f.severity, Severity::High | Severity::Critical))
}

/// Build the blocking `[code-review]` follow-up from high/critical
/// findings — the agent re-enters and must fix each or justify why it
/// doesn't apply. `None` when there is nothing blocking.
pub fn blocking_followup(blocking: &[Finding]) -> Option<LoopMessage> {
    if blocking.is_empty() {
        return None;
    }
    let body = render_findings(blocking);
    Some(LoopMessage::User(UserMessage {
        content: format!(
            "{CODE_REVIEW_TAG} A review of the diff you just made found these high-severity \
             issues. Fix each, or explain why it doesn't apply (out of scope, intended, or \
             something you were told not to do):\n{body}"
        ),
    }))
}

/// Render the advisory (medium/low) findings as a non-blocking
/// `SystemNotice` body. `None` when there is nothing to advise. The caller
/// emits this to the user without re-entering the loop.
pub fn advisory_notice(advisory: &[Finding]) -> Option<String> {
    if advisory.is_empty() {
        return None;
    }
    let body = render_findings(advisory);
    Some(format!(
        "{CODE_REVIEW_TAG} lower-severity notes on your changes (advisory — not blocking):\n{body}"
    ))
}

/// Join finding bodies with the `---` separator, each led by its severity.
fn render_findings(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| format!("[{}] {}", f.severity.label(), f.body.trim()))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
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
        assert_eq!(
            Severity::from_prefix("Critical — x"),
            Some(Severity::Critical)
        );
        assert_eq!(Severity::from_prefix("HIGH"), Some(Severity::High));
        assert_eq!(
            Severity::from_prefix("medium issue"),
            Some(Severity::Medium)
        );
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
        // The ported don't-report carve-outs that keep the security pass
        // from flagging routine churn (roborev parity).
        assert!(p.contains("typosquatting"));
        assert!(p.contains("error-message leakage"));
        assert!(p.contains("ci permission"));
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
        let out =
            "- **Severity**: Critical\n- **Location**: src/auth.rs:42\n- **Problem**: token leak.";
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
        f.sort_by_key(|f| std::cmp::Reverse(f.severity));
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
        assert_eq!(strip_list_marker("\u{2022} item"), "item");
        assert_eq!(strip_list_marker("1. item"), "item");
        assert_eq!(strip_list_marker("99) item"), "item");
        assert_eq!(strip_list_marker("plain"), "plain");
    }

    /// A `•`-bulleted legend must still be recognized as a legend (the
    /// helper strips `•` now, matching detect_block_severity), so a
    /// `•`-bulleted severity line under it isn't mistaken for a finding.
    #[test]
    fn bullet_char_legend_is_not_a_finding() {
        let out = "\
Severity scale:\n\
\u{2022} high: breaks prod\n\
\u{2022} low: cosmetic\n\
\n\
No issues found.";
        assert!(parse_findings(out).is_empty());
    }

    #[test]
    fn strip_field_label_removes_known_labels() {
        assert_eq!(
            strip_field_label("findings: no issues found."),
            "no issues found."
        );
        assert_eq!(strip_field_label("verdict: fail"), "fail");
        assert_eq!(strip_field_label("something else"), "something else");
    }

    // ── Run-diff capture (R2) ─────────────────────────────────────

    #[test]
    fn should_exclude_lockfiles_and_generated() {
        for p in [
            "Cargo.lock",
            "web/package-lock.json",
            "yarn.lock",
            "go.sum",
            "app/bundle.min.js",
            "styles.min.css",
            "out.js.map",
            "node_modules/foo/index.js",
            "target/debug/build.rs",
            "vendor/lib/x.go",
        ] {
            assert!(should_exclude(p), "should exclude {p}");
        }
    }

    #[test]
    fn should_not_exclude_source_files() {
        for p in [
            "src/main.rs",
            "lib/auth.ts",
            "cmd/app/main.go",
            "pkg/util.py",
        ] {
            assert!(!should_exclude(p), "should keep {p}");
        }
    }

    #[test]
    fn split_and_path_extraction() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs\n\
index 111..222 100644\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n\
diff --git a/Cargo.lock b/Cargo.lock\n\
--- a/Cargo.lock\n\
+++ b/Cargo.lock\n\
@@ -1 +1 @@\n\
-x\n\
+y\n";
        let sections = split_diff_sections(diff);
        assert_eq!(sections.len(), 2);
        assert_eq!(section_path(&sections[0]).as_deref(), Some("src/a.rs"));
        assert_eq!(section_path(&sections[1]).as_deref(), Some("Cargo.lock"));
    }

    #[test]
    fn section_path_handles_deletion_dev_null() {
        let section = "\
diff --git a/old.rs b/old.rs\n\
deleted file mode 100644\n\
--- a/old.rs\n\
+++ /dev/null\n";
        assert_eq!(section_path(section).as_deref(), Some("old.rs"));
    }

    #[test]
    fn filter_diff_excludes_drops_lockfile_keeps_source() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n\
diff --git a/Cargo.lock b/Cargo.lock\n\
--- a/Cargo.lock\n\
+++ b/Cargo.lock\n\
@@ -1 +1 @@\n\
-x\n\
+y\n";
        let filtered = filter_diff_excludes(diff);
        assert!(filtered.contains("src/a.rs"), "source kept");
        assert!(!filtered.contains("Cargo.lock"), "lockfile dropped");
    }

    #[test]
    fn cap_diff_truncates_with_note_on_char_boundary() {
        // Multibyte content to exercise the char-boundary walk-back.
        let big = "é".repeat(1000); // 2 bytes each → 2000 bytes
        let capped = cap_diff(&big, 101);
        assert!(capped.contains("diff truncated"));
        assert!(capped.contains("of 2000 bytes"));
        // Must not have split a multibyte char (would panic on slice).
        assert!(capped.starts_with('é'));
    }

    #[test]
    fn cap_diff_leaves_small_input_untouched() {
        assert_eq!(cap_diff("small", 100), "small");
    }

    // Git-backed integration: exercises the one impure seam.
    fn git(dir: &Path, args: &[&str]) -> String {
        let mut full = vec![
            "-c",
            "user.email=test@dirge",
            "-c",
            "user.name=dirge",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ];
        full.extend_from_slice(args);
        let out = Command::new("git")
            .current_dir(dir)
            .arg("-C")
            .arg(dir)
            .args(&full)
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn temp_repo() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "dirge-codereview-diff-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init"]);
        root
    }

    #[test]
    fn capture_run_diff_is_none_on_clean_tree() {
        let repo = temp_repo();
        std::fs::write(repo.join("a.rs"), "fn main() {}\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        assert!(
            capture_run_diff(&repo).is_none(),
            "clean tree yields no diff"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn capture_run_diff_includes_edits_and_untracked_excludes_lockfiles() {
        let repo = temp_repo();
        std::fs::write(repo.join("a.rs"), "fn main() {}\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);

        // A tracked edit, a new untracked source file, and a noisy lockfile.
        std::fs::write(repo.join("a.rs"), "fn main() { let x = 1; }\n").unwrap();
        std::fs::write(repo.join("b.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(repo.join("Cargo.lock"), "# lockfile churn\n").unwrap();

        let diff = capture_run_diff(&repo).expect("dirty tree yields a diff");
        assert!(diff.contains("a.rs"), "tracked edit present");
        assert!(diff.contains("let x = 1"), "tracked edit body present");
        assert!(diff.contains("b.rs"), "untracked new file present");
        assert!(diff.contains("helper"), "untracked file body present");
        assert!(!diff.contains("Cargo.lock"), "lockfile excluded");
        let _ = std::fs::remove_dir_all(&repo);
    }

    // ── Prompt + orchestration (R3) ───────────────────────────────

    #[test]
    fn review_prompt_embeds_format_diff_and_constraints() {
        let p = build_review_prompt(
            "RULE: never push to remote.",
            "diff --git a/x b/x\n+let y = 1;",
            "user asked to add y",
        );
        assert!(p.contains("`---`"), "format contract present");
        assert!(p.contains("let y = 1"), "diff embedded");
        assert!(p.contains("never push to remote"), "constraints embedded");
        assert!(p.contains("user asked to add y"), "transcript embedded");
    }

    #[test]
    fn review_prompt_strips_compaction_summary_from_rules() {
        let rules = format!(
            "RULE: never push.\n\n{}\n## Active Task\nOld phase 3 work.",
            crate::agent::compression::COMPACTION_MARKER,
        );
        let p = build_review_prompt(&rules, "diff", "t");
        assert!(p.contains("never push"), "real rules survive");
        assert!(!p.contains("Active Task"), "stale summary stripped");
    }

    #[test]
    fn review_prompt_caps_large_rules() {
        let huge = "x".repeat(MAX_RULES_CHARS + 5_000);
        let p = build_review_prompt(&huge, "diff", "t");
        assert!(p.contains("instructions truncated"));
    }

    #[tokio::test]
    async fn run_code_review_parses_and_sorts_findings() {
        let review: CriticFn = std::sync::Arc::new(|_prompt| {
            Box::pin(async {
                Ok("Summary.\n- Low: nit.\n---\n- Critical — data loss in write().".to_string())
            })
        });
        let f = run_code_review(&review, "rules", "diff", "t").await;
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].severity, Severity::Critical, "highest first");
        assert_eq!(f[1].severity, Severity::Low);
    }

    #[tokio::test]
    async fn run_code_review_fails_open_on_error() {
        let review: CriticFn =
            std::sync::Arc::new(|_p| Box::pin(async { anyhow::bail!("provider down") }));
        assert!(run_code_review(&review, "r", "d", "t").await.is_empty());
    }

    #[tokio::test]
    async fn run_code_review_clean_yields_no_findings() {
        let review: CriticFn =
            std::sync::Arc::new(|_p| Box::pin(async { Ok("No issues found.".to_string()) }));
        assert!(run_code_review(&review, "r", "d", "t").await.is_empty());
    }

    #[test]
    fn partition_splits_blocking_from_advisory() {
        let findings = vec![
            Finding {
                severity: Severity::Critical,
                location: None,
                body: "Critical — x".into(),
            },
            Finding {
                severity: Severity::High,
                location: None,
                body: "High — y".into(),
            },
            Finding {
                severity: Severity::Medium,
                location: None,
                body: "Medium — z".into(),
            },
            Finding {
                severity: Severity::Low,
                location: None,
                body: "Low — w".into(),
            },
        ];
        let (blocking, advisory) = partition_findings(findings);
        assert_eq!(blocking.len(), 2, "critical + high block");
        assert_eq!(advisory.len(), 2, "medium + low advise");
        assert!(
            blocking
                .iter()
                .all(|f| matches!(f.severity, Severity::High | Severity::Critical))
        );
        assert!(
            advisory
                .iter()
                .all(|f| matches!(f.severity, Severity::Medium | Severity::Low))
        );
    }

    #[test]
    fn blocking_followup_none_when_empty_and_tags_when_present() {
        assert!(blocking_followup(&[]).is_none());
        let blocking = vec![Finding {
            severity: Severity::High,
            location: Some("src/a.rs:1".into()),
            body: "High — auth skipped".into(),
        }];
        let msg = blocking_followup(&blocking).expect("some");
        let content = match &msg {
            LoopMessage::User(u) => &u.content,
            _ => panic!("expected user message"),
        };
        assert!(content.starts_with(CODE_REVIEW_TAG));
        assert!(content.contains("auth skipped"));
        assert!(content.to_lowercase().contains("fix each"));
    }

    #[test]
    fn advisory_notice_none_when_empty_and_marks_non_blocking() {
        assert!(advisory_notice(&[]).is_none());
        let advisory = vec![Finding {
            severity: Severity::Low,
            location: None,
            body: "Low — nit".into(),
        }];
        let text = advisory_notice(&advisory).expect("some");
        assert!(text.starts_with(CODE_REVIEW_TAG));
        assert!(text.contains("nit"));
        assert!(text.to_lowercase().contains("advisory"));
    }

    // ── Two-pass verify/dedupe (R4) ───────────────────────────────

    /// A judge stub that answers pass 1 and pass 2 differently.
    fn two_pass_stub(pass1: &'static str, pass2: &'static str) -> CriticFn {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::sync::Arc::new(move |_p: String| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = if n == 0 { pass1 } else { pass2 }.to_string();
            Box::pin(async move { Ok(out) })
        })
    }

    #[tokio::test]
    async fn verify_pass_drops_false_positives() {
        // Pass 1 finds two; verify keeps only the real one.
        let review = two_pass_stub(
            "- High — SQL injection in query().\n---\n- Low — misread nit.",
            "Verified.\n- High — SQL injection in query().",
        );
        let f = run_code_review(&review, "r", "diff", "t").await;
        assert_eq!(f.len(), 1, "false positive dropped");
        assert_eq!(f[0].severity, Severity::High);
    }

    #[tokio::test]
    async fn verify_pass_can_clear_all_findings() {
        let review = two_pass_stub(
            "- Medium — maybe a bug.",
            "All candidates were speculation.\nNo issues found.",
        );
        assert!(
            run_code_review(&review, "r", "diff", "t").await.is_empty(),
            "verify cleared every finding"
        );
    }

    #[tokio::test]
    async fn verify_ambiguous_output_keeps_pass1_findings() {
        // Pass 2 returns neither parseable findings nor a clean-pass
        // phrase → fall back to pass 1 rather than silently dropping.
        let review = two_pass_stub(
            "- High — real bug in write().",
            "hmm, let me think about this differently...",
        );
        let f = run_code_review(&review, "r", "diff", "t").await;
        assert_eq!(f.len(), 1, "ambiguous verify keeps pass-1 findings");
        assert_eq!(f[0].severity, Severity::High);
    }

    #[tokio::test]
    async fn verify_error_keeps_pass1_findings() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let review: CriticFn = {
            let calls = calls.clone();
            std::sync::Arc::new(move |_p: String| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    if n == 0 {
                        Ok("- High — real bug.".to_string())
                    } else {
                        anyhow::bail!("verify provider down")
                    }
                })
            })
        };
        let f = run_code_review(&review, "r", "diff", "t").await;
        assert_eq!(f.len(), 1, "verify error must not drop pass-1 findings");
    }

    #[test]
    fn build_verify_prompt_lists_candidates_and_diff() {
        let candidates = vec![Finding {
            severity: Severity::High,
            location: Some("a.rs:1".into()),
            body: "High — bug".into(),
        }];
        let p = build_verify_prompt(&candidates, "diff --git a/a.rs");
        assert!(p.contains("FALSE_POSITIVE"), "verify contract present");
        assert!(p.contains("High — bug"), "candidate listed");
        assert!(p.contains("diff --git a/a.rs"), "diff embedded");
        assert!(p.contains("`---`"), "output format embedded");
    }

    #[test]
    fn dedupe_findings_collapses_duplicates() {
        let findings = vec![
            Finding {
                severity: Severity::High,
                location: Some("src/a.rs:10".into()),
                body: "High — auth skipped".into(),
            },
            // Same severity+location → duplicate.
            Finding {
                severity: Severity::High,
                location: Some("src/a.rs:10".into()),
                body: "High — auth check missing".into(),
            },
            // Different location → kept.
            Finding {
                severity: Severity::High,
                location: Some("src/b.rs:3".into()),
                body: "High — other".into(),
            },
        ];
        let out = dedupe_findings(findings);
        assert_eq!(out.len(), 2, "one duplicate collapsed");
        assert_eq!(out[0].location.as_deref(), Some("src/a.rs:10"));
        assert_eq!(out[1].location.as_deref(), Some("src/b.rs:3"));
    }
}

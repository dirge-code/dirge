//! Deterministic artifact-scope sourcing gate (dirge-lavc GAP 1).
//!
//! The claim gate ([`super::claim_gate`]) and the critic both inspect the
//! model's FINAL ANSWER. The fabrication that motivated this issue was not
//! in the answer — it was a `///` doc comment written INTO a source file:
//!
//!     Rates are the providers' published API list prices, checked Aug 2026:
//!     - `gpt-4o` (OpenAI pricing page): ...
//!
//! Six named external sources, in a session with no network access and no
//! fetch tool call anywhere. A claim written into the artifact is
//! structurally outside a final-answer scanner's field of view.
//!
//! This gate checks ADDED COMMENT LINES in the run's diff instead: when an
//! added comment asserts having consulted an external source, and the run's
//! tool history shows no fetch/search, it fires one model-visible nudge.
//!
//! Deliberately conservative, in the same spirit as claim_gate.rs:
//! over-detecting would decline good work and nag forever, so the bias is
//! hard toward UNDER-detecting. The gate defaults to `off` (its own mode
//! gate, separate from `claim_gate_mode` — a different risk profile, and it
//! must be opt-in until it has real-world mileage). What counts as a
//! sourcing claim is a tight vocabulary of external-consultation phrases,
//! plus explicit exclusions for the normal comment kinds that look like
//! sourcing but are not: RFC citations, bug/issue IDs, spec sections, URLs
//! the code itself fetches at runtime, and references to other files in the
//! repo. A missed fabrication is recoverable; a gate that nags on honest
//! comments gets disabled and then catches nothing.

use super::types::GateMode;

/// Tag prefixing the model-visible nudge, so it is greppable in transcripts.
pub(crate) const SOURCE_GATE_TAG: &str = "[source-check]";

/// Per-run nudge ceiling, by mode. Mirrors
/// [`super::claim_gate::claim_nudge_cap`]: `off` never fires, `advisory`
/// says it once, `blocking` re-enters up to three times. Bounded because a
/// model that ignores the first ask will not be persuaded by the fourth.
pub(crate) fn source_nudge_cap(mode: GateMode) -> u8 {
    match mode {
        GateMode::Off => 0,
        GateMode::Advisory => 1,
        GateMode::Blocking => 3,
    }
}

/// Tool names that count as having consulted an external source.
///
/// A bash `curl` is invisible here: the tool set records `bash`, not what
/// the shell did. Note which way that cuts — it is the gate's one
/// OVER-detection risk, not an under-detection one. A run that genuinely
/// curled a page and then cited it has no fetch tool recorded, so the
/// citation reads as unsupported and the gate fires on honest work.
///
/// Accepted rather than fixed, for now: distinguishing a sourcing `curl`
/// from any other shell invocation means parsing bash command strings,
/// which is a much larger and more error-prone surface than this gate is
/// worth. The exposure is bounded — the gate is off by default, `advisory`
/// caps at one nudge, and a model that did fetch can say so. If real use
/// shows this firing on honest work, widen the evidence side (recognize
/// fetch-shaped bash) rather than narrowing the vocabulary.
const FETCH_TOOLS: &[&str] = &["webfetch", "websearch"];

/// Added comment lines the gate deems to assert external sourcing, from the
/// run's diff. Pure: parses two canonical unified diff strings (`git diff
/// --no-ext-diff --no-color` output, as produced by
/// [`super::code_review::capture_run_diff`]), restricts to the files the
/// run actually touched (`allowed_paths`, repo-relative), subtracts lines
/// that were already added before the run started (the baseline diff), and
/// keeps only comment lines whose body matches the sourcing vocabulary.
///
/// `baseline_diff` is the run-start diff; a line present in it is not an
/// addition of THIS run and must not fire. `None` (clean tree at run start)
/// means nothing to subtract.
pub(crate) fn added_sourcing_comments(
    current_diff: &str,
    baseline_diff: Option<&str>,
    allowed_paths: &[String],
) -> Vec<String> {
    let added = parse_added_lines(current_diff);
    let baseline_added = baseline_diff.map(parse_added_lines).unwrap_or_default();
    let mut hits = Vec::new();
    for (path, lines) in &added {
        if !allowed_paths.iter().any(|a| path == a) {
            continue;
        }
        let preexisting = baseline_added.get(path).map(Vec::as_slice).unwrap_or(&[]);
        for line in lines {
            if preexisting.contains(line) {
                continue;
            }
            if let Some(comment) = comment_body(line)
                && has_sourcing_claim(comment)
            {
                hits.push(line.clone());
            }
        }
    }
    hits
}

/// Whether the run's evidence supports a sourcing claim found in the diff.
/// Supported (silent) when ANY fetch/search tool ran; unsupported (fires)
/// when at least one sourcing comment exists and none did.
pub(crate) fn unsupported_sourcing(comments: &[String], tool_names: &[String]) -> Option<String> {
    if comments.is_empty() {
        return None;
    }
    let fetched = tool_names.iter().any(|t| FETCH_TOOLS.contains(&t.as_str()));
    if fetched {
        return None;
    }
    comments.first().cloned()
}

/// Parse a canonical unified diff into per-file added lines. Keys are the
/// path as written in the diff (the `+++ b/...` line, prefixes stripped);
/// values are added (`+`) lines in file order. Tolerant of a capped diff:
/// a hunk truncated mid-way just contributes fewer lines.
fn parse_added_lines(diff: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim();
            current = if path == "/dev/null" {
                None
            } else {
                Some(strip_prefixes(path))
            };
        } else if let Some(added) = line.strip_prefix('+')
            && let Some(path) = &current
        {
            out.entry(path.clone()).or_default().push(added.to_string());
        }
    }
    out
}

/// Strip the `a/`/`b/` prefixes git writes on `diff --git` and `---`/`+++`
/// lines, so a path compares against a repo-relative allowed path.
fn strip_prefixes(path: &str) -> String {
    let trimmed = path.trim();
    for prefix in ["a/", "b/"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    trimmed.to_string()
}

/// The body of an added line if it is a comment line, else `None`. Comment
/// lines start with a comment marker after trimming. The longer markers are
/// tried first so a `///` line keeps a clean body rather than a stray `/`.
fn comment_body(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for marker in ["<!--", "///", "//!", "/*", "//", "*", "#", "--", ";"] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            let body = rest.trim();
            if !body.is_empty() {
                return Some(body);
            }
        }
    }
    None
}

/// Does this comment body assert having consulted an external source?
///
/// The vocabulary is deliberately tight. The general markers fire on plain
/// containment; `checked` and `as of` additionally require a date-ish or
/// page-ish target so ordinary comments ("checked in the parser", "as of
/// now") stay silent.
fn has_sourcing_claim(comment: &str) -> bool {
    let lower = comment.to_ascii_lowercase();
    if has_exclusion(&lower) {
        return false;
    }
    const GENERAL: &[&str] = &[
        "per the",
        "according to",
        "sourced from",
        "retrieved",
        "fetched",
        "published",
        "pricing page",
        "price list",
        "list price",
        "prices from",
        "data from",
    ];
    if GENERAL.iter().any(|m| lower.contains(m)) {
        return true;
    }
    if let Some(rest) = after_marker(&lower, "checked") {
        return checked_target(rest);
    }
    if let Some(rest) = after_marker(&lower, "as of") {
        return date_target(rest);
    }
    false
}

/// Normal comment kinds that look like sourcing but are not. Any match
/// suppresses the line entirely — under-detection wins every tie.
fn has_exclusion(lower: &str) -> bool {
    if contains_word(lower, "rfc")
        || contains_word(lower, "spec")
        || contains_word(lower, "bug")
        || contains_word(lower, "issue")
    {
        return true;
    }
    // A bug/issue/PR id written as #123, or a URL.
    if lower.contains('#') && lower.as_bytes().iter().any(|b| b.is_ascii_digit()) {
        return true;
    }
    if lower.contains("http://") || lower.contains("https://") || lower.contains("www.") {
        return true;
    }
    // A reference to another file in the repo.
    const FILE_TOKENS: &[&str] = &[
        ".rs", ".md", ".toml", ".json", ".yaml", ".yml", ".txt", ".nix", ".sh", ".py", ".ts",
        "src/", "docs/", "./", "../", "agends", "readme",
    ];
    FILE_TOKENS.iter().any(|t| lower.contains(t))
}

/// Whole-word containment (word = alphanumeric-surrounded).
fn contains_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut idx = 0;
    while let Some(rel) = haystack[idx..].find(word) {
        let pos = idx + rel;
        let before_ok = pos == 0 || !bytes[pos - 1].is_ascii_alphanumeric();
        let after = pos + word.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        idx = pos + word.len();
    }
    false
}

/// The slice after `marker` in `lower`, or `None` when the marker is absent
/// or has nothing after it. A word boundary is required before the marker
/// so "rechecked" or "checkedout" don't match.
fn after_marker<'a>(lower: &'a str, marker: &str) -> Option<&'a str> {
    let mut idx = 0;
    while let Some(rel) = lower[idx..].find(marker) {
        let pos = idx + rel;
        let before_ok = pos == 0 || !lower.as_bytes()[pos - 1].is_ascii_alphanumeric();
        if before_ok {
            let rest = &lower[pos + marker.len()..];
            if !rest.trim().is_empty() {
                return Some(rest);
            }
        }
        idx = pos + marker.len();
    }
    None
}

/// `checked` fires only against a date ("checked Aug 2026") or a page-ish
/// target ("checked the OpenAI pricing page"). "checked in the parser" and
/// "checked by the linter" stay silent.
fn checked_target(rest: &str) -> bool {
    date_target(rest) || {
        let trimmed = rest.trim_start();
        trimmed.starts_with("the ") && trimmed.contains("page")
    }
}

/// `as of` / `checked` date target: a month name or a 4-digit year.
fn date_target(rest: &str) -> bool {
    const MONTHS: &[&str] = &[
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
        "jan",
        "feb",
        "mar",
        "apr",
        "jun",
        "jul",
        "aug",
        "sep",
        "oct",
        "nov",
        "dec",
    ];
    if MONTHS.iter().any(|m| rest.contains(m)) {
        return true;
    }
    let bytes = rest.as_bytes();
    bytes.windows(4).any(|w| {
        w[0] == b'2' && w[1].is_ascii_digit() && w[2].is_ascii_digit() && w[3].is_ascii_digit()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The observed fabrication: a `///` doc comment attributing figures to
    /// six named pricing pages, written into src/config/mod.rs.
    const FABRICATED: &str = r#"diff --git a/src/config/mod.rs b/src/config/mod.rs
--- a/src/config/mod.rs
+++ b/src/config/mod.rs
@@ -100,6 +100,7 @@
 pub struct Config {
+    /// Rates are the providers' published API list prices, checked Aug 2026:
+    /// - `gpt-4o` (OpenAI pricing page): ...
     pub claim_gate: Option<String>,
 };
"#;

    fn diff_with(comment: &str) -> String {
        format!(
            "diff --git a/src/foo.rs b/src/foo.rs\n--- a/src/foo.rs\n+++ b/src/foo.rs\n@@ -1 +1 @@\n// {comment}\n"
        )
    }

    #[test]
    fn fabricated_sourcing_comment_fires_without_fetch_tool() {
        let allowed = vec!["src/config/mod.rs".to_string()];
        let hits = added_sourcing_comments(FABRICATED, None, &allowed);
        assert_eq!(
            hits.len(),
            2,
            "both fabricated lines must be flagged: {hits:?}"
        );
        assert_eq!(
            unsupported_sourcing(&hits, &[]),
            Some(hits[0].clone()),
            "no fetch tool ran → the claim is unsupported"
        );
    }

    #[test]
    fn same_comment_with_fetch_tool_is_silent() {
        let allowed = vec!["src/config/mod.rs".to_string()];
        let hits = added_sourcing_comments(FABRICATED, None, &allowed);
        assert_eq!(unsupported_sourcing(&hits, &["webfetch".to_string()]), None);
        assert_eq!(
            unsupported_sourcing(&hits, &["websearch".to_string()]),
            None
        );
    }

    #[test]
    fn rfc_bug_spec_repo_file_and_url_are_silent() {
        let allowed = vec!["src/foo.rs".to_string()];
        let cases = [
            "implements RFC 9110 semantics",
            "see #3421 for the bug",
            "per the spec section 4.2",
            "per the AGENTS.md instructions",
            "see src/agent/loop.rs",
            "fetches https://api.example.com/pricing at runtime",
            "checked in the parser",
            "as of now, this is stable",
        ];
        for c in cases {
            let diff = diff_with(c);
            let hits = added_sourcing_comments(&diff, None, &allowed);
            assert!(
                hits.is_empty(),
                "must stay silent for a normal comment, got {hits:?}: {c}"
            );
        }
    }

    #[test]
    fn plain_code_comments_are_silent() {
        let allowed = vec!["src/foo.rs".to_string()];
        let cases = [
            "increment the counter",
            "TODO: fix the leak",
            "the parser owns the buffer",
            "handle the interrupt",
        ];
        for c in cases {
            let diff = diff_with(c);
            let hits = added_sourcing_comments(&diff, None, &allowed);
            assert!(
                hits.is_empty(),
                "plain comment must be silent: {c} → {hits:?}"
            );
        }
    }

    #[test]
    fn pre_existing_comment_in_baseline_is_silent() {
        // The comment was already in the working tree when the run started
        // (uncommitted WIP): it appears in the run-start baseline diff, so
        // subtracting the baseline removes it and the gate stays silent.
        let allowed = vec!["src/foo.rs".to_string()];
        let diff = diff_with("per the OpenAI pricing page");
        let hits = added_sourcing_comments(&diff, Some(&diff), &allowed);
        assert!(
            hits.is_empty(),
            "pre-existing comment must not fire: {hits:?}"
        );
    }

    #[test]
    fn comment_in_untouched_file_is_silent() {
        // The diff contains a sourcing comment, but in a file the run did
        // not touch — scoping to the run's files excludes it entirely.
        let allowed = vec!["src/other.rs".to_string()];
        let hits = added_sourcing_comments(FABRICATED, None, &allowed);
        assert!(hits.is_empty(), "untouched file must be excluded: {hits:?}");
    }

    #[test]
    fn off_mode_never_fires() {
        assert_eq!(source_nudge_cap(GateMode::Off), 0);
        assert_eq!(source_nudge_cap(GateMode::Advisory), 1);
        assert!(source_nudge_cap(GateMode::Blocking) > source_nudge_cap(GateMode::Advisory));
    }

    #[test]
    fn deleted_file_and_binary_diffs_are_tolerated() {
        let diff = r#"diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-// per the pricing page
diff --git a/img.png b/img.png
Binary files differ
"#;
        let hits = added_sourcing_comments(diff, None, &["gone.rs".to_string()]);
        assert!(
            hits.is_empty(),
            "deleted/binary diffs have no added comments"
        );
    }
}

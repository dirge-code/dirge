//! Unified-diff compressor: keep the files and hunks that changed the most (and the
//! query-relevant ones), trim surrounding context, and stash the rest.
//!
//! A review diff is mostly unchanged context around a few edits. This parses the diff
//! into files and hunks, caps both (heaviest-changed first, plus the first and last so
//! structure survives), trims context lines to a few around each change, and replaces
//! dropped files/hunks/context with retrievable markers. File and hunk headers are kept
//! verbatim so the result still reads as a diff.

use std::collections::HashSet;

use super::{Ctx, Mode, attributed, elide, elide_into, fill_by_score, pick_mode, query_bonus};

/// Most files to keep (by total changed lines).
// dirge: raised from upstream's 20 so a typical multi-file diff (<=200 files,
// <=MAX_HUNKS hunks each) keeps every changed file. Beyond that, or for a file
// with more than MAX_HUNKS hunks, the least-changed files/hunks are still
// dropped to elision markers and the agent must re-run the tool to recover them.
const MAX_FILES: usize = 200;
/// Most hunks to keep per file.
const MAX_HUNKS: usize = 10;
/// Context lines kept on each side of a change inside a kept hunk.
const MAX_CONTEXT: usize = 3;

struct FileDiff<'a> {
    header: Vec<&'a str>,
    hunks: Vec<Hunk<'a>>,
}

struct Hunk<'a> {
    header: &'a str,
    body: Vec<&'a str>,
}

impl Hunk<'_> {
    fn changes(&self) -> usize {
        self.body.iter().filter(|l| is_change(l)).count()
    }
}

impl FileDiff<'_> {
    fn changes(&self) -> usize {
        self.hunks.iter().map(Hunk::changes).sum()
    }

    /// Every line of the file section in order (header, then each hunk's header + body).
    /// The elision marker counts these, so a dropped file reports its true line count
    /// rather than `1` (the bug from passing a single joined string to [`elide`]).
    fn lines(&self) -> Vec<&str> {
        let mut lines: Vec<&str> = self.header.clone();
        for h in &self.hunks {
            lines.push(h.header);
            lines.extend(&h.body);
        }
        lines
    }
}

/// A hunk body line that adds or removes (not context, not a `\ No newline` marker).
fn is_change(line: &str) -> bool {
    (line.starts_with('+') || line.starts_with('-')) && !line.starts_with("\\")
}

/// Compress a unified diff. Returns `None` when parsing finds no files or nothing is
/// dropped/trimmed.
pub fn compress(text: &str, ctx: &Ctx, query: &HashSet<String>) -> Option<String> {
    let files = parse(text);
    if files.is_empty() {
        return None;
    }

    // Signal = changed lines; a diff that's mostly *context* goes aggressive — no
    // surrounding context kept, only the `+`/`-` lines. Total counts droppable body
    // lines only (file/hunk headers are never dropped, so including them would
    // over-state the noise and trip aggressive on diffs that have little context to cut).
    let total: usize = files
        .iter()
        .flat_map(|f| &f.hunks)
        .map(|h| h.body.len())
        .sum();
    let changes: usize = files.iter().map(FileDiff::changes).sum();
    let max_context = match pick_mode(ctx.mode, total, changes) {
        Mode::Aggressive => 0,
        Mode::Adaptive => MAX_CONTEXT,
    };

    // File cap: keep the heaviest-changed MAX_FILES (order-stable). Everything else is
    // dropped to a single stashed reference under its own header.
    let keep_file = cap_by_score(
        &files.iter().map(|f| f.changes() as f64).collect::<Vec<_>>(),
        MAX_FILES,
    );

    let mut out: Vec<String> = Vec::new();
    let mut changed = false;
    for (fi, file) in files.iter().enumerate() {
        if !keep_file[fi] {
            out.push(file.header.first().copied().unwrap_or_default().to_string());
            // Count the file's real lines (matches the hunk-drop path below), not `1`
            // from passing a single joined string to `elide`.
            out.push(elide(&file.lines()));
            changed = true;
            continue;
        }
        out.extend(file.header.iter().map(|l| l.to_string()));

        // Hunk cap within the file: keep first + last + heaviest, by changes plus query
        // overlap so a small but relevant hunk isn't crowded out.
        let scores: Vec<f64> = file
            .hunks
            .iter()
            .map(|h| {
                let q = h
                    .body
                    .iter()
                    .map(|l| query_bonus(l, query))
                    .fold(0.0, f64::max);
                (h.changes() as f64 * 0.05).min(0.5) + q
            })
            .collect();
        let keep_hunk = cap_by_score(&scores, MAX_HUNKS);

        for (hi, hunk) in file.hunks.iter().enumerate() {
            if keep_hunk[hi] {
                out.push(hunk.header.to_string());
                let (trimmed, trimmed_any) = trim_context(&hunk.body, max_context);
                out.extend(trimmed);
                changed |= trimmed_any;
            } else {
                let mut lines = vec![hunk.header];
                lines.extend(&hunk.body);
                out.push(elide(&lines));
                changed = true;
            }
        }
    }

    // Attribution rail: a windowed diff opens with the recovery header (no-op when the
    // never-inflate rail ended up keeping everything it tried to elide).
    changed.then(|| attributed(out, text.lines().count(), ctx))
}

/// Keep slots scoring highest, always including the first and last (so structure
/// brackets the kept set). All slots kept when there are no more than `max`.
fn cap_by_score(scores: &[f64], max: usize) -> Vec<bool> {
    let n = scores.len();
    if n <= max {
        return vec![true; n];
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    fill_by_score(&mut keep, scores, max);
    keep
}

/// Within a kept hunk, keep every change line and at most [`MAX_CONTEXT`] context lines
/// on each side of a change; collapse longer interior context runs into one marker.
/// Returns the rebuilt body and whether anything was trimmed.
fn trim_context(body: &[&str], max_context: usize) -> (Vec<String>, bool) {
    let n = body.len();
    let change: Vec<bool> = body.iter().map(|l| is_change(l)).collect();
    // Distance from each line to the nearest change (forward + backward sweeps).
    let mut dist = vec![usize::MAX; n];
    let mut last = usize::MAX;
    for i in 0..n {
        if change[i] {
            last = 0;
        } else if last != usize::MAX {
            last += 1;
        }
        dist[i] = last;
    }
    let mut next = usize::MAX;
    for i in (0..n).rev() {
        if change[i] {
            next = 0;
        } else if next != usize::MAX {
            next += 1;
        }
        dist[i] = dist[i].min(next);
    }
    let keep: Vec<bool> = (0..n)
        .map(|i| change[i] || dist[i] <= max_context)
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut trimmed = false;
    let mut i = 0;
    while i < n {
        if keep[i] {
            out.push(body[i].to_string());
            i += 1;
        } else {
            let start = i;
            while i < n && !keep[i] {
                i += 1;
            }
            // Never-inflate rail: a tiny context run stays inline instead of a marker.
            trimmed |= elide_into(&body[start..i], &mut out);
        }
    }
    (out, trimmed)
}

/// Split a unified diff into files and hunks. `diff --git` delimits files when present;
/// otherwise a `--- ` line starts each file (plain `diff -u` output).
fn parse(text: &str) -> Vec<FileDiff<'_>> {
    let git_mode = text.starts_with("diff --git ") || text.contains("\ndiff --git ");
    let lines: Vec<&str> = text.lines().collect();
    // In non-git output a `--- ` line opens a file *only* when the next line is its
    // `+++ ` mate. Without that pairing, a removed source line that itself begins with
    // `-- ` (an SQL/Lua/Haskell comment, prefixed by the diff's own `-` → `--- `) would
    // be misread as a new file header, truncating the hunk. `diff --git` mode keys off
    // the unambiguous `diff --git ` line, so no lookahead is needed there.
    let is_file_start = |i: usize| {
        let line = lines[i];
        if git_mode {
            line.starts_with("diff --git ")
        } else {
            line.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))
        }
    };

    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    for (i, &line) in lines.iter().enumerate() {
        if is_file_start(i) {
            if let Some(f) = cur.take() {
                files.push(f);
            }
            cur = Some(FileDiff {
                header: vec![line],
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(f) = cur.as_mut() else {
            continue; // preamble before the first file header
        };
        if line.starts_with("@@") {
            f.hunks.push(Hunk {
                header: line,
                body: Vec::new(),
            });
        } else if let Some(h) = f.hunks.last_mut() {
            h.body.push(line);
        } else {
            f.header.push(line);
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llmtrim::stages::toolout::{ModeSetting, test_ctx};

    /// One file whose single hunk has a long context tail around one change.
    fn padded_diff() -> String {
        let mut lines = vec![
            "diff --git a/big.rs b/big.rs".to_string(),
            "--- a/big.rs".to_string(),
            "+++ b/big.rs".to_string(),
            "@@ -1,30 +1,30 @@".to_string(),
            "-let removed = 0;".to_string(),
            "+let added = 1;".to_string(),
        ];
        for i in 0..25 {
            lines.push(format!(" unchanged context line {i}"));
        }
        lines.join("\n")
    }

    #[test]
    fn trims_far_context_keeps_changes() {
        let diff = padded_diff();
        let out = compress(&diff, &test_ctx(), &HashSet::new()).expect("compresses");

        assert!(out.contains("-let removed = 0;"), "removal kept");
        assert!(out.contains("+let added = 1;"), "addition kept");
        assert!(out.contains("@@ -1,30 +1,30 @@"), "hunk header kept");
        assert!(
            out.contains(" unchanged context line 0"),
            "near context kept"
        );
        assert!(!out.contains("context line 24"), "far context trimmed");
        assert!(
            out.contains("omitted"),
            "trimmed context elided by position"
        );
    }

    #[test]
    fn drops_least_changed_files_over_cap() {
        // One heavily-changed file plus many one-line files: the cap keeps the heavy one
        // and at least one tail file, dropping the rest to elision markers.
        let mut sections = vec![format!(
            "diff --git a/hot.rs b/hot.rs\n--- a/hot.rs\n+++ b/hot.rs\n@@ -1,5 +1,5 @@\n{}",
            (0..5)
                .map(|i| format!("-old{i}\n+new{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        )];
        for i in 0..MAX_FILES + 5 {
            sections.push(format!(
                "diff --git a/f{i}.rs b/f{i}.rs\n--- a/f{i}.rs\n+++ b/f{i}.rs\n@@ -1 +1 @@\n-a\n+b"
            ));
        }
        let diff = sections.join("\n");
        let out = compress(&diff, &test_ctx(), &HashSet::new()).expect("compresses");

        assert!(out.contains("a/hot.rs"), "the heavily-changed file is kept");
        // A kept file renders its full `+++ b/…` header; a dropped one renders only its
        // `diff --git` line plus an elision. So full headers are capped and elisions appear.
        assert!(
            out.matches("+++ b/").count() <= MAX_FILES,
            "rendered files are capped at MAX_FILES"
        );
        assert!(
            out.contains("omitted"),
            "dropped files became elision markers"
        );
    }

    #[test]
    fn dropped_file_marker_reports_real_line_count() {
        // One hot file plus many cold ones. A dropped cold file must report its true line
        // count in the elision marker, not "1 lines omitted" (the bug: a single joined
        // string was passed to `elide`, so its slice length was always 1).
        let mut sections = vec![format!(
            "diff --git a/hot.rs b/hot.rs\n--- a/hot.rs\n+++ b/hot.rs\n@@ -1,6 +1,6 @@\n{}",
            (0..6)
                .map(|i| format!("-old{i}\n+new{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        )];
        // Each cold file: `diff --git`, `---`, `+++`, `@@`, `-a`, `+b` = 6 lines.
        for i in 0..MAX_FILES + 5 {
            sections.push(format!(
                "diff --git a/c{i}.rs b/c{i}.rs\n--- a/c{i}.rs\n+++ b/c{i}.rs\n@@ -1 +1 @@\n-a\n+b"
            ));
        }
        let diff = sections.join("\n");
        let out = compress(&diff, &test_ctx(), &HashSet::new()).expect("compresses");

        assert!(
            out.contains("[… 6 lines omitted …]"),
            "dropped file reports its 6 lines: {out}"
        );
        assert!(
            !out.contains("[… 1 lines omitted …]"),
            "no bogus 1-line marker"
        );
    }

    #[test]
    fn removed_sql_comment_line_is_not_a_file_header() {
        // Non-git `diff -u`. A removed source line `-- drop the column` (SQL/Lua/Haskell
        // comment) renders as `--- drop the column` after the diff's own `-` prefix. It
        // must be parsed as hunk body, not mistaken for a `--- a/file` header (which
        // would split a spurious second file and truncate this hunk).
        let diff = "--- a/schema.sql\n\
                    +++ b/schema.sql\n\
                    @@ -1,4 +1,4 @@\n\
                     CREATE TABLE t (\n\
                    --- drop the column\n\
                    +-- keep the column\n\
                       id INT\n\
                     );";
        let files = parse(diff);
        assert_eq!(
            files.len(),
            1,
            "one file, the SQL comment did not start a second"
        );
        let body = &files[0].hunks[0].body;
        assert!(
            body.contains(&"--- drop the column"),
            "the removed `-- ` comment stayed in the hunk body: {body:?}"
        );
    }

    #[test]
    fn small_diff_is_left_alone() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new";
        assert_eq!(compress(diff, &test_ctx(), &HashSet::new()), None);
    }

    #[test]
    fn aggressive_drops_all_context() {
        // Aggressive sets the context budget to 0, so even the near context the adaptive
        // path keeps is dropped — only the changed lines and headers remain.
        let diff = padded_diff();
        let ctx = Ctx {
            max_lines: 30,
            template: true,
            mode: ModeSetting::Aggressive,
            ..test_ctx()
        };
        let out = compress(&diff, &ctx, &HashSet::new()).expect("compresses");
        assert!(out.contains("-let removed = 0;"), "removal kept");
        assert!(out.contains("+let added = 1;"), "addition kept");
        assert!(out.contains("@@ -1,30 +1,30 @@"), "hunk header kept");
        assert!(
            !out.contains(" unchanged context line 0"),
            "all context dropped"
        );
    }
}

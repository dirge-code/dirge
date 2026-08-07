//! Tool-output kind detection — shape-based, cheap, zero-model.
//!
//! Each candidate segment is classified by structural shape only (no keywords from the
//! user's language). Diff wins first (unambiguous `@@`/`--- ` markers), then grep
//! (`path:line:` records), then log (a meaningful share of lines carrying a level or
//! failure signal). Anything else returns `None` and is left for the prose stages.

use once_cell::sync::Lazy;
use regex::Regex;

/// The tool-output shapes this stage compresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutKind {
    Log,
    Diff,
    Grep,
}

/// A grep / ripgrep record: `path:line:` or `path:line:col:`. The path field must hold
/// a path-ish character (letter, `.`, `/`, `\`) so a bare `12:34:56` clock — purely
/// numeric before the colon — is not mistaken for a match. An optional leading drive
/// letter (`C:`) is allowed for Windows paths.
static GREP_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:[A-Za-z]:)?[^:\n]*[A-Za-z./\\][^:\n]*:\d+:").unwrap());

/// Minimum non-empty lines for the line-oriented kinds (grep, log).
const MIN_LINES: usize = 3;
/// Minimum non-empty lines before a segment is considered for log windowing.
const MIN_LOG_LINES: usize = 8;

/// dirge's `read` tool opens every excerpt with `(N lines total, showing lines A-B)`
/// (or `(≥N lines total, …)` when the line count is a lower bound). Unambiguous, so it
/// alone marks the segment a file excerpt. Matched within the first few lines rather
/// than at position 0 because a relational-default note or an injection-guard wrapper
/// can precede it.
static EXCERPT_HEADER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\(\s*(?:≥)?\d+ lines total[,)]").unwrap());

/// How far in to look for [`EXCERPT_HEADER`] — past any note / `<untrusted-file>` wrapper.
const HEADER_LOOKAHEAD: usize = 8;

/// A `read`-style line-number prefix: `  42: ` or, with `line_hashes`, `  42 a3f: `.
static NUMBER_PREFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*\d+(?: [0-9a-f]{2,4})?: ").unwrap());

/// A leading declaration keyword — the arm that recognizes indentation-delimited
/// languages (Python, YAML-ish config) that the trailing-punctuation arm misses.
static CODE_KEYWORD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(?:pub |priv(?:ate)? |protected |public |static |async |export |default |final |abstract )*\
(?:fn|def|class|struct|enum|impl|trait|interface|function|const|let|var|type|use|import|from|package|namespace|module|return|if|elif|else|for|while|match|switch|case|try|catch|except|finally|with|yield|await|uniform|varying|attribute|layout|precision|void|int|float|bool|vec[234]|mat[234]|#include|#define|#pragma|@\w+)\b",
    )
    .unwrap()
});

/// Fraction of lines that must read as code before a segment counts as source.
const CODE_RATIO_PCT: usize = 60;

/// Is this segment source code or a file excerpt — content the agent reads to edit,
/// not machine noise to window?
///
/// The tool-output kinds win first: a grep dump's `src/x.rs:10:    let x = 1;` ends
/// like code and a stack trace opens with keywords, so without the short-circuit this
/// would exempt the very shapes the stage exists to compress.
pub fn is_code(text: &str) -> bool {
    if detect(text).is_some() {
        return false;
    }
    is_file_excerpt(text) || is_source(text)
}

/// A `read`-tool excerpt, identified by its own header.
fn is_file_excerpt(text: &str) -> bool {
    text.lines()
        .take(HEADER_LOOKAHEAD)
        .any(|l| EXCERPT_HEADER.is_match(l.trim_start()))
}

/// Source code without a `read` header — a paste, or another tool's file output.
/// Strips any line-number prefix first so numbered and raw excerpts score the same.
///
/// Bails as soon as the verdict is settled: this runs over every candidate segment on
/// every request, and a long log would otherwise pay a regex pass per line to learn
/// what the first few hundred already decided.
fn is_source(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < MIN_LINES {
        return false;
    }
    let need = lines.len() * CODE_RATIO_PCT;
    let (mut code, mut seen) = (0usize, 0usize);
    for l in &lines {
        seen += 1;
        let bare = NUMBER_PREFIX.replace(l, "");
        let t = bare.trim();
        // `>` covers JSX/TSX, HTML and XML, whose rows end in `>` or `/>` and match
        // none of the statement punctuation or keywords (GH #755).
        if matches!(
            t.chars().next_back(),
            Some('{' | '}' | ';' | ')' | ':' | '>')
        ) || t.starts_with("//")
            || t.starts_with("/*")
            || t.starts_with("* ")
            || CODE_KEYWORD.is_match(t)
        {
            code += 1;
            if code * 100 >= need {
                return true;
            }
        } else if (code + lines.len() - seen) * 100 < need {
            return false; // even every remaining line counting can't reach the ratio
        }
    }
    code * 100 >= need
}

/// Classify a tool-output segment, or `None` if it is not a shape this stage handles.
pub fn detect(text: &str) -> Option<OutKind> {
    if is_diff(text.trim_start()) {
        return Some(OutKind::Diff);
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < MIN_LINES {
        return None;
    }
    if is_grep(&lines) {
        return Some(OutKind::Grep);
    }
    if is_log(&lines) {
        return Some(OutKind::Log);
    }
    None
}

/// A unified diff: an explicit `diff --git` header, or a `--- `/`+++ ` file header
/// paired with at least one `@@` hunk header.
fn is_diff(t: &str) -> bool {
    if t.starts_with("diff --git ") {
        return true;
    }
    let has_hunk = t.starts_with("@@ ") || t.contains("\n@@ ");
    let has_file = t.starts_with("--- ") || t.contains("\n--- ") || t.contains("\n+++ ");
    has_hunk && has_file
}

/// At least three records and ≥75% of non-empty lines are `path:line:` matches.
fn is_grep(lines: &[&str]) -> bool {
    let matches = lines.iter().filter(|l| GREP_LINE.is_match(l)).count();
    matches >= MIN_LINES && matches * 4 >= lines.len() * 3
}

/// Log-shaped: enough lines, and either ≥30% of lines carrying any level token, or
/// failure lines dense enough for the segment's length (two outright failure lines is
/// only enough on short segments — ≥10% of lines must be failures on longer ones).
/// The density requirement keeps long prose that merely *mentions* failure a couple of
/// times (e.g. instructions about error handling) out of errors-only windowing, while a
/// real long log still qualifies via the level-token share.
fn is_log(lines: &[&str]) -> bool {
    if lines.len() < MIN_LOG_LINES {
        return false;
    }
    let level = lines
        .iter()
        .filter(|l| super::signals::LEVEL.is_match(l))
        .count();
    if level * 100 >= lines.len() * 30 {
        return true;
    }
    let strong = lines
        .iter()
        .filter(|l| super::signals::STRONG.is_match(l))
        .count();
    strong >= 2 && strong * 10 >= lines.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_git_diff() {
        let d = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new";
        assert_eq!(detect(d), Some(OutKind::Diff));
    }

    #[test]
    fn detects_plain_unified_diff() {
        let d = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n line\n-old\n+new";
        assert_eq!(detect(d), Some(OutKind::Diff));
    }

    #[test]
    fn detects_grep_output() {
        let g = "src/main.rs:10:    let x = 1;\n\
                 src/main.rs:42:    foo(x);\n\
                 src/lib.rs:7:pub fn foo() {}";
        assert_eq!(detect(g), Some(OutKind::Grep));
    }

    #[test]
    fn clock_times_are_not_grep() {
        // Numeric-only field before the colon must not read as a path:line record.
        let log = "12:00:01 service started ok\n\
                   12:00:02 handling request fine\n\
                   12:00:03 all good here now";
        assert_ne!(detect(log), Some(OutKind::Grep));
    }

    #[test]
    fn detects_log_with_failures() {
        let log = "INFO  build started\n\
                   INFO  compiling module a\n\
                   INFO  compiling module b\n\
                   ERROR failed to resolve symbol foo\n\
                   INFO  compiling module c\n\
                   ERROR type mismatch in bar\n\
                   INFO  compiling module d\n\
                   INFO  done with warnings";
        assert_eq!(detect(log), Some(OutKind::Log));
    }

    #[test]
    fn long_prose_mentioning_failures_is_not_log() {
        // Regression: a long prose instruction segment where only two lines mention
        // failure keywords must not be windowed as a log (live capture: a 106-line
        // conversation-compaction prompt was gutted to errors-only).
        let prose: Vec<String> = (0..104)
            .map(|i| format!("Step {i}: describe the section thoroughly, capturing every detail of the request in flowing prose."))
            .chain([
                "Errors and fixes: list all errors that you ran into, and how you fixed them.".to_string(),
                "Tool calls will be rejected and you will fail the task entirely.".to_string(),
            ])
            .collect();
        assert_eq!(detect(&prose.join("\n")), None);
    }

    #[test]
    fn real_compaction_prompt_is_not_log() {
        let t = include_str!("../../fixtures/compaction_prompt.txt");
        assert_eq!(detect(t), None);
    }

    #[test]
    fn long_level_heavy_log_with_few_errors_still_detects() {
        // 100 INFO lines + 2 ERROR lines: low strong density, but every line carries a
        // level token, so the level-share arm keeps it a log.
        let mut lines: Vec<String> = (0..100)
            .map(|i| format!("INFO  compiling module {i}"))
            .collect();
        lines.push("ERROR failed to resolve symbol foo".to_string());
        lines.push("ERROR type mismatch in bar".to_string());
        assert_eq!(detect(&lines.join("\n")), Some(OutKind::Log));
    }

    #[test]
    fn plain_prose_is_not_tool_output() {
        let prose = "The quarterly report covers revenue and costs.\n\
                     Margins improved across every region this year.\n\
                     The board approved the new budget unanimously.";
        assert_eq!(detect(prose), None);
    }

    /// dirge's `read` tool numbers every line (`  42: …`, or `  42 a3f: …` with
    /// `line_hashes`). That prefix is the strong, cheap signal that a segment is a
    /// file excerpt the agent is about to edit — not machine noise to window.
    #[test]
    fn detects_numbered_file_excerpts() {
        let plain = "(3 lines total, showing lines 1-3)\n\n\
                     1: fn main() {\n\
                     2:     println!(\"hi\");\n\
                     3: }";
        assert!(is_code(plain), "plain numbering");

        let hashed = "(3 lines total, showing lines 1-3)\n\n\
                      1 a3f: fn main() {\n\
                      2 9c1:     println!(\"hi\");\n\
                      3 04e: }";
        assert!(is_code(hashed), "hash-anchored numbering");
    }

    /// Padded numbering on a wide file, and an excerpt that starts mid-file (the
    /// `offset` path), must read the same way.
    #[test]
    fn detects_padded_and_offset_excerpts() {
        let body: String = (100..160)
            .map(|i| format!("{i:>4}:     let x = compute({i});"))
            .collect::<Vec<_>>()
            .join("\n");
        let excerpt = format!("(1202 lines total, showing lines 100-159)\n\n{body}");
        assert!(is_code(&excerpt));
    }

    /// The shapes this stage exists to compress must not be mistaken for code, or
    /// the fix would silently disable tool-output windowing.
    #[test]
    fn logs_grep_and_diffs_are_not_code() {
        let log = "INFO  build started\n\
                   INFO  compiling module a\n\
                   ERROR failed to resolve symbol foo\n\
                   INFO  compiling module b\n\
                   ERROR type mismatch in bar\n\
                   INFO  compiling module c\n\
                   INFO  compiling module d\n\
                   INFO  done with warnings";
        assert!(!is_code(log), "log");

        let grep = "src/main.rs:10:    let x = 1;\n\
                    src/main.rs:42:    foo(x);\n\
                    src/lib.rs:7:pub fn foo() {}";
        assert!(!is_code(grep), "grep");

        let diff = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new";
        assert!(!is_code(diff), "diff");

        let prose = "The quarterly report covers revenue and costs.\n\
                     Margins improved across every region this year.\n\
                     The board approved the new budget unanimously.";
        assert!(!is_code(prose), "prose");
    }

    /// GH #755 reports this from the JSX angle. A JSX/TSX body is nearly all tag rows
    /// ending in `>` or `/>` — no statement punctuation, no leading keyword — so the
    /// generic source arm has to recognize `>` or a paste of a component reads as
    /// prose and gets windowed.
    #[test]
    fn detects_jsx_without_a_read_header() {
        let jsx = "export function Panel({ items }) {\n\
                     return (\n\
                     <div className=\"panel\">\n\
                       <Header title=\"Items\" />\n\
                       <ul className=\"list\">\n\
                         {items.map((it) => (\n\
                           <li key={it.id} className=\"row\">\n\
                             <Badge tone={it.tone} />\n\
                             <span className=\"label\">{it.label}</span>\n\
                           </li>\n\
                         ))}\n\
                       </ul>\n\
                     </div>\n\
                   );\n\
                   }";
        assert!(is_code(jsx));
    }

    /// A timestamped log whose lines happen to open with digits and a colon must not
    /// read as a numbered excerpt: the numbers have to *ascend by one*.
    #[test]
    fn timestamped_logs_are_not_numbered_excerpts() {
        let log = (0..40)
            .map(|i| format!("1200{i:02}: service heartbeat ok"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!is_code(&log));
    }
}

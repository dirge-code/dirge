//! Grep / ripgrep compressor: lossless template fold first, then query-relevant
//! windowing with a per-file floor.
//!
//! Search dumps list `path:line:content` records. Repetitive match sets (the same call
//! pattern across a file) fold losslessly via [`template::collapse_global`] — every
//! path, line number and argument survives in `[×N: …]` tuples — which on enumeration
//! queries ("where is X called?") preserves the *entire* answer at a fraction of the
//! tokens. Only when the folded dump still exceeds the budget is it windowed: matches
//! are scored by query overlap, fold representatives are never droppable (each carries
//! ×N members), and the first match of every file is force-kept so no file vanishes —
//! a keep-more floor, not a drop rule. Dropped runs become positional elision markers
//! under the shared attribution header.
//!
//! There is deliberately no aggressive ("one match per file") mode: grep matches carry
//! no intrinsic severity signal, so all relevance ties — and selection without a signal
//! destroys exactly the enumeration answers agents grep for.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;

use super::{Ctx, FORCE_PRIORITY, MIN_KEEP, query_bonus, rebuild, select_keep, template};
use crate::llmtrim::stages::sizing::optimal_keep;

/// Captures the file field of a `path:line:` record (same shape as the detector, with
/// a capture group).
static GREP_REC: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^((?:[A-Za-z]:)?[^:\n]*[A-Za-z./\\][^:\n]*):\d+:").unwrap());

fn file_of(line: &str) -> Option<&str> {
    GREP_REC
        .captures(line)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// Compress a grep segment. Returns `None` when nothing changed (already small and
/// nothing folded).
pub fn compress(text: &str, ctx: &Ctx, query: &HashSet<String>) -> Option<String> {
    // Lossless first: fold same-template match runs. Information-preserving, so it
    // needs no relevance signal and can never destroy an enumeration answer.
    let (collapsed, folded) = if ctx.template {
        template::collapse_global(text)
    } else {
        (text.to_string(), false)
    };
    let lines: Vec<&str> = collapsed.lines().collect();
    if lines.len() <= ctx.max_lines {
        return folded.then_some(collapsed);
    }

    // Still over budget: window by query relevance under the adaptive budget. A fold
    // representative compresses ×N matches already — dropping it would lose them all,
    // so it is force-kept. The first match of every file is force-kept too (floor).
    let files: Vec<Option<&str>> = lines.iter().map(|l| file_of(l)).collect();
    let mut first_in_file = vec![false; lines.len()];
    let mut seen: HashSet<&str> = HashSet::new();
    for (i, f) in files.iter().enumerate() {
        if let Some(f) = f
            && seen.insert(f)
        {
            first_in_file[i] = true;
        }
    }
    let scores: Vec<f64> = lines
        .iter()
        .map(|l| {
            if l.contains("[×") {
                FORCE_PRIORITY
            } else {
                0.2 + query_bonus(l, query)
            }
        })
        .collect();
    let k = optimal_keep(&lines, MIN_KEEP, ctx.max_lines);
    let mut keep = select_keep(&scores, k, FORCE_PRIORITY);
    for (slot, &first) in keep.iter_mut().zip(&first_in_file) {
        *slot |= first;
    }

    if keep.iter().all(|&x| x) {
        return folded.then_some(collapsed);
    }
    Some(rebuild(&lines, &keep, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llmtrim::stages::toolout::{ModeSetting, test_ctx};

    /// A unique, digit-free word per index (`aa`, `ab`, …) so no two lines share a
    /// template even after value masking.
    fn uniq_word(i: usize) -> String {
        let a = (b'a' + (i / 26) as u8) as char;
        let b = (b'a' + (i % 26) as u8) as char;
        format!("{a}{b}")
    }

    /// 81 matches whose content is unique per line — no token position holds a
    /// dominant constant, so nothing folds and the windowing path is exercised.
    fn distinct_dump() -> String {
        let mut lines = Vec::new();
        for i in 0..40 {
            let w = uniq_word(i);
            lines.push(format!("src/a.rs:{}:{w}_{w} {w}_input {w}_handler", i + 1));
        }
        for i in 0..40 {
            let w = uniq_word(i + 40);
            lines.push(format!("src/b.rs:{}:{w}_{w} {w}_thing {w}_caller", i + 1));
        }
        lines.push("src/target.rs:5:let widget = build();".to_string());
        lines.join("\n")
    }

    /// 81 matches sharing one template per file — the enumeration shape ("where is X
    /// called?") that must fold losslessly.
    fn repetitive_dump() -> String {
        let mut lines = Vec::new();
        for i in 0..40 {
            lines.push(format!("src/a.rs:{}:    let v = step({i});", i + 1));
        }
        for i in 0..40 {
            lines.push(format!("src/b.rs:{}:    helper({i});", i + 1));
        }
        lines.push("src/target.rs:5:    let widget = build();".to_string());
        lines.join("\n")
    }

    #[test]
    fn repetitive_matches_fold_losslessly() {
        // The enumeration case: every match is the answer. The fold must preserve all
        // 81 of them — every line number survives, nothing is elided.
        let dump = repetitive_dump();
        let out = compress(&dump, &test_ctx(), &HashSet::new()).expect("folds");

        assert!(out.lines().count() <= 30, "far below budget: {out}");
        assert!(out.contains("[×40:"), "both 40-match groups folded");
        assert!(!out.contains("omitted"), "lossless — nothing elided");
        for file in ["src/a.rs:", "src/b.rs:", "src/target.rs:"] {
            assert!(out.contains(file), "{file} still represented");
        }
        // Regular columns range-fold (lossless: line numbers 1..40 step 1, args 0..39).
        assert!(
            out.contains("(1..40; 0..39)"),
            "both columns range-folded losslessly: {out}"
        );
    }

    #[test]
    fn windows_distinct_matches_and_keeps_every_file() {
        let dump = distinct_dump();
        let out = compress(&dump, &test_ctx(), &HashSet::new()).expect("compresses");

        assert!(out.lines().count() < dump.lines().count(), "fewer lines");
        for file in ["src/a.rs:", "src/b.rs:", "src/target.rs:"] {
            assert!(out.contains(file), "{file} still represented");
        }
        assert!(
            out.contains("omitted"),
            "dropped matches elided by position: {out}"
        );
        assert!(
            out.starts_with("[llmtrim compressed this tool output:"),
            "windowed output is attributed with the recovery header: {out}"
        );
    }

    #[test]
    fn query_relevant_match_survives() {
        let dump = distinct_dump();
        let query: HashSet<String> = ["widget".to_string()].into_iter().collect();
        let out = compress(&dump, &test_ctx(), &query).expect("compresses");
        assert!(out.contains("let widget = build()"), "query hit is kept");
    }

    #[test]
    fn small_dump_is_left_alone() {
        let dump = "src/a.rs:1:one\nsrc/a.rs:2:two\nsrc/b.rs:3:three";
        assert_eq!(compress(dump, &test_ctx(), &HashSet::new()), None);
    }

    #[test]
    fn aggressive_setting_no_longer_destroys_matches() {
        // Regression: `mode = aggressive` used to keep ONE match per file (80 → 2),
        // erasing enumeration answers; the agent then blamed the tool wrapper and
        // re-read whole files. Grep now ignores the aggressive split entirely: the
        // repetitive dump folds losslessly whatever the mode says.
        let dump = repetitive_dump();
        let ctx = Ctx {
            max_lines: 30,
            template: true,
            mode: ModeSetting::Aggressive,
            ..test_ctx()
        };
        let out = compress(&dump, &ctx, &HashSet::new()).expect("folds");
        assert!(
            out.contains("(1..40; 0..39)"),
            "aggressive mode must keep every match (range-folded): {out}"
        );
    }

    #[test]
    fn fold_representatives_survive_windowing() {
        // One foldable 40-match group buried in 80 distinct matches: the fold line
        // carries ×40 members and must never be dropped by the budget.
        let mut lines: Vec<String> = distinct_dump().lines().map(str::to_string).collect();
        for i in 0..40 {
            lines.push(format!("src/c.rs:{}:    retry({i});", i + 1));
        }
        let dump = lines.join("\n");
        let out = compress(&dump, &test_ctx(), &HashSet::new()).expect("compresses");
        assert!(
            out.contains("[×40:"),
            "fold representative force-kept through windowing: {out}"
        );
    }
}

//! Log compressor: lossless Drain template collapse, then level/stack-aware windowing.
//!
//! Build/test logs are mostly repetitive progress noise around a few real failures.
//! First [`template::collapse`] folds parametric line runs losslessly; then lines are
//! scored ([`super::priority`] + [`super::query_bonus`]) and only the budget's worth of
//! the highest-scoring lines survive — but every failure line is force-kept, and
//! dropped runs become positional elision markers.

use std::collections::HashSet;

use super::{
    Ctx, FORCE_PRIORITY, MIN_KEEP, Mode, WARN_PRIORITY, pick_mode, priority, query_bonus, rebuild,
    select_keep, template,
};
use crate::llmtrim::stages::sizing::optimal_keep;

/// Compress a log segment. Returns `None` when nothing changed (already small, no
/// templates to fold, nothing to window).
pub fn compress(text: &str, ctx: &Ctx, query: &HashSet<String>) -> Option<String> {
    // Signal = failure lines; the split goes aggressive when they're sparse in a big log.
    let raw: Vec<&str> = text.lines().collect();
    let errors = raw.iter().filter(|l| priority(l) >= FORCE_PRIORITY).count();
    // Errors-only is only safe when there *are* errors to keep. A big level-light dump
    // (e.g. a 60-line INFO-only status paste) has zero strong lines, so errors-only would
    // emit nothing but an elision marker and erase the whole thing — fall through to
    // adaptive windowing instead (head/tail + query hits survive).
    if errors >= 1 && pick_mode(ctx.mode, raw.len(), errors) == Mode::Aggressive {
        return compress_errors_only(text, ctx);
    }
    let (collapsed, folded) = if ctx.template {
        // Global collapse: consecutive runs *and* non-adjacent (interleaved parallel-build)
        // same-template lines, folded into the same `[×N: …]` representation.
        template::collapse_global(text)
    } else {
        (text.to_string(), false)
    };
    let lines: Vec<&str> = collapsed.lines().collect();

    // Below budget after template collapse: keep the (possibly shorter) collapsed form,
    // but only report a change if collapse actually folded a run (`folded`, not a string
    // compare — `join("\n")` strips a trailing newline and would falsely read as changed).
    if lines.len() <= ctx.max_lines {
        return folded.then_some(collapsed);
    }

    let scores: Vec<f64> = lines
        .iter()
        .map(|l| priority(l) + query_bonus(l, query))
        .collect();
    let k = optimal_keep(&lines, MIN_KEEP, ctx.max_lines);
    let keep = select_keep(&scores, k, FORCE_PRIORITY);

    // If selection kept everything (all lines were forced failures), the windowing was
    // a no-op; still surface the collapse if it shrank the text.
    if keep.iter().all(|&k| k) {
        return folded.then_some(collapsed);
    }
    Some(rebuild(&lines, &keep, ctx))
}

/// Errors-only mode: keep failure lines and their stack frames plus a count
/// summary, drop everything else to positional elision markers. Far more aggressive than
/// adaptive windowing. `None` when every line is a failure (nothing to strip).
fn compress_errors_only(text: &str, ctx: &Ctx) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let keep = aggressive_keep(&lines);
    if keep.iter().all(|&k| k) {
        return None;
    }
    Some(format!(
        "{}\n{}",
        summary_line(&lines),
        rebuild(&lines, &keep, ctx)
    ))
}

/// Keep only failure lines and the indented stack-trace frames immediately under them.
fn aggressive_keep(lines: &[&str]) -> Vec<bool> {
    let mut keep = vec![false; lines.len()];
    for i in 0..lines.len() {
        if priority(lines[i]) >= FORCE_PRIORITY {
            keep[i] = true;
        } else if (lines[i].starts_with(' ') || lines[i].starts_with('\t')) && i > 0 && keep[i - 1]
        {
            keep[i] = true; // stack-trace frame attached to the error kept above
        }
    }
    keep
}

/// One-line level census prepended to the errors-only output, so the model still sees
/// how much was dropped and at what severity.
fn summary_line(lines: &[&str]) -> String {
    let (mut errors, mut warnings) = (0usize, 0usize);
    for l in lines {
        let p = priority(l);
        if p >= FORCE_PRIORITY {
            errors += 1;
        } else if p >= WARN_PRIORITY {
            warnings += 1;
        }
    }
    format!(
        "[log: {} lines — {errors} error(s), {warnings} warning(s); errors-only below]",
        lines.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llmtrim::stages::toolout::{ModeSetting, test_ctx};

    fn noisy_log(infos: usize) -> String {
        let mut lines = vec!["INFO  build started".to_string()];
        for i in 0..infos {
            lines.push(format!("INFO  compiling module number {i}"));
        }
        lines.push("ERROR failed to resolve symbol `foo`".to_string());
        lines.push("ERROR type mismatch in `bar`".to_string());
        lines.push("INFO  build finished with errors".to_string());
        lines.join("\n")
    }

    #[test]
    fn template_collapse_folds_repetitive_noise_losslessly() {
        // The 60 progress lines differ only by a number → one template run. Collapse
        // handles them with no drop at all, and the errors are untouched.
        let log = noisy_log(60);
        let out = compress(&log, &test_ctx(), &HashSet::new()).expect("compresses");

        assert!(out.lines().count() < log.lines().count(), "log got shorter");
        assert!(out.contains("failed to resolve symbol"), "error 1 survives");
        assert!(out.contains("type mismatch"), "error 2 survives");
        assert!(
            out.contains("[×60:"),
            "repetitive run folded into one template line"
        );
        assert!(!out.contains("omitted"), "lossless fold drops nothing");
    }

    #[test]
    fn windows_non_templatable_noise() {
        // With template collapse off, the near-duplicate progress lines can't fold, so
        // the adaptive sizer windows them (it sees one near-dup cluster) and elides the
        // dropped run; the distinct error lines are force-kept.
        let log = noisy_log(60);
        let ctx = Ctx {
            max_lines: 30,
            template: false,
            mode: ModeSetting::Adaptive,
            ..test_ctx()
        };
        let out = compress(&log, &ctx, &HashSet::new()).expect("compresses");

        assert!(out.lines().count() < log.lines().count(), "log got shorter");
        assert!(out.contains("failed to resolve symbol"), "error 1 survives");
        assert!(out.contains("type mismatch"), "error 2 survives");
        assert!(out.contains("build started"), "head kept");
        assert!(out.contains("build finished"), "tail kept");
        assert!(
            out.contains("omitted"),
            "dropped noise is elided by position"
        );
    }

    #[test]
    fn small_log_is_left_alone() {
        let log = "INFO start\nINFO middle\nERROR boom\nINFO end";
        // 4 lines < budget and no templated run → no change.
        assert_eq!(compress(log, &test_ctx(), &HashSet::new()), None);
    }

    #[test]
    fn aggressive_mode_keeps_only_errors_and_summary() {
        // 100 INFO lines + 2 errors → errors-only output: a summary line, both errors,
        // and an elision for the dropped noise. Far smaller than adaptive windowing.
        let mut lines: Vec<String> = (0..100)
            .map(|i| format!("INFO  step {i} routine nominal pass"))
            .collect();
        lines.insert(50, "ERROR disk full on volume /dev/sda1".to_string());
        lines.push("ERROR flush failed: broken pipe".to_string());
        let log = lines.join("\n");

        let ctx = Ctx {
            max_lines: 40,
            template: true, // ignored in aggressive mode
            mode: ModeSetting::Aggressive,
            ..test_ctx()
        };
        let out = compress(&log, &ctx, &HashSet::new()).expect("compresses");

        assert!(
            out.starts_with("[log: 102 lines — 2 error(s)"),
            "summary header first: {out}"
        );
        assert!(
            out.contains("disk full on volume /dev/sda1"),
            "error 1 kept"
        );
        assert!(out.contains("flush failed: broken pipe"), "error 2 kept");
        assert!(!out.contains("routine nominal"), "all INFO noise dropped");
        assert!(out.contains("omitted"), "noise elided by position");
        // Errors-only is dramatically smaller than the adaptive cap would keep.
        assert!(
            out.lines().count() < 10,
            "errors-only collapses to a handful of lines"
        );
    }

    #[test]
    fn aggressive_with_zero_errors_does_not_erase_the_log() {
        // A 60-line INFO-only status dump (no failure tokens at all) pasted under
        // Aggressive. Errors-only would keep nothing and emit just an elision marker,
        // erasing the user's content. Instead it must fall back to adaptive windowing:
        // head/tail survive and the body is a real (non-empty) window, not a bare marker.
        let lines: Vec<String> = (0..60)
            .map(|i| format!("INFO service {i} state healthy uptime {i}h region eu-{i}"))
            .collect();
        let log = lines.join("\n");
        let head = &lines[0];
        let tail = &lines[59];

        let ctx = Ctx {
            max_lines: 30,
            template: false, // distinct lines anyway; force the windowing path, not a fold
            mode: ModeSetting::Aggressive,
            ..test_ctx()
        };
        let out = compress(&log, &ctx, &HashSet::new()).expect("windows rather than erasing");

        assert!(
            out.contains(head.as_str()),
            "head line survives, not erased: {out}"
        );
        assert!(
            out.contains(tail.as_str()),
            "tail line survives, not erased"
        );
        assert!(
            out.contains("omitted"),
            "dropped middle is elided by position"
        );
        // The whole thing was NOT reduced to a lone marker (the erase bug).
        assert!(
            out.lines().count() > 3,
            "kept real content, not just a marker: {out}"
        );
        assert!(
            out.lines().count() < log.lines().count(),
            "still compressed"
        );
    }

    #[test]
    fn query_relevant_lines_are_favored() {
        // A unique INFO line mentioning the query term should survive windowing even
        // though INFO normally scores low.
        let mut lines: Vec<String> = (0..60)
            .map(|i| format!("INFO  routine step {i} nominal"))
            .collect();
        lines.insert(
            30,
            "INFO  cache subsystem re=widget initialized".to_string(),
        );
        let log = lines.join("\n");

        let query: HashSet<String> = ["widget".to_string()].into_iter().collect();
        let out = compress(&log, &test_ctx(), &query).expect("compresses");
        assert!(out.contains("re=widget"), "query-relevant line is kept");
    }
}

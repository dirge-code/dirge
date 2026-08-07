//! Generic fallback for tool output that isn't log/diff/grep/JSON — closes "any tool".
//!
//! The redundancy signal is *template-foldability*: machine output is full of lines that
//! share a template and differ only in ids/counts (verbose dependency trees, status
//! dumps, repeated records), whereas prose lines are each unique. So the fallback runs
//! the lossless Drain collapse ([`template::collapse`]) first: if nothing folds, the
//! segment isn't template-repetitive (it's prose or already tight) and is declined — left
//! to the retrieve stage. If folding alone fits the budget, that lossless result ships.
//! Only when a folded-but-still-large segment remains does it window by line importance +
//! query overlap, eliding the rest by position (`[… N lines omitted …]`).
//!
//! Conservative by construction: the fold gate and the pipeline token gate both guard
//! against over-compressing, which is why this can run on unrecognized content with no
//! per-shape detector — and if the agent needs an elided detail, it re-runs the tool.

use std::collections::HashSet;

use super::{Ctx, FORCE_PRIORITY, MIN_KEEP, priority, query_bonus, rebuild, select_keep, template};
use crate::llmtrim::stages::sizing::optimal_keep;

/// Minimum lines before the fallback considers a segment.
const MIN_LINES: usize = 30;

/// Compress unrecognized but template-repetitive tool output. `None` unless the segment
/// is large and something actually folds (otherwise it's prose — not ours).
pub fn compress(text: &str, ctx: &Ctx, query: &HashSet<String>) -> Option<String> {
    if text.lines().count() < MIN_LINES {
        return None;
    }
    // Fold gate: lossless template collapse. If nothing folds, the lines are distinct
    // (prose / already tight) and we decline — that's the retrieve stage's job. Gate on
    // the `folded` flag, not `collapsed == text`: collapse rebuilds via `join("\n")`,
    // which strips a trailing newline, so a string compare reads "changed" for any prose
    // ending in '\n' even when nothing folded — defeating the decline.
    let (collapsed, folded) = template::collapse_global(text);
    if !folded {
        return None;
    }
    let lines: Vec<&str> = collapsed.lines().collect();
    if lines.len() <= ctx.max_lines {
        return Some(collapsed); // lossless fold alone fit the budget
    }

    // Still large after folding: window the folded lines by importance + query relevance,
    // eliding the rest.
    let scores: Vec<f64> = lines
        .iter()
        .map(|l| priority(l) + query_bonus(l, query))
        .collect();
    let k = optimal_keep(&lines, MIN_KEEP, ctx.max_lines);
    let keep = select_keep(&scores, k, FORCE_PRIORITY);
    if keep.iter().all(|&x| x) {
        return Some(collapsed);
    }
    Some(rebuild(&lines, &keep, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llmtrim::stages::toolout::test_ctx;

    /// 80 status lines on one template (differing only by an id) + one error — the shape
    /// of a verbose, unrecognized tool dump.
    fn repetitive_dump() -> String {
        let mut lines: Vec<String> = (0..80)
            .map(|i| format!("checking dependency package-{i} resolved ok cached"))
            .collect();
        lines.insert(
            40,
            "ERROR version conflict on left-pad needs manual resolution".to_string(),
        );
        lines.join("\n")
    }

    #[test]
    fn folds_repetitive_output_and_keeps_error() {
        let dump = repetitive_dump();
        let out = compress(&dump, &test_ctx(), &HashSet::new()).expect("fires");
        assert!(out.lines().count() < dump.lines().count(), "got shorter");
        assert!(
            out.contains("version conflict on left-pad"),
            "error survives"
        );
        // The runs collapse losslessly to template lines (no information dropped).
        assert!(out.contains("[×"), "repetitive runs folded");
    }

    #[test]
    fn declines_diverse_prose() {
        // Distinct sentences (no shared template — different content words per line) fold
        // to nothing, so the fallback declines and leaves them to retrieve.
        const W: &[&str] = &[
            "alpha", "bravo", "cobalt", "dune", "ember", "flint", "granite", "harbor", "ivory",
            "jade", "kelp", "lotus", "maple", "nectar", "opal", "pearl", "quartz", "rust", "slate",
            "topaz",
        ];
        let prose: String = (0..40)
            .map(|i| {
                format!(
                    "The {} review of {} examined {} across the {} division.",
                    W[i % W.len()],
                    W[(i + 3) % W.len()],
                    W[(i + 7) % W.len()],
                    W[(i + 11) % W.len()]
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(compress(&prose, &test_ctx(), &HashSet::new()), None);
    }

    #[test]
    fn declines_short_segments() {
        let short = (0..10)
            .map(|i| format!("line {i} same"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(compress(&short, &test_ctx(), &HashSet::new()), None);
    }

    /// Shape of the real defect (capture 1781203662905322-27117d, messages[34]): ~120
    /// mostly-indented HTML-ish lines where the structural indentation prior used to
    /// drown the query bonus, so the three ask-relevant middle lines were elided. With
    /// distinct-hit scoring (0.2/word, cap 0.6) they must outrank plain indented lines
    /// and survive a 30-line window.
    #[test]
    fn query_relevant_lines_beat_indentation_prior() {
        let mut lines: Vec<String> = (0..120)
            .map(|i| format!("    <td class=\"row-{i}\">status value pending</td>"))
            .collect();
        lines.insert(58, "      svg dark variant uses theme tokens".to_string());
        lines.insert(
            59,
            "      prefers-color-scheme light maps the svg palette".to_string(),
        );
        lines.insert(
            60,
            "      light theme fallback for the embedded svg".to_string(),
        );
        let dump = lines.join("\n");
        let query: HashSet<String> = ["svg".to_string(), "light".to_string(), "theme".to_string()]
            .into_iter()
            .collect();
        let out = compress(&dump, &test_ctx(), &query).expect("fires");
        assert!(
            out.lines().count() < dump.lines().count(),
            "windowing happened"
        );
        for needle in [
            "svg dark variant",
            "prefers-color-scheme light",
            "light theme fallback",
        ] {
            assert!(out.contains(needle), "ask-relevant line kept: {needle}");
        }
    }

    #[test]
    fn query_relevant_line_survives() {
        let mut lines: Vec<String> = (0..80)
            .map(|i| format!("scanning module-{i} clean nothing to report"))
            .collect();
        lines.insert(
            50,
            "module billing references the widget gateway endpoint".to_string(),
        );
        let dump = lines.join("\n");
        let query: HashSet<String> = ["widget".to_string()].into_iter().collect();
        let out = compress(&dump, &test_ctx(), &query).expect("fires");
        assert!(out.contains("widget gateway"), "query-relevant line kept");
    }
}

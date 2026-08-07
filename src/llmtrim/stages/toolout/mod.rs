//! Stage T — tool-output compression (feature #1).
//!
//! Tool results (logs, diffs, grep output) are the bulk of an agent's context and the
//! noisiest: 10k-line build logs around a handful of errors, 200-line diffs changing
//! three lines, grep dumps with one relevant hit per file. The prose-oriented stages
//! (retrieve, ngram) don't fit this shape. This stage detects the *kind* of each tool
//! output and routes it to a purpose-built lexical compressor:
//!
//! - **log**  → lossless template fold, then level/stack-aware line selection
//! - **diff** → file/hunk capping + context trimming (grammar, not folding: template
//!   collapse would corrupt patch semantics)
//! - **grep** → lossless template fold, then query-relevant windowing with a
//!   one-match-per-file *floor* (a keep-more guarantee, never a drop rule)
//!
//! The shape detectors contribute only structure *hints* (what a file field is, which
//! tokens are failure levels, where hunks begin); none of them owns a drop policy. The
//! drop policy is one shared recipe — fold losslessly first, ship that if it fits, else
//! window under an adaptive budget (`crate::llmtrim::stages::sizing`) with dropped runs
//! becoming positional elision markers (`[… N lines omitted …]`, like the `retrieve`
//! stage) — plus three universal rails that apply to every windowed segment, current
//! and future kinds alike:
//!
//! 1. **Attribution** (`rebuild`): windowed output opens with a self-identifying
//!    header naming llmtrim and the recovery action, so an agent never misattributes
//!    the elision to the tool (or to whatever wrapper ran it).
//! 2. **Repeat → passthrough** ([`ToolOutputStage::apply`]): when a candidate's content
//!    already appears earlier in the request — the agent re-ran the tool to get the
//!    dropped detail back — the newest occurrence ships in full. Equality is on a
//!    volatile-value-masked fingerprint (`template::fingerprint`), since re-runs are
//!    rarely byte-identical (timings, timestamps, PIDs). This is what makes the
//!    header's "re-run the tool" promise true: compression is deterministic, so
//!    without this rail a retry would be windowed identically and the agent would
//!    conclude the tool itself is broken.
//! 3. **Never inflate** (`elide_into`): an elision marker is emitted only when it is
//!    shorter than the lines it hides; a lone `--` separator survives as itself.
//!
//! Lossy, `InputTokens`-gated (reverts if it doesn't cut tokens), `Content`-scoped.
//! Zero model calls.
//!
//! Note on universality: the level/failure keywords scored below are tokens
//! *machine-emitted* by runtimes and build tools (`ERROR`, `FATAL`, `Traceback`,
//! `panicked`), not human prose, so a fixed set is appropriate. Locale-specific terms
//! from the user's request are handled by the query-overlap bonus, which is
//! Unicode-segmented via `lex_words`.

mod detect;
mod diff;
mod generated;
mod grep;
mod log;
mod normalize;
mod plaintext;
pub(crate) mod signals;
mod template;

use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use crate::llmtrim::gate::{GateKind, PlanEntry, Scope, Transform};
use crate::llmtrim::ir::Request;
use crate::llmtrim::provider::Provider;
use crate::llmtrim::stages::tools::lex_words;
use detect::OutKind;

/// First lines always kept for leading context.
const HEAD: usize = 2;
/// Last lines always kept for trailing context (final status / summary).
const TAIL: usize = 2;
/// Priority at or above which a line is force-kept regardless of budget (an error).
const FORCE_PRIORITY: f64 = 1.0;
/// Floor on the adaptive keep budget — never window below this many lines.
const MIN_KEEP: usize = 3;

/// In `Auto`, a segment goes aggressive only when it has at least this many units …
const AUTO_MIN_LINES: usize = 60;
/// … and its signal (errors / changed lines / distinct files) is at most this percent
/// of them. Big + signal-sparse ⇒ most of it is droppable noise, so signal-only saves
/// hugely with the answer preserved; otherwise stay adaptive (keep more, safer).
const AUTO_SIGNAL_PCT: usize = 25;

/// The compression aggressiveness a run asks for (from config / preset). Public because
/// it is the type of [`ToolOutputStage::mode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModeSetting {
    /// Always window to the adaptive budget (keep more).
    Adaptive,
    /// Always keep signal-only (errors / changes) + a summary. Kinds with no
    /// signal-anchored aggressive form (grep) window adaptively regardless.
    Aggressive,
    /// Decide per segment by noise density (the tuned default).
    Auto,
}

impl ModeSetting {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "adaptive" => Self::Adaptive,
            "aggressive" => Self::Aggressive,
            _ => Self::Auto,
        }
    }
}

/// The resolved mode for one segment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Adaptive,
    Aggressive,
}

/// Resolve the split for one segment. `signal` = must-keep units (errors / changed
/// lines / distinct files); `total` = the unit count the compressor windows over.
pub(crate) fn pick_mode(setting: ModeSetting, total: usize, signal: usize) -> Mode {
    match setting {
        ModeSetting::Adaptive => Mode::Adaptive,
        ModeSetting::Aggressive => Mode::Aggressive,
        ModeSetting::Auto => {
            // `signal >= 1`: signal-only selection needs signal to anchor on. With zero
            // must-keep units there is nothing to select *by* — "sparse" degenerates to
            // "drop everything", so fall back to adaptive windowing instead.
            if signal >= 1 && total >= AUTO_MIN_LINES && signal * 100 <= total * AUTO_SIGNAL_PCT {
                Mode::Aggressive
            } else {
                Mode::Adaptive
            }
        }
    }
}

/// Wording of the elision header (dirge-09e8 arm 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderStyle {
    /// The shipped-through-0.21 wording. Kept so an A/B control arm reproduces it byte
    /// for byte.
    Legacy,
    /// States who authored the gaps, that the underlying content is complete, and how to
    /// get it back. Models were reading the gaps as content and concluding the file was
    /// corrupt or an edit had failed.
    Explicit,
}

impl HeaderStyle {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "legacy" => Self::Legacy,
            _ => Self::Explicit,
        }
    }
}

/// First line of a tool result the model asked to receive uncompressed
/// (`read(verbatim=true)`). Kept in sync with `agent::tools::VERBATIM_MARKER`; llmtrim
/// is a standalone engine, so it matches on the literal rather than importing it.
pub const VERBATIM_PREFIX: &str = "[dirge: verbatim";

pub struct ToolOutputStage {
    /// Upper bound on lines kept per tool-output segment (the adaptive budget ceiling).
    pub max_lines: usize,
    /// Skip segments shorter than this many lines — below it the markers cost more than
    /// the drop saves.
    pub min_lines: usize,
    /// Run the lossless Drain template collapse on logs before windowing.
    pub template: bool,
    /// Adaptive/aggressive split: `Adaptive` (always window), `Aggressive` (always
    /// signal-only), or `Auto` (decide per segment by noise density).
    pub mode: ModeSetting,
    /// Compress content that is not a tool result — the user's own messages. Off by
    /// default: a paste is not tool output, nothing about it can be "re-run", and the
    /// silent Drain fold was reaching the model with no marker at all.
    pub user_text: bool,
    /// Window code and file excerpts. Off by default: the line ranking here is a *log*
    /// heuristic and on source it keeps a near-arbitrary few percent, destroying the
    /// line numbers `edit_lines` anchors on.
    pub code: bool,
    /// Elision-header wording.
    pub header: HeaderStyle,
    /// Honor the [`VERBATIM_PREFIX`] opt-out on a tool result.
    pub verbatim: bool,
}

/// Per-segment knobs handed to each kind's compressor.
#[derive(Clone, Copy)]
pub(crate) struct Ctx {
    pub max_lines: usize,
    pub template: bool,
    /// Adaptive/aggressive split for this run (resolved per-segment when `Auto`).
    pub mode: ModeSetting,
    /// Elision-header wording.
    pub header: HeaderStyle,
    /// Is this segment a tool result? Decides whether the header may tell the model to
    /// re-run anything.
    pub tool_result: bool,
}

impl Transform for ToolOutputStage {
    fn name(&self) -> &str {
        "toolout"
    }

    fn gate_kind(&self) -> GateKind {
        GateKind::InputTokens
    }

    fn scope(&self) -> Scope {
        Scope::Content
    }

    fn apply(
        &self,
        req: &mut Request,
        provider: &dyn Provider,
        _plan: &mut Vec<PlanEntry>,
    ) -> Result<()> {
        let pointers = crate::llmtrim::cache_zone::compressible_pointers(req, provider);

        // Pre-pass (feature #6): strip ANSI escapes and collapse carriage-return progress
        // on each candidate *before* detection. This is lossless for the model and lets
        // colored cargo/pytest output be detected/scored at all (level tokens are no longer
        // hidden inside escapes). `changed` flags segments the pre-pass altered so a
        // normalized-but-not-windowed segment still ships its (smaller) cleaned form.
        let texts: Vec<Option<(String, bool)>> = pointers
            .iter()
            .map(|p| req.get_str(p).map(normalize::normalize))
            .collect();
        let kinds: Vec<Option<OutKind>> = texts
            .iter()
            .map(|t| t.as_ref().and_then(|(t, _)| detect::detect(t)))
            .collect();

        // The "ask" = the short segments (instruction / question). Tool-output lines
        // overlapping it are biased to survive. Built from length, not kind, so it stays
        // the question even when the long segments are unrecognized (PlainText) output.
        let query: HashSet<String> = texts
            .iter()
            .filter_map(|t| t.as_ref().map(|(t, _)| t.as_str()))
            .filter(|t| t.lines().count() < self.min_lines)
            .flat_map(lex_words)
            .collect();

        let base_ctx = Ctx {
            max_lines: self.max_lines,
            template: self.template,
            mode: self.mode,
            header: self.header,
            tool_result: true,
        };

        // Rail: repeat → passthrough. A candidate whose content already appears at an
        // earlier content pointer is a re-invocation returning the same output — the
        // agent asking for the windowed detail back (the elision header tells it to).
        // Ship the newest occurrence in full. Equality is on the volatile-value-masked
        // `template::fingerprint`, not raw text: most re-runs are *not* byte-identical
        // (test timings like TAP's `duration_ms`, log timestamps, ports, PIDs), and raw
        // equality would re-window exactly the retry the header promised would work.
        // Masking is provider-neutral and language-free; a masked false match merely
        // ships one output uncompressed.
        let repeats: HashSet<String> = {
            let candidates: HashSet<&str> = pointers.iter().map(String::as_str).collect();
            let mut earlier: HashSet<u64> = HashSet::new();
            let mut repeats = HashSet::new();
            for p in provider.content_text_pointers(req) {
                let Some(t) = req.get_str(&p) else { continue };
                let fp = template::fingerprint(t);
                if candidates.contains(p.as_str()) && earlier.contains(&fp) {
                    repeats.insert(p);
                } else {
                    earlier.insert(fp);
                }
            }
            repeats
        };

        for ((ptr, text), kind) in pointers.iter().zip(&texts).zip(&kinds) {
            let Some((text, normalized)) = text else {
                continue;
            };
            if text.lines().count() < self.min_lines || repeats.contains(ptr) {
                // Too small to window, or a repeated invocation the agent made to get the
                // full output back (passthrough). Either way no windowing; still ship the
                // pre-pass-cleaned form if the ANSI/CR strip changed anything (the gate
                // reverts if it didn't actually save tokens). Checked before the opt-outs
                // below so the per-line scans never run on a segment that was going to be
                // skipped anyway.
                if *normalized {
                    req.set(ptr, Value::String(text.clone()));
                }
                continue;
            }
            // Both halves are needed to cover every wire shape. Anthropic and Gemini put
            // tool output in a *block* inside a `user` turn, so only the block type
            // identifies it; OpenAI Chat Completions and Responses put it in a turn of
            // its own, which has no block type but does resolve to `Role::Tool`. Missing
            // either half would classify real tool output as user text and silently
            // switch this stage off for that provider.
            let tool_result = crate::llmtrim::provider::is_tool_result_ptr(req, ptr)
                || provider.role_at(req, ptr) == Some(crate::llmtrim::provider::Role::Tool);
            // Three opt-outs, all decided before any transform touches the text — a skip,
            // not a revert, because both of the transforms below (Drain fold as well as
            // windowing) can reach the model unmarked, and the token gate cannot tell a
            // ruined file from a well-compressed log.
            //
            // 1. Not tool output. The user's own message: nothing to "re-run", and the
            //    fold marker arrives with no header at all.
            // 2. Code / a file excerpt. Ranked by a log heuristic it loses ~95% of its
            //    lines and all of its line numbers.
            // 3. Explicitly exempted by the model via `read(verbatim=true)`.
            //
            // Ordered cheapest-first: a pointer/role lookup, then a prefix compare, and
            // only then the per-line code scan.
            if (!self.user_text && !tool_result)
                || (self.verbatim && text.trim_start().starts_with(VERBATIM_PREFIX))
                || (!self.code && detect::is_code(text))
            {
                // Left strictly alone — even the ANSI/CR pre-pass is a change the model
                // would have to reconcile against what it believes is on disk.
                continue;
            }
            let ctx = Ctx {
                tool_result,
                ..base_ctx
            };
            let compressed = match kind {
                Some(OutKind::Log) => log::compress(text, &ctx, &query),
                Some(OutKind::Grep) => grep::compress(text, &ctx, &query),
                Some(OutKind::Diff) => diff::compress(text, &ctx, &query),
                // Unrecognized shape. First try the generated/lockfile near-total elision
                // (feature #7, high-confidence machine-noise shapes only); otherwise the
                // redundancy-gated generic fallback ("any tool").
                None => {
                    generated::compress(text).or_else(|| plaintext::compress(text, &ctx, &query))
                }
            };
            if let Some(compressed) = compressed {
                req.set(ptr, Value::String(compressed));
            } else if *normalized {
                // No per-kind compression, but the pre-pass cleaned it → ship the cleaned
                // form so the ANSI/CR savings aren't lost.
                req.set(ptr, Value::String(text.clone()));
            }
        }
        Ok(())
    }
}

/// Priority assigned to a warning line (also the floor for "this is a warning" in the
/// summary count).
pub(crate) const WARN_PRIORITY: f64 = 0.6;

/// Intrinsic importance of a log/output line in `[0,1]`: failures dominate, warnings
/// next, indented stack-frame continuations kept above plain noise.
pub(crate) fn priority(line: &str) -> f64 {
    if signals::STRONG.is_match(line) {
        FORCE_PRIORITY
    } else if signals::WARN.is_match(line) {
        WARN_PRIORITY
    } else if line.starts_with(' ') || line.starts_with('\t') {
        0.5 // stack-trace / continuation line — keep with its error
    } else {
        0.3
    }
}

/// Relevance bonus for a line overlapping the request's words (capped, additive on top
/// of [`priority`]). Unicode-segmented, so it works in any language.
///
/// Counts *distinct* query words (repeating one word doesn't stack) at 0.2 each, capped
/// at 0.6 — so two distinct ask-words on a plain line (0.3 + 0.4 = 0.7) outrank the 0.5
/// indentation prior, and relevance can dominate purely structural scores in uniformly
/// indented content (HTML/markdown/YAML). STRONG error lines ([`FORCE_PRIORITY`] = 1.0)
/// still rank at or above any non-error line with up to two distinct hits.
pub(crate) fn query_bonus(line: &str, query: &HashSet<String>) -> f64 {
    if query.is_empty() {
        return 0.0;
    }
    let hits: HashSet<String> = lex_words(line)
        .into_iter()
        .filter(|w| query.contains(w))
        .collect();
    (hits.len() as f64 * 0.2).min(0.6)
}

/// Fill `keep` up to `budget` slots by picking the highest-scoring unfilled indices,
/// ties broken by original (ascending) order. Indices already set in `keep` count
/// toward the budget; the caller sets any forced/pinned slots before calling this.
/// Shared by [`select_keep`] and [`diff::cap_by_score`].
pub(crate) fn fill_by_score(keep: &mut [bool], scores: &[f64], budget: usize) {
    let n = keep.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let mut count = keep.iter().filter(|&&x| x).count();
    for &i in &order {
        if count >= budget {
            break;
        }
        if !keep[i] {
            keep[i] = true;
            count += 1;
        }
    }
}

/// Choose which of `scores.len()` lines to keep: always the first [`HEAD`] and last
/// [`TAIL`], always any at or above `force`, then fill by descending score (ties by
/// original order) until `k` are kept. Forced lines may exceed `k` — errors are never
/// dropped to hit a budget.
pub(crate) fn select_keep(scores: &[f64], k: usize, force: f64) -> Vec<bool> {
    let n = scores.len();
    let mut keep = vec![false; n];
    for slot in keep.iter_mut().take(HEAD.min(n)) {
        *slot = true;
    }
    for slot in keep.iter_mut().skip(n.saturating_sub(TAIL)) {
        *slot = true;
    }
    for (i, &s) in scores.iter().enumerate() {
        if s >= force {
            keep[i] = true;
        }
    }
    fill_by_score(&mut keep, scores, k);
    keep
}

/// Replace a dropped run of lines with a positional elision marker — dropped content is
/// referenced by position, never stored (matches the `retrieve` stage's convention). If
/// the agent needs it back, it re-runs the tool (true by construction: the repeat →
/// passthrough rail ships a re-invocation in full).
pub(crate) fn elide(dropped: &[&str]) -> String {
    if dropped.len() == 1 {
        "[… 1 line omitted …]".to_string()
    } else {
        format!("[… {} lines omitted …]", dropped.len())
    }
}

/// Rail: never inflate. Append the [`elide`] marker for `dropped` — unless the marker
/// would cost as much as the lines it hides (a lone `--` separator, a tiny run), in
/// which case the lines themselves are appended. Returns whether a marker was emitted.
pub(crate) fn elide_into(dropped: &[&str], out: &mut Vec<String>) -> bool {
    let marker = elide(dropped);
    let dropped_len: usize = dropped.iter().map(|l| l.len() + 1).sum();
    if marker.len() + 1 >= dropped_len {
        out.extend(dropped.iter().map(|l| (*l).to_string()));
        false
    } else {
        out.push(marker);
        true
    }
}

/// True for a line this module emitted as an elision marker (not content).
fn is_elision_marker(line: &str) -> bool {
    line.starts_with("[… ") && line.ends_with(" omitted …]")
}

/// Rail: attribution. Prefix a windowed `body` with a self-identifying header stating
/// who elided and how to recover, so the agent never misattributes the gaps to the tool
/// (or to whatever wrapper ran it). No header when nothing was actually elided (the
/// never-inflate rail may have kept every "dropped" run).
pub(crate) fn attributed(body: Vec<String>, total: usize, ctx: &Ctx) -> String {
    if !body.iter().any(|l| is_elision_marker(l)) {
        return body.join("\n");
    }
    let shown = body.iter().filter(|l| !is_elision_marker(l)).count();
    let header = match (ctx.header, ctx.tool_result) {
        (HeaderStyle::Legacy, _) => format!(
            "[llmtrim: showing {shown} of {total} lines — re-run the tool for the full output]"
        ),
        // Three claims, because models get all three wrong: they attribute the gaps to
        // the tool or to the file, they treat the visible text as the whole file (and so
        // report unmatched braces or a failed edit), and they invent workarounds instead
        // of asking again.
        (HeaderStyle::Explicit, true) => format!(
            "[llmtrim compressed this tool output: {shown} of {total} lines shown. The gaps \
             and any [×N] markers are llmtrim's, not the file's — the underlying content is \
             complete and unchanged. Re-run the same tool call for the full text.]"
        ),
        // No tool to re-run: this is the user's own message.
        (HeaderStyle::Explicit, false) => format!(
            "[llmtrim compressed this message: {shown} of {total} lines shown. The gaps and \
             any [×N] markers are llmtrim's, not the author's — the original is complete \
             and unchanged.]"
        ),
    };
    format!("{header}\n{}", body.join("\n"))
}

/// Reassemble `lines` in original order, keeping those flagged in `keep`, collapsing
/// each maximal dropped run into one [`elide`] marker (unless the marker would inflate),
/// and opening with the [`attributed`] recovery header. Shared by the log, grep and
/// plaintext compressors (all select at line granularity).
pub(crate) fn rebuild(lines: &[&str], keep: &[bool], ctx: &Ctx) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if keep[i] {
            out.push(lines[i].to_string());
            i += 1;
        } else {
            let start = i;
            while i < lines.len() && !keep[i] {
                i += 1;
            }
            elide_into(&lines[start..i], &mut out);
        }
    }
    attributed(out, lines.len(), ctx)
}

#[cfg(test)]
pub(crate) fn test_ctx() -> Ctx {
    Ctx {
        max_lines: 30,
        template: true,
        mode: ModeSetting::Adaptive,
        header: HeaderStyle::Explicit,
        tool_result: true,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Ctx, HeaderStyle, Mode, ModeSetting, attributed, elide, elide_into, pick_mode, rebuild,
        test_ctx,
    };

    #[test]
    fn auto_goes_aggressive_only_when_big_and_sparse() {
        assert_eq!(
            pick_mode(ModeSetting::Auto, 100, 5),
            Mode::Aggressive,
            "big + sparse"
        );
        assert_eq!(
            pick_mode(ModeSetting::Auto, 100, 40),
            Mode::Adaptive,
            "big but dense"
        );
        assert_eq!(
            pick_mode(ModeSetting::Auto, 30, 1),
            Mode::Adaptive,
            "too small"
        );
        // Rail: no signal → no aggressive. Zero must-keep units means signal-only
        // selection has nothing to anchor on; "sparse" must not degenerate to "drop all".
        assert_eq!(
            pick_mode(ModeSetting::Auto, 1000, 0),
            Mode::Adaptive,
            "zero signal stays adaptive"
        );
    }

    #[test]
    fn elide_grammar_is_number_aware() {
        assert_eq!(elide(&["x"]), "[… 1 line omitted …]");
        assert_eq!(elide(&["x", "y"]), "[… 2 lines omitted …]");
    }

    #[test]
    fn elide_never_inflates() {
        // A lone `--` separator (rg -C output) is shorter than any marker: keep it.
        let mut out = Vec::new();
        assert!(!elide_into(&["--"], &mut out), "no marker for a tiny run");
        assert_eq!(out, vec!["--".to_string()]);

        // A long run is genuinely worth a marker.
        let big = vec!["this line is long enough to be worth eliding away"; 4];
        let mut out = Vec::new();
        assert!(elide_into(&big, &mut out), "marker for a real run");
        assert_eq!(out, vec!["[… 4 lines omitted …]".to_string()]);
    }

    /// The legacy header, kept verbatim so the A/B control arm reproduces today's
    /// behavior exactly (dirge-09e8 arm 3).
    #[test]
    fn rebuild_attributes_the_elision_and_states_recovery() {
        let lines: Vec<&str> = (0..10)
            .map(|_| "payload line with real content here")
            .collect();
        let mut keep = vec![false; 10];
        keep[0] = true;
        keep[9] = true;
        let ctx = Ctx {
            header: HeaderStyle::Legacy,
            ..test_ctx()
        };
        let out = rebuild(&lines, &keep, &ctx);
        assert!(
            out.starts_with("[llmtrim: showing 2 of 10 lines — re-run the tool"),
            "self-identifying recovery header first: {out}"
        );
        assert!(
            out.contains("[… 8 lines omitted …]"),
            "positional marker kept"
        );
    }

    /// dirge-09e8 arm 3. Models read the gaps as *file* content and conclude the file
    /// is corrupt, an edit failed, or a brace is unmatched. The header has to deny all
    /// three: name llmtrim as the author of the gaps, say the underlying content is
    /// complete, and name the recovery action.
    #[test]
    fn explicit_header_denies_that_the_gaps_are_content() {
        let lines: Vec<&str> = (0..10)
            .map(|_| "payload line with real content here")
            .collect();
        let mut keep = vec![false; 10];
        keep[0] = true;
        keep[9] = true;
        let ctx = Ctx {
            header: HeaderStyle::Explicit,
            tool_result: true,
            ..test_ctx()
        };
        let out = rebuild(&lines, &keep, &ctx);
        let head = out.lines().next().unwrap();
        assert!(head.contains("llmtrim"), "names the author: {head}");
        assert!(head.contains("2 of 10 lines"), "states the ratio: {head}");
        assert!(
            head.contains("not the file's"),
            "denies the gaps are file content: {head}"
        );
        assert!(
            head.contains("complete and unchanged"),
            "asserts the underlying file is intact: {head}"
        );
        assert!(
            head.to_ascii_lowercase().contains("re-run"),
            "names the recovery action: {head}"
        );
    }

    /// The same header on a non-tool segment must not tell the model to "re-run the
    /// tool" — there is no tool. Only reachable in the legacy `toolout_user_text` arm,
    /// but it has to be right there too.
    #[test]
    fn explicit_header_does_not_invent_a_tool_for_user_text() {
        let lines: Vec<&str> = (0..10)
            .map(|_| "payload line with real content here")
            .collect();
        let mut keep = vec![false; 10];
        keep[0] = true;
        keep[9] = true;
        let ctx = Ctx {
            header: HeaderStyle::Explicit,
            tool_result: false,
            ..test_ctx()
        };
        let out = rebuild(&lines, &keep, &ctx);
        let head = out.lines().next().unwrap();
        assert!(!head.contains("tool"), "no tool to re-run: {head}");
        assert!(
            head.contains("complete and unchanged"),
            "still asserts the original is intact: {head}"
        );
    }

    #[test]
    fn no_header_when_nothing_was_actually_elided() {
        // The never-inflate rail can return every "dropped" line — then there is no
        // elision to attribute and the header must not appear.
        let body = vec!["--".to_string(), "ok".to_string()];
        let out = attributed(body, 2, &test_ctx());
        assert_eq!(out, "--\nok");
    }

    #[test]
    fn forced_modes_ignore_the_signal() {
        assert_eq!(pick_mode(ModeSetting::Aggressive, 10, 9), Mode::Aggressive);
        assert_eq!(pick_mode(ModeSetting::Adaptive, 1000, 1), Mode::Adaptive);
    }

    #[test]
    fn mode_setting_parses_with_auto_fallback() {
        assert_eq!(ModeSetting::parse("aggressive"), ModeSetting::Aggressive);
        assert_eq!(ModeSetting::parse("ADAPTIVE"), ModeSetting::Adaptive);
        assert_eq!(ModeSetting::parse("auto"), ModeSetting::Auto);
        assert_eq!(ModeSetting::parse("nonsense"), ModeSetting::Auto);
    }
}

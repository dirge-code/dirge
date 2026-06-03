//! Storm breaker — repeat-loop detection for tool calls.
//!
//! Faithful port of `DeepSeek-Reasonix/src/repair/storm.ts` (66 lines).
//!
//! Tracks (tool_name, args) tuples in a sliding window. When the same
//! call appears `threshold` times within `window_size` entries, the
//! call is suppressed (the model is stuck in a loop).
//!
//! Mutating calls (write, edit, bash) clear prior read-only entries
//! from the window so a post-edit verify-read isn't flagged as a
//! repeat. Mutators still count amongst themselves — three identical
//! edits in a row IS a storm.
//!
//! Storm-exempt tools (cheap inspectors like `list_dir`) never trip
//! the guard regardless of repetition count.

use super::tools::ToolCall;

/// Outcome of `StormBreaker::inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StormVerdict {
    pub suppress: bool,
    pub reason: Option<String>,
}

impl StormVerdict {
    fn pass() -> Self {
        Self {
            suppress: false,
            reason: None,
        }
    }

    fn suppress(name: &str, count: usize) -> Self {
        Self {
            suppress: true,
            reason: Some(format!(
                "{name} called with identical args {count} times — repeat-loop guard tripped"
            )),
        }
    }
}

/// Summary of what the storm breaker did to a batch of tool calls.
#[derive(Debug, Clone, Default)]
pub struct StormReport {
    /// How many calls were suppressed.
    pub storms_broken: usize,
    /// Per-suppression reasons for diagnostics.
    pub notes: Vec<String>,
}

impl StormReport {
    /// True when every call was suppressed and there was at least one.
    pub fn all_suppressed(&self, original_count: usize) -> bool {
        self.storms_broken > 0 && self.storms_broken == original_count && original_count > 0
    }
}

struct RecentEntry {
    name: String,
    args: String,
    read_only: bool,
}

/// Tracks (name, args) repeats in a sliding window.
///
/// Mutating calls clear prior read-only entries while still
/// counting amongst themselves. Storm-exempt calls never trigger.
// The `Option<Box<dyn Fn ...>>` predicate type is more readable inline
// than aliased; both fields use the exact same shape so the lint's
// "factor into a type" suggestion would just rename without clarifying.
#[allow(clippy::type_complexity)]
pub struct StormBreaker {
    window_size: usize,
    threshold: usize,
    is_mutating: Option<Box<dyn Fn(&ToolCall) -> bool + Send + Sync>>,
    is_storm_exempt: Option<Box<dyn Fn(&ToolCall) -> bool + Send + Sync>>,
    recent: Vec<RecentEntry>,
}

impl StormBreaker {
    #[allow(clippy::type_complexity)]
    pub fn new(
        window_size: usize,
        threshold: usize,
        is_mutating: Option<Box<dyn Fn(&ToolCall) -> bool + Send + Sync>>,
        is_storm_exempt: Option<Box<dyn Fn(&ToolCall) -> bool + Send + Sync>>,
    ) -> Self {
        assert!(
            threshold >= 2,
            "storm breaker threshold must be >= 2 (got {threshold})"
        );
        assert!(
            window_size >= threshold,
            "storm breaker window_size ({window_size}) must be >= threshold ({threshold})"
        );
        Self {
            window_size,
            threshold,
            is_mutating,
            is_storm_exempt,
            recent: Vec::with_capacity(window_size),
        }
    }

    pub fn inspect(&mut self, call: &ToolCall) -> StormVerdict {
        let name = &call.name;
        if name.is_empty() {
            return StormVerdict::pass();
        }
        if let Some(ref exempt) = self.is_storm_exempt
            && exempt(call)
        {
            return StormVerdict::pass();
        }
        // Canonical signature shared with the scavenge dedup (run.rs):
        // `canonical_json` explicitly sorts object keys and normalizes numeric
        // reprs (`1` ≡ `1.0`), so the repeat detector isn't silently dependent
        // on `serde_json`'s `preserve_order` feature staying off (dirge-ark9,
        // closing dirge-7bwx review-fix #6) and matches the scavenger's
        // dedup exactly.
        let args = super::message::canonical_json(&call.arguments);

        let mutating = self.is_mutating.as_ref().map(|f| f(call)).unwrap_or(false);
        let read_only = !mutating;

        if mutating {
            // Drop prior read-only entries — the file/shell state just
            // changed, so a verify-read after this should start with a
            // clean slate. Keep mutator entries: 3 identical edits in
            // a row is still a storm (model in a loop).
            // Iterate in reverse so removals don't shift indices.
            let mut i = self.recent.len();
            while i > 0 {
                i -= 1;
                if self.recent[i].read_only {
                    self.recent.remove(i);
                }
            }
        }

        let count = self
            .recent
            .iter()
            .filter(|e| e.name == *name && e.args == args)
            .count();

        if count >= self.threshold.saturating_sub(1) {
            return StormVerdict::suppress(name, count + 1);
        }

        self.recent.push(RecentEntry {
            name: name.clone(),
            args,
            read_only,
        });
        while self.recent.len() > self.window_size {
            self.recent.remove(0);
        }

        StormVerdict::pass()
    }

    pub fn reset(&mut self) {
        self.recent.clear();
    }

    /// Filter a batch of tool calls through the storm breaker.
    /// Returns surviving calls and a report of what was suppressed.
    /// Port of `ToolCallRepair.process()` storm phase
    /// (repair/index.ts:111-121).
    pub fn filter_calls(&mut self, calls: &[ToolCall]) -> (Vec<ToolCall>, StormReport) {
        let mut surviving: Vec<ToolCall> = Vec::with_capacity(calls.len());
        let mut report = StormReport::default();

        for call in calls {
            let verdict = self.inspect(call);
            if verdict.suppress {
                report.storms_broken += 1;
                if let Some(reason) = verdict.reason {
                    tracing::warn!("storm breaker: {reason}");
                    report.notes.push(reason);
                }
            } else {
                surviving.push(call.clone());
            }
        }

        if report.storms_broken > 0 {
            tracing::info!(
                suppressed = report.storms_broken,
                surviving = surviving.len(),
                "storm breaker: {}/{} calls suppressed",
                report.storms_broken,
                calls.len()
            );
        }

        (surviving, report)
    }
}

/// Built-in mutating tools: calls that change filesystem state or run
/// external code. Derived from the canonical tool→[`Operation`] mapping
/// (`Edit` = file mutation, `Execute` = shell) rather than a hand-kept
/// name list — so a new mutating tool is classified the moment it has a
/// permission operation, with no second list to forget (dirge-uxuv).
pub fn default_mutating(call: &ToolCall) -> bool {
    use crate::permission::engine::tool_operation;
    use crate::permission::engine::types::Operation;
    matches!(
        tool_operation(&call.name),
        Operation::Edit | Operation::Execute
    )
}

/// Built-in storm-exempt tools: read-only inspectors that should never
/// trip the repeat-loop guard regardless of repetition count. Derived
/// from the canonical mapping (`Operation::Read`) — covers read/grep/
/// find/glob/list_dir/repo_overview AND the lsp + semantic read tools,
/// which are equally side-effect-free (dirge-uxuv).
pub fn default_exempt(call: &ToolCall) -> bool {
    use crate::permission::engine::tool_operation;
    use crate::permission::engine::types::Operation;
    matches!(tool_operation(&call.name), Operation::Read)
}

impl Default for StormBreaker {
    fn default() -> Self {
        Self::new(
            6,
            3,
            Some(Box::new(default_mutating)),
            Some(Box::new(default_exempt)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: args,
        }
    }

    fn call_json(name: &str, args_json: &str) -> ToolCall {
        call(
            name,
            serde_json::from_str::<serde_json::Value>(args_json).unwrap_or(json!({})),
        )
    }

    #[test]
    fn passes_through_below_threshold() {
        let mut sb = StormBreaker::new(6, 3, None, None);
        assert!(!sb.inspect(&call_json("x", "{}")).suppress);
        assert!(!sb.inspect(&call_json("x", "{}")).suppress);
    }

    #[test]
    fn suppresses_on_threshold_reached() {
        let mut sb = StormBreaker::new(6, 3, None, None);
        sb.inspect(&call_json("x", "{}"));
        sb.inspect(&call_json("x", "{}"));
        let verdict = sb.inspect(&call_json("x", "{}"));
        assert!(verdict.suppress);
        assert!(verdict.reason.unwrap().contains("repeat-loop guard"));
    }

    #[test]
    fn distinguishes_different_args_as_different_calls() {
        let mut sb = StormBreaker::new(6, 3, None, None);
        sb.inspect(&call_json("x", r#"{"a":1}"#));
        sb.inspect(&call_json("x", r#"{"a":2}"#));
        sb.inspect(&call_json("x", r#"{"a":3}"#));
        let verdict = sb.inspect(&call_json("x", r#"{"a":4}"#));
        assert!(!verdict.suppress);
    }

    #[test]
    fn forgets_old_calls_beyond_window() {
        let mut sb = StormBreaker::new(3, 3, None, None);
        sb.inspect(&call_json("x", "{}"));
        sb.inspect(&call_json("x", "{}"));
        sb.inspect(&call_json("y", "{}"));
        sb.inspect(&call_json("z", "{}"));
        sb.inspect(&call_json("w", "{}"));
        // Only the most recent 3 are in the window now, none of which
        // is "x", so a single new "x" should not suppress.
        assert!(!sb.inspect(&call_json("x", "{}")).suppress);
    }

    #[test]
    fn intervening_mutating_call_resets_window_for_rerereads() {
        let mutators: Box<dyn Fn(&ToolCall) -> bool + Send + Sync> =
            Box::new(|c| matches!(c.name.as_str(), "edit_file" | "write_file"));
        let mut sb = StormBreaker::new(6, 3, Some(mutators), None);
        let args = r#"{"path":"src/env.ts"}"#;
        assert!(!sb.inspect(&call_json("read_file", args)).suppress);
        assert!(
            !sb.inspect(&call_json(
                "edit_file",
                r#"{"path":"src/env.ts","new_text":"x"}"#,
            ))
            .suppress
        );
        assert!(!sb.inspect(&call_json("read_file", args)).suppress);
        assert!(
            !sb.inspect(&call_json(
                "edit_file",
                r#"{"path":"src/env.ts","new_text":"y"}"#,
            ))
            .suppress
        );
        // 3rd read_file with identical args — would trip the breaker
        // pre-fix, but each edit_file legitimately changed the file in
        // between.
        assert!(!sb.inspect(&call_json("read_file", args)).suppress);
    }

    #[test]
    fn predicate_flagged_write_file_resets_the_window() {
        let mutators: Box<dyn Fn(&ToolCall) -> bool + Send + Sync> =
            Box::new(|c| c.name == "write_file");
        let mut sb = StormBreaker::new(6, 3, Some(mutators), None);
        assert!(!sb.inspect(&call_json("read_file", "{}")).suppress);
        assert!(!sb.inspect(&call_json("read_file", "{}")).suppress);
        assert!(!sb.inspect(&call_json("write_file", "{}")).suppress);
        // Buffer cleared by write_file — a fresh pair of reads is now safe.
        assert!(!sb.inspect(&call_json("read_file", "{}")).suppress);
        assert!(!sb.inspect(&call_json("read_file", "{}")).suppress);
    }

    // dirge-uxuv: the storm classifiers derive from the canonical
    // tool_operation mapping, not a hand-kept name list — so they can't
    // drift from the permission engine's notion of what a tool does.
    #[test]
    fn default_classifiers_track_tool_operation() {
        for t in ["write", "edit", "apply_patch", "bash"] {
            assert!(
                default_mutating(&call_json(t, "{}")),
                "{t} must be mutating"
            );
            assert!(
                !default_exempt(&call_json(t, "{}")),
                "{t} must not be exempt"
            );
        }
        // Read-only tools — incl. lsp + semantic-read, which the old
        // hardcoded exempt list omitted.
        for t in [
            "read",
            "grep",
            "find_files",
            "glob",
            "list_dir",
            "repo_overview",
            "lsp",
            "list_symbols",
        ] {
            assert!(default_exempt(&call_json(t, "{}")), "{t} must be exempt");
            assert!(!default_mutating(&call_json(t, "{}")), "{t} not mutating");
        }
        // Neither mutating nor exempt → counted normally.
        for t in ["webfetch", "task", "memory", "mcp_tool"] {
            assert!(!default_mutating(&call_json(t, "{}")), "{t}");
            assert!(!default_exempt(&call_json(t, "{}")), "{t}");
        }
    }

    #[test]
    fn with_no_predicate_every_tool_counts() {
        let mut sb = StormBreaker::new(6, 3, None, None);
        sb.inspect(&call_json("edit_file", "{}"));
        sb.inspect(&call_json("edit_file", "{}"));
        assert!(sb.inspect(&call_json("edit_file", "{}")).suppress);
    }

    mod storm_exempt {
        use super::*;

        #[test]
        fn exempt_tools_never_trip_the_storm_guard() {
            let exempt: Box<dyn Fn(&ToolCall) -> bool + Send + Sync> =
                Box::new(|c| matches!(c.name.as_str(), "read_file" | "list_jobs"));
            let mut sb = StormBreaker::new(6, 3, None, Some(exempt));
            for _ in 0..10 {
                assert!(
                    !sb.inspect(&call_json("read_file", r#"{"path":"/foo"}"#))
                        .suppress
                );
            }
        }

        #[test]
        fn non_exempt_tools_still_trip_after_exempt_reads() {
            let exempt: Box<dyn Fn(&ToolCall) -> bool + Send + Sync> =
                Box::new(|c| c.name == "read_file");
            let mut sb = StormBreaker::new(3, 3, None, Some(exempt));
            sb.inspect(&call_json("edit_file", "{}"));
            sb.inspect(&call_json("edit_file", "{}"));
            sb.inspect(&call_json("read_file", "{}"));
            sb.inspect(&call_json("read_file", "{}"));
            sb.inspect(&call_json("read_file", "{}"));
            assert!(sb.inspect(&call_json("edit_file", "{}")).suppress);
        }
    }

    #[test]
    fn filter_calls_passes_through_below_threshold() {
        let mut sb = StormBreaker::new(6, 3, None, None);
        let calls = vec![call_json("x", "{}"), call_json("x", "{}")];
        let (surviving, report) = sb.filter_calls(&calls);
        assert_eq!(surviving.len(), 2);
        assert_eq!(report.storms_broken, 0);
    }

    #[test]
    fn filter_calls_suppresses_at_threshold() {
        let mut sb = StormBreaker::new(6, 3, None, None);
        let calls = vec![
            call_json("x", "{}"),
            call_json("x", "{}"),
            call_json("x", "{}"),
        ];
        let (surviving, report) = sb.filter_calls(&calls);
        // First two pass, third is suppressed.
        assert_eq!(surviving.len(), 2);
        assert_eq!(report.storms_broken, 1);
        // Not all-suppressed — 2 calls survived.
        assert!(!report.all_suppressed(3));
    }

    #[test]
    fn filter_calls_all_suppressed_on_second_batch() {
        let mut sb = StormBreaker::new(6, 3, None, None);
        // First batch: 3 calls, 3rd suppressed
        let calls1: Vec<ToolCall> = (0..3).map(|_| call_json("x", "{}")).collect();
        let (surviving1, _) = sb.filter_calls(&calls1);
        assert_eq!(surviving1.len(), 2);

        // Second batch: same 3 calls again — all suppressed now
        // because there are already 2 in the window.
        let calls2: Vec<ToolCall> = (0..3).map(|_| call_json("x", "{}")).collect();
        let (surviving2, report2) = sb.filter_calls(&calls2);
        assert_eq!(surviving2.len(), 0);
        assert_eq!(report2.storms_broken, 3);
        assert!(report2.all_suppressed(3));
    }
}

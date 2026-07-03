//! Multi-tier auto-compaction decision engine — and the **canonical reference
//! for the whole context-budget ladder** (dirge-w5iy). The budget policy is
//! split across two cohesive modules by concern, and this is the one place
//! that documents the complete picture:
//!
//!   - **decision** (when/whether/how-hard to fold) lives here.
//!   - **mechanism** (the token estimator, per-result caps, the summarizer,
//!     and the snip override) lives in [`crate::agent::compression`].
//!
//! Faithful port of `DeepSeek-Reasonix/src/context-manager.ts` (345 lines).
//!
//! # The budget ladder
//!
//! Every threshold is a **fraction of the model's context window** (`ctx_max`),
//! compared against the current token count. In ascending order of pressure:
//!
//! | Fraction | Tier | Owner | Action |
//! |----------|------|-------|--------|
//! | 0.60 | Aggressive per-result cap | compression: [`AGGRESSIVE_CAP_THRESHOLD`] | tighten each tool-result cap (3000→1000 tok) to head off overflow *before* a fold is needed |
//! | 0.75 | Post-response fold | [`HISTORY_FOLD_THRESHOLD`] | fold older history into a summary, keep a 20% tail. Also gates the summarizer LLM call ([`should_compress`]) |
//! | 0.78 | Aggressive fold | [`HISTORY_FOLD_AGGRESSIVE_THRESHOLD`] | the normal fold didn't buy enough headroom → halve the tail budget (10%) |
//! | 0.80 | Exit-with-summary | [`FORCE_SUMMARY_THRESHOLD`] | defense in depth: force a final summary and end the turn |
//! | 0.90 | Turn-start fold | [`TURN_START_FOLD_THRESHOLD`] | before the first API call — catches a terminal prior turn, session restore, or a huge user paste |
//!
//! Plus a guard (not a pressure tier): the **min-savings check** (0.30,
//! `HISTORY_FOLD_MIN_SAVINGS_FRACTION`) skips a fold whose head wouldn't
//! shrink the log by at least that fraction.
//!
//! # One estimator, two measurement points
//!
//! There is a **single** token estimator —
//! [`compression::estimate_messages_tokens`] (`chars / CHARS_PER_TOKEN`). What
//! differs is *when* the count is taken, not *how*:
//!
//!   - **pre-send** (turn-start fold, the per-result cap tier): the local
//!     estimate, since the API hasn't been called yet;
//!   - **post-response** ([`decide_after_usage`]): the API's exact
//!     `prompt_tokens` from the usage response.
//!
//! These two numbers can legitimately disagree (the estimate is approximate);
//! that's inherent to measuring before vs. after the call, not a duplicated
//! estimator.
//!
//! # The snip override
//!
//! A pre-send "snip" ([`compression::cap_oversized_tool_results`]) can free
//! enough tokens that a *normal* post-response fold is unnecessary; the
//! suppression lives in `run.rs` via [`compression::snip_bought_enough`]
//! (a snip freeing ≥10% of the window skips a normal fold; aggressive /
//! force-summary folds always proceed). It is intentionally *not* baked into
//! [`decide_after_usage`] so the decision stays a pure function of the token
//! ratio; run.rs composes the two.
//!
//! # Tail protection: two strategies
//!
//! Recent messages are protected by **message count**
//! ([`compression::PROTECT_TAIL_DEFAULT`]) at the pruning layer, while the
//! fold tiers above express the tail as a **token fraction** of the window
//! (20% / 10%). They are not equivalent (5 messages may be 100 or 50 000
//! tokens); run.rs picks the `protect_tail` count per fold kind.
//!
//! [`AGGRESSIVE_CAP_THRESHOLD`]: crate::agent::compression::AGGRESSIVE_CAP_THRESHOLD
//! [`should_compress`]: crate::agent::compression::should_compress
//! [`compression::estimate_messages_tokens`]: crate::agent::compression::estimate_messages_tokens
//! [`compression::cap_oversized_tool_results`]: crate::agent::compression::cap_oversized_tool_results
//! [`compression::snip_bought_enough`]: crate::agent::compression::snip_bought_enough
//! [`compression::PROTECT_TAIL_DEFAULT`]: crate::agent::compression::PROTECT_TAIL_DEFAULT

use serde::Serialize;

// ================================================================
// Threshold constants — port of context-manager.ts:27-43
// ================================================================

/// Auto-fold when a turn's response shows promptTokens above
/// this fraction of ctxMax.
pub const HISTORY_FOLD_THRESHOLD: f64 = 0.75;

/// Tail budget after a normal fold, as a fraction of ctxMax.
pub const HISTORY_FOLD_TAIL_FRACTION: f64 = 0.2;

/// Above this fraction the normal fold's tail budget didn't
/// buy enough headroom — fold harder.
pub const HISTORY_FOLD_AGGRESSIVE_THRESHOLD: f64 = 0.78;

/// Tail budget after an aggressive fold — half the normal one,
/// sacrifices recent context for headroom.
pub const HISTORY_FOLD_AGGRESSIVE_TAIL_FRACTION: f64 = 0.1;

/// Skip the fold if the head wouldn't shrink the log by at
/// least this fraction.
#[cfg(test)]
pub const HISTORY_FOLD_MIN_SAVINGS_FRACTION: f64 = 0.3;

/// Above this fraction we exit the turn with a summary instead
/// of folding (defense in depth).
pub const FORCE_SUMMARY_THRESHOLD: f64 = 0.8;

/// Turn-start local estimate above this fraction triggers a
/// pre-iter fold. Covers cases the post-response fold can't
/// (terminal prior turn, fresh session restore, huge user
/// paste).
pub const TURN_START_FOLD_THRESHOLD: f64 = 0.9;

// ================================================================
// Data types — port of context-manager.ts:67-85
// ================================================================

/// What action the context manager recommends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PostUsageDecisionKind {
    /// Context is within healthy limits — carry on.
    None,
    /// Fold older messages into a summary; keep the tail.
    Fold,
    /// Exceeded even the exit-with-summary threshold — force
    /// a final summary before ending the turn.
    ExitWithSummary,
}

/// Decision after a turn's response.
#[derive(Debug, Clone, Copy)]
pub struct PostUsageDecision {
    pub kind: PostUsageDecisionKind,
    #[allow(dead_code)]
    pub prompt_tokens: u64,
    #[allow(dead_code)]
    pub ctx_max: u64,
    pub ratio: f64,
    /// Token budget for the recent tail when kind is Fold.
    /// Smaller in the aggressive band.
    pub tail_budget: Option<u64>,
    /// True when this fold is in the aggressive band (78%-80%).
    pub aggressive: bool,
}

/// Turn-start estimate result.
#[derive(Debug, Clone, Copy)]
pub struct TurnStartEstimate {
    pub estimate_tokens: u64,
    pub ctx_max: u64,
    pub ratio: f64,
}

// ================================================================
// Decision logic — port of context-manager.ts:134-177
// ================================================================

/// Decide what to do after a turn's response — fold, exit with
/// summary, or carry on. Port of `ContextManager.decideAfterUsage`
/// (context-manager.ts:134-165).
///
/// `prompt_tokens`: the prompt_tokens value from the API usage
///   response. If `None`, the decision is `None` (no usage data).
/// `ctx_max`: the model's context window size in tokens.
/// `already_folded_this_turn`: true if we already folded earlier
///   in this turn (prevents double-fold).
pub fn decide_after_usage(
    prompt_tokens: Option<u64>,
    ctx_max: u64,
    already_folded_this_turn: bool,
) -> PostUsageDecision {
    decide_after_usage_with_threshold(prompt_tokens, ctx_max, already_folded_this_turn, None)
}

/// MiMo-style incremental checkpoint cadence by context-window size
/// (port of opencode `defaultThresholdsFor`). These are NON-destructive
/// background checkpoint *writes* — they keep the full context and just
/// refresh the durable checkpoint often, so a later overflow/fold almost
/// always has a fresh checkpoint to recover from. They are independent of
/// the destructive fold thresholds above. `< 25K` windows disable the
/// subsystem (too little headroom to be worth it).
pub fn checkpoint_thresholds_for(ctx_max: u64) -> Vec<f64> {
    if ctx_max < 25_000 {
        return Vec::new();
    }
    if ctx_max <= 200_000 {
        return vec![0.2, 0.4, 0.6, 0.8];
    }
    if ctx_max <= 500_000 {
        return (1..=9).map(|i| i as f64 * 0.1).collect();
    }
    (1..=18).map(|i| i as f64 * 0.05).collect()
}

/// Tracks which incremental-checkpoint thresholds a run has crossed so each
/// fires its background writer exactly once. A destructive fold rebuilds
/// the context, so [`reset`](Self::reset) clears the crossed state and the
/// next growth re-checkpoints (mirrors opencode `resetThresholds`).
#[derive(Debug, Clone)]
pub struct CheckpointSchedule {
    thresholds: Vec<f64>,
    crossed: Vec<bool>,
}

impl CheckpointSchedule {
    pub fn new(ctx_max: u64) -> Self {
        let thresholds = checkpoint_thresholds_for(ctx_max);
        let crossed = vec![false; thresholds.len()];
        Self {
            thresholds,
            crossed,
        }
    }

    /// Whether the subsystem is active for this window size.
    pub fn is_enabled(&self) -> bool {
        !self.thresholds.is_empty()
    }

    /// Record the current usage `ratio` (prompt_tokens / ctx_max). Returns
    /// `true` when it has just crossed one or more not-yet-crossed
    /// thresholds — the caller fires a single background checkpoint for
    /// the crossing (multiple thresholds crossed at once → one write).
    pub fn note_usage(&mut self, ratio: f64) -> bool {
        let mut newly = false;
        for (i, &t) in self.thresholds.iter().enumerate() {
            if !self.crossed[i] && ratio >= t {
                self.crossed[i] = true;
                newly = true;
            }
        }
        newly
    }

    /// Clear crossed state after a destructive fold rebuilt the context.
    pub fn reset(&mut self) {
        self.crossed.fill(false);
    }
}

/// Process-wide early-fold threshold, installed once at startup from
/// `Config::compaction_fold_threshold` via [`init_fold_threshold`]. Lets a
/// user opt into MiMo-style earlier checkpointing without threading the
/// value through the whole loop config — same OnceLock-set-once convention
/// as `timeout::Timeouts::init`. Unset → the [`HISTORY_FOLD_THRESHOLD`]
/// default.
static FOLD_THRESHOLD_OVERRIDE: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();

/// Install the configured early-fold threshold process-wide. Idempotent —
/// the first call wins; later calls are ignored. Called once at startup
/// after config load. `None` (or never calling this) keeps the default.
pub fn init_fold_threshold(override_fraction: Option<f64>) {
    let _ = FOLD_THRESHOLD_OVERRIDE.set(override_fraction);
}

/// Default working-context budget in tokens. The effective context for
/// compaction math is `min(model_window, context_target)`. Set to 250k as a
/// reasonable middle ground: newer models (DeepSeek V4, Claude Opus 4.6,
/// Gemini 2.5 Pro) handle this well, while smaller local models stay capped
/// at their actual window via the `min` — the full model-table lookup still
/// applies below this. Set an explicit `context_target` in config.json to
/// lower it (e.g. 100_000) for stricter folding on cost-sensitive routes.
pub const DEFAULT_CONTEXT_TARGET: u64 = 250_000;

/// Floor for a configured target — below this the agent can't get useful
/// work done between folds.
const MIN_CONTEXT_TARGET: u64 = 16_000;

/// Process-wide working-context budget, installed once at startup from
/// `Config::context_target`. The compaction decision treats the effective
/// window as `min(model_window, this)` so a 200k/1M model still folds
/// within the budget instead of drifting into the degradation zone.
static CONTEXT_TARGET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Resolve a configured target to the value to install: `None` →
/// [`DEFAULT_CONTEXT_TARGET`] (250k); a set value is floored at
/// [`MIN_CONTEXT_TARGET`]. Pure, so the floor/default logic is testable
/// without touching the process global.
pub fn resolve_context_target(configured: Option<u64>) -> u64 {
    configured
        .map(|t| t.max(MIN_CONTEXT_TARGET))
        .unwrap_or(DEFAULT_CONTEXT_TARGET)
}

/// Install the working-context budget process-wide. Idempotent (first call
/// wins). `None` (the default) uses [`DEFAULT_CONTEXT_TARGET`] (250k).
pub fn init_context_target(target: Option<u64>) {
    let _ = CONTEXT_TARGET.set(resolve_context_target(target));
}

/// The configured working-context budget (tokens). Default
/// [`DEFAULT_CONTEXT_TARGET`] (250_000).
pub fn context_target() -> u64 {
    *CONTEXT_TARGET.get().unwrap_or(&DEFAULT_CONTEXT_TARGET)
}

/// The effective context window for all compaction math: the smaller of
/// the model's advertised window and the configured budget. When a budget
/// is configured, every existing tier (fold / aggressive / force / turn-start
/// / incremental checkpoint) operates within the budget, keeping the live
/// context in
/// the model's smart zone no matter how large the window claims to be.
pub fn effective_ctx_max(model_window: u64) -> u64 {
    model_window.min(context_target())
}

/// Process-wide explicit context-window override from
/// `Config::context_window`, installed at startup. When set it replaces the
/// built-in model-table lookup for the loop's window (before the
/// [`context_target`] cap), so a user can correct it or supply one for a
/// model the table doesn't know. Previously the loop read the table
/// directly and silently ignored this config; mirroring the other budget
/// knobs as a process global lets the loop honor it without threading the
/// full `Config` through. `None` → use the model table.
static CONTEXT_WINDOW_OVERRIDE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();

/// Install the explicit context-window override. Idempotent.
pub fn init_context_window_override(window: Option<u64>) {
    let _ = CONTEXT_WINDOW_OVERRIDE.set(window);
}

/// The configured context-window override, if any.
pub fn context_window_override() -> Option<u64> {
    CONTEXT_WINDOW_OVERRIDE.get().copied().flatten()
}

/// Process-wide toggle for the incremental background checkpoint. Default
/// ON (mirrors MiMo) — installed once at startup from
/// `Config::incremental_checkpoint`. Only an explicit `Some(false)`
/// disables it.
static INCREMENTAL_CHECKPOINT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Install the incremental-checkpoint toggle. `None` keeps the default-on
/// behavior; `Some(false)` turns it off process-wide.
pub fn init_incremental_checkpoint(enabled: Option<bool>) {
    let _ = INCREMENTAL_CHECKPOINT.set(enabled.unwrap_or(true));
}

/// Whether the incremental background checkpoint is active. Default true.
pub fn incremental_checkpoint_enabled() -> bool {
    *INCREMENTAL_CHECKPOINT.get().unwrap_or(&true)
}

/// Round 2 (memory-awareness feedback): set when background learning
/// (post-session consolidation) has written new memories. The system-prompt
/// memory block is baked at agent-build time, so mid-session writes wouldn't
/// otherwise reach the running agent until restart. The loop consumes this
/// at the next turn boundary and re-injects the refreshed memory block.
static MEMORIES_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark consolidated memories as changed; the running loop re-injects the
/// refreshed block at its next turn boundary. Called by the post-session
/// orchestrator after its learning passes complete.
pub fn mark_memories_dirty() {
    MEMORIES_DIRTY.store(true, std::sync::atomic::Ordering::Release);
}

/// Consume the memory-dirty flag — returns `true` exactly once per `mark`,
/// resetting it. Cheap to poll every turn (a single atomic swap).
pub fn take_memories_dirty() -> bool {
    MEMORIES_DIRTY.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// dirge-0gxb: verbatim pre-recall toggle. When on, the loop auto-searches
/// long-term memory on each turn's verbatim user message and injects the hits
/// as a SUPPLEMENTAL model-facing context block — never into persisted history
/// or the frozen system-prompt snapshot, so it can't churn the prefix cache.
/// Surfaces relevant stored memory the agent might not think to search for.
/// A process-global set once at build time (like [`MEMORIES_DIRTY`]) keeps a
/// single opt-in bool out of every `LoopConfig` literal.
static PRE_RECALL_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set from `config.memory.verbatim_pre_recall` at agent-build time.
pub fn set_verbatim_pre_recall(enabled: bool) {
    PRE_RECALL_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
}

/// Whether the loop should pre-recall this turn. Cheap to poll (one atomic load).
pub fn verbatim_pre_recall_enabled() -> bool {
    PRE_RECALL_ENABLED.load(std::sync::atomic::Ordering::Acquire)
}

/// Max entries surfaced in a pre-recall block — a handful of hints, not a dump.
const PRE_RECALL_LIMIT: usize = 5;

/// dirge-7xi2: a cheap triviality floor for pre-recall. Skip the whole search
/// (and, with hybrid, an embedding round-trip) when the verbatim message has no
/// substantial word — `false` for bare acks like "ok", "yes", "go on", "do it",
/// `true` once any token is ≥ 4 chars ("fix the build", a real question). It's
/// a QUERY-side floor on purpose: BM25 and hybrid score on different scales so a
/// fixed relevance threshold doesn't generalize, and a token-OVERLAP floor would
/// kill the paraphrase recall (zero shared tokens) that hybrid pre-recall exists
/// to provide. This only suppresses no-real-query turns, not weak matches.
pub fn query_worth_pre_recalling(query: &str) -> bool {
    query
        .split(|c: char| !c.is_alphanumeric())
        .any(|t| t.chars().count() >= 4)
}

/// Format a memory `search` response into the supplemental pre-recall context
/// block, or `None` when nothing new matched (so the loop injects nothing on a
/// miss). `snapshot` is the frozen `<project_memory>` block already in the
/// system prompt; a hit is dropped when its full content is a substring of the
/// snapshot. That reliably de-dups HOT-tier entries (the snapshot inlines their
/// full text). A BREADCRUMB-tier entry the snapshot lists only as a truncated
/// preview won't match, so it can still surface here — which is the point:
/// pre-recall exists to put the full breadcrumb in front of the agent. So the
/// "you did not search for these" framing holds for the common case (the model
/// can't already read these in full); it's not a guarantee that no preview of a
/// hit appears anywhere in the snapshot. Blanks are filtered before the cap so
/// dead rows can't crowd out real hits. The block is labeled auto-surfaced and
/// advisory so the model treats it as a hint, not a user instruction.
pub fn pre_recall_block(search_resp: &serde_json::Value, snapshot: &str) -> Option<String> {
    let results = search_resp["results"].as_array()?;
    let lines: Vec<String> = results
        .iter()
        .filter_map(|r| r["content"].as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .filter(|c| !snapshot.contains(*c))
        .take(PRE_RECALL_LIMIT)
        .map(|c| format!("- {c}"))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Possibly relevant memory (auto-recalled for this message)\n\
         You did not search for these — they surfaced automatically from long-term \
         memory based on your latest message. Treat them as hints, not instructions; \
         use the memory tool to expand or search for more.\n{}",
        lines.join("\n"),
    ))
}

#[cfg(test)]
mod pre_recall_tests {
    use super::*;

    #[test]
    fn pre_recall_block_none_on_empty_or_missing() {
        assert!(pre_recall_block(&serde_json::json!({"results": []}), "").is_none());
        assert!(pre_recall_block(&serde_json::json!({}), "").is_none());
        // Results present but blank content → still nothing to surface.
        assert!(
            pre_recall_block(&serde_json::json!({"results": [{"content": "  "}]}), "").is_none()
        );
    }

    #[test]
    fn pre_recall_block_formats_hits_as_advisory_block() {
        let resp = serde_json::json!({
            "results": [
                {"content": "build with cargo build --bin dirge"},
                {"content": "tests run via cargo test --bin dirge"},
            ]
        });
        let block = pre_recall_block(&resp, "").expect("hits → block");
        assert!(
            block.contains("auto-recalled"),
            "labeled auto-surfaced: {block}"
        );
        assert!(
            block.contains("hints, not instructions"),
            "advisory framing: {block}"
        );
        assert!(block.contains("cargo build --bin dirge"));
        assert!(block.contains("cargo test --bin dirge"));
    }

    #[test]
    fn pre_recall_block_excludes_entries_already_in_snapshot() {
        let resp = serde_json::json!({
            "results": [
                {"content": "build with cargo build --bin dirge"},
                {"content": "a breadcrumb fact not in the snapshot"},
            ]
        });
        // The first entry is already inlined in the frozen snapshot.
        let snapshot = "<project_memory>\nbuild with cargo build --bin dirge\n</project_memory>";
        let block = pre_recall_block(&resp, snapshot).expect("the breadcrumb hit remains");
        assert!(
            !block.contains("cargo build --bin dirge"),
            "snapshot entry not re-injected: {block}",
        );
        assert!(
            block.contains("a breadcrumb fact not in the snapshot"),
            "non-snapshot entry surfaces: {block}",
        );
    }

    #[test]
    fn query_worth_pre_recalling_floors_trivial_acks() {
        // Bare acknowledgments / contentless turns → skip.
        for trivial in ["", "  ", "ok", "yes", "go on", "do it", "k", "yep!"] {
            assert!(
                !query_worth_pre_recalling(trivial),
                "should skip trivial query {trivial:?}",
            );
        }
        // Anything with a substantial word → search.
        for real in ["fix the build", "how do I cache the widget", "rollback"] {
            assert!(
                query_worth_pre_recalling(real),
                "should pre-recall real query {real:?}",
            );
        }
    }

    #[test]
    fn pre_recall_block_filters_blanks_before_capping() {
        // Three blanks up front then six real hits: the cap must apply to the
        // SURVIVORS, not raw rows (else blanks would crowd out real hits).
        let mut rows: Vec<_> = (0..3)
            .map(|_| serde_json::json!({"content": "   "}))
            .collect();
        rows.extend((0..6).map(|i| serde_json::json!({"content": format!("real fact {i}")})));
        let block = pre_recall_block(&serde_json::json!({"results": rows}), "").unwrap();
        let bullets = block.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            bullets, PRE_RECALL_LIMIT,
            "caps survivors, not raw rows: {block}"
        );
    }

    /// The core supplemental-not-replacing guarantee (dirge-0gxb): running the
    /// pre-recall path (search + format) leaves the frozen system-prompt
    /// snapshot byte-identical. Pre-recall adds a separate context message; it
    /// must never touch the snapshot (which would churn the prefix cache).
    #[test]
    fn pre_recall_leaves_the_frozen_snapshot_byte_identical() {
        use crate::extras::dirge_paths::ProjectPaths;
        use crate::extras::memory_db::{MemoryKind, SqliteMemoryStore};

        let dir = std::env::temp_dir().join(format!(
            "dirge-prerecall-snap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let paths = ProjectPaths::new(&dir);

        // Seed an entry, then reload so it's captured in the frozen snapshot.
        {
            let seed = SqliteMemoryStore::load(&paths).unwrap();
            seed.add_entry(
                "memory",
                "build with cargo build --bin dirge",
                Some(MemoryKind::Procedural),
            )
            .unwrap();
        }
        let store = SqliteMemoryStore::load(&paths).unwrap();
        let before = store.format_for_system_prompt();
        assert!(before.contains("cargo build"), "entry is in the snapshot");

        // Run the pre-recall path: search the verbatim message, format a block.
        // (Empty snapshot arg here so the hit surfaces — snapshot-dedup is
        // covered separately; this test isolates snapshot immutability.)
        let resp = store.search_entries("build cargo").unwrap();
        assert!(
            pre_recall_block(&resp, "").is_some(),
            "pre-recall surfaced the seeded entry"
        );

        let after = store.format_for_system_prompt();
        assert_eq!(
            before, after,
            "pre-recall must leave the frozen snapshot byte-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Clamp a configured early-fold threshold into a safe range. An override
/// may only make the NORMAL fold fire *earlier* — never later than the
/// default and never below a floor that would fold almost immediately —
/// so the aggressive/force bands above it keep their ordering. An explicit
/// `override_fraction` wins (used by callers and tests); otherwise the
/// startup-installed process global is consulted; out-of-range or absent
/// values fall back to [`HISTORY_FOLD_THRESHOLD`].
pub fn effective_fold_threshold(override_fraction: Option<f64>) -> f64 {
    let candidate = override_fraction.or_else(|| FOLD_THRESHOLD_OVERRIDE.get().copied().flatten());
    match candidate {
        Some(f) if f.is_finite() && (0.3..=HISTORY_FOLD_THRESHOLD).contains(&f) => f,
        _ => HISTORY_FOLD_THRESHOLD,
    }
}

/// As [`decide_after_usage`], but with a configurable early-fold threshold
/// (MiMo's "checkpoint/compress earlier" knob). A lower threshold folds —
/// and therefore writes the durable checkpoint — sooner and from more
/// coherent context, at the cost of more frequent folds. The aggressive
/// and force-summary bands are unchanged; the override is clamped by
/// [`effective_fold_threshold`] so it can only lower the normal band.
pub fn decide_after_usage_with_threshold(
    prompt_tokens: Option<u64>,
    ctx_max: u64,
    already_folded_this_turn: bool,
    fold_threshold_override: Option<f64>,
) -> PostUsageDecision {
    let Some(prompt_tokens) = prompt_tokens else {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::None,
            prompt_tokens: 0,
            ctx_max,
            ratio: 0.0,
            tail_budget: None,
            aggressive: false,
        };
    };
    if ctx_max == 0 {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::None,
            prompt_tokens,
            ctx_max,
            ratio: 0.0,
            tail_budget: None,
            aggressive: false,
        };
    }
    let ratio = prompt_tokens as f64 / ctx_max as f64;

    if ratio > FORCE_SUMMARY_THRESHOLD {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::ExitWithSummary,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: None,
            aggressive: false,
        };
    }

    if already_folded_this_turn {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::None,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: None,
            aggressive: false,
        };
    }

    if ratio > HISTORY_FOLD_AGGRESSIVE_THRESHOLD {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::Fold,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: Some((ctx_max as f64 * HISTORY_FOLD_AGGRESSIVE_TAIL_FRACTION) as u64),
            aggressive: true,
        };
    }

    if ratio > effective_fold_threshold(fold_threshold_override) {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::Fold,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: Some((ctx_max as f64 * HISTORY_FOLD_TAIL_FRACTION) as u64),
            aggressive: false,
        };
    }

    PostUsageDecision {
        kind: PostUsageDecisionKind::None,
        prompt_tokens,
        ctx_max,
        ratio,
        tail_budget: None,
        aggressive: false,
    }
}

/// Turn-start estimate vs ctxMax. Caller folds if the ratio
/// crosses TURN_START_FOLD_THRESHOLD. Port of
/// `ContextManager.estimateTurnStart`
/// (context-manager.ts:167-177).
///
/// `estimate_tokens`: a local estimate of total request tokens
///   (messages + tools + system prompt).
/// `ctx_max`: the model's context window size in tokens.
pub fn estimate_turn_start(estimate_tokens: u64, ctx_max: u64) -> TurnStartEstimate {
    let ratio = if ctx_max == 0 {
        f64::INFINITY
    } else {
        estimate_tokens as f64 / ctx_max as f64
    };
    TurnStartEstimate {
        estimate_tokens,
        ctx_max,
        ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // working-context budget
    // ============================================================

    #[test]
    fn resolve_context_target_defaults_and_floors() {
        assert_eq!(resolve_context_target(None), DEFAULT_CONTEXT_TARGET);
        assert_eq!(resolve_context_target(Some(50_000)), 50_000);
        // A tiny configured target is floored so the agent can still work.
        assert_eq!(resolve_context_target(Some(1_000)), MIN_CONTEXT_TARGET);
        // A target above the default is kept; min(window, target)
        // applies the real cap later.
        assert_eq!(resolve_context_target(Some(500_000)), 500_000);
    }

    #[test]
    fn default_budget_caps_at_250k() {
        // DEFAULT_CONTEXT_TARGET is 250k. Models with window >= 250k are
        // capped at 250k.
        assert_eq!(effective_ctx_max(1_000_000), 250_000);
        assert_eq!(effective_ctx_max(2_000_000), 250_000);
        // Models with window < 250k use their own window.
        assert_eq!(effective_ctx_max(128_000), 128_000);
        // Fold threshold at 0.75 * 250k = 187_500.
        assert_eq!(
            decide_after_usage(Some(187_501), 250_000, false).kind,
            PostUsageDecisionKind::Fold
        );
        assert_eq!(
            decide_after_usage(Some(187_500), 250_000, false).kind,
            PostUsageDecisionKind::None
        );
    }

    #[test]
    fn capped_budget_folds_within_the_target() {
        // With a 100k working budget the normal fold fires at 0.75 * 100k
        // = 75k, keeping the live context in the smart zone no matter how
        // large the advertised window is.
        let budget = 100_000u64;
        assert_eq!(
            decide_after_usage(Some(74_000), budget, false).kind,
            PostUsageDecisionKind::None
        );
        assert_eq!(
            decide_after_usage(Some(76_000), budget, false).kind,
            PostUsageDecisionKind::Fold
        );
    }

    // ============================================================
    // incremental checkpoint schedule
    // ============================================================

    #[test]
    fn checkpoint_cadence_matches_mimo_by_window() {
        assert!(
            checkpoint_thresholds_for(8_000).is_empty(),
            "tiny window disabled"
        );
        assert_eq!(checkpoint_thresholds_for(128_000), vec![0.2, 0.4, 0.6, 0.8]);
        assert_eq!(checkpoint_thresholds_for(200_000), vec![0.2, 0.4, 0.6, 0.8]);
        assert_eq!(checkpoint_thresholds_for(400_000).len(), 9, "10% cadence");
        assert_eq!(checkpoint_thresholds_for(1_000_000).len(), 18, "5% cadence");
    }

    #[test]
    fn schedule_fires_each_threshold_once_until_reset() {
        let mut s = CheckpointSchedule::new(128_000); // [.2,.4,.6,.8]
        assert!(s.is_enabled());
        assert!(!s.note_usage(0.1), "below first threshold");
        assert!(s.note_usage(0.25), "crossed 20%");
        assert!(!s.note_usage(0.30), "no new threshold");
        assert!(s.note_usage(0.45), "crossed 40%");
        // Jumping past several at once is a single firing.
        assert!(s.note_usage(0.85), "crossed 60% and 80% together");
        assert!(!s.note_usage(0.9), "all crossed");
        // A destructive fold rebuilds context → re-arm.
        s.reset();
        assert!(s.note_usage(0.25), "fires again after reset");
    }

    #[test]
    fn disabled_schedule_never_fires() {
        let mut s = CheckpointSchedule::new(10_000);
        assert!(!s.is_enabled());
        assert!(!s.note_usage(0.99));
    }

    // ============================================================
    // early-fold threshold override
    // ============================================================

    /// The override is clamped: a value in `0.3..=0.75` is honored; out of
    /// range or absent falls back to the default. Uses explicit params so
    /// it doesn't depend on the process-global install.
    #[test]
    fn effective_fold_threshold_clamps_override() {
        assert_eq!(effective_fold_threshold(Some(0.5)), 0.5);
        assert_eq!(effective_fold_threshold(Some(0.3)), 0.3);
        assert_eq!(
            effective_fold_threshold(Some(0.9)),
            HISTORY_FOLD_THRESHOLD,
            "above the default is rejected (can't fold later)"
        );
        assert_eq!(
            effective_fold_threshold(Some(0.05)),
            HISTORY_FOLD_THRESHOLD,
            "below the floor is rejected"
        );
        assert_eq!(
            effective_fold_threshold(Some(f64::NAN)),
            HISTORY_FOLD_THRESHOLD
        );
        assert_eq!(effective_fold_threshold(None), HISTORY_FOLD_THRESHOLD);
    }

    /// A lower override folds sooner: a ratio that is healthy at the
    /// default (0.75) becomes a Fold under an earlier threshold, while the
    /// aggressive/force bands above it are unchanged.
    #[test]
    fn lower_override_folds_earlier() {
        // 60% of the window: no fold at the default threshold…
        let d = decide_after_usage_with_threshold(Some(76_800), 128_000, false, None);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
        // …but folds with an early 0.5 threshold.
        let d = decide_after_usage_with_threshold(Some(76_800), 128_000, false, Some(0.5));
        assert_eq!(d.kind, PostUsageDecisionKind::Fold);
        assert!(!d.aggressive, "still the normal band, not aggressive");
        // The force-summary band is independent of the override.
        let d = decide_after_usage_with_threshold(Some(110_000), 128_000, false, Some(0.5));
        assert_eq!(d.kind, PostUsageDecisionKind::ExitWithSummary);
    }

    // ============================================================
    // decide_after_usage
    // ============================================================

    #[test]
    fn no_usage_data_returns_none() {
        let d = decide_after_usage(None, 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
        assert_eq!(d.ratio, 0.0);
    }

    #[test]
    fn below_threshold_returns_none() {
        // 50K out of 128K = ~39% → below 75% threshold
        let d = decide_after_usage(Some(50_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
    }

    #[test]
    fn above_75pct_triggers_fold() {
        // 98K out of 128K = ~76.5% → above 75%, below 78%
        let d = decide_after_usage(Some(98_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::Fold);
        assert!(!d.aggressive);
        // Tail budget: 20% of 128K = 25600
        assert_eq!(d.tail_budget, Some(25600));
    }

    #[test]
    fn above_78pct_triggers_aggressive_fold() {
        // 101K out of 128K = ~78.9% → above 78%
        let d = decide_after_usage(Some(101_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::Fold);
        assert!(d.aggressive);
        // Aggressive tail budget: 10% of 128K = 12800
        assert_eq!(d.tail_budget, Some(12800));
    }

    #[test]
    fn above_80pct_triggers_exit_with_summary() {
        // 105K out of 128K = ~82% → above 80%
        let d = decide_after_usage(Some(105_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::ExitWithSummary);
    }

    #[test]
    fn already_folded_prevents_double_fold() {
        // Even though ratio is above 75%, we don't fold again
        let d = decide_after_usage(Some(100_000), 128_000, true);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
    }

    #[test]
    fn already_folded_does_not_prevent_exit_with_summary() {
        // Above 80% still triggers exit even if already folded
        let d = decide_after_usage(Some(105_000), 128_000, true);
        assert_eq!(d.kind, PostUsageDecisionKind::ExitWithSummary);
    }

    #[test]
    fn zero_ctx_max_handled_gracefully() {
        // ctx_max == 0 is degenerate (unknown model, config error).
        // Guard returns None rather than computing inf/NaN ratio.
        let d = decide_after_usage(Some(1000), 0, false);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
    }

    // ============================================================
    // estimate_turn_start
    // ============================================================

    #[test]
    fn estimate_below_threshold() {
        let e = estimate_turn_start(50_000, 128_000);
        assert!(e.ratio < TURN_START_FOLD_THRESHOLD);
        assert_eq!(e.ctx_max, 128_000);
    }

    #[test]
    fn estimate_above_threshold() {
        let e = estimate_turn_start(120_000, 128_000);
        assert!(e.ratio > TURN_START_FOLD_THRESHOLD);
    }

    #[test]
    fn estimate_at_boundary() {
        let boundary = (128_000.0 * TURN_START_FOLD_THRESHOLD) as u64;
        let e = estimate_turn_start(boundary, 128_000);
        // At exactly the threshold — caller decides whether to fold
        assert!((e.ratio - TURN_START_FOLD_THRESHOLD).abs() < 0.001);
    }

    // ============================================================
    // Threshold constant sanity
    // ============================================================

    // Compile-time invariants: a violation is now a build error rather
    // than a test failure. (clippy::assertions_on_constants flagged the
    // former runtime `assert!`s because both operands are consts.)
    const _: () = assert!(FORCE_SUMMARY_THRESHOLD > HISTORY_FOLD_AGGRESSIVE_THRESHOLD);
    const _: () = assert!(HISTORY_FOLD_AGGRESSIVE_THRESHOLD > HISTORY_FOLD_THRESHOLD);
    const _: () = assert!(HISTORY_FOLD_THRESHOLD > HISTORY_FOLD_MIN_SAVINGS_FRACTION);
    const _: () = assert!(HISTORY_FOLD_AGGRESSIVE_TAIL_FRACTION < HISTORY_FOLD_TAIL_FRACTION);
}

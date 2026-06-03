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

    if ratio > HISTORY_FOLD_THRESHOLD {
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

    #[test]
    fn thresholds_are_strictly_ordered() {
        assert!(FORCE_SUMMARY_THRESHOLD > HISTORY_FOLD_AGGRESSIVE_THRESHOLD);
        assert!(HISTORY_FOLD_AGGRESSIVE_THRESHOLD > HISTORY_FOLD_THRESHOLD);
        assert!(HISTORY_FOLD_THRESHOLD > HISTORY_FOLD_MIN_SAVINGS_FRACTION);
    }

    #[test]
    fn aggressive_tail_is_smaller_than_normal_tail() {
        assert!(HISTORY_FOLD_AGGRESSIVE_TAIL_FRACTION < HISTORY_FOLD_TAIL_FRACTION);
    }
}

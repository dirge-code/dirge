//! Shared salience machinery (dirge-mlxa).
//!
//! The pure, store-agnostic pieces of the memory ranking/eviction model,
//! extracted from `memory_db.rs` so a second store — the `skills` table
//! (dirge-70ht) — can reuse the exact same decay, effectiveness, and
//! confidence math instead of reimplementing it. A learned skill is a
//! named procedural memory with supporting files, so it wants the same
//! signals: reinforce on use, decay on disuse, effectiveness from a
//! success/failure record, confidence as a tiebreak.
//!
//! Only the generic constants and the two pure scoring functions live
//! here. Kind-derived base salience stays with each store (memories key
//! it on `MemoryKind`; skills use a single procedural-like base), and so
//! do store-specific policies like the working-memory reserve and the
//! supersession-confidence values.

// ── Usage-driven lifecycle (dirge-jyks) ──────────────────────────────

/// How recently an entry must have been used (expanded / invoked) to
/// count as "in active use" for eviction decisions.
pub const RECENT_USE_WINDOW_DAYS: i64 = 14;

/// Effective-salience bonus for recently-used entries during eviction.
/// 0.15 is half a kind-tier step: enough that a consulted `working`
/// note (0.3 → 0.45) outlives an untouched `episodic` one (0.45 ties
/// break by age), without letting use alone outrank a durable
/// `identity` fact.
pub const RECENT_USE_BONUS: f64 = 0.15;

/// Salience reinforcement applied on each use (`expand` / `invoke`) —
/// being looked up IS the relevance signal. Capped at 1.0 by callers.
pub const USE_REINFORCEMENT: f64 = 0.05;

/// Periodic decay applied by the curator's mechanical pass to entries
/// older than the stale window with no recent use. Floor at 0.1 so
/// nothing decays to oblivion silently.
pub const DISUSE_DECAY: f64 = 0.05;
pub const DECAY_FLOOR: f64 = 0.1;

// ── Recurrence-weighted graduation (dirge-4nix) ──────────────────────

/// Salience bump for each additional member beyond the first in a
/// recurrence cluster. A cluster of size N adds
/// RECURRENCE_SALIENCE_STEP * (N - 1) to the representative,
/// capped at SALIENCE_CAP.
pub const RECURRENCE_SALIENCE_STEP: f64 = 0.05;
/// Absolute ceiling on salience from recurrence graduation.
pub const SALIENCE_CAP: f64 = 0.9;

// ── Procedural effectiveness (dirge-zygq) ────────────────────────────

/// Weight on the log-damped net success/failure record. With 0.15:
/// net `log10(1+|net|)*0.15`, so +1 ≈ +0.045, +9 ≈ +0.15, +99
/// saturates at the [`EFFECTIVENESS_CAP`].
pub const EFFECTIVENESS_WEIGHT: f64 = 0.15;
/// Bound on the effectiveness term so a hot playbook can't outrank a
/// durable identity fact (0.75) on its record alone.
pub const EFFECTIVENESS_CAP: f64 = 0.3;

/// Fold a net success/failure record into an effective-salience delta.
///
/// Pure math: the caller decides whether the entry carries an outcome
/// signal at all (memories gate this on the `procedural` kind; skills
/// always do). Log-damped and bounded by [`EFFECTIVENESS_CAP`]:
/// +1 ≈ +0.045, +9 ≈ +0.15, +99 saturates at +0.30 (intermediate
/// records sit between — e.g. +19 ≈ +0.20); failures mirror negative.
/// Returns 0 for an even record.
pub fn effectiveness_bonus(success_count: i64, failure_count: i64) -> f64 {
    let net = success_count - failure_count;
    if net == 0 {
        return 0.0;
    }
    let magnitude = ((1 + net.unsigned_abs()) as f64).log10() * EFFECTIVENESS_WEIGHT;
    let bounded = magnitude.min(EFFECTIVENESS_CAP);
    if net > 0 { bounded } else { -bounded }
}

// ── Confidence axis (dirge-fa10) ─────────────────────────────────────

/// Truth-likelihood of an entry, in [0,1]. Distinct from salience
/// (importance) — a fact can be important but contested, or trivial but
/// certain. Default for a freshly captured entry.
pub const DEFAULT_CONFIDENCE: f64 = 0.6;

/// Eviction weight on confidence. Decisive role is as a TIEBREAK: among
/// entries of equal salience the lower-confidence one evicts first.
/// Across different kinds it's only a gentle nudge — 0.25 keeps the full
/// [0,1] swing within ±0.1, below the 0.1–0.15 gaps between kind tiers,
/// so a contested fact never jumps the kind hierarchy. Centered on
/// [`DEFAULT_CONFIDENCE`] so the common case stays neutral.
pub const CONFIDENCE_EVICTION_WEIGHT: f64 = 0.25;

/// Map a confidence value to its eviction-salience delta, centered so
/// the default is neutral and the full [0,1] range spans ±0.1.
pub fn confidence_eviction_bonus(confidence: f64) -> f64 {
    (confidence - DEFAULT_CONFIDENCE) * CONFIDENCE_EVICTION_WEIGHT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effectiveness_is_zero_for_even_record() {
        assert_eq!(effectiveness_bonus(0, 0), 0.0);
        assert_eq!(effectiveness_bonus(3, 3), 0.0);
        assert_eq!(effectiveness_bonus(10_000, 10_000), 0.0);
    }

    #[test]
    fn effectiveness_is_signed_log_damped_and_capped() {
        // +1 ≈ 0.045, +9 ≈ 0.15 (documented anchor points).
        assert!((effectiveness_bonus(1, 0) - 0.045).abs() < 0.005);
        assert!((effectiveness_bonus(9, 0) - 0.15).abs() < 0.005);

        // Symmetric under sign flip.
        let up = effectiveness_bonus(3, 0);
        let down = effectiveness_bonus(0, 3);
        assert!(up > 0.0 && down < 0.0);
        assert!((up + down).abs() < 1e-9);

        // Saturates at the cap in both directions.
        assert!(effectiveness_bonus(10_000, 0) <= EFFECTIVENESS_CAP + 1e-9);
        assert!(effectiveness_bonus(10_000, 0) > EFFECTIVENESS_CAP - 0.05);
        assert!(effectiveness_bonus(0, 10_000) >= -EFFECTIVENESS_CAP - 1e-9);
    }

    #[test]
    fn confidence_bonus_is_neutral_at_default_and_bounded() {
        assert_eq!(confidence_eviction_bonus(DEFAULT_CONFIDENCE), 0.0);
        // Centered on 0.6, so the swing runs -0.15 (certain-false) to +0.1
        // (certain-true) — small enough that it can't jump a kind tier.
        assert!((confidence_eviction_bonus(1.0) - 0.1).abs() < 1e-9);
        assert!((confidence_eviction_bonus(0.0) + 0.15).abs() < 1e-9);
    }
}

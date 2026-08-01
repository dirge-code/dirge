//! Safe-state abort — the third rung of EXEC's failure ladder (dirge-uw2l.4).
//!
//! EXEC (the paper's Smart Executive) handles a task it can't achieve with a
//! three-rung ladder: try an alternate method (rung 1); request a recovery
//! from MIR (rung 2); and if it still can't execute or repair the plan,
//! cleanly abort the plan and bring the spacecraft to a safe state while
//! requesting a new plan (rung 3). dirge already had rung 1 (the storm
//! breaker + the repeat-loop guard's reflect-then-pivot, with the reflexion
//! log accumulating every abandoned approach) and rung 2 (the failure
//! tracker's recovery checkpoint at the threshold). This module is rung 3:
//! when the failure streak reaches 2× the checkpoint threshold AND unverified
//! edits sit on the tree AND a verified-green point exists behind the run, it
//! replaces that boundary's checkpoint with a single message that aborts the
//! current approach and asks for a fresh plan from the last green state.
//!
//! `advisory` mode performs NO file writes — it tells the model the tree is
//! unverified on top of a known-good point and lets it decide whether to undo
//! its post-green edits and re-plan. It deliberately names no restore
//! command: `/rewind` does not exist as a slash command (the picker is
//! Esc-Esc, keyed by user-message INDEX, not by the snapshot turn id stamped
//! here), and rewinding *to* the green turn would revert the green-making
//! work itself, since snapshots hold pre-mutation state.
//!
//! `auto` mode (dirge-uw2l.6) performs the restore in the harness. It is
//! gated on a coverage proof, because `snapshots::capture` is wired into the
//! edit tools and not into `bash`: a `sed -i` or an in-place formatter
//! mutates a file with no pre-state recorded, and restoring the captured
//! edits while leaving that alone yields a tree in a state that never
//! existed. So before writing anything, the loop diffs a git fingerprint of
//! the tree taken at the green moment against one taken now
//! ([`super::worktree_probe`]) and proceeds only when the snapshot store can
//! put back every file that changed. Anything short of that — no repo, no
//! fingerprint, one uncaptured file — declines to the advisory wording. The
//! failure mode is "auto didn't fire", never "auto left a broken tree".
//!
//! # Why this can't loop
//!
//! A safe-state abort consumes one unit of the hard cap
//! [`MAX_SAFE_STATE_ABORTS`] = 2. The cap is monotonic — it only ever
//! increments, never decrements — so across an entire run at most two aborts
//! fire, after which the rung is inert (advisory emits nothing further).
//! Between fires, the once-per-streak guard (`fired_this_streak`) blocks a
//! second abort within the SAME failure streak: it only clears when the
//! streak resets (observed as the `due` signal going false, which happens on
//! any successful tool result), and re-climbing to 2× the threshold costs ≥6
//! weighted failures. Therefore any chain abort→(fresh failures)→abort must
//! traverse a full streak reset and re-climb between each fire, and is bounded
//! by the hard cap regardless. There is no transition from "fired" back to
//! "fireable" that doesn't either spend the cap or require a fresh 6-failure
//! climb; concretely the worst case over a whole run is two advisory messages,
//! then silence.
//!
//! Self-contained — no rig/LLM state. Owned as a local in `run_loop`; when the
//! loop wires it with [`SafeStateMode::Off`] (the default), behaviour is
//! byte-identical to the loop without the rung.

use super::reflexion::ReflectionLog;
use super::types::SafeStateMode;

/// Max safe-state aborts per run. Monotonic budget; see the module's
/// can't-loop argument. Two mirrors the paper's "abort and re-plan"
/// happening at most twice before a human-level stop is warranted.
pub const MAX_SAFE_STATE_ABORTS: u8 = 2;

/// Display tag prefixing the safe-state replan message. The UI keys on this
/// to attribute it to the system rather than the user, and the headless
/// harness-notice mirror (dirge-uw2l.7) uses it to surface the injection.
pub const SAFE_STATE_TAG: &str = "[safe-state]";

/// Per-run state for the safe-state abort rung. Owned as a local in
/// `run_loop`, persists across the outer (turn) loop so a green point from an
/// earlier turn is still a valid re-plan target later.
#[derive(Debug, Default)]
pub struct SafeStateEngine {
    /// The user-turn bucket id stamped at the most recent verified-green
    /// moment. `None` until verification has gone green this run. Only ever
    /// stamped on a turn that ENDED fresh-green (see [`Self::decide`]), so a
    /// later restore targets real green content, not a stale label.
    last_green_turn: Option<String>,
    /// Safe-state aborts already spent this run. Monotonic — bounds the total.
    aborts_used: u8,
    /// Whether an abort already fired for the current failure streak. Cleared
    /// when the streak resets (observed as `due` going false), so only a fresh
    /// 2× crossing can fire again.
    fired_this_streak: bool,
    /// Working-tree fingerprint captured at the same moment as
    /// `last_green_turn` (dirge-uw2l.6). Diffed against a fresh sample at
    /// abort time to find every file mutated since green — including ones
    /// `bash` changed, which the snapshot store never sees.
    last_green_fingerprint: Option<super::worktree_probe::TreeFingerprint>,
}

impl SafeStateEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Working-tree fingerprint taken at the same green moment
    /// (dirge-uw2l.6). `None` until a green has been seen, or when the
    /// project isn't a git work tree — either way auto declines.
    pub fn green_fingerprint(&self) -> Option<&super::worktree_probe::TreeFingerprint> {
        self.last_green_fingerprint.as_ref()
    }

    /// Stamp the green fingerprint. Called by the loop at the same instant
    /// it observes `fresh_green`, so the marker and the fingerprint always
    /// describe the same moment.
    pub fn set_green_fingerprint(&mut self, fp: Option<super::worktree_probe::TreeFingerprint>) {
        self.last_green_fingerprint = fp;
    }

    /// Boundary decision (dirge-uw2l.4). Called once per outer-loop iteration.
    /// Returns the safe-state replan message body when the rung fires, else
    /// `None` — and the caller then falls back to the rung-2 recovery
    /// checkpoint, so safe-state REPLACES (never adds to) that boundary's
    /// message.
    ///
    /// - `fresh_green`: the verifier reports the tree is at a verified-green
    ///   point right now (used to stamp `last_green_turn`).
    /// - `due`: the failure tracker's streak has reached 2× the checkpoint
    ///   threshold.
    /// - `edits_since_verify`: unverified code edits sitting on the tree.
    /// - `current_turn_id`: the active user-turn bucket id (for stamping and
    ///   for naming the green turn in the message).
    /// - `reflections` / `excerpts`: carried into the message so the re-plan
    ///   doesn't repeat an abandoned approach or a tool that just failed.
    #[allow(clippy::too_many_arguments)] // one fused boundary decision; the
    // inputs are each genuinely independent
    // and bundling them would obscure the
    // rung's conditions.
    pub fn decide(
        &mut self,
        mode: SafeStateMode,
        fresh_green: bool,
        due: bool,
        edits_since_verify: u32,
        current_turn_id: Option<&str>,
        reflections: &ReflectionLog,
        excerpts: &[(String, String)],
        // dirge-uw2l.6: invoked with the green turn id ONLY after every gate
        // has passed and only in [`SafeStateMode::Auto`], so the expensive,
        // destructive part never runs for a boundary that wouldn't have
        // fired. Returns `Some(n)` when it verified coverage and restored
        // `n` files, `None` when it declined — which degrades to the
        // advisory wording. Taking it as a closure keeps the gating in one
        // place and keeps `decide` unit-testable with no filesystem.
        resolve_restore: impl FnOnce(&str) -> Option<usize>,
    ) -> Option<String> {
        // Off is byte-identical to the loop without the rung: no stamping, no
        // message, no state churn. This short-circuit is what keeps the
        // default a no-op.
        if mode == SafeStateMode::Off {
            return None;
        }
        // Stamp the green marker at a fresh-green moment. Because the caller
        // passes `fresh_green` only when the tree is currently verified-green
        // (no edits since the last passing verify), the stamped turn ENDED
        // green — so restoring from the turn after it lands on real green
        // content. See snapshots::restore_after_green_turn.
        if fresh_green {
            self.last_green_turn = current_turn_id.map(str::to_owned);
        }
        // A streak that isn't due (below 2×, or just reset by a success)
        // re-arms the once-per-streak guard. Returning here also means a
        // green boundary (which resets the tracker's streak) clears
        // fired_this_streak via due=false.
        if !due {
            self.fired_this_streak = false;
            return None;
        }
        // Four gates, each independently sufficient to decline.
        if self.aborts_used >= MAX_SAFE_STATE_ABORTS {
            return None; // hard cap spent — inert for the rest of the run
        }
        if self.fired_this_streak {
            return None; // already aborted this streak; wait for a reset
        }
        if edits_since_verify == 0 {
            return None; // nothing unverified to abort away from
        }
        let Some(green) = self.last_green_turn.as_deref() else {
            return None; // no known-good state to re-plan from
        };
        // Fire: spend one abort unit, mark this streak as already-aborted.
        self.fired_this_streak = true;
        self.aborts_used = self.aborts_used.saturating_add(1);
        // The green marker gates the decision and names the restore target,
        // but is deliberately NOT shown to the model: it is an internal
        // snapshot bucket id the model cannot act on.
        let restored_files = if mode == SafeStateMode::Auto {
            resolve_restore(green)
        } else {
            None
        };
        Some(format_safe_state(
            reflections.block().as_deref(),
            excerpts,
            restored_files,
        ))
    }
}

/// The safe-state replan body (dirge-uw2l.4). Free fn so tests pin the wording.
///
/// Carries the two things a re-plan must not repeat — the reflexion log's
/// abandoned approaches and the streak's failure excerpts — and enforces
/// MIR's single-recovery-action discipline: it asks for ONE next approach,
/// not a menu (a menu invites cycling back through the very dead ends the
/// reflexion log just enumerated).
fn format_safe_state(
    reflections_block: Option<&str>,
    excerpts: &[(String, String)],
    restored_files: Option<usize>,
) -> String {
    let mut s = format!(
        "{SAFE_STATE_TAG} Repeated tool failures have left the working tree in an \
         unverified state on top of the last check that passed. The current \
         approach has failed. The failure ladder has run its course — an \
         alternate method was tried (rung 1) and a recovery was requested \
         (rung 2) — so this rung aborts the approach and asks for a fresh plan \
         from the last known-good state.\n"
    );
    if let Some(block) = reflections_block {
        s.push_str(block);
    }
    if !excerpts.is_empty() {
        s.push_str("\n\nTools that just failed this streak — do not return to any of these:\n");
        for (tool, excerpt) in excerpts {
            s.push_str(&format!("  - {tool}: {excerpt}\n"));
        }
    }
    match restored_files {
        // Auto: the harness already put the tree back, so telling the model
        // to undo its edits would send it chasing changes that are gone.
        Some(n) => s.push_str(&format!(
            "\nThe harness has reverted {n} file{} to that last known-good state, \
             so the tree you are looking at is the one that passed. Your \
             post-check edits are gone — do not try to undo them again. Propose \
             ONE new approach (not a menu) that differs from everything above, \
             and verify it before going further.\n",
            if n == 1 { "" } else { "s" }
        )),
        None => s.push_str(
            "\nConsider undoing the edits you've made since that last passing check \
             before continuing — your own post-check changes only, not unrelated \
             working-tree state — so you re-plan from a state that was known to work \
             rather than on top of a broken one. Then propose ONE new approach (not a \
             menu) that differs from everything above, and verify it before going \
             further.\n",
        ),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::reflexion::ReflectionLog;

    #[test]
    fn off_mode_is_a_no_op() {
        // Off must be byte-identical to the loop without the rung: even with
        // every fire condition met, it returns None AND does no state churn
        // (no green stamp, no spent abort).
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        let msg = engine.decide(
            SafeStateMode::Off,
            true,
            true,
            1,
            Some("u1"),
            &refl,
            &[("edit".into(), "no match".into())],
            |_| None,
        );
        assert!(msg.is_none(), "off never emits");
        assert_eq!(
            engine.last_green_turn.as_deref(),
            None,
            "off does not stamp green"
        );
        assert_eq!(engine.aborts_used, 0, "off spends nothing");
    }

    #[test]
    fn advisory_fires_when_due_mutated_and_green_seen() {
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        // Stamp a green point in an earlier turn.
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &refl,
            &[],
            |_| None,
        );
        // A deep failing streak on a later turn with unverified edits on the
        // tree fires the rung.
        let msg = engine.decide(
            SafeStateMode::Advisory,
            false,
            true,
            1,
            Some("u2"),
            &refl,
            &[("edit".into(), "no match".into())],
            |_| None,
        );
        assert!(msg.is_some(), "due + mutated + green seen fires advisory");
    }

    #[test]
    fn declines_when_green_never_seen() {
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        // No green stamp — there is no known-good state to re-plan from.
        let msg = engine.decide(
            SafeStateMode::Advisory,
            false,
            true,
            1,
            Some("u2"),
            &refl,
            &[("edit".into(), "no match".into())],
            |_| None,
        );
        assert!(msg.is_none(), "no green seen -> decline");
    }

    #[test]
    fn declines_when_no_unverified_edits() {
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &refl,
            &[],
            |_| None,
        );
        // edits_since_verify == 0: the tree is verified, nothing to abort away
        // from — a safe-state abort here would be pointless.
        let msg = engine.decide(
            SafeStateMode::Advisory,
            false,
            true,
            0,
            Some("u2"),
            &refl,
            &[],
            |_| None,
        );
        assert!(msg.is_none(), "no unverified edits -> decline");
    }

    #[test]
    fn bounded_at_max_two_aborts() {
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &refl,
            &[],
            |_| None,
        );
        // Streak 1 fires (aborts_used = 1).
        assert!(
            engine
                .decide(
                    SafeStateMode::Advisory,
                    false,
                    true,
                    1,
                    Some("u2"),
                    &refl,
                    &[],
                    |_| None,
                )
                .is_some()
        );
        // Reset between streaks (a success cleared the tracker).
        engine.decide(
            SafeStateMode::Advisory,
            false,
            false,
            1,
            Some("u2"),
            &refl,
            &[],
            |_| None,
        );
        // Streak 2 fires (aborts_used = 2).
        assert!(
            engine
                .decide(
                    SafeStateMode::Advisory,
                    false,
                    true,
                    1,
                    Some("u2"),
                    &refl,
                    &[],
                    |_| None,
                )
                .is_some()
        );
        assert_eq!(engine.aborts_used, 2);
        // Streak 3 is declined — the hard cap is spent, even though every
        // other condition holds.
        engine.decide(
            SafeStateMode::Advisory,
            false,
            false,
            1,
            Some("u2"),
            &refl,
            &[],
            |_| None,
        );
        assert!(
            engine
                .decide(
                    SafeStateMode::Advisory,
                    false,
                    true,
                    1,
                    Some("u2"),
                    &refl,
                    &[],
                    |_| None,
                )
                .is_none(),
            "hard cap declines a third abort"
        );
    }

    #[test]
    fn once_per_streak() {
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &refl,
            &[],
            |_| None,
        );
        // First boundary of a deep streak fires.
        assert!(
            engine
                .decide(
                    SafeStateMode::Advisory,
                    false,
                    true,
                    1,
                    Some("u2"),
                    &refl,
                    &[],
                    |_| None,
                )
                .is_some()
        );
        // The very next boundary of the SAME streak (still due) does NOT — one
        // abort per streak.
        assert!(
            engine
                .decide(
                    SafeStateMode::Advisory,
                    false,
                    true,
                    1,
                    Some("u2"),
                    &refl,
                    &[],
                    |_| None,
                )
                .is_none(),
            "no second abort within one streak"
        );
        // A reset clears the once-per-streak guard...
        engine.decide(
            SafeStateMode::Advisory,
            false,
            false,
            1,
            Some("u2"),
            &refl,
            &[],
            |_| None,
        );
        // ...so a fresh 2× crossing fires again (within the hard cap).
        assert!(
            engine
                .decide(
                    SafeStateMode::Advisory,
                    false,
                    true,
                    1,
                    Some("u2"),
                    &refl,
                    &[],
                    |_| None,
                )
                .is_some()
        );
    }

    #[test]
    fn green_marker_stamps_only_on_fresh_green() {
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        assert_eq!(
            engine.last_green_turn.as_deref(),
            None,
            "nothing stamped before any green"
        );
        // A fresh-green boundary stamps the active turn.
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &refl,
            &[],
            |_| None,
        );
        assert_eq!(engine.last_green_turn.as_deref(), Some("u1"));
        // A non-green boundary does NOT overwrite the stamp.
        engine.decide(
            SafeStateMode::Advisory,
            false,
            true,
            1,
            Some("u2"),
            &refl,
            &[],
            |_| None,
        );
        assert_eq!(
            engine.last_green_turn.as_deref(),
            Some("u1"),
            "a stale/red boundary keeps the earlier green stamp"
        );
    }

    #[test]
    fn message_carries_reflections_excerpts_and_green_turn() {
        let mut engine = SafeStateEngine::new();
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &ReflectionLog::new(),
            &[],
            |_| None,
        );
        let mut refl = ReflectionLog::new();
        refl.record("edit(a.rs)");
        let msg = engine
            .decide(
                SafeStateMode::Advisory,
                false,
                true,
                1,
                Some("u2"),
                &refl,
                &[("bash".into(), "command failed".into())],
                |_| None,
            )
            .expect("fires");
        assert!(msg.contains("edit(a.rs)"), "carries an abandoned approach");
        assert!(
            msg.contains("bash: command failed"),
            "carries a failure excerpt"
        );
        // The internal snapshot id must NOT leak into model-facing prose, and
        // no restore command may be named — `/rewind` isn't a slash command
        // (Esc-Esc picker, keyed by message index), so naming one would send a
        // already-failing run chasing a command that doesn't exist.
        assert!(!msg.contains("u1"), "internal turn id must not leak: {msg}");
        assert!(!msg.contains("/rewind"), "names no bogus command: {msg}");
        assert!(
            msg.contains("undoing the edits"),
            "still offers the revert in actionable terms: {msg}"
        );
    }

    #[test]
    fn message_asks_for_one_approach_not_a_menu() {
        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &refl,
            &[],
            |_| None,
        );
        let msg = engine
            .decide(
                SafeStateMode::Advisory,
                false,
                true,
                1,
                Some("u2"),
                &refl,
                &[],
                |_| None,
            )
            .expect("fires");
        // MIR's single-recovery-action discipline: demand ONE approach.
        assert!(
            msg.contains("ONE new approach"),
            "the replan demands a single approach, not a menu"
        );
    }

    #[test]
    fn advisory_performs_no_file_write_even_when_a_clean_restore_exists() {
        // The whole reason we ship advisory-only (dirge-uw2l.6): the decision
        // must not touch the tree, even though a clean restore target exists.
        // `decide` returns only a String — it has no path to a file write — but
        // this pins that property against the real snapshot store.
        use crate::agent::tools::snapshots;
        use crate::sync_util::LockExt;

        let _g = snapshots::TEST_GATE.lock_ignore_poison();
        snapshots::clear();
        let dir = std::env::temp_dir().join(format!("dirge-safe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.txt");
        std::fs::write(&p, "v0").unwrap();
        snapshots::begin_turn("u1");
        snapshots::capture(&p);
        std::fs::write(&p, "v1").unwrap(); // green content
        snapshots::begin_turn("u2");
        snapshots::capture(&p);
        std::fs::write(&p, "v2").unwrap(); // post-green breakage

        let mut engine = SafeStateEngine::new();
        let refl = ReflectionLog::new();
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &refl,
            &[],
            |_| None,
        );
        let msg = engine.decide(
            SafeStateMode::Advisory,
            false,
            true,
            1,
            Some("u2"),
            &refl,
            &[("edit".into(), "no match".into())],
            |_| None,
        );
        assert!(msg.is_some(), "advisory fires");
        // advisory performed NO file write — the tree is still broken.
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "v2",
            "advisory performs no file write"
        );
        // ...and a clean restore target provably exists for the deferred-auto
        // path (this call mutates the tree, so it runs AFTER the assertion).
        let restored = snapshots::restore_after_green_turn("u1");
        assert_eq!(restored.len(), 1, "a clean restore target exists");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "v1",
            "and it lands on green"
        );

        snapshots::clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── auto mode (dirge-uw2l.6) ────────────────────────────────────────
    // The resolver closure stands in for the coverage-verified restore, so
    // these pin the DECISION contract without touching a filesystem. The
    // coverage proof itself is tested in `super::worktree_probe`.

    /// Advisory must never invoke the resolver — that closure is the only
    /// thing that writes files, so advisory calling it at all would break
    /// the "performs NO file writes" guarantee.
    #[test]
    fn advisory_never_invokes_the_restore_resolver() {
        let mut engine = SafeStateEngine::new();
        let mut called = false;
        engine.decide(
            SafeStateMode::Advisory,
            true,
            false,
            0,
            Some("u1"),
            &ReflectionLog::new(),
            &[],
            |_| {
                called = true;
                None
            },
        );
        let msg = engine
            .decide(
                SafeStateMode::Advisory,
                false,
                true,
                1,
                Some("u2"),
                &ReflectionLog::new(),
                &[],
                |_| {
                    called = true;
                    Some(3)
                },
            )
            .expect("fires");
        assert!(!called, "advisory must not reach the restore path");
        assert!(msg.contains("Consider undoing"), "advisory wording: {msg}");
    }

    /// A declining resolver (incomplete coverage, no repo, no fingerprint)
    /// degrades auto to exactly the advisory message — the safe fallback.
    #[test]
    fn auto_declines_to_advisory_when_coverage_is_incomplete() {
        let mut engine = SafeStateEngine::new();
        engine.decide(
            SafeStateMode::Auto,
            true,
            false,
            0,
            Some("u1"),
            &ReflectionLog::new(),
            &[],
            |_| None,
        );
        let msg = engine
            .decide(
                SafeStateMode::Auto,
                false,
                true,
                1,
                Some("u2"),
                &ReflectionLog::new(),
                &[],
                |_| None, // resolver declined
            )
            .expect("fires");
        assert!(
            msg.contains("Consider undoing"),
            "a declined restore reads exactly like advisory: {msg}"
        );
        assert!(!msg.contains("harness has reverted"), "{msg}");
    }

    /// A successful restore swaps the closing paragraph: telling the model
    /// to undo edits the harness already removed would send it chasing
    /// changes that no longer exist.
    #[test]
    fn auto_restore_replaces_the_undo_instruction() {
        let mut engine = SafeStateEngine::new();
        engine.decide(
            SafeStateMode::Auto,
            true,
            false,
            0,
            Some("u1"),
            &ReflectionLog::new(),
            &[],
            |_| None,
        );
        let msg = engine
            .decide(
                SafeStateMode::Auto,
                false,
                true,
                1,
                Some("u2"),
                &ReflectionLog::new(),
                &[],
                |green| {
                    assert_eq!(green, "u1", "restores from the GREEN turn marker");
                    Some(3)
                },
            )
            .expect("fires");
        assert!(msg.contains("reverted 3 files"), "{msg}");
        assert!(
            !msg.contains("Consider undoing"),
            "must not also tell the model to undo: {msg}"
        );
        assert!(msg.contains("ONE new approach"), "single-action rule holds");
    }

    /// The resolver is reached only after every gate passes — a boundary
    /// that wouldn't have fired must never touch the filesystem.
    #[test]
    fn resolver_is_not_invoked_when_gates_decline() {
        let mut engine = SafeStateEngine::new();
        let mut called = false;
        // Never green → no known-good state → declines before the resolver.
        engine.decide(
            SafeStateMode::Auto,
            false,
            true,
            1,
            Some("u1"),
            &ReflectionLog::new(),
            &[],
            |_| {
                called = true;
                Some(1)
            },
        );
        assert!(!called, "no green stamp must decline before restoring");

        // Green, due, but nothing unverified on the tree.
        let mut engine = SafeStateEngine::new();
        let mut called2 = false;
        engine.decide(
            SafeStateMode::Auto,
            true,
            false,
            0,
            Some("u1"),
            &ReflectionLog::new(),
            &[],
            |_| None,
        );
        engine.decide(
            SafeStateMode::Auto,
            false,
            true,
            0, // edits_since_verify == 0
            Some("u2"),
            &ReflectionLog::new(),
            &[],
            |_| {
                called2 = true;
                Some(1)
            },
        );
        assert!(!called2, "nothing to revert must decline before restoring");
    }

    /// Off short-circuits before anything, including the resolver.
    #[test]
    fn off_never_invokes_the_resolver() {
        let mut engine = SafeStateEngine::new();
        let mut called = false;
        let out = engine.decide(
            SafeStateMode::Off,
            true,
            true,
            9,
            Some("u1"),
            &ReflectionLog::new(),
            &[],
            |_| {
                called = true;
                Some(1)
            },
        );
        assert!(out.is_none());
        assert!(!called, "off is a no-op, including the destructive path");
    }
}

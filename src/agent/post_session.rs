//! Unified post-session learning orchestrator (dirge-ba0m).
//!
//! Before this module, `done.rs` fired three INDEPENDENT
//! fire-and-forget tasks after every idle Done:
//! - background review (writes skills + memory via tools)
//! - skills curator (mechanical lifecycle + LLM consolidation)
//! - memory curator (mechanical pass + LLM consolidation)
//!
//! They shared no state and ran concurrently, which produced a
//! cluster of races (audit finding F): a skill the review just
//! created getting archived by the curator before first use;
//! three LLM runners competing for provider rate limits at once;
//! lost-update TOCTOU on `.usage.json` / `MEMORY.md` when two
//! passes read-modify-write the same file concurrently.
//!
//! This module replaces those three spawns with ONE detached
//! task that runs the passes STRICTLY IN ORDER, awaiting each
//! before the next. Ordering is the entire coordination
//! primitive — no locks, no semaphores:
//! - the review fully completes (its `record_create` /
//!   `MEMORY.md` writes flushed) before the skills curator reads
//!   `.usage.json`, so a freshly-created skill has its
//!   `created_at` marker and is never mis-aged into Stale;
//! - at most one forked LLM runner is being drained at any
//!   instant, so the passes never compete for rate limits / cache.
//!
//! The stage list is three passes:
//!   1. background review — current-session capture
//!   2. skills curator    — consolidate skills
//!   3. memory curator    — consolidate memory
//!
//! (A cross-session extraction stage was removed in dirge-pfp2: it
//! re-derived durable facts that per-session background review already
//! captures, for the cost of an extra LLM pass.)
//!
//! Each stage is bounded by [`STAGE_TIMEOUT`] so a hung provider
//! call abandons that stage rather than stranding the rest of the
//! chain. The whole orchestrator is a single detached
//! `tokio::spawn` the user's turn never awaits — fire-and-forget
//! is preserved. Adding a future pass is one more entry in the
//! stage list — no new coordination machinery.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::extras::dirge_paths::ProjectPaths;
use crate::provider::AnyAgent;

/// Process-global "an orchestrator is currently running" flag.
/// dirge-ba0m review MEDIUM: `spawn_post_session` fires on every
/// idle Done. A rapid turn-after-turn cadence (the user finishes
/// turn A, then turn B, while A's orchestrator is still in its
/// seconds-to-minutes LLM passes) would otherwise spawn a SECOND
/// orchestrator concurrent with the first — re-introducing the
/// cross-pass races the single-task design exists to eliminate.
/// We skip-if-running: the in-flight orchestrator already covers
/// this session's learning, and the next eligible Done picks up
/// anything new.
static ORCHESTRATOR_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Try to claim the single orchestrator slot. Returns `true` if
/// claimed (caller MUST hold an [`InFlightGuard`] for the run),
/// `false` if an orchestrator is already in flight.
fn try_claim_orchestrator() -> bool {
    ORCHESTRATOR_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// RAII release of the orchestrator slot — panic-safe so a panic
/// in any stage still frees the slot for the next Done.
struct InFlightGuard;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        ORCHESTRATOR_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Per-stage wall-clock ceiling. A learning pass that exceeds
/// this is abandoned so a hung LLM provider can't strand the
/// rest of the chain. Generous — a real review / curation LLM
/// run completes well within five minutes; this only fires on a
/// genuinely stuck request.
///
/// On timeout the stage future is dropped: the LLM portion is
/// stopped by the `AbortRunnerOnDrop` guard inside each `run_*`
/// core (review.rs), and the per-stage `spawn_blocking`
/// mechanical pass (skills/memory curator) is bounded disk +
/// hash work that completes in milliseconds — it cannot hang, so
/// the fact that a dropped `spawn_blocking` handle can't cancel
/// the closure is harmless here.
const STAGE_TIMEOUT: Duration = Duration::from_secs(300);

/// A named post-session stage: a future that runs one learning
/// pass to completion. The future owns its own error handling
/// and reporting (returns `()`); the orchestrator only sequences
/// and time-bounds them.
type Stage = (&'static str, Pin<Box<dyn Future<Output = ()> + Send>>);

/// Single entry point for post-session learning. Fire-and-forget:
/// spawns ONE detached task and returns immediately. Inside that
/// task the passes run strictly in order.
///
/// Replaces the three independent `spawn_*` calls that used to
/// live in `done.rs`.
pub fn spawn_post_session(
    agent: AnyAgent,
    paths: ProjectPaths,
    digest: crate::agent::session_digest::SessionDigest,
    base: String,
    graduation_enabled: bool,
) {
    tokio::spawn(async move {
        // dirge-ba0m: at most one orchestrator in flight per
        // process. A second idle Done while this one is still
        // running its LLM passes is dropped — its learning folds
        // into the in-flight run or the next eligible Done.
        //
        // The claim happens HERE (first poll), not before the
        // spawn, so claim-and-guard are adjacent: if the runtime
        // tears down before this task is ever polled, nothing was
        // claimed and the slot can't leak.
        if !try_claim_orchestrator() {
            tracing::debug!(
                target: "dirge::post_session",
                "post-session orchestrator already running — skipping overlapping spawn",
            );
            return;
        }
        // Releases the in-flight slot on drop (incl. panic / early
        // return). Held for the whole sequence.
        let _in_flight = InFlightGuard;
        // dirge-6rtt: assemble the review transcript HERE, off the UI loop — the
        // digest's `git diff --stat` shells out, and the caller built the
        // (cheap, session-derived) digest on-thread. Doing it after the claim
        // also skips the subprocess entirely when an orchestrator is already
        // running.
        let transcript =
            crate::agent::session_digest::assemble_review_transcript(digest, &paths.root, base);
        let stages: Vec<Stage> = vec![
            (
                "background-review",
                Box::pin(stage_background_review(
                    agent.clone(),
                    paths.clone(),
                    transcript,
                )),
            ),
            (
                "skills-curator",
                Box::pin(stage_skills_curator(agent.clone(), paths.clone())),
            ),
            (
                "memory-curator",
                Box::pin(stage_memory_curator(
                    agent.clone(),
                    paths.clone(),
                    graduation_enabled,
                )),
            ),
        ];
        run_stages_sequentially(stages, STAGE_TIMEOUT).await;
        // Round 2: the review + curator passes may have written or
        // consolidated memories. Flag it so the running loop re-injects the
        // refreshed memory block at its next turn boundary — the agent's
        // baked-in system-prompt block wouldn't otherwise reflect these
        // writes until a restart.
        crate::agent::agent_loop::context_manager::mark_memories_dirty();
    });
}

/// Run stages strictly in order, each bounded by `per_stage_timeout`.
/// A stage that exceeds the timeout is abandoned and logged; the
/// NEXT stage still runs (a hung review must not block the
/// curators, which run on their own independent gates).
///
/// This is the coordination primitive. It guarantees:
/// - happens-before: stage N+1 is not polled until stage N has
///   returned (or timed out);
/// - single-runner: at most one stage future is in flight at any
///   instant;
/// - liveness: one stuck stage cannot strand the rest.
async fn run_stages_sequentially(stages: Vec<Stage>, per_stage_timeout: Duration) {
    for (name, fut) in stages {
        match tokio::time::timeout(per_stage_timeout, fut).await {
            Ok(()) => {}
            Err(_) => {
                tracing::warn!(
                    target: "dirge::post_session",
                    stage = %name,
                    timeout_secs = %per_stage_timeout.as_secs(),
                    "post-session stage timed out — skipping, continuing chain",
                );
            }
        }
    }
}

/// Stage 1 (capture): background review writes project learnings
/// to MEMORY.md / PITFALLS.md / skills. Self-gated by the 15-min
/// `claim_review_slot` throttle inside the core; a rate-limited
/// review returns early WITHOUT blocking the curator stages.
async fn stage_background_review(agent: AnyAgent, paths: ProjectPaths, transcript: String) {
    crate::agent::review::run_background_review(agent, paths, transcript, None).await;
}

/// Stage 2 (skills consolidation): run the skills curator's
/// mechanical lifecycle pass (off-thread, disk + hash), then the
/// LLM consolidation pass if there are agent-created candidates.
/// Gated by the curator's own 7-day `should_run_now`; most
/// sessions short-circuit here.
async fn stage_skills_curator(agent: AnyAgent, paths: ProjectPaths) {
    let paths_for_blocking = paths.clone();
    let candidate_list = tokio::task::spawn_blocking(move || {
        let mut curator = crate::extras::skills::curator::Curator::new(&paths_for_blocking).ok()?;
        if !curator.should_run_now() {
            return None;
        }
        let _ = curator.apply_automatic_transitions();
        // Render candidates AFTER mechanical transitions so
        // newly-stale skills are included.
        crate::extras::skill_db::SkillStore::load(&paths_for_blocking)
            .ok()
            .map(|store| crate::extras::skills::curator::render_candidate_list(&store))
    })
    .await
    .ok()
    .flatten();

    if let Some(candidates) = candidate_list {
        crate::agent::review::run_curator_review(agent, paths, candidates).await;
    }
}

/// Stage 3 (memory consolidation): run the memory curator's
/// mechanical pass (off-thread, reconcile + stale identification),
/// then the LLM consolidation pass if it surfaced stale
/// candidates. Gated by the curator's own 7-day `should_run_now`.
async fn stage_memory_curator(agent: AnyAgent, paths: ProjectPaths, graduation_enabled: bool) {
    let paths_for_blocking = paths.clone();
    let mechanical_report = tokio::task::spawn_blocking(move || {
        let mut curator =
            match crate::extras::memory_curator::MemoryCurator::new(&paths_for_blocking) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(
                        target: "dirge::memory_curator",
                        error = %e,
                        "Failed to construct memory curator — skipping run",
                    );
                    return None;
                }
            };
        if !curator.should_run_now() {
            return None;
        }
        curator.graduation_enabled = graduation_enabled;
        match curator.run_mechanical_pass() {
            Ok(report) => {
                tracing::info!(
                    target: "dirge::memory_curator",
                    total = %report.total_entries,
                    stale = %report.stale_candidates.len(),
                    "memory curator mechanical pass complete",
                );
                Some(report)
            }
            Err(e) => {
                tracing::warn!(
                    target: "dirge::memory_curator",
                    error = %e,
                    "memory curator mechanical pass failed",
                );
                None
            }
        }
    })
    .await
    .ok()
    .flatten();

    if let Some(report) = mechanical_report {
        crate::agent::review::run_memory_curator_review(agent, paths, report).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a stage future that records its name and asserts
    /// non-overlap via an in-flight counter. The `yield_now`
    /// gives any concurrently-polled stage a chance to overlap
    /// if the orchestrator were buggy — so a max-in-flight of 1
    /// is a real guarantee, not an accident of scheduling.
    fn recording_stage(
        name: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
        inflight: Arc<AtomicUsize>,
        max_inflight: Arc<AtomicUsize>,
    ) -> Stage {
        let fut = async move {
            let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
            max_inflight.fetch_max(now, Ordering::SeqCst);
            tokio::task::yield_now().await;
            log.lock().unwrap().push(name);
            inflight.fetch_sub(1, Ordering::SeqCst);
        };
        (name, Box::pin(fut))
    }

    /// Stages run in the exact order given, and never overlap.
    #[tokio::test]
    async fn run_stages_sequentially_runs_in_order_without_overlap() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_inflight = Arc::new(AtomicUsize::new(0));

        let stages = vec![
            recording_stage("a", log.clone(), inflight.clone(), max_inflight.clone()),
            recording_stage("b", log.clone(), inflight.clone(), max_inflight.clone()),
            recording_stage("c", log.clone(), inflight.clone(), max_inflight.clone()),
        ];
        run_stages_sequentially(stages, Duration::from_secs(5)).await;

        assert_eq!(*log.lock().unwrap(), vec!["a", "b", "c"], "strict order");
        assert_eq!(
            max_inflight.load(Ordering::SeqCst),
            1,
            "at most one stage in flight at any instant",
        );
    }

    /// A stage that exceeds the timeout is skipped, and the NEXT
    /// stage still runs. Uses paused time so the long sleep and
    /// the timeout auto-advance without real wall-clock waiting.
    #[tokio::test(start_paused = true)]
    async fn run_stages_sequentially_skips_timed_out_stage_and_continues() {
        let log = Arc::new(Mutex::new(Vec::new()));

        let log_slow = log.clone();
        let slow: Stage = (
            "slow",
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(600)).await;
                log_slow.lock().unwrap().push("slow");
            }),
        );
        let log_after = log.clone();
        let after: Stage = (
            "after",
            Box::pin(async move {
                log_after.lock().unwrap().push("after");
            }),
        );

        run_stages_sequentially(vec![slow, after], Duration::from_secs(300)).await;

        assert_eq!(
            *log.lock().unwrap(),
            vec!["after"],
            "slow stage must be skipped (never pushes), after stage still runs",
        );
    }

    /// Empty stage list is a clean no-op.
    #[tokio::test]
    async fn run_stages_sequentially_handles_empty_list() {
        run_stages_sequentially(vec![], Duration::from_secs(5)).await;
        // No panic = pass.
    }

    /// dirge-ba0m: the orchestrator slot is exclusive — a second
    /// claim while one is in flight is refused, and the slot is
    /// released when the guard drops. This is the overlap guard
    /// that stops a rapid second idle Done from spawning a
    /// concurrent orchestrator.
    ///
    /// NOTE: touches the process-global `ORCHESTRATOR_IN_FLIGHT`
    /// static; it is the only test that does, so parallel test
    /// execution is safe. Resets the flag at the end regardless.
    #[test]
    fn orchestrator_claim_is_exclusive_and_released_on_drop() {
        assert!(try_claim_orchestrator(), "first claim must succeed");
        {
            let _in_flight = InFlightGuard;
            assert!(
                !try_claim_orchestrator(),
                "a concurrent claim must be refused while one is in flight",
            );
        } // guard drops here → slot released
        assert!(
            try_claim_orchestrator(),
            "claim must succeed again after the guard released the slot",
        );
        // Clean up so we don't poison any later run in this process.
        ORCHESTRATOR_IN_FLIGHT.store(false, Ordering::Release);
    }

    /// A stage that finishes well within the timeout runs to
    /// completion (sanity: the timeout doesn't truncate fast
    /// stages). Paused time confirms the stage completes at t=1s
    /// under a 300s ceiling.
    #[tokio::test(start_paused = true)]
    async fn run_stages_sequentially_lets_fast_stages_complete() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let log_fast = log.clone();
        let fast: Stage = (
            "fast",
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                log_fast.lock().unwrap().push("fast");
            }),
        );
        run_stages_sequentially(vec![fast], Duration::from_secs(300)).await;
        assert_eq!(*log.lock().unwrap(), vec!["fast"]);
    }
}

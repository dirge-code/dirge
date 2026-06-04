//! `/plan <request>` — the phased plan workflow entry (vix port, P3e-b).
//!
//! The two read-only forks (explore → plan) run on a SPAWNED task, not inline,
//! so the UI event loop stays responsive (paints, scrolls, Ctrl+C) while the
//! minutes-long LLM calls run [dirge-vuzz]. The task streams
//! [`PlanPhaseEvent`]s back over a channel; the UI loop renders progress lines,
//! launches the streamed implement run on `Ready`, and seeds the reviewer loop
//! (driven in `run_handlers/done.rs`). The phases are separate forks → genuine
//! context resets, per the chosen "separate-agent phases" model.

use tokio::sync::mpsc;

use super::SlashCtx;
use crate::agent::plan::runtime::{
    ActivePlan, PlanKickoff, PlanPhaseEvent, PlanPhaseHandle, collect_runner_text,
};
use crate::agent::plan::workflow::{READONLY_PHASE_TOOLS, explore_prompt, plan_prompt};
use crate::provider::AnyAgent;
use crate::ui::avatar::AvatarState;
use crate::ui::colors::c_error;

/// Switch the bottom bar into the busy presentation and repaint.
///
/// The standard busy indicator is keyed off the single `is_running` flag that
/// the prompt glyph (`░▌` vs `> `), the status word (`running`/`ready`), and
/// the avatar read. We flip it on here (and blank the just-submitted `/plan …`
/// line) the instant the phases are spawned; the UI loop flips it back off when
/// the phase task reports `Aborted`, or keeps it on through the implement run on
/// `Ready`.
fn set_busy(ctx: &mut SlashCtx<'_>, busy: bool) -> anyhow::Result<()> {
    ctx.renderer.set_avatar_state(if busy {
        AvatarState::Thinking
    } else {
        AvatarState::Idle
    });
    let status = crate::ui::status::StatusLine::render(
        ctx.session,
        busy,
        0,
        None,
        ctx.context.current_prompt_name.as_deref(),
        None,
        ctx.bg_store.as_ref(),
        None,
    );
    ctx.renderer.draw_bottom(ctx.input, &status, busy)?;
    ctx.renderer.render_viewport()?;
    *ctx.is_running = busy;
    Ok(())
}

pub(super) async fn cmd_plan(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    _text: &str,
) -> anyhow::Result<()> {
    if !ctx.cfg.resolve_phased_workflow_enabled() {
        ctx.renderer.write_line(
            "/plan is off — set phased_workflow_enabled = true in your config to enable the phased workflow",
            c_error(),
        )?;
        return Ok(());
    }

    let request = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
    if request.trim().is_empty() {
        ctx.renderer
            .write_line("usage: /plan <request>", c_error())?;
        return Ok(());
    }

    // Snapshot everything the forks need OFF the UI thread: a frozen transcript,
    // the review-cycle budget, and a cheap clone of the agent (Arc bumps). The
    // session/renderer/config stay on the UI thread.
    let transcript = crate::agent::review::build_transcript(ctx.session);
    let cycles = ctx.cfg.resolve_phased_workflow_max_review_cycles();
    let agent = ctx.agent.clone();

    // Channel capacity is small — the task emits a handful of progress lines
    // plus one terminal event; the UI loop drains it promptly.
    let (tx, rx) = mpsc::channel::<PlanPhaseEvent>(8);
    let task = tokio::spawn(run_phases_task(agent, request, transcript, cycles, tx));

    // Show busy immediately (the typed line is already cleared), then hand the
    // handle to the loop, which drives the rest from its `select!`. Defensive:
    // abort any prior phase task we're replacing so it can't keep running
    // orphaned (shouldn't happen — /plan is gated while a run is active).
    set_busy(ctx, true)?;
    if let Some(old) = ctx.plan_phase.take() {
        old.task.abort();
    }
    *ctx.plan_phase = Some(PlanPhaseHandle { rx, task });
    Ok(())
}

/// Send a progress line; returns `false` (caller should bail) if the UI loop
/// dropped the receiver (e.g. Ctrl+C aborted the phases).
async fn progress(tx: &mpsc::Sender<PlanPhaseEvent>, text: impl Into<String>, error: bool) -> bool {
    tx.send(PlanPhaseEvent::Progress {
        text: text.into(),
        error,
    })
    .await
    .is_ok()
}

/// The explore → plan forks, run on a spawned task. Streams progress lines and a
/// terminal `Ready`/`Aborted` back over `tx`. Aborting the task (Ctrl+C) drops
/// the in-flight `collect_runner_text` future, whose `AbortRunnerOnDrop` guard
/// cancels the inner phase runner too — so no orphaned LLM call survives.
async fn run_phases_task(
    agent: AnyAgent,
    request: String,
    transcript: String,
    cycles: usize,
    tx: mpsc::Sender<PlanPhaseEvent>,
) {
    // Phase 1: Explore (read-only fork, fresh context).
    if !progress(
        &tx,
        "Phase: Explore — mapping the codebase (read-only)…",
        false,
    )
    .await
    {
        return;
    }
    let explore_runner = agent.spawn_phase_runner(
        explore_prompt(&request),
        transcript.clone(),
        READONLY_PHASE_TOOLS,
    );
    let findings = match collect_runner_text(explore_runner).await {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            progress(&tx, "Phase: Explore — produced no findings; aborting", true).await;
            let _ = tx.send(PlanPhaseEvent::Aborted).await;
            return;
        }
        Err(e) => {
            progress(&tx, format!("Phase: Explore — error: {e}; aborting"), true).await;
            let _ = tx.send(PlanPhaseEvent::Aborted).await;
            return;
        }
    };

    // Phase 2: Plan (read-only fork; the ONLY thing carried over is the findings
    // report — a true context reset between phases).
    if !progress(
        &tx,
        "Phase: Plan — turning findings into an implementation plan…",
        false,
    )
    .await
    {
        return;
    }
    let plan_runner = agent.spawn_phase_runner(
        plan_prompt(&request, &findings),
        transcript,
        READONLY_PHASE_TOOLS,
    );
    let plan = match collect_runner_text(plan_runner).await {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            progress(&tx, "Phase: Plan — produced no plan; aborting", true).await;
            let _ = tx.send(PlanPhaseEvent::Aborted).await;
            return;
        }
        Err(e) => {
            progress(&tx, format!("Phase: Plan — error: {e}; aborting"), true).await;
            let _ = tx.send(PlanPhaseEvent::Aborted).await;
            return;
        }
    };

    // Hand off to the UI loop: it launches the streamed implement run and arms
    // the reviewer loop.
    let impl_prompt = format!(
        "{request}\n\n--- Implementation plan (from the planning phase) ---\n{plan}\n\n\
         Implement this plan now. Make the edits and run the build/tests to verify.",
    );
    progress(
        &tx,
        "Phase: Implement — executing the plan (you'll watch it run)…",
        false,
    )
    .await;
    let _ = tx
        .send(PlanPhaseEvent::Ready(Box::new(PlanKickoff {
            impl_prompt,
            active: ActivePlan {
                plan,
                cycles_left: cycles,
            },
        })))
        .await;
}

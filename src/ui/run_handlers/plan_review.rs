//! Phased `/plan` reviewer loop (P3e-b) — UI orchestration.
//!
//! After a plan-driven implement turn finishes, [`drive_plan_review`] forks a
//! *write-disabled* reviewer that independently runs the code — but OFF the UI
//! thread (dirge-4koy). The reviewer takes tens of seconds to minutes; awaiting
//! it inline froze the event loop. Instead it spawns the reviewer via
//! [`spawn_review`] and parks the handle in `review_phase`, keeping the loop
//! responsive (and Ctrl+C-able). When the verdict lands, the `review_phase`
//! `select!` arm calls [`apply_review_verdict`]: `DONE` / budget-spent / error
//! finalize the turn (the finalization `handle_done` deferred while we were
//! busy reviewing); `NEEDS_FIX` feeds the punch-list into another streamed
//! implement turn, bounded by the cycle budget. The policy decision lives in
//! [`crate::agent::plan::workflow::next_review_step`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::plan::runtime::{ActivePlan, ReviewPhaseHandle, spawn_review};
use crate::agent::plan::workflow::{
    REVIEWER_TOOLS, ReviewStep, implement_retry_prompt, reviewer_prompt,
};
use crate::agent::tools::background::{BackgroundStore, prepend_pending_notifications};
use crate::event::AgentEvent;
use crate::provider::AnyAgent;
use crate::session::{MessageRole, Session, ToolCallEntry};
use crate::ui::colors::{c_agent, c_error};
use crate::ui::renderer::Renderer;
use crate::ui::run_handlers::RunCtx;

/// Fork one reviewer pass for an in-flight `/plan` workflow, OFF the UI thread.
/// No-op unless this `Done` left the run idle (`!is_running`) and a plan is
/// active. Builds the (write-disabled) reviewer runner on-thread — it needs
/// `&agent` — then hands it to [`spawn_review`] and parks the handle in
/// `review_phase`, carrying the just-finished turn's `response` / `tool_calls`
/// so the terminal-verdict branch can finalize it later. Keeps `is_running`
/// true so `handle_done`'s idle finalization doesn't race the reviewer; the
/// `review_phase` arm finalizes (or relaunches) when the verdict lands.
pub(super) fn drive_plan_review(
    ctx: &mut RunCtx<'_>,
    agent: &AnyAgent,
    response: &str,
    tool_calls: &[ToolCallEntry],
    review_phase: &mut Option<ReviewPhaseHandle>,
    is_running: &mut bool,
) -> anyhow::Result<()> {
    // Only when this `Done` left the run idle and a plan is mid-flight.
    if *is_running {
        return Ok(());
    }
    let Some(active) = ctx.active_plan.take() else {
        return Ok(());
    };

    // Transcript reflects the just-committed implement turn (the assistant
    // response was added to the session earlier in `handle_done`).
    let transcript = crate::agent::review::build_transcript(ctx.session);
    ctx.renderer
        .write_line("Phase: Review — reviewer runs the code…", c_agent())?;

    let runner =
        agent.spawn_phase_runner(reviewer_prompt(&active.plan), transcript, REVIEWER_TOOLS);
    *review_phase = Some(spawn_review(
        runner,
        active.plan,
        active.cycles_left,
        response.to_string(),
        tool_calls.to_vec(),
    ));
    // Stay busy while the reviewer runs; the review_phase arm releases /
    // relaunches when the verdict lands.
    *is_running = true;
    Ok(())
}

/// Apply a reviewer verdict that just landed in the `review_phase` arm. On
/// `Retry` (within budget) relaunch the implement run with the punch-list and
/// re-arm `active_plan` with one fewer cycle so the next `Done` reviews again.
/// On a terminal verdict (approved / budget spent / reviewer error) release the
/// busy state and run the idle finalization `handle_done` deferred while the
/// reviewer held the loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_review_verdict(
    result: Result<ReviewStep, String>,
    plan: String,
    cycles_left: usize,
    response: &str,
    tool_calls: &[ToolCallEntry],
    renderer: &mut Renderer,
    session: &mut Session,
    active_plan: &mut Option<ActivePlan>,
    last_user_prompt: &mut String,
    agent: &AnyAgent,
    bg_store: &Option<BackgroundStore>,
    interjection_queue: &Arc<Mutex<VecDeque<String>>>,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    is_running: &mut bool,
) -> anyhow::Result<()> {
    if let Ok(ReviewStep::Retry { feedback }) = &result {
        renderer.write_line(
            "Phase: Review — changes needed; re-implementing…",
            c_agent(),
        )?;
        let retry_prompt = implement_retry_prompt(feedback);
        last_user_prompt.clone_from(&retry_prompt);
        session.add_message(MessageRole::User, &retry_prompt);
        let runner = agent.clone().spawn_runner(
            prepend_pending_notifications(&retry_prompt, bg_store.as_ref()),
            crate::agent::runner::convert_history(session),
            Some(interjection_queue.clone()),
        );
        runner.install_into(
            agent_rx,
            agent_abort,
            agent_interject,
            agent_cancel,
            is_running,
        );
        // One cycle consumed; the next `Done` reviews again.
        *active_plan = Some(ActivePlan {
            plan,
            cycles_left: cycles_left - 1,
        });
        return Ok(());
    }

    // Terminal verdict — render the outcome, then finalize.
    match result {
        Ok(ReviewStep::Approved) => {
            renderer.write_line("Phase: Review — ✓ reviewer approved", c_agent())?;
        }
        Ok(ReviewStep::Exhausted) => {
            renderer.write_line(
                "Phase: Review — fix-cycle budget spent; stopping. Continue manually if needed.",
                c_agent(),
            )?;
        }
        Err(e) => {
            renderer.write_line(
                &format!("Phase: Review — reviewer error: {e}; stopping"),
                c_error(),
            )?;
        }
        Ok(ReviewStep::Retry { .. }) => unreachable!("Retry handled above"),
    }

    // The reviewer concluded — release the busy state the reviewer held and run
    // the finalization (persist + post-session learning + interjection drain)
    // that `handle_done` deferred to here (dirge-4koy).
    *is_running = false;
    super::done::finalize_idle_turn(
        session,
        last_user_prompt,
        response,
        tool_calls,
        agent,
        bg_store,
        interjection_queue,
        agent_rx,
        agent_abort,
        agent_interject,
        agent_cancel,
        is_running,
        true,
    )
}

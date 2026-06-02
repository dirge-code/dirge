//! Phased `/plan` reviewer loop (P3e-b), extracted from `handle_done`.
//!
//! After a plan-driven implement turn finishes, this forks a *write-disabled*
//! reviewer that independently runs the code and emits a verdict
//! ([`crate::agent::phased_orchestrator::review_once`]). `DONE` ends the
//! workflow; `NEEDS_FIX` feeds the punch-list back into another streamed
//! implement turn, bounded by the cycle budget. The policy decision lives in
//! [`crate::agent::plan_workflow::next_review_step`]; this module is the UI-side
//! orchestration (render phase lines, relaunch the implement run) that
//! `handle_done` calls once it knows no plugin follow-up / loop iteration
//! claimed the next turn.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::phased_orchestrator::{ActivePlan, review_once};
use crate::agent::plan_workflow::{ReviewStep, implement_retry_prompt};
use crate::agent::tools::background::{BackgroundStore, prepend_pending_notifications};
use crate::event::AgentEvent;
use crate::provider::AnyAgent;
use crate::session::MessageRole;
use crate::ui::colors::{c_agent, c_error};
use crate::ui::run_handlers::RunCtx;

/// Drive one reviewer pass for an in-flight `/plan` workflow. No-op unless the
/// run just went idle (`!is_running`) and a plan is active. On `NEEDS_FIX`
/// (within budget) it relaunches the implement run — taking over the run-state
/// slots and re-arming `ctx.active_plan` with one fewer cycle so the next
/// `Done` reviews again.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_plan_review(
    ctx: &mut RunCtx<'_>,
    agent: &mut AnyAgent,
    bg_store: &Option<BackgroundStore>,
    interjection_queue: &Arc<Mutex<VecDeque<String>>>,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
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

    match review_once(agent, &active.plan, transcript, active.cycles_left).await {
        Ok(ReviewStep::Approved) => {
            ctx.renderer
                .write_line("Phase: Review — ✓ reviewer approved", c_agent())?;
        }
        Ok(ReviewStep::Exhausted) => {
            ctx.renderer.write_line(
                "Phase: Review — fix-cycle budget spent; stopping. Continue manually if needed.",
                c_agent(),
            )?;
        }
        Ok(ReviewStep::Retry { feedback }) => {
            ctx.renderer.write_line(
                "Phase: Review — changes needed; re-implementing…",
                c_agent(),
            )?;
            let retry_prompt = implement_retry_prompt(&feedback);
            ctx.last_user_prompt.clone_from(&retry_prompt);
            ctx.session.add_message(MessageRole::User, &retry_prompt);
            let runner = agent.clone().spawn_runner(
                prepend_pending_notifications(&retry_prompt, bg_store.as_ref()),
                crate::agent::runner::convert_history(ctx.session),
                Some(interjection_queue.clone()),
            );
            *agent_rx = Some(runner.event_rx);
            *agent_abort = Some(runner.task);
            *agent_interject = Some(runner.interject_tx);
            *agent_cancel = Some(runner.cancel_tx);
            *is_running = true;
            // One cycle consumed; the next `Done` reviews again.
            *ctx.active_plan = Some(ActivePlan {
                plan: active.plan,
                cycles_left: active.cycles_left - 1,
            });
        }
        Err(e) => {
            ctx.renderer.write_line(
                &format!("Phase: Review — reviewer error: {e}; stopping"),
                c_error(),
            )?;
        }
    }
    Ok(())
}

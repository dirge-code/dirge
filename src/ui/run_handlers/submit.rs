//! `submit_resolved_prompt`: the submit continuation shared by the inline
//! submit path and the off-loop `on-prompt` phase completion arm (dirge-qhfk).
//!
//! Given the resolved `on-prompt` outputs (`hint`/`replace`), it builds the
//! final prompt, runs the preemptive-compaction check (deferring the turn to
//! the compaction phase when it fires), and otherwise spawns the agent runner.
//! Extracted verbatim from the `run_interactive` submit arm so both the inline
//! path (Stage 2a — pure refactor) and the on-prompt completion arm (Stage 2b)
//! drive identical behavior.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::AgentEvent;
use crate::provider::AnyAgent;
use crate::session::MessageRole;
use crate::ui::avatar;
use crate::ui::colors::c_error;
use crate::ui::compaction::{CompactionPhaseHandle, CompactionThen};
use crate::ui::events::sanitize_output;
use crate::ui::run_handlers::{AgentBuildDeps, RunCtx};
use crate::ui::theme;

/// Build the final prompt from the resolved `on-prompt` outputs and start the
/// turn (or defer to preemptive compaction). `hint`/`replace` are `None` when
/// no plugin ran. Mirrors the original inline submit tail exactly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_resolved_prompt(
    ctx: &mut RunCtx<'_>,
    deps: &AgentBuildDeps<'_>,
    agent: &mut AnyAgent,
    hint: Option<String>,
    replace: Option<String>,
    text: &str,
    images: Vec<crate::agent::agent_loop::message::ImageRef>,
    is_running: &mut bool,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &Arc<Mutex<VecDeque<String>>>,
    compaction_phase: &mut Option<CompactionPhaseHandle>,
) -> anyhow::Result<()> {
    let prompt = if let Some(replacement) = replace {
        // Echo the rewrite so the user can see what the LLM is actually
        // receiving — otherwise it looks like their message vanished.
        ctx.renderer
            .write_line("[plugin] prompt rewritten:", theme::dim())?;
        for line in replacement.lines() {
            ctx.renderer
                .write_line(&format!("  {}", sanitize_output(line)), theme::dim())?;
        }
        replacement
    } else if let Some(hint) = hint {
        format!("{}\n\n{}", hint, text)
    } else {
        text.to_string()
    };

    // Phase 8: track the user prompt for session DB persistence.
    *ctx.last_user_prompt = text.to_string();

    // Preemptive compaction check. Estimate the new prompt's token cost; if
    // projected_total > 85% of budget, compact BEFORE sending. Reactive
    // recovery at the ContextOverflow arm is the backstop if the estimate
    // undershoots.
    let reserve_for_check = ctx.cfg.resolve_reserve_tokens();
    let max_tokens_for_check = ctx.session.context_window.saturating_sub(reserve_for_check);
    let est_new_tokens = crate::session::Session::estimate_tokens(&prompt);
    let preemptive_fired = ctx.cfg.resolve_compact_enabled()
        && crate::ui::slash::preemptive_compaction_due(
            ctx.session.total_estimated_tokens,
            est_new_tokens,
            max_tokens_for_check,
        );

    // When preemptive compaction fires, run the summarizer OFF-thread and
    // defer this turn to the `compaction_phase` arm (which installs the
    // summary then resends the prompt). `deferred` skips the runner-spawn.
    let mut deferred_to_compaction = false;
    let history = if preemptive_fired {
        ctx.renderer.write_line(
            "▒░ preemptive compaction (context near limit) ░▒",
            theme::accent(),
        )?;
        // forced=true: the preemptive trigger already decided (at 85%,
        // factoring the incoming prompt), so bypass prepare's stricter
        // within-limits gate — otherwise it no-ops in the 85–100% band.
        match crate::ui::slash::prepare_compaction(
            None,
            true,
            agent,
            deps.client,
            ctx.renderer,
            ctx.session,
            ctx.cfg,
        ) {
            Ok(crate::ui::slash::CompactionDecision::Ready(req)) => {
                *compaction_phase = Some(crate::ui::compaction::spawn(
                    *req,
                    CompactionThen::SendPrompt {
                        run_prompt: prompt.clone(),
                        record_text: text.to_string(),
                        images: images.clone(),
                    },
                ));
                *is_running = true;
                ctx.renderer.set_avatar_state(avatar::AvatarState::Thinking);
                deferred_to_compaction = true;
                crate::agent::runner::convert_history(ctx.session)
            }
            Ok(crate::ui::slash::CompactionDecision::NoOp) => {
                crate::agent::runner::convert_history(ctx.session)
            }
            Err(e) => {
                ctx.renderer.write_line(
                    &format!("preemptive compaction failed (will retry reactively if needed): {e}"),
                    c_error(),
                )?;
                crate::agent::runner::convert_history(ctx.session)
            }
        }
    } else {
        crate::agent::runner::convert_history(ctx.session)
    };

    if !deferred_to_compaction {
        let runner = agent.clone().spawn_runner(
            crate::provider::Prompt {
                text: crate::agent::tools::background::prepend_pending_notifications(
                    &prompt,
                    deps.bg_store.as_ref(),
                ),
                images: images.clone(),
            },
            history,
            Some(interjection_queue.clone()),
            Some(ctx.session.assets_dir()),
        );
        runner.install_into(
            agent_rx,
            agent_abort,
            agent_interject,
            agent_cancel,
            is_running,
        );

        ctx.session
            .add_message_with_images(MessageRole::User, text, images);
        crate::ui::begin_snapshot_turn(ctx.session);
        ctx.renderer.set_avatar_state(avatar::AvatarState::Idle);
    }
    Ok(())
}

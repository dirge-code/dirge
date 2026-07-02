//! `AgentEvent::Error` handler extracted from `run_interactive`.
//!
//! Flushes any coalesced trailing tokens, writes the error line, persists
//! the partial turn (so it's searchable), fires the plugin `on-error`
//! hook, tears the runner down, and drops queued interjections — replaying
//! them after an error (e.g. context-length) would just re-trigger it.
//! Behavior is identical to the inline code; pure refactor.

use std::time::Instant;

use compact_str::CompactString;
use tokio::sync::mpsc;

use crate::event::AgentEvent;
use crate::ui::agent_io::{persist_turn_to_db, render_agent_stream};
use crate::ui::avatar;
use crate::ui::colors::{c_agent, c_error};
use crate::ui::events::sanitize_output;
use crate::ui::run_handlers::RunCtx;
use crate::ui::tool_display::close_tool_chamber_if_open;

#[cfg(feature = "plugin")]
use crate::plugin::PluginManager;
#[cfg(feature = "plugin")]
use std::sync::{Arc, Mutex};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_error(
    ctx: &mut RunCtx<'_>,
    error: CompactString,
    was_reasoning: &mut bool,
    is_running: &mut bool,
    last_token_render: &mut Option<Instant>,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<tokio::task::JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    #[cfg(feature = "plugin")] plugin_manager: Option<&Arc<Mutex<PluginManager>>>,
) -> anyhow::Result<()> {
    *was_reasoning = false;
    ctx.renderer.set_avatar_state(avatar::AvatarState::Error);
    #[cfg(feature = "experimental-ui-terminal-tab")]
    ctx.renderer.set_last_tool_name("");
    close_tool_chamber_if_open(ctx.renderer, ctx.last_tool_name, ctx.tool_chamber_open)?;
    // dirge-ufe0: flush any trailing token the render coalescer skipped
    // (the Error event queued behind the final tokens leaves them
    // caught-up-but-unpainted) before the error line is written, so the
    // streamed text stays on-screen above the error (also DB-persisted).
    if !ctx.response_buf.is_empty() {
        render_agent_stream(
            ctx.response_buf,
            ctx.response_start_line,
            c_agent(),
            ctx.renderer,
        )?;
        *last_token_render = None;
    }
    let safe = sanitize_output(&error);
    ctx.renderer
        .write_line(&format!("error: {}", safe), c_error())?;

    // Persist the partial turn (whatever streamed before the error) so it's
    // searchable and the session records what went wrong.
    persist_turn_to_db(
        ctx.session,
        ctx.last_user_prompt,
        ctx.response_buf,
        ctx.tool_calls_buf,
    );

    #[cfg(feature = "plugin")]
    if let Some(pm) = plugin_manager {
        // dirge-qhfk: dispatch OFF the loop thread. Firing `on-error` inline
        // blocked the single runtime thread inside the Janet worker, so a hook
        // opening a dialog deadlocked. Results are ignored; a rare dispatch
        // error goes to the log (the detached task has no `renderer`).
        let ctx_err = format!(
            "@{{:error \"{}\"}}",
            crate::plugin::escape_janet_string(&error)
        );
        crate::ui::phase::spawn_detached_plugin(pm.clone(), "on-error", move |mgr| {
            if let Err(dispatch_err) = mgr.dispatch("on-error", &ctx_err) {
                tracing::warn!(
                    target: "dirge::plugin",
                    error = %dispatch_err,
                    "on-error hook dispatch failed",
                );
            }
        });
    }

    *is_running = false;
    if let Some(tx) = agent_cancel.take() {
        let _ = tx.try_send(());
    }
    if let Some(h) = agent_abort.take() {
        h.abort();
    }
    *agent_rx = None;
    *agent_interject = None;
    *ctx.agent_line_started = false;
    ctx.response_buf.clear();
    *ctx.response_start_line = None;
    ctx.reasoning_buf.clear();
    *ctx.reasoning_start_line = None;

    // Drop queued interjections — they were typed expecting the running
    // turn to succeed; replaying them blindly after an error would just
    // re-trigger it.
    let dropped = interjection_queue.lock().unwrap().len();
    interjection_queue.lock().unwrap().clear();
    if dropped > 0 {
        ctx.renderer.write_line(
            &format!(
                "{} queued message{} dropped due to error",
                dropped,
                if dropped == 1 { "" } else { "s" }
            ),
            c_error(),
        )?;
    }
    Ok(())
}

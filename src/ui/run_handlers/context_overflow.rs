//! `AgentEvent::ContextOverflow` handler extracted from `run_interactive`.
//!
//! Auto-recovery for a context-length error mid-run: persist what streamed so
//! far, then kick off a NON-BLOCKING compaction (dirge-tv3p) — the summarizer
//! LLM runs on a spawned task so the UI stays responsive. The retry (respawn the
//! same prompt against the compacted history) is the `RetryAfterOverflow`
//! continuation the `compaction_phase` select! arm runs after install; it only
//! fires when compaction actually shrank the context AND no side-effecting tools
//! ran on the failed turn.

use compact_str::CompactString;
use tokio::sync::mpsc;

use crate::context::ContextFiles;
use crate::event::AgentEvent;
use crate::provider::AnyAgent;
use crate::ui::agent_io::{persist_turn_to_db, render_agent_stream};
use crate::ui::colors::{c_agent, c_error};
use crate::ui::events::sanitize_output;
use crate::ui::run_handlers::{AgentBuildDeps, RunCtx};
use crate::ui::theme;
use crate::ui::tool_display::close_tool_chamber_if_open;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_context_overflow(
    ctx: &mut RunCtx<'_>,
    prompt: CompactString,
    error: CompactString,
    was_reasoning: &mut bool,
    is_running: &mut bool,
    agent: &mut AnyAgent,
    // Kept for signature stability with the call site; install (which needs the
    // context files) now runs in the `compaction_phase` arm with the loop's own.
    _context: &mut ContextFiles,
    // dirge-4y4l: the ~10 build_agent inputs bundled (see AgentBuildDeps).
    deps: &AgentBuildDeps<'_>,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<tokio::task::JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    // dirge-tv3p: the loop installs the spawned compaction here; its
    // `compaction_phase` arm runs the retry after the summary lands.
    compaction_phase: &mut Option<crate::ui::compaction::CompactionPhaseHandle>,
) -> anyhow::Result<()> {
    let client = deps.client;
    // Audit H17: the streaming run hit a context-length error. Auto-compact
    // then re-spawn with the same prompt against the now-compacted history —
    // opencode-style automatic recovery (compaction.ts:477-558) instead of
    // leaving the user stranded at the error. dirge-tv3p: the summarizer now
    // runs off-thread (was a 10-60s freeze); the retry is the arm's continuation.
    *was_reasoning = false;
    close_tool_chamber_if_open(ctx.renderer, ctx.last_tool_name, ctx.tool_chamber_open)?;
    // dirge-ufe0: flush any trailing token the render coalescer skipped (the
    // ContextOverflow event queued behind the final tokens leaves them
    // caught-up-but-unpainted) before the overflow line is written.
    if !ctx.response_buf.is_empty() {
        render_agent_stream(
            ctx.response_buf,
            ctx.response_start_line,
            c_agent(),
            ctx.renderer,
        )?;
    }
    let safe = sanitize_output(&error);
    ctx.renderer
        .write_line(&format!("context overflow: {}", safe), c_error())?;
    // Persist what we have so far (partial response + tool calls) before
    // tearing down the runner.
    persist_turn_to_db(
        ctx.session,
        ctx.last_user_prompt,
        ctx.response_buf,
        ctx.tool_calls_buf,
    );

    // dirge-b899: record the partial assistant turn (streamed text + the
    // tool calls that DID complete on the failed turn) into session.messages
    // BEFORE compacting, so the compacted history carries the tool RESULTS.
    // That lets recovery resume as a continuation instead of re-sending the
    // prompt — re-sending would re-run the side-effecting tools from scratch.
    // A pure first-token overflow (nothing streamed) records nothing and
    // falls back to a plain prompt re-send.
    let made_progress = !ctx.response_buf.is_empty() || !ctx.tool_calls_buf.is_empty();
    if made_progress {
        // `take` (not clone) — response_buf is cleared in the teardown below
        // regardless, so move the streamed text out instead of heap-copying a
        // potentially-large response on this now-common overflow path.
        let partial = std::mem::take(ctx.response_buf);
        ctx.session.add_message_with_tool_calls(
            crate::session::MessageRole::Assistant,
            &partial,
            std::mem::take(ctx.tool_calls_buf),
        );
    }

    // Tear down the current runner before compaction.
    if let Some(h) = agent_abort.take() {
        h.abort();
    }
    *agent_rx = None;
    *agent_interject = None;
    *agent_cancel = None;
    *ctx.agent_line_started = false;
    ctx.response_buf.clear();
    *ctx.response_start_line = None;
    ctx.reasoning_buf.clear();
    *ctx.reasoning_start_line = None;

    // The failed run is over — reset the per-run tool-call counter.
    *ctx.tool_calls_this_run = 0;

    ctx.renderer
        .write_line("▒░ auto-compacting then retrying ░▒", theme::accent())?;

    // dirge-tv3p: decide on-thread, then spawn the summarizer off-thread. The
    // `compaction_phase` arm installs the summary and, on `Compacted`, resumes
    // the task — as a continuation when the failed turn made progress (the
    // partial turn was recorded above), else by re-sending the prompt.
    match crate::ui::slash::prepare_compaction(
        None,
        false, // forced: auto-compaction stays threshold-gated [dirge-fgtj]
        agent,
        client,
        ctx.renderer,
        ctx.session,
        ctx.cfg,
    ) {
        Ok(crate::ui::slash::CompactionDecision::Ready(req)) => {
            *compaction_phase = Some(crate::ui::compaction::spawn(
                *req,
                crate::ui::compaction::CompactionThen::RetryAfterOverflow {
                    prompt: prompt.to_string(),
                    made_progress,
                },
            ));
            // Stay busy while compaction runs; the arm releases / retries.
        }
        Ok(crate::ui::slash::CompactionDecision::NoOp) => {
            // Compaction decided there's nothing to shrink — retrying would just
            // overflow again. Surface it and drop queued messages (safety).
            ctx.renderer.write_line(
                "auto-compact made no progress; leaving session as-is. Lower keep_recent_tokens, configure summarization_provider, or /clear.",
                c_error(),
            )?;
            *is_running = false;
            drop_queued(ctx, interjection_queue, "compact no-op")?;
        }
        Err(e) if crate::provider::is_anthropic_oauth_compaction_disabled_error(&e) => {
            match crate::ui::slash::prepare_prune_only_compaction(
                ctx.renderer,
                ctx.session,
                ctx.cfg,
                crate::provider::ANTHROPIC_OAUTH_COMPACTION_DISABLED,
            )? {
                Some(req) => {
                    ctx.renderer.write_line(
                        "LLM compaction requires a non-Anthropic-OAuth summarization_provider; using prune-only emergency compaction for this retry.",
                        c_error(),
                    )?;
                    *compaction_phase = Some(crate::ui::compaction::spawn_local(
                        req.summary,
                        req.cut_idx,
                        req.tokens_before,
                        crate::ui::compaction::CompactionThen::RetryAfterOverflow {
                            prompt: prompt.to_string(),
                            made_progress,
                        },
                    ));
                }
                None => {
                    ctx.renderer.write_line(
                        "auto-compact could not prune enough context. Configure summarization_provider or use /clear.",
                        c_error(),
                    )?;
                    *is_running = false;
                    drop_queued(ctx, interjection_queue, "compact failure")?;
                }
            }
        }
        Err(e) => {
            ctx.renderer.write_line(
                &format!("auto-compact failed ({e}); leaving session as-is. Configure summarization_provider or use /clear."),
                c_error(),
            )?;
            *is_running = false;
            drop_queued(ctx, interjection_queue, "compact failure")?;
        }
    }
    Ok(())
}

/// Clear queued interjections (they can't run against an over-full context) and
/// tell the user how many were dropped.
fn drop_queued(
    ctx: &mut RunCtx<'_>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    reason: &str,
) -> anyhow::Result<()> {
    let dropped = interjection_queue.lock().unwrap().len();
    interjection_queue.lock().unwrap().clear();
    if dropped > 0 {
        ctx.renderer.write_line(
            &format!(
                "{} queued message{} dropped due to {reason}",
                dropped,
                if dropped == 1 { "" } else { "s" }
            ),
            c_error(),
        )?;
    }
    Ok(())
}

//! Streaming `AgentEvent` arms (`Reasoning`, `Token`) extracted from
//! `run_interactive`. Both feed the shared `render_agent_stream` pipeline;
//! `Token` additionally drives the plugin per-turn batcher and the
//! dirge-ufe0 render coalescer. The caller keeps the loop-control guards
//! (`Reasoning`'s `show_reasoning` skip, the avatar-state set) inline.
//! Behavior is identical to the inline code; pure refactor (dirge-4y4l).

#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::time::Instant;

use crossterm::style::Color;

use crate::ui::agent_io::{RENDER_FRAME, render_agent_stream, should_render_token};
use crate::ui::avatar;
use crate::ui::colors::c_agent;
use crate::ui::events::sanitize_output;
use crate::ui::renderer::Renderer;
use crate::ui::run_handlers::RunCtx;

#[cfg(feature = "plugin")]
use crate::plugin::PluginManager;
#[cfg(feature = "plugin")]
use crate::ui::streaming::TokenBatcher;
#[cfg(feature = "plugin")]
use std::sync::{Arc, Mutex};

/// `AgentEvent::Reasoning` body (after the caller's avatar-state set +
/// `show_reasoning` guard): accumulate the thinking text and repaint it in
/// the DarkMagenta "thinking" register.
pub(crate) fn handle_reasoning(
    ctx: &mut RunCtx<'_>,
    text: &str,
    was_reasoning: &mut bool,
) -> anyhow::Result<()> {
    let safe = sanitize_output(text);
    ctx.reasoning_buf.push_str(&safe);
    // Shared pipeline with Token. The soft, recessive `thinking` register
    // signals the reasoning voice without competing with the agent's prose
    // (replaces the hard DarkMagenta); markdown highlights still ride the
    // theme accessors.
    render_agent_stream(
        ctx.reasoning_buf,
        ctx.reasoning_start_line,
        crate::ui::theme::thinking(),
        ctx.renderer,
    )?;
    *ctx.agent_line_started = true;
    *was_reasoning = true;
    Ok(())
}

/// `AgentEvent::Token` body: accumulate the assistant token, feed the
/// plugin per-turn batcher (`on-message-update`), and coalesce repaints
/// (dirge-ufe0) so a burst paints at most once per frame. `pending` is the
/// caller's `agent_rx.len()` — when 0 we're caught up to the last queued
/// event, so the final token of a burst always lands.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_token(
    ctx: &mut RunCtx<'_>,
    text: &str,
    was_reasoning: &mut bool,
    last_token_render: &mut Option<Instant>,
    pending: usize,
    #[cfg(feature = "plugin")] plugin_manager: Option<&Arc<Mutex<PluginManager>>>,
    #[cfg(feature = "plugin")] token_batcher: &mut TokenBatcher,
    #[cfg(feature = "plugin")] current_turn_text: &mut String,
    #[cfg(feature = "plugin")] current_turn_index: u32,
) -> anyhow::Result<()> {
    ctx.renderer.set_avatar_state(avatar::AvatarState::Speaking);
    if *was_reasoning {
        ctx.renderer.write_line("", Color::White)?;
        *was_reasoning = false;
        ctx.response_buf.clear();
        *ctx.response_start_line = None;
        // End-of-reasoning marker. The reasoning stays rendered in the
        // scroll; we just stop tracking it so the next reasoning burst
        // anchors at a fresh buffer position below the streamed content.
        // `end_reasoning` stashes the burst first so Ctrl+O can still
        // expand it once the response is showing.
        ctx.end_reasoning();
        *ctx.reasoning_start_line = None;
    }
    let safe = sanitize_output(text);
    ctx.response_buf.push_str(&safe);

    // Stream the token into the per-turn batcher + accumulator. When the
    // batcher crosses its threshold, dispatch `on-message-update` with the
    // cumulative text so far. `current_turn_text` is the full turn text for
    // the closing `on-turn-end` event.
    #[cfg(feature = "plugin")]
    if let Some(pm) = plugin_manager {
        current_turn_text.push_str(text);
        if token_batcher.push(text).is_some() {
            let mut mgr = pm.lock_ignore_poison();
            let _ = mgr.dispatch(
                "on-message-update",
                &format!(
                    "@{{:index {} :partial \"{}\"}}",
                    current_turn_index,
                    crate::plugin::escape_janet_string(current_turn_text),
                ),
            );
        }
    }

    // dirge-ufe0: coalesce repaints. Paint only when caught up to the last
    // queued event (pending == 0, so the final token of a burst lands) or a
    // frame interval elapsed (so a long burst still streams visibly). The
    // ToolCall/Done/Error arms flush response_buf, so a coalesced trailing
    // token still renders before the buffer clears.
    let since = last_token_render.map_or(RENDER_FRAME, |t| t.elapsed());
    if should_render_token(pending, since, RENDER_FRAME) {
        render_agent_stream(
            ctx.response_buf,
            ctx.response_start_line,
            c_agent(),
            ctx.renderer,
        )?;
        *last_token_render = Some(Instant::now());
    }
    *ctx.agent_line_started = true;
    Ok(())
}

/// Render the on-screen feedback when a steering message is queued while the
/// agent is mid-stream, WITHOUT duplicating the partial response.
///
/// The renderer's `stream()` always re-renders the FULL `src` it's handed as
/// one open block at the buffer tail. Writing the echo below it (via
/// `write_line`) seals that open block, so the next streamed token re-opens a
/// NEW block with the whole accumulated `response_buf` — painting the sealed
/// partial a second time (the duplicated `<dirge>` block users saw).
///
/// So: flush + seal the partial as its own committed block, echo the queued
/// message + the "(queued…)" notice, then RESET the render buffer. Tokens that
/// stream before the runner reaches the interjection boundary now render as a
/// fresh continuation block instead of re-painting the partial. The session
/// still gets the full text via the runner's own `partial_response` payload
/// (`handle_interjected` / the `Done` arm persist that, not `response_buf`), so
/// clearing here is render-only and loses nothing from history.
pub(crate) fn render_queued_steering(
    renderer: &mut Renderer,
    response_buf: &mut String,
    response_start_line: &mut Option<usize>,
    text: &str,
    notice: &str,
    notice_color: Color,
) -> anyhow::Result<()> {
    if !response_buf.is_empty() {
        // Flush any coalescer-skipped tail so the sealed block matches
        // everything streamed so far.
        renderer.stream(response_buf, c_agent(), true);
        renderer.render_viewport()?;
    }
    renderer.commit_stream();
    for line in text.lines() {
        let safe_line = sanitize_output(line);
        renderer.write_line(&format!("» {}", safe_line), crate::ui::theme::dim())?;
    }
    renderer.write_line(notice, notice_color)?;
    response_buf.clear();
    *response_start_line = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::renderer::Renderer;

    fn screen(r: &Renderer) -> String {
        r.buffer_lines().join("\n")
    }

    /// Regression: queueing a steering message mid-stream must NOT duplicate
    /// the partial `<dirge>` response. Before the fix, the echo's write_line
    /// sealed the open block and the next streamed token re-opened a new block
    /// with the WHOLE accumulated response_buf — painting the partial twice.
    #[test]
    fn queued_steering_midstream_does_not_duplicate_partial() {
        let mut r = Renderer::new().expect("renderer");
        r.set_test_cols(80);
        let mut response_buf = String::new();
        let mut start: Option<usize> = None;

        // Agent streams a partial response (one open block at the tail).
        response_buf.push_str("ALPHAWORD BETAWORD GAMMAWORD");
        r.stream(&response_buf, c_agent(), true);

        // User queues a steering message while the stream is open.
        render_queued_steering(
            &mut r,
            &mut response_buf,
            &mut start,
            "please refactor",
            "(queued)",
            Color::White,
        )
        .unwrap();

        // The render buffer was reset for a fresh continuation block.
        assert!(response_buf.is_empty());
        assert!(start.is_none());

        // A couple more tokens stream before the runner hits the boundary.
        response_buf.push_str("DELTAWORD");
        r.stream(&response_buf, c_agent(), true);
        r.commit_stream();

        let s = screen(&r);
        // Partial body appears exactly once — not re-painted after the seal.
        assert_eq!(
            s.matches("GAMMAWORD").count(),
            1,
            "partial duplicated:\n{s}"
        );
        assert_eq!(
            s.matches("ALPHAWORD").count(),
            1,
            "partial duplicated:\n{s}"
        );
        // Echo + notice + continuation are all present.
        assert!(s.contains("please refactor"), "missing echo:\n{s}");
        assert!(s.contains("(queued)"), "missing notice:\n{s}");
        assert!(s.contains("DELTAWORD"), "missing continuation:\n{s}");
    }

    /// When nothing has streamed yet, the helper just echoes — no empty block,
    /// no panic.
    #[test]
    fn queued_steering_with_empty_buffer_just_echoes() {
        let mut r = Renderer::new().expect("renderer");
        r.set_test_cols(80);
        let mut response_buf = String::new();
        let mut start: Option<usize> = None;
        render_queued_steering(
            &mut r,
            &mut response_buf,
            &mut start,
            "hello",
            "(queued)",
            Color::White,
        )
        .unwrap();
        let s = screen(&r);
        assert!(s.contains("hello"), "missing echo:\n{s}");
        assert!(s.contains("(queued)"), "missing notice:\n{s}");
    }
}

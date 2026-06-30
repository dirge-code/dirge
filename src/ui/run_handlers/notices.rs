//! Small status / notice `AgentEvent` arms extracted from
//! `run_interactive`. Each is render-only — a pure function of the
//! renderer plus the event payload — so they read and test far more
//! easily out here than buried in the multi-thousand-line `select!`
//! loop. Behavior is identical to the inline code; pure refactor.

use crossterm::style::Color;

use crate::agent::agent_loop::message::EscalationReason;
use crate::agent::agent_loop::tool_input_repair::RepairStatsSnapshot;
use crate::ui::renderer::Renderer;
use crate::ui::text_output::{
    strip_leading_system_reminder, write_critic_lines, write_system_lines, write_user_lines,
};
use crate::ui::theme;

/// `AgentEvent::UserMessage` — the literal prompt sent to the LLM. Strips
/// any leading `<system-reminder>` wrapper (added by
/// `prepend_pending_notifications` when background tasks just finished) so
/// the user sees only their own text; the clean copy is already persisted
/// to the session at submit time.
pub(crate) fn handle_user_message(renderer: &mut Renderer, content: &str) -> std::io::Result<()> {
    let visible = strip_leading_system_reminder(content);
    // dirge-i75f: the in-loop finalization nudges (critic / verifier / todo)
    // re-enter as user-role messages so the model acts on them; surface them
    // under the `<critic>` handle/color rather than the user's `<you>`. The tag
    // is stripped from the display.
    if let Some(body) = crate::ui::events::finalization_nudge_body(visible) {
        write_critic_lines(renderer, body)?;
        return renderer.write_line("", Color::White);
    }
    write_user_lines(renderer, visible)?;
    renderer.write_line("", Color::White)
}

/// Render a user-role message that arrives WHILE an assistant response is
/// still the live stream target, finalizing that response first.
///
/// The finalization nudges (critic / verifier / todo) re-enter the loop as
/// user-role messages without a `Done`/`ToolCall` event in between, so the
/// previous turn's stream anchor (`response_start_line`) is still pointing
/// at the just-streamed response. If we render the nudge and leave the
/// anchor live, the NEXT turn's `replace_from(anchor, …)` truncates the
/// buffer from there and overwrites the nudge — it disappears on screen a
/// moment later even though the model still has it in context. So commit
/// the in-flight response and drop the anchor first; the next response then
/// streams BELOW the nudge. A no-op for an ordinary user turn (the prior
/// `Done` already cleared these). Mirrors the reset the `ToolCall`/`Done`
/// handlers do for the multi-turn-with-tools path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_user_message_after_response(
    renderer: &mut Renderer,
    content: &str,
    response_buf: &mut String,
    response_start_line: &mut Option<usize>,
    reasoning_buf: &mut String,
    reasoning_start_line: &mut Option<usize>,
    agent_line_started: &mut bool,
) -> anyhow::Result<()> {
    if !response_buf.is_empty() {
        // Flush any trailing token the render coalescer skipped, then close
        // the line so the nudge lands on a fresh row below it.
        crate::ui::agent_io::render_agent_stream(
            response_buf,
            response_start_line,
            crate::ui::colors::c_agent(),
            renderer,
        )?;
        if *agent_line_started {
            renderer.write_line("", Color::White)?;
        }
    }
    *agent_line_started = false;
    response_buf.clear();
    *response_start_line = None;
    // The next turn's reasoning must anchor fresh too.
    reasoning_buf.clear();
    *reasoning_start_line = None;

    handle_user_message(renderer, content)?;
    Ok(())
}

/// `AgentEvent::SystemNotice` — a dirge-originated `<system>` log line
/// (e.g. the max-agent-turns cap), rendered in the warning color so it
/// reads as runtime output rather than something the user typed.
pub(crate) fn handle_system_notice(renderer: &mut Renderer, content: &str) -> std::io::Result<()> {
    write_system_lines(renderer, content)?;
    renderer.write_line("", Color::White)
}

/// `AgentEvent::RetryNotice` — transient backoff banner (PROV-2) so the
/// user isn't staring at silence during retry delays.
pub(crate) fn handle_retry_notice(
    renderer: &mut Renderer,
    attempt: u32,
    delay_ms: u64,
) -> std::io::Result<()> {
    renderer.write_line(
        &format!("  ⟳ retry {attempt} ({delay_ms}ms)…"),
        theme::dim(),
    )
}

/// `AgentEvent::EscalationActivated` — Phase 4 dual-client tiering: the
/// next LLM call swapped to the escalation provider. Surface it so the
/// provider takeover isn't silent.
pub(crate) fn handle_escalation_activated(
    renderer: &mut Renderer,
    provider: &str,
    reason: &EscalationReason,
) -> std::io::Result<()> {
    let summary = reason.summary();
    renderer.write_line(
        &format!("  ↑ escalating to {provider} (next turn): {summary}"),
        theme::dim(),
    )
}

/// `AgentEvent::RepairStats` — per-run input-repair telemetry summary.
/// The caller guards the empty-snapshot case (it `continue`s the loop to
/// skip the trailing status redraw); this only renders the summary line.
pub(crate) fn handle_repair_stats(
    renderer: &mut Renderer,
    snapshot: &RepairStatsSnapshot,
) -> std::io::Result<()> {
    let mut parts: Vec<String> = Vec::new();
    if snapshot.md_link_unwrapped > 0 {
        parts.push(format!("{} md-link", snapshot.md_link_unwrapped));
    }
    if snapshot.null_stripped > 0 {
        parts.push(format!("{} null-strip", snapshot.null_stripped));
    }
    if snapshot.json_string_to_array > 0 {
        parts.push(format!("{} json-array", snapshot.json_string_to_array));
    }
    if snapshot.object_to_array > 0 {
        parts.push(format!("{} obj-to-array", snapshot.object_to_array));
    }
    if snapshot.bare_string_to_array > 0 {
        parts.push(format!("{} bare-to-array", snapshot.bare_string_to_array));
    }
    let total = snapshot.total_successful();
    let mut line = format!("  ⊕ repaired {total} input(s): {}", parts.join(", "));
    if snapshot.invalid > 0 {
        line.push_str(&format!("; {} invalid", snapshot.invalid));
    }
    renderer.write_line(&line, theme::dim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::critic::CRITIC_TAG;
    use crate::ui::agent_io::render_agent_stream;
    use crate::ui::colors::c_agent;
    use crate::ui::renderer::Renderer;

    fn has_line_containing(r: &Renderer, needle: &str) -> bool {
        r.buffer_lines()
            .iter()
            .any(|l| crate::ui::ansi::strip_ansi(l).contains(needle))
    }

    /// dirge-m10x: a critic nudge re-enters as a user-role message while the
    /// previous turn's stream anchor is still live. It must survive the NEXT
    /// turn's stream — without the anchor reset, `replace_from` overwrote it
    /// and it vanished from the log.
    #[test]
    fn critic_nudge_survives_the_next_turn_stream() {
        let mut r = Renderer::new().unwrap();

        // Turn 1: stream an assistant response, anchoring response_start_line.
        let mut response_buf = String::from("here is the answer");
        let mut response_start_line: Option<usize> = None;
        let mut reasoning_buf = String::new();
        let mut reasoning_start_line: Option<usize> = None;
        let mut agent_line_started = true;
        render_agent_stream(&response_buf, &mut response_start_line, c_agent(), &mut r).unwrap();
        assert!(response_start_line.is_some(), "turn 1 anchored the stream");

        // Critic nudge re-enters as a user-role message.
        handle_user_message_after_response(
            &mut r,
            &format!("{CRITIC_TAG} verify the build before finishing"),
            &mut response_buf,
            &mut response_start_line,
            &mut reasoning_buf,
            &mut reasoning_start_line,
            &mut agent_line_started,
        )
        .unwrap();

        // Anchor + buffers reset so the next turn streams BELOW the nudge.
        assert_eq!(response_start_line, None);
        assert!(response_buf.is_empty());
        assert!(
            has_line_containing(&r, "verify the build"),
            "critic nudge rendered"
        );

        // Turn 2: the model responds to the critic — fresh stream.
        response_buf.push_str("ok, ran the tests");
        render_agent_stream(&response_buf, &mut response_start_line, c_agent(), &mut r).unwrap();

        // The nudge must still be on screen, and the new turn below it.
        assert!(
            has_line_containing(&r, "verify the build"),
            "critic nudge must survive the next turn's stream",
        );
        assert!(has_line_containing(&r, "ran the tests"));
    }

    /// An ordinary user turn (no in-flight response) renders unchanged — the
    /// finalize path is a no-op when the anchor was already cleared by Done.
    #[test]
    fn ordinary_user_message_renders_normally() {
        let mut r = Renderer::new().unwrap();
        let mut response_buf = String::new();
        let mut response_start_line: Option<usize> = None;
        let mut reasoning_buf = String::new();
        let mut reasoning_start_line: Option<usize> = None;
        let mut agent_line_started = false;
        handle_user_message_after_response(
            &mut r,
            "what time is it?",
            &mut response_buf,
            &mut response_start_line,
            &mut reasoning_buf,
            &mut reasoning_start_line,
            &mut agent_line_started,
        )
        .unwrap();
        assert!(has_line_containing(&r, "what time is it?"));
    }
}

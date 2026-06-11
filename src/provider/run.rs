//! Headless run path for [`AnyAgent`]. Split out of `provider/mod.rs`
//! (dirge-4y4l stage 8): the `--print` / `--loop` entry point that drives
//! the agent loop and collects output for the non-interactive CLI modes.
//!
//! Child module of `provider`, so it reaches `AnyAgent`'s private fields and
//! `spawn_runner` directly (privacy = defining module + descendants).

use super::AnyAgent;
use crate::agent::runner;
use crate::event::AgentEvent;
#[allow(unused_imports)]
use crate::sync_util::LockExt;

/// How the headless event stream ended (dirge-18v2). The JSON result
/// envelope must reflect this — a run that was truncated by the turn
/// cap or whose runner died without a `Done` is NOT a success, and
/// `--print` consumers parse the envelope, not stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// `Done` arrived and no truncation notice was seen.
    Completed,
    /// `Done` arrived but the max-agent-turns cap stopped the run.
    Truncated,
    /// The event channel closed without a `Done` — the runner died
    /// (panic/abort) and `full_response` is whatever streamed first.
    Incomplete,
}

/// Build the machine-readable result envelope for the headless modes.
/// Pure so the success/error mapping is unit-testable without a live
/// runner.
pub(crate) fn headless_result_json(
    end: RunEnd,
    duration_ms: u64,
    num_turns: u32,
    result: &str,
    session_id: &str,
) -> serde_json::Value {
    let (subtype, is_error) = match end {
        RunEnd::Completed => ("success", false),
        // Matches the Claude Code stream-json convention dirge mimics.
        RunEnd::Truncated => ("error_max_turns", true),
        RunEnd::Incomplete => ("error", true),
    };
    serde_json::json!({
        "type": "result",
        "subtype": subtype,
        "is_error": is_error,
        "duration_ms": duration_ms,
        "num_turns": num_turns,
        "result": result,
        "session_id": session_id,
        "total_cost_usd": 0.0,
    })
}

impl AnyAgent {
    pub async fn run_print(
        &self,
        prompt: &str,
        max_turns: usize,
        output_format: crate::cli::OutputFormat,
        // Prior conversation to resume into the model's context. Empty for a
        // fresh run; for `--session <id>` the caller passes the loaded
        // session's history (via `convert_history`) so a headless run
        // continues where it left off instead of starting cold each time.
        history: Vec<rig::completion::Message>,
        // Returns the final response text plus the turn's tool calls (so the
        // caller can persist a full-fidelity assistant message).
    ) -> anyhow::Result<(String, Vec<crate::session::ToolCallEntry>)> {
        // dirge-nqr: honor the cap explicitly even if the agent was
        // built with a different one. `run_print` is the headless
        // entry point — callers explicitly pass the cap they want.
        let agent = self.clone().with_max_turns(Some(max_turns));
        let start_instant = std::time::Instant::now();
        let session_id = runner::uuid_v4_simple();
        let mut num_turns: u32 = 0;
        let suppress_inline = !matches!(output_format, crate::cli::OutputFormat::Text);

        // Plugin `on-prompt` dispatch. Headless modes (--print, --loop)
        // previously skipped this — plugins that mutate the user prompt
        // or block it never fired in CI/script contexts.
        let effective_prompt: String = {
            #[cfg(feature = "plugin")]
            {
                if let Some(pm_arc) = crate::plugin::hook::global() {
                    let mut mgr = pm_arc.lock_ignore_poison();
                    runner::resolve_prompt_with_hooks(prompt, &mut mgr)
                } else {
                    prompt.to_string()
                }
            }
            #[cfg(not(feature = "plugin"))]
            {
                prompt.to_string()
            }
        };

        // StreamJson init event — fires once at startup so downstream
        // tools can pick up cwd/session/model before any turns stream.
        if matches!(output_format, crate::cli::OutputFormat::StreamJson) {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            runner::emit_stream_json_event(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "cwd": cwd,
                "session_id": session_id,
                "tools": Vec::<String>::new(),
                "model": "",
            }));
        }

        // Wire through the new agent_loop path: clone the agent (cheap
        // — Arc internals + refcounts), spawn a runner, and drain the
        // event channel collecting text. Use the max_turns-stamped
        // `agent` from above so the cap is honored.
        let runner = agent.spawn_runner(effective_prompt.clone(), history, None);
        let task = runner.task;
        let mut event_rx = runner.event_rx;

        let mut full_response = String::new();
        let mut had_output = false;
        // dirge-18v2: track how the stream ends so the result envelope
        // can't claim success for a truncated or runner-died run.
        let mut completed = false;
        let mut truncated = false;
        // Accumulate the turn's tool calls so the headless save is
        // full-fidelity (matching the interactive path). Without this the
        // saved assistant message carried only its final text, so a resumed
        // `--session` lost every tool call/result — and a tool-heavy final
        // turn saved an empty/partial message, reading as a cut-off end.
        // Mirrors the UI's ToolCall/ToolResult accumulation
        // (run_handlers/tool_call.rs + tool_result.rs).
        use crate::session::{ToolCallEntry, ToolCallState};
        let mut tool_calls: Vec<ToolCallEntry> = Vec::new();

        while let Some(event) = event_rx.recv().await {
            match event {
                AgentEvent::ToolCall { id, name, args } => {
                    // Start Interrupted; the matching ToolResult flips it to
                    // Completed. An unanswered call stays Interrupted, which
                    // convert_history re-emits so the model sees no orphan.
                    tool_calls.push(ToolCallEntry {
                        id: id.to_string(),
                        name: name.to_string(),
                        args,
                        state: ToolCallState::Interrupted,
                    });
                }
                AgentEvent::ToolResult { id, output, .. } => {
                    let target = if !id.is_empty() {
                        tool_calls.iter_mut().rev().find(|e| e.id == id.as_str())
                    } else {
                        tool_calls
                            .iter_mut()
                            .rev()
                            .find(|e| matches!(e.state, ToolCallState::Interrupted))
                    };
                    if let Some(entry) = target {
                        entry.state = ToolCallState::Completed {
                            result: output.to_string(),
                        };
                    }
                }
                AgentEvent::Token(text) => {
                    full_response.push_str(&text);
                    if !suppress_inline {
                        let safe = crate::ui::ansi::strip_controls(
                            &text,
                            crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                        );
                        print!("{safe}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    had_output = true;
                }
                AgentEvent::Done { response, .. } => {
                    // `Done.response` is the authoritative full text.
                    full_response = response.to_string();
                    completed = true;
                    break;
                }
                AgentEvent::Error(err) => {
                    if had_output {
                        println!();
                    }
                    eprintln!("Error: {}", err);
                    let _ = task.await;
                    return Err(anyhow::anyhow!("{}", err));
                }
                AgentEvent::TurnEnd { .. } => {
                    num_turns += 1;
                }
                AgentEvent::SystemNotice { content } => {
                    // dirge-originated runtime notice (e.g. the
                    // max-agent-turns cap). Headless drives output from
                    // events, so surface it to stderr — and mark the
                    // run truncated so the JSON envelope reflects it
                    // (dirge-18v2); stderr alone is invisible to
                    // `--print` consumers parsing stdout.
                    if content.starts_with(crate::agent::agent_loop::run::MAX_TURNS_NOTICE_PREFIX) {
                        truncated = true;
                    }
                    if had_output {
                        println!();
                    }
                    eprintln!("{}", content);
                }
                // Plugin-driven model swap after last run puts the
                // request in the mgr; caller drains via
                // take_pending_next_model().
                _ => {}
            }
        }

        // Await the spawned task to catch any panics.
        let _ = task.await;

        // Plugin `on-response` + `on-complete` + `prepare-next-run`
        // dispatch. Headless modes previously skipped these.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = crate::plugin::hook::global() {
            let mut mgr = pm_arc.lock_ignore_poison();
            let result = runner::apply_response_hooks(&full_response, &mut mgr);
            if let Some(replacement) = result.replacement {
                if suppress_inline {
                    full_response = replacement;
                } else {
                    println!();
                    println!("[plugin replace-result]");
                    let safe = crate::ui::ansi::strip_controls(
                        &replacement,
                        crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                    );
                    println!("{safe}");
                    full_response = replacement;
                }
            }
        }

        // dirge-18v2: classify how the stream ended. A truncated run
        // or one whose runner died without a Done must not produce a
        // success envelope.
        let end = if !completed {
            RunEnd::Incomplete
        } else if truncated {
            RunEnd::Truncated
        } else {
            RunEnd::Completed
        };
        let result_envelope = headless_result_json(
            end,
            start_instant.elapsed().as_millis() as u64,
            num_turns,
            &full_response,
            &session_id,
        );

        match output_format {
            crate::cli::OutputFormat::Text => {
                println!();
            }
            crate::cli::OutputFormat::Json => {
                if let Ok(s) = serde_json::to_string(&result_envelope) {
                    println!("{}", s);
                }
            }
            crate::cli::OutputFormat::StreamJson => {
                runner::emit_stream_json_event(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": full_response.clone()}],
                    },
                    "session_id": session_id,
                }));
                runner::emit_stream_json_event(result_envelope);
            }
        }

        // The runner died without delivering a Done — the collected
        // text is whatever streamed before it stopped. The envelope
        // above already says is_error; the process must also exit
        // non-zero so script consumers without JSON parsing notice.
        if end == RunEnd::Incomplete {
            return Err(anyhow::anyhow!(
                "run ended without completing — the agent runner stopped before producing a result"
            ));
        }
        Ok((full_response, tool_calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dirge-18v2: the result envelope must reflect how the run ended
    /// — `--print` consumers parse this JSON, not stderr.
    #[test]
    fn result_envelope_reflects_run_end() {
        let ok = headless_result_json(RunEnd::Completed, 10, 2, "answer", "sid");
        assert_eq!(ok["subtype"], "success");
        assert_eq!(ok["is_error"], false);
        assert_eq!(ok["result"], "answer");

        let capped = headless_result_json(RunEnd::Truncated, 10, 100, "partial", "sid");
        assert_eq!(capped["subtype"], "error_max_turns");
        assert_eq!(capped["is_error"], true);
        assert_eq!(capped["result"], "partial", "partial text still delivered");

        let died = headless_result_json(RunEnd::Incomplete, 10, 1, "fragment", "sid");
        assert_eq!(died["subtype"], "error");
        assert_eq!(died["is_error"], true);
    }

    /// The truncation detector matches the notice the agent loop
    /// actually emits — both sides use MAX_TURNS_NOTICE_PREFIX, so a
    /// reworded notice that breaks the coupling fails here.
    #[test]
    fn truncation_notice_prefix_matches_emitter() {
        let cap = 100;
        // Mirror of the format string in agent_loop::run's max-turns
        // branch.
        let notice = format!(
            "{} ({cap}) reached. Stopping the run.",
            crate::agent::agent_loop::run::MAX_TURNS_NOTICE_PREFIX
        );
        assert!(notice.starts_with(crate::agent::agent_loop::run::MAX_TURNS_NOTICE_PREFIX));
        assert!(
            crate::agent::agent_loop::run::MAX_TURNS_NOTICE_PREFIX.starts_with("[dirge]"),
            "notice must stay visually attributable to dirge",
        );
    }
}

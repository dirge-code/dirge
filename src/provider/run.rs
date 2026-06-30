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

/// Build a stream-json `assistant` event for one turn (dirge-kuqp).
/// The text block carries the turn's streamed text (omitted when
/// empty so a tool-only turn doesn't emit a stray empty block),
/// followed by one `tool_use` block per call. Shape mirrors Claude
/// Code so consumers parsing `claude -p --output-format stream-json`
/// work against dirge unchanged.
pub(crate) fn stream_json_assistant_event(
    text: &str,
    tool_uses: &[(String, String, serde_json::Value)],
    session_id: &str,
) -> serde_json::Value {
    let mut content: Vec<serde_json::Value> = Vec::new();
    if !text.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": text}));
    }
    for (id, name, args) in tool_uses {
        content.push(serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": args,
        }));
    }
    serde_json::json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": content,
        },
        "session_id": session_id,
    })
}

/// Build a stream-json `user` event carrying a turn's tool results
/// (dirge-kuqp) as `tool_result` content blocks keyed by the
/// originating call id — the Claude Code shape for tool output.
pub(crate) fn stream_json_tool_result_event(
    results: &[(String, String)],
    session_id: &str,
) -> serde_json::Value {
    let content: Vec<serde_json::Value> = results
        .iter()
        .map(|(id, output)| {
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": output,
            })
        })
        .collect();
    serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": content,
        },
        "session_id": session_id,
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

        // dirge-kuqp: per-turn buffers for incremental stream-json. The
        // bridge collapses `TurnEnd` to a bare index, so we rebuild each
        // turn's assistant/user envelopes from the streamed events and
        // emit them at the turn boundary — making `stream-json` a true
        // incremental stream instead of one final assistant blob.
        let stream_json = matches!(output_format, crate::cli::OutputFormat::StreamJson);
        let mut turn_text = String::new();
        let mut turn_tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();
        let mut turn_tool_results: Vec<(String, String)> = Vec::new();

        while let Some(event) = event_rx.recv().await {
            match event {
                AgentEvent::ToolCall { id, name, args } => {
                    if stream_json {
                        turn_tool_uses.push((id.to_string(), name.to_string(), args.clone()));
                    }
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
                    if stream_json {
                        // Key the result by its call id. Providers that
                        // don't emit ids leave it empty for every call in
                        // the turn, so there's nothing to pair against —
                        // consumers fall back to positional matching, same
                        // as the persistence path below. Results can arrive
                        // in completion order under parallel dispatch, so we
                        // deliberately don't try to reconstruct an index.
                        turn_tool_results.push((id.to_string(), output.to_string()));
                    }
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
                    if stream_json {
                        turn_text.push_str(&text);
                    }
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
                    // dirge-kuqp: flush this turn's assistant text +
                    // tool_use blocks, then any tool results, as soon as
                    // the turn closes. This is what makes the stream
                    // incremental for multi-turn agentic runs.
                    if stream_json {
                        if !turn_text.is_empty() || !turn_tool_uses.is_empty() {
                            runner::emit_stream_json_event(stream_json_assistant_event(
                                &turn_text,
                                &turn_tool_uses,
                                &session_id,
                            ));
                        }
                        if !turn_tool_results.is_empty() {
                            runner::emit_stream_json_event(stream_json_tool_result_event(
                                &turn_tool_results,
                                &session_id,
                            ));
                        }
                        turn_text.clear();
                        turn_tool_uses.clear();
                        turn_tool_results.clear();
                    }
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

        // dirge-kuqp: a plugin `on-response` replacement rewrites the
        // final answer after the last turn already streamed its
        // assistant event. Track it so the StreamJson arm can emit a
        // corrected assistant event rather than leaving the stream
        // showing only the pre-replacement text.
        #[allow(unused_mut)]
        let mut response_replaced = false;

        // Plugin `on-response` + `on-complete` + `prepare-next-run`
        // dispatch. Headless modes previously skipped these.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = crate::plugin::hook::global() {
            let mut mgr = pm_arc.lock_ignore_poison();
            let result = runner::apply_response_hooks(&full_response, &mut mgr);
            if let Some(replacement) = result.replacement {
                response_replaced = true;
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
                // dirge-kuqp: each completed turn already streamed its
                // assistant event at TurnEnd. Two cases still need an
                // assistant event here:
                //   1. A partial final turn whose TurnEnd never arrived
                //      (runner died / interjected mid-turn) — flush the
                //      buffered text + tool calls.
                //   2. A plugin `on-response` replacement rewrote the final
                //      answer after the last turn streamed — emit the
                //      corrected text so the stream matches the result
                //      envelope (which carries the replacement).
                // The two can co-occur (a runner died mid-turn AND a plugin
                // replaced the result): the replacement is authoritative, so
                // it wins the text, but still carry any buffered tool_use
                // blocks from the dead turn so the stream stays well-formed.
                let leftover = !turn_text.is_empty() || !turn_tool_uses.is_empty();
                if response_replaced {
                    runner::emit_stream_json_event(stream_json_assistant_event(
                        &full_response,
                        &turn_tool_uses,
                        &session_id,
                    ));
                } else if leftover {
                    runner::emit_stream_json_event(stream_json_assistant_event(
                        &turn_text,
                        &turn_tool_uses,
                        &session_id,
                    ));
                }
                if (response_replaced || leftover) && !turn_tool_results.is_empty() {
                    runner::emit_stream_json_event(stream_json_tool_result_event(
                        &turn_tool_results,
                        &session_id,
                    ));
                }
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

    /// dirge-kuqp: a turn's assistant event carries its streamed text
    /// as a `text` block followed by one `tool_use` block per call,
    /// matching the Claude Code stream-json shape consumers parse.
    #[test]
    fn assistant_event_has_text_then_tool_use_blocks() {
        let uses = vec![(
            "toolu_1".to_string(),
            "read".to_string(),
            serde_json::json!({"path": "a.rs"}),
        )];
        let ev = stream_json_assistant_event("reading the file", &uses, "sid");
        assert_eq!(ev["type"], "assistant");
        assert_eq!(ev["session_id"], "sid");
        assert_eq!(ev["message"]["role"], "assistant");
        let content = ev["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "reading the file");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "toolu_1");
        assert_eq!(content[1]["name"], "read");
        assert_eq!(content[1]["input"]["path"], "a.rs");
    }

    /// An empty text block is omitted — a tool-only turn (no narration)
    /// emits just the `tool_use` block, not a stray empty text block.
    #[test]
    fn assistant_event_omits_empty_text() {
        let uses = vec![(
            "toolu_1".to_string(),
            "list_dir".to_string(),
            serde_json::json!({}),
        )];
        let ev = stream_json_assistant_event("", &uses, "sid");
        let content = ev["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
    }

    /// A pure-text turn with no tool calls emits a single text block —
    /// the unchanged single-turn shape (system, assistant, result).
    #[test]
    fn assistant_event_text_only() {
        let ev = stream_json_assistant_event("the answer", &[], "sid");
        let content = ev["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "the answer");
    }

    /// Tool results come back as a `user` event with `tool_result`
    /// content blocks keyed by the originating call id — the Claude
    /// Code shape for a turn's tool output.
    #[test]
    fn tool_result_event_shape() {
        let results = vec![
            ("toolu_1".to_string(), "file contents".to_string()),
            ("toolu_2".to_string(), "dir listing".to_string()),
        ];
        let ev = stream_json_tool_result_event(&results, "sid");
        assert_eq!(ev["type"], "user");
        assert_eq!(ev["session_id"], "sid");
        assert_eq!(ev["message"]["role"], "user");
        let content = ev["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
        assert_eq!(content[0]["content"], "file contents");
        assert_eq!(content[1]["tool_use_id"], "toolu_2");
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

//! Opt-in dump of outgoing provider requests — "which prompt goes to which
//! provider, and why" (dirge-wire).
//!
//! dirge normally logs only cache-prefix HASHES of the system prompt + tools
//! (see `agent_loop::rig_stream_factory::emit_cache_prefix_event`), never the
//! payloads, so it's hard to tell why a secondary completion (critic /
//! summarizer / approval evaluator / a forked review or subagent) fired
//! mid-session. This module fills that gap behind an env switch so the default
//! path stays silent and allocation-free.
//!
//! Enable with:
//!   `DIRGE_DUMP_REQUESTS=1`     — one summary line per request: purpose,
//!                                 provider, tool count/names, reasoning flag,
//!                                 and byte sizes.
//!   `DIRGE_DUMP_REQUESTS=full`  — also log the system prompt / one-shot prompt
//!                                 body verbatim.
//!
//! Emitted at INFO on target `dirge::wire`, so it lands in `dirge.log` whenever
//! file logging is on (`-v`, `RUST_LOG`, or `DIRGE_LOG`), or scope it with
//! `RUST_LOG=dirge::wire=info`.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DumpMode {
    Off,
    Summary,
    Full,
}

/// Resolve the dump mode from `DIRGE_DUMP_REQUESTS`. Read per call (requests
/// are rare relative to the env lookup, and re-reading avoids a stale cache if
/// the var is set after start).
pub fn dump_mode() -> DumpMode {
    match std::env::var("DIRGE_DUMP_REQUESTS").ok().as_deref() {
        Some("full") | Some("2") => DumpMode::Full,
        Some(s) if !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("off") => DumpMode::Summary,
        _ => DumpMode::Off,
    }
}

/// True when any dump is requested — lets callers skip building dump-only data.
pub fn enabled() -> bool {
    dump_mode() != DumpMode::Off
}

/// Dump a SIDE-LLM one-shot request — the tool-less completions that share
/// `summarize::oneshot_with_model` (summarizer, critic, approval evaluator,
/// goal). `purpose` is the call's label (the "why"); these never carry tools.
/// `provider` is the canonical provider kind (e.g. "anthropic", "openai");
/// `model` is the model identifier string.
pub fn dump_oneshot(purpose: &str, provider: &str, model: &str, preamble: &str, prompt: &str) {
    match dump_mode() {
        DumpMode::Off => {}
        DumpMode::Summary => tracing::info!(
            target: "dirge::wire",
            purpose,
            kind = "one-shot",
            provider,
            model,
            tools = 0,
            preamble_bytes = preamble.len(),
            prompt_bytes = prompt.len(),
            "provider request",
        ),
        DumpMode::Full => tracing::info!(
            target: "dirge::wire",
            purpose,
            kind = "one-shot",
            provider,
            model,
            tools = 0,
            preamble_bytes = preamble.len(),
            prompt_bytes = prompt.len(),
            preamble = %preamble,
            prompt = %prompt,
            "provider request",
        ),
    }
}

/// Dump a MAIN-LOOP / agent request — turns, escalation, subagents, and forked
/// review/curator runs all flow through the rig stream factory. `provider` is
/// the resolved provider alias; `model` is the model identifier; `tool_names`
/// distinguishes a tool-carrying turn from a stripped one; `messages_bytes` is
/// the approximate byte size of the conversation history sent in the request.
pub fn dump_turn(
    provider: Option<&str>,
    model: &str,
    system_prompt: &str,
    history_len: usize,
    messages_bytes: usize,
    tool_names: &[String],
    reasoning: bool,
) {
    match dump_mode() {
        DumpMode::Off => {}
        DumpMode::Summary => tracing::info!(
            target: "dirge::wire",
            purpose = "turn",
            kind = "agent",
            provider = provider.unwrap_or("default"),
            model,
            system_bytes = system_prompt.len(),
            history_len,
            messages_bytes,
            tool_count = tool_names.len(),
            tools = ?tool_names,
            reasoning,
            "provider request",
        ),
        DumpMode::Full => tracing::info!(
            target: "dirge::wire",
            purpose = "turn",
            kind = "agent",
            provider = provider.unwrap_or("default"),
            model,
            system_bytes = system_prompt.len(),
            history_len,
            messages_bytes,
            tool_count = tool_names.len(),
            tools = ?tool_names,
            reasoning,
            system = %system_prompt,
            "provider request",
        ),
    }
}

/// Format a SUMMARY-LINE string for one-shot requests. Only used in
/// unit tests — production goes through tracing.
#[allow(dead_code)]
pub fn format_oneshot_summary(
    purpose: &str,
    provider: &str,
    model: &str,
    preamble: &str,
    prompt: &str,
) -> String {
    format!(
        "oneshot purpose=\"{purpose}\" provider={provider} model={model} tools=0 preamble_bytes={} prompt_bytes={}",
        preamble.len(),
        prompt.len(),
    )
}

/// Format a SUMMARY-LINE string for agent turn requests.
/// Only used in unit tests — production goes through tracing.
#[allow(dead_code)]
pub fn format_turn_summary(
    provider: &str,
    model: &str,
    system_prompt: &str,
    history_len: usize,
    messages_bytes: usize,
    tool_names: &[String],
    reasoning: bool,
) -> String {
    format!(
        "turn provider={provider} model={model} system_bytes={} history_len={history_len} messages_bytes={messages_bytes} tool_count={} tools={tool_names:?} reasoning={reasoning}",
        system_prompt.len(),
        tool_names.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_env<T>(val: Option<&str>, f: impl FnOnce() -> T) -> T {
        // Serialize via the process env; tests in this module run in-process
        // and don't set it concurrently.
        match val {
            Some(v) => unsafe { std::env::set_var("DIRGE_DUMP_REQUESTS", v) },
            None => unsafe { std::env::remove_var("DIRGE_DUMP_REQUESTS") },
        }
        let out = f();
        unsafe { std::env::remove_var("DIRGE_DUMP_REQUESTS") };
        out
    }

    #[test]
    fn dump_mode_parses_env() {
        with_env(None, || assert!(matches!(dump_mode(), DumpMode::Off)));
        with_env(Some(""), || assert!(matches!(dump_mode(), DumpMode::Off)));
        with_env(Some("0"), || assert!(matches!(dump_mode(), DumpMode::Off)));
        with_env(Some("off"), || {
            assert!(matches!(dump_mode(), DumpMode::Off))
        });
        with_env(Some("1"), || {
            assert!(matches!(dump_mode(), DumpMode::Summary))
        });
        with_env(Some("yes"), || {
            assert!(matches!(dump_mode(), DumpMode::Summary))
        });
        with_env(Some("full"), || {
            assert!(matches!(dump_mode(), DumpMode::Full))
        });
        with_env(Some("2"), || assert!(matches!(dump_mode(), DumpMode::Full)));
    }

    #[test]
    fn format_oneshot_summary_produces_expected() {
        let out = format_oneshot_summary(
            "critic",
            "anthropic",
            "claude-sonnet-4-20250514",
            "You are a code critic.",
            "Review this diff:\nfn main() {}",
        );
        assert!(out.starts_with("oneshot "));
        assert!(out.contains(r#"purpose="critic""#));
        assert!(out.contains("provider=anthropic"));
        assert!(out.contains("model=claude-sonnet-4-20250514"));
        assert!(out.contains("tools=0"));
        assert!(out.contains("preamble_bytes=22"));
        assert!(out.contains("prompt_bytes=30"));
    }

    #[test]
    fn format_turn_summary_produces_expected() {
        let tools = vec!["bash".to_string(), "read".to_string()];
        let out = format_turn_summary(
            "openrouter",
            "o4-mini",
            "You are an AI coding agent.",
            12,
            8432,
            &tools,
            true,
        );
        assert!(out.starts_with("turn "));
        assert!(out.contains("provider=openrouter"));
        assert!(out.contains("model=o4-mini"));
        assert!(out.contains("system_bytes=27"));
        assert!(out.contains("history_len=12"));
        assert!(out.contains("messages_bytes=8432"));
        assert!(out.contains("tool_count=2"));
        assert!(out.contains("reasoning=true"));
        assert!(out.contains("bash"));
        assert!(out.contains("read"));
    }
}

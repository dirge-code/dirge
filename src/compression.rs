use anyhow::{Context as _, Result};

/// Process-global compression configuration, set once at startup from the
/// `[compression]` config section (with env var overrides checked later at
/// each `resolve_compression_*` call site). Initialized by
/// [`init_from_config`] right after the runtime Config is loaded from disk.
static COMPRESSION_CFG: std::sync::OnceLock<crate::config::Compression> =
    std::sync::OnceLock::new();

/// Seed the compression config from the loaded runtime Config. Call ONCE
/// at startup, BEFORE any provider client is built.
///
/// Every field is optional; absent ones take the defaults documented on
/// [`crate::config::Compression`] (`enabled = true`, `preset = "dirge"`).
pub fn init_from_config(cfg: crate::config::Compression) {
    let _ = COMPRESSION_CFG.set(cfg);
}

/// Was `--no-compression` passed on the command line? Set once at startup
/// from the parsed CLI, before any provider client is built.
static CLI_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the `--no-compression` CLI flag. Call at startup alongside
/// [`init_from_config`].
pub fn set_cli_disabled(disabled: bool) {
    CLI_DISABLED.store(disabled, std::sync::atomic::Ordering::Release);
}

/// Was compression disabled by the `--no-compression` CLI flag?
pub fn cli_disabled() -> bool {
    CLI_DISABLED.load(std::sync::atomic::Ordering::Acquire)
}

/// Was compression enabled in the config file? Defaults to `true` if
/// `init_from_config` was never called (feature compiled in but config
/// not yet loaded — fail-safe: assume on).
pub fn configured_enabled() -> bool {
    COMPRESSION_CFG
        .get()
        .and_then(|c| c.enabled)
        .unwrap_or(true)
}

/// Resolve whether the compression interceptor runs, from the three sources
/// that can speak to it. Pure so the precedence is unit-testable without the
/// process-global state the production callers read from.
///
/// Precedence is most-local-wins: `--no-compression` (this invocation, typed
/// deliberately) beats `DIRGE_COMPRESSION` (this shell, possibly a stale
/// export) beats `[compression].enabled` (this project, set once). Absent
/// everywhere → on; compression is opt-out.
///
/// Only the listed disable spellings turn it off. An unrecognized
/// `DIRGE_COMPRESSION` value leaves the engine on rather than silently
/// disabling it over a typo.
pub fn resolve_enabled(cli_disabled: bool, env: Option<&str>, configured: Option<bool>) -> bool {
    if cli_disabled {
        return false;
    }
    match env {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no" | "disabled"
        ),
        None => configured.unwrap_or(true),
    }
}

/// Which preset did the config file choose? Defaults to `"dirge"`.
pub fn configured_preset() -> String {
    COMPRESSION_CFG
        .get()
        .and_then(|c| c.preset.clone())
        .unwrap_or_else(|| "dirge".to_string())
}

/// Apply the `[compression]` tool-output overrides (dirge-09e8) on top of a
/// resolved preset. These exist so `scripts/loop-ab.sh` can stand up a control
/// arm on the pre-fix behavior; absent keys leave the preset's own defaults —
/// which are the fixed ones — in place.
fn apply_toolout_overrides(c: &mut crate::llmtrim::config::DenseConfig) {
    let Some(cfg) = COMPRESSION_CFG.get() else {
        return;
    };
    if let Some(v) = cfg.trim_user_text {
        c.toolout_user_text = v;
    }
    if let Some(v) = cfg.window_code {
        c.toolout_code = v;
    }
    if let Some(ref v) = cfg.header {
        c.toolout_header = v.clone();
    }
    if let Some(v) = cfg.verbatim {
        c.toolout_verbatim = v;
    }
}

/// Dirge's default compression config: "lossless + tool-output windowing, no
/// output-shaping" — the A/B-validated safe default.
///
/// Everything here is behavior-preserving: `toolout` (adaptive/keep-more)
/// windows verbose log/diff/grep tool results, `serialize_*` columnar-encode
/// uniform record arrays (TOON, lossless), and `cache` marks cache
/// breakpoints. Deliberately NOT set: `json_crush` (lossy — samples record
/// arrays down to a row cap), `retrieve`/`skeletonize`/`ngram` (lossy or
/// redundant with dirge's own minify), and every `output_*` control (they
/// alter the model's output, not just the input).
pub fn dirge_default_config() -> crate::llmtrim::config::DenseConfig {
    let mut c = crate::llmtrim::config::DenseConfig::lossless();
    c.toolout = true;
    c.toolout_mode = "adaptive".to_string();
    c.serialize_flatten = true;
    c.serialize_buckets = true;
    c.cache = true;
    c
}

/// Resolve a preset name to a [`DenseConfig`](crate::llmtrim::config::DenseConfig).
///
/// `"dirge"` and `"default"` return [`dirge_default_config`] — a
/// lossless-safe profile with tool-output windowing and no output-shaping
/// directives. **All other names** (`"agent"`, `"aggressive"`, `"auto"`,
/// `"safe"`, `"lossless"`, `"rag"`, `"code"`) delegate to the upstream
/// `DenseConfig::preset()`. Of those, `"safe"` and `"lossless"` are also
/// output-neutral; the rest (`agent` / `aggressive` / `auto` / `rag` /
/// `code`) enable lossy stages (retrieve, skeletonize, json_crush) AND
/// output-shaping directives that **alter the model's output behavior** —
/// they are an opt-in escape hatch for aggressive trimming, not a tuning
/// knob to casually dial.
pub fn config_for_preset(name: &str) -> crate::llmtrim::config::DenseConfig {
    let mut c = if name == "dirge" || name == "default" {
        dirge_default_config()
    } else {
        crate::llmtrim::config::DenseConfig::preset(name).unwrap_or_else(dirge_default_config)
    };
    apply_toolout_overrides(&mut c);
    c
}

/// [`config_for_preset`] plus the caching policy that depends on which backend the request
/// is bound for. Preset choice is the user's; these two are not.
pub fn config_for_provider(
    kind: crate::provider::ProviderKind,
    preset: &str,
) -> crate::llmtrim::config::DenseConfig {
    let mut c = config_for_preset(preset);
    c.cache_prompt_key = accepts_prompt_cache_key(kind);
    c.cache_auto_ttl = crate::prompt_cache::ttl()
        .wire_ttl()
        .unwrap_or_default()
        .to_string();
    c
}

/// Whether a backend accepts OpenAI's `prompt_cache_key` (dirge-07ew).
///
/// This has to be an allowlist rather than "send it and let them ignore it". An
/// OpenAI-compatible server that validates its request body strictly rejects the whole
/// request over an unknown field — Cerebras answered `body.prompt_cache_key: property
/// 'body.prompt_cache_key' is unsupported` with a 422 before it shipped caching, Groq and
/// Volcano Engine's DeepSeek answer the same way today — so a field sent hopefully is a
/// session that cannot make a single request.
///
/// The parameter also has to be worth the risk, and mostly it isn't. It is a routing hint
/// for OpenAI's prefix cache (and required for reliable matching on GPT-5.6 and later), and
/// a router uses it to keep a conversation on the endpoint holding its cache. Everywhere
/// else the caching is automatic with no key to pin: DeepSeek, GLM and Cerebras match the
/// prefix themselves and gain nothing from it, so they are left out even where they might
/// tolerate it. Ollama, OpenCode and Custom point at arbitrary endpoints whose strictness we
/// cannot know, which decides it for them.
fn accepts_prompt_cache_key(kind: crate::provider::ProviderKind) -> bool {
    use crate::provider::ProviderKind as K;
    match kind {
        // First-party OpenAI, including the ChatGPT/Codex backends.
        K::OpenAI => true,
        // Documented, and the fallback OpenRouter uses for sticky routing when no
        // `session_id` is set.
        K::OpenRouter => true,
        K::Anthropic | K::Gemini => false,
        K::DeepSeek | K::Glm | K::Cerebras | K::Kimi => false,
        K::Ollama | K::OpenCode | K::Custom => false,
    }
}

/// Rewrite a request body with an explicit config (the low-level entry point,
/// called from the HTTP interceptor).
pub fn rewrite_with(
    body: &str,
    provider: crate::llmtrim::ir::ProviderKind,
    config: &crate::llmtrim::config::DenseConfig,
) -> Result<String> {
    let result = crate::llmtrim::compress_with_config(body, Some(provider), config)
        .context("llmtrim-core compress_with_config failed")?;
    Ok(result.request_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;

    #[test]
    fn prompt_cache_key_goes_only_where_it_is_documented() {
        // dirge-07ew: an allowlist, not a denylist. A strict OpenAI-compatible endpoint
        // rejects the whole request over an unknown field, so a backend we are unsure
        // about must not receive it — and outside the OpenAI family there is nothing to
        // gain, since those providers match the prefix without a key.
        assert!(accepts_prompt_cache_key(ProviderKind::OpenAI));
        assert!(accepts_prompt_cache_key(ProviderKind::OpenRouter));
        for kind in [
            ProviderKind::Cerebras,
            ProviderKind::DeepSeek,
            ProviderKind::Glm,
            ProviderKind::Kimi,
            ProviderKind::Ollama,
            ProviderKind::OpenCode,
            ProviderKind::Custom,
        ] {
            assert!(
                !accepts_prompt_cache_key(kind),
                "{kind:?} must not be sent prompt_cache_key"
            );
        }
    }

    #[test]
    fn provider_config_carries_the_caching_policy() {
        let c = config_for_provider(ProviderKind::Cerebras, "dirge");
        assert!(c.cache, "the dirge preset still runs the cache stage");
        assert!(!c.cache_prompt_key, "but sends no key to Cerebras");
        let c = config_for_provider(ProviderKind::OpenAI, "dirge");
        assert!(c.cache_prompt_key);
        assert_eq!(
            c.cache_auto_ttl, "1h",
            "the shipped default TTL, absent config"
        );
    }

    #[test]
    fn init_from_config_disables_compression() {
        // OnceLock is set-once per process, so this test must run in
        // isolation. We can't call init_from_config (it would poison the
        // lock for other tests in the same binary), but we can assert
        // that configured_enabled() defaults to true when the lock is
        // still empty (which it is before any prod startup path runs).
        assert!(configured_enabled(), "default should be true");
        assert_eq!(
            configured_preset(),
            "dirge",
            "default preset should be 'dirge'"
        );
    }

    /// dirge-lyqb: the three-source precedence, as a pure function so it is
    /// testable without touching the process-global OnceLock.
    #[test]
    fn resolve_enabled_precedence_cli_then_env_then_config() {
        // Nothing set anywhere → on. Compression is opt-out, not opt-in.
        assert!(resolve_enabled(false, None, None), "default is on");

        // Config alone decides when neither CLI nor env speak.
        assert!(!resolve_enabled(false, None, Some(false)), "config off");
        assert!(resolve_enabled(false, None, Some(true)), "config on");

        // Env beats config in both directions.
        assert!(
            !resolve_enabled(false, Some("0"), Some(true)),
            "env disables over an enabling config"
        );
        assert!(
            resolve_enabled(false, Some("1"), Some(false)),
            "env enables over a disabling config"
        );

        // --no-compression is the user's explicit, most-local intent: it wins
        // over a stale shell export that says otherwise.
        assert!(
            !resolve_enabled(true, Some("1"), Some(true)),
            "CLI flag beats env and config"
        );
    }

    #[test]
    fn resolve_enabled_accepts_every_documented_disable_spelling() {
        for word in ["0", "off", "false", "no", "disabled", "OFF", " No "] {
            assert!(
                !resolve_enabled(false, Some(word), None),
                "{word:?} should disable compression"
            );
        }
        // Anything else is "on" — an unrecognized value must not silently
        // disable the engine.
        for word in ["1", "on", "true", "yes", "dirge", ""] {
            assert!(
                resolve_enabled(false, Some(word), None),
                "{word:?} should leave compression on"
            );
        }
    }

    #[test]
    fn smoke_openai_safe() {
        let body = r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"max_tokens":5}"#;
        let cfg = config_for_preset("safe");
        let out = rewrite_with(body, crate::llmtrim::ir::ProviderKind::OpenAi, &cfg)
            .expect("rewrite_with should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("output should be valid JSON");
        let content = parsed["messages"][0]["content"]
            .as_str()
            .expect("content should be a string");
        assert!(
            content.contains("hi"),
            "compressed content should still contain the original message text 'hi', got: {content}"
        );
    }

    #[test]
    fn byte_identity_when_nothing_fires() {
        let body = r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"temperature":0}"#;
        let cfg = crate::llmtrim::config::DenseConfig::lossless();
        // Turn off toolout so nothing changes the body.
        let mut cfg = cfg;
        cfg.toolout = false;
        let out = rewrite_with(body, crate::llmtrim::ir::ProviderKind::OpenAi, &cfg)
            .expect("rewrite_with should succeed");
        let a: serde_json::Value = serde_json::from_str(body).unwrap();
        let b: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(a, b, "lossless config should not change a trivial body");
    }

    #[test]
    fn needle_survival_toolout_compression() {
        // Build a body with a tool_call + long tool result containing a specific
        // error line, then a user question referencing it.
        let log_lines: Vec<String> = (0..80)
            .map(|i| format!("DEBUG processed item {}", i))
            .collect();
        let mut lines = log_lines.clone();
        lines.insert(42, "ERROR NullPointerException at Foo.java:147".to_string());
        let log = lines.join("\n");
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_logs", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": log},
                {"role": "user", "content": "What caused the NullPointerException?"}
            ],
            "max_tokens": 100
        })
        .to_string();
        let cfg = dirge_default_config();
        let out = rewrite_with(&body, crate::llmtrim::ir::ProviderKind::OpenAi, &cfg)
            .expect("rewrite_with should succeed");
        assert!(
            out.len() < body.len(),
            "compressed output ({}) should be smaller than input ({})",
            out.len(),
            body.len()
        );
        assert!(
            out.contains("Foo.java:147"),
            "needle should survive compression, got:\n{out}"
        );
    }

    #[test]
    fn cache_stability_preserves_cache_control_blocks() {
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "Cached preamble", "cache_control": {"type": "ephemeral"}},
                {"role": "user", "content": "What is Rust?"}
            ],
            "max_tokens": 50
        })
        .to_string();
        let cfg = dirge_default_config();
        let out = rewrite_with(&body, crate::llmtrim::ir::ProviderKind::OpenAi, &cfg)
            .expect("rewrite_with should succeed");
        assert!(
            out.contains("cache_control"),
            "cache_control block must survive compression:\n{out}"
        );
        assert!(
            out.contains("Cached preamble"),
            "cached content must survive compression:\n{out}"
        );
    }

    /// Format a file the way `agent::tools::read` does: a `(N lines total, …)` header,
    /// a blank line, then `  <lineno>: <content>` rows.
    fn as_read_output(src: &str) -> String {
        let lines: Vec<&str> = src.lines().collect();
        let total = lines.len();
        let width = total.to_string().len().max(1);
        let body: String = lines
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>width$}: {}", i + 1, l))
            .collect::<Vec<_>>()
            .join("\n");
        format!("({total} lines total, showing lines 1-{total})\n\n{body}")
    }

    /// dirge's real wire shape: Anthropic, system + first turn cached, then a fresh
    /// tool result and a fresh user message *after* the last cache breakpoint (so
    /// neither is protected by `frozen_pointers`).
    fn anthropic_body(tool_result: &str, user_text: &str) -> String {
        serde_json::json!({
            "model": "claude-opus-4-5",
            "system": [{"type": "text", "text": "You are dirge.",
                        "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "start the task",
                    "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "x.rs"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": tool_result}]},
                {"role": "user", "content": [{"type": "text", "text": user_text}]}
            ],
            "max_tokens": 100
        })
        .to_string()
    }

    fn compress_anthropic(body: &str) -> serde_json::Value {
        let cfg = dirge_default_config();
        let out = rewrite_with(body, crate::llmtrim::ir::ProviderKind::Anthropic, &cfg)
            .expect("rewrite_with should succeed");
        serde_json::from_str(&out).expect("output should be valid JSON")
    }

    /// dirge-09e8 arm 2. A source file read came back windowed to ~5% of itself —
    /// `read.rs` (1202 lines) reached the model as 45 of 938 lines — because the
    /// generic `plaintext` fallback treats code as log-shaped noise and ranks its
    /// lines with a log heuristic. It also destroys the line numbers `edit_lines`
    /// and `line_hash` anchor on.
    #[test]
    fn source_file_reads_are_never_windowed() {
        let src = include_str!("agent/tools/read.rs");
        let read_out = as_read_output(src);
        let v = compress_anthropic(&anthropic_body(&read_out, "fix the bug"));
        let got = v["messages"][2]["content"][0]["content"]
            .as_str()
            .expect("tool result should still be a string");
        assert_eq!(
            got,
            read_out,
            "a source-file read must reach the model byte-identical; got {} of {} lines",
            got.lines().count(),
            read_out.lines().count()
        );
    }

    /// dirge-09e8 arm 1. 120 pasted lines reached the model as a single
    /// `uniform vec{} u_color_{}; [×120: (3; 0..119)]` fold marker — no header, no
    /// elision marker, no signal at all that anything had been removed. The user's
    /// own message is not tool output and must never be touched.
    #[test]
    fn pasted_user_text_is_never_compressed() {
        let pasted: String = (0..120)
            .map(|i| format!("uniform vec3 u_color_{i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let user_text = format!("here is the shader I pasted:\n\n{pasted}");
        let v = compress_anthropic(&anthropic_body("(no output)", &user_text));
        let got = v["messages"][3]["content"][0]["text"]
            .as_str()
            .expect("user text should still be a string");
        assert_eq!(
            got, user_text,
            "the user's own message must reach the model byte-identical"
        );
    }

    /// The fix must not disable tool-output windowing wholesale — a genuine build log
    /// is still the thing this stage exists to compress.
    #[test]
    fn logs_are_still_windowed_after_the_fix() {
        let mut lines: Vec<String> = (0..200)
            .map(|i| format!("INFO  compiling module {i} ok"))
            .collect();
        lines.insert(120, "ERROR failed to resolve symbol foo".to_string());
        let log = lines.join("\n");
        let v = compress_anthropic(&anthropic_body(&log, "what failed?"));
        let got = v["messages"][2]["content"][0]["content"]
            .as_str()
            .expect("tool result should still be a string");
        assert!(
            got.lines().count() < log.lines().count(),
            "a real log must still be windowed, got {} of {} lines",
            got.lines().count(),
            log.lines().count()
        );
        assert!(
            got.contains("failed to resolve symbol foo"),
            "the error line must survive:\n{got}"
        );
    }

    /// dirge-09e8 arm 1. "Is this tool output?" has to be answered for every wire
    /// shape, not just Anthropic's. OpenAI Chat Completions gives a tool result a turn
    /// of its own (`role: "tool"`) with no block type, so a block-type-only test reads
    /// it as user text and switches the whole stage off for OpenAI, DeepSeek, GLM,
    /// Cerebras, Kimi, Ollama and OpenRouter at once.
    #[test]
    fn openai_shaped_tool_results_are_still_recognized_as_tool_output() {
        let mut lines: Vec<String> = (0..200)
            .map(|i| format!("INFO  compiling module {i} ok"))
            .collect();
        lines.insert(120, "ERROR failed to resolve symbol foo".to_string());
        let log = lines.join("\n");
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are a coding agent."},
                {"role": "assistant", "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": log},
                {"role": "user", "content": "what failed?"}
            ],
            "max_tokens": 100
        })
        .to_string();
        let cfg = dirge_default_config();
        let out = rewrite_with(&body, crate::llmtrim::ir::ProviderKind::OpenAi, &cfg)
            .expect("rewrite_with should succeed");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let got = v["messages"][2]["content"]
            .as_str()
            .expect("still a string");
        assert!(
            got.lines().count() < log.lines().count(),
            "an OpenAI-shaped tool result must still be windowed, got {} of {} lines",
            got.lines().count(),
            log.lines().count()
        );
    }

    /// dirge-09e8 arm 4. `read(verbatim=true)` marks its result exempt, so even a
    /// log-shaped result ships whole when the model explicitly asked for it.
    #[test]
    fn verbatim_marked_output_is_passed_through() {
        let mut lines: Vec<String> = (0..200)
            .map(|i| format!("INFO  compiling module {i} ok"))
            .collect();
        lines.insert(120, "ERROR failed to resolve symbol foo".to_string());
        let marked = format!(
            "{}\n{}",
            crate::agent::tools::VERBATIM_MARKER,
            lines.join("\n")
        );
        let v = compress_anthropic(&anthropic_body(&marked, "what failed?"));
        let got = v["messages"][2]["content"][0]["content"]
            .as_str()
            .expect("tool result should still be a string");
        assert_eq!(
            got, marked,
            "a verbatim-marked result must reach the model byte-identical"
        );
    }
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::session::storage;

#[cfg(feature = "mcp")]
use crate::extras::mcp::config::McpServerConfig;

#[cfg(feature = "acp")]
use crate::extras::acp::config::AcpServerConfig;

/// Unified provider declaration. One entry per alias in
/// `config.providers`. The map KEY is the alias the rest of the
/// config (and `provider`, `review_provider`, etc.) refers to.
///
/// `provider_type` is optional: when the alias matches a built-in
/// (anthropic, deepseek, glm, openai, openrouter, gemini, ollama),
/// it's inferred from the key. Set it explicitly only when aliasing
/// a built-in backend under a different name — e.g.
/// `"ollama": { "provider_type": "openai", "base_url": "..." }`
/// aliases the OpenAI-compatible backend under the alias `ollama`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuth {
    #[serde(alias = "api-key")]
    ApiKey,
    #[serde(
        alias = "chatgpt",
        alias = "chat-gpt",
        alias = "chatgpt_auth_tokens",
        alias = "codex"
    )]
    ChatGpt,
    #[serde(alias = "claude-code", alias = "claude_code", alias = "claude")]
    Anthropic,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderEntry {
    pub provider_type: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Authentication source for this provider. Default is API-key
    /// auth. Set to `chatgpt` to reuse Codex ChatGPT-login tokens
    /// (`CODEX_ACCESS_TOKEN` or `CODEX_HOME/auth.json`).
    pub auth: Option<ProviderAuth>,
    /// Name of the env var holding the API key. Kept for backward
    /// compatibility — prefer `api_key` with `${VAR}` interpolation
    /// for clarity.
    pub api_key_env: Option<String>,
    /// API key for this provider. Accepts a literal key OR shell-style
    /// `${ENV_VAR}` interpolation (expanded at use time). Takes
    /// precedence over `api_key_env`. Accepts both `api_key` and
    /// `apiKey` in the JSON.
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
    /// Set to true to allow `http://` URLs (insecure). Default false —
    /// only `https://` is accepted. Non-https endpoints send every
    /// prompt, file content, and tool result in plaintext over the
    /// network. Only enable for local-only proxies (ollama, vllm, etc.)
    /// that are NOT reachable from other hosts.
    pub allow_insecure: bool,
    /// Per-provider override for the streaming chunk timeout. Same
    /// units / semantics as the top-level `stream_chunk_timeout_secs`
    /// but takes precedence for this specific provider.
    pub stream_chunk_timeout_secs: Option<u64>,
    /// Per-provider model options. Free-form map; known keys are
    /// honored by the request builder, unknown keys are ignored.
    /// Currently honored: `temperature` (f64, overrides cfg/CLI for
    /// requests routed through this provider).
    pub options: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ProviderEntry {
    /// Resolve the API key declared on this entry, expanding
    /// `${VAR}` interpolation against the process environment.
    /// Returns:
    /// - `Some(Ok(key))` when a literal or successfully-expanded key is available
    /// - `Some(Err(missing_var))` when `${VAR}` is configured but the env var is unset
    /// - `None` when no `api_key` is configured on the entry
    pub fn resolved_api_key(&self) -> Option<Result<String, String>> {
        let raw = self.api_key.as_deref()?;
        if let Some(name) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => Some(Ok(v)),
                _ => Some(Err(name.to_string())),
            }
        } else {
            Some(Ok(raw.to_string()))
        }
    }

    /// `options.temperature` as an f64 when set. Other shapes (string,
    /// integer, missing) return `None`.
    pub fn options_temperature(&self) -> Option<f64> {
        self.options.as_ref()?.get("temperature")?.as_f64()
    }
}

/// Logical role a provider can be assigned to. Used by
/// `Config::resolve_role` to look up the named provider for that
/// role (and fall back to the default for non-default roles).
///
/// `Review`, `Escalation`, `Summarization`, and `Subagent` are
/// declared for the unified role-routing surface; the
/// corresponding call-sites (background review, Phase 4
/// escalation, compaction summarizer, `task` subagent) wire up in
/// follow-up commits. They're tested today via the role
/// resolver but not yet referenced from a runtime path, so
/// `#[allow(dead_code)]` keeps the warning quiet for the
/// config-only PR.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum ConfigRole {
    Default,
    Review,
    Escalation,
    Summarization,
    Subagent,
    Critic,
    Approval,
}

/// One VSCode-style key binding: bind a key chord (or chord sequence like
/// `"ctrl-x ctrl-s"`) to a named command. `key` is a chord like `"ctrl-t"`
/// / `"pageup"` / `"ctrl-shift-x"`; `command` is one of the rebindable
/// global commands (`ui::keymap::KeyAction`) or input-editor commands
/// (`ui::keymap::InputAction`), or `"none"` to unbind the default on that
/// chord. Parsed by `ui::keymap::Keymaps::from_config`.
#[derive(Debug, Clone, Deserialize)]
pub struct KeybindingConfig {
    pub key: String,
    pub command: String,
}

/// Long-term memory retrieval tuning (dirge-4hld). Absent/default = the
/// builtin BM25 store, unchanged. `hybrid_retrieval` opts into dense+BM25
/// fusion, which additionally needs an embeddings backend (`embed_url`, plus
/// `embed_api_key_env` for hosted ones); if the backend isn't configured the
/// store silently stays BM25-only.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Turn on hybrid (dense + BM25) memory search. Default off.
    pub hybrid_retrieval: Option<bool>,
    /// OpenAI-compatible `/v1/embeddings` endpoint URL.
    pub embed_url: Option<String>,
    /// Embedding model id; defaults to `memory_hybrid::DEFAULT_EMBED_MODEL`.
    pub embed_model: Option<String>,
    /// Env var holding the embeddings API key. Omit for a keyless local
    /// endpoint. (The key itself is never stored in config.)
    pub embed_api_key_env: Option<String>,
    /// dirge-0gxb: each turn, auto-search memory on the verbatim user message
    /// and inject the hits as a supplemental context block (never the frozen
    /// snapshot). Surfaces relevant memory the agent wouldn't think to look
    /// up. Default off.
    pub verbatim_pre_recall: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub websearch: Option<bool>,
    pub webfetch: Option<bool>,
    /// Phase 3 / part 2: inline output budget for the `bash`
    /// tool. Output at-or-below this size (AND ≤200 lines) is
    /// returned verbatim; anything above is written to
    /// `~/.dirge/transient/<pid>/bash-<unix_ts>.txt` and a head/
    /// tail summary is returned to the model along with a hint
    /// telling it to use the `read` tool to inspect specific
    /// portions. Default 8 KiB. Set to a huge number to disable
    /// the relay; set lower to keep more turns inline-summarized.
    pub bash_output_inline_max_bytes: Option<usize>,
    /// As above but for the `webfetch` tool. Default 8 KiB. The
    /// 10 MiB streaming body cap inside `webfetch` itself is
    /// independent and stays as the in-memory ceiling.
    pub webfetch_output_inline_max_bytes: Option<usize>,
    /// dirge-nmv5: inline output budget for the `task` subagent
    /// tool. Subagent answers larger than this are relayed to
    /// `~/.dirge/transient/<pid>/task-<unix_ts>.txt` and the parent
    /// agent receives a head/tail summary + a `read`-tool hint to
    /// fetch the full payload. Default 8 KiB. Replaces the legacy
    /// 3000-char hard truncation that silently dropped the tail of
    /// large subagent answers.
    pub task_output_inline_max_bytes: Option<usize>,
}

/// Override block for the named per-operation timeouts, under the
/// config `timeouts` object. Every field is in seconds; unset fields
/// fall back to [`crate::timeout::Timeouts::DEFAULT`]. Mirrors the field
/// set of [`crate::timeout::Timeouts`] (dirge-onlr / dirge-4xgd) so the
/// previously scattered magic-number timeouts have one configurable home.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct TimeoutsConfig {
    pub stream_chunk_secs: Option<u64>,
    pub request_establish_secs: Option<u64>,
    pub tool_call_gap_secs: Option<u64>,
    pub mcp_call_secs: Option<u64>,
    pub mcp_init_secs: Option<u64>,
    pub lsp_request_secs: Option<u64>,
    pub lsp_initialize_secs: Option<u64>,
    pub bash_secs: Option<u64>,
}

/// Per-server LSP configuration. All fields optional — unspecified fields
/// fall back to the built-in defaults for the given `server_id`.
///
/// Two forms are accepted:
/// - `{ "disabled": true }` to turn off a built-in server entirely.
/// - any subset of `{ command, extensions, env, initialization, disabled }`
///   to override pieces of the default.
#[cfg(feature = "lsp")]
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct LspServerConfig {
    pub command: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    /// Extensions to ADD to the server's built-in list (additive — does
    /// not replace). e.g. `"extend_extensions": ["janet"]` on
    /// `clojure-lsp` keeps clj/cljs/… and also routes `.janet` files to
    /// it. Accepts `extendExtensions` too.
    #[serde(alias = "extendExtensions")]
    pub extend_extensions: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub initialization: Option<serde_json::Value>,
    pub disabled: Option<bool>,
}

#[cfg(feature = "lsp")]
impl crate::lsp::server::AsExtensionOverride for LspServerConfig {
    fn extensions(&self) -> Option<&[String]> {
        self.extensions.as_deref()
    }
    fn extend_extensions(&self) -> Option<&[String]> {
        self.extend_extensions.as_deref()
    }
    fn disabled(&self) -> bool {
        self.disabled.unwrap_or(false)
    }
}

/// Per-plugin settings under the config `plugins` object, keyed by plugin
/// name (the directory name or the `.janet` file stem under a plugin search
/// dir). Both fields default to "unset"; the host treats that as
/// enabled + not auto-started, so existing setups load every plugin as
/// before.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    /// Load this plugin? Default true. `false` skips loading it entirely.
    pub enabled: Option<bool>,
    /// Passed to the plugin (via `harness/plugin-config`) so it can
    /// self-engage at startup instead of waiting for a trigger. Plugin-
    /// specific: e.g. `backpressured` engages its loop when this is true.
    pub auto_start: Option<bool>,
}

/// `lsp = true`  → enable built-in servers with default commands.
/// `lsp = false` → disable LSP entirely.
/// `lsp = { server-id = { … } }` → enable defaults, overriding the named
///   servers with the provided config.
#[cfg(feature = "lsp")]
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LspConfig {
    Enabled(bool),
    Servers(HashMap<String, LspServerConfig>),
}

#[cfg(feature = "lsp")]
impl LspConfig {
    /// `true` when LSP should be on. Defaults to enabled.
    pub fn is_enabled(&self) -> bool {
        match self {
            LspConfig::Enabled(b) => *b,
            LspConfig::Servers(_) => true,
        }
    }

    /// Per-server overrides keyed by server id. Empty when LSP is a bool.
    pub fn server_overrides(&self) -> &HashMap<String, LspServerConfig> {
        match self {
            LspConfig::Enabled(_) => {
                // Empty borrow without allocating per-call.
                static EMPTY: std::sync::OnceLock<HashMap<String, LspServerConfig>> =
                    std::sync::OnceLock::new();
                EMPTY.get_or_init(HashMap::new)
            }
            LspConfig::Servers(map) => map,
        }
    }
}

/// Sandbox mode from config.json. Accepts:
/// - `true` / `false` (bool)
/// - `"off"` / `"bwrap"` / `"microvm"` (string)
/// - `{"mode": "microvm", "image": "...", "cpus": 2, "memory_mib": 1024}` (object)
///
/// Backward compatibility: the old form `{"mode": "microvm", "microvm": {"image": "..."}}`
/// is still accepted transparently.
///
/// TODO(sandbox-net): network filtering
///   The microVM gets full outbound network via TSI (Transparent Socket
///   Impersonation). There is currently NO domain/IP allowlisting —
///   any process in the guest can reach any host on the internet.
///   The plan is to add a host-side SNI proxy that intercepts all
///   guest TCP port 443 traffic, checks the TLS Server Name Indication
///   against a configurable `domains_allowlist`, and drops non-matching
///   connections. Port 80 HTTP would be blocked entirely (force HTTPS).
///   The proxy would run as a lightweight sidecar spawned by the runner
///   and connected via `krun_add_net_unixstream`. Until that's done,
///   the VM has unrestricted outbound network access.
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    pub mode: Option<String>,
    pub image: Option<String>,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
}

impl SandboxConfig {
    pub fn to_mode(&self) -> crate::sandbox::SandboxMode {
        #[cfg(feature = "sandbox-microvm")]
        {
            match self.mode.as_deref() {
                Some("microvm") => crate::sandbox::SandboxMode::Microvm,
                Some("off") => crate::sandbox::SandboxMode::Off,
                _ => crate::sandbox::SandboxMode::Bwrap,
            }
        }
        #[cfg(not(feature = "sandbox-microvm"))]
        {
            match self.mode.as_deref() {
                Some("microvm") => {
                    eprintln!(
                        "warning: sandbox=microvm in config but dirge was built without the sandbox-microvm feature. Using bwrap instead."
                    );
                    crate::sandbox::SandboxMode::Bwrap
                }
                Some("off") => crate::sandbox::SandboxMode::Off,
                _ => crate::sandbox::SandboxMode::Bwrap,
            }
        }
    }
}

// ── deserialization glue: accept old flat forms too ──────────────

/// Convert a JSON value to a bounded integer, erroring (not wrapping)
/// when it isn't a non-negative integer in range for `T`. dirge-mt91:
/// the legacy nested-microvm path used `as u8`/`as u32` casts that
/// silently wrapped.
fn checked_u64<T, E>(v: &serde_json::Value, field: &str) -> Result<T, E>
where
    T: TryFrom<u64>,
    E: serde::de::Error,
{
    let n = v
        .as_u64()
        .ok_or_else(|| E::custom(format!("microvm.{field} must be a non-negative integer")))?;
    T::try_from(n).map_err(|_| E::custom(format!("microvm.{field} value {n} out of range")))
}

impl<'de> Deserialize<'de> for SandboxConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;

        struct SandboxConfigVisitor;

        impl<'de> de::Visitor<'de> for SandboxConfigVisitor {
            type Value = SandboxConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "a sandbox mode string, bool, or {mode, image, cpus, memory_mib} object",
                )
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(SandboxConfig {
                    mode: Some(if v { "bwrap" } else { "off" }.to_string()),
                    ..Default::default()
                })
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(SandboxConfig {
                    mode: Some(v.to_string()),
                    ..Default::default()
                })
            }

            fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut mode: Option<String> = None;
                let mut image: Option<String> = None;
                let mut cpus: Option<u8> = None;
                let mut memory_mib: Option<u32> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "mode" => mode = Some(map.next_value()?),
                        "image" => image = Some(map.next_value()?),
                        "cpus" => cpus = Some(map.next_value()?),
                        "memory_mib" => memory_mib = Some(map.next_value()?),
                        // Accept old nested microvm: {image, cpus, memory_mib}.
                        // dirge-mt91: bound the integer casts — `as u8`/`as u32`
                        // silently wrapped (256 CPUs → 0). Out-of-range errors.
                        "microvm" => {
                            let sub: serde_json::Value = map.next_value()?;
                            if let Some(obj) = sub.as_object() {
                                if image.is_none() {
                                    image =
                                        obj.get("image").and_then(|v| v.as_str().map(String::from));
                                }
                                if cpus.is_none()
                                    && let Some(v) = obj.get("cpus")
                                {
                                    cpus = Some(checked_u64::<u8, M::Error>(v, "cpus")?);
                                }
                                if memory_mib.is_none()
                                    && let Some(v) = obj.get("memory_mib")
                                {
                                    memory_mib =
                                        Some(checked_u64::<u32, M::Error>(v, "memory_mib")?);
                                }
                            }
                        }
                        _ => {
                            let _: de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(SandboxConfig {
                    mode,
                    image,
                    cpus,
                    memory_mib,
                })
            }
        }

        deserializer.deserialize_any(SandboxConfigVisitor)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: Option<String>,
    /// Default authentication source for providers that do not set
    /// `providers.<name>.auth`. `ApiKey` remains the implicit default.
    pub auth: Option<ProviderAuth>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub no_tools: Option<bool>,
    pub no_context_files: Option<bool>,
    pub context_window: Option<u64>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub max_agent_turns: Option<usize>,
    pub compact_enabled: Option<bool>,
    /// Unified provider map. Keyed by alias; the alias is what
    /// `provider` / `review_provider` / `escalation_provider` /
    /// `summarization_provider` / `subagent_provider` reference.
    /// Each entry's `provider_type` defaults to the alias key
    /// when omitted.
    pub providers: Option<HashMap<String, ProviderEntry>>,
    /// User-defined agent profiles (dirge-ykeu), keyed by name. Each is a
    /// `{ prompt, model, allow_tools/deny_tools, reasoning, temperature }`
    /// bundle. Lowest-precedence source — `.dirge/agents/*.md` and
    /// `~/.config/dirge/agents/*.md` files override same-named entries here.
    /// Absent = no profiles (fully opt-in; today's behavior unchanged).
    pub agents: Option<HashMap<String, crate::context::agent_defs::AgentConfig>>,
    /// Per-plugin settings, keyed by plugin name (the directory name or
    /// the `.janet` file stem under a plugin search dir). Absent entry =
    /// enabled, not auto-started (backward compatible).
    pub plugins: Option<HashMap<String, PluginSettings>>,
    pub permission: Option<serde_json::Value>,
    pub restrictive: Option<bool>,
    pub accept_all: Option<bool>,
    pub yolo: Option<bool>,
    pub sandbox: Option<SandboxConfig>,
    /// OCI image for microVM sandbox (e.g. "local://dirge-microvm:alpine",
    /// "docker.io/library/debian:stable-slim"). **Deprecated:** prefer the
    /// nested `sandbox.microvm.image` key for new configs. This top-level
    /// key still works as a fallback.
    pub microvm_image: Option<String>,
    pub default_permission_mode: Option<String>,
    pub show_tool_details: Option<bool>,
    pub show_edit_diff: Option<bool>,
    /// Make the model's thinking/reasoning burst visible by default,
    /// without having to press Ctrl+O each turn (GH #461). Absent or
    /// `false` keeps today's behavior (reasoning hidden until toggled).
    pub show_reasoning: Option<bool>,
    /// Preferred default pane layout for the TUI: a `|`/`,`/space-
    /// separated subset of `left`, `main`, `right` (e.g.
    /// `"left|main|right"`, `"main"`, `"main|right"`). The main pane is
    /// always shown; this picks which side panels appear at startup. The
    /// `/display` command overrides it at runtime. Absent → both side
    /// panels follow the automatic width-based behavior.
    pub display: Option<String>,
    pub tool_result_max_chars: Option<usize>,
    /// Cap on tool-result body lines shown by default inside a tool
    /// chamber. Anything past this collapses to a
    /// `↓ N more lines (Ctrl+O to expand)` footer, and the user can
    /// re-print the most recent collapsed result in full via Ctrl+O.
    /// `tool_result_max_chars` still applies on top as a hard
    /// character ceiling for the displayed slice.
    pub tool_result_max_lines: Option<usize>,
    /// Per-chunk read deadline for streaming LLM responses, in seconds.
    /// Default 300s (5 min). Bump higher (600–900) if you use models
    /// with very long reasoning budgets (Claude 3.7 extended thinking,
    /// GPT-5 thinking, etc.) and see false-positive "stream chunk timed
    /// out" errors mid-turn. Set lower if you want faster failure
    /// detection on flaky networks; below ~60s is risky on reasoning
    /// models.
    pub stream_chunk_timeout_secs: Option<u64>,
    pub default_prompt: Option<String>,
    /// Optional provider to use for background review at session end.
    /// When not set, the review fork reuses the main session's provider.
    pub review_provider: Option<String>,
    /// Optional provider for escalation (Phase 4 future hook).
    pub escalation_provider: Option<String>,
    /// Optional provider for context summarization / compaction.
    pub summarization_provider: Option<String>,
    /// Early-fold threshold as a fraction of the model's context window
    /// (e.g. `0.5`). Lowers the point at which history folds into a
    /// summary — and thus when the durable session checkpoint is written
    /// — so it captures earlier, from more coherent context (MiMo's
    /// "compress before the window fills" insight). Clamped to
    /// `0.3..=0.75`; out-of-range or unset keeps the `0.75` default.
    /// Installed process-wide at startup.
    pub compaction_fold_threshold: Option<f64>,
    /// Working-context budget in tokens (default: 250_000). The compaction
    /// decision treats the effective window as `min(model_window, this)`, so
    /// models below 250k use their own window while larger models are capped.
    /// Set lower (e.g. 100_000) for stricter folding on cost-sensitive
    /// routes. Floored at 16k; a value above the model's real window is a
    /// no-op (the window wins). Installed process-wide at startup.
    pub context_target: Option<u64>,
    /// Incremental background checkpoint (MiMo-style): refresh the durable
    /// session checkpoint at 20%-interval usage thresholds, in the
    /// background, without folding the live context — so a resume after a
    /// crash/quit recovers a fresh state. Default ON; set `false` to
    /// disable (skips the background summary calls). Installed process-wide
    /// at startup.
    pub incremental_checkpoint: Option<bool>,
    /// Optional provider for sub-agents (`task` tool).
    pub subagent_provider: Option<String>,
    /// Optional provider for the F6 in-loop critic (tier 3). When set,
    /// the verifier escalates to a bounded LLM critique at finalization
    /// on substantive runs. Unset (default) = no critic, no cost.
    pub critic_provider: Option<String>,
    /// Optional system-preamble override for the in-loop critic. When set,
    /// replaces the built-in `CRITIC_PREAMBLE` for every prompt unless a
    /// prompt's own `critic_preamble` frontmatter overrides it. Unset
    /// (default) = built-in critic stance. See `resolve_critic_preamble`.
    pub critic_preamble: Option<String>,
    /// How the diff-aware code reviewer engages at finalization. One of
    /// `off` / `advisory` / `blocking` (case-insensitive, trimmed).
    /// `advisory` *(default)* runs the review detached in the background
    /// and surfaces findings as a non-blocking notice — it never holds up
    /// a turn. `blocking` awaits the review and re-enters the loop on
    /// high/critical findings (the legacy behaviour). `off` disables it
    /// entirely. A prompt's `code_review` front-matter overrides this
    /// per-prompt; only meaningful when a `critic_provider` is set. See
    /// [`resolve_code_review_mode`](Self::resolve_code_review_mode).
    pub code_review: Option<String>,
    /// dirge-0g6i: optional provider for LLM auto-approval. When set, a
    /// permission prompt is routed to this model (with a safety prompt)
    /// which replies ALLOW/DENY instead of asking the human. Unset
    /// (default) = human prompts as usual. See docs/permissions.md.
    pub approval_provider: Option<String>,
    /// UI color theme. Known built-in values: `phosphor` (default,
    /// 80s CRT green) and `plain` (white/cyan).
    ///
    /// Any other value looks for a custom theme file at
    /// `~/.config/dirge/<theme>.theme.json` — see the
    /// `ui::theme` module for the JSON format. Fields not in the
    /// file inherit from the phosphor preset so minimal overrides
    /// work (e.g. just `{"accent": "magenta"}`).
    ///
    /// If neither the built-in name nor the file matches, dirge
    /// falls back to phosphor with a warning rather than refusing
    /// to start.
    pub theme: Option<String>,
    /// VSCode-style key-binding overrides for the global command keys.
    /// Each entry binds a chord to a command (see `KeybindingConfig`);
    /// applied over the built-in defaults by `ui::keymap`.
    pub keybindings: Option<Vec<KeybindingConfig>>,
    /// dirge-5kkx.1: auto-cancel an in-progress emacs-style chord sequence
    /// (e.g. after `ctrl-x` of `ctrl-x ctrl-s`) when no continuing key
    /// arrives within this many milliseconds. Absent = wait indefinitely
    /// (emacs default); Esc/Ctrl+G always cancels regardless.
    pub chord_timeout_ms: Option<u64>,
    /// Cross-session Up-arrow history: how many of the most-recent prior
    /// sessions in the same project (same working directory) to mine for
    /// command history. Their user prompts are seeded into history ahead
    /// of the current session's own, so pressing Up in a fresh session
    /// recalls commands typed in earlier conversations. `None` (default)
    /// → 3. Set 0 to keep history scoped to the current session only.
    pub max_sessions: Option<usize>,
    /// User-defined aliases for built-in slash commands: `{alias: command}`.
    /// `{"exit": "quit"}` makes `/exit` run `/quit`. A leading `/` on either
    /// side is optional. Targets that aren't known built-ins warn at startup
    /// (likely typos); plugin-command targets pass through unvalidated.
    /// Aliases are expanded before dispatch (`ui::slash::aliases`) and are
    /// NOT built-ins — they don't appear in `slash_command_names()`.
    pub slash_aliases: Option<HashMap<String, String>>,
    pub tools: Option<ToolsConfig>,
    /// dirge-4hld: long-term memory retrieval tuning (hybrid dense+BM25).
    pub memory: Option<MemoryConfig>,

    /// Phase-3 (`docs/AGENTIC_LOOP_PLAN.md`): when true, ship only
    /// `tool_search` + a small always-on set in the per-turn tool
    /// defs, and let the model discover the rest via
    /// `tool_search(query)`. Default `false` — preserves the
    /// "ship every tool every turn" path. Useful on long sessions
    /// with MCP-heavy toolsets (≈30% token savings).
    pub dynamic_tool_search: Option<bool>,

    /// Phase 4 part 2 (`docs/AGENTIC_LOOP_PLAN.md`): consecutive-turn
    /// threshold for the context-depth reminder system. `None`
    /// (default) keeps the feature OFF — long sessions get no
    /// reminders. Recommended value: 8. Set lower for tighter
    /// re-focusing; higher to silence the reminder for routine
    /// multi-step refactors.
    pub context_depth_reminder_threshold: Option<usize>,
    /// Phase 3 (`dirge-phyi`, vix port): opt-in phased plan workflow —
    /// explore → plan → reviewer-runs-code loop, each phase a fresh
    /// context-reset fork. `None`/`false` (default) keeps the normal
    /// single-agent path. The orchestration core lives in
    /// `crate::agent::plan::workflow`; the runtime drain in
    /// `crate::agent::plan::runtime`.
    pub phased_workflow_enabled: Option<bool>,
    /// Max reviewer-runs-code fix cycles before the phased workflow gives
    /// up with `Exhausted`. `None` defaults to 2 (vix's default). Only
    /// consulted when `phased_workflow_enabled` is on.
    pub phased_workflow_max_review_cycles: Option<usize>,
    /// dirge-onlr / dirge-4xgd: per-operation timeout overrides. Unset
    /// fields fall back to `crate::timeout::Timeouts::DEFAULT`. Merged in
    /// `resolve_timeouts()` and installed process-wide at startup.
    pub timeouts: Option<TimeoutsConfig>,
    #[cfg(feature = "lsp")]
    pub lsp: Option<LspConfig>,
    #[cfg(feature = "mcp")]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,

    /// ACP server config map when compiled with the `acp` feature.
    /// Used by the editor-integration server; dirge's ACP transport
    /// is stdio-only — the TCP / Unix-socket forms live here for
    /// future expansion but are not honored today.
    #[cfg(feature = "acp")]
    pub acp_servers: Option<HashMap<String, AcpServerConfig>>,
}

impl Config {
    /// Snapshot of the unified providers map. Empty when not set.
    pub fn providers_map(&self) -> HashMap<String, ProviderEntry> {
        self.providers.clone().unwrap_or_default()
    }

    /// Whether the plugin named `name` should be loaded. Default true —
    /// only an explicit `"enabled": false` skips it.
    // Consumed only by the plugin loader (main.rs, `cfg(feature = "plugin")`)
    // and the config tests; dead in a no-plugin build (e.g. the Windows
    // `windows-default` set), where `-D warnings` would otherwise fail.
    #[allow(dead_code)]
    pub fn plugin_enabled(&self, name: &str) -> bool {
        self.plugins
            .as_ref()
            .and_then(|m| m.get(name))
            .and_then(|s| s.enabled)
            .unwrap_or(true)
    }

    /// Whether the plugin named `name` requested auto-start. Default false.
    #[allow(dead_code)] // plugin-only consumer; see `plugin_enabled`.
    pub fn plugin_auto_start(&self, name: &str) -> bool {
        self.plugins
            .as_ref()
            .and_then(|m| m.get(name))
            .and_then(|s| s.auto_start)
            .unwrap_or(false)
    }

    /// Phase 4 part 2: resolve the context-depth reminder
    /// threshold. Trivially returns the field — encapsulated as a
    /// method so future callers don't see the `Option` directly
    /// and so we can add validation (e.g. clamp to >= 1) without
    /// changing every consumer.
    pub fn resolve_context_depth_threshold(&self) -> Option<usize> {
        // Clamp to a minimum of 2: a threshold of 0 or 1 would
        // emit a reminder on the very first tool call, which
        // defeats the purpose.
        self.context_depth_reminder_threshold.map(|t| t.max(2))
    }

    /// Resolve a logical role to `(alias, entry)`. For non-default
    /// roles, falls back to `self.provider` when no role-specific
    /// assignment is configured. Returns `None` only when neither
    /// the role nor the default provider names a present entry,
    /// AND the alias doesn't match a built-in.
    pub fn resolve_role(&self, role: ConfigRole) -> Option<(String, ProviderEntry)> {
        let providers = self.providers.as_ref();
        let role_name: Option<&str> = match role {
            ConfigRole::Default => self.provider.as_deref(),
            ConfigRole::Review => self.review_provider.as_deref().or(self.provider.as_deref()),
            ConfigRole::Escalation => self
                .escalation_provider
                .as_deref()
                .or(self.provider.as_deref()),
            ConfigRole::Summarization => self
                .summarization_provider
                .as_deref()
                .or(self.provider.as_deref()),
            ConfigRole::Subagent => self
                .subagent_provider
                .as_deref()
                .or(self.provider.as_deref()),
            // No fallback to the default provider: the critic is opt-in,
            // so it resolves only when `critic_provider` is explicitly set.
            ConfigRole::Critic => self.critic_provider.as_deref(),
            // Likewise opt-in: auto-approval resolves only when
            // `approval_provider` is explicitly set (no default fallback).
            ConfigRole::Approval => self.approval_provider.as_deref(),
        };
        let alias = role_name?.to_string();
        if let Some(map) = providers
            && let Some(entry) = map
                .get(&alias)
                .or_else(|| map.get(&alias.to_ascii_lowercase()))
        {
            return Some((alias, entry.clone()));
        }
        // Alias names a built-in but no explicit entry: synthesize a
        // default entry so callers don't have to special-case.
        if crate::provider::parse_provider(&alias).is_some() {
            return Some((alias, ProviderEntry::default()));
        }
        None
    }

    /// Resolve the critic's system preamble: the config override when set,
    /// else the built-in `CRITIC_PREAMBLE`. A prompt may further override
    /// this via frontmatter (applied in `build_agent`).
    pub fn resolve_critic_preamble(&self) -> &str {
        self.critic_preamble
            .as_deref()
            .unwrap_or(crate::agent::agent_loop::critic::CRITIC_PREAMBLE)
    }

    /// Resolve the diff-aware reviewer's engagement mode from
    /// [`code_review`](Self::code_review): `off`/`advisory`/`blocking`,
    /// parsed case-insensitively and trimmed. `None` and an empty value
    /// resolve to the default `Advisory`. An unrecognized non-empty value
    /// also resolves to `Advisory` but logs a warning, so a typo never
    /// silently disables the reviewer.
    pub fn resolve_code_review_mode(&self) -> crate::agent::agent_loop::types::CodeReviewMode {
        use crate::agent::agent_loop::types::CodeReviewMode;
        let Some(raw) = self.code_review.as_deref() else {
            return CodeReviewMode::default();
        };
        CodeReviewMode::from_wire(raw).unwrap_or_else(|| {
            tracing::warn!(
                target: "dirge::config",
                value = raw.trim(),
                "unrecognized `code_review` value; falling back to `advisory` \
                 (valid: off | advisory | blocking)"
            );
            CodeReviewMode::Advisory
        })
    }

    /// Resolve the provider_type for an entry — the entry's
    /// explicit value when set, otherwise the alias (lowercased)
    /// which must match a built-in.
    pub fn provider_type_of(name: &str, entry: &ProviderEntry) -> String {
        entry
            .provider_type
            .clone()
            .unwrap_or_else(|| name.to_ascii_lowercase())
    }

    /// Resolve the context window for the active model. Precedence:
    ///   1. explicit `context_window` in config.json
    ///   2. per-model static table (`context_window_for_model`)
    ///   3. 128_000 fallback
    ///
    /// `model` is the resolved model id (after CLI / config / default
    /// resolution). Passing an empty string falls through to (3).
    pub fn resolve_context_window(&self, model: &str) -> u64 {
        if let Some(v) = self.context_window {
            return v;
        }
        context_window_for_model(model).unwrap_or(128_000)
    }

    pub fn resolve_reserve_tokens(&self) -> u64 {
        self.reserve_tokens.unwrap_or(16_384)
    }

    pub fn resolve_keep_recent_tokens(&self) -> u64 {
        self.keep_recent_tokens.unwrap_or(20_000)
    }

    pub fn resolve_compact_enabled(&self) -> bool {
        self.compact_enabled.unwrap_or(true)
    }

    /// Phase-3: dynamic-tool-search opt-in. Default off.
    pub fn resolve_dynamic_tool_search(&self) -> bool {
        self.dynamic_tool_search.unwrap_or(false)
    }

    /// Phased plan workflow opt-in (vix port). Default off — `/plan` is gated
    /// on this as a master kill-switch.
    pub fn resolve_phased_workflow_enabled(&self) -> bool {
        self.phased_workflow_enabled.unwrap_or(false)
    }

    /// Reviewer-runs-code fix-cycle budget for the phased workflow.
    /// Default 2 (vix's default).
    pub fn resolve_phased_workflow_max_review_cycles(&self) -> usize {
        self.phased_workflow_max_review_cycles.unwrap_or(2)
    }

    pub fn resolve_tool_result_max_chars(&self) -> usize {
        self.tool_result_max_chars.unwrap_or(500)
    }

    pub fn resolve_tool_result_max_lines(&self) -> usize {
        self.tool_result_max_lines.unwrap_or(4)
    }

    /// Resolve the chunk timeout for the active provider.
    ///
    /// Precedence:
    ///   1. `providers[name].stream_chunk_timeout_secs`
    ///   2. top-level `stream_chunk_timeout_secs`
    ///   3. `[timeouts].stream_chunk_secs` → `Timeouts::DEFAULT` (300s)
    ///
    /// Passing an unknown / empty provider name falls through past
    /// (1) to the top-level / default.
    pub fn resolve_stream_chunk_timeout(&self, provider: &str) -> std::time::Duration {
        // Provider lookup is case-insensitive because `parse_provider`
        // accepts `--provider Anthropic` (#2 fix). Without this, a
        // capitalized CLI / config provider name built the client
        // fine but missed the `providers.anthropic` override silently.
        let lower = provider.to_ascii_lowercase();
        let from_provider = self
            .providers
            .as_ref()
            .and_then(|m| m.get(provider).or_else(|| m.get(&lower)))
            .and_then(|p| p.stream_chunk_timeout_secs);
        // Provider override and the top-level key still win; otherwise
        // fall through to the centralized default.
        match from_provider.or(self.stream_chunk_timeout_secs) {
            Some(secs) => std::time::Duration::from_secs(secs),
            None => self.resolve_timeouts().stream_chunk,
        }
    }

    /// Resolve the named per-operation timeouts (dirge-onlr / dirge-4xgd):
    /// each field is its `[timeouts]` override when set, else the built-in
    /// default ([`crate::timeout::Timeouts::DEFAULT`]). Installed
    /// process-wide at startup via `Timeouts::init`, so all consumers read
    /// the same resolved values through `Timeouts::get()` — the single
    /// source of truth replacing the magic-number consts that used to live
    /// in config, the stream loop, the MCP client, and the LSP manager.
    pub fn resolve_timeouts(&self) -> crate::timeout::Timeouts {
        let d = crate::timeout::Timeouts::DEFAULT;
        let c = self.timeouts.clone().unwrap_or_default();
        let or_default = |o: Option<u64>, default: std::time::Duration| {
            o.map(std::time::Duration::from_secs).unwrap_or(default)
        };
        crate::timeout::Timeouts {
            stream_chunk: or_default(c.stream_chunk_secs, d.stream_chunk),
            request_establish: or_default(c.request_establish_secs, d.request_establish),
            tool_call_gap: or_default(c.tool_call_gap_secs, d.tool_call_gap),
            mcp_call: or_default(c.mcp_call_secs, d.mcp_call),
            mcp_init: or_default(c.mcp_init_secs, d.mcp_init),
            lsp_request: or_default(c.lsp_request_secs, d.lsp_request),
            lsp_initialize: or_default(c.lsp_initialize_secs, d.lsp_initialize),
            bash: or_default(c.bash_secs, d.bash),
        }
    }

    pub fn resolve_show_edit_diff(&self) -> bool {
        self.show_edit_diff.unwrap_or(true)
    }

    /// Whether the thinking/reasoning burst is visible by default (GH #461).
    /// Defaults to false — reasoning stays hidden until toggled with Ctrl+O.
    pub fn resolve_show_reasoning(&self) -> bool {
        self.show_reasoning.unwrap_or(false)
    }

    /// Resolve the cross-session history depth (how many prior
    /// same-project sessions are mined for Up-arrow recall). Default 3.
    pub fn resolve_max_sessions(&self) -> usize {
        self.max_sessions.unwrap_or(3)
    }

    /// Resolve the sandbox mode, preferring the nested `sandbox.mode`.
    pub fn resolve_sandbox_mode(&self) -> crate::sandbox::SandboxMode {
        self.sandbox
            .as_ref()
            .map(|s| s.to_mode())
            .unwrap_or(crate::sandbox::SandboxMode::Off)
    }

    /// Resolve the microVM image: `sandbox.image` first, then
    /// the legacy top-level `microvm_image` as fallback.
    pub fn resolve_microvm_image(&self) -> Option<String> {
        self.sandbox
            .as_ref()
            .and_then(|s| s.image.clone())
            .or_else(|| self.microvm_image.clone())
    }

    /// Resolve microVM vCPU count. Default 1.
    pub fn resolve_microvm_cpus(&self) -> u8 {
        self.sandbox.as_ref().and_then(|s| s.cpus).unwrap_or(1)
    }

    /// Resolve microVM RAM in MiB. Default 512.
    pub fn resolve_microvm_memory_mib(&self) -> u32 {
        self.sandbox
            .as_ref()
            .and_then(|s| s.memory_mib)
            .unwrap_or(512)
    }
}

/// Static per-model context-window table. Returns `None` for unknown
/// models so callers can fall back to a sane default. Matched by
/// case-insensitive substring so a provider-prefixed or
/// version-suffixed id (`openai/gpt-4o`, `claude-3.5-sonnet-20241022`,
/// `deepseek-v4-pro`) still hits the right family. Order matters:
/// the FIRST matching prefix wins — list longer / more-specific
/// keys first.
///
/// Values are the model's documented maximum context (input + output
/// combined where the provider quotes a unified figure). Update as
/// providers extend their context budgets.
/// Read `EXA_API_KEY`, trimming whitespace and treating empty as unset.
/// Single source so every consumer (web-search tool, MCP auto-register,
/// the builder) applies the same trim/empty policy (dirge-3xqe).
pub fn exa_api_key() -> Option<String> {
    std::env::var("EXA_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn web_env_true(k: &str) -> bool {
    std::env::var(k)
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

/// Whether the websearch tool is enabled: config `tools.websearch`
/// (default true) OR `WEBSEARCH_ENABLED`. Single source for the
/// precedence duplicated across the two builder paths (dirge-f8oe).
pub fn websearch_enabled(cfg: &Config) -> bool {
    cfg.tools.as_ref().and_then(|t| t.websearch).unwrap_or(true)
        || web_env_true("WEBSEARCH_ENABLED")
}

/// Whether the webfetch tool is enabled: config `tools.webfetch`
/// (default true) OR `WEBFETCH_ENABLED`.
pub fn webfetch_enabled(cfg: &Config) -> bool {
    cfg.tools.as_ref().and_then(|t| t.webfetch).unwrap_or(true) || web_env_true("WEBFETCH_ENABLED")
}

pub fn context_window_for_model(model: &str) -> Option<u64> {
    let m = model.to_lowercase();
    // Ordered: most-specific first.
    const TABLE: &[(&str, u64)] = &[
        // DeepSeek
        ("deepseek-v4", 1_000_000),
        ("deepseek-r1", 128_000),
        ("deepseek", 128_000),
        // GLM / ZhipuAI
        ("glm-5.2", 1_000_000),
        ("glm-4.6", 200_000),
        ("glm-4.5", 128_000),
        ("glm-4", 128_000),
        // Anthropic Claude
        ("claude-opus-4-5", 1_000_000),
        ("claude-opus-4-7", 1_000_000),
        ("claude-sonnet-4-5", 1_000_000),
        ("claude-sonnet-4-6", 1_000_000),
        ("claude-opus", 200_000),
        ("claude-sonnet", 200_000),
        ("claude-haiku", 200_000),
        ("claude-3-7", 200_000),
        ("claude-3.5", 200_000),
        ("claude-3", 200_000),
        ("claude", 200_000),
        // OpenAI GPT
        ("gpt-5", 400_000),
        ("gpt-4.1", 1_000_000),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4", 128_000),
        ("o3", 200_000),
        ("o1", 200_000),
        // Google Gemini
        ("gemini-2.0-flash-thinking", 32_000),
        ("gemini-2.5-pro", 2_000_000),
        ("gemini-2.5-flash", 1_000_000),
        ("gemini-2.0-pro", 2_000_000),
        ("gemini-2.0-flash", 1_000_000),
        ("gemini-1.5-pro", 2_000_000),
        ("gemini-1.5-flash", 1_000_000),
        ("gemini-pro", 128_000),
        ("gemini", 128_000),
        // Meta / Llama (via OpenRouter and others)
        ("llama-4", 1_000_000),
        ("llama-3.3", 128_000),
        ("llama-3.1", 128_000),
        ("llama-3", 8_000),
        // Mistral
        ("mistral-large", 128_000),
        ("mistral", 32_000),
        // Qwen
        ("qwen2.5", 128_000),
        ("qwen", 32_000),
    ];
    for (key, window) in TABLE {
        if m.contains(key) {
            return Some(*window);
        }
    }
    None
}

pub fn config_file_path() -> PathBuf {
    storage::config_path().join("config.json")
}

/// Project-local config, layered on top of the global
/// `~/.config/dirge/config.json`. CWD-relative (matches `.dirge/agents`,
/// `.dirge/plugins`, `.dirge/skills`). Fields present here override
/// their global counterparts; absent keys fall through. Map-valued
/// fields (`providers`, `mcp_servers`, `agents`, …) merge key-by-key
/// rather than replacing the whole map.
pub fn project_config_file_path() -> PathBuf {
    PathBuf::from(".dirge").join("config.json")
}

/// Read a config JSON file into a `serde_json::Value`. Returns `None`
/// when the file is absent (caller falls back to defaults). A
/// present-but-unreadable or unparseable file is a hard error: print
/// the offending path and exit, matching the contract for the global
/// config so a typo never silently downgrades to defaults.
fn read_config_value(path: &Path) -> Option<serde_json::Value> {
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!(
            "error: failed to read config file ({}): {}\n\
             Fix the file or remove it to use defaults.",
            path.display(),
            e,
        );
        std::process::exit(1);
    });
    Some(serde_json::from_str(&content).unwrap_or_else(|e| {
        eprintln!(
            "error: {} is not a valid config: {}\n\
             Fix the file or remove it to use defaults.",
            path.display(),
            e,
        );
        std::process::exit(1);
    }))
}

/// Recursively merge `overlay` into `base`. Object keys are unioned
/// (overlay wins on collision, recursing when both sides are objects);
/// any non-object overlay value replaces `base` outright. This gives
/// the "project layers on top of global without wiping absent global
/// keys" semantics: scalars (`provider`, `max_tokens`, …) override,
/// while maps union so a project can add/override a single entry
/// without redeclaring the whole map. An empty overlay object is a
/// no-op (global entries retained) — there is intentionally no syntax
/// to *clear* a global map from a project config.
fn merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            for (k, v) in overlay_map {
                match base_map.get_mut(&k) {
                    Some(existing) => merge_json(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (slot, overlay) => *slot = overlay,
    }
}

pub fn load() -> Config {
    let global_path = config_file_path();
    let project_path = project_config_file_path();

    // Base = global config.json (empty object → defaults if absent).
    let mut value: serde_json::Value = read_config_value(&global_path)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    // Layer the project-local `.dirge/config.json` on top.
    if let Some(project) = read_config_value(&project_path) {
        merge_json(&mut value, project);
    }

    // Reject legacy config shape BEFORE deserialising. Checked on the
    // merged value so a legacy key in EITHER file surfaces with both
    // paths named so the user knows which file to edit.
    if let Some(obj) = value.as_object() {
        const LEGACY: &[&str] = &["custom_providers", "model", "review_model"];
        let found: Vec<&str> = LEGACY
            .iter()
            .copied()
            .filter(|k| obj.contains_key(*k))
            .collect();
        if !found.is_empty() {
            eprintln!(
                "error: legacy config keys found ({:?}) after merging {} and {}",
                found,
                global_path.display(),
                project_path.display(),
            );
            eprintln!("Migrate to the unified `providers` map:");
            eprintln!("  - top-level `model`         -> `providers.<active-provider>.model`");
            eprintln!("  - `custom_providers.X`      -> `providers.X`");
            eprintln!("  - top-level `review_model`  -> `providers.<review-provider>.model`");
            eprintln!(
                "Then optionally set `review_provider`, `escalation_provider`, \
                 `summarization_provider`, `subagent_provider`."
            );
            std::process::exit(2);
        }
    }

    #[allow(unused_mut)]
    let mut cfg: Config = serde_json::from_value(value).unwrap_or_else(|e| {
        eprintln!(
            "error: merged config ({} + {}) is not valid: {}\n\
             Fix the offending file or remove it to use defaults.",
            global_path.display(),
            project_path.display(),
            e,
        );
        std::process::exit(1);
    });

    // Validate `providers` at load time so a typo in
    // `provider_type` (or an alias that doesn't match a built-in
    // and has no explicit provider_type) surfaces immediately
    // instead of failing at first agent call with a cryptic
    // "unknown provider" deep in the call stack.
    if let Some(providers) = cfg.providers.as_ref() {
        for (name, p) in providers {
            let ptype = Config::provider_type_of(name, p);
            if crate::provider::parse_provider(&ptype).is_none() {
                eprintln!(
                    "error: provider {:?} has invalid provider_type {:?}.\n\
                     Either the alias must match a built-in (openrouter, openai,\n\
                     anthropic, gemini, deepseek, glm, ollama, custom) or set\n\
                     `provider_type` explicitly to one of those.",
                    name, ptype,
                );
                std::process::exit(1);
            }
        }
    }

    #[cfg(feature = "mcp")]
    if cfg.mcp_servers.is_none() {
        // Only auto-register the Exa default when there's actually
        // a non-empty API key. An empty `EXA_API_KEY=""` (e.g. unset
        // via a `.envrc` that intentionally clears it) used to
        // register Exa anyway with an empty header, then every web-
        // search call failed with 401 at first use. Skip cleanly
        // when no usable key is present.
        match exa_api_key() {
            Some(key) => {
                let mut headers = HashMap::new();
                headers.insert("x-api-key".to_string(), key);
                let mut defaults = HashMap::new();
                defaults.insert(
                    "Exa Web Search".to_string(),
                    McpServerConfig::Url {
                        url: "https://mcp.exa.ai/mcp".to_string(),
                        headers,
                        allow_external_paths: false,
                    },
                );
                cfg.mcp_servers = Some(defaults);
            }
            _ => {
                // Key unset or empty — leave mcp_servers as None so
                // the host knows there's nothing to connect to.
            }
        }
    }

    cfg
}

/// Merge sandbox-related keys into the user's config.json without
/// clobbering unrelated keys. Reads the existing file (if any),
/// sets/overwrites only the given keys, and writes back pretty-printed
/// JSON. Creates the config dir + file if they don't exist.
#[cfg(feature = "sandbox-microvm")]
pub fn update_config_file(updates: &serde_json::Value) -> anyhow::Result<()> {
    let path = config_file_path();

    let mut existing: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    if let Some(obj) = updates.as_object() {
        for (k, v) in obj {
            existing.insert(k.clone(), v.clone());
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&existing)?;
    std::fs::write(&path, json)?;
    Ok(())
}

#[cfg(all(test, feature = "lsp"))]
mod tests {
    use super::*;

    /// dirge-mt91: the legacy nested `microvm: {cpus, memory_mib}`
    /// form used `as u8`/`as u32` casts that silently wrapped — 256
    /// CPUs became 0. Out-of-range values must now be a clean
    /// deserialization error, and valid ones still parse.
    #[test]
    fn sandbox_legacy_nested_rejects_out_of_range_cpus() {
        let ok: SandboxConfig =
            serde_json::from_str(r#"{ "mode": "microvm", "microvm": { "cpus": 4 } }"#).unwrap();
        assert_eq!(ok.cpus, Some(4));

        let err = serde_json::from_str::<SandboxConfig>(
            r#"{ "mode": "microvm", "microvm": { "cpus": 256 } }"#,
        );
        assert!(err.is_err(), "256 CPUs must error, not wrap to 0");

        let err = serde_json::from_str::<SandboxConfig>(
            r#"{ "mode": "microvm", "microvm": { "memory_mib": 5000000000 } }"#,
        );
        assert!(err.is_err(), "out-of-range memory_mib must error");
    }

    /// Phased workflow is opt-in and off by default; the review-cycle
    /// budget defaults to vix's 2 and is honored when set.
    #[test]
    fn phased_workflow_defaults_off_with_two_cycles() {
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!cfg.resolve_phased_workflow_enabled());
        assert_eq!(cfg.resolve_phased_workflow_max_review_cycles(), 2);

        let cfg: Config = serde_json::from_str(
            r#"{ "phased_workflow_enabled": true, "phased_workflow_max_review_cycles": 4 }"#,
        )
        .unwrap();
        assert!(cfg.resolve_phased_workflow_enabled());
        assert_eq!(cfg.resolve_phased_workflow_max_review_cycles(), 4);
    }

    /// dirge-4hld: the `memory` block is absent by default and parses its
    /// fields when present.
    #[test]
    fn chord_timeout_ms_absent_and_parses() {
        // dirge-5kkx.1: off by default; parses from the documented key.
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert!(cfg.chord_timeout_ms.is_none());
        let cfg: Config = serde_json::from_str(r#"{ "chord_timeout_ms": 1500 }"#).unwrap();
        assert_eq!(cfg.chord_timeout_ms, Some(1500));
    }

    #[test]
    fn critic_preamble_absent_and_parses() {
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert!(cfg.critic_preamble.is_none());

        let cfg: Config =
            serde_json::from_str(r#"{ "critic_preamble": "Be extra strict." }"#).unwrap();
        assert_eq!(cfg.critic_preamble.as_deref(), Some("Be extra strict."));

        // Absent → resolve returns the built-in critic preamble.
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(
            cfg.resolve_critic_preamble(),
            crate::agent::agent_loop::critic::CRITIC_PREAMBLE,
        );
    }

    #[test]
    fn code_review_field_absent_and_parses() {
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert!(cfg.code_review.is_none());

        let cfg: Config = serde_json::from_str(r#"{ "code_review": "blocking" }"#).unwrap();
        assert_eq!(cfg.code_review.as_deref(), Some("blocking"));
    }

    #[test]
    fn resolve_code_review_mode_each_string_and_default() {
        use crate::agent::agent_loop::types::CodeReviewMode;

        // Each documented string (case-insensitive, trimmed) maps through.
        let mk = |raw: &str| {
            Config::deserialize(serde_json::json!({ "code_review": raw }))
                .unwrap()
                .resolve_code_review_mode()
        };
        assert_eq!(mk("off"), CodeReviewMode::Off);
        assert_eq!(mk("OFF"), CodeReviewMode::Off);
        assert_eq!(mk("  Blocking  "), CodeReviewMode::Blocking);
        assert_eq!(mk("advisory"), CodeReviewMode::Advisory);

        // Absent / empty → default Advisory.
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(cfg.resolve_code_review_mode(), CodeReviewMode::Advisory);
        let cfg: Config = serde_json::from_str(r#"{ "code_review": "   " }"#).unwrap();
        assert_eq!(cfg.resolve_code_review_mode(), CodeReviewMode::Advisory);
    }

    #[test]
    fn resolve_code_review_mode_unknown_warns_and_defaults() {
        use crate::agent::agent_loop::types::CodeReviewMode;
        // An unrecognized non-empty value must NOT silently map to Off —
        // it falls back to Advisory (the safe, non-disabling default).
        let cfg: Config = serde_json::from_str(r#"{ "code_review": "nuclear" }"#).unwrap();
        assert_eq!(cfg.resolve_code_review_mode(), CodeReviewMode::Advisory);
    }

    /// Cross-session Up-arrow history depth: absent by default (→3),
    /// parses from the documented key, and resolve applies the default.
    #[test]
    fn max_sessions_absent_and_parses() {
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert!(cfg.max_sessions.is_none());
        assert_eq!(cfg.resolve_max_sessions(), 3, "default depth is 3");

        let cfg: Config = serde_json::from_str(r#"{ "max_sessions": 5 }"#).unwrap();
        assert_eq!(cfg.max_sessions, Some(5));
        assert_eq!(cfg.resolve_max_sessions(), 5);

        // 0 is a valid opt-out: mine zero prior sessions.
        let cfg: Config = serde_json::from_str(r#"{ "max_sessions": 0 }"#).unwrap();
        assert_eq!(cfg.resolve_max_sessions(), 0);
    }

    #[test]
    fn memory_config_defaults_absent_and_parses() {
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert!(cfg.memory.is_none(), "no memory block by default");

        let cfg: Config = serde_json::from_str(
            r#"{ "memory": { "hybrid_retrieval": true, "embed_url": "http://localhost:11434/v1/embeddings", "embed_api_key_env": "OPENAI_API_KEY", "verbatim_pre_recall": true } }"#,
        )
        .unwrap();
        let m = cfg.memory.expect("memory block present");
        assert_eq!(m.hybrid_retrieval, Some(true));
        assert_eq!(
            m.embed_url.as_deref(),
            Some("http://localhost:11434/v1/embeddings")
        );
        assert_eq!(
            m.embed_model, None,
            "model is optional (falls back to default)"
        );
        assert_eq!(m.embed_api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(m.verbatim_pre_recall, Some(true));
    }

    /// dirge-j0s2 (GH #461): `show_reasoning` controls whether the thinking
    /// burst is visible by default. Absent → false (current behavior).
    #[test]
    fn show_reasoning_defaults_off_and_parses() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.show_reasoning, None);
        assert!(!cfg.resolve_show_reasoning(), "off by default");

        let cfg: Config = serde_json::from_str(r#"{"show_reasoning": true}"#).unwrap();
        assert!(cfg.resolve_show_reasoning());

        let cfg: Config = serde_json::from_str(r#"{"show_reasoning": false}"#).unwrap();
        assert!(!cfg.resolve_show_reasoning());
    }

    /// dirge-4xgd: `[timeouts]` overrides merge onto Timeouts::DEFAULT;
    /// unset fields keep their defaults.
    #[test]
    fn timeouts_override_merges_onto_defaults() {
        let d = crate::timeout::Timeouts::DEFAULT;

        // No block → all defaults.
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        let t = cfg.resolve_timeouts();
        assert_eq!(t.mcp_call, d.mcp_call);
        assert_eq!(t.lsp_request, d.lsp_request);

        // Partial block → named fields override, rest default.
        let cfg: Config =
            serde_json::from_str(r#"{ "timeouts": { "mcp_call_secs": 45, "bash_secs": 300 } }"#)
                .unwrap();
        let t = cfg.resolve_timeouts();
        assert_eq!(t.mcp_call, std::time::Duration::from_secs(45));
        assert_eq!(t.bash, std::time::Duration::from_secs(300));
        // Untouched fields keep defaults.
        assert_eq!(t.lsp_request, d.lsp_request);
        assert_eq!(t.mcp_init, d.mcp_init);
        assert_eq!(t.request_establish, d.request_establish);

        // dirge-u44q: request_establish_secs is a first-class override.
        let cfg: Config =
            serde_json::from_str(r#"{ "timeouts": { "request_establish_secs": 90 } }"#).unwrap();
        let t = cfg.resolve_timeouts();
        assert_eq!(t.request_establish, std::time::Duration::from_secs(90));
        assert_eq!(t.stream_chunk, d.stream_chunk);
    }

    #[test]
    fn lsp_config_parses_as_bool() {
        let cfg: Config = serde_json::from_str(r#"{"lsp": true}"#).unwrap();
        assert!(cfg.lsp.unwrap().is_enabled());

        let cfg: Config = serde_json::from_str(r#"{"lsp": false}"#).unwrap();
        assert!(!cfg.lsp.unwrap().is_enabled());
    }

    /// dirge-99ic: `plugins.<name>.{enabled, auto_start}` toggles, with
    /// enabled defaulting to true (plugins load unless explicitly off).
    #[test]
    fn plugin_toggles_parse_with_enabled_default_true() {
        let cfg: Config = serde_json::from_str(
            r#"{
                "plugins": {
                    "backpressured": {"enabled": true, "auto_start": true},
                    "nrepl": {"enabled": false},
                    "noisy": {"auto_start": true}
                }
            }"#,
        )
        .unwrap();

        assert!(cfg.plugin_enabled("backpressured"));
        assert!(cfg.plugin_auto_start("backpressured"));

        assert!(!cfg.plugin_enabled("nrepl"));
        assert!(!cfg.plugin_auto_start("nrepl"));

        // enabled omitted → defaults to true; auto_start honored.
        assert!(cfg.plugin_enabled("noisy"));
        assert!(cfg.plugin_auto_start("noisy"));

        // Absent entry → enabled, not auto-started.
        assert!(cfg.plugin_enabled("unlisted"));
        assert!(!cfg.plugin_auto_start("unlisted"));

        // No `plugins` block at all → everything loads (backward compat).
        let empty: Config = serde_json::from_str("{}").unwrap();
        assert!(empty.plugin_enabled("anything"));
        assert!(!empty.plugin_auto_start("anything"));
    }

    #[test]
    fn provider_auth_mode_parses_chatgpt_aliases() {
        let cfg: Config = serde_json::from_str(
            r#"{
                "auth": "chatgpt",
                "providers": {
                    "openai": { "auth": "chatgpt" },
                    "codex": { "auth": "chatgpt_auth_tokens" }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(cfg.auth, Some(ProviderAuth::ChatGpt));
        let providers = cfg.providers.unwrap();
        assert_eq!(providers["openai"].auth, Some(ProviderAuth::ChatGpt));
        assert_eq!(providers["codex"].auth, Some(ProviderAuth::ChatGpt));

        let cfg: Config =
            serde_json::from_str(r#"{ "providers": { "openai": { "auth": "api-key" } } }"#)
                .unwrap();
        assert_eq!(
            cfg.providers.unwrap()["openai"].auth,
            Some(ProviderAuth::ApiKey)
        );
    }

    #[test]
    fn provider_auth_mode_parses_anthropic_aliases() {
        let cfg: Config = serde_json::from_str(
            r#"{
                "providers": {
                    "anthropic": { "auth": "anthropic" },
                    "claude": { "auth": "claude-code" }
                }
            }"#,
        )
        .unwrap();

        let providers = cfg.providers.unwrap();
        assert_eq!(providers["anthropic"].auth, Some(ProviderAuth::Anthropic));
        assert_eq!(providers["claude"].auth, Some(ProviderAuth::Anthropic));
    }

    #[test]
    fn lsp_config_parses_as_per_server_map() {
        let raw = r#"{
            "lsp": {
                "rust": { "command": ["my-rust-analyzer", "--my-arg"] },
                "typescript": { "disabled": true }
            }
        }"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let overrides = cfg.lsp.as_ref().unwrap().server_overrides();
        assert_eq!(overrides.len(), 2);
        assert_eq!(
            overrides["rust"].command.as_ref().unwrap(),
            &vec!["my-rust-analyzer".to_string(), "--my-arg".to_string()]
        );
        assert_eq!(overrides["typescript"].disabled, Some(true));
    }

    // Regression: when lsp is omitted entirely, default is "enabled with
    // built-in commands" — the CLI's resolve_lsp_enabled handles that.
    // Config-side, an absent value parses to `None`.
    #[test]
    fn absent_lsp_config_is_none() {
        let cfg: Config = serde_json::from_str(r#"{"provider": "deepseek"}"#).unwrap();
        assert!(cfg.lsp.is_none());
    }

    // Regression: a config that mixes overrides for valid server ids
    // (rust) with disabled-only entries (typescript) must parse cleanly.
    #[test]
    fn lsp_config_mixes_command_and_disabled_entries() {
        let raw = r#"{
            "lsp": {
                "rust": { "command": ["rust-analyzer"], "env": {"RUST_LOG": "info"} },
                "typescript": { "disabled": true }
            }
        }"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let overrides = cfg.lsp.as_ref().unwrap().server_overrides();
        assert!(overrides["rust"].command.is_some());
        assert_eq!(
            overrides["rust"]
                .env
                .as_ref()
                .unwrap()
                .get("RUST_LOG")
                .unwrap(),
            "info"
        );
        assert_eq!(overrides["typescript"].disabled, Some(true));
    }
}

#[cfg(test)]
mod model_context_tests {
    use super::*;

    /// Per-model table maps common provider/version-prefixed ids to
    /// their published context windows.
    #[test]
    fn known_models_resolve_to_published_windows() {
        for (model, want) in &[
            ("deepseek-v4-pro", 1_000_000),
            ("deepseek/deepseek-v4-flash", 1_000_000),
            ("claude-opus-4-7", 1_000_000),
            ("claude-sonnet-4-6", 1_000_000),
            ("claude-3.5-sonnet-20241022", 200_000),
            ("openai/gpt-4o", 128_000),
            ("gpt-5", 400_000),
            ("gemini-2.5-pro", 2_000_000),
            ("gemini-1.5-flash-002", 1_000_000),
            ("glm-4.6", 200_000),
            ("glm-5.2", 1_000_000),
        ] {
            let got = context_window_for_model(model);
            assert_eq!(
                got,
                Some(*want),
                "model {model} expected {want}, got {got:?}",
            );
        }
    }

    /// Unknown models return `None` so the caller falls back to the
    /// 128k default.
    #[test]
    fn unknown_model_returns_none() {
        assert!(context_window_for_model("totally-fictional-model").is_none());
        assert!(context_window_for_model("").is_none());
    }

    /// Match is case-insensitive — provider ids that uppercase
    /// product names still hit the table.
    #[test]
    fn model_match_is_case_insensitive() {
        assert_eq!(context_window_for_model("Claude-Opus-4-7"), Some(1_000_000));
        assert_eq!(context_window_for_model("DEEPSEEK-V4-PRO"), Some(1_000_000));
    }

    /// Explicit `context_window` in config wins over the model table.
    #[test]
    fn explicit_config_overrides_model_table() {
        let cfg = Config {
            context_window: Some(50_000),
            ..Default::default()
        };
        // deepseek would normally resolve to 1M.
        assert_eq!(cfg.resolve_context_window("deepseek-v4-pro"), 50_000);
    }

    /// Default fallback (no explicit config, unknown model) = 128k.
    #[test]
    fn fallback_default_is_128k() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve_context_window("unknown-model-9000"), 128_000);
    }
}

#[cfg(test)]
mod provider_role_tests {
    use super::*;

    fn cfg_with_providers(json: &str) -> Config {
        serde_json::from_str(json).expect("parses")
    }

    #[test]
    fn resolve_role_default_returns_provider_entry() {
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "providers": { "deepseek": { "model": "deepseek-v4-pro" } }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Default).unwrap();
        assert_eq!(name, "deepseek");
        assert_eq!(entry.model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn resolve_role_review_falls_back_to_default_provider() {
        // No review_provider set — review should fall back to the
        // active provider's entry.
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "providers": { "deepseek": { "model": "deepseek-v4-pro" } }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Review).unwrap();
        assert_eq!(name, "deepseek");
        assert_eq!(entry.model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn resolve_role_review_uses_explicit_assignment() {
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "review_provider": "glm",
                "providers": {
                    "deepseek": { "model": "deepseek-v4-pro" },
                    "glm": { "model": "glm-4.6" }
                }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Review).unwrap();
        assert_eq!(name, "glm");
        assert_eq!(entry.model.as_deref(), Some("glm-4.6"));
    }

    #[test]
    fn provider_type_of_returns_explicit_value_when_set() {
        let entry = ProviderEntry {
            provider_type: Some("openai".to_string()),
            ..Default::default()
        };
        assert_eq!(Config::provider_type_of("ollama", &entry), "openai");
    }

    #[test]
    fn provider_type_of_falls_back_to_alias_when_unset() {
        let entry = ProviderEntry::default();
        assert_eq!(Config::provider_type_of("deepseek", &entry), "deepseek");
        // Lowercases so `Anthropic` alias still parses as built-in.
        assert_eq!(Config::provider_type_of("Anthropic", &entry), "anthropic");
    }

    #[test]
    fn providers_map_returns_clone() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "deepseek": { "model": "x" } }
            }"#,
        );
        let map = cfg.providers_map();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("deepseek"));
    }

    #[test]
    fn providers_map_empty_when_unset() {
        let cfg = Config::default();
        assert!(cfg.providers_map().is_empty());
    }

    /// New unified shape (matches the target documented in the
    /// refactor): a `providers` map with mixed built-in entries
    /// (just a `model`) and aliased entries (`provider_type` +
    /// `base_url`) parses cleanly and round-trips through
    /// `resolve_role` / `provider_type_of`.
    #[test]
    fn new_shape_with_aliased_ollama_parses() {
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "providers": {
                    "deepseek": { "model": "deepseek-v4-pro" },
                    "ollama": {
                        "provider_type": "openai",
                        "base_url": "http://127.0.0.1:11434/v1"
                    }
                }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Default).unwrap();
        assert_eq!(name, "deepseek");
        assert_eq!(entry.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(Config::provider_type_of("deepseek", &entry), "deepseek");

        let ollama = cfg.providers_map().get("ollama").cloned().unwrap();
        assert_eq!(Config::provider_type_of("ollama", &ollama), "openai");
        assert_eq!(
            ollama.base_url.as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
    }

    /// `api_key` accepts both snake_case and `apiKey` camelCase. A literal
    /// passes through; a `${VAR}` form expands against the env at call
    /// time.
    #[test]
    fn api_key_literal_passes_through() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "api_key": "sk-literal" } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(
            entry.resolved_api_key().and_then(|r| r.ok()),
            Some("sk-literal".to_string())
        );
    }

    #[test]
    fn api_key_camel_case_alias_parses() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "apiKey": "sk-camel" } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(entry.api_key.as_deref(), Some("sk-camel"));
    }

    #[test]
    fn api_key_env_interpolation_expands() {
        // SAFETY: tests in this module are inside the same process so
        // setting an env var is racy across threads. Use a uniquely-
        // named var so a concurrent test doesn't observe ours.
        let var = "DIRGE_TEST_API_KEY_EXPAND";
        unsafe { std::env::set_var(var, "sk-from-env") };
        let cfg = cfg_with_providers(&format!(
            r#"{{
                "providers": {{ "glm": {{ "api_key": "${{{var}}}" }} }}
            }}"#
        ));
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(
            entry.resolved_api_key().and_then(|r| r.ok()),
            Some("sk-from-env".to_string())
        );
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn api_key_env_interpolation_reports_missing_var() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "api_key": "${DIRGE_TEST_MISSING_VAR_NEVER_SET}" } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        let err = entry.resolved_api_key().unwrap().unwrap_err();
        assert_eq!(err, "DIRGE_TEST_MISSING_VAR_NEVER_SET");
    }

    #[test]
    fn api_key_none_when_unset() {
        let entry = ProviderEntry::default();
        assert!(entry.resolved_api_key().is_none());
    }

    /// `options.temperature` is honored as f64. Other types in the
    /// same slot return None.
    #[test]
    fn options_temperature_f64() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "options": { "temperature": 0.2 } } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(entry.options_temperature(), Some(0.2));
    }

    #[test]
    fn options_temperature_missing_or_wrong_shape() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": {
                    "no-options":  {},
                    "wrong-shape": { "options": { "temperature": "hot" } }
                }
            }"#,
        );
        assert_eq!(
            cfg.providers_map()
                .get("no-options")
                .unwrap()
                .options_temperature(),
            None
        );
        assert_eq!(
            cfg.providers_map()
                .get("wrong-shape")
                .unwrap()
                .options_temperature(),
            None
        );
    }

    /// Legacy `model` at top level is detected before deserialization.
    /// `load()` reads from disk so we can't drive it directly here;
    /// we verify the detection predicate the same way `load()` does.
    #[test]
    fn legacy_model_key_detected() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"model": "deepseek-v4-pro"}"#).unwrap();
        let obj = raw.as_object().unwrap();
        let legacy = ["custom_providers", "model", "review_model"];
        let found: Vec<&str> = legacy
            .iter()
            .copied()
            .filter(|k| obj.contains_key(*k))
            .collect();
        assert_eq!(found, vec!["model"]);
    }

    #[test]
    fn legacy_custom_providers_key_detected() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"custom_providers": {"x": {}}}"#).unwrap();
        let obj = raw.as_object().unwrap();
        let legacy = ["custom_providers", "model", "review_model"];
        let found: Vec<&str> = legacy
            .iter()
            .copied()
            .filter(|k| obj.contains_key(*k))
            .collect();
        assert_eq!(found, vec!["custom_providers"]);
    }
}

#[cfg(test)]
mod config_merge_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_overrides_scalar_but_keeps_absent_keys() {
        let mut base = json!({ "provider": "deepseek", "max_tokens": 4096 });
        merge_json(&mut base, json!({ "max_tokens": 8192 }));
        assert_eq!(base["provider"], "deepseek");
        assert_eq!(base["max_tokens"], 8192);
    }

    #[test]
    fn merge_unions_map_values_key_by_key() {
        let mut base = json!({
            "providers": { "deepseek": { "model": "v3" }, "glm": { "model": "glm-4.6" } }
        });
        merge_json(
            &mut base,
            json!({ "providers": { "deepseek": { "model": "v4-pro" } } }),
        );
        assert_eq!(base["providers"]["deepseek"]["model"], "v4-pro");
        assert_eq!(base["providers"]["glm"]["model"], "glm-4.6");
    }

    #[test]
    fn merge_recurses_into_nested_objects() {
        let mut base = json!({ "providers": { "ollama": { "base_url": "x", "model": "qwen" } } });
        merge_json(
            &mut base,
            json!({ "providers": { "ollama": { "model": "llama" } } }),
        );
        assert_eq!(base["providers"]["ollama"]["base_url"], "x");
        assert_eq!(base["providers"]["ollama"]["model"], "llama");
    }

    #[test]
    fn merge_empty_map_overlay_is_a_noop_union() {
        let mut base = json!({ "mcp_servers": { "exa": {} } });
        merge_json(&mut base, json!({ "mcp_servers": {} }));
        assert!(base["mcp_servers"]["exa"].as_object().is_some());
    }
}

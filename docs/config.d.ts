/**
 * Type definitions for ~/.config/dirge/config.json
 * Generated from src/config/mod.rs and docs/config.md
 */

// ── Top-level config ───────────────────────────────────────────────

interface Config {
  /** Active provider alias. Built-ins: openrouter, openai, anthropic, gemini/google, deepseek, glm/zhipu, ollama. Default: "openrouter". */
  provider?: string;

  /** Default auth source for providers without explicit `auth`. */
  auth?: ProviderAuth;

  /** Maximum response tokens. Default: 8192. */
  max_tokens?: number;

  /** Model sampling temperature (0.0–2.0). Clamped with warning if out of range. */
  temperature?: number;

  /** Disable all tools. Default: false. */
  no_tools?: boolean;

  /** Disable loading AGENTS.md / CLAUDE.md context files. Default: false. */
  no_context_files?: boolean;

  /** Session context-window size for status and auto-compaction. Default: 128000. */
  context_window?: number;

  /** Tokens to reserve before compaction triggers. Default: 16384. */
  reserve_tokens?: number;

  /** Approximate recent-token budget kept verbatim during compaction. Default: 20000. */
  keep_recent_tokens?: number;

  /** Maximum agent turns per response. Default: 100. */
  max_agent_turns?: number;

  /** Enable automatic conversation compaction. Default: true. */
  compact_enabled?: boolean;

  /** Provider map keyed by alias. Each role key references one of these aliases. */
  providers?: Record<string, ProviderEntry>;

  /** User-defined agent profiles, keyed by name. */
  agents?: Record<string, AgentConfig>;

  /** Per-plugin settings, keyed by plugin name (directory or .janet stem). */
  plugins?: Record<string, PluginSettings>;

  /** Permission rules. Deserialized into a strict struct (only the keys on
   *  PermissionConfig are accepted; unknown keys are rejected). */
  permission?: PermissionConfig;

  /** Select restrictive permission mode. Overridden by accept_all/yolo. */
  restrictive?: boolean;

  /** Auto-approve tools inside cwd. Overridden by yolo. */
  accept_all?: boolean;

  /** Bypass every check. Use with caution. */
  yolo?: boolean;

  /** Sandbox bash commands: bool, mode string, or object. Default: false (off). */
  sandbox?: boolean | "off" | "bwrap" | "microvm" | SandboxConfig;

  /** OCI image for microVM. Deprecated — use sandbox.image instead. */
  microvm_image?: string;

  /** Permission mode when no CLI flag is set: "standard", "restrictive", "accept", or "yolo". */
  default_permission_mode?: "standard" | "restrictive" | "accept" | "yolo";

  /** Show tool-result output in the TUI. Default: true. */
  show_tool_details?: boolean;

  /** Show colorized diff for edit results. Default: true. */
  show_edit_diff?: boolean;

  /** Startup pane layout: pipe/comma/space-separated subset of "left", "main", "right". E.g. "left|main|right". */
  display?: string;

  /** Hard ceiling on characters per tool result. Default: 500. */
  tool_result_max_chars?: number;

  /** Body lines shown before collapsing to "N more lines (Ctrl+O)". Default: 4. */
  tool_result_max_lines?: number;

  /** Per-chunk streaming read deadline in seconds. Default: 300. */
  stream_chunk_timeout_secs?: number;

  /** Prompt name to activate on startup. Default: "code". */
  default_prompt?: string;

  /** Provider alias for background session-review pass. Falls back to provider. */
  review_provider?: string;

  /** Provider alias for one-shot retry after repair-exhaustion. Falls back to provider. */
  escalation_provider?: string;

  /** Provider alias for context compaction. Falls back to provider. */
  summarization_provider?: string;

  /** Provider alias for task-tool subagents. Falls back to provider. */
  subagent_provider?: string;

  /** Provider alias for F6 in-loop critic + goal-gate judge. No fallback — unset = off. */
  critic_provider?: string;

  /** Provider alias for LLM auto-approval of permission prompts. No fallback — unset = human prompts. */
  approval_provider?: string;

  /** Fraction of context window (0.3–0.75) at which history folds into summary. Default: 0.75. */
  compaction_fold_threshold?: number;

  /** Working-context budget in tokens. Effective window = min(model_window, this). Default: 100000, floored at 16k. */
  context_target?: number;

  /** Refresh durable checkpoint in background at 20%-of-window thresholds. Default: true. Forced off in headless mode. */
  incremental_checkpoint?: boolean;

  /** UI color theme: "phosphor" (default), "plain", or custom name matching ~/.config/dirge/<name>.theme.json. */
  theme?: string;

  /** VSCode-style key-binding overrides for global commands. */
  keybindings?: KeybindingConfig[];

  /** Per-tool configuration. */
  tools?: ToolsConfig;

  /** Long-term memory retrieval tuning (hybrid dense+BM25). Absent = BM25 only. */
  memory?: MemoryConfig;

  /** Ship only tool_search + small always-on set per request; model loads more via tool_search(query). ~30% token savings on MCP-heavy sessions. Default: false. */
  dynamic_tool_search?: boolean;

  /** Consecutive same-file tool calls before mid-turn reminder restates active task. Opt-in; unset = disabled. Recommended: 8. */
  context_depth_reminder_threshold?: number;

  /** Enable /plan phased workflow (explore → plan → implement → reviewer-runs-code). Master kill-switch. Default: false. */
  phased_workflow_enabled?: boolean;

  /** Reviewer-runs-code fix-cycle budget for /plan before stopping with Exhausted. Default: 2. */
  phased_workflow_max_review_cycles?: number;

  /** Per-operation timeout overrides in seconds. Omitted fields keep built-in defaults. */
  timeouts?: TimeoutsConfig;

  /** LSP configuration: boolean to enable/disable, or per-server overrides. (lsp feature) */
  lsp?: boolean | Record<string, LspServerConfig>;

  /** MCP server map keyed by name. (mcp feature) Omitted = default Exa Web Search if EXA_API_KEY is set. Empty object = no servers. */
  mcp_servers?: Record<string, McpServerConfig>;

  /** ACP server config map keyed by name. (acp feature) */
  acp_servers?: Record<string, AcpServerConfig>;
}

// ── Provider types ─────────────────────────────────────────────────

type ProviderAuth = "api-key" | "chatgpt" | "anthropic";

interface ProviderEntry {
  /** Built-in provider type. Optional — inferred from alias key when it matches a built-in name. */
  provider_type?: string;

  /** Endpoint base URL for custom/self-hosted endpoints. */
  base_url?: string;

  /** Model name for this provider. */
  model?: string;

  /** Auth mode: "api-key" (default), "chatgpt" for Codex tokens, "anthropic"/"claude-code" for Claude Code OAuth. */
  auth?: ProviderAuth;

  /** Name of env var holding the API key. Prefer `api_key` with ${VAR} interpolation. */
  api_key_env?: string;

  /** Literal key or ${ENV_VAR} interpolation. Takes precedence over api_key_env. Also accepts "apiKey". */
  api_key?: string;

  /** Allow http:// URLs (plaintext). Default: false. Only enable for local-only proxies. */
  allow_insecure?: boolean;

  /** Per-provider streaming chunk timeout override in seconds. */
  stream_chunk_timeout_secs?: number;

  /** Free-form model options; currently honors "temperature" (f64). Unknown keys ignored. */
  options?: Record<string, unknown>;
}

// ── Agent profiles ─────────────────────────────────────────────────

interface AgentConfig {
  /** System prompt body. None = use active/default prompt. */
  prompt?: string;

  /** Provider alias or model name for this agent's calls. None = keep current model. */
  model?: string;

  /** Tools to allow (deny every built-in not listed). deny_tools wins if both present. */
  allow_tools?: string[];

  /** Tools to deny while this profile is active. Wins over allow_tools when both present. */
  deny_tools?: string[];

  /** Reasoning effort hint: "low", "medium", or "high". Free-form. */
  reasoning?: string;

  /** Sampling temperature override for this agent. */
  temperature?: number;

  /** One-line summary for /agents listing. */
  description?: string;
}

// ── Plugin settings ────────────────────────────────────────────────

interface PluginSettings {
  /** Whether to load the plugin. Default: true (unset = enabled). */
  enabled?: boolean;

  /** Passed to plugin via harness/plugin-config so it can self-engage at startup. Default: false. */
  auto_start?: boolean;
}

// ── Sandbox configuration ──────────────────────────────────────────

interface SandboxConfig {
  /** Sandbox mode: "off", "bwrap" (bubblewrap, Linux), or "microvm". */
  mode?: "off" | "bwrap" | "microvm";

  /** microVM root image. E.g. "alpine:latest", "docker.io/library/debian:stable-slim". */
  image?: string;

  /** microVM vCPU count (1–255). Default: 1. */
  cpus?: number;

  /** microVM memory in MiB. Default: 512. */
  memory_mib?: number;
}

// ── Key bindings ───────────────────────────────────────────────────

interface KeybindingConfig {
  /** Key chord: case-insensitive, modifier-key format. E.g. "ctrl-t", "pageup", "ctrl-shift-x". */
  key: string;

  /** Command name, or "none"/"unbind" to remove the default on that chord.
   *  Matched case-insensitively and `-`/`_`-agnostic (so "next-chat" also works). */
  command:
    | "toggle_reasoning"
    | "expand"
    | "scroll_page_up"
    | "scroll_page_down"
    | "scroll_to_top"
    | "scroll_to_bottom"
    | "next_chat"
    | "prev_chat"
    | "close_chat"
    | "kill_subagent"
    | "drop_queue"
    | "none"
    | "unbind";
}

// ── Tools configuration ────────────────────────────────────────────

interface ToolsConfig {
  /** Enable websearch tool. Default: true (also controlled by WEBSEARCH_ENABLED env). */
  websearch?: boolean;

  /** Enable webfetch tool. Default: true (also controlled by WEBFETCH_ENABLED env). */
  webfetch?: boolean;

  /** Inline output budget for bash tool in bytes. Output above this is relayed to file + summary returned. Default: 8192 (8 KiB). */
  bash_output_inline_max_bytes?: number;

  /** Inline output budget for webfetch tool in bytes. Default: 8192 (8 KiB). */
  webfetch_output_inline_max_bytes?: number;

  /** Inline output budget for task subagent tool in bytes. Default: 8192 (8 KiB). */
  task_output_inline_max_bytes?: number;
}

// ── Memory configuration ───────────────────────────────────────────

interface MemoryConfig {
  /** Turn on hybrid (dense + BM25) memory search. Default: false. */
  hybrid_retrieval?: boolean;

  /** OpenAI-compatible /v1/embeddings endpoint URL. Required for hybrid mode. */
  embed_url?: string;

  /** Embedding model id. Default: "text-embedding-3-small". Set when using non-OpenAI endpoint. */
  embed_model?: string;

  /** Name of env var holding the embeddings API key. Omit for keyless local endpoint. */
  embed_api_key_env?: string;

  /** Auto-search memory on verbatim user message each turn and inject hits as supplemental context. Default: false. */
  verbatim_pre_recall?: boolean;
}

// ── Timeout configuration ──────────────────────────────────────────

interface TimeoutsConfig {
  /** Per-chunk read deadline for streaming LLM response (fallback). Default: 300s. */
  stream_chunk_secs?: number;

  /** Stall window while a tool call is mid-assembly in the stream. Default: 30s. */
  tool_call_gap_secs?: number;

  /** Total budget for one MCP tool call including reconnect + retry. Default: 120s. */
  mcp_call_secs?: number;

  /** MCP server initialize handshake timeout. Default: 10s. */
  mcp_init_secs?: number;

  /** Any non-initialize LSP request timeout. Default: 30s. */
  lsp_request_secs?: number;

  /** LSP initialize handshake timeout. Default: 45s. */
  lsp_initialize_secs?: number;

  /** Default bash tool timeout when the call omits one. Default: 120s. */
  bash_secs?: number;
}

// ── LSP configuration ──────────────────────────────────────────────

interface LspServerConfig {
  /** argv to launch the server. Replaces built-in default. E.g. ["rust-analyzer"]. */
  command?: string[];

  /** File extensions this server handles. Replaces built-in list. */
  extensions?: string[];

  /** Extensions to ADD to the server's built-in list (additive). Also accepts "extendExtensions". */
  extend_extensions?: string[];

  /** Extra environment variables for the child process. */
  env?: Record<string, string>;

  /** Sent as initializationOptions in LSP initialize request. Free-form JSON. */
  initialization?: unknown;

  /** true removes the server entirely. Default: false. */
  disabled?: boolean;
}

// ── MCP server configuration ───────────────────────────────────────

type McpServerConfig = McpCommandServer | McpUrlServer;

interface McpCommandServer {
  /** Executable to spawn for stdio transport (required). */
  command: string;

  /** Arguments passed to the command. Default: []. */
  args?: string[];

  /** Extra environment variables for the child process. Default: {}. */
  env?: Record<string, string>;

  /** Bypass cwd-external-path guard for this server's tools. Default: false. */
  allow_external_paths?: boolean;

  // Discriminator — must not contain "url"
  url?: never;
}

interface McpUrlServer {
  /** Remote MCP endpoint URL (required). */
  url: string;

  /** HTTP headers sent with every request. Default: {}. */
  headers?: Record<string, string>;

  /** Bypass cwd-external-path guard for this server's tools. Default: false. */
  allow_external_paths?: boolean;

  // Discriminator — must not contain "command"
  command?: never;
}

// ── ACP server configuration ───────────────────────────────────────

type AcpServerConfig = AcpTcpServer | AcpStdioServer;

interface AcpTcpServer {
  /** TCP host to listen on. */
  host: string;

  /** TCP port to listen on (u16). */
  port: number;

  /** Optional API key for authentication. */
  api_key?: string;
}

/** Stdio transport — empty object with no host/port. */
interface AcpStdioServer {}

// ── Permission configuration ───────────────────────────────────────

interface PermissionConfig {
  /** Fallback action when no rule matches. Default: "ask". Also accepts the alias key "default". */
  "*"?: "allow" | "ask" | "deny";

  /** Ordered rule list (last match wins). */
  rules?: PermissionRule[];

  /** Rules for paths outside the working directory. Same shape as `rules`;
   *  `op` defaults to "*" for these entries. */
  external_directory?: PermissionRule[];

  /** Doom-loop retry behavior: "ask", "allow", or "deny". Set to "allow" to disable hard-deny. */
  doom_loop?: "allow" | "ask" | "deny";
}

interface PermissionRule {
  /** Operation class: "read", "edit", "execute", "network", "mcp", "memory", "skill", "agent", "meta", or "*" (any; alias "any"). Optional — defaults to "*". */
  op?: string;

  /** Glob pattern. Read/edit use path-style globs; execute/network/mcp use shell-style. */
  match: string;

  /** Effect: "allow", "ask", or "deny". */
  effect: "allow" | "ask" | "deny";

  /** Optional — narrow the rule to a single concrete tool name (e.g. "grep"). */
  tool?: string;
}

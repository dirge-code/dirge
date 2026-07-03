# Configuration

dirge reads an optional JSON config file named `config.json` from its config
folder:

- If `DIRGE_CONFIG_DIR` is set: `$DIRGE_CONFIG_DIR/config.json`
- Otherwise: the platform config directory joined with `dirge/config.json`
  (for example `$XDG_CONFIG_HOME/dirge/config.json` on Linux)
- Fallback: `$HOME/.config/dirge/config.json`

A project may also ship a partial `<project>/.dirge/config.json`. It is
deep-merged on top of the global file: scalar fields override, while maps
(`providers`, `mcp_servers`, `agents`, `slash_aliases`) union key-by-key, so
a project can add or override a single entry without redeclaring the whole
map. Arrays are replaced wholesale, not merged — notably `keybindings` (a
list): a project `keybindings` entry wipes the global list rather than
extending it, so redeclare the full set if you also want the global bindings. Absent keys fall through to the global file. An
empty object (e.g. `"providers": {}`) is a no-op, not a wipe — there is no
syntax to clear a global map from a project config. CLI flags and env vars
still take precedence over both files.

All config keys are optional. CLI flags and their environment-backed values
(such as `DIRGE_PROVIDER` and `DIRGE_MODEL`) take precedence where both exist.

Example:

```json
{
  "provider": "openrouter",
  "max_tokens": 8192,
  "temperature": 0.7,
  "context_window": 128000,
  "reserve_tokens": 16384,
  "keep_recent_tokens": 20000,
  "compact_enabled": true,
  "default_prompt": "code",
  "default_permission_mode": "standard",
  "show_tool_details": true,
  "show_edit_diff": true,
  "show_reasoning": false,
  "display": "left|main|right",
  "tool_result_max_chars": 500,
  "tool_result_max_lines": 4,
  "providers": {
    "openrouter": {
      "model": "deepseek/deepseek-v4-flash"
    },
    "local-vllm": {
      "provider_type": "openai",
      "base_url": "http://localhost:8000/v1",
      "api_key_env": "VLLM_API_KEY"
    }
  },
  "permission": {
    "*": "ask",
    "rules": [
      { "op": "edit",    "match": "**/*.rs",   "effect": "allow" },
      { "op": "edit",    "match": "**",        "effect": "ask"   },
      { "op": "execute", "match": "cargo test", "effect": "allow" },
      { "op": "execute", "match": "rm **",     "effect": "deny"  }
    ],
    "external_directory": [
      { "match": "/tmp/**", "effect": "allow" },
      { "match": "/**",     "effect": "ask"   }
    ],
    "doom_loop": "ask"
  }
}
```

Accepted top-level keys:

| Key                       | Type    | Description                                                                                                                                                                 |
| ------------------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `provider`                | string  | Active provider alias. Built-ins are `openrouter`, `openai`, `anthropic`, `gemini`/`google`, `deepseek`, `glm`/`zhipu`, `opencode`, and `ollama`; any alias declared in `providers` is also accepted. Default: `openrouter`. See [Providers and roles](#providers-and-roles). |
| `auth`                    | string  | Default authentication source for providers that don't set their own `providers.<name>.auth`: `api-key` (the implicit default), `chatgpt` (Codex/OpenAI login tokens), or `anthropic` / `claude-code` (Anthropic Claude Code OAuth). See [Providers and roles](#providers-and-roles). |
| `providers`               | object  | Map of provider alias → entry. The active model lives in `providers.<active-provider>.model`. Each role key below points at one of these aliases. See [Providers and roles](#providers-and-roles). |
| `review_provider`         | string  | Provider alias for the background session-review pass. Falls back to `provider`. |
| `escalation_provider`     | string  | Provider alias for the one-shot retry after repair-exhaustion / pre-write syntax failure. Falls back to `provider` (no-op when equal). |
| `summarization_provider`  | string  | Provider alias for context compaction. Falls back to `provider`; with Anthropic OAuth, configure a non-Anthropic-OAuth summarization provider for LLM compaction side calls. Reactive overflow can still use a local prune-only emergency fallback, but high-fidelity LLM summaries require this route. |
| `subagent_provider`       | string  | Provider alias for `task` tool subagents. Falls back to `provider`. |
| `critic_provider`         | string  | Provider alias for the F6 in-loop critic (tier 3). When set, the verifier escalates to a bounded LLM critique at finalization on substantive runs (one call per run); it also judges the **goal gate** (`--goal`) and powers the diff-aware **code reviewer** (reviews the run's uncommitted diff, blocks on high/critical findings, advises on medium/low — also runnable on demand via `/code-review`). **No fallback** — unset means no critic, no reviewer, no goal gate, and no cost. |
| `critic_preamble`         | string  | Optional system-preamble override for the F6 in-loop critic. Replaces the built-in critic stance for every prompt. A prompt's `critic_preamble` frontmatter overrides this per-prompt; a `critic: false` frontmatter suppresses the critic entirely for that prompt. The **goal gate** is unaffected by either — it always judges under its own fixed preamble. Unset = built-in. |
| `code_review`             | string  | How the diff-aware code reviewer engages at finalization: `off` \| `advisory` \| `blocking`. **`advisory`** (default) runs the review **in the background** after the turn finalizes and surfaces all findings (high/critical included) as one non-blocking `<system>` notice — it never holds up the turn and never re-enters the loop, so a tight debug loop isn't interrupted. **`blocking`** runs it synchronously on the finalization path: high/critical findings re-enter the loop (fix or justify), medium/low advise. **`off`** disarms it entirely (no diff capture, no judge call, zero cost). Still requires `critic_provider` (the reviewer reuses its judge); a `critic: false` prompt also suppresses it. A prompt's `code_review` frontmatter overrides this per-prompt. |
| `context_target` | integer | Working-context budget in tokens (default: `250000`). The compaction decision treats the effective window as `min(model_window, context_target)`, so models with a window below 250k use their own (e.g. 128k stays 128k) while larger models are capped. Set lower (e.g. `100000`) for stricter folding on cost-sensitive routes. Floored at 16k. |
| `compaction_fold_threshold` | float | Fraction of the (budgeted) context window (0.3–0.75) at which history folds into a summary — and the durable checkpoint is written. Lower folds/checkpoints earlier, from more coherent context. Unset keeps the 0.75 default. Composes with `context_target`: the fold point is `fraction × min(model_window, context_target)`. |
| `incremental_checkpoint`  | bool    | Refresh the durable session checkpoint in the background at 20%-of-window usage thresholds, without folding, so a resumed session recovers fresh state (adapted from [MiMo-Code](https://github.com/XiaomiMiMo/MiMo-Code)). Default `true`; set `false` to disable the background summary calls. Forced off in headless `-p`/`--loop` (nothing there persists it). |
| `agents`                  | object  | Optional user-defined [agent profiles](agents.md), keyed by name. Each is a `{ prompt, model, deny_tools/allow_tools, reasoning, temperature, description }` bundle activated at runtime with `/agent <name>`. Lowest-precedence source — `.dirge/agents/*.md` and `~/.config/dirge/agents/*.md` override same-named entries. Absent = no profiles (opt-in). |
| `max_tokens`              | integer | Maximum response tokens. Default: `8192`.                                                                                                                                   |
| `max_agent_turns`         | integer | Maximum agent turns per response. Default: `100`.                                                                                                                           |
| `temperature`             | number  | Model sampling temperature in `0.0`–`2.0`. `--temperature` CLI flag overrides this. Values outside the range are clamped with a stderr warning.                            |
| `no_tools`                | boolean | Disable all tools. Default: `false`.                                                                                                                                        |
| `no_context_files`        | boolean | Disable loading global/project `AGENTS.md` and `CLAUDE.md` context files. Default: `false`.                                                                                 |
| `context_window`          | integer | Advertised model context window — feeds the status line and the legacy token-reserve path (the compaction *fold* decision uses `context_target`, not this). Resolved as: explicit value here → built-in per-model table → `128000`. The table usually decides, e.g. Claude Sonnet/Opus 4.x, DeepSeek-V4, GPT-4.1, Gemini-2.5-Flash, and Llama-4 = 1M (Gemini-2.5-Pro = 2M); base Claude and o3 = 200k; GPT-4o, DeepSeek-R1, and older 128k models = 128k. |
| `reserve_tokens`          | integer | Tokens to reserve before compaction is triggered. Default: `16384`.                                                                                                         |
| `keep_recent_tokens`      | integer | Approximate recent-token budget kept verbatim during compaction. Default: `20000`.                                                                                          |
| `compact_enabled`         | boolean | Enable automatic conversation compaction. Default: `true`.                                                                                                                  |
| `dynamic_tool_search`     | boolean | Ship only `tool_search` + a small always-on toolset per request; the model loads more tools on demand via `tool_search(query)`. ~30% token savings on MCP-heavy sessions. Default: `false`. |
| `context_depth_reminder_threshold` | integer | Consecutive same-file tool calls before a single mid-turn reminder restates the active task + touched files. Opt-in; unset (default) disables. Recommended value: `8`. |
| `phased_workflow_enabled` | boolean | Enable the `/plan` phased workflow (explore → plan → implement → reviewer-runs-code loop). Master kill-switch — `/plan` is inert unless this is `true`. Default: `false`. See [agent-loop.md](agent-loop.md#phased-plan-workflow-plan). |
| `phased_workflow_max_review_cycles` | integer | Reviewer-runs-code fix-cycle budget for `/plan`: how many times a `NEEDS_FIX` verdict re-runs the implementer before stopping. Default: `2`. |
| `permission`              | object  | Permission rules; see the permission config notes below.                                                                                                                    |
| `restrictive`             | boolean | Select restrictive permission mode. Overridden by `accept_all`/`yolo` if those are also true.                                                                               |
| `accept_all`              | boolean | Select accept mode, equivalent to `--accept-all`. Overridden by `yolo` if true.                                                                                             |
| `yolo`                    | boolean | Select yolo mode, auto-approving all operations.                                                                                                                            |
| `sandbox`                 | bool / string / object | Sandbox bash commands. `true`/`false`, a mode string (`"off"`, `"bwrap"`, `"microvm"`), or an object — see [Sandbox configuration](#sandbox-configuration). Default: `false`. |
| `default_permission_mode` | string  | Permission mode when no mode boolean/CLI flag is set. Use `standard`, `restrictive`, `accept`, or `yolo`.                                                                   |
| `show_tool_details`       | boolean | Show tool-result output in the TUI. Default: `true`.                                                                                                                         |
| `show_edit_diff`          | boolean | Show colorized diff output for `edit` tool results (`-` red, `+` green, `@@` cyan). Default: `true`.                                                                        |
| `show_reasoning`          | boolean | Show the model's thinking/reasoning by default, instead of having to press `Ctrl+O` each turn. Default: `false`.                                                            |
| `max_sessions`            | integer | How many of the most-recent prior sessions in the same project (same working dir) to mine for Up-arrow / Ctrl+F command history, seeded ahead of the current session's prompts. Default: `3`. Set `0` to keep recall to the current session only. See [Command history](#command-history-cross-session-recall). |
| `display`                 | string  | Preferred startup pane layout: a `\|`/`,`/space-separated subset of `left`, `main`, `right` (e.g. `"main\|right"`, `"main"`). The main pane is always shown; this picks which side panels appear. Override at runtime with `/display`. Default: automatic (side panels shown at ≥152 cols). |
| `tool_result_max_chars`   | integer | Hard ceiling on characters per tool result. Default: `500`. Combined with `tool_result_max_lines` (lines applied first; chars trim what's left).                                |
| `tool_result_max_lines`   | integer | Body lines shown inside a tool chamber before collapsing to `↓ N more lines (Ctrl+O to expand)`. Default: `4`. Press `Ctrl+O` to re-print the most recent collapsed result in full. `edit`, `apply_patch`, `question`, `task`, and `task_status` are exempt (their body IS the value). |
| `default_prompt`          | string  | Prompt name to activate on startup. Default: `code`.                                                                                                                        |
| `theme`                   | string  | UI color theme. `phosphor` (default — 80s CRT green-on-black), `plain` (pre-theme white/cyan), or any `<name>.theme.json` file in the config dir. See [themes.md](themes.md). |
| `tools`                   | object  | Per-tool settings. `tools.websearch` / `tools.webfetch` (`bool`, default `true`) gate the two web tools, but only as one operand of an OR: a tool is registered when its config value is true **or** its env var (`WEBSEARCH_ENABLED` / `WEBFETCH_ENABLED`; truthy = `true` or `1`) is set — so an explicit config `false` is re-enabled by the env var, not dropped. `tools.bash_output_inline_max_bytes`, `tools.webfetch_output_inline_max_bytes`, and `tools.task_output_inline_max_bytes` (`integer`, default `8192` each) cap how much of a `bash` / `webfetch` / `task` result is returned inline versus relayed to `~/.dirge/transient/<pid>/` with a head/tail summary and a `read`-tool hint. |
| `memory`                  | object  | Long-term memory retrieval tuning. See [Hybrid memory retrieval](#hybrid-memory-retrieval) below. Absent = the builtin BM25 store. |
| `mcp_servers`             | object  | MCP server map when compiled with the `mcp` feature. When omitted, defaults to a single Exa Web Search server; see below.                                                   |
| `acp_servers`             | object  | ACP server config map when compiled with the `acp` feature. See the ACP section below.                                                                                       |

### Context window & compaction

Two keys control how large the live context grows before history is folded
into a summary (compaction):

- **`context_target`** — the working-context budget in tokens (default
  `100000`). The decision treats the effective window as `min(model_window,
  context_target)`, so dirge folds to stay within *your* budget rather than the
  model's full advertised window (whose quality degrades well before it fills).
  Floored at 16k; a value above the model's real window is a no-op.
- **`compaction_fold_threshold`** — the fraction of that budget at which the
  fold (and durable checkpoint) fires, clamped to `0.3`–`0.75` (default
  `0.75`). Lower folds earlier, from more coherent context.

The **fold point** — the size the context reaches before compaction kicks in —
is the product of the two:

```
fold_point = compaction_fold_threshold × min(model_window, context_target)
```

| Goal | `context_target` | `compaction_fold_threshold` | Folds at |
| ---- | ---------------- | --------------------------- | -------- |
| Default (200k model) | unset (100k) | unset (0.75) | ~75k |
| Smaller, tighter context | `60000` | unset (0.75) | ~45k |
| Same budget, fold earlier | unset (100k) | `0.5` | ~50k |
| Both | `80000` | `0.6` | ~48k |

```json
{
  "context_target": 80000,
  "compaction_fold_threshold": 0.6
}
```

Set `compact_enabled` to `false` to disable automatic compaction entirely.
(The separate `context_window` / `reserve_tokens` keys feed the status line and
the older token-reserve path; the budget above is what the fold decision uses.)

### Hybrid memory retrieval

By default the `memory` tool's `search` is BM25 (keyword) only — exact on
paths, error codes, and identifiers, but blind to paraphrase. Opt into hybrid
dense+BM25 retrieval to also recover semantically-related entries, fused with
Reciprocal Rank Fusion. It needs an OpenAI-compatible embeddings endpoint.

```json
{
  "memory": {
    "hybrid_retrieval": true,
    "embed_url": "https://api.openai.com/v1/embeddings",
    "embed_model": "text-embedding-3-small",
    "embed_api_key_env": "OPENAI_API_KEY"
  }
}
```

| Key                 | Type    | Description |
| ------------------- | ------- | ----------- |
| `hybrid_retrieval`  | boolean | Turn on dense+BM25 fusion. Default `false`. |
| `embed_url`         | string  | OpenAI-compatible `/v1/embeddings` endpoint. **Required** for hybrid; if unset, retrieval stays BM25. |
| `embed_model`       | string  | Embedding model id. Default `text-embedding-3-small` — set it when pointing at a non-OpenAI endpoint. |
| `embed_api_key_env` | string  | Name of the env var holding the API key (the key itself is never stored in config). Omit for a keyless local endpoint. |
| `verbatim_pre_recall` | boolean | Each turn, auto-search memory on the verbatim user message and inject the hits as a supplemental context note (separate from the frozen system-prompt snapshot — it never changes the cached prefix). Surfaces relevant memory the agent wouldn't think to look up. Works with BM25 or hybrid. Default `false`. |

Safe by default and on failure: with `hybrid_retrieval` off, or the endpoint
unset/unreachable/timed out, search silently falls back to BM25 — it never
errors. Embeddings are computed at search time (the first search of a session
embeds all active entries; later searches only embed the query) and cached for
the session.

Cost note: enabling `hybrid_retrieval` **and** `verbatim_pre_recall` together
means roughly one embeddings API call per agent turn (pre-recall searches the
verbatim message every turn, and with hybrid each search embeds the query). On
a paid endpoint that adds up over a long session; on a local endpoint it's just
latency.

## Providers and roles

Providers are declared once in the `providers` map and referenced by alias from
the role-assignment keys — so each role can run on a different model:

```json
{
  "provider": "deepseek",
  "review_provider": "glm",
  "escalation_provider": "anthropic",
  "subagent_provider": "glm",

  "providers": {
    "deepseek": {
      "model": "deepseek-v4-pro"
    },
    "glm": {
      "model": "glm-4.6"
    },
    "anthropic": {
      "model": "claude-opus-4-5"
    },
    "ollama": {
      "provider_type": "openai",
      "base_url": "http://127.0.0.1:11434/v1",
      "model": "llama3.1"
    }
  }
}
```

Each `providers` entry accepts:

| Field | Description |
|-------|-------------|
| `provider_type` | Built-in provider type to speak. Optional — defaults to the entry's alias when that alias matches a built-in name. |
| `base_url` | Endpoint base URL (for custom / self-hosted endpoints). |
| `model` | Model name for this provider. |
| `api_key` | Literal key or `${ENV_VAR}` interpolation. Takes precedence over `api_key_env`. |
| `api_key_env` | Name of the env var holding the API key. |
| `auth` | Authentication mode: `api-key` (default), `chatgpt` for Codex/OpenAI login tokens, or `anthropic` / `claude-code` for Anthropic Claude Code OAuth. |
| `allow_insecure` | Allow `http://` URLs (plaintext). Default `false`; only enable for local-only proxies. |
| `stream_chunk_timeout_secs` | Per-provider streaming chunk timeout override. |
| `options` | Free-form per-provider model options; currently honors `temperature`. |

The aliases on the left of the map become the values you write in
role-assignment keys.

### OpenAI browser / device-code auth

Run `dirge auth openai` to authorize OpenAI through the browser OAuth flow and
persist a local OAuth refresh token. Dirge prints an OpenAI authorization URL
and waits for the browser redirect on `http://localhost:1455/auth/callback`.
This is the preferred ChatGPT/Codex subscription login path.

For headless environments, run `dirge auth openai --device-code` to use the
older device-code flow. Before using that mode, enable device-code auth in
ChatGPT Codex security settings. Dirge prints the OpenAI verification URL and
user code; the user code is part of the interactive login UX, but you should
not share it with anyone.

The credential store lives in the Dirge data directory, not the repository or
program directory:

- Linux default: `~/.local/share/dirge/auth.json`
- Override: `$DIRGE_DATA_DIR/auth.json`

Successful login persists across Dirge sessions. Delete `auth.json` or revoke
the OpenAI authorization if you want to force a new login.

For the canonical `openai` provider with no configured `base_url`, a fresh
stored OAuth credential is treated as subscription auth and is preferred before
API-key billing. Explicit `auth: "chatgpt"` also uses this fresh Dirge-managed
OpenAI OAuth credential before falling back to legacy `~/.codex/auth.json`
storage, so rerunning `dirge auth openai` is enough to recover from a stale
Codex login file. OpenAI-compatible aliases and providers with a custom
`base_url` keep normal API-key behavior. If no fresh OAuth credential exists,
Dirge uses the usual API-key sources: explicit CLI keys, key files/stdin, config
`api_key`, config `api_key_env`, and provider environment variables. If the
OAuth/Codex request reports subscription quota or model-access exhaustion, Dirge
asks before switching that request to API-key billing.

Troubleshooting:

- Browser callback port is busy: stop the process using port 1455 and rerun
  `dirge auth openai`, or use `dirge auth openai --device-code` in a headless
  environment.
- `OpenAI device-code auth is not enabled` or a 404 from the user-code endpoint:
  enable device-code auth in ChatGPT Codex security settings and rerun
  `dirge auth openai --device-code`.
- Timeout: complete approval in the browser and rerun the command.
- Corrupt auth store: fix or remove `auth.json`, then rerun `dirge auth openai`.

### Anthropic Claude Code OAuth

To use a Claude Pro/Max subscription token instead of an Anthropic API key,
run:

```bash
dirge auth anthropic
```

Complete the browser login. dirge listens on `http://localhost:53692/callback`,
exchanges the PKCE code, and writes credentials to
`~/.claude/.credentials.json` in the same shape as Claude Code. Then configure
the Anthropic provider to use OAuth:

```json
{
  "provider": "anthropic",
  "providers": {
    "anthropic": {
      "auth": "anthropic",
      "model": "claude-sonnet-4-5"
    }
  }
}
```

Aliases `claude-code`, `claude_code`, and `claude` are accepted for the same
auth mode. `ANTHROPIC_OAUTH_TOKEN` can also provide a raw access token for
smoke tests, but persisted credentials are preferred because dirge can refresh
expired tokens before rebuilding the Anthropic client.

### Role assignments

| Key | Used for | Falls back to |
|-----|----------|---------------|
| `provider` | Default / main loop | (none — required) |
| `review_provider` | Background session-review pass | `provider` |
| `escalation_provider` | One-shot retry after repair-exhaustion / pre-write syntax failure | `provider` (no-op when equal) |
| `summarization_provider` | Context compaction side calls (required for LLM compaction when `provider` uses Anthropic OAuth) | `provider` when safe |
| `subagent_provider` | `task` tool subagents | `provider` |
| `critic_provider` | F6 in-loop critic (tier 3) + diff-aware code reviewer (`/code-review`) + goal-gate judge (`--goal`) | none (off) |

When a role's provider equals `provider` (either explicitly or by fallback), no
duplicate client is constructed and the feature has zero overhead — escalation
routes, for example, simply don't fire because they'd be a no-op anyway.

> **Migration note**: dirge no longer reads the legacy top-level `model`,
> `custom_providers`, or `review_model` keys — starting a session with any of
> those at the root fails fast with a migration hint. Move `model` inside the
> active provider's entry, `custom_providers.<name>` entries directly into
> `providers`, and `review_model` into the entry referenced by
> `review_provider`.

## Permissions

Permission actions are lowercase strings: `allow`, `ask`, or `deny`. `rules`
is an **ordered list** read top-to-bottom; **last match wins**. Each rule has:

- `op` — the operation class it governs (NOT a tool name). One of:
  `read`, `edit`, `execute`, `network`, `mcp`, `memory`, `skill`,
  `agent`, `meta`, or `*` (any). `edit` covers write/edit/apply_patch —
  they're one operation, so one rule governs all three.
- `match` — a glob. Read/edit use path-style globs (`*` is one path
  segment, `**` spans directories); execute/network/mcp use shell-style
  (`*` matches anything including `/`, trailing ` *` makes args optional).
  The `*` (any) op uses shell-style too, since it can match commands and
  MCP keys as well as paths. MCP patterns match the full key
  `mcp_tool:{server}:{tool}`.
- `effect` — `allow`, `ask`, or `deny`.
- `tool` *(optional)* — narrow the rule to a single concrete tool name
  (e.g. `"grep"`) instead of the whole op.

Use `"*"` for the default action, `external_directory` (also a `rules`
list, op defaults to `*`) for absolute-path rules outside the working
directory, and `doom_loop` for the retry-loop hard-deny (set to `allow`
to disable it). dirge always installs its built-in safe bash allow/deny
rules and a read-only/memory/skill/in-cwd-write allow set beneath your
rules; your `rules` override them.

MCP tools default to `ask` for ALL servers — they execute external code
(the server's implementation, plus whatever filesystem / network / API
effects it has), and silent default-allow let entire query sequences run
before any prompt fired. To re-enable silent allow for a trusted server:

```json
{
  "permission": {
    "rules": [
      { "op": "mcp", "match": "mcp_tool:lattice:*", "effect": "allow" }
    ]
  }
}
```

Or accept once at the alert and pick "allow always" for the same
session-allowlist effect.

### Mode semantics

- **`standard`** (default): every rule in `permission` is consulted; tools without
  matching rules fall back to `*` (default `allow`).
- **`restrictive`**: like `standard`, but any tool whose rule resolves to `allow`
  via the `*` fallback (no explicit allow rule matched) is converted to `ask`.
  Explicit `allow` rules still allow. Explicit `deny` rules still deny.
- **`accept`** (equivalent to `--accept-all`): auto-allows tools whose targets
  resolve inside the working directory; tools touching paths outside still
  consult `external_directory` rules.
- **`yolo`** (equivalent to `--yolo`): bypasses every check. Use with caution.

CLI precedence (high → low): `--yolo` > `--accept-all` > `--restrictive` >
`default_permission_mode` config > `standard`.

When compiled with MCP support, `mcp_servers` accepts command-based and URL-based
servers:

```json
{
  "mcp_servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "env": {}
    },
    "semantic-index": {
      "command": "my-indexer",
      "args": ["--repo", "/work/other-project"],
      "allow_external_paths": true
    },
    "remote-search": {
      "url": "https://example.com/mcp",
      "headers": {
        "authorization": "Bearer token"
      }
    }
  }
}
```

If `mcp_servers` is omitted (`null`) and the `mcp` feature is enabled, dirge
adds a default Exa Web Search MCP server at `https://mcp.exa.ai/mcp` with the
`x-api-key` header set to `EXA_API_KEY` when that environment variable is set.
Set `"mcp_servers": {}` to disable all MCP servers.

### Per-server external-path opt-in (`allow_external_paths`)

By default an MCP tool call whose JSON arguments name a path resolving outside
the working directory is refused with a clear error — matching the trust model
of dirge's built-in file tools (`read` / `write` / `edit` anchored to cwd).
The check scans top-level args fields named `path`, `file_path`, `file`,
`directory`, `dir`, `cwd`, and the `paths` array.

Some MCP servers legitimately need broader scope: a semantic indexer pointed
at a sibling repo, a project-wide search tool, a backup utility. Set
`"allow_external_paths": true` on that one server's config (both `Command` and
`Url` variants accept it; default `false`) to skip the cwd guard for tools
from THAT server only.

The flag is path-scoped and narrow:

- It only bypasses the cwd-external-path check.
- It does NOT bypass `mcp_tool` deny rules, prompt `deny_tools` frontmatter,
  doom-loop detection, the sandbox, or `--yolo`/`--restrictive` mode logic —
  every other gate runs unchanged.
- It applies per-server: enabling it on `semantic-index` does not affect
  `filesystem` or any other server in the same config.

Pair it with a tight `mcp_tool` rule for layered control, e.g.:

```json
{
  "mcp_servers": {
    "semantic-index": {
      "command": "indexer",
      "allow_external_paths": true
    }
  },
  "permission": {
    "rules": [
      { "op": "mcp", "match": "mcp_tool:semantic-index:*",          "effect": "allow" },
      { "op": "mcp", "match": "mcp_tool:semantic-index:write_file", "effect": "deny"  }
    ]
  }
}
```

### MCP tools and prompt deny-lists

Per-prompt `deny_tools` frontmatter (see "Prompt restrictions" below) applies
to MCP tools too. The deny gate matches against three names for each MCP tool
call:

- the raw tool name as exported by the MCP server (e.g. `edit`, `write_file`),
- the qualified `mcp_tool:<server>:<name>`,
- the umbrella `mcp_tool` (denies every MCP tool from every server).

So a plan-mode prompt that ships `deny_tools: [edit, write, apply_patch, bash]`
also blocks any MCP server that exports a tool named `edit` / `write` /
`apply_patch` / `bash`. Use `mcp_tool` as a blanket deny when in doubt about
what an MCP server might expose.

## Plugin trust boundary

The Janet plugin system runs INSIDE the trust boundary. Plugin hooks
(`on-tool-start`, `on-tool-end`) can mutate tool inputs, block tool calls,
and replace tool outputs with arbitrary text. They cannot, however, bypass
the permission checker (`check_perm*` runs inside the inner tool, after the
plugin pre-hook). If you load third-party plugins, treat them with the same
care you'd give to executing third-party code in your shell — the plugin's
trust level effectively equals the user's. There is no sandboxing.

### Per-plugin settings (`plugins`)

Plugins are discovered from `~/.config/dirge/plugins/` and
`./.dirge/plugins/` and load automatically. The optional `plugins` object
toggles them by name — the **directory name** (multi-file plugins) or the
**`.janet` file stem** (single-file plugins):

```json
{
  "plugins": {
    "backpressured": { "enabled": true, "auto_start": true },
    "nrepl":         { "enabled": false }
  }
}
```

| Field | Default | Effect |
|-------|---------|--------|
| `enabled` | `true` | Whether to load the plugin. `false` skips it entirely. |
| `auto_start` | `false` | Passed to the plugin via `harness/plugin-config`; a plugin that supports it self-engages at startup (e.g. `backpressured` runs its loop without the keyword). |

A plugin with no entry — or no `plugins` block at all — is **enabled and not
auto-started**, so existing setups load every plugin exactly as before.

Plugin authors: read your own settings in **load-time** code with
`(harness/plugin-config)`, which returns `@{:enabled bool :auto-start bool}`
(or `nil`). The host sets it just before your files load and clears it
after, so capture it at the top level — not from a shared hook, where it
would reflect the last plugin loaded.

## Sandbox configuration

The `sandbox` key accepts three forms:

```jsonc
// 1. boolean — false (off) or true (bubblewrap, Linux)
"sandbox": true

// 2. mode string
"sandbox": "off" | "bwrap" | "microvm"

// 3. object — required for microVM, optional for tuning
"sandbox": {
  "mode": "microvm",        // "off" | "bwrap" | "microvm"
  "image": "alpine:latest", // microVM root image (microvm mode)
  "cpus": 2,                // microVM vCPUs (1–255)
  "memory_mib": 1024        // microVM memory in MiB
}
```

- `bwrap` runs each bash command inside [bubblewrap](https://github.com/containers/bubblewrap) (Linux only; needs the `bwrap` binary). The working directory is bound read-write; the rest of the filesystem is read-only.
- `microvm` runs commands in a full microVM (requires the `sandbox-microvm` build feature). `image`, `cpus`, and `memory_mib` apply only to this mode.
- A legacy nested form `{"mode": "microvm", "microvm": {"image", "cpus", "memory_mib"}}` is still accepted; out-of-range `cpus`/`memory_mib` are now a config error rather than silently wrapping.

## Streaming timeouts

dirge applies a per-chunk read deadline to streaming LLM responses so a
silently-dropped TCP connection (which reqwest can't always detect) doesn't
freeze the agent. The default is 5 minutes (`300s`) — well above any
legitimate reasoning gap from Claude 3.7 extended thinking, GPT-5 thinking,
or large-tool-output processing. Bump it if you see false-positive
`stream chunk timed out` errors in the middle of a turn.

Resolution order (first hit wins):

1. `providers.<name>.stream_chunk_timeout_secs` — per-provider override
2. top-level `stream_chunk_timeout_secs` — applies to every provider
3. `300s` default

Provider name matching is case-insensitive (`anthropic` matches
`--provider Anthropic`).

```json
{
  "stream_chunk_timeout_secs": 300,
  "providers": {
    "anthropic": { "stream_chunk_timeout_secs": 900 },
    "ollama":    { "stream_chunk_timeout_secs": 60 },
    "my-vllm": {
      "provider_type": "openai",
      "base_url": "http://localhost:8000/v1",
      "api_key_env": "VLLM_API_KEY",
      "stream_chunk_timeout_secs": 1200
    }
  }
}
```

## Operation timeouts

Every other per-operation timeout is named in one place — the `timeouts`
block — and installed process-wide at startup. Each field is in seconds;
omitted fields keep their built-in default. (The streaming chunk timeout
above is the one exception with richer per-provider precedence;
`timeouts.stream_chunk_secs` acts as its global fallback.)

| Field | Default | What it bounds |
|---|---|---|
| `stream_chunk_secs` | 300 | Per-chunk read deadline for a streaming LLM response (fallback for the per-provider key above) |
| `request_establish_secs` | 300 | Deadline for establishing a streaming request — the connection/handshake and the wait for the first response event. Distinct from `stream_chunk_secs`, which only guards gaps *between* chunks once the stream is live; a connection that stalls during the handshake never produces a first chunk. A timeout here is retried automatically. Generous by default so it only fires on a genuine stall; lower it for faster failure or raise it if a provider legitimately delays the first response. |
| `tool_call_gap_secs` | 60 | Stall window while a tool call is mid-assembly in the stream. A timeout here is retried automatically when no response text has been emitted yet (the partial tool call is discarded and the request restarted); if text was already shown, the partial is kept to avoid a duplicated response. Raise it only if your provider legitimately pauses longer than 60s between tool-call deltas. |
| `mcp_call_secs` | 120 | Total budget for one MCP tool call, including reconnect + retry |
| `mcp_init_secs` | 10 | MCP server `initialize` handshake |
| `lsp_request_secs` | 30 | Any non-`initialize` LSP request |
| `lsp_initialize_secs` | 45 | LSP `initialize` handshake |
| `bash_secs` | 120 | Default `bash` tool timeout when the call omits one |

```json
{
  "timeouts": {
    "mcp_call_secs": 60,
    "lsp_initialize_secs": 90,
    "bash_secs": 300
  }
}
```

## Key bindings

VSCode-style overrides. `keybindings` is an array of
`{ "key": "<chord>", "command": "<command>" }`; each entry layers over the
built-in defaults, so you only list what you want to change. One array
covers BOTH the global "command" keys (scroll, chat nav, …) and the
input-editor keys (cursor motion, kill-ring, history, …) — each entry
routes to the right one by its command name.

```json
{
  "keybindings": [
    { "key": "ctrl-t",        "command": "toggle_reasoning" },
    { "key": "ctrl-shift-k",  "command": "kill_subagent" },
    { "key": "ctrl-r",        "command": "none" },
    { "key": "alt-a",         "command": "cursor_line_start" },
    { "key": "ctrl-x ctrl-s", "command": "scroll_to_top" }
  ]
}
```

- **`key`** — a chord, or a whitespace-separated *sequence* of chords for
  an emacs-style binding (e.g. `ctrl-x ctrl-s`). A chord is
  case-insensitive, `-` or `+` separated, modifiers before the key.
  Modifiers: `ctrl`, `alt` (a.k.a. `meta`/`option`), `shift`. Keys: a
  single character, `f1`–`f12`, or a named key (`enter`, `esc`, `tab`,
  `backspace`, `delete`, `insert`, `space`, `up`/`down`/`left`/`right`,
  `home`, `end`, `pageup`/`pgup`, `pagedown`/`pgdn`). Examples: `ctrl-t`,
  `pageup`, `ctrl-shift-x`, `f5`, `ctrl-x ctrl-s`.
- **`command`** — one of the global or input commands below, or **`none`**
  (also `unbind`) to disable the default binding on that chord (clears it
  from both contexts).
- Binding a command to a new chord **adds** it (the default chord still
  works unless you separately unbind it). Binding a chord that already
  has a default **replaces** it.

### Global commands

| Command | Default | Action |
|---|---|---|
| `toggle_reasoning` | `ctrl-r` | Show/hide reasoning tokens |
| `expand` | `ctrl-o` | Expand buffered thinking / reprint last collapsed tool result |
| `scroll_page_up` | `pageup` | Scroll chat up one page |
| `scroll_page_down` | `pagedown` | Scroll chat down one page |
| `scroll_to_top` | `ctrl-home` | Jump to top of chat |
| `scroll_to_bottom` | `ctrl-end` | Jump to bottom of chat |
| `next_chat` | `ctrl-n` | Next subagent chat window |
| `prev_chat` | `ctrl-p` | Previous subagent chat window |
| `close_chat` | `ctrl-x` | Close the active chat window |
| `kill_subagent` | `ctrl-k` | Kill the focused subagent |
| `drop_queue` | `alt-x` | Drop queued interjections (without cancelling the run) |
| `cycle_prompt` | `shift-tab` | Cycle the active prompt layer to the next available prompt |

### Input-editor commands

| Command | Default | Action |
|---|---|---|
| `cursor_line_start` | `ctrl-a`, `home` | Cursor to start of line |
| `cursor_line_end` | `ctrl-e`, `end` | Cursor to end of line |
| `cursor_left` | `ctrl-b`, `left` | Cursor one character left |
| `cursor_right` | `right` | Cursor one character right |
| `word_left` | `alt-b`, `alt-left` | Cursor one word left |
| `word_right` | `alt-f`, `alt-right` | Cursor one word right |
| `delete_char_back` | `ctrl-h` | Delete character before cursor |
| `delete_char_forward` | `ctrl-d` | Delete character at cursor (forward) |
| `kill_to_line_end` | `ctrl-k` | Kill to end of line |
| `kill_to_line_start` | `ctrl-u` | Kill to start of line |
| `kill_word_back` | `ctrl-w` | Kill word before cursor |
| `delete_word_back` | `alt-backspace` | Delete word before cursor |
| `delete_word_forward` | `alt-d` | Delete word after cursor |
| `yank` | `ctrl-y` | Paste from the kill-ring |
| `yank_pop` | `alt-y` | Cycle the kill-ring at the last yank |
| `history_prev` | `ctrl-p` | Previous history entry |
| `history_next` | `ctrl-n` | Next history entry |
| `reverse_search` | `ctrl-f` | Reverse-i-search over history |
| `line_up` | `up` | Up one line (then history) |
| `line_down` | `down` | Down one line (then history) |
| `undo` | `ctrl-z` | Undo the last edit |

Some chords serve both contexts (e.g. `ctrl-k` is `kill_subagent` *and*
`kill_to_line_end`, `ctrl-n` is `next_chat` *and* `history_next`). The
global command only fires in its situation — `kill_subagent` only when the
input box is empty, chat nav only with more than one chat window — so the
editor handler gets the key the rest of the time.

### Chord sequences (emacs-style)

A `key` may be a sequence like `ctrl-x ctrl-s`. After the first chord the
footer shows the pending prefix (`ctrl-x-`) and waits; the next chord
completes (or aborts) the sequence. **Esc** or **Ctrl+G** cancels a
pending prefix. By default a pending prefix waits indefinitely (emacs
style); set `"chord_timeout_ms": <n>` at the top level of the config to
auto-cancel it after `n` milliseconds of inactivity. Sequences fire for
**global commands only**; a sequence bound to an input command is rejected
with a startup warning. Binding a sequence whose first chord is also a
single-key command disables that single-key binding (the sequence wins) —
you'll see a warning.

Notes:
- **Always fixed** (never rebindable): the cancel/interrupt gesture
  **Ctrl+C / Esc** (the panic button) and intrinsic editing —
  typing a character, **Backspace**, **Delete**, **Enter** to submit,
  **Ctrl+J** (insert newline), and **Tab** completion. Binding a global
  command to one of these chords shadows the intrinsic behavior while
  active.
- Plugins can also add and override bindings; user config always wins over
  a plugin. See [plugins.md](plugins.md#keyboard-shortcuts).
- Unrecognized chords or unknown commands are skipped with a warning on
  startup; the rest of the config still loads.

## Command history (cross-session recall)

Pressing **Up** in the input box recalls previous prompts, and **Ctrl+F**
opens a reverse-i-search over them. By default the recall pool is the
*current* session's prompts. Set top-level `max_sessions` to an integer
`N` (default `3`) to additionally mine the `N` most-recent *prior*
sessions in the same project (matching `working_dir`) for their user
prompts. Those older prompts are seeded ahead of the current session's
own, so Up starts from your newest command and walks back through earlier
conversations in the project.

The scan is scoped to the same project and excludes the current
conversation's own compaction-fold rotations, so a fold doesn't double its
prompts into history. Set `"max_sessions": 0` to keep recall limited to
the current session. Synthetic turns (system-reminder wrappers, mid-turn
steering, auto-continue markers) never enter history.

## Slash-command aliases

Rename a built-in slash command, or give it a short alias, with the
top-level `slash_aliases` map. The key is what you type (with or without a
leading `/`); the value is the built-in command it runs (again with or
without a leading `/`). Arguments you type after the alias are passed
through to the target.

```json
{
  "slash_aliases": {
    "exit": "quit",
    "q": "/quit",
    "cls": "/clear"
  }
}
```

With the above, `/exit`, `/q`, and `/cls` all run `/quit` or `/clear`. The
alias is resolved once before dispatch, so it inherits its target's
behavior (e.g. an alias for `/quit` works while the agent is running).

- Aliases don't replace the built-in — both names work unless they
  collide (an alias key that matches a built-in shadows it).
- A leading `/` on either side is optional and normalized.
- A target that isn't a known built-in produces a startup warning (likely
  a typo) but is still passed through — it may resolve to a plugin
  command. Plugin-command targets can't be validated ahead of time.
- An empty alias key is ignored (with a warning); it would otherwise make
  a bare `/` run the target.
- Configured aliases are listed under `slash aliases` in `/help`.

## Environment variables

| Variable | Purpose |
|----------|---------|
| `EXA_API_KEY` | API key for the built-in `websearch` tool and the default Exa MCP server. Without this the `websearch` tool emits a startup warning and is not registered. |
| `DIRGE_WEBFETCH_ALLOW_PRIVATE` | Set to `1` (or any non-empty value) to allow `webfetch` to call private / loopback IPs. By default `webfetch` enforces SSRF protection — it refuses `localhost`, `127.x`, `10.x`, `172.16-31.x`, `192.168.x`, and link-local addresses. Override only in trusted local-dev contexts; never set this in production environments that touch attacker-influenced URLs. |
| `WEBSEARCH_ENABLED` / `WEBFETCH_ENABLED` | Force-enable the corresponding tool when not enabled via `tools.*` config. Useful in container builds where you set the toggle once via env rather than per-config-file. |

## LSP configuration

When compiled with the `lsp` feature (default-on), dirge spawns language
servers on demand to surface compile errors in tool output. The `lsp` config
key accepts three forms:

```json
// Default-on, built-in commands for rust/typescript/pyright/clojure-lsp.
{ "lsp": true }

// Off entirely. Same as the --no-lsp CLI flag.
{ "lsp": false }

// Default-on with per-server overrides.
{
  "lsp": {
    "rust": {
      "command": ["rust-analyzer"],
      "env": { "RA_LOG": "rust_analyzer=debug" },
      "initialization": { "cargo": { "buildScripts": { "enable": true } } }
    },
    "typescript": { "disabled": true }
  }
}
```

Per-server fields (all optional):

| Field            | Type             | Description |
| ---------------- | ---------------- | ----------- |
| `command`           | string[] | argv to launch the server. Replaces the built-in default. |
| `extensions`        | string[] | **Replaces** the server's built-in extension list. |
| `extend_extensions` | string[] | **Appends** to the built-in list (deduped). e.g. route `.janet` to `clojure-lsp` without re-listing clj/cljs/cljc/edn/bb. Accepts `extendExtensions` too. |
| `env`               | object   | extra env vars for the child process. |
| `initialization`    | object   | sent as `initializationOptions` in the LSP `initialize` request. |
| `disabled`          | boolean  | `true` removes the server entirely. |

Example — make `clojure-lsp` also handle Janet files (keeps the built-in Clojure extensions):

```json
{ "lsp": { "clojure-lsp": { "extend_extensions": ["janet"] } } }
```

CLI flag: `--no-lsp` (overrides the config; same effect as `lsp: false`).

### Built-in server commands

| Server id     | Default command                              |
| ------------- | -------------------------------------------- |
| `rust`        | `rust-analyzer`                              |
| `typescript`  | `typescript-language-server --stdio`         |
| `pyright`     | `pyright-langserver --stdio`                 |
| `clojure-lsp` | `clojure-lsp`                                |

Servers are spawned lazily on first file touch and cached per `(workspace_root, server_id)` pair. Concurrent agent tool calls for the same file deduplicate so dirge never races two `rust-analyzer` processes against one workspace.

### Known limitations

- The `extensions` override is currently ignored. The claimed-extensions list lives in the static `builtin_servers()` registry at `src/lsp/server.rs`. Adding new extensions today requires editing that file. Follow-up.
- v1 has four built-in servers. Additional servers can be added by extending `builtin_servers()` + `ProcessSpawner::default_commands()` in source.

## ACP (Agent Communication Protocol) configuration

When compiled with the `acp` feature, dirge can act as an ACP agent server.
The following config keys are available:

| Key           | Type    | Description                                            |
| ------------- | ------- | ------------------------------------------------------ |
| `acp_servers` | object  | Named ACP server configurations (see below)            |

dirge's ACP runs over stdio only; the `acp_host` / `acp_port`
keys that earlier docs mentioned have been removed from the CLI
and config in favor of editors driving the agent via stdio.

ACP server configs (in `acp_servers`) support two transport types:

```json
{
  "acp_servers": {
    "tcp-server": {
      "host": "127.0.0.1",
      "port": 7243,
      "api_key": "optional-key"
    }
  }
}
```

When `--acp` is passed without `--acp-host`, dirge runs in stdio mode
(the editor spawns it as a subprocess). With `--acp-host`, it listens on TCP.

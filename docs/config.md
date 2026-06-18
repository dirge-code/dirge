# Configuration

dirge reads an optional JSON config file named `config.json` from its config
folder:

- If `DIRGE_CONFIG_DIR` is set: `$DIRGE_CONFIG_DIR/config.json`
- Otherwise: the platform config directory joined with `dirge/config.json`
  (for example `$XDG_CONFIG_HOME/dirge/config.json` on Linux)
- Fallback: `$HOME/.config/dirge/config.json`

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
| `provider`                | string  | Active provider alias. Built-ins are `openrouter`, `openai`, `anthropic`, `gemini`/`google`, `deepseek`, `glm`/`zhipu`, and `ollama`; any alias declared in `providers` is also accepted. Default: `openrouter`. See [Providers and roles](#providers-and-roles). |
| `providers`               | object  | Map of provider alias → entry. The active model lives in `providers.<active-provider>.model`. Each role key below points at one of these aliases. See [Providers and roles](#providers-and-roles). |
| `review_provider`         | string  | Provider alias for the background session-review pass. Falls back to `provider`. |
| `escalation_provider`     | string  | Provider alias for the one-shot retry after repair-exhaustion / pre-write syntax failure. Falls back to `provider` (no-op when equal). |
| `summarization_provider`  | string  | Provider alias for context compaction. Falls back to `provider`. |
| `subagent_provider`       | string  | Provider alias for `task` tool subagents. Falls back to `provider`. |
| `critic_provider`         | string  | Provider alias for the F6 in-loop critic (tier 3). When set, the verifier escalates to a bounded LLM critique at finalization on substantive runs (one call per run), and it also serves as the judge for the **goal gate** (`--goal`). **No fallback** — unset means no critic, no goal gate, and no cost. |
| `context_target` | integer | Working-context budget in tokens (default `100000`). The compaction decision treats the effective window as `min(model_window, context_target)`, so the live context is folded — and project memory formed — to stay within the budget instead of trusting a model's full advertised window, whose effective quality degrades well before it fills (the "smart zone" runs out around 100k regardless of size — see [Chroma context-rot research](https://garrit.xyz/posts/2026-05-06-dont-trust-large-context-windows)). Floored at 16k; a value above the model's real window is a no-op. With the default fold fraction the live context stays around 75% of the budget. |
| `compaction_fold_threshold` | float | Fraction of the (budgeted) context window (0.3–0.75) at which history folds into a summary — and the durable checkpoint is written. Lower folds/checkpoints earlier, from more coherent context. Unset keeps the 0.75 default. Composes with `context_target`: the fold point is `fraction × min(model_window, context_target)`. |
| `incremental_checkpoint`  | bool    | Refresh the durable session checkpoint in the background at 20%-of-window usage thresholds, without folding, so a resumed session recovers fresh state (adapted from [MiMo-Code](https://github.com/XiaomiMiMo/MiMo-Code)). Default `true`; set `false` to disable the background summary calls. Forced off in headless `-p`/`--loop` (nothing there persists it). |
| `agents`                  | object  | Optional user-defined [agent profiles](agents.md), keyed by name. Each is a `{ prompt, model, deny_tools/allow_tools, reasoning, temperature, description }` bundle activated at runtime with `/agent <name>`. Lowest-precedence source — `.dirge/agents/*.md` and `~/.config/dirge/agents/*.md` override same-named entries. Absent = no profiles (opt-in). |
| `max_tokens`              | integer | Maximum response tokens. Default: `8192`.                                                                                                                                   |
| `max_agent_turns`         | integer | Maximum agent turns per response. Default: `100`.                                                                                                                           |
| `temperature`             | number  | Model sampling temperature in `0.0`–`2.0`. `--temperature` CLI flag overrides this. Values outside the range are clamped with a stderr warning.                            |
| `no_tools`                | boolean | Disable all tools. Default: `false`.                                                                                                                                        |
| `no_context_files`        | boolean | Disable loading global/project `AGENTS.md` and `CLAUDE.md` context files. Default: `false`.                                                                                 |
| `context_window`          | integer | Session context-window size used for status and auto-compaction. Default: `128000`.                                                                                         |
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
| `display`                 | string  | Preferred startup pane layout: a `\|`/`,`/space-separated subset of `left`, `main`, `right` (e.g. `"main\|right"`, `"main"`). The main pane is always shown; this picks which side panels appear. Override at runtime with `/display`. Default: automatic (side panels shown at ≥152 cols). |
| `tool_result_max_chars`   | integer | Hard ceiling on characters per tool result. Default: `500`. Combined with `tool_result_max_lines` (lines applied first; chars trim what's left).                                |
| `tool_result_max_lines`   | integer | Body lines shown inside a tool chamber before collapsing to `↓ N more lines (Ctrl+O to expand)`. Default: `4`. Press `Ctrl+O` to re-print the most recent collapsed result in full. `edit`, `apply_patch`, `question`, `task`, and `task_status` are exempt (their body IS the value). |
| `default_prompt`          | string  | Prompt name to activate on startup. Default: `code`.                                                                                                                        |
| `theme`                   | string  | UI color theme. `phosphor` (default — 80s CRT green-on-black), `plain` (pre-theme white/cyan), or any `<name>.theme.json` file in the config dir. See [themes.md](themes.md). |
| `tools`                   | object  | Optional per-tool enable map. Currently honors `tools.websearch` and `tools.webfetch` (both `bool`, default `true`); set either to `false` to drop the tool from the registered set even when its env vars are present. |
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
| `allow_insecure` | Allow `http://` URLs (plaintext). Default `false`; only enable for local-only proxies. |
| `stream_chunk_timeout_secs` | Per-provider streaming chunk timeout override. |
| `options` | Free-form per-provider model options; currently honors `temperature`. |

The aliases on the left of the map become the values you write in
role-assignment keys.

### Role assignments

| Key | Used for | Falls back to |
|-----|----------|---------------|
| `provider` | Default / main loop | (none — required) |
| `review_provider` | Background session-review pass | `provider` |
| `escalation_provider` | One-shot retry after repair-exhaustion / pre-write syntax failure | `provider` (no-op when equal) |
| `summarization_provider` | Context compaction | `provider` |
| `subagent_provider` | `task` tool subagents | `provider` |
| `critic_provider` | F6 in-loop critic (tier 3) + goal-gate judge (`--goal`) | none (off) |

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
| `tool_call_gap_secs` | 30 | Stall window while a tool call is mid-assembly in the stream |
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

VSCode-style overrides for the global "command" keys. `keybindings` is an
array of `{ "key": "<chord>", "command": "<command>" }`; each entry is
layered over the built-in defaults, so you only list what you want to
change.

```json
{
  "keybindings": [
    { "key": "ctrl-t",       "command": "toggle_reasoning" },
    { "key": "ctrl-shift-k", "command": "kill_subagent" },
    { "key": "ctrl-r",       "command": "none" }
  ]
}
```

- **`key`** — a chord: case-insensitive, `-` or `+` separated, modifiers
  before the key. Modifiers: `ctrl`, `alt` (a.k.a. `meta`/`option`),
  `shift`. Keys: a single character, `f1`–`f12`, or a named key
  (`enter`, `esc`, `tab`, `backspace`, `delete`, `insert`, `space`,
  `up`/`down`/`left`/`right`, `home`, `end`, `pageup`/`pgup`,
  `pagedown`/`pgdn`). Examples: `ctrl-t`, `pageup`, `ctrl-shift-x`, `f5`.
- **`command`** — one of the rebindable commands below, or **`none`**
  (also `unbind`) to disable the default binding on that chord.
- Binding a command to a new chord **adds** it (the default chord still
  works unless you separately unbind it). Binding a chord that already
  has a default **replaces** it.

| Command | Default | Action |
|---|---|---|
| `toggle_reasoning` | `ctrl-r` | Show/hide reasoning tokens |
| `scroll_page_up` | `pageup` | Scroll chat up one page |
| `scroll_page_down` | `pagedown` | Scroll chat down one page |
| `scroll_to_top` | `home` | Jump to top of chat |
| `scroll_to_bottom` | `end` | Jump to bottom of chat |
| `next_chat` | `ctrl-n` | Next subagent chat window |
| `prev_chat` | `ctrl-p` | Previous subagent chat window |
| `close_chat` | `ctrl-x` | Close the active chat window |
| `kill_subagent` | `ctrl-k` | Kill the focused subagent |

Notes:
- **Not rebindable** (kept fixed): the text-editing keys in the input box
  (Ctrl+A/E/W, kill-ring, word motion, history) and the universal
  cancel/interrupt gesture (**Ctrl+C / Ctrl+D / Esc**) — the latter must
  always be available as the panic button.
- Rebinding a command to a key the input editor uses (e.g. `ctrl-a`)
  shadows that editing key while the binding is active.
- Unrecognized chords or unknown commands are skipped with a warning on
  startup; the rest of the config still loads.

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

# Plugins

dirge embeds [Janet](https://janet-lang.org) so plugins can hook the agent
loop, gate or rewrite tool calls, register slash commands and custom
tools, drive session navigation, and surface dialogs. A plugin is a Janet
script; the host loads it, then dispatches events to top-level functions
with conventional names.

Requires building with `--features plugin`. The default `cargo install`
includes it; verify with `dirge --version`.

## Setup

dirge auto-loads plugins from these directories at startup:

| Path | Scope |
|------|-------|
| `~/.config/dirge/plugins/` (or `$XDG_CONFIG_HOME/dirge/plugins/`) | Global, applies to every project |
| `./.dirge/plugins/` (relative to cwd) | Project-local, loaded after globals so it wins on name collision |

A plugin is **either**:

- A single `*.janet` file. The stem becomes the namespace.
- A directory of `*.janet` files. The directory name is the namespace;
  every file inside loads into the *same* Janet environment in
  lexicographic order. Use `00-`, `01-` prefixes to control load order
  when one file depends on another.

No manifest, no entry point. Anything the file does at load time
(registering renderers, commands, providers, tools) takes effect
immediately. Hook functions are discovered by name.

```janet
# ~/.config/dirge/plugins/hello.janet
(defn on-prompt [ctx]
  (harness/notify (string "user said: " (ctx :prompt)) :info))
```

You can name a hook either bare (`on-prompt`) or namespaced
(`my-plugin-on-prompt`). The host scans the shared environment and finds
both. Multiple plugins can register the same hook; they each run in load
order.

## Hooks

Every hook receives a single `ctx` table. Return values are either
ignored or used by the host as noted.

| Hook | When it fires | What it can return |
|------|---------------|--------------------|
| `on-init` | Once at session start, after config and agent are ready. `ctx` = `{:model :cwd :provider}` | Ignored |
| `on-prompt` | After the user submits a message, before the LLM call. `ctx` = `{:prompt}` | Optional string appended to the system prompt for this turn. Use `harness/replace-prompt` to overwrite the user message itself |
| `on-response` | After the agent finishes a multi-turn response. `ctx` = `{:response}` | Ignored |
| `on-tool-start` | Before a tool runs (built-in or MCP), after permission checks. `ctx` = `{:tool :args}` | Ignored. Use `harness/block` / `harness/mutate-input` |
| `on-tool-end` | After the tool returns (or errors). `ctx` = `{:tool :output}` | Ignored. Use `harness/replace-result` |
| `on-error` | A tool or LLM call raised an error. `ctx` = `{:error}` | Ignored |
| `on-complete` | The agent finished its full run | Ignored |
| `on-turn-start` | Start of one LLM call within a run. `ctx` = `{:index}` | Ignored |
| `on-message-update` | Every ~16 streamed tokens during a turn. `ctx` = `{:index :partial}` | Ignored |
| `on-turn-end` | After this turn's tool results return. `ctx` = `{:index :message}` | Ignored |
| `prepare-next-run` | Between completed run and the next prompt. Place to call `harness/set-next-model`, `harness/add-steering`, `harness/add-followup` | Ignored |
| `before-agent-start` | Once before the agent starts, with the assembled system prompt. `ctx` = `{:system-prompt}` | Ignored. Use `harness/append-system-prompt` to add to the preamble (append-only) |
| `transform-context` | Before every LLM call, with the current messages. `ctx` = `{:messages}` (JSON array string) | Ignored. Use `harness/replace-context` to prune/inject for that call (transcript unchanged) |
| `message-end` | After the assistant message finalizes, before it is stored. `ctx` = `{:message}` | Ignored. Use `harness/rewrite-message` to rewrite the stored/persisted text |
| `on-before-compact` | Before a compaction fold. `ctx` = `{:message-count :tokens}` | Ignored — **observe-only, cannot cancel** (cancelling an emergency fold would overflow the context) |
| `on-compact` | When summarizing the middle slice during a fold. `ctx` = `{:messages}` (JSON array string) | Ignored. Use `harness/set-compact-summary` to supply a summary instead of the LLM (validated; invalid falls through) |

### Dispatch rules

- `on-prompt` fires once per user message; `on-turn-start` fires once
  per LLM call (a single prompt can produce many turns).
- `on-tool-start` runs *after* permission checks. If the user denied the
  tool, neither it nor `on-tool-end` fires.
- `on-tool-end` fires even when the inner tool errored, so a plugin can
  substitute a recovery output via `harness/replace-result`.
- Subagents (the `task` tool) run isolated: no tool access, no plugin
  hooks. `on-tool-start` / `on-tool-end` do not fire for anything inside
  a subagent.
- **Multi-plugin `harness/block` is first-wins.** When two plugins
  register `on-tool-start` and one calls `(harness/block reason)`,
  dispatch stops. Subsequent plugins do not run for that tool call.
- **`harness/mutate-input` and `harness/replace-result` chain
  last-write-wins.** Each plugin sees what the previous wrote and may
  refine or overwrite it.

## `harness/*` API

All `harness/*` symbols are preloaded; you can call them from any plugin
file without imports.

### Logging and context

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/log` | `(msg)` | Writes to dirge's log file (visible with `dirge --verbose`). Not shown in chat |
| `harness/get-cwd` | `()` | Returns the agent's working directory |
| `harness/has-symbol?` | `(name)` | True if `name` is bound in the Janet env |

### Prompt control

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/replace-prompt` | `(text)` | Rewrites the current user message before the LLM sees it. Meaningful only in `on-prompt` |
| `harness/request-prompt` | `(text)` | Queues a follow-up prompt to run as a fresh turn after the current one |
| `harness/store-response` | `(text)` | Sets the `harness-response` binding so the next `on-prompt` can read the prior assistant message. The host calls this automatically after every turn; plugins normally only read `harness-response` |

### System prompt, context, message & compaction control

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/append-system-prompt` | `(text)` | Appends `text` to the assembled system prompt. Meaningful only in `before-agent-start`. Append-only — the model-identity + tool-docs preamble is preserved. Multiple calls in one hook concatenate |
| `harness/replace-context` | `(json-array)` | Replaces the message array for the next LLM call with a parsed JSON array. Meaningful only in `transform-context`. Affects that one call; the persisted transcript is unchanged. Malformed JSON is ignored (original context kept) |
| `harness/rewrite-message` | `(text)` | Replaces the finalized assistant text before it is stored/persisted. Meaningful only in `message-end`. The text already streamed to screen; this rewrites stored history (e.g. redaction) |
| `harness/set-compact-summary` | `(text)` | Supplies a compaction summary used instead of the LLM summarizer. Meaningful only in `on-compact`. Validated like any summary; an invalid value falls through to the LLM |

### Tool interception

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/block` | `(reason)` | Tool is not executed. The LLM sees `reason` as the tool error. Stops further `on-tool-start` plugins for this call |
| `harness/mutate-input` | `(json-str)` | Tool runs with the rewritten args. Pass a JSON string; the host re-parses it |
| `harness/replace-result` | `(text)` | The actual tool output is discarded; the LLM sees `text` |

### Run-boundary control

Call these from `prepare-next-run` (or `on-tool-end` for thinking level)
to influence the next run.

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/set-next-model` | `(name)` | Switches the model for the next run |
| `harness/set-next-thinking-level` | `(level)` | One of `"none"`, `"low"`, `"medium"`, `"high"` |
| `harness/request-stop-after-turn` | `()` | Asks the loop to stop after the current turn finishes |
| `harness/add-steering` | `(content)` | Injects a user message at the START of the next run |
| `harness/add-followup` | `(content)` | Adds a turn AFTER the current run completes |

### Notifications and entries

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/notify` | `(msg &opt level)` | One-shot chat line. `level` is `:info` (default), `:warn`, or `:error`. Not persisted |
| `harness/append-entry` | `(type data &opt display)` | Records a typed timeline entry that survives save/load. `display` defaults to `true`; pass `false` for plugin state that should round-trip but not show |

### Renderers (session entries)

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/register-renderer` | `(type fn-name)` | Associates an entry type with a Janet function (by name). The function receives the entry's `data` string |
| `harness/render` | `(color text)` | Inside a renderer, emits one chat line. Colors: `cyan`, `red`, `yellow`, `green`, `blue`, `magenta`, `white`, `black`, `grey` (alias `darkgrey`), plus `dark*` variants. Keyword forms accepted |

If no renderer is registered for an entry's type, the host dumps the raw
`data` in dim grey.

### Custom messages (live UI)

`LoopMessage::Custom` events flow through chat but never reach the LLM —
they are UI-only. Without a registered message renderer the UI prints
the content verbatim.

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/add-custom-message` | `(content)` / `(type content)` / `(type content display)` | Pushes a custom UI message. `display=false` suppresses the chat row |
| `harness/register-message-renderer` | `(type fn-name)` | Registers a renderer for a custom message `type`. The handler receives the full wrapper JSON (`{role customType content display}`) and returns the line to display |

### Slash commands

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/register-command` | `(name fn-name)` | Typing `/name arg-string` calls `(fn-name "arg-string")`. The return string is displayed; return `nil` for silence |

The handler runs on the Janet worker; long-running handlers stall the
agent.

### Custom tools

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/register-tool` | `(name description label parameters handler &opt execution-mode prepare-arguments)` | Registers an LLM-visible tool. Collisions with built-ins drop the plugin tool with a warning |
| `harness/emit-tool-progress` | `(text)` | Inside a tool handler, pushes a streaming progress update tagged with the current tool-call id |

Arguments to `harness/register-tool`:

- `name` — the LLM-visible tool name.
- `description` — shown to the LLM; state when and how to use it.
- `label` — UI banner. Falls back to `name` when empty.
- `parameters` — JSON-schema string. Invalid JSON falls back to `{}`.
- `handler` — name of a Janet function called as `(handler args-json)`.
  Returns a string or any value `string` can render.
- `execution-mode` — `:parallel` (default, read-only) or `:sequential`
  (mutating). One sequential tool forces the whole batch sequential.
  Pass `nil` when you want only `prepare-arguments`.
- `prepare-arguments` — optional Janet function name that normalizes the
  raw args JSON before schema validation. Errors or non-string returns
  fall back to the original args. Runs synchronously; keep it light.

Inside a tool handler, the binding `harness-current-tool-call` holds the
LLM-assigned tool-call id (`nil` outside handlers).

### Keyboard shortcuts

A plugin can **add** a shortcut that runs its own code, and **override** a
built-in binding (remap or unbind it).

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/register-shortcut` | `(keys handler &opt description)` | Bind a key to plugin CODE. The handler receives the matched key spec as its sole string arg; returning a non-nil string surfaces as a chat line |
| `harness/bind-key` | `(keys command)` | Bind a key (or chord sequence) to a BUILT-IN command name, or `"none"` to unbind a default |

Use `register-shortcut` to run a Janet function on a key; use `bind-key`
to remap one of dirge's built-in commands. `bind-key`'s `command` is any
name from the global or input-editor tables in
[config.md](config.md#key-bindings) (e.g. `scroll_to_top`,
`cursor_line_start`), and `keys` may be an emacs-style sequence like
`"ctrl-x ctrl-s"`.

Key spec grammar (case-insensitive): `(modifier "-")* key-name`, with one
or more chords separated by whitespace for a sequence. Modifiers: `ctrl`,
`control`, `alt`, `meta`, `shift`. Key names: a single character,
`f1`..`f12`, or one of `enter`, `esc`, `tab`, `backspace`, `space`, `up`,
`down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `delete`,
`insert`.

**Precedence:** built-in defaults < plugin `bind-key` < the user's
`keybindings` config — the user always wins, so a `bind-key` is a default a
user can still override. `register-shortcut` handlers dispatch after the
built-in global commands but before the text input.

Reserved keys neither form can override: Ctrl+C, Esc (the panic
gesture), and the search / rewind picker keys. Modifier matching is exact —
`ctrl-x` and `ctrl-shift-x` are distinct bindings. Bad specs are dropped
with a `tracing::warn`.

```janet
# Remap a built-in: scroll to top with an emacs sequence, and disable the
# default Ctrl+R reasoning toggle.
(harness/bind-key "ctrl-x ctrl-t" "scroll_to_top")
(harness/bind-key "ctrl-r" "none")

# Bind a key to your own code.
(defn my-handler [key] (string "you pressed " key))
(harness/register-shortcut "f8" "my-handler" "Say hi")
```

### Dialogs

These block the Janet worker until the UI thread returns. Safe from any
hook or command.

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/confirm` | `(title question)` | Returns `true` on confirm, `false` on Cancel/Esc |
| `harness/select` | `(title options)` | Shows a picker; returns the chosen string or `nil` on cancel |

### Language servers (LSP)

Query the running language servers from a plugin. Like dialogs these
block the Janet worker until the async query returns, so they are safe to
call from any hook or command.

Feature-detect with `(harness/lsp?)` and fall back gracefully. The
predicate is **true only when LSP is both compiled in and active at
runtime** — so when it returns true, a following `harness/lsp` call is
guaranteed to reach a server (returning a JSON string), never a silent
`nil`. When LSP is unavailable (not built, or disabled in config) the
predicate is false and every query returns `nil`.

Positions are **1-based** line/column (matching the `lsp` tool and most
editors); passing `0`, a negative, or a non-number raises a Janet error
(it's a plugin bug, not a no-op). The result is a **JSON string** of the
underlying LSP response (parse with `(json/decode result)`); bad-request
and unknown-op errors come back as `{"error": "..."}` rather than raising.

A query is bounded by a 30-second timeout: if a language server is wedged
or unusually slow, the call returns `nil` instead of freezing the plugin.

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/lsp?` | `()` | `true` when the LSP bridge is built in **and** wired to a live server manager, else `false` |
| `harness/lsp` | `(op file &opt line char query)` | Generic query. `op` ∈ `"definition"`, `"references"`, `"hover"`, `"documentSymbol"`, `"workspaceSymbol"`, `"implementation"`, `"incomingCalls"`, `"outgoingCalls"`, `"diagnostics"` (camelCase aliases `goToDefinition`/`findReferences`/`goToImplementation` also accepted). Returns a JSON string or `nil` |
| `harness/lsp-definition` | `(file line char)` | Go-to-definition at the position |
| `harness/lsp-references` | `(file line char)` | All references to the symbol at the position |
| `harness/lsp-hover` | `(file line char)` | Hover (type/doc) for the symbol |
| `harness/lsp-implementation` | `(file line char)` | Implementations of the symbol |
| `harness/lsp-incoming-calls` | `(file line char)` | Call-hierarchy callers of the symbol |
| `harness/lsp-outgoing-calls` | `(file line char)` | Call-hierarchy callees of the symbol |
| `harness/lsp-document-symbols` | `(file)` | Symbol outline for the whole file |
| `harness/lsp-workspace-symbols` | `(file query)` | Workspace symbol search (`file` anchors the server set) |
| `harness/lsp-diagnostics` | `(file)` | Currently published diagnostics for the file (does not wait for fresh ones) |

```janet
(when (harness/lsp?)
  (def defs (json/decode (harness/lsp-definition "src/main.rs" 42 7)))
  (harness/notify (string "definition sites: " (length defs))))
```

### Custom LLM providers

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/register-provider` | `(name type base-url &opt api-key-env)` | Registers an OpenAI-compatible (or rig-supported) endpoint. `type` is e.g. `"openai"`. After registration, `/model name/<model-id>` switches to it. Config-declared providers win on collision |

### Session tree

The session is stored as a node tree. These ops queue on a per-session
buffer; the host applies them between UI events. No synchronous return.

| Function | Signature | Effect |
|----------|-----------|--------|
| `harness/set-label` | `(id label-or-nil)` | Sets or clears a node label. Visible in `/tree` |
| `harness/fork` | `(id &opt position)` | Branches off the entry. Default `:before` (entry's parent becomes leaf, text restored to editor); `:at` makes the entry itself the leaf with no restore |
| `harness/navigate-tree` | `(id)` | Moves the active leaf to `id`. Role-aware: user messages behave like `fork :before`, others become the new leaf directly |
| `harness/new-session` | `(&opt parent-session)` | Persists the current session and starts a fresh one in place |
| `harness/switch-session` | `(prefix)` | Loads a saved session by id prefix; persists the current one first |

## Example

A plugin that warns when `bash` runs `rm`, with a confirmation dialog,
and times every turn.

```janet
# ~/.config/dirge/plugins/safety.janet

(var turn-start 0)

(defn on-turn-start [ctx]
  (set turn-start (os/time)))

(defn on-turn-end [ctx]
  (harness/notify
    (string "turn " (ctx :index) " took " (- (os/time) turn-start) "s")
    :info))

(defn on-tool-start [ctx]
  (when (= (ctx :tool) "bash")
    (let [cmd (get-in ctx [:args "command"])]
      (when (string/find "rm" cmd)
        (unless (harness/confirm "Confirm" (string "Run: " cmd "?"))
          (harness/block "user denied rm"))))))
```

## Debugging

- Janet errors in a hook are caught. The error appears in TWO places:
  a red `[plugin] hook <hook>.<fn> errored: <message>` notification in
  chat, and a `tracing::warn` with target `dirge::plugin`. The hook's
  return value is treated as `nil` and dispatch continues.
- Run `dirge --verbose` (or `RUST_LOG=dirge::plugin=warn`) to see the
  structured log including Janet stack lines.
- `harness/log` writes to the same log stream. Use it for ad-hoc
  breadcrumbs.
- `harness/notify` is the easiest "did this code run?" probe — it lights
  up the chat without polluting the LLM context.
- Hook not firing? Check the function name exactly — `on_prompt`
  (underscore) is a different symbol than `on-prompt`.
- Plugins not loading at all? `dirge --version` must list `plugin` in
  the feature list.

### Threading caveats

Janet runs on a single dedicated worker thread.

- Hooks are serialized; no in-Janet races.
- Long-running Janet code blocks every subsequent hook, tool, and
  dialog. Defer heavy work via `harness/add-followup` or
  `harness/request-prompt`.
- Plugin tools cannot be preempted mid-evaluation. When the user
  cancels, an in-flight handler runs to completion in the background;
  its result is discarded but it holds the plugin lock until it
  returns. Keep handlers bounded.
- The blocking bridges (`harness/confirm`, `harness/select`,
  `harness/lsp`) are answered by the host's async runtime, which only
  makes progress once startup finishes. Call them from hooks or
  commands, **not** at plugin load time (top level): a load-time call
  can't be serviced while the loader is blocked, so it falls back
  (LSP times out to `nil` after 30s; dialogs in a headless run return
  the auto-confirm default). Hooks and commands run after startup, so
  there they work normally.
- No hot reload. Restart dirge to pick up plugin changes.

### Common gotchas

- `ctx` keys are keywords: `(get ctx :tool)` works, `(get ctx "tool")`
  does not.
- `harness/block` only takes effect inside `on-tool-start`. Calling it
  from a slash command does nothing.

## Divergences from pi

dirge's plugin surface is modeled on pi's extension API but differs
in a few deliberate ways (dirge-2n4r):

- **Steering / follow-up are push-only.** Plugins call
  `harness/add-steering` / `harness/add-followup` to queue messages.
  There is no pull-style `get-steering-messages` / `get-followup-messages`
  hook a plugin can *define* — a plugin defining those names is not
  dispatched. Use the `harness/add-*` calls from any hook instead.
- **`harness/register-provider` covers base-URL / type override only.**
  Unlike pi's `registerProvider`, it does not support custom model
  lists, OAuth flows, or custom stream handlers. It's for pointing an
  existing provider type at a different endpoint (proxy / local LLM).
- **Model swaps are run-boundary, not mid-run.** `harness/set-next-model`
  takes effect on the next run (the agent is rebuilt at the run
  boundary). A mid-run, between-turns model swap is not wired — the
  request is applied at the next run boundary. (`harness/set-next-thinking-level`,
  by contrast, DOES apply between turns within a run.)
- **The plugin runtime is opt-in at build time.** It requires the
  `plugin` Cargo feature (Janet runtime). The project `build.sh`
  enables it by default, but a bare `cargo build` compiles the plugin
  layer to no-op stubs — so a plugin-less build is valid and runs with
  zero Janet dependency. If you build dirge yourself and want plugins,
  build with `--features plugin` (or use `build.sh`).

## Reference plugins

In [`plugins/`](../plugins/):

- `hello_cmd.janet` — minimal slash command.
- `notify_example.janet` — `harness/notify` from a hook.
- `prefix_lang.janet` — `harness/replace-prompt` to rewrite user input.
- `protected_paths.janet` — `harness/block` to gate `bash` and `write`.
- `confirm_destructive.janet` — adds `harness/confirm` to the gate.
- `select_persona.janet` — `harness/select` plus a slash command.
- `bookmark.janet` — `harness/append-entry` with a custom renderer.
- `example_tool.janet` — `harness/register-tool` end-to-end.
- `example_shortcut.janet` — `harness/register-shortcut`.
- `example_bind_key.janet` — `harness/bind-key` (remap built-in commands).
- `example_message_renderer.janet` — `harness/register-message-renderer`.
- `turn_timing.janet` — `on-turn-start` / `on-turn-end` for telemetry.
- `local_openai.janet` — `harness/register-provider` for a local LLM.
- `session_tree.janet` — `harness/set-label` and `harness/new-session`.
- `workflow.janet` — multi-phase inversion of control.
- `plan_search.janet` — `/plan <task>` runs a task with diverse natural-language planning before code (PlanSearch, arXiv:2409.03733), via `harness/request-prompt`.
- `turn_timer/` — a multi-file plugin sharing state across files.
- `response_inspector.janet`, `test_plugin.janet` — smaller probes.

# Clojure addons

dirge can host addons written in Clojure. They run on an embedded
[clojurust](https://github.com/BuddhiLW/clojurust) interpreter, beside Janet
plugins, and implement hive-addon's `IAddon` protocol (`hive-addon.protocol`,
MIT). dirge embeds that namespace unchanged, so an addon needs no protocol
library on its source path. Written as portable `.cljc`, the same addon also
runs in any JVM host of that protocol.

Build with the feature enabled:

```bash
cargo build --release --features addons
```

## Installing an addon

An addon is a directory laid out like this:

```text
my-addon/
  deps.edn                              ; optional, see below
  src/my_addon/core.cljc
  resources/META-INF/addons/my-addon.edn
```

dirge searches for `META-INF/addons/*.edn` and `META-INF/hive-addons/*.edn`
(hive-addon's own layout) manifests under:

1. `<project>/.dirge/addons/`
2. `~/.config/dirge/addons/`
3. every directory listed in `addons.paths` in `config.json`

A symlink to an addon checkout works in any of them.

The addon's `src/` and `resources/` go on the interpreter's source path, plus
the `src/` of every `:local/root` dependency named in its `deps.edn`
(followed transitively). Anything else can be added with `addons.source_paths`
or the `DIRGE_ADDON_PATH` environment variable (a `:`-separated list). An
addon on the embedded `hive-addon.protocol` needs nothing more; any other
protocol library's source must be reachable one of these ways.

Manifests in another directory under `META-INF` are read when that directory
name is listed in `addons.manifest_dirs`.

A manifest whose init namespace has no `.cljc` or `.cljrs` source on that path
is skipped, so JVM-only addons can share a repository with portable ones.

```json
{
  "addons": {
    "enabled": true,
    "paths": ["~/src/my-addons"],
    "manifest_dirs": ["other-host-addons"],
    "source_paths": ["~/src/addon-protocol/src"]
  }
}
```

## The protocol

dirge binds one protocol namespace per run, chosen in this order:

1. `addons.protocol_ns` in `config.json`
2. the `:addon/protocol-ns` a manifest declares (the first one found, when
   manifests disagree; the others are logged)
3. `hive-addon.protocol`, embedded in dirge byte for byte from hive-addon

```clojure
(ns my-addon.core
  (:require [hive-addon.protocol :as p]))

(defrecord MyAddon []
  p/IAddon
  (addon-id [_] "my.addon")
  (initialize! [_ _] {:success? true :errors []})
  (shutdown! [_] {:success? true})
  (tools [_] [])
  (hooks [_] {})
  (health [_] {:status :ok}))
```

dirge resolves these functions from the bound namespace at startup and
refuses to start the host if a required one is missing:

| Function | Required | Meaning |
|---|---|---|
| `(addon? x)` | yes | whether `x` implements the protocol |
| `(initialize! addon config)` | yes | start; returns `{:success? bool :errors [...]}` |
| `(shutdown! addon)` | yes | release resources |
| `(tools addon)` | yes | tool definitions, see below |
| `(hooks addon)` | no | map of hook key to function, see below |
| `(health addon)` | no | `{:status :ok}` or similar |

## The manifest

```edn
{:addon/id      "my.addon"
 :addon/init-ns "my-addon.core"
 :addon/init-fn "addon-ctor"
 :addon/config  {}}
```

`:addon/protocol-ns` is optional: it names the protocol namespace the addon
implements when that is not `hive-addon.protocol`.

`init-fn` is called with `:addon/config` and must return an addon. dirge then
calls `initialize!` with `{:addon/id … :addon/config … :dirge/host {…}}`.

## Tools

Every entry of `(tools addon)` becomes a tool the model can call:

```clojure
{:name        "count-rows"
 :description "Counts the rows it is handed"
 :inputSchema {:type "object" :properties {:rows {:type "array"}}}
 :handler     (fn [params] {:content [{:type "text" :text (str (count (:rows params)))}]})}
```

Arguments arrive as a map with keyword keys. The handler may return the
result shape above (`:isError true` marks a failure), a string, or any data,
which the model sees as JSON. Names are restricted to `[A-Za-z0-9_-]`; a
built-in tool's name always wins, and between addons the first loaded wins.

Addon tools go through the same permission check as Janet plugin tools
(`plugin_tool`).

## Hooks

`(hooks addon)` returns a map from hook key to function. dirge calls these
keys and ignores the rest:

| Key | Called with | Return |
|---|---|---|
| `:dirge/session-start` | `{:session-id :cwd :first-prompt? :mcp-servers}` | text added before the session's first prompt in this process |
| `:dirge/session-end` | `{:session-id :cwd :reason}` | ignored |
| `:dirge/system-prompt` | `{:cwd :session-id}` | text appended to the system prompt |
| `:dirge/on-prompt` | `{:prompt :session-id :first-prompt?}` | text added before the user's prompt |
| `:dirge/before-tool-call` | `{:tool :args :tool-call-id}` | `nil`, `{:block "reason"}`, `{:context "text"}` or `{:args {…}}` |
| `:dirge/after-tool-call` | `{:tool :args :result :error?}` | text appended to the tool result |
| `:dirge/event` | one event of the run, see [Watching the run](#watching-the-run-dirgeevent) | ignored |

A text answer may also be given as `{:context "text"}`. Hooks run for the
main session only, after Janet plugin and command hooks. Subagents never run
addon hooks. An exception in a hook is logged and ignored.

`:first-prompt?` is true when the session has no earlier conversation (a new
session, or one `/clear` emptied), false for a resumed one.

### Session start and end

`:dirge/session-start` runs once per session in this process, as the first
prompt's run opens: when dirge starts, after `/clear`, after `/sessions`
switches, and when a compaction gives the session a new id. `:mcp-servers`
names the MCP servers connected by then; when they are still connecting in
the background, dirge waits up to 10 seconds for them first. The answers go
before that prompt, as a system reminder.

`:dirge/session-end` runs when the session ends, before dirge closes its MCP
servers, so the hook can still call them. `:reason` is `:exit` when dirge
exits (quitting the TUI, or a `--print` run finishing) and `:swap` when
`/clear` or `/sessions` puts another session in its place. It runs only for
a session whose start ran.

### When the prompt hooks run

`:dirge/session-start`, `:dirge/system-prompt` and `:dirge/on-prompt` run
inside the prompt's run, off dirge's event loop, before the first model
call: the TUI stays responsive meanwhile, and `mcp-call` and `call-tool`
work from them. They wait their turn behind any other call into the addons
still running (a command, a tool or a tool-call hook). A prompt's run opens
without `:dirge/session-start` answers after 30 seconds, and without
`:dirge/system-prompt` and `:dirge/on-prompt` answers after another 30. A
session end waits at most 10 seconds. Each skip is logged, and so is each
start and end that ran, on the `dirge::session` target.

A listener that outlasts its budget keeps running on the interpreter thread,
and hooks after it wait behind it: a slow `:dirge/session-start` can make the
first prompt's `:dirge/system-prompt` and `:dirge/on-prompt` miss their
budget too. When a start or end legitimately takes longer (an MCP call to a
slow server, say), raise its budget:

```json
{ "addons": { "session_start_timeout_secs": 180, "session_end_timeout_secs": 60 } }
```

## Slash commands

The `:dirge/commands` entry of the hooks map registers slash commands:

```clojure
{:dirge/commands
 {"rows" {:description "Count rows in the side panel"
          :handler     (fn [{:keys [args argv cwd]}] ...)}}}
```

`/rows a b` calls the handler with `{:args "a b" :argv ["a" "b"] :cwd …}`.
It returns `nil`, a string, or a map:

| Key | Effect |
|---|---|
| `:text` or `:markdown` | shown in the chat area |
| `:prompt` | submitted as the next prompt, starting a turn |

Command names are 1 to 32 characters of `[a-z0-9_:-]` starting with a letter.
Built-in commands and Janet plugin commands take precedence; between addons
the first loaded wins. Addon commands appear in `/help` and tab completion.

A handler runs off dirge's event loop, so dirge stays responsive while it
works. Until it answers dirge is busy: a second addon command or
`/addons reload` is refused, and a prompt typed meanwhile is queued. A
permission prompt the handler raises (a `call-tool` of `bash`, say) appears
and is answered as usual. Ctrl+C stops waiting for the answer; the handler
itself still runs to its end on the interpreter thread.

## Calling dirge

The `dirge.harness` namespace is available to addon code:

| Function | Effect |
|---|---|
| `(notify msg)` / `(notify msg level)` | a line in the chat area; `level` is `:info`, `:warn` or `:error` |
| `(log level msg)` | a log event on the `dirge::addon` target |
| `(cwd)` | dirge's working directory |
| `(version)` | the dirge version |
| `(tools)` | names of the dirge tools `call-tool` can run |
| `(call-tool name)` / `(call-tool name args)` | run a dirge tool (built-in or MCP) with the `args` map; answers `{:ok text}` or `{:error msg}` |
| `(panel op)` | a side-panel request, see below; returns true when delivered |
| `(mcp-servers)` | names of the MCP servers dirge is connected to |
| `(mcp-call server tool)` / `(mcp-call server tool args)` | call an MCP tool over dirge's own connection |
| `(json-parse text)` | JSON text as data (keyword keys), or `nil` |

`panel` takes one of:

```clojure
{:op :show   :id "rows" :title "Rows" :lines ["plain" {:text "warned" :face "warn"}]}
{:op :show   :id "rows" :title "Rows" :markdown "# Rows\n- one"}
{:op :append :id "log"  :text "one more line" :face "dim"}
{:op :focus  :id "log"  :title "Log"}
{:op :close  :id "rows"}
```

Faces are `normal`, `dim`, `accent`, `success`, `warn` and `error`.

`panel` is a port: the host hands the request to whatever panel sink the
build wires in. This build wires none, so `panel` returns false and draws
nothing. Addon code should treat false as "no panel here" and carry on.

`mcp-call` answers the tool result (`{:content [...] :isError bool}`) or
`{:error "why"}`. It blocks until the server answers. It is refused (an
`{:error}` answer) while dirge's event loop is waiting on the addon, which is
the case for loading at startup and shutdown at exit. Call it from hooks,
commands and tools, and from `initialize!` and `shutdown!` during
`/addons reload`. To reach MCP when a session starts, use
`:dirge/session-start` rather than `initialize!`: at startup the servers
may not be connected yet.

Before the call leaves dirge, `mcp-call` applies the refusals the model's
own MCP calls get, and it answers `{:error}` when:

- a `deny` permission rule matches `mcp_tool:<server>:<tool>`,
- the active prompt's `deny_tools` names the tool, `mcp_tool:<server>:<tool>`
  or `mcp_tool`, or
- an argument names a path outside the working directory and the server's
  config does not set `allow_external_paths: true`.

It never asks the user. A call that would prompt if the model made it (the
default for MCP tools, or an `ask` rule) runs without asking: an addon is
code you installed, running inside dirge. The answer is the server's
result as sent, without the size cap and injection scan applied to MCP
results the model sees, and a dropped connection is not reconnected by the
call.

`call-tool` goes through the tool's own permission check, so a call to
`bash` still asks the user. It refuses addon tools and `task`, and it is
unavailable during loading at startup and during shutdown at exit, while
dirge is waiting on the addon. Call it from a hook, a command, a tool, or
from `initialize!` and `shutdown!` during `/addons reload`. It runs on the Janet
plugin tool bridge, so it needs a dirge built with the `plugin` feature (in
the default set, not in `no-plugin` or `windows-default`). Without it
`(tools)` is empty and `call-tool` answers `{:error}` saying so.

To keep an addon portable, resolve these at call time, for example
`(when-let [f (resolve 'dirge.harness/notify)] (f "hi"))`, so the code is a
no-op on hosts without them.

## Lifecycle

Addons load at startup, before the first agent run, on a dedicated
interpreter thread. `shutdown!` runs for every addon when dirge exits. It
is skipped when a call into the addons is still running then, and dirge
waits at most 5 seconds for it to finish.

`/addons` lists the loaded addons with their tools, hooks, commands and
health, and any manifest that failed. `/addons reload` picks up changes
without restarting dirge:

1. every addon is shut down,
2. the loaded namespaces under each addon's own `src/` are evaluated again
   (the libraries it depends on, including the protocol, are not),
3. every manifest found now is loaded, constructed and initialized afresh.

New and removed tools reach the agent at the next prompt. A tool whose name a
built-in or another tool (a Janet plugin or MCP tool, say) already uses is not
installed, and the reload lists it as skipped; the same rule applies when
dirge starts. A reload also starts the host when dirge was started without
any addon installed. Keep
state that must survive a reload in `defonce`. A reload does not pick up a
changed `protocol_ns`; restart for that.

A reload runs off the event loop like an addon command: dirge is busy until
it finishes, and a permission prompt raised from `initialize!` or `shutdown!`
is answered as usual. Ctrl+C stops waiting for it; the reload still finishes
on the interpreter thread, but its tool changes do not reach the running
agent, so run `/addons reload` again.

## Live development

The addon runtime is meant to be the part of dirge you change while it
runs: new behaviour goes into an addon, evaluated in place, and dirge's
Rust core stays as it is.

### An nREPL into the running addons

Built with `--features addons-nrepl`, dirge can serve an nREPL on the addon
runtime's own thread, so an editor (CIDER, Calva, Conjure), dirge's own
`nrepl` plugin, or any nREPL client evaluates code among the loaded addons,
with `dirge.harness` in reach:

```json
{ "addons": { "nrepl": { "port": 0, "bind": "127.0.0.1" } } }
```

or, without touching the config, `DIRGE_ADDON_NREPL=1 dirge` (or a port
number instead of `1`; `0`, `off` or `false` turns a configured server off).
The server binds loopback by default and writes the port it bound to
`.dirge/addons/.nrepl-port` (`port_file` moves it; `""` writes none). `/addons`
shows the endpoint.

Evaluations run between dirge's calls into the addons, never during one, so
a REPL form sees the same state a hook does. `mcp-call` and `call-tool` work
from the REPL.

### Refreshing in place

After every REPL evaluation dirge asks each addon again for its `tools` and
`hooks` (commands included), without `shutdown!` or `initialize!`, and hands
what changed to the running agent: a new tool is offered to the model from
the next request, a removed one is withdrawn, and a changed hook runs its
new code on its next call. Addon code can ask for the same thing with
`(dirge.harness/refresh!)`, which runs once the current call returns. Set
`"live_refresh": false` under `addons` to refresh only when asked.

So write `tools` and `hooks` to build their answer when called, from vars
and atoms, rather than capturing functions once in `initialize!`:

```clojure
(defonce !extra (atom []))

(defrecord MyAddon []
  p/IAddon
  (tools [_] (into [base-tool] @!extra))       ; re-read on every refresh
  (hooks [_] {:dirge/on-prompt on-prompt}))    ; `on-prompt` resolved anew
```

Redefining `on-prompt` at the REPL is then enough. `/addons reload` is still
the tool for a full restart of every addon: it re-evaluates the sources on
disk and runs the lifecycle.

### Open hook keys

Hook keys are open. Besides the `:dirge/*` keys listed under
[Hooks](#hooks), an addon may register any keyword; `/addons` lists every
key an addon registered. A seam in dirge reaches such a key by name
(`AddonHost::emit`) without a new hook point in the host, so adding a seam
costs one call site, not a change to the addon host's types.

### Watching the run: `:dirge/event`

The `:dirge/event` hook hears every event of the main session's run as the
front end gets it, one map per event, keyed by `:event`:

| `:event` | Other keys |
|---|---|
| `:turn-start`, `:turn-end` | `:index` |
| `:tool-call` | `:id :tool :args` |
| `:tool-result` | `:id :output` |
| `:usage` | `:input-tokens :cached-input-tokens :cache-creation-input-tokens :output-tokens` |
| `:done` | `:response :tokens :cost` |
| `:compaction-started` | `:tokens-before` |
| `:context-compacted` | `:session-id :tokens-before :tokens-after :summary :kind` |
| `:checkpoint` | `:summary` |
| `:error`, `:context-overflow` | `:message` |
| `:retry` | `:attempt :delay-ms :message` |
| `:user-message` | `:content` |
| `:interjected` | `:response :tokens` |
| `:notice` | `:content` |
| `:custom-message` | `:payload` |
| `:escalation` | `:provider :reason` |
| `:repair-stats` | |

Streamed token and reasoning deltas are not sent: the whole response
arrives with `:done`. Text longer than 16 KiB is cut and marked. The hook's
answer is ignored and nothing waits for it: events are queued to the addon
runtime and run in order after whatever it is doing, and when more than 256
are waiting new ones are dropped.

## Example

`tests/fixtures/addons/` holds a minimal protocol namespace and an addon
that uses a tool, a hook, slash commands, `panel`, `mcp-call` and
`dirge.harness/notify`.

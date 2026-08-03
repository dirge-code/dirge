# nREPL Plugin

Evaluates Clojure code against an nREPL server from within dirge. The agent
can call the `nrepl_eval` tool, and you get slash commands for interactive
REPL use.

## How it works

**Connection.** On startup the plugin reads `.nrepl-port` from the project
root and opens an nREPL session. If there is no `.nrepl-port` (i.e. no REPL
running) it stays idle—no errors, no noise.

A REPL started *later* is picked up without you doing anything: `on-init`
fires once, so `nrepl_eval` re-reads `.nrepl-port` and connects on its first
call. This matters when the agent starts the REPL itself mid-session—the
startup read has long since happened. `/nrepl-connect` remains for
interactive use.

**Agent tools.** `nrepl_eval` and `nrepl_connect` appear in the LLM's tool
list. Both exist as tools deliberately: the agent cannot type a slash
command, so a plugin whose only connect path is `/nrepl-connect` strands the
model asking you to run it.
A skill prompt injected at the start of every session explains common
patterns (require with `:reload`, run tests, inspect vars). The model only
calls it when it knows Clojure is in play.

**Delimiter repair.** LLMs frequently produce broken parentheses. Before
every eval the code is scanned for unmatched `()`, `[]`, and `{}`; missing
closers are appended so the nREPL server gets valid syntax on the first
attempt. Strings and line comments are skipped.

**Timeout + interrupt.** Per-eval timeout (default 120 s, configurable via
`/nrepl-timeout`). An `interrupt` op is sent when the limit is exceeded.
`/nrepl-interrupt` is available for manual cancellation.

## Installation

Copy into one of two auto-discovered directories:

```
~/.config/dirge/plugins/nrepl/          # global — every project
<project>/.dirge/plugins/nrepl/         # per-project — wins on collision
```

No manifest, no config. Restart dirge to pick up the plugin.

## Files

| File | Role |
|------|------|
| `00-state.janet` | Bencode codec, nREPL protocol, paren repair, timeout+interrupt, eval |
| `01-hooks.janet` | Auto-connect on startup, skill-prompt injection |
| `02-commands.janet` | User slash commands |
| `03-tools.janet` | `nrepl_eval` LLM-callable tool registration |

Files load in lexicographic order into a shared Janet environment.

## Slash commands

| Command | Effect |
|---------|--------|
| `/nrepl-connect [host] [port]` | Connect (defaults to `127.0.0.1`, port from `.nrepl-port`) |
| `/nrepl-disconnect` | Close session and TCP connection |
| `/nrepl-eval <code>` | Evaluate Clojure expression, show result |
| `/nrepl-status` | Show connection state |
| `/nrepl-timeout [seconds]` | Get/set per-eval timeout (default 120 s) |
| `/nrepl-interrupt` | Interrupt a long-running eval |

## LLM tools

`nrepl_eval` — evaluate Clojure:

```json
{"code": "(+ 1 2 3)"}
```

Returns the value, stdout, stderr, current namespace, and an optional
repair notice when delimiters were fixed. Connects from `.nrepl-port` first
if not already connected.

`nrepl_connect` — connect or reconnect. Both parameters optional; with none,
reads `.nrepl-port`:

```json
{"host": "127.0.0.1", "port": 51208}
```

Only needed for a non-default host/port or after a server restart.

## Prerequisites

- dirge built with `--features plugin` (the default build includes it)
- A running Clojure nREPL server that writes `.nrepl-port` in the project root

No external `janet` binary is required. The plugin runs entirely inside
dirge's embedded Janet runtime — including the TCP transport, which uses
Janet's built-in `net/*` (driven by dirge's event loop). It shells out to
nothing.

## Differences from clojure-mcp-light

| Feature | clojure-mcp-light | This plugin |
|---------|-------------------|-------------|
| Transport | CLI (subprocess per eval) | Direct TCP (persistent connection) |
| Interface | MCP tool via io | dirge tool + slash commands |
| Delimiter repair | edamame + parinfer-rust/parinferish | Pure Janet stack matcher |
| Session persistence | Disk-based save/load | Ephemeral (lives as long as the REPL) |
| Port discovery | lsof + `.nrepl-port` | `.nrepl-port` (re-read lazily on eval) |
| Env detection | clj/bb/basilisp/shadow | None |
| Eval timeout | Configurable per-call | Per-eval with interrupt |
| Formatting | cljfmt (optional) | None |

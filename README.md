# dirge

A minimal, fast coding agent written in Rust — inspired by [pi](https://pi.dev/docs/latest/usage), [opencode](https://opencode.ai/), and [maki](https://github.com/tontinton/maki).

A dirge is a song to keep the dead from losing their way. It turns grief into something that is remembered. Agents are like mayflies awoken for a moment to work and to forget, with every new session effacing the old one. Dirge keeps watch over things said and done, always folding context into memory to carry past mistakes and preferences across the gulf between sessions. It sings the past forward, so that no grave need be dug twice. Dirge grieves for nothing, since nothing is truly buried under its care, and its lament is a promise that what was built here once will be remembered.

## Why dirge

What sets dirge apart from other agentic editors:

- **Tiny and fast.** Roughly 8 MB RAM idle, 15 MB working, 36 MB binary (approximate, measured on a Linux release build: `opt-level=3` + LTO) — versus ~300 MB for JS-based agents. Native Rust, no runtime.
- **Built to keep weaker/cheaper models on the rails.** A [robust agent loop](docs/features.md#robust-agent-loop) repairs malformed tool calls, validates every write through tree-sitter *before* it touches disk, escalates to a stronger model on repeated failure, and trips circuit breakers on non-progressing loops.
- **One explainable permission engine.** All authorization flows through a single Policy Decision Point with four modes, op-based rules, session allowlists, and a `/why` command that traces exactly which policy decided and why. See [docs/permissions.md](docs/permissions.md).
- **Role-based multi-provider routing.** Point the main loop, review, escalation, summarization, and subagent roles at different models — mix DeepSeek, GLM, Anthropic, OpenAI, Ollama, and any OpenAI-compatible endpoint in one session. Define your own opt-in [agent profiles](docs/agents.md) (a named model + prompt + tool-policy bundle) and switch personas mid-session with `/agent`.
- **Self-improving project memory.** Persistent per-project memory (plus a global cross-project tier for durable user preferences) and a post-session orchestrator that extracts learnings and curates memory + skills.
- **Long-horizon sessions that resume where they left off.** Every conversation keeps a durable, incrementally-refreshed checkpoint, anchored to a stable identity so resuming a long, compacted session recovers its live state instead of a stale snapshot. Autonomous runs can be held to a natural-language stop condition with `--goal`. Adapted from [MiMo-Code](https://github.com/XiaomiMiMo/MiMo-Code).
- **Code intelligence baked in.** Tree-sitter [semantic tools](docs/semantic.md) and [LSP diagnostics](docs/lsp.md) for 10+ languages, surfaced inline so the agent fixes compile errors on the same turn.
- **Extensible at runtime.** A [Janet plugin system](docs/plugins.md) hooks the full lifecycle, and [Claude-compatible skills](docs/skills.md) load instructions on demand.
- **Delegate to dirge from another agent.** Run `dirge mcp` to expose dirge as an [MCP server](docs/mcp-server.md): a planner agent (e.g. Claude Code) hands implementation tasks to dirge on a persistent per-project session, then reviews the summary + changed files it returns.

See the full [feature catalog](docs/features.md) for everything else.

## No embeddings, on purpose

dirge ships no vector index: code search is plain grep delivered inline, and
cross-session memory search is SQLite FTS5. A recent empirical study of agentic
search — [*Is Grep All You Need? How Agent Harnesses Reshape Agentic
Search*](https://arxiv.org/abs/2605.15184) (Sen et al., 2026) — supports these
defaults:

- Inline grep beat vector retrieval for **every** harness/model pair tested on
  long-term conversational memory QA (LongMemEval) — the same task dirge's
  session memory and FTS5 session search are built for.
- The harness mattered as much as the retriever: moving the same model between
  agent stacks shifted accuracy by ~16 points. In the authors' words, retrieval
  in an agent loop "is really retrieval-plus-orchestration" — and the
  orchestration layer is where dirge invests.
- Weaker models degraded the most under vector search and under file-based
  result delivery that turns each hit into a multi-step read-and-integrate
  workflow. Inline lexical search was the most forgiving combination, which
  fits dirge's goal of keeping cheaper models on the rails.

The study covers conversational memory, not code semantics. For structural
code questions dirge reaches for tree-sitter [semantic tools](docs/semantic.md)
and [LSP](docs/lsp.md) rather than embeddings.

## Installation

> The crate is published as **`dirge-agent`** (the short `dirge` name was
> already taken on crates.io). The installed command is still `dirge`.

```bash
# Batteries included — MCP, LSP, ACP, plugins, and every tree-sitter
# language are on by default.
cargo install dirge-agent
```

Or install a prebuilt binary with [Homebrew](https://brew.sh) (macOS + Linux):

```bash
brew install dirge-code/dirge/dirge
# equivalently: brew tap dirge-code/dirge && brew install dirge
```

Homebrew also makes upgrades a one-liner (`brew upgrade dirge`), and on macOS
it installs without the Gatekeeper quarantine prompt you'd get from
double-clicking a downloaded tarball.

Want a leaner binary? Opt out of the defaults and pick only what you need:

```bash
# Minimal: just the core agent + MCP, no semantic tools / plugins / ACP
cargo install dirge-agent --no-default-features --features "loop,git-worktree,mcp,lsp"

# Core + only the languages you use
cargo install dirge-agent --no-default-features \
  --features "loop,git-worktree,mcp,lsp,semantic-rust,semantic-python"
```

Prebuilt binaries for Linux (glibc + static musl), macOS (Intel + Apple
Silicon), and Windows are attached to each [GitHub Release](https://github.com/dirge-code/dirge/releases).

### Optional: sandbox mode

Install [bubblewrap](https://github.com/containers/bubblewrap) for `--sandbox`, which runs every bash command inside an isolated environment:

```bash
# Debian/Ubuntu:  apt install bubblewrap
# Fedora:         dnf install bubblewrap
# Arch:           pacman -S bubblewrap
```

## Quick start

```bash
# Set your API key (OpenRouter is default)
export OPENROUTER_API_KEY="[api_key]"

# Interactive session (default prompt: code)
dirge

# One-shot mode
dirge -p "Explain this project"

# Continue last session
dirge -c

# Resume a specific session by id/prefix — or create one with that exact id
# if it doesn't exist yet (a stable id for scripting and the shell plugin)
dirge --session my-refactor

# Browse and pick a session interactively
dirge -r

# Run dirge as an MCP server so another agent can delegate tasks to it
# (register with `claude mcp add dirge -- dirge mcp`). See docs/mcp-server.md
dirge mcp

# Explicit provider/model
dirge --provider openrouter --model openai/gpt-4o

# DeepSeek and GLM are first-class providers
export DEEPSEEK_API_KEY="sk-..."
dirge --provider deepseek  # defaults to deepseek-v4-pro

export GLM_API_KEY="..."
dirge --provider glm       # defaults to glm-4

# Verbose mode — debug-level dirge logs + warn-level plugin hook errors
dirge --verbose
```

Avoid `--api-key <key>` outside one-off testing — it's visible to other
processes via `ps` and emits a startup warning. Prefer a key file, stdin, or
the provider's env var:

```bash
dirge --provider openai --api-key-file /run/secrets/openai_key
pass openai-key | dirge --provider openai --api-key-stdin
```

### OpenAI device-code login

Dirge can store a local OpenAI OAuth refresh token for OpenAI provider fallback:

```bash
dirge auth openai
```

Before running it, enable device-code auth in ChatGPT Codex security settings.
The command prints an OpenAI verification URL and user code; do not share that
code with anyone. On success, credentials are saved under the Dirge data
directory: `~/.local/share/dirge/auth.json` on Linux, or
`$DIRGE_DATA_DIR/auth.json` when `DIRGE_DATA_DIR` is set. The login persists
across Dirge sessions until you delete that file or OpenAI revokes/expires it.

Provider credential precedence is unchanged: configured API keys,
`--api-key-file`, `--api-key-stdin`, config `api_key`, config `api_key_env`, and
provider environment variables win first. If no higher-precedence OpenAI key is
available, Dirge uses the fresh stored OAuth access token against the ChatGPT
Codex backend. Expired OAuth credentials require rerunning `dirge auth openai` or
setting an API key.

Troubleshooting: a 404 or "device-code auth is not enabled" error means the
ChatGPT Codex security setting is still disabled. A timeout means the browser
approval did not complete in time; rerun `dirge auth openai`. To reset local
authorization, delete the `auth.json` file and log in again.

## Slash commands

| Command | Description |
|---------|-------------|
| `/model [name]` | Show or switch model |
| `/prompt [name]` | List or activate prompts (`code`, `plan`, `review`, etc.) |
| `/agent [name\|off]` | List or switch [agent profiles](docs/agents.md) — a named model + prompt + tool-policy bundle |
| `/clear` | Clear conversation |
| `/cd [path]` | Change working directory |
| `/undo` | Undo last exchange |
| `/compress` (or `/compact`) | Force an LLM-summarization compaction pass now — unlike automatic compaction, an explicit `/compress` runs even when the context is still within limits |
| `/mode [mode]` | Set security mode (`standard`, `restrictive`, `accept`, `yolo`) |
| `/reasoning` | Toggle reasoning visibility |
| `/btw <question>` | Ask a quick question (no tools, doesn't affect session) |
| `/sessions` | List/save/load sessions |
| `/tree [id-prefix]` | Show session tree; with prefix, switch the active branch to that leaf |
| `/fork [id-prefix]` | Branch off the chosen message (default: last user message) and restore its text to the editor |
| `/clone <id-prefix>` | Switch the active branch to the entry without restoring text |
| `/loop [prompt]` | Start iterative coding loop (needs the `loop` feature; otherwise prints a hint) |
| `/plan <task>` | Run the phased explore→plan→implement→review workflow (opt-in via `phased_workflow_enabled`). See [docs/agent-loop.md](docs/agent-loop.md#phased-plan-workflow-plan) |
| `/worktree <name>` | Create a git worktree on branch |
| `/wt-merge [branch]` | Merge worktree branch |
| `/wt-exit` | Exit worktree |
| `/toggle` | Toggle features on/off (currently todo tools) |
| `/regen-prompts` | Restore built-in prompts |
| `/mcp` | List MCP servers and tools (only present in builds with the `mcp` feature) |
| `/kill [id]` | Kill the subagent on the focused chat tab (also `Ctrl+K`) |
| `/panel [on\|off\|auto\|debug]` | Toggle both side panels together — left: session vitals (context gauge, recent activity, git); right: system load, MCP, LSP, todos, modified files. `auto` shows them at ≥152 cols; `debug` forces the layout-debug view. |
| `/display <panes>` | Choose which panes show, e.g. `/display main`, `/display main\|right`, `/display left\|main\|right`. The main pane is always shown; left/right toggle independently. Set a default with the `display` config key. |
| `/allow [list\|add\|remove\|clear]` | Manage the session permission allowlist; bare `/allow` lists it. See [docs/permissions.md](docs/permissions.md#allow-always-and-the-session-allowlist) |
| `/why <tool> [input]` | Dry-run a permission decision and print the full policy trace |
| `/retry` | Retry last prompt |
| `/quit` | Exit dirge |
| `/help` | Show all commands |

For key bindings, the inline avatar, and tool-output display, see [docs/tui.md](docs/tui.md).

## Shell integration (the `:` prefix)

An optional zsh plugin lets you talk to dirge **without leaving your shell**.
Type `:<prompt>` at your normal prompt and press Enter — the prompt runs
through dirge headlessly, the answer prints, and you're back at the shell.
Every `:` command in a shell shares one dirge session, so follow-ups keep
context. `:resume` opens the full TUI on that session; `:new` starts a fresh
one.

```bash
$ : what does this repo's build pipeline do?   # asks dirge, prints the answer
$ git status                                    # normal shell — unaffected
$ : now add a clippy step to CI                 # same session → has context
```

Install by sourcing it from `~/.zshrc`; see
[shell-plugin/README.md](shell-plugin/README.md). (It's built on
`dirge --session <id>`, which creates the session on first use and resumes it
thereafter.)

## Supported providers

OpenRouter (default), OpenAI, Anthropic, Gemini, DeepSeek, GLM (ZhipuAI),
Ollama, and any custom OpenAI-compatible endpoint.

Providers are declared once in `$XDG_CONFIG_HOME/dirge/config.json` and
referenced by alias from role-assignment keys (`provider`, `review_provider`,
`escalation_provider`, `summarization_provider`, `subagent_provider`) — so each
role can run on a different model. See [docs/config.md](docs/config.md) for the schema,
provider aliases, role-assignment table, permission rules, and MCP setup.

## Documentation

| Document | Topic |
|---|---|
| [docs/config.md](docs/config.md) | Config file location, keys, provider aliases, permission rules, MCP servers |
| [docs/features.md](docs/features.md) | Full feature catalog, robust agent loop, performance |
| [docs/permissions.md](docs/permissions.md) | Authorization engine, security modes, `/why` |
| [docs/prompts.md](docs/prompts.md) | Prompts system, per-prompt tool restrictions, context files |
| [docs/agents.md](docs/agents.md) | Agent profiles — named model + prompt + tool-policy bundles, `/agent` switching |
| [docs/skills.md](docs/skills.md) | Claude-compatible skills |
| [docs/semantic.md](docs/semantic.md) | Tree-sitter semantic code tools |
| [docs/lsp.md](docs/lsp.md) | LSP integration and built-in server set |
| [docs/tui.md](docs/tui.md) | Key bindings, avatar, tool-output display, themes |
| [docs/plugins.md](docs/plugins.md) | Janet plugin authoring — hooks, `harness/*` API, examples |
| [docs/agent-loop.md](docs/agent-loop.md) | Multi-turn execution loop architecture |
| [docs/tool-input-repair.md](docs/tool-input-repair.md) | Repair layer for malformed tool calls |
| [docs/themes.md](docs/themes.md) | Built-in palettes and custom theme schema |

## License

GPL-3.0-only

## Acknowledgements

This project builds on and is deeply indebted to:

- [**zerostack**](https://github.com/gi-dellav/zerostack) by Giuseppe Della Vedova — the original minimal coding agent that dirge was forked from. Provides the core agent architecture, permission system, TUI, and prompt infrastructure.
- [**maki**](https://github.com/tontinton/maki) by Tony Solomonik — a feature-rich Rust coding agent. The Claude-compatible skills system, bash tree-sitter permissions, memory tool, bang commands (`!`/`!!`), `/cd` command, `/btw` query, rewind picker, and task/subagent tool were all ported from maki.
- [**Hermes Agent**](https://github.com/NousResearch/hermes-agent) by Nous Research — a reasoning-aware coding agent with structured thinking patterns.
- [**pi coding-agent**](https://github.com/earendil-works/pi/tree/main/packages/coding-agent) by Earendil Works — a developer agent with robust tool-use and workflow automation.
- [**vix**](https://github.com/kirby88/vix) — a battle-tested Go coding agent. dirge's phased plan workflow (the `/plan` command: explore → plan → implement → reviewer-runs-code loop), the minified tree-sitter read/edit family, the hard read-before-edit gate, the thinking-stall watchdog, mandatory reason/intent fields on navigation tools, and the todo-completion nudge were all ported from vix.
- [**MiMo-Code**](https://github.com/XiaomiMiMo/MiMo-Code) by Xiaomi — a long-horizon coding agent. dirge's long-horizon session work is adapted from its design: the durable per-conversation checkpoint, incremental background checkpointing on a 20%-of-window cadence, the stable conversation identity that lets a resumed session pick up where it left off, the goal gate (a judge-verified natural-language stop condition for autonomous runs), and the global cross-project memory tier.

# Documentation

Feature-by-feature documentation for dirge. For installation, quick
start, and feature overview, see the top-level [README](../README.md).
For configuration keys and provider setup, see [config.md](config.md).

| Document | Topic |
|---|---|
| [config.md](config.md) | Config file location, keys, provider aliases + roles, permission rules, MCP/LSP/ACP servers |
| [features.md](features.md) | Full feature catalog — core capabilities, the robust agent loop, performance |
| [agents.md](agents.md) | Agent profiles — named personas, model routing, and tooled subagents |
| [subagent-dispatch-strategy.md](subagent-dispatch-strategy.md) | Coordinated subagents — batch dispatch, profile tiers, retries, and read-write isolation |
| [permissions.md](permissions.md) | Authorization engine — the single decision point, operations/claims, policy precedence, sane defaults, config, security modes, `/why` |
| [prompts.md](prompts.md) | Prompts system — built-in prompts, per-prompt `deny_tools` restrictions, custom prompts, context files |
| [skills.md](skills.md) | Claude-compatible skills — discovery directories, `SKILL.md` format |
| [spec-workflow.md](spec-workflow.md) | Spec-driven workflow — living specs, change deltas, task tracking, the `spec` tool + `/spec` command, SQLite storage |
| [semantic.md](semantic.md) | Tree-sitter semantic code tools — symbols, definitions, callers/callees, per-language export detection |
| [lsp.md](lsp.md) | LSP integration — inline diagnostics, built-in server set, workspace root resolution |
| [tui.md](tui.md) | Terminal UI — key bindings, inline avatar, tool-output display, theme |
| [agent-loop.md](agent-loop.md) | Multi-turn agent execution loop — turn structure, hooks, stream pipeline, tool dispatch |
| [failure-ladder.md](failure-ladder.md) | Tiered verification, progress/stall + turn-budget signals, safe-state abort, residual objectives |
| [verification-discipline.md](verification-discipline.md) | Project gate, CI advisory, masked-command guard, exploration-prologue bound, capability tier, and how to measure a loop change |
| [tool-input-repair.md](tool-input-repair.md) | Repair layer for malformed tool calls — repair kinds, `dirge-hints` schema annotations, telemetry |
| [plugins.md](plugins.md) | Janet plugin authoring — hook reference, `harness/*` API, examples |
| [themes.md](themes.md) | Built-in palettes and custom theme JSON schema |
| [storyboards/](storyboards/) | Step-by-step walkthroughs of user-facing flows |

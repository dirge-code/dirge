# PLAN: Tooled subagents (v1 — readonly, isolated, configurable)

## Goal
Let a `task(agent="<profile>")` subagent run a **real filtered agent loop** instead
of a tool-less one-shot, behind an opt-in per profile. v1 ships exactly one tooled
tier — **readonly** — with recursion, session-attribution, interactive, and mutating
tools hard-excluded. The default-subagent path (no `agent=`, or a profile that doesn't
opt in) stays byte-identical to today's `btw_query_with`, so the dirge-mifq regression
test and all current behavior remain green.

## Locked decisions
- **Session id:** fresh child id (`sub-<uuid>`), parent-linked conceptually but
  **inert in v1** (every tool that would consume it is force-excluded).
- **Recursion:** hard ban — `task`/`task_status` force-excluded.
- **Config:** tiers + raw allow/deny override. v1 tiers: `toolless` (default),
  `readonly`. `readwrite` reserved (resolver errors if named).
- **Fact propagation:** isolated — empty transcript, child writes stay child-scoped.
- **max_turns:** default **25**, overridable per profile via `subagent.max_turns`.
- **Permissions (v1):** inherited automatically (filter the parent's already-built
  tools → each retained tool carries the parent's PermCheck).
- **websearch/webfetch:** kept in the readonly base.

## v1 readonly tool universe
`READONLY_BASE`: `read, read_minified, grep, find_files, glob, list_dir,
repo_overview, websearch, webfetch`

`SUBAGENT_FORCED_EXCLUDES`: `task, task_status, memory, skill, spec,
write_todo_list, session_search, issue, graph, question, plan_enter, plan_exit`

Readonly invariant: final allow-list is **intersected with `READONLY_BASE`**, so
`allow:` can never escalate; `deny:` narrows.

## Tasks
1. **Config model + tier resolver** — `task.rs` (types + resolver), `agent_defs.rs`
   (parse `subagent:` block + `AgentDefinition`/`AgentConfig` fields).
2. **Extend `SubagentRoute`** with `tool_allow` + `max_turns`.
3. **Resolve policy → route at startup** (`main.rs:1202`).
4. **Process-global live-agent handle** (`provider/mod.rs` + tail of `build_agent`).
5. **`spawn_subagent_runner` on `AnyAgent`** (`provider/spawn.rs`).
6. **Drain + relay helper** (first live producer for dormant chat events).
7. **`TaskTool::call` branch** (foreground + background).
8. **Abort watcher** (`/kill` + Ctrl+K on the tooled path).
9. **Tool description** update.
10. **Update dirge-mifq regression test** (keep btw assertions; cover new shape).
11. **Docs** (`agents.md`, `features.md`, `config.md`).

## Verification
```bash
cargo fmt --check
cargo clippy --bin dirge -- -D warnings
cargo test --bin dirge
```

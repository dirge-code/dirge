# dirge as an MCP server (`dirge mcp`)

Run dirge as an [MCP](https://modelcontextprotocol.io) server so another
agent — e.g. Claude Code — can **delegate implementation tasks to dirge
and review them**. The calling agent does high-level planning and
architecture; dirge handles the implementation details in the project,
and the caller reviews the result.

Built into the binary (the `mcp-server` feature, on by default). Speaks
MCP over stdio.

## Register it with Claude Code

From inside the project you want dirge to work in:

```bash
claude mcp add dirge -- dirge mcp
```

or add it to the project's `.mcp.json`:

```json
{
  "mcpServers": {
    "dirge": { "command": "dirge", "args": ["mcp"] }
  }
}
```

The server runs in the project directory, so dirge's session lives in
that project's `.dirge/` and its tools operate on that codebase.

Options: `dirge mcp --model <id>` to pin the model dirge uses for
delegated work, `--sandbox bwrap|microvm` to isolate the bash it runs.

## The session model

The server keeps **one persistent session per project**. Every
`delegate` runs against the same on-disk dirge session, so dirge
accumulates context across tasks — a follow-up "fix the edge case" lands
with the full history of what it already did. The current session id is
remembered in `<project>/.dirge/mcp_current_session.json`, so it survives
a server restart. Start a fresh session (new task/thread) with
`new_session` or `delegate(new_session=true)`.

## Tools

### `delegate`
Hand dirge an implementation task. It edits files / runs commands in the
project on the current session, then returns a summary plus the files it
changed for review.

| param | type | notes |
|---|---|---|
| `task` | string (required) | the task + any constraints, in plain language |
| `new_session` | bool | start a fresh session first (new task/thread) |
| `session_label` | string | label for the new session |
| `max_turns` | int | cap on dirge's turns (default 30); on hit, `status:"max_turns"` |

Returns:
```json
{
  "session_id": "mcp-…",
  "status": "completed" | "max_turns" | "error",
  "summary": "what dirge did",
  "files_changed": ["src/foo.rs", "tests/foo.rs"],
  "turns": 7,
  "duration_ms": 41230
}
```
`summary` + `files_changed` is enough to decide: accept, ask for a fix
(call `delegate` again — same session), or move on. Review the actual
diff with your own tools (`git diff`).

### `new_session`
`new_session(label?)` → `{ session_id, label }`. Rotate to a fresh
session without immediately delegating.

### `session_info`
`{ session_id, label, project_dir, message_count, last_active, model, sandbox }` —
orientation on the current session.

### `list_sessions`
Recent sessions in the project: `[{ id, last_active, messages, preview }]`
so you can resume a past thread.

## The loop in practice

1. Plan → `delegate("implement the token-bucket limiter in src/ratelimit.rs: 100 req/s, burst 20, per-IP; add unit tests")`.
2. Review the returned `summary` + `files_changed`; inspect the diff and run the tests yourself.
3. One edge case off → `delegate("burst refill is wrong on idle reset; fix and add a test")` — same session, dirge has the context.
4. New area → `delegate("wire the limiter into the HTTP middleware", new_session=true, session_label="middleware")`.

## Safety

`delegate` runs dirge with **accept-all scoped to the project cwd**
(auto-approves edits/bash inside the project, like `claude -p
--permission-mode acceptEdits`) — never `--yolo`. Add `--sandbox` on the
`dirge mcp` command to additionally isolate bash. The server only ever
operates in the directory it was launched in.

## Implementation note

v1 executes each delegation by spawning `dirge -p --session <id>
--accept-all --output-format json <task>` as a child process — robust and
isolated, but it cold-starts dirge each delegation. A warm in-process
executor that keeps the LSP / MCP / semantic managers hot across
delegations is a planned follow-up; it swaps only the executor, not this
MCP API.

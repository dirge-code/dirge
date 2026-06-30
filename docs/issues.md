# Issue tracker

A persistent, agent-facing kanban board built into dirge. It is a stateful
extension of the memory model: open issues are the agent's working board and
are surfaced automatically; closed ones drop off it.

Unlike `write_todo_list` — an ephemeral, within-session checklist — issues
**persist across sessions** in the per-project session DB
(`.dirge/sessions/state.db`, `issues` table). No external tracker, no extra
process, no separate storage engine.

## What makes it different from polling a tracker

The harness **injects the board at the start of each turn**. The model does not
have to remember to list its work — the top open issues arrive as a
`<system-reminder>` block before it sees the prompt:

```
<system-reminder>
Issue board (your persistent kanban — surfaced automatically; you did not ask for it). ...
- #1 [in_progress] (high) Wire up OAuth refresh
- #2 [open] (normal) Add dark mode
… and 3 more open issue(s) not shown. Use the `issue` tool (action=list) or /issues to see all.
</system-reminder>
```

The board is bounded (top issues only, with a "+N more" hint) so a large backlog
can't flood the context. It is injected into model-facing context only — never
persisted into session history — and refreshes every turn, so it always reflects
current state. Forked review/curator runners don't receive it.

## States and priorities

- **Status**: `open` → `in_progress` → `done`, plus `blocked`. The board shows
  the live states (open / in_progress / blocked), ordered in_progress → blocked
  → open, then by priority, then most-recently-touched. `done` issues leave the
  board.
- **Priority**: `high` / `normal` / `low` (default `normal`).

Ids are short integers shown as `#7`; inputs accept `7`, `#7`, or `iss-7`.

## The `issue` tool (model-facing)

One tool with an `action`:

| action | args | effect |
|--------|------|--------|
| `create` | `title`, optional `body`, `priority` | new open issue |
| `start` | `id` | → `in_progress` |
| `block` | `id` | → `blocked` |
| `close` | `id` | → `done` (stamps closed time) |
| `update` | `id`, optional `status`/`priority` | mutate fields (validated before any write) |
| `show` | `id` | full details |
| `list` | optional status filter | the board, or all issues with a status |
| `search` | `query` | substring match over title + body |

The model is nudged (in the tool description and the injected board) to `create`
issues as it discovers work, `start` one when it begins, and `close` it when
done.

## The `/issues` command (human-facing)

The same store, viewed from the TUI:

- `/issues` or `/issues list` — the live board
- `/issues list <status>` — filter by `open`/`in_progress`/`blocked`/`done`
- `/issues search <query>` — substring search
- `/issues <id>` — one issue's details (accepts `7` or `#7`)

## Storage

The `issues` table is self-owned: it is created idempotently
(`CREATE TABLE IF NOT EXISTS`) when first opened, independent of the session
DB's versioned migrations. The store opens a plain connection with a busy
timeout, so it never contends on the session DB's migration lock. Each issue
records the session that last touched it for provenance.

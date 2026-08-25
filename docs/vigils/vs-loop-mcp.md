# Vigils vs. Loop vs. MCP

Dirge has three ways to drive agent behavior. They serve different needs.

## Loop

`--loop` runs the agent continuously — one turn after another, chained, until a
max iteration cap or manual stop. It has two extension flags:

- `--loop-oneshot` — run exactly one iteration
- `--loop-persist` — save session to disk after each iteration

Best for: long autonomous coding tasks where the agent owns the full run
end-to-end. Think "build this feature from scratch" or "fix all the clippy
warnings."

## Vigil

`--vigil` is a wakeup-and-sleep runtime. The agent is asleep most of the time. It
wakes only when:

- A timer fires (toll)
- A file changes (watcher)
- A TCP connection arrives (harbinger)
- A Janet plugin pushes an event (`vigil/emit`)

Before waking, an optional rite gate runs — a shell command that must pass. This
means no LLM cost for false alarms: the rite runs `cargo test`, and only if tests
fail does the agent get a turn.

After a turn, the agent goes back to sleep. If `procession` is set, it injects an
event into another vigil's queue first — chaining without manual intervention.

Best for: monitoring, alert-driven remediation, health checks, CI watchdogs,
webhook handlers, and anything where the agent should be reactive, not
continuously running.

## MCP (Model Context Protocol)

MCP is a protocol for connecting dirge to external tools. It's an integration
surface: an MCP server provides tools, resources, and prompts that the agent can
invoke during a turn.

Best for: adding external capabilities to agent turns — database queries, API
calls, specialized tools. MCP extends *what the agent can do during a turn*; it's
not a scheduling or trigger mechanism.

## Decision guide

| Need | Use |
|---|---|
| Agent runs continuously on a long task | Loop |
| Agent wakes on a schedule, checks something, sleeps | Vigil (toll) |
| Agent wakes when files change | Vigil (watcher) |
| Agent wakes on an external signal (webhook, API poll) | Vigil (harbinger or plugin) |
| Agent only wakes if a check passes | Vigil (with rite) |
| Chain one agent task into another automatically | Vigil (procession) |
| Add tools the agent can call during a turn | MCP |
| Server-pushed notifications during a turn | MCP |
| Fire-and-forget command dispatch (no LLM) | Vigil (commands mode) |
| Persistent background polling of an external service | Vigil + Janet plugin |

## Why vigil instead of cron + loop?

A cron job running `dirge --loop-oneshot` is possible, but it lacks:

- **Queue coalescing** — 15 rapid events become one agent turn with aggregated
  context, not 15 separate turns
- **Rite gates** — cron always launches; vigils gate on an optional check, saving
  LLM cost on false alarms
- **Procession chaining** — one vigil fires, another fires next, without a cron
  scheduler mediating
- **Per-vigil reap cadence** — each vigil reaps independently; cron forces one
  global schedule
- **Backpressure** — bounded ring-buffer queues; cron has no queue semantics
- **In-process state** — vigils share dirge's session, permission model, and
  plugin system; cron processes are isolated

Vigils are a first-class runtime primitive, not a workaround.

## Why vigil instead of a Janet plugin alone?

A Janet plugin can poll and call `(vigil/emit ...)`, but the plugin doesn't
provide:

- The reaper: coalescing, rite gates, per-vigil cadence
- The queue: bounded, backpressured, per-vigil
- Template substitution: `{rite_output}`, `{files}`, `{job}`, custom fields
- Procession chaining
- `/vigil status`, `/vigil pause`, `/vigil resume`

The plugin is the event *producer*. The vigil-keeper is the event *consumer*.
Together they form a complete monitoring and remediation loop. The plugin decides
when to emit; the vigil-keeper handles everything downstream.

## Why vigil instead of MCP for monitoring?

MCP is a synchronous request-response protocol. The agent calls an MCP tool
during a turn to get information. It doesn't:

- Schedule periodic checks
- Push events to a sleeping agent
- Coalesce multiple events
- Gate on pre-checks before spending tokens
- Chain agent turns

Vigils and MCP are complementary. An MCP tool might be what the agent *uses*
during a vigil observance turn to query a database, check a dashboard, or update
a ticket. The vigil handles *when and whether* the turn fires; MCP handles *what
capabilities* are available during it.

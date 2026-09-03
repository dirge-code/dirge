# Vigils — Heartbeat, Wakeup & Monitoring Mode

Vigil is a wakeup-and-sleep runtime for dirge. It monitors triggers, queues
events, reaps them on a configurable cadence, runs optional gate checks, and
dispatches agent turns only when a gate passes — then returns to monitoring.

Start it with `dirge --vigil`. Control vigils from inside the TUI with `/vigil`
slash commands.

## What vigils do

- **Monitor** — timer ticks, filesystem changes, TCP socket connections, or
  custom Janet plugin probes
- **Queue** — bounded per-vigil channels (256 events) with ring-buffer
  backpressure
- **Reap** — drain into coalesced batches on independent per-vigil intervals
- **Gate** — optional `rite` command (shell check) before firing an agent turn
- **Observe** — one agent turn with a template-substituted prompt, then back to
  sleep
- **Chain** — optional `procession` field: after one vigil fires, inject an event
  into the next vigil's queue (bypassing its trigger)

## Why vigils instead of `/loop`?

`/loop` runs the agent continuously — turn after turn, no pause, no trigger, no
conditional gate. Vigils are the monitoring counterpart:

| | Loop | Vigil |
|---|---|---|
| Trigger | None (immediate) | Timer, file watch, socket, plugin probe |
| Idle | Busy (always running) | Sleep (wakes only on trigger) |
| Gate | None | Rite command (optional) |
| Chaining | Manual (re-prompt) | Automate (procession) |
| Best for | Long autonomous tasks | Monitoring and alert-driven fix loops |

## Quick start

```bash
# Create a vigil config
cat > .dirge/vigils/hello.json << 'EOF'
{
  "name": "hello",
  "trigger": { "type": "toll", "interval_secs": 30 },
  "reap_interval_secs": 30,
  "prompt": "Vigil fired. Rite output: {rite_output}",
  "rite": { "cmd": "echo 'all clear'" }
}
EOF

# Start in vigil mode
dirge --vigil

# In the TUI
/vigil status        # see all vigils
/vigil pause hello   # temporarily stop
/vigil resume hello  # restart
/vigil remove hello  # remove entirely
```

## More docs

- [Usage](usage.md) — configuration reference, triggers, rites, template
  variables, slash commands
- [Use Cases](use-cases.md) — CI monitoring, file-diff review, webhook handlers,
  Jenkins/Prefect/Airflow remediation, and custom Janet plugins
- [vs. Loop & MCP](vs-loop-mcp.md) — when to use vigil, loop, or MCP; how they
  differ; why you might prefer one over the others

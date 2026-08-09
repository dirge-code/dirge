# Vigil Usage

Configuration reference, trigger types, rite gates, template variables, slash
commands, and Janet plugin integration.

## Configuration

Vigils are defined in two places, merged by name (config wins on collision):

### A) `config.json` — inline `vigils` block

```json
{
  "vigils": [
    {
      "name": "ci-watch",
      "trigger": { "type": "toll", "interval_secs": 300 },
      "reap_interval_secs": 300,
      "rite": { "cmd": "cargo test --quiet 2>&1" },
      "prompt": "Tests failed:\n{rite_output}\n\nFix the failures.",
      "procession": "review-changes"
    }
  ]
}
```

### B) `.dirge/vigils/*.json` — filesystem vigils

One file per vigil. Same schema. Filesystem vigils load alongside config vigils;
config entries win on name collision.

```json
{
  "name": "ci-watch",
  "trigger": { "type": "toll", "interval_secs": 300 },
  "reap_interval_secs": 300,
  "rite": { "cmd": "cargo test --quiet 2>&1" },
  "prompt": "Tests failed:\n{rite_output}\n\nFix the failures."
}
```

### VigilEntry fields

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | yes | — | Unique vigil name. Used in `/vigil` commands and `vigil/emit`. |
| `trigger` | yes | — | What produces events: toll, watcher, or harbinger. |
| `reap_interval_secs` | no | `30` | How often the reaper drains the queue. |
| `rite` | no | `null` | Optional gate command. If the rite fails, the observance is skipped. |
| `prompt` | no | `""` | Template string sent to the agent for observances. |
| `procession` | no | `null` | Name of the next vigil to chain to after this one fires. |

## Trigger types

### Toll (timer)

Fires on a fixed interval. Good for polling, health checks, periodic scans.

```json
{
  "trigger": { "type": "toll", "interval_secs": 300 }
}
```

`interval_secs` controls how often events are *produced*. `reap_interval_secs`
controls how often they're *consumed*. When both are equal (e.g., both 300), one
event per reap window — simple. When `interval_secs` is shorter than
`reap_interval_secs`, multiple events are coalesced into one batch.

### Watcher (filesystem)

Fires when files change in a watched directory. Uses inotify under the hood.

```json
{
  "trigger": { "type": "watcher", "path": "src/" }
}
```

Includes a 500ms debounce to coalesce rapid changes. Events carry the changed
file path and event kind (modify, create, delete).

### Harbinger (TCP socket)

Fires on incoming TCP connections. Two socket modes:

#### Template mode (default)

The raw socket payload substitutes into `{harbinger_data}` in the prompt. The
agent turn fires with the full payload.

```json
{
  "trigger": {
    "type": "harbinger",
    "address": "127.0.0.1:9090",
    "protocol": "tcp"
  },
  "prompt": "Harbinger received:\n{harbinger_data}\n\nRespond."
}
```

Send data with `nc`:

```bash
echo '{"message":"hello"}' | nc -w1 127.0.0.1 9090
```

#### Commands mode

The socket payload carries a command name and optional args. The vigil-keeper
looks up the command in a pre-registered map, substitutes `{arg}` placeholders,
and dispatches — no agent turn, no LLM cost.

```json
{
  "trigger": {
    "type": "harbinger",
    "address": "127.0.0.1:9091",
    "protocol": "tcp",
    "socket_mode": "commands",
    "commands": {
      "build": {
        "tool": "bash",
        "args": { "command": "cargo build {release_flag}" }
      },
      "ping": {
        "tool": "bash",
        "args": { "command": "echo 'pong'" }
      }
    }
  }
}
```

```bash
echo '{"command":"build","args":{"release_flag":"--release"}}' | nc -w1 127.0.0.1 9091
echo '{"command":"ping"}' | nc -w1 127.0.0.1 9091
```

Security constraints for commands mode:

- Must bind to `127.0.0.1` (loopback) — rejected at startup otherwise
- The commands map must be non-empty — rejected at startup otherwise
- The caller provides only a command name and flat args — no tool name, no raw
  argument strings
- Dispatched tools must pass dirge's existing permission check — nothing is
  auto-confirmed

## Rites (gate checks)

A rite is an optional shell command that runs before each observance. If the
command exits non-zero, the observance is skipped.

```json
{
  "rite": { "cmd": "cargo test --quiet 2>&1" }
}
```

Use rites to:

- Only wake the agent when a real problem exists (`curl ... | grep ERROR`)
- Run a cheap pre-check before spending LLM tokens
- Gate on git state, API health, or any shell-testable condition

## Template variables

The `prompt` field supports these template variables:

| Variable | Source |
|---|---|
| `{name}` | Vigil name |
| `{files}` | Comma-separated changed file paths |
| `{events}` | Comma-separated event kinds (toll, watcher, harbinger) |
| `{event_count}` | Number of events in this reap window |
| `{timestamp}` | ISO 8601 reap time |
| `{rite_output}` | stdout+stderr from rite command |
| `{rite_exit_code}` | Exit code from rite command |
| `{harbinger_data}` | Raw socket payload (first connection in the window) |

Additionally, any `{key}` matching a string field in the merged event context
objects is substituted. This is how custom Janet plugin fields (job names, build
numbers, flow IDs) flow into observance prompts.

## Processions (chaining)

When a vigil's `procession` field names another vigil, the vigil-keeper injects
an event into that vigil's queue after the observance completes — bypassing its
trigger. The next vigil processes it on its own next reap.

```json
[
  {
    "name": "ci-watch",
    "trigger": { "type": "toll", "interval_secs": 300 },
    "reap_interval_secs": 300,
    "rite": { "cmd": "cargo test --quiet 2>&1" },
    "prompt": "Tests failed:\n{rite_output}",
    "procession": "review-changes"
  },
  {
    "name": "review-changes",
    "trigger": { "type": "watcher", "path": "src/" },
    "reap_interval_secs": 30,
    "prompt": "Code changed after CI failure. Review {files}."
  }
]
```

## Slash commands

All commands work from inside the TUI (`dirge --vigil`):

| Command | Description |
|---|---|
| `/vigil status` | Show all vigils: name, trigger, reap interval, active/paused/stopped |
| `/vigil pause <name>` | Pause a vigil. Triggers still fire but observances are suppressed. |
| `/vigil resume <name>` | Resume a paused vigil. |
| `/vigil remove <name>` | Remove a vigil entirely. |
| `/vigil add` | Add a vigil at runtime (interactive). |
| `/vigil start` | Start all vigils. |
| `/vigil stop` | Stop all vigils. |
| `/vigil rest` | Set a vigil's state to resting (completed its task). |

## Janet plugin integration

Janet plugins can push events into any vigil's queue via `(vigil/emit ...)`. The
plugin decides when to emit — polling an external API, receiving a webhook,
reacting to a condition — and the queue is the contract.

```janet
# Minimal plugin: poll Jenkins and emit failed builds
(defn poll-jenkins []
  (let [json-str (sh-capture "curl -s http://localhost:8080/api/json")]
    (each job (filter #(= "FAILURE" (get-in % [:lastBuild :result]))
                      (parse json-str :jobs))
      (vigil/emit "jenkins-remediate"
                  {:job (job :name)
                   :build_number (string (get-in job [:lastBuild :number]))
                   :url (get-in job [:lastBuild :url])
                   :status "FAILURE"}))))
```

Three lifecycle hooks are available:

- `on-vigil-event` — fired as an event enters the queue (return a table to
  enrich the context)
- `on-vigil-reap` — fired when the reaper drains a vigil
- `on-vigil-observance` — fired after the agent turn completes

```janet
(harness/register-hook "on-vigil-event" "my-enrich-fn")
```

## Process model

Startup: `dirge --vigil` loads vigils from `config.json` and `.dirge/vigils/*.json`,
merges them (config wins), creates per-vigil mpsc channels, spawns trigger tasks
(tokio intervals, inotify watchers, TCP listeners), spawns the reaper (per-vigil
`FuturesUnordered`), and enters the TUI loop.

At runtime: triggers push `VigilEvent` structs into their vigil's channel. The
reaper drains each channel on its own independent interval, coalesces events into
a `CoalescedBatch`, runs the rite if configured, and if the rite passes, wakes
the TUI loop to spawn an agent turn. After the turn, if a `procession` is set, an
event is injected into the next vigil's queue.

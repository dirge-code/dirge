# Vigil — Heartbeat & Wakeup Mode

## Motivation

`/loop` runs the agent continuously — turn after turn, chained, until a max iteration cap or manual stop. It works for autonomous task execution but has no pause between turns: the agent runs, the loop immediately launches the next iteration, rinse, repeat.

Two gaps:

- There is no mechanism to wake the agent up _on a trigger_ (timer, file change, network event), run one turn, then go back to sleep — monitoring, not looping.
- There is no conditional gate — no "check this first, only run the agent if the check says so."

Vigil fills both. It is a wakeup-and-sleep runtime that monitors triggers, queues events, reaps them on a configurable cadence, runs optional gates, dispatches the agent when gates pass, and returns to monitoring. The agent only runs when it should.

### Existing `/loop` extensions

Two flags retrofit onto the existing `--loop` mode:

- `--loop-oneshot` — run exactly one iteration then stop. Equivalent to `--loop-max 1`.
- `--loop-persist` — save the session to disk after each iteration so a later `--session` resume picks up the accumulated context. `--loop` currently runs entirely in memory with no persistence; `--loop-persist` makes each iteration a saved checkpoint in the same session (cumulative context between iterations). Without `--loop-persist`, each iteration is a fresh turn with no prior-turn history.

These are independent of vigil. The `--loop-oneshot` and `--loop-persist` flags are gated on `#[cfg(feature = "loop")]` since they extend the existing `--loop` mode. The vigil feature gate only gates the vigil runtime and slash commands.

## Theme

"Dirge" is both a funeral lament and a direction (from Latin _dirige_, "direct my path"). Vigil extends that: a vigil is the watch kept over the dead — staying awake, attentive, waiting. It is direction _through_ watchfulness.

The feature is named **vigil** — the period of watchful attention between agent turns.

- A watcher configuration is a **vigil**
- The runtime that monitors triggers and dispatches agent turns is the **vigil-keeper**
- When a vigil completes its task it is **laid to rest** (state `resting`)
- One vigil chaining to the next is a **procession**

## Architecture: Queue & Reaper Model

Triggers are _producers_, the reaper is the _consumer_. Each vigil has its own bounded mpsc channel. Trigger frequency and dispatch cadence are independently tunable.

```
┌──────────────────┐   ┌──────────────────┐   ┌──────────────────┐
│    toll (timer)   │   │ watcher (inotify)│   │harbinger (socket)│
│  tokio::interval  │   │  notify crate    │   │  TcpListener     │
└────────┬─────────┘   └────────┬─────────┘   └────────┬─────────┘
         │                      │                      │
         │  event_tx.send()     │  event_tx.send()     │  event_tx.send()
         ▼                      ▼                      ▼
    ┌────────────────┐  ┌────────────────┐  ┌────────────────┐
    │ mpsc(256)      │  │ mpsc(256)      │  │ mpsc(256)      │
    │ vigil: ci-watch│  │ vigil: review  │  │ vigil: webhook │
    └───────┬────────┘  └───────┬────────┘  └───────┬────────┘
            │                   │                   │
            ▼                   ▼                   ▼
    ┌──────────────────────────────────────────────────────┐
    │                   vigil-keeper                        │
    │                                                       │
    │  Per-vigil reap intervals (FuturesUnordered):         │
    │    ci-watch     → reap every 300s                     │
    │    review-src   → reap every 30s                      │
    │    webhook-9000 → reap every 10s                      │
    │                                                       │
    │  When a vigil's reap interval ticks:                  │
    │    1. drain its channel → Vec<VigilEvent>             │
    │    2. coalesce()       → one CoalescedBatch           │
    │    3. run rite (optional gate)                        │
    │    4. if rite passes → spawn observance (agent turn)  │
    │    5. if procession set → inject event into next      │
    │       vigil's channel (bypassing its trigger)         │
    └──────────────────────────────────────────────────────┘
```

Key: there is no single `select!` loop over steps. Each vigil's reap interval fires independently via `FuturesUnordered`; the vigil-keeper is a `loop` that drives `FuturesUnordered::next()`.

### Why per-vigil channels?

**Isolation.** Each vigil has a dedicated `mpsc::channel::<VigilEvent>(256)`. A shared channel would let one vigil's reap drain another vigil's events — `try_recv()` takes whatever is at the head of the queue, with no way to filter by vigil name. Per-vigil channels eliminate this race: each reap drains only its own events.

**Decoupling.** Triggers fire at their natural cadence (timer ticks, inotify events, socket connections). The reaper consolidates them on its own schedule. A CI toll that fires every 5 minutes and a file watcher that fires on every save don't compete — each pushes into its own channel, and each reaper drains independently.

**Coalescing.** 15 `watcher` events for `ci-watch` in one reap window become _one_ observance with batched context:

```
{files: ["src/main.rs", "src/lib.rs", "src/tests.rs"], event_count: 15, events: ["modify", "modify", "create"]}
```

The agent sees the full picture, not one change at a time.

**Backpressure.** Producers use `try_send()` — a non-blocking send that returns `Err(TrySendError::Full(event))` when the bounded channel (256) is full. On `Full`, the oldest event in the channel is popped and dropped (a `tracing::warn!` logs the loss), then `try_send` is retried. This gives ring-buffer semantics without a specialized data structure: the channel stays at capacity, fresh events push out stale ones, and memory is bounded.

### Per-vigil reap interval

Each vigil has its own `reap_interval_secs` field. Defaults to 30s when unset. A CI toll at 300s, a file-review watcher at 30s, a webhook at 10s — each reaps independently. The vigil-keeper uses `FuturesUnordered<tokio::time::Interval>` to manage multiple independent intervals.

### Toll interval vs. reap interval

For toll (timer) triggers, `interval_secs` on the trigger controls how often an event is _produced_ (pushed into the channel), while `reap_interval_secs` controls how often the reaper _drains_ and dispatches. They are independent: a toll with `interval_secs: 60` and `reap_interval_secs: 300` produces 5 events per reap window, which are coalesced into one observance.

When both values are equal (e.g., both 300), the timers are in lockstep — one event per reap window, prompt and simple.

### Trigger types

- **toll** (timer/poll): Fires on a fixed interval (like a bell tolling).
- **watcher** (inotify/filesystem): Fires when files change in a watched path.
- **harbinger** (socket/network): Fires on an incoming TCP or Unix socket connection.

### Stages per reap

- **Queue drain** — pull all events from this vigil's dedicated mpsc channel.
- **Coalesce** — merge N events into one `CoalescedBatch` with aggregated context (deduplicated file list, event counts, per-trigger payloads).
- **Rite** (gate check) — optional shell command and optional git dirty check. If the rite fails, the observance is skipped and the batch is discarded.
- **Observance** (agent turn) — the agent runs one turn against the prompt (template-substituted from the coalesced context).
- **Procession** — if `procession` is set, the vigil-keeper injects an event directly into the next vigil's queue (bypassing its trigger — this is a forced wake), and the next vigil will process it on its own next reap. If procession is not set, return to sleep.

### Socket dispatch modes

Socket vigils (harbingers) have a `socket_mode` field on the trigger variant itself:

- `template` (default): The raw payload substitutes into `{harbinger_data}` in the prompt, then the agent turn runs.
- `commands`: The payload is parsed as JSON `{"command":"<name>","args":{...}}` and dispatched against a pre-registered command map — no agent turn, no LLM cost.

`socket_mode` only exists on `VigilTrigger::Harbinger`, making the invalid combination (`toll` with `commands`) unrepresentable.

### Security model for `commands` mode

`commands` mode replaces the earlier `tool_call` design. The problem with `tool_call` + allowlist: limiting *which tool* is called doesn't limit what the tool *does*. `{"tool":"bash","args":{"command":"rm -rf /"}}` passes a `bash` allowlist. The attack surface lives in the arguments, not the tool name.

**Named commands.** Every `commands` harbinger MUST specify a non-empty `commands` map — a dictionary of named commands, each defining a fully-specified tool call with optional `{arg_name}` template placeholders. The vigil-keeper rejects at startup any `commands` harbinger with an empty or absent command map.

The socket payload carries only a command name and a flat args dictionary:

```json
{"command": "build", "args": {"release": true}}
```

The vigil-keeper looks up `"build"` in the command map, substitutes `{release}` in the template, and dispatches the result. The caller never provides a tool name, never constructs raw argument strings — they pick from a menu and fill in pre-declared slots.

Example: a CI hook with three commands:

```json
{
  "name": "ci-hook",
  "trigger": {
    "harbinger": {
      "address": "127.0.0.1:9090",
      "protocol": "tcp",
      "socket_mode": "commands",
      "commands": {
        "build": {
          "tool": "bash",
          "args": {
            "command": "cargo build{release}",
            "description": "build {release}"
          }
        },
        "test": {
          "tool": "bash",
          "args": {
            "command": "cargo test{test_name}",
            "description": "run {test_name}"
          }
        },
        "lint": {
          "tool": "bash",
          "args": {
            "command": "cargo clippy -- -D warnings"
          }
        }
      }
    }
  },
  "prompt": ""
}
```

Template substitution rules:

- `{arg_name}` in any string value inside `args` is replaced with the corresponding value from the socket payload's `args` dict.
- Missing optional args are replaced with the empty string: `{release}` → `""` when `release` is absent or `false`, `" --release"` when `true`.
- Unknown args (not present in any template) are ignored with a `tracing::debug!` log — no error, forward-compatible.
- Template substitution is string-level only; it cannot change JSON structure. You can't inject `{"tool": "write"}` via a template value because the tool is fixed in the command definition.
- The `lint` command has no templates — it's fully static. Any `args` in the payload are ignored.

Socket payloads for the example above:

```json
{"command": "build", "args": {"release": true}}
→ bash -c "cargo build --release"

{"command": "test", "args": {"test_name": " -- --nocapture"}}
→ bash -c "cargo test -- --nocapture"

{"command": "lint"}
→ bash -c "cargo clippy -- -D warnings"

{"command": "unknown"}
→ rejected with warning log (not in command map)

{"command": "build", "args": {"release": "; rm -rf /"}}
→ bash -c "cargo build ; rm -rf /"
→ this is still dangerous — arg values are raw strings. See mitigation below.
```

**Shell injection in template values.** The caller controls the substituted values, so `{"release": "; rm -rf /"}` does inject into the shell command. This is inherent to string templating. The defense is the same as for any CI system: the commands are defined by the project owner, the socket is loopback-only, and the caller is a trusted local process. For untrusted callers, use static commands (no templates, like `lint` above) so the caller provides nothing but the command name.

**Binding constraints.** Harbinger vigils with `socket_mode: "commands"` must bind to `127.0.0.1` (loopback-only) or a Unix domain socket with restrictive filesystem permissions. The vigil-keeper rejects at startup any `commands` harbinger bound to a non-loopback address.

**No auto-confirm.** If a dispatched tool call would require permission elevation (a `harness/confirm` dialog), it is denied. `commands` mode auto-confirms nothing. The tool must be pre-approved by the user's existing permission config.

**Audit logging.** Every `commands` dispatch is logged at `info!` level with the source address, command name, substituted tool call, and any ignored unknown args.

## Janet Plugin API

External engine adapters (Prefect, Airflow, Jenkins, custom webhooks) live in Janet plugins, not in Rust. The Rust core provides the queue and dispatch machinery; Janet plugins hook into it at three lifecycle points and expose four functions for custom control.

### Lifecycle hooks

| Hook | When fired | Janet context | Return value |
|---|---|---|---|
| `on-vigil-event` | Event enters the queue (pre-push) | See per-trigger shapes below | Modified context map (merged into the event), or `nil` to pass through unchanged |
| `on-vigil-reap` | Reaper drains for a vigil, pre-rite | `{:vigil "ci-watch" :count 3 :files ["src/a.rs" "src/b.rs"] :trigger :watcher}` | `nil` (fire-and-forget) |
| `on-vigil-observance` | Agent turn completes | `{:vigil "ci-watch" :response "Tests passed." :exit :ok :rite_passed true}` | `nil` (fire-and-forget) |

**Return value semantics for `on-vigil-event`:** If the hook returns a Janet table, its keys are shallow-merged into the event's context before the event is pushed into the queue. This allows plugins to enrich, transform, or annotate events without replacing the core fields (`:vigil`, `:trigger`, `:timestamp`). If the hook returns `nil`, the event passes through unchanged.

**Per-trigger `on-vigil-event` context shapes:**

For a **toll** trigger:

```
{:vigil "ci-watch" :trigger :toll :timestamp "2026-08-15T14:00:00Z"}
```

For a **watcher** trigger (single event, pre-coalesce):

```
{:vigil "review-src" :trigger :watcher :file "src/main.rs" :event "modify" :timestamp "2026-08-15T14:00:01Z"}
```

For a **harbinger** trigger:

```
{:vigil "automation-hook" :trigger :harbinger :harbinger_data "{\"command\":\"build\",\"args\":{\"release\":true}}" :timestamp "2026-08-15T14:00:02Z"}
```

### Janet functions exposed from Rust

| Function | Signature | Purpose |
|---|---|---|
| `vigil/emit` | `(vigil/emit name context)` | Push an event into a vigil's queue — the core extension point for custom triggers |
| `vigil/list` | `(vigil/list)` → `[{:name "ci-watch" :state :active :trigger :toll} ...]` | Return all vigils |
| `vigil/set-state` | `(vigil/set-state name state)` | Set state: `:active`, `:paused`, `:resting` |
| `vigil/get` | `(vigil/get name)` → `{:name "ci-watch" :state :active :trigger :toll ...}` | Get a single vigil by name, or `nil` if not found |

`vigil/emit` pushes one event into the named vigil's bounded mpsc channel. It is the sole entry point for external triggers — a Janet plugin that wants to inject events calls `(vigil/emit ...)`. How that plugin decides _when_ to emit (polling an external API, receiving its own webhook, etc.) is the plugin's responsibility. The queue is the contract.

If the named vigil does not exist, `vigil/emit` returns `:not-found`. Dynamic vigil registration from Janet is a Phase 2 concern exposed via `vigil/register`.

### Example: Prefect adapter plugin

A `.janet` file that spawns a polling loop and emits events into the vigil queue. The polling loop is Janet's problem; `vigil/emit` is Rust's contract.

```janet
# Runs on plugin load. Spawns a background fiber that polls Prefect.
(defn start-prefect-poller []
  (ev/spawn
    (fn []
      (loop
        (each run (prefect/fetch-failed-runs)
          (vigil/emit "prefect-remediate"
                      {:run_id (run :id)
                       :flow_name (run :flow_name)
                       :error (run :error)}))
        (ev/sleep 60)))))

# The vigil-keeper starts toll vigils from its own config. This plugin
# just publishes events — the reaper picks them up on the vigil's reap
# interval. No vigil registration API needed in Phase 1.
```

### Example: Jenkins webhook adapter

A harbinger vigil receives Jenkins build notifications. The Janet `on-vigil-event` hook transforms the Jenkins-specific payload into the standard vigil context:

```janet
(defn jenkins-transform [ctx]
  (let [raw (ctx :harbinger_data)
        parsed (json/decode raw)]
    # Return a modified context — the vigil-keeper shallow-merges this
    # into the event before it enters the queue.
    {:build_number (parsed :number)
     :job_name (parsed :job)
     :console_url (parsed :console_url)}))

(harness/register-hook "on-vigil-event" "jenkins-transform")
```

## Configuration

Vigils are defined in three places and merged into the database at startup:

### A) `config.json` — inline `vigils` block

```json
{
  "vigils": [
    {
      "name": "ci-watch",
      "trigger": { "toll": { "interval_secs": 300 } },
      "reap_interval_secs": 300,
      "rite": { "cmd": "cargo test --quiet 2>&1" },
      "prompt": "Tests failed. Investigate and fix.\n\nTest output:\n{rite_output}",
      "procession": "review-changes"
    },
    {
      "name": "review-changes",
      "trigger": { "watcher": { "path": "src/", "events": ["modify"] } },
      "reap_interval_secs": 30,
      "prompt": "Code changed after CI failure. Review {files} and assess."
    },
    {
      "name": "automation-hook",
      "trigger": {
        "harbinger": {
          "address": "127.0.0.1:9090",
          "protocol": "tcp",
          "socket_mode": "commands",
          "commands": {
            "build": {
              "tool": "bash",
              "args": { "command": "cargo build" }
            }
          }
        }
      },
      "reap_interval_secs": 10,
      "prompt": ""
    }
  ]
}
```

Note: `socket_mode` and `commands` live inside the `harbinger` trigger object, not at the top-level `VigilEntry`. `commands` is required when `socket_mode` is `"commands"` — the vigil-keeper rejects at startup any `commands` harbinger without a non-empty command map.

### B) `.dirge/vigils/*.json` — filesystem vigils

One vigil per JSON file, same shape as above. File name is the vigil name. Auto-discovered at startup alongside the config block. Mirrors `.dirge/skills/` and `.dirge/agents/` patterns.

Example: `.dirge/vigils/ci-watch.json`

```json
{
  "trigger": { "toll": { "interval_secs": 300 } },
  "reap_interval_secs": 300,
  "rite": { "cmd": "cargo test --quiet 2>&1" },
  "prompt": "Tests failed. Investigate and fix.\n\nTest output:\n{rite_output}",
  "procession": "lint-check"
}
```

### C) Database (runtime state)

The canonical registry is the `vigils` table in `state.db`. Config and filesystem files are import sources — they seed the DB. `/vigil add` writes directly to DB. State (`active`/`paused`/`resting`) is DB-authoritative and survives restarts.

### Config structs (Rust)

```rust
// In Config (src/config/mod.rs)
#[cfg(feature = "vigil")]
pub vigils: Option<Vec<VigilEntry>>,

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VigilTrigger {
    Toll { interval_secs: u64 },
    Watcher { path: String, events: Vec<String> },
    Harbinger {
        address: String,
        protocol: String,
        #[serde(default)]
        socket_mode: SocketMode,
        /// Required when socket_mode is Commands. The vigil-keeper rejects
        /// a Commands harbinger with an empty map at startup.
        #[serde(default)]
        commands: HashMap<String, VigilCommand>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VigilRite {
    pub cmd: Option<String>,
    pub git_dirty: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SocketMode {
    #[default]
    Template,
    Commands,
}

/// A pre-registered named command for socket_mode: "commands".
/// The caller provides a command name; the vigil-keeper looks up the
/// template and substitutes {arg_name} placeholders from the socket payload.
#[derive(Debug, Clone, Deserialize)]
pub struct VigilCommand {
    pub tool: String,
    /// String values in args may contain {arg_name} templates.
    pub args: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VigilEntry {
    pub name: String,
    pub trigger: VigilTrigger,
    #[serde(default = "default_reap_interval")]
    pub reap_interval_secs: u64,
    #[serde(default)]
    pub rite: VigilRite,
    pub prompt: String,
    pub procession: Option<String>,
}

fn default_reap_interval() -> u64 { 30 }
```

### VigilRunState (runtime tracking)

This type is held by the vigil-keeper at runtime and surfaced to the post-turn dispatch so `decide_post_done_action` knows whether a vigil observance just completed:

```rust
// In src/extras/vigil/types.rs
#[derive(Debug, Clone)]
pub struct VigilRunState {
    /// Whether the vigil-keeper is currently running (i.e., an observance
    /// just completed and we should return to sleep rather than idle).
    pub active: bool,
    /// The name of the vigil whose observance just completed.
    pub current_vigil: Option<String>,
    /// The event queue sender for the vigil-keeper's control channel.
    pub ctl_tx: Option<tokio::sync::mpsc::Sender<VigilCtl>>,
}
```

### VigilState (UI-layer runtime tracking)

This type is held by the TUI event loop and surfaced to slash commands via `SlashCtx`. It is separate from `VigilRunState` — `VigilRunState` lives inside the vigil-keeper, while `VigilState` is the UI's view of vigil activity:

```rust
// In src/extras/vigil/mod.rs
#[derive(Debug, Clone)]
pub struct VigilState {
    /// Whether the vigil-keeper is currently active (an observance
    /// just completed and we should return to sleep rather than idle).
    pub active: bool,
    /// If set, the current agent turn is a vigil observance. The post-turn
    /// handler reads this to dispatch `on-vigil-observance` with the agent's
    /// response text. Cleared after dispatch.
    pub pending_observance: Option<PendingObservance>,
}
```

`pending_observance` carries the vigil name and event count from the reaper to the post-turn handler, so the `on-vigil-observance` Janet hook can be dispatched with the agent's response and exit code.

### SQLite schema (in `.dirge/sessions/state.db`)

```sql
CREATE TABLE IF NOT EXISTS vigils (
    id                  TEXT PRIMARY KEY,
    name                TEXT NOT NULL UNIQUE,
    trigger_type        TEXT NOT NULL,          -- 'toll' | 'watcher' | 'harbinger'
    trigger_config      TEXT NOT NULL,          -- JSON blob
    reap_interval_secs  INTEGER DEFAULT 30,
    rite_cmd            TEXT,
    rite_git            INTEGER DEFAULT 0,
    prompt              TEXT NOT NULL,
    procession          TEXT,                   -- next vigil name (flat chain)
    state               TEXT DEFAULT 'active',  -- active | paused | resting | error
    laid_to_rest_at     TEXT,
    laid_to_rest_by     TEXT,                   -- session id
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_vigils_state ON vigils(state);
```

Note: `socket_mode` and `commands` are stored inside `trigger_config` JSON (part of the harbinger blob), not as top-level columns.

## Lifecycle

- **`active`** — Watching, will fire on trigger, reaper picks it up. Default state.
- **`paused`** — Suspended, retains config, reaper skips it. Set via `/vigil pause <name>`.
- **`resting`** — Task complete, soft-closed. The row stays for audit. Set via `/vigil rest <name>` or agent tool. `/vigil resume <name>` flips back to `active`.
- **`error`** — Failed to start or rite errored persistently. Set by runtime.

### Import merge behavior

At startup, the vigil-keeper:

1. Opens the `vigils` table (creates if needed)
2. Reads existing vigils into memory
3. Scans `Config::vigils` — for each: update if `name` matches (preserving `state`/`laid_to_rest_at`/`laid_to_rest_by`), insert if new
4. Scans `.dirge/vigils/*.json` — same merge logic
5. Validates each vigil: `commands` harbingers must have a non-empty `commands` map and bind only to loopback/Unix addresses. Invalid vigils are logged at `error!` and skipped.
6. Registers each valid active vigil with the reaper, spawning its trigger producer, mpsc channel, and reap interval

## CLI

```bash
# Headless vigil mode — reads vigils from config + .dirge/vigils/
dirge --vigil

# Import vigils from a file on startup
dirge --vigil --vigil-config vigils.json

# Manage vigils
dirge vigil add toll --interval 300 --reap 300 --rite "cargo test --quiet" --prompt "Fix failing tests"
dirge vigil add watcher --path src/ --events modify --reap 30 --prompt "Review changes in {files}"
dirge vigil add harbinger --port 9090 --reap 10 --socket-mode commands --commands '{"build":{"tool":"bash","args":{"command":"cargo build"}}}'
dirge vigil list
dirge vigil remove <name>
dirge vigil pause <name>
dirge vigil resume <name>
dirge vigil rest <name>

# Extended --loop flags (gated on #[cfg(feature = "loop")], NOT vigil)
dirge --loop --loop-oneshot --loop-prompt "Fix the build"
dirge --loop --loop-persist --loop-prompt "Iterative refactor"
```

### CLI struct additions

```rust
// src/cli.rs

// --loop-oneshot and --loop-persist are gated on 'loop', not 'vigil':
#[cfg(feature = "loop")]
#[arg(long = "loop-oneshot", help = "Run exactly one loop iteration then stop")]
pub loop_oneshot: bool,

#[cfg(feature = "loop")]
#[arg(long = "loop-persist", help = "Persist session to disk each iteration")]
pub loop_persist: bool,

// Vigil flags are gated on 'vigil':
#[cfg(feature = "vigil")]
#[arg(long = "vigil", help = "Run in vigil mode (heartbeat/wakeup)")]
pub vigil_mode: bool,

#[cfg(feature = "vigil")]
#[arg(long = "vigil-config", help = "JSON file with vigil definitions")]
pub vigil_config: Option<PathBuf>,

#[command(subcommand)]
pub command: Option<Command>,

// New subcommand variant (inside Command enum):
#[cfg(feature = "vigil")]
Vigil {
    #[command(subcommand)]
    action: VigilAction,
},

// VigilAction enum:
#[derive(Debug, Subcommand)]
pub enum VigilAction {
    Add {
        #[command(subcommand)]
        trigger: VigilAddTrigger,
        #[arg(long = "reap", default_value = "30")]
        reap_interval_secs: u64,
        #[arg(long = "rite")]
        rite_cmd: Option<String>,
        #[arg(long = "prompt")]
        prompt: String,
        #[arg(long = "procession")]
        procession: Option<String>,
    },
    List,
    Remove { name: String },
    Pause { name: String },
    Resume { name: String },
    Rest { name: String },
}

#[derive(Debug, Subcommand)]
pub enum VigilAddTrigger {
    Toll {
        #[arg(long = "interval")]
        interval_secs: u64,
    },
    Watcher {
        #[arg(long = "path")]
        path: String,
        #[arg(long = "events")]
        events: Vec<String>,
    },
    Harbinger {
        #[arg(long = "port")]
        port: u16,
        #[arg(long = "socket-mode", default_value = "template")]
        socket_mode: String,
        #[arg(long = "commands", help = "JSON map of named commands (required for socket-mode=commands)")]
        commands: Option<String>,
    },
}
```

## TUI

```
/vigil start [name]         Start all vigils or a named one
/vigil stop [name]          Stop
/vigil pause <name>         Suspend a vigil
/vigil resume <name>        Resume a paused or resting vigil
/vigil rest <name>          Lay a vigil to rest (soft-complete)
/vigil status               List all vigils with states
/vigil add toll --interval 60 --reap 60 --prompt "…" [--procession <next>]
/vigil add watcher --path src/ --events modify --reap 30 --prompt "…"
/vigil add harbinger --port 9090 --reap 10 --prompt "…" [--socket-mode commands --commands '{"build":{...}}']
/vigil remove <name>
```

## Prompt template variables

Available in the `prompt` field of a vigil entry. After coalescing, batched context is available for files and events:

- `{name}` — Vigil name
- `{files}` — Changed file paths as comma-separated list (watcher trigger, coalesced)
- `{events}` — Event types as comma-separated list (watcher: create/modify/delete)
- `{event_count}` — Number of events in this reap window
- `{timestamp}` — ISO 8601 time of reap
- `{rite_output}` — stdout+stderr from the rite command
- `{rite_exit_code}` — Exit code from the rite command
- `{harbinger_data}` — Raw payload from socket connection (first connection in reap window)

## Post-turn behavior

A new `PostDoneAction::VigilSleep` variant signals that the observance is complete and the vigil-keeper should return to monitoring. No loop, no followup.

```rust
pub enum PostDoneAction {
    Followup(String),
    LoopIter,
    LoopStop,
    VigilSleep,   // NEW — gated behind #[cfg(feature = "vigil")]
    Idle,
}

pub fn decide_post_done_action(
    followup: Option<String>,
    loop_active: bool,
    loop_should_stop: bool,
    #[cfg(feature = "vigil")] vigil_active: bool,   // NEW — cfg-gated parameter
) -> PostDoneAction {
    if let Some(text) = followup {
        return PostDoneAction::Followup(text);
    }
    #[cfg(feature = "vigil")]
    if vigil_active {
        return PostDoneAction::VigilSleep;
    }
    if !loop_active {
        return PostDoneAction::Idle;
    }
    if loop_should_stop {
        PostDoneAction::LoopStop
    } else {
        PostDoneAction::LoopIter
    }
}
```

**Why cfg-gate the parameter rather than the whole function:** The function is called unconditionally from `done.rs`. Adding a non-cfg-gated `vigil_active: bool` parameter forces every call site (including the test at `mod_tests.rs:134`) to pass a fourth argument, even when the `vigil` feature is disabled. By cfg-gating the parameter itself, the function has 3 parameters without the feature and 4 with it — call sites use the same `#[cfg]` pattern to match.

**Call site pattern in `src/ui/run_handlers/done.rs`:**

```rust
#[cfg(feature = "vigil")]
let vigil_active = vigil_bits
    .state
    .as_ref()
    .is_some_and(|v| v.active);
#[cfg(not(feature = "vigil"))]
let vigil_active = ();  // unused — cfg-gated parameter will be absent

let action = crate::plugin::decide_post_done_action(
    followup_for_decision,
    loop_active,
    loop_should_stop,
    #[cfg(feature = "vigil")]
    vigil_active,
);
```

**Test update in `src/plugin/mod_tests.rs`:** The `test_post_done_action` test (line 134) must be updated with `#[cfg]` branches. Without the `vigil` feature, the test is unchanged (3 parameters). With `vigil`, a new assertion is added for the `VigilSleep` variant:

```rust
#[test]
fn test_post_done_action() {
    let followup = Some("retry".to_string());
    assert_eq!(
        decide_post_done_action(followup.clone(), true, false),
        PostDoneAction::Followup("retry".into())
    );
    assert_eq!(
        decide_post_done_action(followup.clone(), false, false),
        PostDoneAction::Followup("retry".into())
    );
    // Loop iteration only when no followup.
    assert_eq!(
        decide_post_done_action(None, true, false),
        PostDoneAction::LoopIter
    );
    // Loop stop only when no followup and should_stop.
    assert_eq!(
        decide_post_done_action(None, true, true),
        PostDoneAction::LoopStop
    );
    // Idle: nothing to do.
    assert_eq!(
        decide_post_done_action(None, false, false),
        PostDoneAction::Idle
    );

    #[cfg(feature = "vigil")]
    {
        // VigilSleep: vigil active outranks loop.
        assert_eq!(
            decide_post_done_action(None, true, false, true),
            PostDoneAction::VigilSleep
        );
        // Followup still beats vigil.
        assert_eq!(
            decide_post_done_action(followup.clone(), false, false, true),
            PostDoneAction::Followup("retry".into())
        );
    }
}
```

## File map

- `Cargo.toml` — `notify` dep, `vigil` feature gate, feature lists, `check-cfg`
- `src/cli.rs` — `--vigil`, `--vigil-config`, `Vigil` subcommand + `VigilAction` enum; `--loop-oneshot`, `--loop-persist` (gated on `loop`)
- `src/config/mod.rs` — `VigilEntry`, `VigilTrigger` (with `Harbinger { socket_mode, commands }`), `VigilRite`, `SocketMode`, `VigilCommand` types; `Config::vigils` field
- `src/extras/dirge_paths.rs` — `vigils_dir()` → `.dirge/vigils/`
- `src/extras/vigil_db.rs` — `VigilStore` — SQLite CRUD over `state.db`, follows `IssueStore` pattern
- `src/extras/vigil/types.rs` — `VigilConfig`, `VigilPayload`, `VigilEvent` (mpsc message), `TriggerType`, `RiteResult`, `CoalescedBatch`, `VigilRunState`
- `src/extras/vigil/rite.rs` — `run_rite(cfg, batch) -> RiteResult` — shell command + git dirty
- `src/extras/vigil/dispatch.rs` — `build_prompt()`, `dispatch_commands()` (command-map lookup + template substitution), `run_observance()`
- `src/extras/vigil/toll.rs` — `spawn_toll()` — `tokio::time::interval`, pushes `VigilEvent` into its vigil's channel via `try_send` with overflow-drop
- `src/extras/vigil/watcher.rs` — `spawn_watcher()` — `notify` crate, 500ms debounce, pushes into its vigil's channel
- `src/extras/vigil/harbinger.rs` — `spawn_harbinger()` — `TcpListener`/`UnixListener`, 5s read timeout, loopback-only + non-empty command-map enforcement for `commands` mode, pushes into its vigil's channel
- `src/extras/vigil/reaper.rs` — `VigilReaper` — per-vigil reap intervals via `FuturesUnordered`, `drain_queue()`, `coalesce_by_vigil()`, rite check, dispatch, procession
- `src/extras/vigil/mod.rs` — `VigilKeeper` — wires producers (each with own channel) + reaper + dispatch, import merge, startup validation
- `src/plugin/mod.rs` — `PostDoneAction::VigilSleep` variant; extend `decide_post_done_action` with cfg-gated `vigil_active` parameter
- `src/plugin/loader.rs` — hook registration for `on-vigil-event`, `on-vigil-reap`, `on-vigil-observance`
- `src/plugin/worker.rs` — Janet functions: `vigil/emit`, `vigil/list`, `vigil/set-state`, `vigil/get`
- `src/ui/run_handlers/done.rs` — Handle `VigilSleep`: reset `is_running`; cfg-gated `vigil_active` at call site
- `src/ui/slash/cmd/vigil_cmd/mod.rs` — `/vigil` dispatch
- `src/ui/slash/cmd/vigil_cmd/add.rs` — `/vigil add toll|watcher|harbinger`
- `src/ui/slash/cmd/vigil_cmd/` — `start.rs`, `stop.rs`, `status.rs`, `rest.rs`, `pause.rs`, `resume.rs`, `remove.rs`
- `src/ui/slash/mod.rs` — `vigil_state` in `SlashCtx`, dispatch `/vigil`
- `src/ui/mod.rs` — `vigil_rx` arm in main `select!` loop
- `src/main.rs` — `--vigil` entry point, `--loop-oneshot`/`--loop-persist` wiring
- `src/extras/mod.rs` — `pub mod vigil_db;` (ungated, no deps beyond rusqlite) + `#[cfg(feature = "vigil")] pub mod vigil;`
- `specs/vigil.allium` — Formal Allium v3 specification for the vigil feature (Phase 1 deliverables)

## Implementation plan

### Phase 1: Foundation

1. **Cargo.toml** — `notify` dep, `vigil` feature, add to `default` + `no-plugin` + `check-cfg`
2. **`--loop-oneshot` / `--loop-persist`** — `cli.rs` + `main.rs` (gated on `loop`, not `vigil`)
3. **Config types** — `VigilEntry`, `VigilTrigger` (with `Harbinger { socket_mode, commands }`), `VigilRite`, `SocketMode`, `VigilCommand` in `config/mod.rs`
4. **`vigils_dir()`** — add to `dirge_paths.rs`

### Phase 2: Database

5. **`vigil_db.rs`** — `VigilStore` with full SQLite CRUD, lifecycle states. Add `pub mod vigil_db;` to `extras/mod.rs` (ungated).
6. **`extras/mod.rs` vigil module** — `#[cfg(feature = "vigil")] pub mod vigil;`

### Phase 3: Runtime (queue + reaper)

7. **`vigil/types.rs`** — `VigilConfig`, `VigilPayload`, `VigilEvent` (mpsc message), `CoalescedBatch`, `VigilRunState`, `VigilCtl`
8. **`vigil/rite.rs`** — gate runner
9. **`vigil/dispatch.rs`** — prompt builder, commands dispatch (command-map lookup + template substitution), `run_observance()`
10. **`vigil/toll.rs`** — timer producer, each vigil gets its own `mpsc::channel(256)`, pushes `VigilEvent` via `try_send` with overflow-drop
11. **`vigil/watcher.rs`** — inotify producer with own per-vigil channel, 500ms debounce, pushes via `try_send`
12. **`vigil/harbinger.rs`** — socket producer with own per-vigil channel (template + commands), loopback-only + non-empty command-map enforcement for `commands` mode, pushes via `try_send`
13. **`vigil/reaper.rs`** — `VigilReaper`: per-vigil reap intervals via `FuturesUnordered`, `drain_queue()` from vigil's own channel, `coalesce_by_vigil()`, rite check, spawn observance, procession (inject event into next vigil's channel)
14. **`vigil/mod.rs`** — `VigilKeeper::run()` — wires producers (each with own channel) + reaper + dispatch, import merge from config and filesystem

### Phase 4: Plugin integration

15. **Plugin hooks** — `on-vigil-event`, `on-vigil-reap`, `on-vigil-observance` registered in `plugin/loader.rs`, dispatched from the reaper; `on-vigil-event` return value shallow-merged into event context
16. **Janet functions** — `vigil/emit`, `vigil/list`, `vigil/set-state`, `vigil/get` in `plugin/worker.rs`
17. **`PostDoneAction::VigilSleep`** — cfg-gated variant + cfg-gated `decide_post_done_action` parameter in `plugin/mod.rs`; cfg-gated `vigil_active` at call site in `done.rs`; test update in `mod_tests.rs`

### Phase 5: CLI + TUI

18. **CLI** — `--vigil` flag + `vigil` subcommand + `VigilAction` enum in `cli.rs` + `main.rs`
19. **`/vigil` slash command** — full subcommand set in `ui/slash/cmd/vigil_cmd/`; register in `slash/mod.rs` and `slash/cmd/mod.rs`
20. **TUI wiring** — `vigil_rx` arm in `ui/mod.rs` event loop

### Phase 6: Specification

21. **Allium spec** — Write `specs/vigil.allium` (v3) covering toll timer, watcher inotify, harbinger socket with `commands` dispatch and template substitution security constraints, queue backpressure, per-vigil channel isolation, and rite gating.

## Risks

- **Inotify flood**: A formatter touching 50 files fires 50 events. Mitigated by 500ms debounce _before_ the event enters the queue, plus coalescing at reap time so the agent sees one batch, not 50 turns.
- **Harbinger read hang**: Unbounded `read_to_string` on TCP can block. Mitigated by `tokio::time::timeout` (5s default, configurable).
- **Observance blocks reaping**: Agent turns are async. Mitigated by spawning each observance on a detached `tokio::spawn`; the reaper continues looping `FuturesUnordered`. One observance per vigil at a time, gated by `AtomicBool` running flag — a second reap for the same vigil while its observance is still running is skipped.
- **Queue overflow**: Producers use `try_send()`. When a vigil's bounded channel (256) is full, the oldest event is popped and dropped with a `tracing::warn!` log, then the new event is pushed. This bounds memory per vigil and signals tuning pressure: shorten the reap interval or increase the channel bound.
- **Commands dispatch**: `socket_mode: "commands"` is restricted to loopback/Unix sockets only and requires a non-empty `commands` map. Every message is validated against the command map at dispatch time — only pre-registered command names are accepted. Template substitution is string-level and caller-controlled arg values are raw strings; for untrusted callers, use static commands (no templates) so the caller provides nothing but the command name. Permission escalation is blocked (no auto-confirm).
- **Feature creep**: DAG complexity. Mitigated by flat `procession` field — one vigil chains to at most one next vigil. External engines (Prefect, Airflow, Jenkins) plug in via Janet `vigil/emit`, not in Rust — the queue is the extensibility boundary.

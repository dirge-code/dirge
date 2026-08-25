# Vigil Functional Test Configs

Four vigil configurations covering all trigger modes plus three workflow engine
integrations (Jenkins, Prefect, Airflow). Copy configs to `.dirge/vigils/` and
run `dirge --vigil` to test each mode.

## Files

- `sanity-toll.json` — Timer trigger, fires every 10s
- `sanity-watcher.json` — Filesystem trigger, watches `watch-dir/`
- `sanity-harbinger-template.json` — TCP socket, template mode (port 9090)
- `sanity-harbinger-commands.json` — TCP socket, commands mode (port 9091)
- `sanity-jenkins.json` — Vigil config for `jenkins-remediate`
- `sanity-prefect.json` — Vigil config for `prefect-remediate`
- `sanity-airflow.json` — Vigil config for `airflow-remediate`
- `setup-jenkins.sh` — Creates failing Jenkins job + build
- `setup-airflow.sh` — Configures Airflow basic auth, creates failing DAG + run
- `setup-prefect.sh` — Creates failing Prefect flow run
- `podman-compose.yml` — Jenkins (8080), Prefect Server (4200), Airflow (8081)
- `plugins/` — Janet poller plugins for each engine

## Setup

```bash
# Copy vigils into the project's runtime directory
cp tests/fixtures/vigil/sanity-*.json .dirge/vigils/

# Verify they were picked up
dirge vigil list
```

## Test: Toll (timer)

The toll vigil fires every 10 seconds. Start dirge in vigil mode and
check the status panel:

```bash
dirge --vigil
# In the TUI: /vigil status
# Expect: sanity-toll status=active, trigger=toll
# Every 10s the rite runs `echo 'toll-ok'` and the reaper fires.
# /vigil pause sanity-toll   — to stop
# /vigil resume sanity-toll  — to restart
```

## Test: Watcher (filesystem)

```bash
dirge --vigil
# In another terminal, trigger a file change:
touch tests/fixtures/vigil/watch-dir/trigger.txt
echo "changed" >> tests/fixtures/vigil/watch-dir/trigger.txt

# In the TUI: /vigil status
# Expect: sanity-watcher status=active, trigger=watcher
# The watcher fires on modify events; the 500ms debounce coalesces
# rapid changes. After the reap interval, an observance fires.
```

## Test: Harbinger (template mode)

```bash
dirge --vigil
# In another terminal, send data to the socket:
echo '{"message":"hello from template mode"}' | nc -w1 127.0.0.1 9090

# The raw payload substitutes into {harbinger_data} in the prompt:
#   "Harbinger received: {\"message\":\"hello from template mode\"}"
```

## Test: Harbinger (commands mode)

```bash
dirge --vigil
# In another terminal, send a command:
echo '{"command":"echo","args":{"message":"hello world"}}' | nc -w1 127.0.0.1 9091

# The vigil-keeper looks up "echo" in the commands map, substitutes
# {message} → "hello world", and dispatches:
#   bash -c "echo 'commands-ok: hello world'"

# Static command (no args needed):
echo '{"command":"ping"}' | nc -w1 127.0.0.1 9091
# → bash -c "echo 'pong'"

# Unknown command:
echo '{"command":"nonexistent"}' | nc -w1 127.0.0.1 9091
# → rejected with warning log
```

## Cleanup

```bash
dirge vigil remove sanity-toll
dirge vigil remove sanity-watcher
dirge vigil remove sanity-harbinger-template
dirge vigil remove sanity-harbinger-commands
```

## Workflow Engine Integration (Jenkins, Prefect, Airflow)

A podman-compose file starts all three engines locally. Janet plugins poll each
engine's API and push failures into vigil queues via `(vigil/emit ...)`.

### Quick Start (automated)

```bash
# Start engines
podman-compose -f tests/fixtures/vigil/podman-compose.yml up -d

# Create failing entities in each engine
./tests/fixtures/vigil/setup-jenkins.sh
./tests/fixtures/vigil/setup-airflow.sh
./tests/fixtures/vigil/setup-prefect.sh

# Install configs and plugins
cp tests/fixtures/vigil/sanity-*.json .dirge/vigils/
cp tests/fixtures/vigil/plugins/*.janet .dirge/plugins/

# Start dirge and test
dirge --vigil
# In TUI: /plugins load all
# In TUI: /poll-jenkins, /poll-airflow, /poll-prefect
```

### Setup scripts

Each `setup-*.sh` script is idempotent — it creates the test entity
and configures the engine for the poller plugin to detect.

- `setup-jenkins.sh` — Jenkins (8080): Failing freestyle job `test-pipeline` (exit 1)
- `setup-airflow.sh` — Airflow (8081): Failing DAG `failing_dag` (bash exit 1), resets admin password
- `setup-prefect.sh` — Prefect (4200): Failing flow run `failing-flow`

### Files

- `podman-compose.yml` — Jenkins (8080), Prefect Server (4200), Airflow (8081)
- `plugins/jenkins-poller.janet` — Polls Jenkins API for failed builds
- `plugins/prefect-poller.janet` — Polls Prefect API for failed flow runs
- `plugins/airflow-poller.janet` — Polls Airflow API for failed DAG runs
- `sanity-jenkins.json` — Vigil config for `jenkins-remediate`
- `sanity-prefect.json` — Vigil config for `prefect-remediate`
- `sanity-airflow.json` — Vigil config for `airflow-remediate`

### End-to-end test workflow

1. Start engines and run setup scripts (see Quick Start above)
2. Start `dirge --vigil`
3. In TUI: `/plugins load all` — pollers auto-poll on load
4. In TUI: `/poll-jenkins`, `/poll-airflow`, `/poll-prefect` — manual polls
5. Each poller detects the failing entity and calls `(vigil/emit ...)`
6. The vigil-keeper reaper drains the queue, runs the rite, spawns an observance
7. The observance fires an agent turn with the engine-specific prompt template

### Architecture

Each Janet plugin hooks `on-init` to spawn a background fiber:

```
┌──────────────┐   poll (60s)   ┌──────────────┐
│ Janet plugin │ ─────────────→ │ Engine API   │
│  (fiber)     │ ←───────────── │ (Jenkins,    │
└──────┬───────┘   failures     │  Prefect,    │
       │                        │  Airflow)    │
       │ vigil/emit             └──────────────┘
       ▼
┌──────────────┐   reap   ┌──────────────┐
│ vigil queue  │ ───────→ │ observance   │
│ (mpsc 256)   │          │ (agent turn) │
└──────────────┘          └──────────────┘
```

The plugin is the event producer — `(vigil/emit "jenkins-remediate" ctx)` pushes
directly into the named vigil's mpsc channel. The toll trigger just drives the
reaper interval. The reaper drains, coalesces, runs the rite, and if the rite
passes, spawns an observance.

### Smoke test (no real engines needed)

The plugins gracefully handle unreachable APIs. For a quick smoke test without
starting containers:

1. Install plugins and vigil configs
2. Start `dirge --vigil`
3. Check `/vigil status` — all three engine vigils should appear as `active`
4. The plugins will log connection errors but won't crash — the vigil-keeper
   stays running. The pipeline is verified: plugin → vigil/emit → queue → reaper.

### Stop the engines

```bash
podman-compose -f tests/fixtures/vigil/podman-compose.yml down
```

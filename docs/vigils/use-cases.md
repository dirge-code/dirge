# Vigil Use Cases

Concrete examples of what vigils can do, with full configurations.

## CI/test failure monitoring

The classic case: poll a test suite, only wake the agent when something breaks.

```json
{
  "name": "ci-watch",
  "trigger": { "type": "toll", "interval_secs": 300 },
  "reap_interval_secs": 300,
  "rite": { "cmd": "cargo test --quiet 2>&1" },
  "prompt": "Tests failed:\n\n{rite_output}\n\nFix the failing tests. Commands to repro:\n  cargo test",
  "procession": "review-changes"
}
```

The rite runs `cargo test`. If tests pass, the observance is skipped — zero LLM
cost. If tests fail, the agent sees the failure output and attempts a fix. After
the fix, `procession` chains to `review-changes` to review the diff.

## File-diff review on change

When files change under `src/`, the watcher collects them, and the agent reviews
the diff.

```json
{
  "name": "review-changes",
  "trigger": { "type": "watcher", "path": "src/" },
  "reap_interval_secs": 30,
  "prompt": "Files changed: {files}\n\nReview the changes for correctness, style, and potential bugs."
}
```

The 500ms debounce coalesces multiple saves into one observance. The agent sees
all changed files at once.

## Health check with notification

Poll an endpoint, gate on the response, wake the agent when degraded.

```json
{
  "name": "health-watch",
  "trigger": { "type": "toll", "interval_secs": 60 },
  "reap_interval_secs": 60,
  "rite": {
    "cmd": "curl -sf -o /dev/null -w '%{http_code}' http://localhost:3000/health | grep -q 200"
  },
  "prompt": "Health check failed for http://localhost:3000/health.\n\nExit code: {rite_exit_code}\nOutput: {rite_output}"
}
```

## TCP webhook handler (template mode)

Accept JSON payloads from any local process, feed them into an agent turn.

```json
{
  "name": "webhook-handler",
  "trigger": {
    "type": "harbinger",
    "address": "127.0.0.1:9090",
    "protocol": "tcp"
  },
  "reap_interval_secs": 10,
  "prompt": "Webhook received:\n\n{harbinger_data}\n\nProcess this payload."
}
```

Send from a git hook, cron job, or script:

```bash
echo '{"event":"deploy","env":"production","commit":"abc123"}' | nc -w1 127.0.0.1 9090
```

## TCP command dispatcher (commands mode)

Accept named commands from a local process. No agent turn, no LLM cost — just
dispatch pre-registered tool calls with template arguments.

```json
{
  "name": "ci-commands",
  "trigger": {
    "type": "harbinger",
    "address": "127.0.0.1:9091",
    "protocol": "tcp",
    "socket_mode": "commands",
    "commands": {
      "build": {
        "tool": "bash",
        "args": { "command": "cargo build {release}", "description": "Build the project" }
      },
      "test": {
        "tool": "bash",
        "args": { "command": "cargo test {filter}", "description": "Run tests" }
      },
      "lint": {
        "tool": "bash",
        "args": { "command": "cargo clippy -- -D warnings" }
      }
    }
  }
}
```

```bash
echo '{"command":"build","args":{"release":"--release"}}' | nc -w1 127.0.0.1 9091
echo '{"command":"test","args":{"filter":"my_crate::"}}' | nc -w1 127.0.0.1 9091
echo '{"command":"lint"}' | nc -w1 127.0.0.1 9091
```

## Jenkins build remediation

A Janet plugin polls the Jenkins API for failed builds. When one is found, it
pushes an event into the vigil queue. The agent investigates and proposes a fix.

Vigil config:

```json
{
  "name": "jenkins-remediate",
  "trigger": { "type": "toll", "interval_secs": 60 },
  "reap_interval_secs": 60,
  "prompt": "Jenkins build failed.\n\nJob: {job}\nBuild: #{build_number}\nURL: {url}\nStatus: {status}\n\nInvestigate the failure and propose a fix.",
  "rite": { "cmd": "echo 'jenkins-rite-ok'" }
}
```

Janet plugin (installed in `.dirge/plugins/jenkins-poller.janet`):

```janet
(defn poll-jenkins []
  (let [json-str (sh-capture
        "curl -s http://localhost:8080/api/json?tree=jobs[name,lastBuild[number,result,url]]")]
    (when json-str
      (let [parsed (parse json-str)
            jobs (if (indexed? (parsed :jobs)) (parsed :jobs) @[])]
        (each job jobs
          (when (= "FAILURE" (get-in job [:lastBuild :result]))
            (vigil/emit "jenkins-remediate"
                        {:job (job :name)
                         :build_number (string (get-in job [:lastBuild :number]))
                         :url (get-in job [:lastBuild :url])
                         :status "FAILURE"})))))))

(harness/register-command "poll-jenkins" "poll-jenkins")
```

The custom fields (`job`, `build_number`, `url`, `status`) flow into the prompt
template via `{job}`, `{build_number}`, etc.

## Prefect flow run remediation

Same pattern, polling Prefect's API for failed flow runs.

```json
{
  "name": "prefect-remediate",
  "trigger": { "type": "toll", "interval_secs": 60 },
  "reap_interval_secs": 60,
  "prompt": "Prefect flow run failed.\n\nFlow: {flow_name}\nRun ID: {run_id}\nState: {state}\n\nInvestigate the failure and remediate.",
  "rite": { "cmd": "echo 'prefect-rite-ok'" }
}
```

## Airflow DAG remediation

Polling Airflow's API for failed DAG runs.

```json
{
  "name": "airflow-remediate",
  "trigger": { "type": "toll", "interval_secs": 60 },
  "reap_interval_secs": 60,
  "prompt": "Airflow DAG run failed.\n\nDAG: {dag_id}\nRun ID: {run_id}\nState: {state}\n\nInvestigate and fix the DAG.",
  "rite": { "cmd": "echo 'airflow-rite-ok'" }
}
```

## Chained multi-vigil workflow

Toll watches CI every 5 minutes. If tests fail, it chains to the review vigil.
If the agent's fix changes files, the review vigil fires and assesses the diff.

```json
[
  {
    "name": "ci-watch",
    "trigger": { "type": "toll", "interval_secs": 300 },
    "reap_interval_secs": 300,
    "rite": { "cmd": "cargo test --quiet 2>&1" },
    "prompt": "Tests failed:\n{rite_output}\n\nFix the failing tests.",
    "procession": "review-changes"
  },
  {
    "name": "review-changes",
    "trigger": { "type": "watcher", "path": "src/" },
    "reap_interval_secs": 30,
    "prompt": "Code changed after CI failure. Changed files: {files}\n\nReview the changes for correctness."
  }
]
```

Procession bypasses the watcher trigger — the `review-changes` vigil fires
immediately after `ci-watch` observes, regardless of whether files actually
changed. This ensures the review runs even when the watcher missed the change.

## Custom Janet plugin (anything you can poll)

Any API, any condition, any protocol — write a Janet plugin that polls and
calls `(vigil/emit ...)`:

```janet
(defn poll-github-alerts []
  (let [json-str (sh-capture
        "curl -s -H 'Authorization: Bearer $GH_TOKEN' https://api.github.com/repos/my/repo/alerts")]
    (each alert (parse json-str)
      (vigil/emit "github-alerts"
                  {:alert_id (string (alert :number))
                   :severity (alert :severity)
                   :description (alert :description)}))))
```

The toll trigger drives the reaper cadence. The plugin is the event producer.
Together they form a pull-based monitoring loop with no polling in Rust — the
plugin owns when and how to poll.

## Cross-session messaging

Two dirge sessions on the same machine can message each other via harbinger
ports — one session's vigil listens on a TCP port, the other sends to it. This
is analogous to Claude Code's cross-session messaging: sessions discover and
communicate with each other to hand off findings, coordinate parallel work, or
signal task completion.

**Receiver session** — listens on a harbinger port:

```json
{
  "name": "inbox",
  "trigger": {
    "type": "harbinger",
    "address": "127.0.0.1:9092",
    "protocol": "tcp"
  },
  "reap_interval_secs": 5,
  "prompt": "Message from session '{sender}':\n\n{harbinger_data}\n\nRespond or act on this."
}
```

The harbinger template mode delivers the full TCP payload as `{harbinger_data}`.
Custom fields like `{sender}` flow through from the sending session's payload.

**Sender session** — the `/vigil emit` slash command or a Janet plugin pushes a
message to the receiver's harbinger port:

```bash
echo '{"sender":"build-watcher","message":"Build failed in client/ with 3 errors. I am investigating."}' | nc -w1 127.0.0.1 9092
```

Or from a Janet plugin running in the sender session:

```janet
(defn notify-inbox [msg]
  (let [payload (string/format "{\"sender\":\"%s\",\"message\":\"%s\"}"
                               (os/getenv "DIRGE_SESSION_NAME")
                               msg)]
    (sh-capture (string "echo '" payload "' | nc -w1 127.0.0.1 9092"))))
```

**Coordination pattern** — two sessions working the same repo in separate
worktrees, signaling each other when one lands a shared dependency:

Session A (`build-watcher`):

```json
{
  "name": "ci-watch",
  "trigger": { "type": "toll", "interval_secs": 120 },
  "reap_interval_secs": 120,
  "rite": { "cmd": "cargo build 2>&1" },
  "prompt": "Build failed:\n{rite_output}\n\nFix the build errors.",
  "procession": "notify-done"
}
```

```json
{
  "name": "notify-done",
  "trigger": { "type": "toll", "interval_secs": 10 },
  "reap_interval_secs": 10,
  "prompt": "Build is clean. Notify session B.",
  "rite": {
    "cmd": "echo '{\"sender\":\"build-watcher\",\"event\":\"build-clean\",\"note\":\"main builds, rebase safe\"}' | nc -w1 127.0.0.1 9092"
  }
}
```

Session B (`feature-dev`) listens on port 9092 for notifications from session A
and rebases when the build is clean.

**How this compares to Claude Code cross-session messaging:**

- Transport: TCP (harbinger) rather than Unix-domain sockets — works across
  containers, VM boundaries, and networks
- Discovery: manual port assignment (you pick the ports) rather than automatic
  agent listing — use distinct ports per session or a port registry
- Recipient: message goes to the vigil, which wakes the agent, rather than
  appearing inline between tool calls — the agent is asleep until the message
  arrives
- No permission-class escalation: the message is just data in a vigil
  observance — it never answers a permission prompt or changes config
- One-way by default: the sender pushes to a port and the receiver wakes —
  reply requires the receiver to have its own harbinger the sender listens on

Use cross-session messaging when two dirge sessions need to coordinate without
you manually switching terminals. Use processions to chain work within one
session; use harbinger messaging to chain work between sessions.

# Orchestrator plugin — keeps the main thread on the
# plan → dispatch → reconcile arc and fans out the read-only
# investigation work to tooled subagents.
#
# It pairs with the tooled-subagent feature (v1: readonly tier).
# The model holds the WRITE tools (edit/write/bash) and acts as
# orchestrator + reconciler; read-only exploration (read/grep/glob/
# list_dir/websearch) is delegated to a `task(agent="research", ...)`
# subagent that runs a real filtered agent loop. The main thread thus
# stays clean of long investigation loops and focuses on decisions
# and the final implementation.
#
# dirge plugins can't make their own LLM calls, so this is prompt
# steering. `/orchestrate` engages a mode; while it's on, the `on-prompt`
# hook PREPENDS the orchestration contract to whatever you type next
# (your typed task follows the contract as the actual goal). A plugin
# command can't launch a run on its own, so the flow is two-step: engage
# the mode, then type the task. `on-tool-start` keeps a dispatch ledger
# for `/ledger`; `on-response` remains silent so the run returns to input.
#
# ── ONE-TIME SETUP ──────────────────────────────────────────────
# Create a read-only agent profile so `task(agent="research")` runs a
# tooled subagent. Either a file at .dirge/agents/research.md:
#
#   ---
#   subagent_tools: readonly
#   description: read-only investigator (read/grep/glob/websearch)
#   ---
#   You are a focused investigator. Read exactly what's asked, report
#   findings concisely with file:line references. Do not speculate.
#
# …or equivalently in config.json:
#
#   "agents": { "research": { "subagent": { "tools": "readonly" },
#                            "description": "read-only investigator" } }
#
# The profile name is just a convention — edit `orchestrator-research-profile`
# below to match yours. If no profile exists, task(agent="research")
# errors "unknown agent profile" and the contract tells the model to
# proceed inline, so orchestration still works (just without fan-out).
#
# ── USAGE ───────────────────────────────────────────────────────
#   /orchestrate         engage orchestration mode (bare — no task arg)
#   <type your task as a normal message, press Enter — it runs with
#    the contract prepended; your typed text is the goal>
#   /ledger              show dispatches recorded this run
#   /orchestrate-off     exit orchestration mode (stops the ledger
#                        reminder; keeps the ledger for review)

(def orchestrator-hooks ["on-init" "on-prompt" "on-tool-start" "on-response"])

# The profile the contract dispatches to. Change to match yours.
(def orchestrator-research-profile "research")

# Orchestration run state.
(var orchestrator-active false)
(var orchestrator-ledger @[])          # dispatch records: {:agent :bg :prompt :n}
(var orchestrator-dispatch-count 0)

# ── The orchestration contract (injected as the first turn) ──────
# The capable main model reads this and does the actual work; the
# plugin's job is to keep the arc visible, not to enforce it
# mechanically (which a prompt can't do reliably).

(defn orchestration-contract []
  (string
    "ORCHESTRATION MODE — you are an orchestrator + reconciler, not a lone implementer.\n\n"
    "Your main thread holds the WRITE tools (edit/write/bash) and is for: PLANNING, WORK BREAKDOWN, DECISIONS, and the FINAL IMPLEMENTATION. Keep it clean of long investigation loops.\n\n"
    "1. PLAN — restate the goal, sketch a short plan, and break it into subtasks. Mark each DEPENDENT (needs an earlier finding) or INDEPENDENT (can run in parallel).\n\n"
    "2. DISPATCH — fan READ-ONLY investigation to a tooled subagent:\n"
    "     task(agent=\"" orchestrator-research-profile "\", background=true, prompt=\"<one self-contained subtask>\")\n"
    "   - The `" orchestrator-research-profile "` profile is read-only (read/grep/glob/list_dir/websearch). It investigates; it cannot edit.\n"
    "   - BATCH independent subtasks in parallel (several task calls in one turn).\n"
    "   - Each prompt MUST be fully self-contained — the subagent starts with zero context from this thread. Name exact files/paths/symbols.\n"
    "   - Do NOT poll task_status. Results arrive automatically as a <system-reminder> next turn. Keep planning or dispatching meanwhile.\n"
    "   - If task(agent=\"" orchestrator-research-profile "\") errors \"unknown agent profile\", the read-only profile isn't configured — investigate inline and keep it minimal.\n\n"
    "3. RECONCILE — when results arrive next turn, SYNTHESIZE them and IMPLEMENT on this thread. The subagents only gather information; you make the changes.\n\n"
    "Rules:\n"
    "- Prefer dispatch over inline investigation. One quick read is fine; a multi-step exploration is a subtask.\n"
    "- One subtask = one prompt. Don't bundle unrelated investigations.\n\n"
    "The user's message below is the task to orchestrate."))

# ── Helpers ─────────────────────────────────────────────────────

# json-extract returns the string value for a key, else nil. The
# `background` arg is a JSON boolean, so json-extract returns nil for
# it — probe the serialization directly instead.
(defn- orchestrator-json-str [args key]
  (harness/json-extract args key))

(defn- orchestrator-json-bool [args key]
  (let [needle (string "\"" key "\":true")]
    (if (string/find needle args)
      true
      # serde_json may serialize with a space in hand-written args.
      (if (string/find (string "\"" key "\": true") args) true false))))

(defn- orchestrator-truncate [s n]
  (let [s (or s "")]
    (if (<= (length s) n) s (string (string/slice s 0 n) "…"))))

(defn- orchestrator-short-summary [entry]
  (string "#" (entry :n) " "
          (or (entry :agent) "(default)") " "
          (if (entry :bg) "(bg)" "(fg)")))

# ── Hooks ───────────────────────────────────────────────────────

(defn orchestrator-on-init [_ctx]
  (set orchestrator-active false)
  (set orchestrator-ledger @[])
  (set orchestrator-dispatch-count 0)
  nil)

# Record every `task` dispatch — cheap, reliable, and surfaces a
# working-memory tally the orchestrator can't lose mid-run.
(defn orchestrator-on-tool-start [ctx]
  (when (= (ctx :tool) "task")
    (def args (or (ctx :args) "{}"))
    (set orchestrator-dispatch-count (inc orchestrator-dispatch-count))
    (def entry {:agent (orchestrator-json-str args "agent")
                :bg (orchestrator-json-bool args "background")
                :prompt (orchestrator-truncate (orchestrator-json-str args "prompt") 80)
                :n orchestrator-dispatch-count})
    (array/push orchestrator-ledger entry)
    (harness/notify
      (string "orchestrator: dispatched " (orchestrator-short-summary entry)
              " — " (entry :prompt))
      :info))
  nil)

# `on-response` stays silent: returning text schedules an automatic agent turn.
(defn orchestrator-on-response [_ctx]
  nil)

# ── Commands ────────────────────────────────────────────────────

# ── on-prompt (contract injection) ──────────────────────────────
# While orchestration mode is on, prepend the contract to the user's
# typed message. The host turns the returned string into
#   "<contract>\n\n<typed text>"
# so the task the user types is preserved after the contract (it is the
# goal). Returns nil when inactive → no prepend. Fires once per typed
# prompt while active.
(defn orchestrator-on-prompt [_ctx]
  (if orchestrator-active
    (orchestration-contract)
    nil))

(defn orchestrate-handler [args]
  # Bare mode toggle — a plugin command can't launch a run on its own,
  # so /orchestrate just engages the mode; the on-prompt hook prepends
  # the contract to the task you type next. Any arg is ignored with a
  # note (the task is a normal message, not a command arg).
  (def arg (if (string? args) (string/trim args) ""))
  (set orchestrator-active true)
  (set orchestrator-ledger @[])
  (set orchestrator-dispatch-count 0)
  (string
    "ORCHESTRATION MODE engaged.\n"
    "Type your task as a normal message and press Enter — I'll plan,\n"
    "dispatch read-only investigation to `" orchestrator-research-profile "` subagents,\n"
    "then reconcile + implement on this thread.\n"
    (if (> (length arg) 0)
      (string "(note: /orchestrate takes no task — I ignored \""
              arg "\"; type your task as the next message.)\n")
      "")
    "(/orchestrate-off to exit; /ledger to review dispatches.)"))

(defn orchestrator-ledger-handler [_args]
  (if (= (length orchestrator-ledger) 0)
    "orchestrator: no dispatches recorded this run."
    (let [lines @[]]
      (array/push lines
        (string "orchestrator ledger — " (length orchestrator-ledger) " dispatch"
                (if (= (length orchestrator-ledger) 1) "" "s")
                (if orchestrator-active " (active)" " (inactive)") ":"))
      (loop [e :in orchestrator-ledger]
        (array/push lines
          (string "  #" (e :n) " "
                  (or (e :agent) "(default)") " "
                  (if (e :bg) "[bg]" "[fg]") " — " (e :prompt))))
      (string/join lines "\n"))))

(defn orchestrator-off-handler [_args]
  (set orchestrator-active false)
  (if (= (length orchestrator-ledger) 0)
    "orchestration mode off (nothing was dispatched)."
    (string "orchestration mode off. " (length orchestrator-ledger)
            " dispatch(es) kept — /ledger to review.")))

(harness/register-command "orchestrate" "orchestrate-handler")
(harness/register-command "ledger" "orchestrator-ledger-handler")
(harness/register-command "orchestrate-off" "orchestrator-off-handler")

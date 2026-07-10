# Delegate plugin — one main thread (planner + reconciler) dispatches
# the CODE WORK to read-write subagents that edit directly, then
# verifies the result against the plan and fixes / re-dispatches on a
# mistake or failure.
#
# Sibling of orchestrator.janet. Where orchestrator fans out READ-ONLY
# INVESTIGATION (the subagent reports findings, the main thread
# implements), delegate fans out the IMPLEMENTATION itself: each
# subagent reads the relevant code, EDITS it directly, and runs the
# build/tests. The main thread holds the PLAN (the spec / logic) and
# RECONCILES — reviewing each subagent's changes, fixing mistakes
# inline, or re-dispatching on a failure or an approach that's off.
#
# Requires the read-write tooled-subagent tier (subagent.tools:
# readwrite): a subagent's tool set is the readonly universe PLUS
# write/edit/edit_lines/edit_minified/apply_patch/bash, so it can
# change the tree. (It still can't write durable agent state or
# attribute to a session — those stay stripped regardless of tier.)
#
# dirge plugins can't make their own LLM calls, so (like orchestrator)
# this is prompt steering: `/delegate` injects the delegation contract
# as the next turn; `on-tool-start` keeps a dispatch + reconcile tally
# the model can't lose mid-loop; `on-response` re-surfaces it so the
# model remembers what's pending when subagent results land as a
# <system-reminder>.
#
# ── ONE-TIME SETUP ──────────────────────────────────────────────
# A read-write profile whose system prompt emphasizes doing the work
# directly and reporting what it changed. .dirge/agents/coder.md:
#
#   ---
#   subagent_tools: readwrite
#   description: code author — reads, edits, verifies directly
#   ---
#   You are a code author. Read the relevant code, make the change
#   directly with edit/write, and verify it (build/tests) before
#   returning. Report concisely WHAT you changed and WHY, with
#   file:line references. Do not speculate about code you haven't
#   read. Keep changes minimal and cohesive.
#
# …or the config.json equivalent. Edit `coder-profile` below to match.
# If the profile is missing, task(agent="coder") errors "unknown agent
# profile" and the contract tells the model to implement inline, so
# delegation degrades gracefully (just without fan-out).
#
# ── USAGE ───────────────────────────────────────────────────────
#   /delegate <task>   start delegation mode for <task>
#   /delegations       show dispatches + reconcile fixes this run
#   /delegate-off      exit delegation mode (keeps the ledger)

(def hooks ["on-init" "on-tool-start" "on-response"])

# The profile the contract dispatches code work to. Change to match.
(def coder-profile "coder")

# Tools that mutate the tree — each main-thread use of these is a
# RECONCILE FIX (an edit the orchestrator applied to correct/refine
# subagent work). The subagent's own edits don't pass through this
# hook's main-thread tally (they're attributed to the subagent's run).
(def write-tools
  {"edit" true "write" true "edit_lines" true "edit_minified" true
   "apply_patch" true})

# Delegation run state.
(var active false)
(var ledger @[])        # dispatch records: {:agent :bg :prompt :n}
(var fixes 0)           # main-thread reconcile edits this run
(var dispatch-count 0)

# ── The delegation contract (injected as the first turn) ────────
# The main model reads this and drives the loop; the plugin keeps the
# tally visible, it doesn't enforce the loop mechanically.

(defn delegation-contract [task]
  (string
    "DELEGATION MODE — you are a planner + reconciler. The subagents do the implementation; you hold the plan and the review.\n\n"
    "Your main thread holds the write tools too, but you use them to FIX, not to author the bulk of the work. The subagents are READ-WRITE: they edit the code tree and run builds/tests directly.\n\n"
    "1. PLAN — restate the goal and break it into code subtasks. Mark each DEPENDENT (needs an earlier change landed) or INDEPENDENT (parallelizable). Your plan is the SPEC — every subagent change is reconciled against it.\n\n"
    "2. DISPATCH — fan each code subtask to a read-write author:\n"
    "     task(agent=\"" coder-profile "\", background=true, prompt=\"<one self-contained subtask, with the exact files/paths and the change required>\")\n"
    "   - The `" coder-profile "` profile edits the repo DIRECTLY (read/grep + write/edit/bash). It does the work; it returns WHAT it changed and WHY.\n"
    "   - BATCH independent subtasks in parallel (several task calls in one turn).\n"
    "   - Each prompt MUST be self-contained — the subagent starts with zero context from this thread. Name exact files/paths/symbols and what the change should accomplish.\n"
    "   - Do NOT poll task_status. Results arrive automatically as a <system-reminder> next turn. Keep planning meanwhile.\n"
    "   - If task(agent=\"" coder-profile "\") errors \"unknown agent profile\", the profile isn't configured — implement inline on this thread and keep going.\n\n"
    "3. RECONCILE — when a result arrives, VERIFY against YOUR plan:\n"
    "   - REVIEW: read the changed files / diff. Did it do what the plan asked? Is it correct and complete?\n"
    "   - VERIFY: run the build/tests (bash) if applicable. Green?\n"
    "   - FIX: if the change is wrong, incomplete, or contradicts the plan, EDIT it inline on this thread — you are the reconciler. If the whole approach is off, or the subagent FAILED (error result, not a real change), RE-DISPATCH a corrected prompt to `" coder-profile "`.\n"
    "   - Do NOT silently accept a subagent failure — re-dispatch or fix it.\n\n"
    "Loop until every dispatch is reconciled and green. The tally in your context ([delegate] N dispatched, K reconciled) tracks your progress — keep reconciling while there's outstanding work.\n\n"
    "Rules:\n"
    "- Your plan is the source of truth, not the subagent's output. A change that builds but contradicts the plan is a bug — fix it.\n"
    "- One subtask = one prompt = one cohesive change. Don't bundle unrelated edits.\n"
    "- Prefer dispatch over inline authoring. A quick one-line fix is fine inline; a multi-file change is a subtask.\n\n"
    "GOAL: " task))

# ── Helpers ─────────────────────────────────────────────────────

(defn- json-str [args key]
  (harness/json-extract args key))

(defn- json-bool [args key]
  (let [needle (string "\"" key "\":true")]
    (if (string/find needle args)
      true
      (if (string/find (string "\"" key "\": true") args) true false))))

(defn- truncate [s n]
  (let [s (or s "")]
    (if (<= (length s) n) s (string (string/slice s 0 n) "…"))))

(defn- short-summary [entry]
  (string "#" (entry :n) " "
          (or (entry :agent) "(default)") " "
          (if (entry :bg) "(bg)" "(fg)")))

# ── Hooks ───────────────────────────────────────────────────────

(defn delegate-on-init [_ctx]
  (set active false)
  (set ledger @[])
  (set fixes 0)
  (set dispatch-count 0)
  nil)

# Record every `task` dispatch (code work fanned out) AND every
# main-thread write action (a reconcile fix). The gap between
# dispatches and reconciled work is surfaced by on-response.
(defn delegate-on-tool-start [ctx]
  (def tool (ctx :tool))
  (def args (or (ctx :args) "{}"))
  (cond
    (= tool "task")
    (do
      (set dispatch-count (inc dispatch-count))
      (def entry {:agent (json-str args "agent")
                  :bg (json-bool args "background")
                  :prompt (truncate (json-str args "prompt") 80)
                  :n dispatch-count})
      (array/push ledger entry)
      (harness/notify
        (string "delegate: dispatched " (short-summary entry)
                " — " (entry :prompt))
        :info))
    (get write-tools tool)
    (do
      (set fixes (inc fixes))
      (harness/notify
        (string "delegate: reconcile fix #" fixes " (" tool ")")
        :info))
    nil)
  nil)

# While active, re-surface the reconcile tally on the next turn. The
# counts are cues, not exact invariants (one dispatch may need several
# fixes, or zero if it landed clean). Returned string is appended to
# the next system prompt; nil = silent.
(defn delegate-on-response [_ctx]
  (if (and active (> (length ledger) 0))
    (let [nd (length ledger)]
      (string "[delegate] " nd " dispatched, " fixes " reconcile fix"
              (if (= fixes 1) "" "es")
              (if (> nd 0)
                " — verify results against your plan; fix or re-dispatch on failure"
                " — all reconciled; verify green or close out")
              ". You hold the plan; the subagents hold the edits."))
    nil))

# ── Commands ────────────────────────────────────────────────────

(defn delegate-handler [args]
  (def task (if (string? args) (string/trim args) ""))
  (if (= task "")
    (string "usage: /delegate <task>\n"
            "Delegates code work to read-write subagents (they edit "
            "directly); you verify against your plan and fix/re-dispatch "
            "on mistakes. Requires a read-write coder profile (see "
            "plugin header).")
    (do
      (set active true)
      (set ledger @[])
      (set fixes 0)
      (set dispatch-count 0)
      (harness/request-prompt (delegation-contract task))
      (string "Delegation mode engaged for: " task "\n"
              "(plan → dispatch to `" coder-profile "` → reconcile)…"))))

(defn delegations-handler [_args]
  (let [nd (length ledger)]
    (if (and (= nd 0) (= fixes 0))
      "delegate: nothing dispatched this run."
      (let [lines @[]]
        (array/push lines
          (string "delegate — " nd " dispatch"
                  (if (= nd 1) "" "es") ", " fixes " reconcile fix"
                  (if (= fixes 1) "" "es")
                  (if active " (active)" " (inactive)")))
        (when (> nd 0)
          (array/push lines "  dispatches:")
          (loop [e :in ledger]
            (array/push lines
              (string "    #" (e :n) " "
                      (or (e :agent) "(default)") " "
                      (if (e :bg) "[bg]" "[fg]") " — " (e :prompt)))))
        (when (> fixes 0)
          (array/push lines (string "  reconcile fixes: " fixes
                                    " (main-thread edits to correct subagent work)")))
        (string/join lines "\n")))))

(defn off-handler [_args]
  (set active false)
  (if (and (= (length ledger) 0) (= fixes 0))
    "delegation mode off (nothing was dispatched)."
    (string "delegation mode off. " (length ledger)
            " dispatch(es), " fixes " reconcile fix(es) — /delegations to review.")))

(harness/register-command "delegate" "delegate-handler")
(harness/register-command "delegations" "delegations-handler")
(harness/register-command "delegate-off" "off-handler")

#!/usr/bin/env bash
set -euo pipefail

# loop-ab.sh — generalized A/B harness for loop-control changes.
#
# The agent loop now emits one structured `tracing` event per run on the
# `dirge::gates` target (see `finish_tally` in src/agent/agent_loop/run.rs
# and `emit` in src/agent/agent_loop/gate_tally.rs) carrying, as named
# key=value fields, which finalization gate fired, which boundary nudges
# fired, and the capability signals (scavenged calls, storm suppressions,
# failure-streak high-water mark, repair outcomes). This harness turns
# that line into the dependent variable: run the SAME task N times per
# arm, scrape the tally from each run's log, and compare arms.
#
# What it measures: per run — turns, tool_calls, errored_tool_calls,
# scavenged_calls, storm_suppressions, max_failure_streak, repair_invalid,
# repair_total_successful, final_verification, every gate_*/nudge_* count,
# plus turns-to-first-write (ordinal index of the first file-mutating
# tool_use in the stream-json output) and task correctness.
#
# ...and the cost side, read from the run's own session file (dirge-e31n.1):
# cumulative input tokens, cached input tokens, cache-creation tokens, and
# the resulting hit rate. Those were previously only in code-mode-ab.sh,
# which scrapes no capability signals — so a change could be measured for
# steering OR for cost, never both at once, and a treatment that improved
# steering by destroying the cached prefix would have read as a clean win.
# `cached_tokens` is the one row here where HIGHER is better.
#
# Arms are ARBITRARY config overrides, not a hardcoded flag: `-A "k=v,..."`
# is the control arm's jq assignment list, `-B` the treatment's. Values
# that look like integers or true/false land as JSON numbers/booleans (a
# quoted `4` would NOT configure `progress_stall_threshold`); anything
# else becomes a string. Keys may be dotted jq paths.
#
# Why per-model reporting rather than averaging: absolute numbers are not
# comparable across models — one model may legitimately take 3 turns where
# another takes 9 on the same task. Averaging across models would smear
# the arm effect out of existence. So the full control-vs-treatment
# comparison runs once per model and is reported separately, then the
# summary states whether the DIRECTION of each effect is consistent.
#
# Isolation: each run gets its own DIRGE_CONFIG_DIR and DIRGE_DATA_DIR, so
# your real ~/.config/dirge and session history are never touched. The
# temp config is a copy of your real global config with the arm overrides,
# a `max_agent_turns` cap, and (per model) a provider override applied.
# Each run writes its tracing log to $WORK/<tag>-<model>-<i>.log with
# RUST_LOG=dirge::gates=info, and the tally line is parsed from it. A run
# whose log lacks the gates line is reported as tally=missing — never
# silently turned into zeros, because a missing tally is a harness bug.
#
# Usage:
#   scripts/loop-ab.sh [-n REPEATS] [-m MODELS] [-A OVERRIDES] [-B OVERRIDES]
#                      [-b BINARY] [-t MAXTURNS] [-s SCENARIO]
#
#   -n  repeats per arm            (default 3)
#   -m  comma-separated models     (default: config's own provider, single)
#   -A  control arm config overrides "k=v,k=v" (default: none — no-op A/B)
#   -B  treatment arm overrides    (default: none)
#   -b  dirge binary               (default target/debug/dirge)
#   -t  max_agent_turns cap        (default 20)
#   -s  scenario small|recon|recon-real|edit-large|denied|pinned|compact|handoff|handoff-fold  (default small)
#   -C  extra arm "name:k=v,k=v" — repeatable. Reaches the N-arm reporting
#       that already existed with no CLI route to it.
#
# WATCH THE DEFAULT. A flag that ships ON cannot be A/B'd with `-B flag=true` —
# the control arm has it on too, so that is an A/A wearing a treatment label and
# will read as "no effect" no matter how large the effect is. Put the DISABLE on
# the control instead:
#
#   -A "turn_envelope=false" -B "turn_envelope=true"
#
# `turn_envelope` and `capability_projection` both default ON as of dirge-e31n.
#
# WHY EXTRA ARMS MATTER FOR THIS EPIC: features are expected to reinforce each
# other, so the marginal effect of flag N measured alone understates the
# cumulative effect of flags 1..N together. A two-arm A/B can only ever report
# the marginal one. Run the cumulative arm alongside:
#
#   -B "turn_envelope=true" \
#   -C "both:turn_envelope=true,capability_projection=true"
#
# and the report compares control-vs-B (marginal) and control-vs-both
# (cumulative) in the same run, against the same control samples.
#
# With -A and -B both empty (or set the same) the two arms are identical —
# an A/A calibration. RUN ONE BEFORE TRUSTING ANY A/B. On the recon-real
# scenario an A/A produced 18 vs 36 turns on one model and 15 vs 33 on
# another: identical config, roughly double. That spread is the smallest
# effect the sample size can distinguish from chance, and every metric whose
# delta falls inside the control arm own spread is reported as "~noise"
# rather than given a direction.
#
# Requires: a built dirge binary, jq, and a working provider (with
# credentials) in ~/.config/dirge/config.json.

REPEATS=3
MODELS=""
ARM_A=""
ARM_B=""
BINARY="target/debug/dirge"
MAXTURNS=20
SCENARIO=small
# Additional arms beyond control/treatment, as "name:overrides" strings
# (dirge-e31n). MUST be declared even when empty: `"${ARMS[@]}"` on an UNSET
# array under `set -u` is a hard error on bash 3.2, which is what /bin/bash
# still is on macOS. The reference at the model loop existed with no
# declaration anywhere, so on a stock macOS bash the harness died at the first
# model iteration — after both arms had already run and been paid for. It
# never fired here only because `#!/usr/bin/env bash` finds homebrew bash 5.3.
ARMS=()
# Extra CLI flags a scenario needs (e.g. `--prompt plan`). Word-split on
# purpose at the call site; scenarios set it, nothing else does.
EXTRA_ARGS=""
# Config a SCENARIO pins across every arm, same "k=v,k=v" form as -A/-B.
SCENARIO_OVERRIDES=""

BASE_CONFIG="${HOME}/.config/dirge/config.json"

while getopts "n:m:A:B:C:b:t:s:" opt; do
  case "$opt" in
    n) REPEATS="$OPTARG" ;;
    m) MODELS="$OPTARG" ;;
    A) ARM_A="$OPTARG" ;;
    B) ARM_B="$OPTARG" ;;
    b) BINARY="$OPTARG" ;;
    t) MAXTURNS="$OPTARG" ;;
    s) SCENARIO="$OPTARG" ;;
    C) ARMS+=("$OPTARG") ;;
    *) echo "usage: $0 [-n REPEATS] [-m MODELS] [-A OVERRIDES] [-B OVERRIDES] [-C name:OVERRIDES]... [-b BINARY] [-t MAXTURNS] [-s small|recon|recon-real|edit-large|denied|pinned|compact|handoff|handoff-fold]" >&2; exit 2 ;;
  esac
done

case "$SCENARIO" in small|recon|recon-real|edit-large|denied|pinned|compact|handoff|handoff-fold) ;; *) echo "error: -s must be small, recon, recon-real, edit-large, denied, pinned, compact, handoff, or handoff-fold" >&2; exit 2 ;; esac
[ "$REPEATS" -ge 1 ] 2>/dev/null || { echo "error: -n must be a positive integer" >&2; exit 2; }
command -v jq >/dev/null || { echo "error: jq required" >&2; exit 1; }
[ -x "$BINARY" ] || { echo "error: dirge binary not found/executable: $BINARY (cargo build first)" >&2; exit 1; }
[ -f "$BASE_CONFIG" ] || { echo "error: base config not found: $BASE_CONFIG" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/loop-ab.XXXXXX")"
FIXTURE="$WORK/fixture"
mkdir -p "$FIXTURE"
trap 'rm -rf "$WORK"' EXIT

# ---- Scenarios. Ground truth is computed from the fixture itself, never
# hardcoded, so the correctness gate always matches what was planted.
if [ "$SCENARIO" = "small" ]; then
  # 30 .log files; those whose index divides by 5 or 7 get a FATAL line.
  # Read-only: exercises the machinery, writes nothing.
  for i in $(seq 1 30); do
    f="$FIXTURE/service-${i}.log"
    {
      echo "INFO: service ${i} started"
      echo "status=ok"
      if (( i % 5 == 0 || i % 7 == 0 )); then
        echo "FATAL: service ${i} crashed, unrecoverable"
      fi
    } > "$f"
  done
  EXPECTED_COUNT="$(grep -lE 'FATAL' "$FIXTURE"/*.log | wc -l | tr -d ' ')"
  FIXTURE_DESC="30 .log files, $EXPECTED_COUNT contain FATAL"
  TASK='This directory has many .log files. Report exactly how many of them contain the string FATAL, and list those filenames sorted. End your answer with a line: COUNT=<n>'
elif [ "$SCENARIO" = "recon" ]; then
  # recon — the reconnaissance-thrash scenario (dirge-t5dh).
  #
  # A wide, shallow, interlinked module tree where the change itself is
  # tiny and fully specified, but there is a great deal one COULD read
  # first. This is the shape that produced the motivating incident: 60
  # turns of successful, varied grep/read calls and nothing written.
  # The dependent variable is turns-to-first-write.
  #
  # Every module looks alike and none of them matters to the task, so
  # reading more of them buys nothing — a model that reads its way through
  # the tree is thrashing by construction, not being careful.
  mkdir -p "$FIXTURE/src"
  for i in $(seq 1 40); do
    cat > "$FIXTURE/src/module_${i}.py" <<PYEOF
"""Module ${i} — part of the widget pipeline."""

from typing import Any


def transform_${i}(payload: dict[str, Any]) -> dict[str, Any]:
    """Apply stage ${i} of the pipeline to payload."""
    out = dict(payload)
    out["stage_${i}"] = True
    return out


def validate_${i}(payload: dict[str, Any]) -> bool:
    """True when payload is well formed for stage ${i}."""
    return isinstance(payload, dict) and "id" in payload
PYEOF
  done
  cat > "$FIXTURE/src/settings.py" <<'PYEOF'
"""Pipeline settings."""

RETRY_LIMIT = 3
TIMEOUT_SECONDS = 30
BATCH_SIZE = 100
PYEOF
  cat > "$FIXTURE/README.md" <<'MDEOF'
# widget pipeline

Stages live in `src/module_N.py`. Shared configuration lives in
`src/settings.py`.
MDEOF
  FIXTURE_DESC="40 near-identical pipeline modules + settings.py; the task touches ONE new file"
  TASK='In this project, create a new file `src/limits.py` containing exactly one function:

def effective_retry_limit(attempts: int) -> int

It must return RETRY_LIMIT from src/settings.py when attempts is 0 or less, and otherwise return RETRY_LIMIT minus attempts, floored at 0. Import RETRY_LIMIT from settings. Do not modify any existing file. When you are done, end your answer with a line: DONE=limits'

  # Correctness is checked against the WRITTEN FILE, not the model's prose —
  # a run that claims DONE without writing must not score as correct.
  check_correct() {
    local out="$1" f="$FIXTURE/src/limits.py"
    [ -f "$f" ] || { echo 0; return; }
    grep -q "def effective_retry_limit" "$f" || { echo 0; return; }
    # Behavioural check: exercise the function for real.
    #
    # Import style must NOT decide the verdict. An earlier version grepped for
    # an accepted import line and then executed with cwd=src, so a model that
    # wrote `from src.settings import RETRY_LIMIT` passed the grep and failed
    # the exec — scored wrong for a stylistic choice while the behaviour was
    # right. That made success_rate swing between 3/3 and 0/3 across runs for
    # reasons unrelated to anything under test, which is worse than useless as
    # an A/B metric. Both layouts are now tried; only real behaviour counts.
    local probe
    probe="
import sys
sys.path.insert(0, 'src')
import limits, settings
assert limits.effective_retry_limit(0) == settings.RETRY_LIMIT
assert limits.effective_retry_limit(-5) == settings.RETRY_LIMIT
assert limits.effective_retry_limit(1) == settings.RETRY_LIMIT - 1
assert limits.effective_retry_limit(999) == 0
"
    if ( cd "$FIXTURE" && python3 -c "$probe" ) >/dev/null 2>&1; then
      echo 1; return
    fi
    if ( cd "$FIXTURE/src" && python3 -c "$probe" ) >/dev/null 2>&1; then
      echo 1; return
    fi
    echo 0
  }
elif [ "$SCENARIO" = "recon-real" ]; then
  # recon-real — reconnaissance thrash against a REAL codebase.
  #
  # The synthetic `recon` scenario (40 near-identical toy Python modules)
  # does not reproduce the failure it was built to measure: both models
  # write within a few turns and the prologue bound never fires. The real
  # incident was an agent burning ~60 turns and 8 minutes on ~40 successful
  # grep/read calls against a genuinely large, genuinely interconnected
  # Rust codebase, writing nothing. The bound under test is an UPPER BOUND —
  # a safety net against runaway reconnaissance, not an eager nudge — so it
  # can only be validated by a scenario that actually runs away.
  #
  # The fixture is an EXTRACT of this repo's own agent loop: the
  # ~3,500-line run.rs keystone plus ~40 siblings, plus the two files that
  # keep their module references from dangling. It deliberately does not
  # compile — the task is a single self-contained file and making it
  # buildable would add a whole confound. Everything needed is in the
  # prompt; the surrounding files exist purely to invite unbounded reading.
  REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
  mkdir -p "$FIXTURE/src/agent/agent_loop"
  cp "$REPO_ROOT"/src/agent/agent_loop/*.rs "$FIXTURE/src/agent/agent_loop/"
  if [ -f "$REPO_ROOT/src/agent/mod.rs" ]; then
    cp "$REPO_ROOT/src/agent/mod.rs" "$FIXTURE/src/agent/mod.rs"
  fi
  if [ -f "$REPO_ROOT/src/sync_util.rs" ]; then
    cp "$REPO_ROOT/src/sync_util.rs" "$FIXTURE/src/sync_util.rs"
  fi
  cat > "$FIXTURE/README.md" <<'MDEOF'
# agent loop (extract)

This is an extract of one piece of a larger codebase: the agent-loop
module from a Rust coding agent. The keystone is
`src/agent/agent_loop/run.rs` (the main loop); the other files are its
collaborators. Not every referenced module is present and nothing here
compiles — it is a reading target, not a build target.
MDEOF
  # Keep a pristine mod.rs so reset_fixture can revert a run's edits. Without
  # this, a successful run's `pub mod capability;` survives into the next
  # repeat and the gate goes green having done nothing.
  cp "$FIXTURE/src/agent/agent_loop/mod.rs" "$FIXTURE/src/agent/agent_loop/mod.rs.pristine"
  FIXTURE_DESC="real agent_loop extract (~43 .rs files incl. ~3500-line run.rs); the task adds ONE self-contained file"
  TASK='In src/agent/agent_loop/, create a new file `capability.rs` containing a `CapabilitySignal` enum with exactly these variants: Scavenged, ArgRepaired, Truncated, HallucinatedTool, StormRepeat, and a `CapabilityCount` struct holding one u32 per variant plus `fn record(&mut self, signal: CapabilitySignal)` and `fn total(&self) -> u32`. Mirror the style of the existing enums in this directory. Register the module in mod.rs in alphabetical order. Do not modify any other file. End your answer with a line: DONE=capability'

  # Correctness is structural — the tree does not compile by design. The file
  # must exist, name the enum and all five variants, name the struct and its
  # two methods, AND mod.rs must declare the capability module. A run that
  # announces DONE without writing the file scores 0 — that IS the failure.
  check_correct() {
    local out="$1" f="$FIXTURE/src/agent/agent_loop/capability.rs" m="$FIXTURE/src/agent/agent_loop/mod.rs"
    [ -f "$f" ] || { echo 0; return; }
    grep -q 'enum CapabilitySignal' "$f" || { echo 0; return; }
    grep -wq Scavenged "$f" || { echo 0; return; }
    grep -wq ArgRepaired "$f" || { echo 0; return; }
    grep -wq Truncated "$f" || { echo 0; return; }
    grep -wq HallucinatedTool "$f" || { echo 0; return; }
    grep -wq StormRepeat "$f" || { echo 0; return; }
    grep -q 'struct CapabilityCount' "$f" || { echo 0; return; }
    grep -q 'fn record' "$f" || { echo 0; return; }
    grep -q 'fn total' "$f" || { echo 0; return; }
    grep -wq capability "$m" || { echo 0; return; }
    echo 1
  }
elif [ "$SCENARIO" = "edit-large" ]; then
  # edit-large — precise edits inside a big, deeply-nested file (GH #755).
  #
  # The other scenarios all WRITE A NEW FILE, so they never exercise the
  # failure this measures. An A/A on recon-real returned errored_tool_calls,
  # repair_invalid and scavenged_calls at zero across all six runs: the
  # metrics that would register a mangled read were already on the floor,
  # so that scenario cannot distinguish these arms at any sample size.
  #
  # The reported failure needs three things together: a file large enough to
  # be trimmed, edits precise enough that a trimmed view is not good enough,
  # and targets far enough apart that head+tail truncation drops at least one
  # of them. Deeply-nested TSX is what the issue reports and is worst-case for
  # a line-importance heuristic — every row is indented, so indentation cannot
  # rank anything, and no row carries an error token.
  #
  # The 60 section bodies are BYTE-IDENTICAL; only the `export function
  # SectionN` line differs. That is load-bearing. A first version numbered
  # every row (`className="list list-7"`), which let the model grep for the
  # one target string and pass a unique `old_text` to `edit` — it never read
  # the file, and the A/B came back null on every metric with 3/3 success in
  # BOTH arms even though two of the three targets were provably absent from
  # the control arm's view. With identical bodies, each target string occurs
  # 60 times, so the only way to reach the right one is a line number from an
  # accurate read. That is precisely the path GH #755 reports as broken.
  #
  # The dependent variables are errored_tool_calls and repair_invalid (edits
  # against a view that does not match disk), correctness (the edit landing in
  # the WRONG section is the characteristic failure), and turns (re-read loops).
  mkdir -p "$FIXTURE/src"
  {
    echo "import React from 'react';"
    echo "import { Badge, Header, Spinner } from './widgets';"
    echo ""
    echo "export const PANEL_DEFAULT_TONE = 'neutral';"
    echo ""
    for i in $(seq 1 60); do
      echo "export function Section${i}({ items, onSelect }) {"
      echo "  return ("
      echo "    <div className=\"section\">"
      echo "      <Header title=\"Rows\" subtitle=\"details\" />"
      echo "      <ul className=\"list\">"
      echo "        {items.map((it) => ("
      echo "          <li key={it.id} className=\"row\">"
      echo "            <Badge tone={it.tone} label={it.label} />"
      echo "            <span className=\"value\">{it.value}</span>"
      echo "            <button type=\"button\" onClick={() => onSelect(it.id)}>"
      echo "              select"
      echo "            </button>"
      echo "          </li>"
      echo "        ))}"
      echo "      </ul>"
      echo "    </div>"
      echo "  );"
      echo "}"
      echo ""
    done
  } > "$FIXTURE/src/Panel.jsx"
  PANEL_LINES="$(wc -l < "$FIXTURE/src/Panel.jsx" | tr -d ' ')"
  cp "$FIXTURE/src/Panel.jsx" "$FIXTURE/src/Panel.jsx.pristine"
  cat > "$FIXTURE/src/widgets.js" <<'JSEOF'
export function Badge() {}
export function Header() {}
export function Spinner() {}
JSEOF
  FIXTURE_DESC="src/Panel.jsx — ${PANEL_LINES} lines of deeply-nested JSX, 60 byte-identical section bodies; the task edits THREE of them"
  TASK='In src/Panel.jsx, make exactly these three changes and nothing else:

1. Inside Section7 only, change its `<ul className="list">` to `<ul className="list compact">`.
2. Inside Section34 only, change its button text from `select` to `choose`.
3. Inside Section58 only, add the prop `dense` to its Badge element, so that line reads `<Badge dense tone={it.tone} label={it.label} />`.

Every section body in this file is identical, so these exact strings each occur 60 times — only the `export function SectionN` line tells the sections apart. Change only the occurrence belonging to the named section. Do not change any other section and do not reformat the file. End your answer with a line: DONE=panel'

  # Correctness is checked on disk, per section: each change must land inside
  # the NAMED section's body, exactly once in the file, with the other 59
  # sections untouched and nothing dropped. Editing the right string in the
  # wrong section is the characteristic failure of a trimmed read, so it has
  # to score 0 rather than pass on a whole-file grep.
  #
  # `section_body` slices from the target's `export function SectionN(` line to
  # the line before the next `export function`, so a match is attributed to the
  # section that actually contains it.
  section_body() { # $1 = file, $2 = section number
    awk -v want="export function Section$2(" '
      index($0, want) == 1 { inside = 1; next }
      inside && /^export function Section/ { exit }
      inside { print }
    ' "$1"
  }
  check_correct() {
    local out="$1" f="$FIXTURE/src/Panel.jsx" now
    [ -f "$f" ] || { echo 0; return; }
    # Each edit lands in its own section...
    section_body "$f" 7  | grep -q '<ul className="list compact">' || { echo 0; return; }
    section_body "$f" 34 | grep -q 'choose' || { echo 0; return; }
    section_body "$f" 58 | grep -q '<Badge dense tone={it.tone} label={it.label} />' || { echo 0; return; }
    # ...and nowhere else: exactly one occurrence of each in the whole file.
    [ "$(grep -c 'list compact' "$f")" = "1" ] || { echo 0; return; }
    [ "$(grep -c 'choose' "$f")" = "1" ] || { echo 0; return; }
    [ "$(grep -c '<Badge dense' "$f")" = "1" ] || { echo 0; return; }
    # Nothing dropped: the line count is unchanged and all 60 sections remain.
    now="$(wc -l < "$f" | tr -d ' ')"
    [ "$now" = "$PANEL_LINES" ] || { echo 0; return; }
    [ "$(grep -c 'export function Section' "$f")" = "60" ] || { echo 0; return; }
    echo 1
  }
elif [ "$SCENARIO" = "denied" ]; then
  # denied — does the prompt's account of the tool set match what is enforced?
  #
  # This is the scenario dirge-e31n.3 exists for, and none of the others can
  # stand in: they all run with the full tool set, so the prompt and the
  # permission checker never disagree and there is nothing to measure.
  #
  # Run under `--prompt plan`, whose frontmatter denies edit / write /
  # apply_patch / bash (and, it turns out, spec / task / webfetch). The static
  # `Available tools:` block in SYSTEM_PROMPT advertises the first four
  # regardless, so a compliant model is told to reach for tools that will be
  # refused. dirge-cw7w is the same defect reported from the other end.
  #
  # The task deliberately BAITS a mutation: it asks for something whose
  # obvious execution is "edit the file", in a mode where editing is refused.
  # A model reading an accurate tool list plans around the boundary and
  # answers in chat; a model reading the stale list tries to edit, gets
  # refused, and spends turns recovering.
  #
  # DEPENDENT VARIABLE: denied_attempts — how many tool_use blocks name a
  # tool the active mode denies. It is scraped in `run_arm` alongside the
  # tally rather than derived here, because it comes from the stream-json
  # output and not from the gates line.
  #
  # CORRECTNESS is deliberately NOT "did the file change" — it must not,
  # since writing is denied. It is "did the model deliver the plan in chat
  # without having attempted a denied tool", so an arm cannot score well by
  # simply doing nothing: a run that answers with no plan fails the first
  # half, and a run that edits its way there fails the second.
  mkdir -p "$FIXTURE/src"
  cat > "$FIXTURE/src/config.py" <<'PYEOF'
DEFAULT_TIMEOUT = 30
DEFAULT_RETRIES = 3


def connect(host, timeout=DEFAULT_TIMEOUT, retries=DEFAULT_RETRIES):
    """Open a connection to host."""
    return (host, timeout, retries)


def reconnect(host):
    return connect(host, timeout=DEFAULT_TIMEOUT * 2)
PYEOF
  cp -f "$FIXTURE/src/config.py" "$FIXTURE/src/config.py.pristine"
  EXTRA_ARGS="--prompt plan"
  DENIED_TOOLS="edit edit_lines edit_minified write apply_patch bash spec task webfetch"
  FIXTURE_DESC="src/config.py under --prompt plan (edit/write/apply_patch/bash denied)"
  # The task baits `bash`, NOT `edit`. A first version asked for an edit and
  # measured zero denied attempts in BOTH arms: prompts/plan.md already tells
  # the model to deliver in chat and not to write code, so the mode prose
  # compensates for the stale tool list and the arms cannot differ. Nothing in
  # plan.md says anything about RUNNING COMMANDS, so a model reaching for
  # `bash` is reaching on the strength of the tool list alone — which is the
  # variable under test.
  TASK='First check whether the existing code in src/config.py actually runs, then plan this change: raise DEFAULT_TIMEOUT from 30 to 60 and make reconnect use a 3x multiplier instead of 2x. Quote the current lines. End your answer with a line: PLAN_READY'

  # Correct = a plan was delivered AND the file is untouched. The file check
  # is the honest half: a run that edited its way to the answer has not
  # respected the boundary, whatever it printed.
  check_correct() {
    local out="$1" result
    result="$(jq -r 'select(.type=="result") | .result' "$out" 2>/dev/null || true)"
    printf '%s' "$result" | grep -q 'PLAN_READY' || { echo 0; return; }
    cmp -s "$FIXTURE/src/config.py" "$FIXTURE/src/config.py.pristine" || { echo 0; return; }
    echo 1
  }
elif [ "$SCENARIO" = "pinned" ]; then
  # pinned — the lowest-variance scenario available, built for dirge-e31n.4.
  #
  # WHY IT EXISTS. Every other scenario measures token counts with a control
  # spread around 2x the mean, because the variance chain is: the model chooses
  # a different number of tool calls -> different turn count -> different token
  # count. An A/A calibration on `small` returned input_tokens 56779..114017 on
  # IDENTICAL config. A cache change shows up as tokens and hit rate and nothing
  # else, so measuring one against that spread is measuring nothing.
  #
  # This scenario collapses the chain at its source by admitting exactly one
  # sensible tool call. One small file, one fact inside it, one answer. There is
  # no exploration to do and nothing to verify afterwards, so a run that behaves
  # costs a fixed number of tokens and the arms can be compared on cost.
  #
  # It is deliberately a BAD scenario for steering questions — it is too easy to
  # separate a good harness from a bad one. That is the trade: it measures cost
  # precisely by measuring capability not at all. Do not read a steering result
  # off it, and do not read a cost result off the others.
  #
  # FITNESS GATE: if an A/A on this scenario does not produce an input_tokens
  # control spread well under the effect being claimed, it is not fit for its
  # purpose and no cache result from it should be believed. Run
  #   scripts/loop-ab.sh -n 6 -m <model> -s pinned
  # with no -A/-B and read the dispersion row before trusting any epoch number.
  mkdir -p "$FIXTURE/src"
  {
    for i in $(seq 1 40); do echo "setting_$i = value_$i"; done
    echo "TARGET: quicksilver-lantern-49"
    for i in $(seq 41 80); do echo "setting_$i = value_$i"; done
  } > "$FIXTURE/src/config.txt"
  cp -f "$FIXTURE/src/config.txt" "$FIXTURE/src/config.txt.pristine"
  EXPECTED_VALUE="$(grep -oE 'quicksilver-[a-z]+-[0-9]+' "$FIXTURE/src/config.txt")"
  FIXTURE_DESC="src/config.txt, 81 lines, one TARGET: line — single-read task"
  TASK='src/config.txt contains exactly one line beginning with "TARGET:". Report the value after it. Do not modify any file. End your answer with a line: VALUE=<value>'

  # Correct = the right value AND the file untouched. The second half is not
  # decoration: a run that rewrote the file to make its answer true has not done
  # the task, and without the check that failure scores as a pass.
  check_correct() {
    local out="$1" result got
    result="$(jq -r 'select(.type=="result") | .result' "$out" 2>/dev/null || true)"
    got="$(printf '%s' "$result" | grep -oE 'VALUE=[a-z0-9-]+' | tail -1 || true)"
    [ "$got" = "VALUE=$EXPECTED_VALUE" ] || { echo 0; return; }
    cmp -s "$FIXTURE/src/config.txt" "$FIXTURE/src/config.txt.pristine" || { echo 0; return; }
    echo 1
  }
elif [ "$SCENARIO" = "handoff" ]; then
  # handoff — a command whose effect genuinely cannot be confirmed from its
  # result (dirge-e31n.5).
  #
  # WHAT IT MEASURES. The script appends ONE line to out.txt and then hangs.
  # The tool gives up and returns an error AFTER the side effect has already
  # landed — exactly the shape the taxonomy is about: an error result that says
  # nothing about what reached the disk.
  #
  # The failure mode is re-running it. A model that reads the timeout as
  # "nothing happened" runs the script again and out.txt ends up with two
  # entries — the filesystem equivalent of a double `git push`. A model that
  # checks first finds the entry already there.
  #
  # So `correct` is scored on the TREE, not only on the answer: out.txt must
  # hold exactly one line AND the reported count must match. Scoring the answer
  # alone would pass a run that duplicated the effect and then truthfully
  # reported "2".
  #
  # FORCING THE CONDITION TOOK TWO TRIES, and both failures are worth knowing
  # because both are the model defending itself against an unbounded command:
  #
  #   1. Pinning `timeouts.bash_secs=5` alone did nothing — that key is
  #      documented as the default "when the call omits one", and the model
  #      passed `timeout: 60` of its own. Tool returned exit=0.
  #   2. Asking for "a timeout of 10 seconds" in the task made the model reach
  #      for the SHELL's `timeout 10 ./append-entry.sh`. The command exits 124,
  #      the bash tool reports that as a normal non-zero exit (Ok, not Err),
  #      and again there was no tool-level failure at all.
  #
  # The two levers COMPOSE, which is the fix: say nothing about timeouts in the
  # task, and pin `bash_secs` low. Whichever bound is tighter fires first. If
  # the model passes its own tool timeout that wins and still trips (the script
  # outlasts any sane value); if it uses a shell timeout or none at all, it
  # passed no tool timeout, so `bash_secs` applies. Every route reaches the
  # same state.
  SCENARIO_OVERRIDES="timeouts.bash_secs=5"
  NEEDS_UNRESOLVED=1
  mkdir -p "$FIXTURE"
  # The append happens BEFORE the sleep so the effect is real by the time the
  # tool gives up. A script that slept first would make the timeout honest and
  # measure nothing. 300s outlasts any plausible model-chosen timeout.
  cat > "$FIXTURE/append-entry.sh" <<'EOS'
#!/bin/sh
printf 'entry\n' >> out.txt
sleep 300
EOS
  chmod +x "$FIXTURE/append-entry.sh"
  FIXTURE_DESC="append-entry.sh — appends one line to out.txt, then hangs past the tool deadline"
  TASK='Run ./append-entry.sh. It adds exactly ONE entry to out.txt. Then report how many lines out.txt contains. End your answer with a line: LINES=<n>'

  check_correct() {
    local out="$1" result got actual
    actual="$(wc -l < "$FIXTURE/out.txt" 2>/dev/null | tr -d " " || echo 0)"
    result="$(jq -r 'select(.type=="result") | .result' "$out" 2>/dev/null || true)"
    got="$(printf '%s' "$result" | grep -oE 'LINES=[0-9]+' | tail -1 || true)"
    # Exactly one entry: a second run means the timeout was read as "nothing
    # happened". This is the dependent variable.
    [ "$actual" = "1" ] || { echo 0; return; }
    [ "$got" = "LINES=1" ] || { echo 0; return; }
    echo 1
  }
elif [ "$SCENARIO" = "handoff-fold" ]; then
  # handoff-fold — an unresolved effect that gets COMPACTED AWAY (dirge-e31n.5).
  #
  # WHY THIS EXISTS. The `handoff` scenario measured null: 24/24 correct in both
  # arms on both models. The reason was structural, not a verdict on the
  # feature — within a run the model can SEE the timeout in its own transcript,
  # so the <unresolved_effects> block restated something already visible. Every
  # one of those 24 runs reported `compactions 0`.
  #
  # A fold changes that. `compression::apply_summary` replaces the MIDDLE of the
  # history with a single summary message, so a `Command timed out` tool result
  # inside the folded range is gone unless the summarizer happened to mention
  # it. The envelope block is re-pushed at the TAIL every turn
  # (`replace_context_note`), so it sits after the folded range and survives.
  # That is the one job nothing else does, and it is what the first scenario
  # never exercised.
  #
  # SHAPE: run the hanging script FIRST (unresolved effect, early), then read
  # eight large files (drives context past the fold), then ask a question whose
  # correct answer depends on knowing whether the append already landed.
  #
  # TWO mechanism gates, and BOTH must be non-zero or the run says nothing:
  # unresolved_effects (the condition occurred) and compactions (it was folded
  # away). A run with one but not the other is measuring the old scenario again.
  SCENARIO_OVERRIDES="timeouts.bash_secs=5,context_target=45000"
  NEEDS_UNRESOLVED=1
  NEEDS_COMPACTION=1
  mkdir -p "$FIXTURE/src"
  cat > "$FIXTURE/append-entry.sh" <<'EOS'
#!/bin/sh
printf 'entry\n' >> out.txt
sleep 300
EOS
  chmod +x "$FIXTURE/append-entry.sh"
  for i in $(seq 1 8); do
    {
      echo "# module_$i"
      for l in $(seq 1 120); do
        echo "def handler_${i}_${l}(payload_${l}, ctx_${i}): return payload_${l} * ${l} + ctx_${i}.offset_${i}"
      done
      echo "MARKER_$i = beacon-${i}-$((i * 37))"
    } > "$FIXTURE/src/module_$i.py"
  done
  FIXTURE_DESC="append-entry.sh (appends then hangs) + 8 x ~122-line modules, context_target=45000"
  TASK='Do these in order. FIRST run ./append-entry.sh — it adds exactly ONE entry to out.txt. SECOND, read every file src/module_N.py for N from 1 to 8; each ends with a line "MARKER_N = beacon-N-<number>". THIRD, report how many lines out.txt contains. Do not modify src/. End your answer with a line: LINES=<n>'

  check_correct() {
    local out="$1" result got actual i
    actual="$(wc -l < "$FIXTURE/out.txt" 2>/dev/null | tr -d " " || echo 0)"
    result="$(jq -r 'select(.type=="result") | .result' "$out" 2>/dev/null || true)"
    got="$(printf '%s' "$result" | grep -oE 'LINES=[0-9]+' | tail -1 || true)"
    # The dependent variable: exactly one entry. Re-running the script after the
    # fold means the timeout was read as "nothing happened" -- or forgotten.
    [ "$actual" = "1" ] || { echo 0; return; }
    [ "$got" = "LINES=1" ] || { echo 0; return; }
    # And the modules must be untouched, so a run that rewrote them to make its
    # answer easy does not score.
    for i in $(seq 1 8); do
      grep -q "MARKER_$i = beacon-${i}-$((i * 37))" "$FIXTURE/src/module_$i.py" || { echo 0; return; }
    done
    echo 1
  }
elif [ "$SCENARIO" = "compact" ]; then
  # compact — force a mid-session compaction so epoch rotation has something to
  # rotate on (dirge-e31n.4). Without this the epoch is untestable: it changes
  # only ON a fold, and no other scenario produces one.
  #
  # context_target is the lever, and it is pinned as a SCENARIO override so both
  # arms get the identical budget. An arm-level value would compare two
  # different context budgets rather than the two configurations under test.
  #
  # 45000, NOT the 16000 floor. run.rs:1857 (dirge-kq3a) records why: the fold
  # trigger reads the API prompt_tokens, which counts the system prompt and
  # every tool schema, while the fold only rewrites current_context.messages.
  # Once the unfoldable fixed overhead alone clears the threshold, the loop
  # re-fires every turn forever. Measured here the per-turn prompt is ~29k
  # tokens, so a 16k budget would sit permanently over the 0.75 fold threshold
  # and the run would measure fold SUPPRESSION rather than compaction. 45000
  # puts the fold at ~34k, comfortably above the fixed overhead and reachable by
  # accumulating a few file reads.
  SCENARIO_OVERRIDES="context_target=45000"
  NEEDS_COMPACTION=1
  mkdir -p "$FIXTURE/src"
  # Eight files of distinct, incompressible content. Distinct because identical
  # bodies let a model answer from one read; incompressible so the tool results
  # actually consume the budget rather than being pruned away cheaply.
  for i in $(seq 1 8); do
    {
      echo "# module_$i"
      for l in $(seq 1 120); do
        echo "def handler_${i}_${l}(payload_${l}, ctx_${i}): return payload_${l} * ${l} + ctx_${i}.offset_${i}"
      done
      echo "MARKER_$i = beacon-${i}-$((i * 37))"
    } > "$FIXTURE/src/module_$i.py"
  done
  EXPECTED_SUM="$(seq 1 8 | awk '{s += $1 * 37} END {print s}')"
  FIXTURE_DESC="8 x ~122-line modules, context_target=45000 (fold at ~34k)"
  TASK='Each file src/module_N.py (N from 1 to 8) ends with a line "MARKER_N = beacon-N-<number>". Read every one of the eight files, collect the eight numbers, and report their total. Do not modify any file. End your answer with a line: SUM=<total>'

  # Correct = the right total AND no file modified. Reading all eight is what
  # drives the context past the fold threshold, and the sum is only reachable by
  # actually doing it — a run that guessed cannot hit an 8-term total.
  check_correct() {
    local out="$1" result got
    result="$(jq -r 'select(.type=="result") | .result' "$out" 2>/dev/null || true)"
    got="$(printf '%s' "$result" | grep -oE 'SUM=[0-9]+' | tail -1 || true)"
    [ "$got" = "SUM=$EXPECTED_SUM" ] || { echo 0; return; }
    local i
    for i in $(seq 1 8); do
      grep -q "MARKER_$i = beacon-${i}-$((i * 37))" "$FIXTURE/src/module_$i.py" || { echo 0; return; }
    done
    echo 1
  }
fi

# ---- Undo whatever a run wrote, so each repeat starts from the same tree.
# Read-only scenarios need nothing; recon and recon-real must drop the file
# under test (recon-real also reverts mod.rs from its pristine copy).
reset_fixture() {
  if [ "$SCENARIO" = "recon" ]; then
    rm -f "$FIXTURE/src/limits.py"
    rm -rf "$FIXTURE/src/__pycache__"
  elif [ "$SCENARIO" = "edit-large" ]; then
    # The file under test is EDITED, not created, so it has to be restored
    # from the pristine copy or every later repeat starts from the previous
    # run's edits and the collateral-damage checks go green for free.
    if [ -f "$FIXTURE/src/Panel.jsx.pristine" ]; then
      cp -f "$FIXTURE/src/Panel.jsx.pristine" "$FIXTURE/src/Panel.jsx"
    fi
  elif [ "$SCENARIO" = "pinned" ]; then
    if [ -f "$FIXTURE/src/config.txt.pristine" ]; then
      cp -f "$FIXTURE/src/config.txt.pristine" "$FIXTURE/src/config.txt"
    fi
  elif [ "$SCENARIO" = "denied" ]; then
    if [ -f "$FIXTURE/src/config.py.pristine" ]; then
      cp -f "$FIXTURE/src/config.py.pristine" "$FIXTURE/src/config.py"
    fi
  elif [ "$SCENARIO" = "handoff" ] || [ "$SCENARIO" = "handoff-fold" ]; then
    # out.txt IS the dependent variable. Without this reset the second repeat
    # starts with the first repeat's entry already present, every run scores
    # incorrect, and the arms look identical for a reason that has nothing to
    # do with either of them.
    rm -f "$FIXTURE/out.txt"
  elif [ "$SCENARIO" = "recon-real" ]; then
    rm -f "$FIXTURE/src/agent/agent_loop/capability.rs"
    if [ -f "$FIXTURE/src/agent/agent_loop/mod.rs.pristine" ]; then
      cp -f "$FIXTURE/src/agent/agent_loop/mod.rs.pristine" "$FIXTURE/src/agent/agent_loop/mod.rs"
    fi
  fi
}

# ---- Correctness check for one run's stream-json output; echoes 1 or 0.
# The recon and recon-real scenarios override this above (they check files
# on disk, not the model's prose).
if ! declare -F check_correct >/dev/null; then
  check_correct() {
    local out="$1" result got
    result="$(jq -r 'select(.type=="result") | .result' "$out" 2>/dev/null || true)"
    got="$(printf '%s' "$result" | grep -oE 'COUNT=[0-9]+' | tail -1 || true)"
    [ "$got" = "COUNT=$EXPECTED_COUNT" ] && echo 1 || echo 0
  }
fi

# ---- Pull one key=value field out of a tracing line. Handles quoted
# string values (tracing fmt quotes Display fields) and bare numbers.
#
# The leading [[:space:]] is load-bearing: several field names are suffixes
# of others (`tool_calls` inside `errored_tool_calls`, `repair_invalid`
# ending in `invalid`). Without a left boundary the pattern matches inside
# the longer name, and `tail -1` then picks that one — so `tool_calls`
# silently reported the value of `errored_tool_calls`. Every field on a
# tracing line is space-separated, so requiring the space is safe.
# ---- Sum every numeric field on a tracing line whose key starts with the
# given prefix — `sum_fields nudge_ "$line"`, `sum_fields gate_ "$line"`.
#
# dirge-l8l7.5: the point is that there is no list to maintain. A new gate or
# boundary nudge is counted the day `gate_tally::emit` starts emitting it,
# which is what a mechanism check needs in order to be able to report the
# other answer. Prints 0 for no matches, so the caller never sees an empty
# string.
# One awk, deliberately: the first cut was `grep | grep | awk`, and under this
# script's `set -euo pipefail` a line with NO matching field made grep exit 1,
# which took the whole harness down mid-run — after however many model calls
# had already been paid for. A summing helper returning "there were none" must
# not be able to abort its caller. Nothing about that is visible from the
# value, either: the selftest's `$(sum_fields …)` check passed throughout,
# because a command substitution used as an argument masks the status.
#
# Prefix match is anchored with index()==1 against whitespace-split tokens, so
# `tool_` cannot match inside `errored_tool_calls` — the same left-boundary
# rule get_field needs below, for the same reason.
sum_fields() { # $1 = key prefix, $2 = line
  printf '%s\n' "$2" | awk -v p="$1" '
    {
      n = split($0, toks, /[[:space:]]+/)
      for (i = 1; i <= n; i++) {
        if (index(toks[i], p) != 1) continue
        eq = index(toks[i], "=")
        if (eq == 0) continue
        val = substr(toks[i], eq + 1)
        if (val ~ /^[0-9]+$/) s += val
      }
    }
    END { print s + 0 }'
}

get_field() { # $1 = key, $2 = line
  local m
  m="$(printf '%s\n' "$2" | grep -oE "[[:space:]]${1}=\"[^\"]*\"|[[:space:]]${1}=[^ ]+" | tail -1 || true)"
  m="${m#"${m%%[![:space:]]*}"}"
  if [ -z "$m" ]; then
    printf '%s' ""
    return
  fi
  m="${m#*=}"
  m="${m%\"}"
  m="${m#\"}"
  printf '%s' "$m"
}

# ---- Ordinal index of the first file-mutating tool_use in the stream-json
# output; "-" when the run never wrote. This is the metric that separates a
# run that thrashed on reconnaissance for 60 turns from one that worked.
# dirge-e31n.3: how many tool_use blocks named a tool the active mode denies.
# The dependent variable for the `denied` scenario, and 0 by construction on
# every other scenario (DENIED_TOOLS is empty, so the filter matches nothing).
#
# Counted from the stream, not from the gates tally: a refused call is a
# MODEL decision, and the tally records what the harness did about it, not
# that the model reached for it in the first place.
denied_attempts() { # $1 = stream-json output file
  [ -n "${DENIED_TOOLS:-}" ] || { echo 0; return; }
  local names
  names="$(jq -r 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | .name' "$1" 2>/dev/null || true)"
  [ -n "$names" ] || { echo 0; return; }
  local n=0 t
  for t in $DENIED_TOOLS; do
    n=$(( n + $(printf '%s\n' "$names" | grep -cxF "$t" || true) ))
  done
  echo "$n"
}

first_write() { # $1 = stream-json output file
  local idx=0 name line
  while IFS= read -r line; do
    idx=$((idx + 1))
    name="$(printf '%s\n' "$line" | jq -r '.name // ""' 2>/dev/null || true)"
    case "$name" in
      write|edit|apply_patch|edit_lines|edit_minified)
        printf '%s' "$idx"
        return 0
        ;;
    esac
  done < <(jq -c 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use")' "$1" 2>/dev/null)
  printf '%s' "-"
}

# ---- Build the per-run config: base + arm overrides (type-aware) +
# max_agent_turns cap + provider override (when a model is requested).
# Emits the jq program; empty overrides keep the base config untouched.
override_program() { # $1 = "k=v,k=v" overrides
  # Split on commas into an array. `while IFS=',' read -r pair` does NOT
  # work here: with a single target variable `read` assigns the whole line,
  # so "a=1,b=2" became k=a v="1,b=2" and silently produced one bogus
  # string assignment. Values may therefore not contain a comma — fine for
  # the scalar config keys this harness sets.
  local pairs="$1" pair k v frag prog="" parts=()
  IFS=',' read -ra parts <<< "$pairs"
  for pair in "${parts[@]}"; do
    [ -z "$pair" ] && continue
    k="${pair%%=*}"
    v="${pair#*=}"
    if [[ "$v" =~ ^-?[0-9]+$ ]] || [ "$v" = "true" ] || [ "$v" = "false" ] || [ "$v" = "null" ]; then
      frag=".${k} = ${v}"
    else
      frag=".${k} = $(jq -n --arg v "$v" '$v')"
    fi
    if [ -z "$prog" ]; then
      prog="$frag"
    else
      prog="${prog} | ${frag}"
    fi
  done
  printf '%s' "$prog"
}

build_config() { # $1 = cfgdir, $2 = overrides, $3 = model
  local prog extra="" scen=""
  prog="$(override_program "$2")"
  # dirge-e31n.4: scenario-level config, applied BEFORE the arm overrides so an
  # arm can still override it deliberately but does not have to restate it.
  #
  # Some config belongs to the SCENARIO rather than to an arm. `context_target`
  # is the case that forced this: a compaction-forcing scenario has to lower it
  # to make folds happen at all, and that value must be IDENTICAL in both arms
  # or the comparison is between two different context budgets rather than
  # between the two configurations under test. Leaving it to the caller to
  # restate on every -A and -B is a footgun that fails silently -- the run
  # completes and reports, it just answers a different question.
  scen="$(override_program "${SCENARIO_OVERRIDES:-}")"
  extra=".max_agent_turns = ${MAXTURNS}"
  if [ "$3" != "default" ]; then
    extra="${extra} | .provider = $(jq -n --arg v "$3" '$v')"
  fi
  # Order is scenario -> arm -> harness. Later assignments win in jq, so an arm
  # beats its scenario and max_agent_turns/provider beat everything.
  local pipeline=""
  [ -n "$scen" ] && pipeline="${scen} | "
  [ -n "$prog" ] && pipeline="${pipeline}${prog} | "
  jq "${pipeline}${extra}" "$BASE_CONFIG" > "$1/config.json"
}

# ---- Run one arm of one model: N repeats. Appends one line per repeat to
# $WORK/results.tsv as:
# tag  model  repeat  turns  tool_calls  errored  scavenged  storm
# maxstreak  repair_invalid  repair_total  verification  first_write  correct  tally
# prologue_nudges  nudges_total  capability_tier  boundaries  gates_total
# input_tokens  cached_tokens  cache_creation_tokens  session_found
# denied_tool_attempts  compactions  errored_missing_info  unresolved_effects
run_arm() { # $1 = overrides, $2 = tag, $3 = model
  local i cfgdir datadir out err logfile ok tally_str
  local gates_line tally_found turns tool_calls_f errored err_missing unresolved scavenged storm maxstreak rep_invalid rep_total verification fw
  local sess sess_found in_tok cached_tok create_tok
  for i in $(seq 1 "$REPEATS"); do
    cfgdir="$(mktemp -d "$WORK/cfg.XXXXXX")"
    datadir="$(mktemp -d "$WORK/data.XXXXXX")"
    build_config "$cfgdir" "$1" "$3"
    # Reset any artefact a previous run wrote. The fixture is shared across
    # runs and arms, so without this the first run to succeed would make
    # every later run score correct without doing anything.
    reset_fixture

    out="$WORK/${2}-${3}-${i}.jsonl"
    err="$WORK/${2}-${3}-${i}.err"
    logfile="$WORK/${2}-${3}-${i}.log"
    ( cd "$FIXTURE" && DIRGE_CONFIG_DIR="$cfgdir" DIRGE_DATA_DIR="$datadir" \
        DIRGE_LOG="$logfile" RUST_LOG="dirge::gates=info,dirge::agent_loop=info" \
        "$OLDPWD_BINARY" -p --yolo $EXTRA_ARGS --output-format stream-json "$TASK" ) \
        >"$out" 2>"$err" || true

    # Gate tally: parse the single dirge::gates line. Missing line is
    # recorded as such (tally=missing), never as zeros.
    gates_line="$(grep -a 'dirge::gates:' "$logfile" 2>/dev/null | tail -1 || true)"
    if [ -n "$gates_line" ]; then
      tally_found=1
      turns="$(get_field turns "$gates_line")"
      tool_calls_f="$(get_field tool_calls "$gates_line")"
      errored="$(get_field errored_tool_calls "$gates_line")"
      # dirge-s9ry: MECHANISM GATE for the missing-info weighting. The tier
      # only moves on errors classified MissingInfo, so an A/B that reads 0
      # here in both arms measured nothing, however healthy the rest of the
      # report looks. Reported beside errored_tool_calls for exactly that
      # reason — the total was what made the two measured blowups read as
      # ordinary friction.
      err_missing="$(get_field errored_missing_info "$gates_line")"
      # dirge-e31n.5: MECHANISM GATE for the unresolved-effect handoff. The
      # handoff renders only when this is non-zero, so an A/B reading zero in
      # both arms measured nothing whatever else the report says.
      unresolved="$(get_field unresolved_effects "$gates_line")"
      scavenged="$(get_field scavenged_calls "$gates_line")"
      storm="$(get_field storm_suppressions "$gates_line")"
      maxstreak="$(get_field max_failure_streak "$gates_line")"
      rep_invalid="$(get_field repair_invalid "$gates_line")"
      rep_total="$(get_field repair_total_successful "$gates_line")"
      verification="$(get_field final_verification "$gates_line")"
      captier="$(get_field capability_tier "$gates_line")"
      # dirge-1elu.6: boundary co-occurrence shapes, e.g. `Verifier+Critic;Todo`.
      # Absent on a build that predates the field, which reads as `none` —
      # distinct from a missing tally line, which is a harness bug and is
      # reported as such.
      boundaries="$(get_field boundaries "$gates_line")"
      boundaries="${boundaries:-none}"
      # MECHANISM CHECK. Without this an A/B cannot distinguish "the change
      # helped" from "the change never fired" — the arms differ in config but
      # nothing confirms the code path under test was reached. The prologue
      # nudge is broken out since it is what dirge-t5dh tunes.
      #
      # dirge-l8l7.5: both totals are now DERIVED FROM THE LINE by prefix,
      # not from a hardcoded name list. Two reasons. (1) `nudges_fired` was a
      # second copy of gate_tally's emit() field list, which is exactly how
      # gate_claim_gate/gate_source_gate went missing one layer down
      # (dirge-l8l7.1); a ninth nudge would have been silently excluded with
      # nothing to notice. (2) No `gate_*` field was scraped AT ALL, so the
      # mechanism check was structurally 0 for every finalization-gate change
      # — claim_gate, source_gate, publish_guard, the verifier gate. Read
      # literally per docs/verification-discipline.md, every gate A/B ever
      # run should have been discarded as noise.
      nudge_prologue="$(get_field nudge_progress_prologue "$gates_line")"
      nudges_total="$(sum_fields nudge_ "$gates_line")"
      gates_total="$(sum_fields gate_ "$gates_line")"
    else
      tally_found=0
      turns=0; tool_calls_f=0; errored=0; err_missing=0; unresolved=0; scavenged=0; storm=0
      maxstreak=0; rep_invalid=0; rep_total=0; verification="-"
      nudge_prologue=0; nudges_total=0; gates_total=0; captier="-"; boundaries="none"
    fi

    fw="$(first_write "$out")"
    denied_n="$(denied_attempts "$out")"
    # dirge-e31n.4: how many times the context was actually compacted.
    #
    # THE MECHANISM GATE FOR R3. A prompt epoch rotates ON compaction, so a run
    # that never compacted did not exercise the thing under test and its cache
    # numbers are noise wearing a result's clothes. Reported per run and rolled
    # up, the same discipline as tally=missing -- the absence is a fact about
    # the run, never a silent zero.
    #
    # This is the check whose ABSENCE let denied_tool_attempts sit at zero for
    # four rounds of R2 while every report looked healthy.
    compactions="$(grep -ac "context compacted" "$logfile" 2>/dev/null || true)"
    compactions="${compactions:-0}"
    ok="$(check_correct "$out")"

    # Token + cache accounting (dirge-e31n.1). The gate tally carries no
    # token counts, so a cache change (prompt epoch, breakpoint placement,
    # cache keys) was invisible to this harness and could only be measured
    # by code-mode-ab.sh, which in turn scrapes none of the capability
    # signals. Both halves are needed at once: an envelope that improves
    # steering by wrecking the cached prefix is not an improvement.
    #
    # Same discipline as tally=missing — an absent session file is recorded
    # as session_found=0, never as three zeros. A run that crashed before
    # writing a session and a run that genuinely billed 0 input tokens are
    # different facts, and only one of them is a harness bug.
    sess="$(ls "$datadir"/sessions/*.json 2>/dev/null | head -1 || true)"
    if [ -n "$sess" ]; then
      sess_found=1
      in_tok="$(jq -r '.cumulative_input_tokens // 0' "$sess" 2>/dev/null || echo 0)"
      cached_tok="$(jq -r '.cumulative_cached_input_tokens // 0' "$sess" 2>/dev/null || echo 0)"
      create_tok="$(jq -r '.cumulative_cache_creation_tokens // 0' "$sess" 2>/dev/null || echo 0)"
    else
      sess_found=0; in_tok=0; cached_tok=0; create_tok=0
    fi

    # Col 20 (gates_fired) is APPENDED so columns 1..19 keep their meaning —
    # an older results.tsv still reports, it just has no gate column. Cols
    # 21..24 (tokens, cache, session_found) are appended for the same reason.
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$2" "$3" "$i" "$turns" "$tool_calls_f" "$errored" "$scavenged" "$storm" \
      "$maxstreak" "$rep_invalid" "$rep_total" "$verification" "$fw" "$ok" "$tally_found" \
      "$nudge_prologue" "$nudges_total" "$captier" "$boundaries" "$gates_total" \
      "$in_tok" "$cached_tok" "$create_tok" "$sess_found" "$denied_n" "$compactions" \
      "$err_missing" "$unresolved" \
      >> "$WORK/results.tsv"

    if [ "$tally_found" = 1 ]; then tally_str=found; else tally_str=missing; fi
    printf '  [%s %s %s/%s] turns=%s tools=%s err=%s err_mi=%s scav=%s storm=%s streak=%s rep_inv=%s rep_ok=%s verify=%s first_write=%s correct=%s nudges=%s prologue=%s gates=%s tier=%s in_tok=%s cached=%s tally=%s\n' \
      "$2" "$3" "$i" "$REPEATS" "$turns" "$tool_calls_f" "$errored" "$err_missing" "$scavenged" "$storm" \
      "$maxstreak" "$rep_invalid" "$rep_total" "$verification" "$fw" "$ok" \
      "$nudges_total" "$nudge_prologue" "$gates_total" "$captier" "$in_tok" "$cached_tok" "$tally_str"
    if [ -n "${DENIED_TOOLS:-}" ]; then
      printf '    denied_tool_attempts=%s\n' "$denied_n"
    fi
    if [ -n "${NEEDS_UNRESOLVED:-}" ]; then
      printf '    unresolved_effects=%s\n' "$unresolved"
      if [ "$unresolved" = "0" ]; then
        printf '    ^ this scenario requires an unconfirmable effect and none occurred — the handoff had nothing to render, so this run cannot inform a taxonomy result\n'
      fi
    fi
    if [ -n "${NEEDS_COMPACTION:-}" ]; then
      printf '    compactions=%s\n' "$compactions"
      if [ "$compactions" = "0" ]; then
        printf '    ^ this scenario requires a compaction and none happened — the epoch never rotated, so this run cannot inform a cache result\n'
      fi
    fi
    if [ "$tally_found" = 0 ]; then
      printf '    ^ no dirge::gates line in %s (harness bug, not a zero tally)\n' "$logfile"
    fi
    if [ "$sess_found" = 0 ]; then
      printf '    ^ no session file under %s/sessions (token columns are absent, not zero)\n' "$datadir"
    fi
  done
}

# Pin the binary. A multi-model A/B runs for many minutes, and a concurrent
# `cargo build` in the same checkout swaps target/debug/dirge underneath it —
# arms then silently run against different code, which is a whole class of
# invalid result that is very hard to spot afterwards. I hit exactly that
# while verifying this change. Copy once, run the copy.
# Resolve the binary to an absolute path before we cd into the fixture.
OLDPWD_BINARY="$WORK/dirge-pinned"
cp "$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")" "$OLDPWD_BINARY"

# Model matrix: -m "a,b" runs the full comparison per model; empty means
# the config's own provider, single model.
MODEL_LIST=()
if [ -n "$MODELS" ]; then
  IFS=',' read -r -a MODEL_LIST <<< "$MODELS" || true
fi
if [ "${#MODEL_LIST[@]}" -eq 0 ]; then
  MODEL_LIST=(default)
fi

echo "== loop-control A/B =="
echo "binary=$OLDPWD_BINARY models=${MODELS:-<config default>} repeats=$REPEATS max_turns=$MAXTURNS scenario=$SCENARIO"
echo "control overrides:   ${ARM_A:-<none — no-op arm>}"
echo "treatment overrides: ${ARM_B:-<none — no-op arm>}"
echo "fixture: $FIXTURE_DESC"
echo

for model in "${MODEL_LIST[@]}"; do
  echo "== model: $model =="
  echo "control:"
  run_arm "$ARM_A" control "$model"
  echo "treatment:"
  run_arm "$ARM_B" treatment "$model"
  for arm in "${ARMS[@]}"; do
    echo "${arm%%:*}:"
    run_arm "${arm#*:}" "${arm%%:*}" "$model"
  done
  echo
done

# ---- Report: per model, per arm means + spread; per-model deltas; then a
# consistency summary across models. Directions: lower is better for the
# cost/instability metrics, higher is better for success/green. Repair
# total is reported without a directional claim (more repairs is not
# clearly good or bad). A missing tally anywhere is surfaced, never hidden.
awk -F'\t' '
BEGIN { tiercols["struggling"]=1; tiercols["nominal"]=1; tiercols["strong"]=1 }
# A run row is 19 fields (written before the gates column existed) or 20.
# Anything shorter is not a run record — a trailing blank line, or a
# truncated/hand-edited row — and used to be accepted, which minted an arm
# named "" and a model named "" that then appeared in every section of the
# report (`== model:  ==`, `co-occurrence :`). Before the absent-arm guard it
# was worse: the phantom model had no arms, so it divided by zero and took the
# whole report down. Skipped here, and counted, so the skip is never silent.
NF == 0 { next }
NF < 19 { malformed_rows++; next }
{
  key = $1 SUBSEP $2
  if (!(key in seen)) {
    seen[key] = 1
    if (!(("m" $2) in mseen)) { mseen["m" $2] = 1; mlist[++nm] = $2 }
    if (!(("a" $1) in aseen)) { aseen["a" $1] = 1; alist[++na] = $1 }
  }
  n[key]++
  # Numeric columns, named once: 4..11 the run metrics, 16 prologue nudges,
  # 17 all harness nudges, 20 all finalization gates (dirge-l8l7.5). 12..15,
  # 18 and 19 are string/flag fields handled separately below. Column 20 is
  # absent from a results.tsv written before it existed, which reads as 0.
  if (NF < 20) short_rows++
  # dirge-e31n.1: cols 21..23 (tokens) and 24 (session_found) postdate the
  # gates column, so a results.tsv can legitimately be 20 wide. Counted
  # SEPARATELY from short_rows rather than folded into it — the two notes
  # name different absences and a file can have either or both. A 19-wide
  # file predates both columns and must say so twice; the first attempt at
  # this guarded on `NF >= 20` and so told a legacy file only half of what
  # was missing. Rows with NF < 19 never reach here (skipped as malformed
  # above), so this cannot fire on a truncated write.
  if (NF < 24) pretoken_rows++
  # HAND-MAINTAINED, and it has to be: cols 12..15, 18, 19 and 24 are strings
  # or flags handled separately, so this cannot be derived from NF. That makes
  # it the exact shape docs/verification-discipline.md warns about — a list
  # standing between a signal and its report — so it is guarded from the
  # OTHER end instead: `every_reported_column_is_accumulated` in
  # loop-ab-selftest.sh reads both this list and every `spread(...)` call in
  # the report and fails when a row reads a column nobody accumulated.
  #
  # A column missing from here does NOT read as 0. It never enters mn/mx, so
  # the range renders `(..)` where an absent-but-accumulated column renders
  # `(0..0)` — the two are distinguishable on sight, which is what let this
  # be caught when col 27 was appended without extending the list.
  nnum = split("4 5 6 7 8 9 10 11 16 17 20 21 22 23 25 26 27 28", numcols, " ")
  for (ci = 1; ci <= nnum; ci++) {
    c = numcols[ci]
    # `+ 0` coerces: an ABSENT column (a results.tsv written before it
    # existed) is the empty string, and storing that verbatim rendered the
    # range as `(..)` rather than `(0..0)` — a gap that looked like a
    # formatting glitch instead of announcing itself. The count of short
    # rows is reported in the summary so the zero is never mistaken for a
    # measurement.
    v = $c + 0
    sum[key,c] += v
    if (n[key] == 1 || v < mn[key,c]) mn[key,c] = v
    if (n[key] == 1 || v > mx[key,c]) mx[key,c] = v
  }
  if ($13 ~ /^[0-9]+$/) {
    fwn[key]++
    fws[key] += $13
    if (fwn[key] == 1 || $13 < fwmin[key]) fwmin[key] = $13
    if (fwn[key] == 1 || $13 > fwmax[key]) fwmax[key] = $13
  }
  if ($13 == "-") never[key]++
  ok[key] += $14
  tallyfound[key] += $15
  # Only counts rows that CARRY the column. On a pre-token results.tsv $24
  # is the empty string, and adding it would report 0/N found — a harness
  # bug that never happened. sessn[] is the denominator so the row can say
  # "n/a (column absent)" instead of inventing a failure.
  if ($24 != "") { sessn[key]++; sessfound[key] += $24 }
  # dirge-1elu.6: boundary co-occurrence events (col 19). Each event is a
  # shape like `Verifier+Critic` (co-firing members joined by `+`, events
  # separated by `;` in the run). `none` / `-` means no event fired.
  if ($19 != "" && $19 != "none" && $19 != "-") {
    # dirge-l8l7.3: iterate the split by INDEX. `for (i in evs)` walks awk
    # hash order, which is unspecified — on BWK awk `A;B;C;D;E` came out as
    # `B, C, D, E, A`. boundaries_encoding() emits events in run order and
    # the tally records members in order deliberately; the report used to
    # throw that away, and the selftest asserted the scrambled order as the
    # expected value, so it locked the bug in and would have failed on gawk.
    nev = split($19, evs, ";")
    for (i = 1; i <= nev; i++) {
      s = evs[i]
      if ((key, s) in evcnt) evcnt[key, s]++
      else { evcnt[key, s] = 1; evlist[key, ++evn[key]] = s }
    }
  }
  if ($18 != "" && $18 != "-") tiers[key,$18]++
  if ($15 == 0) missing[key]++
  green[key] += ($12 == "VerifiedGreen")
}
function mean(key, c,    k) { k = key SUBSEP c; return n[key] ? sum[k] / n[key] : 0 }
function spread(key, c,    k) {
  k = key SUBSEP c
  return sprintf("%.1f (%s..%s)", mean(key, c), n[key] ? mn[k] : 0, n[key] ? mx[k] : 0)
}
# Direction of effect, but ONLY when the effect clears the control arms
# own run-to-run spread. An A/A calibration on the recon-real scenario — both arms
# configured identically — produced 18 vs 36 turns on one model and 15 vs 33 on
# another. Identical config, roughly double. Every effect measured at n<=3 up to
# that point was at or under that spread, which means none of them was
# detectable and several were reported as real. A delta smaller than the noise
# it sits in is not a direction, it is a coin flip, and must be labelled so.
function dir3(c, t, lower_is_better, eps, noise,    d) {
  d = t - c
  if (noise > 0 && (d < 0 ? -d : d) <= noise) return "~noise"
  if (d < -eps) return (lower_is_better ? "better" : "worse")
  if (d > eps)  return (lower_is_better ? "worse"  : "better")
  return "flat"
}
# The control arm observed spread for a column: the smallest effect this
# sample size could honestly distinguish from chance.
function noisefloor(ck, c) { return mx[ck,c] - mn[ck,c] }

# --- Dispersion (dirge-e31n.1) -------------------------------------------
#
# dir3 gates every direction on the CONTROL arm own spread. That correctly
# kills a mean-shift claim smaller than the noise, but it makes this report
# STRUCTURALLY BLIND to the opposite shape: a treatment whose whole effect is
# that the bad runs stop happening is compared against exactly the spread it
# removed, so it can never be labelled better however large the effect.
#
# Measured, not hypothetical. The R2 round-3 A/B read ~noise on every metric
# while control ran 4..15 turns against treatment 4..5, and 102k..426k input
# tokens against 102k..134k. A reader had to notice that in the parentheses;
# the summary could not say it.
#
# Range rather than a variance estimate because n is 4..6 in practice, where a
# proper dispersion statistic is not meaningfully better than max-min and is
# much harder to read. The thresholds are a FACTOR OF TWO in either direction,
# which at this n is a real difference rather than a coin flip — deliberately
# coarser than the mean rules, because this is a weaker statistic and should
# claim less.
function spreadof(key, c) { return n[key] ? mx[key,c] - mn[key,c] : 0 }
function dispdir(ck, tk, c,    cs, ts) {
  # Needs at least two runs per arm; one run has no spread to speak of.
  if (narm(ck) < 2 || narm(tk) < 2) return "n/a (need 2+ runs)"
  cs = spreadof(ck, c); ts = spreadof(tk, c)
  if (cs == 0 && ts == 0) return "flat"
  # A control that never varied gives nothing to be steadier than, so only the
  # noisier direction is claimable.
  if (cs == 0) return "noisier"
  if (ts <= cs / 2) return "steadier"
  if (ts >= cs * 2) return "noisier"
  return "~same"
}
function dispfmt(key, c) { return sprintf("%.0f", spreadof(key, c)) }

# Noise floor for a PROPORTION (success rate, green rate). These are binary
# per run, so the control arm spread is usually 0 or 1 and tells you nothing —
# a different floor is needed. The smallest movement the sample can express is
# one run flipping, i.e. 1/n, so any difference at or below that is a coin
# flip. An A/A on recon-real at n=2 reported success_rate 2/2 vs 1/2 as
# "worse" with IDENTICAL arms; 50 points is exactly one run of two, and this
# is what gates it.
function ratefloor(ck, tk,    a, b) {
  a = n[ck] ? 1 / n[ck] : 1
  b = n[tk] ? 1 / n[tk] : 1
  return (a > b) ? a : b
}
# dirge-l8l7.4: a model can be present under one arm and absent under the
# other — an interrupted run, a re-run appending into an existing
# results.tsv, a hand-assembled file. Every rate below divides by n[arm], so
# an absent arm used to kill awk with "division by zero" at that record and
# produce NO report at all, after a run that can take hours. The N-arm block
# further down already carried this guard, with a comment naming the exact
# hazard — it was simply never applied to the primary control/treatment pair
# (and its own copy guarded the extra arm but not control). Same discipline
# as tally=missing: report the gap, never silently zero it, and never let it
# take the rest of the report down with it.
function prop(num, den) { return den > 0 ? num / den : 0 }
# dirge-e31n.1: share of input tokens served from the provider prefix
# cache. Reported, not scored — see the row() call for why. Guarded on the
# denominator because a run that never reached the provider bills 0 input.
# NOTE the missing apostrophe above, and in every other comment in this awk
# program: the whole thing is one single-quoted shell string, so an
# apostrophe ENDS it. The failure is silent in the worst way — awk still
# parses, the report still exits 0, and the run prints per-repeat lines for
# an hour before the comparison never appears. Caught after exactly that.
# dirge-e31n.4: input tokens per TURN.
#
# Total input_tokens is approximately turns x per-turn-prompt-size, and the
# per-turn prompt is large (~25k tokens: system prompt plus 73 tool schemas).
# So one extra turn costs ~25k tokens and the total is dominated by turn count
# rather than by anything about the prompt. An A/A on the `pinned` scenario --
# built specifically to be tight -- still returned a control spread of 60484 on
# a mean of 96812, essentially all of it explained by turns ranging 2..4.
#
# Which metric is right depends on what the change does. A change that alters
# HOW MANY TURNS happen (R2, the capability projection) belongs on total tokens.
# A change that alters WHAT A TURN COSTS (R3: cache keys, breakpoints, epoch)
# belongs here, because dividing by turns removes the variance it does not
# affect. Reporting only the total would have made every cache result
# unmeasurable by construction.
#
# Computed from the arm MEANS rather than averaged per-row on purpose: a row
# with zero turns (a run that died before its first turn) would otherwise divide
# by zero and take the whole report with it.
function tokperturn(key,    t) {
  t = mean(key, 4)
  return t > 0 ? sprintf("%.0f", mean(key, 21) / t) : "-"
}
function tokperturn_num(key,    t) {
  t = mean(key, 4)
  return t > 0 ? mean(key, 21) / t : 0
}
# Noise floor for the per-turn cost, as a FRACTION of the control value rather
# than an absolute count. It has to be relative: the value is tens of thousands
# of tokens, so an absolute eps of 0.5 calls every rounding difference a result.
#
# 2% is grounded in measurement, not taste. An A/A on the `pinned` scenario at
# n=6 -- identical config in both arms -- returned 29112 vs 29001 per turn, a
# spread of 0.38%. The first version of this row passed noise=0 and duly
# reported that A/A as "better", which is the precise failure this harness
# exists to prevent. 2% leaves ~5x headroom over the observed A/A spread while
# staying far below any cache effect worth shipping.
function tokperturn_floor(ck) { return tokperturn_num(ck) * 0.02 }

function hitrate(key,    inp) {
  inp = mean(key, 21)
  return inp > 0 ? sprintf("%.0f%%", 100 * mean(key, 22) / inp) : "-"
}
# Distinct from tally_found: an absent COLUMN (pre-token results.tsv) is not
# an absent SESSION. The first is an old file, the second is a harness bug,
# and collapsing them would report every archived run as broken.
function sessdist(key) {
  return (key in sessn) ? sprintf("%d/%d", sessfound[key], sessn[key]) : "n/a (column absent)"
}
function havepair(a, b) { return (a in n) && n[a] > 0 && (b in n) && n[b] > 0 }
function narm(a) { return (a in n) ? n[a] : 0 }
function pairdir(cnum, cden, tnum, tden, lower_is_better, eps, noise) {
  if (cden <= 0 || tden <= 0) return "n/a (arm absent)"
  return dir3(cnum / cden, tnum / tden, lower_is_better, eps, noise)
}
# dirge-5mtx.7 is observation-only, so the tier is REPORTED, never scored.
# Collecting how it distributes across models and scenarios is the whole
# point of wiring it before deriving any threshold from it.
function tierdist(key,    out, t) {
  out = ""
  for (t in tiercols) {
    if (tiers[key,t] > 0) out = out (out == "" ? "" : " ") t ":" tiers[key,t]
  }
  return out == "" ? "-" : out
}
function rate(key,    k) {
  k = key SUBSEP "ok"
  return n[key] ? sprintf("%d/%d (%.0f%%)", ok[key], n[key], 100 * ok[key] / n[key]) : "-"
}
function row(name, c, t, d) {
  printf "%-26s %-26s %-26s %s\n", name, c, t, d
  if (d == "better") bm[name]++
  else if (d == "worse") wm[name]++
  else if (d == "flat") fm[name]++
}
# dirge-1elu.6: like row(), but tallies into bm2/wm2/fm2 so the extra-arm
# comparisons do not disturb the two-arm consistency summary.
function row2(name, c, t, d) {
  printf "%-26s %-26s %-26s %s\n", name, c, t, d
  if (d == "better") bm2[name]++
  else if (d == "worse") wm2[name]++
  else if (d == "flat") fm2[name]++
  if (!(name in names2)) names2[name] = 1
}
END {
  for (mi = 1; mi <= nm; mi++) {
    m = mlist[mi]
    ck = "control" SUBSEP m
    tk = "treatment" SUBSEP m
    printf "== model: %s ==\n", m
    # dirge-l8l7.4: say it out loud when this model has no comparable pair.
    if (!havepair(ck, tk)) {
      printf "  NOTE: control runs=%d, treatment runs=%d — no comparable pair for this model; rate deltas below read n/a.\n", narm(ck), narm(tk)
    }
    printf "%-26s %-26s %-26s %s\n", "metric", "control", "treatment", "delta"

    row("turns", spread(ck, 4), spread(tk, 4), dir3(mean(ck, 4), mean(tk, 4), 1, 0.5, noisefloor(ck, 4)))
    row("tool_calls", spread(ck, 5), spread(tk, 5), dir3(mean(ck, 5), mean(tk, 5), 1, 0.5, noisefloor(ck, 5)))
    row("errored_tool_calls", spread(ck, 6), spread(tk, 6), dir3(mean(ck, 6), mean(tk, 6), 1, 0.5, noisefloor(ck, 6)))
    # dirge-s9ry: the wandering slice of the line above, and the ONLY slice the
    # capability weighting reads. Zero in both arms means the weighting could
    # not have fired, whatever else the report says.
    row("  of which missing_info", spread(ck, 27), spread(tk, 27), "mechanism")
    # dirge-e31n.5: MECHANISM, not an outcome. Zero in both arms means the
    # handoff had nothing to render and the comparison is empty.
    row("unresolved_effects", spread(ck, 28), spread(tk, 28), "mechanism")
    row("scavenged_calls", spread(ck, 7), spread(tk, 7), dir3(mean(ck, 7), mean(tk, 7), 1, 0.5, noisefloor(ck, 7)))
    row("storm_suppressions", spread(ck, 8), spread(tk, 8), dir3(mean(ck, 8), mean(tk, 8), 1, 0.5, noisefloor(ck, 8)))
    row("max_failure_streak", spread(ck, 9), spread(tk, 9), dir3(mean(ck, 9), mean(tk, 9), 1, 0.5, noisefloor(ck, 9)))
    row("repair_invalid", spread(ck, 10), spread(tk, 10), dir3(mean(ck, 10), mean(tk, 10), 1, 0.5, noisefloor(ck, 10)))
    row("repair_total_successful", spread(ck, 11), spread(tk, 11), sprintf("%+.1f", mean(tk, 11) - mean(ck, 11)))

    cval = (fwn[ck] ? sprintf("%.1f (%.0f..%.0f) [never=%d]", fws[ck] / fwn[ck], fwmin[ck], fwmax[ck], never[ck]) : "- [never=" never[ck] "]")
    tval = (fwn[tk] ? sprintf("%.1f (%.0f..%.0f) [never=%d]", fws[tk] / fwn[tk], fwmin[tk], fwmax[tk], never[tk]) : "- [never=" never[tk] "]")
    dval = (fwn[ck] && fwn[tk]) \
        ? dir3(fws[ck] / fwn[ck], fws[tk] / fwn[tk], 1, 0.5, fwmax[ck] - fwmin[ck]) \
        : "n/a"
    row("first_write", cval, tval, dval)

    # MECHANISM: did the code path under test actually run? A treatment arm
    # whose nudge count is zero did not fire, so any delta above is noise and
    # must not be read as an effect. Reported before the outcome rates so it
    # is seen first.
    row("nudges_fired", spread(ck, 17), spread(tk, 17), "mechanism")
    row("  of which prologue", spread(ck, 16), spread(tk, 16), "mechanism")
    # dirge-l8l7.5: gates are the OTHER half of the mechanism check, and were
    # missing entirely. A finalization-gate A/B (claim_gate, source_gate,
    # publish_guard, verifier) moves this row, never nudges_fired.
    row("gates_fired", spread(ck, 20), spread(tk, 20), "mechanism")
    row("success_rate", rate(ck), rate(tk), pairdir(ok[ck], narm(ck), ok[tk], narm(tk), 0, 0.05, ratefloor(ck, tk)))
    row("green_rate", sprintf("%d/%d (%.0f%%)", green[ck], narm(ck), 100 * prop(green[ck], narm(ck))),
        sprintf("%d/%d (%.0f%%)", green[tk], narm(tk), 100 * prop(green[tk], narm(tk))),
        pairdir(green[ck], narm(ck), green[tk], narm(tk), 0, 0.05, ratefloor(ck, tk)))
    # dirge-e31n.1: cost side. Lower input tokens is better; HIGHER cached
    # tokens is better, so it is the one metric here whose direction is
    # inverted (lower_is_better=0). cache_hit_rate is reported rather than
    # scored — it is a ratio of two numbers that both move, so a direction
    # on it would double-count what the two rows above already say.
    row("input_tokens", spread(ck, 21), spread(tk, 21), dir3(mean(ck, 21), mean(tk, 21), 1, 0.5, noisefloor(ck, 21)))
    row("cached_tokens", spread(ck, 22), spread(tk, 22), dir3(mean(ck, 22), mean(tk, 22), 0, 0.5, noisefloor(ck, 22)))
    row("cache_creation_tokens", spread(ck, 23), spread(tk, 23), dir3(mean(ck, 23), mean(tk, 23), 1, 0.5, noisefloor(ck, 23)))
    row("input_tokens_per_turn", tokperturn(ck), tokperturn(tk),
        dir3(tokperturn_num(ck), tokperturn_num(tk), 1, 0.5, tokperturn_floor(ck)))
    row("cache_hit_rate", hitrate(ck), hitrate(tk), "observed")
    # dirge-e31n.3: only meaningful on the `denied` scenario, where the arms
    # differ in what the prompt SAYS the model has. Lower is better: a call
    # to a denied tool is a turn the model spent on a route it was told it
    # had and does not.
    row("denied_tool_attempts", spread(ck, 25), spread(tk, 25), dir3(mean(ck, 25), mean(tk, 25), 1, 0.5, noisefloor(ck, 25)))
    # MECHANISM, not an outcome — more compactions is neither better nor worse.
    # What matters is that it is NON-ZERO on a scenario built to force one.
    row("compactions", spread(ck, 26), spread(tk, 26), "mechanism")
    row("capability_tier", tierdist(ck), tierdist(tk), "observed")
    row("tally_found", sprintf("%d/%d", tallyfound[ck], n[ck]), sprintf("%d/%d", tallyfound[tk], n[tk]), "must be full")
    row("session_found", sessdist(ck), sessdist(tk), "must be full")

    # RUN-TO-RUN SPREAD. Read this before accepting a page of ~noise: a change
    # that removes the bad runs shows up here and nowhere else above.
    printf "\n%-26s %-26s %-26s %s\n", "dispersion (max-min)", "control", "treatment", "delta"
    ndisp = split("4 5 6 9 21", dispcols, " ")
    split("turns tool_calls errored_tool_calls max_failure_streak input_tokens", dispnames, " ")
    steadier = 0; noisier = 0
    for (di = 1; di <= ndisp; di++) {
      dc = dispcols[di]
      dv = dispdir(ck, tk, dc)
      if (dv == "steadier") steadier++
      else if (dv == "noisier") noisier++
      printf "%-26s %-26s %-26s %s\n", dispnames[di], dispfmt(ck, dc), dispfmt(tk, dc), dv
    }
    if (steadier > 0 || noisier > 0) {
      printf "  treatment is steadier on %d metric(s), noisier on %d — spread is a WEAKER signal than the means above, so read it as a hint to raise n, not as a result.\n", steadier, noisier
    }

    # dirge-1elu.6: co-occurrence per arm — which gates and nudges fired
    # together at one decision point, with counts. An arm whose boundaries
    # field is `none` in every run is reported as such (mechanism: nothing
    # fired — same discipline as the nudge sums).
    for (ai = 1; ai <= na; ai++) {
      ak = alist[ai]
      kk = ak SUBSEP m
      if (evn[kk] > 0) {
        desc = ""
        for (ei = 1; ei <= evn[kk]; ei++) {
          shp = evlist[kk, ei]
          desc = desc (desc == "" ? "" : ", ") shp " x" evcnt[kk, shp]
        }
        printf "  co-occurrence %s: %s\n", ak, desc
      } else {
        printf "  co-occurrence %s: none (no boundary events in any run)\n", ak
      }
    }

    # dirge-1elu.6: N-arm mode — every arm beyond control/treatment gets the
    # same comparison against the control arm. Uses row2() so the two-arm
    # consistency summary below is not polluted.
    for (ai = 1; ai <= na; ai++) {
      ak = alist[ai]
      if (ak == "control" || ak == "treatment") continue
      ek = ak SUBSEP m
      # An arm can be absent for THIS model — a launch that failed, or an
      # N-arm matrix that is not fully crossed. Say so and move on: every
      # rate below divides by n[ek], so proceeding would divide by zero and
      # abort the whole report. Reporting the absence rather than skipping
      # silently is the same discipline as `tally=missing` — a gap in the
      # data is a fact about the run, not a zero.
      if (!(ek in n) || n[ek] == 0) {
        printf "== model: %s — arm: %s ==\n  no runs for this model — arm not comparable\n", m, ak
        continue
      }
      printf "== model: %s — arm: %s ==\n", m, ak
      printf "%-26s %-26s %-26s %s\n", "metric", "control", ak, "delta"
      row2("turns", spread(ck, 4), spread(ek, 4), dir3(mean(ck, 4), mean(ek, 4), 1, 0.5, noisefloor(ck, 4)))
      row2("tool_calls", spread(ck, 5), spread(ek, 5), dir3(mean(ck, 5), mean(ek, 5), 1, 0.5, noisefloor(ck, 5)))
      row2("errored_tool_calls", spread(ck, 6), spread(ek, 6), dir3(mean(ck, 6), mean(ek, 6), 1, 0.5, noisefloor(ck, 6)))
      row2("  of which missing_info", spread(ck, 27), spread(ek, 27), "mechanism")
      row2("unresolved_effects", spread(ck, 28), spread(ek, 28), "mechanism")
      row2("scavenged_calls", spread(ck, 7), spread(ek, 7), dir3(mean(ck, 7), mean(ek, 7), 1, 0.5, noisefloor(ck, 7)))
      row2("storm_suppressions", spread(ck, 8), spread(ek, 8), dir3(mean(ck, 8), mean(ek, 8), 1, 0.5, noisefloor(ck, 8)))
      row2("max_failure_streak", spread(ck, 9), spread(ek, 9), dir3(mean(ck, 9), mean(ek, 9), 1, 0.5, noisefloor(ck, 9)))
      row2("repair_invalid", spread(ck, 10), spread(ek, 10), dir3(mean(ck, 10), mean(ek, 10), 1, 0.5, noisefloor(ck, 10)))
      row2("repair_total_successful", spread(ck, 11), spread(ek, 11), sprintf("%+.1f", mean(ek, 11) - mean(ck, 11)))
      eval2 = (fwn[ek] ? sprintf("%.1f (%.0f..%.0f) [never=%d]", fws[ek] / fwn[ek], fwmin[ek], fwmax[ek], never[ek]) : "- [never=" never[ek] "]")
      row2("first_write", cval, eval2, "n/a")
      row2("nudges_fired", spread(ck, 17), spread(ek, 17), "mechanism")
      row2("  of which prologue", spread(ck, 16), spread(ek, 16), "mechanism")
      row2("gates_fired", spread(ck, 20), spread(ek, 20), "mechanism")
      row2("input_tokens", spread(ck, 21), spread(ek, 21), dir3(mean(ck, 21), mean(ek, 21), 1, 0.5, noisefloor(ck, 21)))
      row2("cached_tokens", spread(ck, 22), spread(ek, 22), dir3(mean(ck, 22), mean(ek, 22), 0, 0.5, noisefloor(ck, 22)))
      row2("input_tokens_per_turn", tokperturn(ck), tokperturn(ek),
          dir3(tokperturn_num(ck), tokperturn_num(ek), 1, 0.5, tokperturn_floor(ck)))
      # dirge-l8l7.4: the guard above covers the EXTRA arm; control can be
      # absent for this model just as easily, and these divide by it too.
      row2("success_rate", rate(ck), rate(ek), pairdir(ok[ck], narm(ck), ok[ek], narm(ek), 0, 0.05, ratefloor(ck, ek)))
      row2("green_rate", sprintf("%d/%d (%.0f%%)", green[ck], narm(ck), 100 * prop(green[ck], narm(ck))),
          sprintf("%d/%d (%.0f%%)", green[ek], narm(ek), 100 * prop(green[ek], narm(ek))),
          pairdir(green[ck], narm(ck), green[ek], narm(ek), 0, 0.05, ratefloor(ck, ek)))
      row2("tally_found", sprintf("%d/%d", tallyfound[ck], n[ck]), sprintf("%d/%d", tallyfound[ek], n[ek]), "must be full")
      row2("session_found", sessdist(ck), sessdist(ek), "must be full")
      # The extra arm needs the spread rows too, and needs them MORE than the
      # two-arm block does: extra arms are how a CUMULATIVE configuration gets
      # measured, and the whole reason to run one is the expectation that
      # stacked changes reinforce. Reporting the cumulative means while hiding
      # whether the cumulative arm is steadier would leave the interesting
      # half of that question unanswerable.
      printf "\n%-26s %-26s %-26s %s\n", "dispersion (max-min)", "control", ak, "delta"
      nd2 = split("4 5 6 9 21", dcol2, " ")
      split("turns tool_calls errored_tool_calls max_failure_streak input_tokens", dnam2, " ")
      st2 = 0; no2 = 0
      for (dj = 1; dj <= nd2; dj++) {
        d2 = dcol2[dj]
        v2 = dispdir(ck, ek, d2)
        if (v2 == "steadier") st2++
        else if (v2 == "noisier") no2++
        printf "%-26s %-26s %-26s %s\n", dnam2[dj], dispfmt(ck, d2), dispfmt(ek, d2), v2
      }
      if (st2 > 0 || no2 > 0) {
        printf "  %s is steadier on %d metric(s), noisier on %d.\n", ak, st2, no2
      }
      printf "\n"
    }
    printf "\n"
  }

  printf "== summary across models ==\n"
  if (nm == 1) {
    printf "  single model tested — the per-model deltas above are NOT evidence for a steering change.\n"
    printf "  Re-run with -m \"model1,model2,...\" before treating any direction as real.\n"
  } else {
    # dirge-l8l7.6: iterate to what split() RETURNED, not a hardcoded 10.
    # Adding a metric name to the string above used to drop it from the
    # summary silently — the same hand-maintained-count class as the rest of
    # this epic.
    nnames = split("turns tool_calls errored_tool_calls scavenged_calls storm_suppressions max_failure_streak repair_invalid first_write success_rate green_rate", names, " ")
    for (i = 1; i <= nnames; i++) {
      name = names[i]
      b = bm[name] + 0; w = wm[name] + 0; f = fm[name] + 0
      if (b + w + f == 0) { printf "  %-26s no usable comparison\n", name; continue }
      # dirge-l8l7.6: there was no `f == nm` arm, so a metric that came out
      # FLAT in every model was labelled MIXED — the label for a
      # disagreement. An A/A calibration, whose whole purpose is to produce
      # unanimous no-effect, therefore read as an inconsistent result.
      held = (b == nm) ? "better in every model" \
           : ((w == nm) ? "worse in every model" \
           : ((f == nm) ? "flat in every model" : "MIXED"))
      printf "  %-26s better %d, worse %d, flat %d of %d models — %s\n", name, b, w, f, nm, held
    }
  }
  total_missing = 0
  for (k in missing) total_missing += missing[k]
  if (total_missing > 0) {
    printf "  WARNING: %d run(s) produced no dirge::gates line (missing tally is a harness bug — check DIRGE_LOG/RUST_LOG).\n", total_missing
  }
  if (short_rows > 0) {
    printf "  NOTE: %d row(s) predate the gates_fired column (fewer than 20 fields); gates_fired reads 0 for those by default, which is an absence, not a measurement.\n", short_rows
  }
  if (pretoken_rows > 0) {
    printf "  NOTE: %d row(s) predate the token columns (fewer than 24 fields); input_tokens/cached_tokens read 0 for those, which is an absence, not a measurement.\n", pretoken_rows
  }
  if (malformed_rows > 0) {
    printf "  WARNING: %d row(s) had too few fields to be a run record and were skipped — check %s for truncated writes.\n", malformed_rows, FILENAME
  }

  # dirge-1elu.6: N-arm consistency across models (only meaningful with
  # several models and at least one extra arm). bm2/wm2/fm2 were filled by
  # the extra-arm blocks above.
  #
  # dirge-l8l7.2: this block used to sit INSIDE the `total_missing > 0`
  # branch above — a brace mis-nesting, no other cause. So the N-arm
  # cross-model summary, which is the entire deliverable of the N-arm mode,
  # printed only when at least one run had FAILED to produce a gates line,
  # and was structurally unreachable on a healthy run. Measured: an
  # all-tallies-found fixture emitted nothing; flipping one tally to 0 made
  # it appear. The selftest missed it because its fixture carries a missing
  # tally on purpose (to assert `tally_found 0/1`), which parked it on the
  # broken branch.
  if (na > 2 && nm > 1) {
    printf "== summary across models, extra arms ==\n"
    for (ai = 1; ai <= na; ai++) {
      ak = alist[ai]
      if (ak == "control" || ak == "treatment") continue
      printf "  arm %s (vs control)\n", ak
      for (ni2 in names2) {
        b = bm2[ni2]; w = wm2[ni2]; f = fm2[ni2]
        if (b + w + f == 0) continue
        printf "    %-26s better %d, worse %d, flat %d of %d models\n", ni2, b, w, f, nm
      }
    }
  }
}
' "$WORK/results.tsv"

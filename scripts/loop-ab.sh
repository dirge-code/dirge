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
#   -s  scenario small|recon       (default small)
#
# With -A and -B both empty the two arms are identical — a useful check
# that the harness itself adds no bias.
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

BASE_CONFIG="${HOME}/.config/dirge/config.json"

while getopts "n:m:A:B:b:t:s:" opt; do
  case "$opt" in
    n) REPEATS="$OPTARG" ;;
    m) MODELS="$OPTARG" ;;
    A) ARM_A="$OPTARG" ;;
    B) ARM_B="$OPTARG" ;;
    b) BINARY="$OPTARG" ;;
    t) MAXTURNS="$OPTARG" ;;
    s) SCENARIO="$OPTARG" ;;
    *) echo "usage: $0 [-n REPEATS] [-m MODELS] [-A OVERRIDES] [-B OVERRIDES] [-b BINARY] [-t MAXTURNS] [-s small]" >&2; exit 2 ;;
  esac
done

case "$SCENARIO" in small|recon) ;; *) echo "error: -s must be small or recon" >&2; exit 2 ;; esac
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
else
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
    grep -qE "from settings import RETRY_LIMIT|from src.settings import RETRY_LIMIT|import settings" "$f" || { echo 0; return; }
    # Behavioural check: exercise the function for real.
    ( cd "$FIXTURE/src" && python3 -c "
import limits, settings
assert limits.effective_retry_limit(0) == settings.RETRY_LIMIT
assert limits.effective_retry_limit(-5) == settings.RETRY_LIMIT
assert limits.effective_retry_limit(1) == settings.RETRY_LIMIT - 1
assert limits.effective_retry_limit(999) == 0
" ) >/dev/null 2>&1 && echo 1 || echo 0
  }
fi

# ---- Undo whatever a run wrote, so each repeat starts from the same tree.
# Read-only scenarios need nothing; recon must drop the file under test.
reset_fixture() {
  if [ "$SCENARIO" = "recon" ]; then
    rm -f "$FIXTURE/src/limits.py"
    rm -rf "$FIXTURE/src/__pycache__"
  fi
}

# ---- Correctness check for one run's stream-json output; echoes 1 or 0.
# The recon scenario overrides this above (it checks the file on disk).
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
  local prog extra=""
  prog="$(override_program "$2")"
  extra=".max_agent_turns = ${MAXTURNS}"
  if [ "$3" != "default" ]; then
    extra="${extra} | .provider = $(jq -n --arg v "$3" '$v')"
  fi
  if [ -z "$prog" ]; then
    jq "$extra" "$BASE_CONFIG" > "$1/config.json"
  else
    jq "${prog} | ${extra}" "$BASE_CONFIG" > "$1/config.json"
  fi
}

# ---- Run one arm of one model: N repeats. Appends one line per repeat to
# $WORK/results.tsv as:
# tag  model  repeat  turns  tool_calls  errored  scavenged  storm
# maxstreak  repair_invalid  repair_total  verification  first_write  correct  tally
run_arm() { # $1 = overrides, $2 = tag, $3 = model
  local i cfgdir datadir out err logfile ok tally_str
  local gates_line tally_found turns tool_calls_f errored scavenged storm maxstreak rep_invalid rep_total verification fw
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
        DIRGE_LOG="$logfile" RUST_LOG="dirge::gates=info" \
        "$OLDPWD_BINARY" -p --yolo --output-format stream-json "$TASK" ) \
        >"$out" 2>"$err" || true

    # Gate tally: parse the single dirge::gates line. Missing line is
    # recorded as such (tally=missing), never as zeros.
    gates_line="$(grep -a 'dirge::gates:' "$logfile" 2>/dev/null | tail -1 || true)"
    if [ -n "$gates_line" ]; then
      tally_found=1
      turns="$(get_field turns "$gates_line")"
      tool_calls_f="$(get_field tool_calls "$gates_line")"
      errored="$(get_field errored_tool_calls "$gates_line")"
      scavenged="$(get_field scavenged_calls "$gates_line")"
      storm="$(get_field storm_suppressions "$gates_line")"
      maxstreak="$(get_field max_failure_streak "$gates_line")"
      rep_invalid="$(get_field repair_invalid "$gates_line")"
      rep_total="$(get_field repair_total_successful "$gates_line")"
      verification="$(get_field final_verification "$gates_line")"
    else
      tally_found=0
      turns=0; tool_calls_f=0; errored=0; scavenged=0; storm=0
      maxstreak=0; rep_invalid=0; rep_total=0; verification="-"
    fi

    fw="$(first_write "$out")"
    ok="$(check_correct "$out")"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$2" "$3" "$i" "$turns" "$tool_calls_f" "$errored" "$scavenged" "$storm" \
      "$maxstreak" "$rep_invalid" "$rep_total" "$verification" "$fw" "$ok" "$tally_found" \
      >> "$WORK/results.tsv"

    if [ "$tally_found" = 1 ]; then tally_str=found; else tally_str=missing; fi
    printf '  [%s %s %s/%s] turns=%s tools=%s err=%s scav=%s storm=%s streak=%s rep_inv=%s rep_ok=%s verify=%s first_write=%s correct=%s tally=%s\n' \
      "$2" "$3" "$i" "$REPEATS" "$turns" "$tool_calls_f" "$errored" "$scavenged" "$storm" \
      "$maxstreak" "$rep_invalid" "$rep_total" "$verification" "$fw" "$ok" "$tally_str"
    if [ "$tally_found" = 0 ]; then
      printf '    ^ no dirge::gates line in %s (harness bug, not a zero tally)\n' "$logfile"
    fi
  done
}

# Resolve the binary to an absolute path before we cd into the fixture.
OLDPWD_BINARY="$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")"

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
  echo
done

# ---- Report: per model, per arm means + spread; per-model deltas; then a
# consistency summary across models. Directions: lower is better for the
# cost/instability metrics, higher is better for success/green. Repair
# total is reported without a directional claim (more repairs is not
# clearly good or bad). A missing tally anywhere is surfaced, never hidden.
awk -F'\t' '
{
  key = $1 SUBSEP $2
  if (!(key in seen)) {
    seen[key] = 1
    if (!(("m" $2) in mseen)) { mseen["m" $2] = 1; mlist[++nm] = $2 }
  }
  n[key]++
  for (c = 4; c <= 11; c++) {
    sum[key,c] += $c
    if (n[key] == 1 || $c < mn[key,c]) mn[key,c] = $c
    if (n[key] == 1 || $c > mx[key,c]) mx[key,c] = $c
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
  if ($15 == 0) missing[key]++
  green[key] += ($12 == "VerifiedGreen")
}
function mean(key, c,    k) { k = key SUBSEP c; return n[key] ? sum[k] / n[key] : 0 }
function spread(key, c,    k) {
  k = key SUBSEP c
  return sprintf("%.1f (%s..%s)", mean(key, c), n[key] ? mn[k] : 0, n[key] ? mx[k] : 0)
}
function dir3(c, t, lower_is_better, eps,    d) {
  d = t - c
  if (d < -eps) return (lower_is_better ? "better" : "worse")
  if (d > eps)  return (lower_is_better ? "worse"  : "better")
  return "flat"
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
END {
  for (mi = 1; mi <= nm; mi++) {
    m = mlist[mi]
    ck = "control" SUBSEP m
    tk = "treatment" SUBSEP m
    printf "== model: %s ==\n", m
    printf "%-26s %-26s %-26s %s\n", "metric", "control", "treatment", "delta"

    row("turns", spread(ck, 4), spread(tk, 4), dir3(mean(ck, 4), mean(tk, 4), 1, 0.5))
    row("tool_calls", spread(ck, 5), spread(tk, 5), dir3(mean(ck, 5), mean(tk, 5), 1, 0.5))
    row("errored_tool_calls", spread(ck, 6), spread(tk, 6), dir3(mean(ck, 6), mean(tk, 6), 1, 0.5))
    row("scavenged_calls", spread(ck, 7), spread(tk, 7), dir3(mean(ck, 7), mean(tk, 7), 1, 0.5))
    row("storm_suppressions", spread(ck, 8), spread(tk, 8), dir3(mean(ck, 8), mean(tk, 8), 1, 0.5))
    row("max_failure_streak", spread(ck, 9), spread(tk, 9), dir3(mean(ck, 9), mean(tk, 9), 1, 0.5))
    row("repair_invalid", spread(ck, 10), spread(tk, 10), dir3(mean(ck, 10), mean(tk, 10), 1, 0.5))
    row("repair_total_successful", spread(ck, 11), spread(tk, 11), sprintf("%+.1f", mean(tk, 11) - mean(ck, 11)))

    cval = (fwn[ck] ? sprintf("%.1f (%.0f..%.0f) [never=%d]", fws[ck] / fwn[ck], fwmin[ck], fwmax[ck], never[ck]) : "- [never=" never[ck] "]")
    tval = (fwn[tk] ? sprintf("%.1f (%.0f..%.0f) [never=%d]", fws[tk] / fwn[tk], fwmin[tk], fwmax[tk], never[tk]) : "- [never=" never[tk] "]")
    dval = (fwn[ck] && fwn[tk]) ? dir3(fws[ck] / fwn[ck], fws[tk] / fwn[tk], 1, 0.5) : "n/a"
    row("first_write", cval, tval, dval)

    row("success_rate", rate(ck), rate(tk), dir3(ok[ck] / n[ck], ok[tk] / n[tk], 0, 0.05))
    row("green_rate", sprintf("%d/%d (%.0f%%)", green[ck], n[ck], 100 * green[ck] / n[ck]),
        sprintf("%d/%d (%.0f%%)", green[tk], n[tk], 100 * green[tk] / n[tk]),
        dir3(green[ck] / n[ck], green[tk] / n[tk], 0, 0.05))
    row("tally_found", sprintf("%d/%d", tallyfound[ck], n[ck]), sprintf("%d/%d", tallyfound[tk], n[tk]), "must be full")
    printf "\n"
  }

  printf "== summary across models ==\n"
  if (nm == 1) {
    printf "  single model tested — the per-model deltas above are NOT evidence for a steering change.\n"
    printf "  Re-run with -m \"model1,model2,...\" before treating any direction as real.\n"
  } else {
    split("turns tool_calls errored_tool_calls scavenged_calls storm_suppressions max_failure_streak repair_invalid first_write success_rate green_rate", names, " ")
    for (i = 1; i <= 10; i++) {
      name = names[i]
      b = bm[name] + 0; w = wm[name] + 0; f = fm[name] + 0
      if (b + w + f == 0) { printf "  %-26s no usable comparison\n", name; continue }
      held = (b == nm) ? "better in every model" : ((w == nm) ? "worse in every model" : "MIXED")
      printf "  %-26s better %d, worse %d, flat %d of %d models — %s\n", name, b, w, f, nm, held
    }
  }
  total_missing = 0
  for (k in missing) total_missing += missing[k]
  if (total_missing > 0) {
    printf "  WARNING: %d run(s) produced no dirge::gates line (missing tally is a harness bug — check DIRGE_LOG/RUST_LOG).\n", total_missing
  }
}
' "$WORK/results.tsv"

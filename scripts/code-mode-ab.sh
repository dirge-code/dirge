#!/usr/bin/env bash
set -euo pipefail

# A/B harness for the `code_mode_rubric` config flag.
#
# Runs one fixed bulk task headless, N times with the rubric OFF (control)
# and N times with it ON (treatment), then reports mean input tokens and
# mean tool-call count per arm. The rubric nudges the model to collapse a
# fan-out of per-item tool calls into ONE bash script whose raw output
# stays out of context; this measures whether that actually lowers the
# input tokens the model pays for.
#
# Isolation: each run gets its own DIRGE_CONFIG_DIR and DIRGE_DATA_DIR, so
# your real ~/.config/dirge and session history are never touched. The
# temp config is a copy of your real global config with two keys
# overridden (`code_mode_rubric` and, optionally, `provider`) plus a low
# `max_agent_turns` cap so a runaway loop can't burn budget.
#
# Three scenarios (-s):
#   small   30 .log files, count how many contain FATAL (one grep).      [default]
#   large   120 .log files, aggregate ERROR codes and report the top one
#           + how many files have errors (one grep|sort|uniq pipeline).
#   fanout  120 .log files, each with a STATUS line and a REGION line on
#           DIFFERENT lines. Report files that are both STATUS=down and
#           REGION=eu. A single-line grep can't AND across two lines, so
#           the naive path opens each candidate file (real fan-out, file
#           bodies into context) while the code-mode path intersects two
#           `grep -l` results in one script. This is the case where a
#           model would actually fan out even though it defaults to grep.
#
# Ground truth is computed from the generated fixture, not hardcoded, so
# the correctness gate always matches what was actually planted.
#
# Usage:
#   scripts/code-mode-ab.sh [-n REPEATS] [-p PROVIDER] [-b BINARY] [-t MAXTURNS] [-s SCENARIO]
#
#   -n  repeats per arm            (default 3)
#   -p  override top-level provider(default: keep config's own)
#   -b  dirge binary               (default target/debug/dirge)
#   -t  max_agent_turns cap        (default 20)
#   -s  scenario small|large|fanout(default small)
#
# Requires: a built dirge binary, jq, and a working provider (with
# credentials) in ~/.config/dirge/config.json.

REPEATS=3
PROVIDER=""
BINARY="target/debug/dirge"
MAXTURNS=20
SCENARIO="small"
BASE_CONFIG="${HOME}/.config/dirge/config.json"

while getopts "n:p:b:t:s:" opt; do
  case "$opt" in
    n) REPEATS="$OPTARG" ;;
    p) PROVIDER="$OPTARG" ;;
    b) BINARY="$OPTARG" ;;
    t) MAXTURNS="$OPTARG" ;;
    s) SCENARIO="$OPTARG" ;;
    *) echo "usage: $0 [-n REPEATS] [-p PROVIDER] [-b BINARY] [-t MAXTURNS] [-s small|large|fanout]" >&2; exit 2 ;;
  esac
done

case "$SCENARIO" in small|large|fanout) ;; *) echo "error: -s must be small, large, or fanout" >&2; exit 2 ;; esac
command -v jq >/dev/null || { echo "error: jq required" >&2; exit 1; }
[ -x "$BINARY" ] || { echo "error: dirge binary not found/executable: $BINARY" >&2; exit 1; }
[ -f "$BASE_CONFIG" ] || { echo "error: base config not found: $BASE_CONFIG" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/code-mode-ab.XXXXXX")"
FIXTURE="$WORK/fixture"
mkdir -p "$FIXTURE"
trap 'rm -rf "$WORK"' EXIT

# ---- Build the fixture and set TASK + ground-truth expectations.
if [ "$SCENARIO" = "small" ]; then
  # 30 .log files; those whose index divides by 5 or 7 get a FATAL line.
  for i in $(seq 1 30); do
    {
      for l in $(seq 1 60); do echo "svc$i line $l: routine heartbeat ok status=200 latency=12ms"; done
      if (( i % 5 == 0 || i % 7 == 0 )); then echo "FATAL: service $i crashed, unrecoverable"; fi
    } > "$FIXTURE/service-$i.log"
  done
  EXPECTED_COUNT="$(grep -lE 'FATAL' "$FIXTURE"/*.log | wc -l | tr -d ' ')"
  FIXTURE_DESC="30 .log files, $EXPECTED_COUNT contain FATAL"
  TASK='This directory has many .log files. Report exactly how many of them contain the string FATAL, and list those filenames sorted. End your answer with a line: COUNT=<n>'
elif [ "$SCENARIO" = "large" ]; then
  # 120 .log files. Errors are logged as `ERROR code=E<digits>`. Different
  # codes are planted at different frequencies so the top code is unambiguous:
  #   E500 on every 3rd file, E404 on every 4th, E503 on every 10th.
  for i in $(seq 1 120); do
    {
      for l in $(seq 1 150); do echo "svc$i line $l: routine heartbeat ok status=200 latency=12ms"; done
      if (( i % 3 == 0 )); then echo "ERROR code=E500 service $i internal failure"; fi
      if (( i % 4 == 0 )); then echo "ERROR code=E404 service $i resource missing"; fi
      if (( i % 10 == 0 )); then echo "ERROR code=E503 service $i unavailable"; fi
    } > "$FIXTURE/service-$i.log"
  done
  EXPECTED_FILES="$(grep -lE 'ERROR code=E[0-9]+' "$FIXTURE"/*.log | wc -l | tr -d ' ')"
  EXPECTED_TOP="$(grep -ohE 'E[0-9]+' "$FIXTURE"/*.log | sort | uniq -c | sort -rn | head -1 | grep -oE 'E[0-9]+')"
  FIXTURE_DESC="120 .log files; $EXPECTED_FILES contain an ERROR, top code $EXPECTED_TOP"
  TASK='This directory has many .log files. Errors are logged as lines of the form `ERROR code=E<digits>`. Find the single most frequently occurring error code across all files, and how many files contain at least one ERROR line. End your answer with exactly two lines: TOP_CODE=<code> and FILES_WITH_ERRORS=<n>'
else
  # 120 .log files. STATUS and REGION are on DIFFERENT lines within each file,
  # so no single-line grep can AND them. STATUS=down on every 3rd file (~40),
  # REGION=eu on every 4th; the intersection (every 12th) is the answer. A
  # naive model greps STATUS=down (~40 candidates) then must inspect each to
  # check REGION -> real per-file fan-out; the code-mode path intersects two
  # `grep -l` lists in one script with no file bodies entering context.
  for i in $(seq 1 120); do
    case $(( i % 3 )) in 0) st=down ;; 1) st=degraded ;; *) st=ok ;; esac
    case $(( i % 4 )) in 0) rg=eu ;; 2) rg=ap ;; *) rg=us ;; esac
    {
      echo "STATUS=$st"
      for l in $(seq 1 80); do echo "svc$i line $l: routine heartbeat ok latency=12ms"; done
      echo "REGION=$rg"
      for l in $(seq 81 160); do echo "svc$i line $l: routine heartbeat ok latency=12ms"; done
    } > "$FIXTURE/service-$i.log"
  done
  EXPECTED_COUNT="$( ( cd "$FIXTURE" && grep -l 'STATUS=down' ./*.log 2>/dev/null | xargs grep -l 'REGION=eu' 2>/dev/null ) | wc -l | tr -d ' ')"
  FIXTURE_DESC="120 .log files; $EXPECTED_COUNT are both STATUS=down and REGION=eu (markers on different lines)"
  TASK='This directory has many .log files. Each file has a STATUS=<...> line and, on a different line, a REGION=<...> line. Report how many files have BOTH STATUS=down and REGION=eu, and list those filenames sorted. End your answer with a line: COUNT=<n>'
fi

# ---- Correctness check for one run's stream-json output; echoes 1 or 0.
check_correct() {
  local out="$1" result
  result="$(jq -r 'select(.type=="result") | .result' "$out" 2>/dev/null || true)"
  if [ "$SCENARIO" != "large" ]; then
    # small + fanout both gate on a single COUNT= line
    local got
    got="$(printf '%s' "$result" | grep -oE 'COUNT=[0-9]+' | tail -1 || true)"
    [ "$got" = "COUNT=$EXPECTED_COUNT" ] && echo 1 || echo 0
  else
    local got_top got_files
    got_top="$(printf '%s' "$result" | grep -oE 'TOP_CODE=E[0-9]+' | tail -1 || true)"
    got_files="$(printf '%s' "$result" | grep -oE 'FILES_WITH_ERRORS=[0-9]+' | tail -1 || true)"
    if [ "$got_top" = "TOP_CODE=$EXPECTED_TOP" ] && [ "$got_files" = "FILES_WITH_ERRORS=$EXPECTED_FILES" ]; then
      echo 1
    else
      echo 0
    fi
  fi
}

# ---- Run one arm: rubric = 0|1, tag = label. Appends one line per repeat
# to $WORK/results.tsv as: tag  repeat  input_tokens  cached  toolcalls  count_correct
run_arm() {
  local rubric="$1" tag="$2" i cfgdir datadir out err sess tokens cached toolcalls ok
  for i in $(seq 1 "$REPEATS"); do
    cfgdir="$(mktemp -d "$WORK/cfg.XXXXXX")"
    datadir="$(mktemp -d "$WORK/data.XXXXXX")"
    # temp global config = base + overrides
    jq \
      --argjson rubric "$rubric" \
      --argjson turns "$MAXTURNS" \
      --arg prov "$PROVIDER" \
      '.code_mode_rubric = ($rubric == 1)
       | .max_agent_turns = $turns
       | (if $prov == "" then . else .provider = $prov end)' \
      "$BASE_CONFIG" > "$cfgdir/config.json"

    out="$WORK/${tag}-${i}.jsonl"
    err="$WORK/${tag}-${i}.err"
    ( cd "$FIXTURE" && DIRGE_CONFIG_DIR="$cfgdir" DIRGE_DATA_DIR="$datadir" \
        "$OLDPWD_BINARY" -p --yolo --output-format stream-json "$TASK" ) \
        >"$out" 2>"$err" || true

    sess="$(ls "$datadir"/sessions/*.json 2>/dev/null | head -1 || true)"
    if [ -n "$sess" ]; then
      tokens="$(jq -r '.cumulative_input_tokens // 0' "$sess" 2>/dev/null || echo 0)"
      cached="$(jq -r '.cumulative_cached_input_tokens // 0' "$sess" 2>/dev/null || echo 0)"
    else
      tokens=0; cached=0
    fi
    # tool calls = tool_use blocks across all assistant events (JSONL, one per line)
    toolcalls="$(jq -c 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use")' "$out" 2>/dev/null | wc -l | tr -d ' ')"
    ok="$(check_correct "$out")"

    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$tag" "$i" "$tokens" "$cached" "$toolcalls" "$ok" >> "$WORK/results.tsv"
    printf '  [%s %s/%s] input=%s cached=%s tool_calls=%s correct=%s\n' \
      "$tag" "$i" "$REPEATS" "$tokens" "$cached" "$toolcalls" "$ok"
  done
}

# Resolve the binary to an absolute path before we cd into the fixture.
OLDPWD_BINARY="$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")"

echo "== code-mode rubric A/B =="
echo "binary=$OLDPWD_BINARY provider=${PROVIDER:-<config default>} repeats=$REPEATS max_turns=$MAXTURNS scenario=$SCENARIO"
echo "fixture: $FIXTURE_DESC"
echo

echo "control (rubric OFF):"
run_arm 0 control
echo "treatment (rubric ON):"
run_arm 1 treatment
echo

# ---- Report means + delta
awk -F'\t' '
  { n[$1]++; tok[$1]+=$3; cac[$1]+=$4; tc[$1]+=$5; ok[$1]+=$6 }
  END {
    for (a in n) {
      mt[a]=tok[a]/n[a]; mc[a]=tc[a]/n[a]; mca[a]=cac[a]/n[a]; ma[a]=ok[a]"/"n[a]
    }
    printf "%-11s  %12s  %12s  %11s  %8s\n", "arm", "mean_input", "mean_cached", "mean_calls", "correct"
    printf "%-11s  %12.0f  %12.0f  %11.1f  %8s\n", "control",   mt["control"],   mca["control"],   mc["control"],   ma["control"]
    printf "%-11s  %12.0f  %12.0f  %11.1f  %8s\n", "treatment", mt["treatment"], mca["treatment"], mc["treatment"], ma["treatment"]
    if (mt["control"] > 0) {
      dt = (mt["control"]-mt["treatment"])/mt["control"]*100
      dc = mc["control"]-mc["treatment"]
      printf "\ninput-token reduction: %.1f%%   tool-call reduction: %.1f\n", dt, dc
    }
  }
' "$WORK/results.tsv"

#!/usr/bin/env bash
set -euo pipefail

# loop-ab-selftest.sh — exercise loop-ab.sh's reporting awk against a synthetic
# results.tsv, with no models, no network, and no builds.
#
# Why this exists: the reporting layer is awk embedded in bash, and three real
# bugs shipped in it at once (dirge-1elu.6) — the arm list collected the model
# column instead of the tag column, the co-occurrence lookup key had its two
# components reversed against the accumulation key, and the boundaries value was
# scraped from a TSV column that `run_arm` never wrote. None of them could fail a
# Rust test, and all three degrade SILENTLY: the report still prints, it just
# says "none" forever. That is the failure mode docs/verification-discipline.md
# calls a gate that cannot fail.
#
# The fixture below is built so each assertion has a known-other-answer:
#   control    — no boundary events at all
#   treatment  — two gates co-firing at ONE boundary   -> `Verifier+Critic`
#   allgates   — the same two gates at SEPARATE boundaries -> `Critic, Verifier`
# The treatment/allgates pair is the point. If the report cannot tell those two
# apart, co-occurrence is not being measured, and neither row means anything
# alone.
#
# Usage: scripts/loop-ab-selftest.sh   (exit 0 = pass)

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ab="$here/loop-ab.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Extract the reporting awk program from loop-ab.sh so we test the REAL one
# rather than a copy that can drift out of sync with it.
awk '/^awk -F/{flag=1; next} /^'"'"' "\$WORK\/results.tsv"/{flag=0} flag' "$ab" > "$work/report.awk"
if [ ! -s "$work/report.awk" ]; then
  echo "FAIL: could not extract the reporting awk program from $ab" >&2
  exit 1
fi

# tag model repeat turns tools err scav storm streak rep_inv rep_tot verify
# first_write correct tally prologue nudges tier boundaries
row() { printf '%s\t%s\t%s\t10\t20\t0\t0\t0\t0\t0\t0\t%s\t3\t%s\t%s\t0\t2\tnominal\t%s\n' "$@"; }
{
  row control   m1 1 VerifiedGreen 1 1 none
  row control   m1 2 VerifiedGreen 1 1 none
  row treatment m1 1 VerifiedGreen 1 1 'Verifier+Critic'
  row treatment m1 2 VerifiedGreen 1 1 'Verifier+Critic'
  row allgates  m1 1 VerifiedGreen 1 1 'Verifier;Critic'
  row allgates  m1 2 VerifiedGreen 1 1 'Verifier;Critic'
  row control   m3 1 -             0 0 none
  row treatment m3 1 -             0 0 none
} > "$work/results.tsv"

out="$(awk -F'\t' -f "$work/report.awk" "$work/results.tsv" 2>&1)"

fails=0
want() { # $1 = description, $2 = grep -E pattern
  if printf '%s\n' "$out" | grep -qE "$2"; then
    echo "  ok   $1"
  else
    echo "  FAIL $1 (no line matching: $2)"
    fails=$((fails + 1))
  fi
}

echo "loop-ab.sh reporting self-test:"
# Arms are named by TAG, not by model. The bug printed `co-occurrence m1`.
want "arms are named by tag, not model"        '^  co-occurrence (control|treatment|allgates):'
if printf '%s\n' "$out" | grep -qE '^  co-occurrence m[0-9]+:'; then
  echo "  FAIL an arm was named after a model (tag/model column mix-up)"
  fails=$((fails + 1))
else
  echo "  ok   no arm named after a model"
fi
# The discriminating pair. Neither assertion means anything without the other.
want "co-firing at one boundary reads as one event" '^  co-occurrence treatment: Verifier\+Critic x2$'
want "the same gates at separate boundaries differ" '^  co-occurrence allgates: Critic x2, Verifier x2$'
want "an arm with no events says so"                '^  co-occurrence control: none'
# N-arm mode picks up the third arm and compares it against control.
want "N-arm mode compares the extra arm"            '^== model: m1 — arm: allgates ==$'
# A missing tally is surfaced, never silently zeroed.
want "a missing tally is reported, not zeroed"      '^tally_found +0/1'

if [ "$fails" -ne 0 ]; then
  echo
  echo "$fails check(s) failed. Full report:"
  printf '%s\n' "$out"
  exit 1
fi
echo "all checks passed"

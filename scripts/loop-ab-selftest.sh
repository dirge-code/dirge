#!/usr/bin/env bash
set -euo pipefail

# loop-ab-selftest.sh — exercise loop-ab.sh's reporting layer against
# synthetic fixtures, with no models, no network, and no builds.
#
# Why this exists: the reporting layer is awk embedded in bash, and bugs there
# degrade SILENTLY — the report still prints, it just says "none" forever, or
# quietly omits a section. None of them can fail a Rust test. That is the
# failure mode docs/verification-discipline.md calls a gate that cannot fail.
#
# First round (dirge-1elu.6) it was three: the arm list collected the model
# column instead of the tag column, the co-occurrence lookup key had its two
# components reversed against the accumulation key, and the boundaries value
# was scraped from a TSV column `run_arm` never wrote.
#
# Second round (dirge-l8l7) it was four more, and this file is why they
# survived: it was an OUTPUT test, not a DISCRIMINATION test. Three of its
# seven assertions had a known-other-answer — the treatment/allgates pair —
# and those are the ones that caught their bugs. The rest asserted "the report
# says X" with nothing establishing what would have made it say not-X, so a
# section that printed only on a broken run, and an ordering that came out of
# awk's hash table, both read as passes. One assertion was worse than useless:
# it pinned the SCRAMBLED event order as the expected value, which locked the
# bug in and would have failed on gawk.
#
# So: every claim below is checked against a fixture where the answer must be
# different. `want`/`reject` state one side, `differs` states both. When you
# add an assertion, add its other side, or it is not evidence.
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
# Same for the shell-side field summer (dirge-l8l7.5), for the same reason.
sed -n '/^sum_fields() {/,/^}/p' "$ab" > "$work/sum_fields.sh"
if [ ! -s "$work/sum_fields.sh" ]; then
  echo "FAIL: could not extract sum_fields() from $ab" >&2
  exit 1
fi
# shellcheck source=/dev/null
. "$work/sum_fields.sh"

# An awk abort must arrive as a FAILING CHECK, not as `set -e` killing this
# script — a bare early death names nothing, and "the harness exited 1" is the
# least useful form a finding can take. dirge-l8l7.4 aborted the whole report
# with "division by zero"; that has to read as a report that did not happen.
report() { awk -F'\t' -f "$work/report.awk" "$1" 2>&1 || echo "REPORT-ABORTED"; }

# Lines between a section header and the next blank line. The two-arm and
# N-arm blocks use the SAME row labels (`gates_fired` appears in both), so an
# unanchored grep can be satisfied by the other section — which is exactly how
# a wrong-column mutation escaped this file on its first run.
slice() { # $1 = output, $2 = header (literal)
  printf '%s\n' "$1" | awk -v h="$2" '$0 == h {inb=1; next} inb && /^$/ {exit} inb'
}

# tag model repeat turns tools err scav storm streak rep_inv rep_tot verify
# first_write correct tally prologue nudges tier boundaries gates
row() { # tag model repeat verify correct tally boundaries gates
  printf '%s\t%s\t%s\t10\t20\t0\t0\t0\t0\t0\t0\t%s\t3\t%s\t%s\t0\t2\tnominal\t%s\t%s\n' "$@"
}

# ---- Fixtures. Each exists to be the other answer for some assertion. -----

# HEALTHY: two models x three arms, every tally found, every run identical
# except for its boundary shape. This is the shape a real, correct run has.
mk_healthy() {
  local m
  for m in m1 m2; do
    row control   "$m" 1 VerifiedGreen 1 1 none              0
    row control   "$m" 2 VerifiedGreen 1 1 none              0
    row treatment "$m" 1 VerifiedGreen 1 1 'Verifier+Critic' 2
    row treatment "$m" 2 VerifiedGreen 1 1 'Verifier+Critic' 2
    row allgates  "$m" 1 VerifiedGreen 1 1 'Verifier;Critic' 2
    row allgates  "$m" 2 VerifiedGreen 1 1 'Verifier;Critic' 2
  done
}
mk_healthy > "$work/healthy.tsv"

# SICK: byte-identical except one run produced no dirge::gates line. The
# ONLY difference from healthy is the tally flag on one row.
awk -F'\t' 'BEGIN{OFS="\t"} NR==1{$15=0} {print}' "$work/healthy.tsv" > "$work/sick.tsv"

# LOPSIDED: a model present under control and absent under treatment. Used to
# abort the whole report with "division by zero" (dirge-l8l7.4).
{ mk_healthy; row control m3 1 VerifiedGreen 1 1 none 0; } > "$work/lopsided.tsv"

# ORDER: five distinct events in a known input order.
{
  row control   m1 1 VerifiedGreen 1 1 none        0
  row treatment m1 1 VerifiedGreen 1 1 'A;B;C;D;E' 5
} > "$work/order.tsv"

# MECH: two arms only, so `gates_fired` appears exactly once and cannot be
# satisfied by the N-arm section's row of the same name. The gate count (3) is
# deliberately DIFFERENT from the nudge count (2, fixed by `row`), so reading
# the wrong column is visible rather than coincidentally equal — the same
# discipline that `get_field`'s left-boundary fix came out of.
{
  row control   m1 1 VerifiedGreen 1 1 none              0
  row control   m1 2 VerifiedGreen 1 1 none              0
  row treatment m1 1 VerifiedGreen 1 1 'Verifier+Critic' 3
  row treatment m1 2 VerifiedGreen 1 1 'Verifier+Critic' 3
} > "$work/mech.tsv"

# LEGACY: the same runs as recorded before the gates column existed.
cut -f1-19 "$work/mech.tsv" > "$work/legacy.tsv"

out_healthy="$(report "$work/healthy.tsv")"
out_sick="$(report "$work/sick.tsv")"
out_lopsided="$(report "$work/lopsided.tsv")"
out_order="$(report "$work/order.tsv")"
out_mech="$(report "$work/mech.tsv")"
out_legacy="$(report "$work/legacy.tsv")"

fails=0
want() { # $1 = output, $2 = description, $3 = grep -E pattern
  if printf '%s\n' "$1" | grep -qE "$3"; then
    echo "  ok   $2"
  else
    echo "  FAIL $2 (no line matching: $3)"
    fails=$((fails + 1))
  fi
}
reject() { # $1 = output, $2 = description, $3 = pattern that must NOT match
  if printf '%s\n' "$1" | grep -qE "$3"; then
    echo "  FAIL $2 (matched, and must not: $3)"
    fails=$((fails + 1))
  else
    echo "  ok   $2"
  fi
}
differs() { # $1 = description, $2 = a, $3 = b
  if [ "$2" != "$3" ]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1 (both sides produced the same answer — the check cannot discriminate)"
    fails=$((fails + 1))
  fi
}
eq() { # $1 = description, $2 = got, $3 = want
  if [ "$2" = "$3" ]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1 (got '$2', want '$3')"
    fails=$((fails + 1))
  fi
}

echo "loop-ab.sh reporting self-test:"

# ---- Nothing below means anything if the report did not run. Checked first
# and for every fixture, because an awk abort produces no rows at all and
# every `reject` would then pass vacuously.
for fx in healthy sick lopsided order mech legacy; do
  eval "reject \"\$out_$fx\" \"the $fx report ran to completion\" 'REPORT-ABORTED'"
done

# ---- Arms are named by TAG, not by model. The bug printed `co-occurrence m1`.
want   "$out_healthy" "arms are named by tag, not model" '^  co-occurrence (control|treatment|allgates):'
reject "$out_healthy" "no arm named after a model"       '^  co-occurrence m[0-9]+:'

# ---- Co-firing vs co-presence. Neither assertion means anything without the
# other: if the report cannot tell these two apart, co-occurrence is not being
# measured at all.
want "$out_healthy" "co-firing at one boundary reads as one event"  '^  co-occurrence treatment: Verifier\+Critic x2$'
want "$out_healthy" "the same gates at separate boundaries differ"  '^  co-occurrence allgates: Verifier x2, Critic x2$'
differs "co-firing and separate-firing produce different lines" \
  "$(printf '%s\n' "$out_healthy" | grep '^  co-occurrence treatment:')" \
  "$(printf '%s\n' "$out_healthy" | grep '^  co-occurrence allgates:')"
want "$out_healthy" "an arm with no events says so" '^  co-occurrence control: none'

# ---- dirge-l8l7.3: events come out in the order the run produced them.
# `for (i in evs)` walked awk hash order and reported A;B;C;D;E as
# `B, C, D, E, A`. The previous version of this file asserted that scrambled
# order as the expected value.
want "$out_order" "co-occurrence preserves input event order" \
  '^  co-occurrence treatment: A x1, B x1, C x1, D x1, E x1$'

# ---- dirge-l8l7.2: the N-arm cross-model summary must not be conditioned on
# the run being broken. It used to sit inside the missing-tally warning
# branch, so it printed on `sick` and never on `healthy` — and the old fixture
# carried a missing tally, which is why nothing noticed.
want "$out_healthy" "N-arm mode compares the extra arm"                 '^== model: m1 — arm: allgates ==$'
want "$out_healthy" "extra-arms summary prints on a HEALTHY run"        '^== summary across models, extra arms ==$'
want "$out_sick"    "extra-arms summary also prints on a broken run"    '^== summary across models, extra arms ==$'

# ---- The missing-tally warning is the thing that IS conditional. This pair
# is what proves the two are no longer the same switch.
want   "$out_sick"    "a missing tally is reported, not zeroed" '^tally_found +1/2'
want   "$out_sick"    "the missing-tally WARNING fires"         'WARNING: 1 run\(s\) produced no dirge::gates line'
reject "$out_healthy" "and stays silent on a healthy run"       'WARNING: [0-9]+ run\(s\) produced no dirge::gates line'

# ---- dirge-l8l7.4: an absent arm is reported, and does not take the report
# with it. Before the guard this produced NO output at all.
want "$out_lopsided" "a lopsided model still produces a report"   '^== model: m3 ==$'
want "$out_lopsided" "the absent arm is named, not silently zeroed" 'NOTE: control runs=1, treatment runs=0'
want "$out_lopsided" "its rate delta reads n/a, not a number"    '^success_rate .*n/a \(arm absent\)$'
want "$out_lopsided" "and the other models still report"         '^== model: m1 ==$'
# The other side: a complete pair must NOT be labelled absent.
reject "$out_healthy" "a complete pair is not labelled absent" 'NOTE: control runs='

# ---- dirge-l8l7.5: gates are half the mechanism check and were missing.
# Asserted on the two-arm-only fixture, where gate count (3) differs from
# nudge count (2), so reading the wrong column changes the answer.
want "$out_mech" "gates_fired reads the gate column"   '^gates_fired +0\.0 \(0\.\.0\) +3\.0 \(3\.\.3\) +mechanism$'
want "$out_mech" "nudges_fired still reads its own"    '^nudges_fired +2\.0 \(2\.\.2\) +2\.0 \(2\.\.2\) +mechanism$'
# The N-arm block carries its own copy of the row and needs its own check —
# sliced, because the label is shared with the two-arm block above.
want "$(slice "$out_healthy" '== model: m1 — arm: allgates ==')" \
  "the N-arm block reports gates too" '^gates_fired +0\.0 \(0\.\.0\) +2\.0 \(2\.\.2\)'
# The other side: on a TSV with no gates column it must read 0, not invent a
# number, and must say the column was absent. Back-compat for an older file.
want "$out_legacy"  "a pre-gates-column TSV reads gates as 0"      '^gates_fired +0\.0 \(0\.\.0\) +0\.0 \(0\.\.0\)'
want "$out_legacy"  "and says the column was absent, not measured" 'NOTE: 4 row\(s\) predate the gates_fired column'
reject "$out_mech"  "a full-width TSV carries no such note"        'predate the gates_fired column'
differs "the gates column is actually read, not fabricated" \
  "$(printf '%s\n' "$out_mech"   | grep '^gates_fired')" \
  "$(printf '%s\n' "$out_legacy" | grep '^gates_fired')"

# ---- dirge-l8l7.6: unanimous-flat is not a disagreement.
want   "$out_healthy" "flat in every model is labelled as such" 'turns +better 0, worse 0, flat 2 of 2 models — flat in every model'
reject "$out_healthy" "and is not called MIXED"                 'turns .*— MIXED'

# ---- dirge-l8l7.5: sum_fields has no hardcoded name list.
eq "sum_fields totals a prefix"        "$(sum_fields nudge_ ' nudge_a=1 nudge_b=2 gate_x=5')"  3
eq "sum_fields keeps prefixes apart"   "$(sum_fields gate_  ' nudge_a=1 nudge_b=2 gate_x=5')"  5
eq "sum_fields counts an UNKNOWN field" "$(sum_fields nudge_ ' nudge_track_work=1 nudge_brand_new_thing=4')" 5
eq "sum_fields reports 0, not empty"   "$(sum_fields gate_  ' nudge_a=1')"                     0

if [ "$fails" -ne 0 ]; then
  echo
  echo "$fails check(s) failed. Healthy-fixture report:"
  printf '%s\n' "$out_healthy"
  exit 1
fi
echo "all checks passed"

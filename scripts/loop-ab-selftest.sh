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
# input_tokens cached_tokens cache_creation_tokens session_found denied_attempts
row() { # tag model repeat verify correct tally boundaries gates
  printf '%s\t%s\t%s\t10\t20\t0\t0\t0\t0\t0\t0\t%s\t3\t%s\t%s\t0\t2\tnominal\t%s\t%s\t1000\t400\t0\t1\t0\n' "$@"
}

# A row carrying DIFFERENT token values, so an assertion about the token
# columns has a known other-answer rather than matching the constant every
# other row also writes. 2000 in / 1800 cached is 90% against the default
# 1000 / 400 = 40%, and the two are on opposite sides of every direction
# rule: more input is worse, more cached is better.
row_tok() { # tag model repeat verify correct tally boundaries gates in cached create sessfound
  printf '%s\t%s\t%s\t10\t20\t0\t0\t0\t0\t0\t0\t%s\t3\t%s\t%s\t0\t2\tnominal\t%s\t%s\t%s\t%s\t%s\t%s\t0\n' "$@"
}

# A row carrying a non-zero denied_tool_attempts (col 25), so the row that
# reads it has a known other-answer. The value differs from every other
# numeric constant these fixtures use, so a wrong-column read is visible
# rather than coincidentally equal.
row_denied() { # tag model repeat verify correct tally boundaries gates denied
  printf '%s\t%s\t%s\t10\t20\t0\t0\t0\t0\t0\t0\t%s\t3\t%s\t%s\t0\t2\tnominal\t%s\t%s\t1000\t400\t0\t1\t%s\n' "$@"
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

# PRETOKEN: recorded AFTER the gates column but BEFORE the token columns —
# a legitimate 20-wide file, not a truncated one. It must report the missing
# token columns and must NOT be accused of predating gates_fired or of being
# a truncated write, and its session_found must read as an absent column
# rather than 0/2 (which would call every archived run a harness bug).
cut -f1-20 "$work/mech.tsv" > "$work/pretoken.tsv"

# TOKENS: control bills more input and caches less than treatment. Both
# token rows must therefore point the SAME way (treatment better) while
# reading opposite columns — a swapped-column bug flips exactly one of them.
# Spreads are wide enough that the deltas clear the noise floor.
{
  row_tok control   m1 1 VerifiedGreen 1 1 none              0 9000 1000 0 1
  row_tok control   m1 2 VerifiedGreen 1 1 none              0 9100 1100 0 1
  row_tok treatment m1 1 VerifiedGreen 1 1 'Verifier+Critic' 3 3000 2700 0 1
  row_tok treatment m1 2 VerifiedGreen 1 1 'Verifier+Critic' 3 3100 2800 0 1
} > "$work/tokens.tsv"

# NOSESSION: identical to tokens.tsv except one run wrote no session file.
# Only the session_found flag differs, so a report that cannot tell them
# apart is not reading the column.
awk -F'\t' 'BEGIN{OFS="\t"} NR==1{$24=0} {print}' "$work/tokens.tsv" > "$work/nosession.tsv"

# BLANK: mech.tsv plus a trailing empty line. Must report identically — a
# blank line used to mint an arm named "" and a model named "".
{ cat "$work/mech.tsv"; printf '\n'; } > "$work/blank.tsv"

# TRUNCATED: mech.tsv plus a row cut off mid-write.
{ cat "$work/mech.tsv"; printf 'control\tm1\t3\t10\n'; } > "$work/truncated.tsv"

out_healthy="$(report "$work/healthy.tsv")"
out_sick="$(report "$work/sick.tsv")"
out_lopsided="$(report "$work/lopsided.tsv")"
out_order="$(report "$work/order.tsv")"
out_mech="$(report "$work/mech.tsv")"
out_blank="$(report "$work/blank.tsv")"
out_truncated="$(report "$work/truncated.tsv")"
out_legacy="$(report "$work/legacy.tsv")"
# DENIED: the arms differ ONLY in denied_tool_attempts — control reaches for
# tools it does not have, treatment does not. This is the shape a working
# capability-projection A/B produces, and the fixture exists so the row is
# checked against a case where the answer must differ.
{
  row_denied control   m1 1 VerifiedGreen 1 1 none              0 7
  row_denied control   m1 2 VerifiedGreen 1 1 none              0 9
  row_denied treatment m1 1 VerifiedGreen 1 1 'Verifier+Critic' 3 0
  row_denied treatment m1 2 VerifiedGreen 1 1 'Verifier+Critic' 3 0
} > "$work/denied.tsv"

out_denied="$(report "$work/denied.tsv")"
out_pretoken="$(report "$work/pretoken.tsv")"
out_tokens="$(report "$work/tokens.tsv")"
out_nosession="$(report "$work/nosession.tsv")"

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
same() { # $1 = description, $2 = a, $3 = b
  if [ "$2" = "$3" ]; then
    echo "  ok   $1"
  else
    echo "  FAIL $1 (the two reports differ, and must not)"
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

# ---- The awk program is ONE single-quoted shell string, so an apostrophe
# anywhere inside it — including in a comment — terminates the quote. What
# happens next is the worst available failure: awk still parses what it got,
# the trailing words become filenames ("awk: can't open file prefix"), the
# script still exits 0, and the run prints per-repeat lines for an hour
# before the comparison silently never appears. Nothing above catches it,
# because the extraction below reads the block by line range and so is happy
# either way. This is a source check, not an output check, for that reason.
awk_start="$(grep -n '^awk -F' "$ab" | cut -d: -f1)"
awk_end="$(awk -v s="$awk_start" 'NR>s && /^'"'"' "\$WORK\/results.tsv"/{print NR; exit}' "$ab")"
stray="$(awk -v s="$awk_start" -v e="$awk_end" "NR>s && NR<e && /'/ {print NR\": \"\$0}" "$ab")"
if [ -z "$stray" ]; then
  echo "  ok   no apostrophe inside the single-quoted awk program"
else
  echo "  FAIL apostrophe inside the awk program would end its quoting:"
  printf '       %s\n' "$stray"
  fails=$((fails + 1))
fi

# ---- Nothing below means anything if the report did not run. Checked first
# and for every fixture, because an awk abort produces no rows at all and
# every `reject` would then pass vacuously.
for fx in healthy sick lopsided order mech legacy blank truncated pretoken tokens nosession denied; do
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

# ---- denied_tool_attempts (dirge-e31n.3). Fewer attempts at tools the mode
# refuses is better, and the row must read col 25 rather than any of the
# numerically-similar columns around it.
want "$out_denied" "denied_tool_attempts reads its own column" \
  '^denied_tool_attempts +8\.0 \(7\.\.9\) +0\.0 \(0\.\.0\)'
want "$out_denied" "fewer denied attempts is better" '^denied_tool_attempts .* better$'
# Other side: a fixture where nothing was attempted must not report a win.
reject "$out_mech" "a run with no denied attempts reports no direction" \
  '^denied_tool_attempts .* better$'

# ---- Token + cache columns (dirge-e31n.1). Asserted on tokens.tsv, where
# control and treatment carry DIFFERENT values in both columns, so a row
# that read the wrong column would show the other arm's number.
want "$out_tokens" "input_tokens reads its own column" \
  '^input_tokens +9050\.0 \(9000\.\.9100\) +3050\.0 \(3000\.\.3100\)'
want "$out_tokens" "cached_tokens reads its own column" \
  '^cached_tokens +1050\.0 \(1000\.\.1100\) +2750\.0 \(2700\.\.2800\)'
# The direction rule is INVERTED between the two: less input is better,
# more cached is better. A copy-pasted dir3 call gets one of them backwards,
# and only checking both sides catches it.
want "$out_tokens" "less input is better"  '^input_tokens .* better$'
want "$out_tokens" "more cached is better" '^cached_tokens .* better$'
# Ratio of the two, not a third column: 1050/9050 = 12%, 2750/3050 = 90%.
want "$out_tokens" "cache_hit_rate divides cached by input" \
  '^cache_hit_rate +12% +90% +observed$'
want "$out_tokens" "a full-token run reports every session found" '^session_found +2/2 +2/2'

# ---- An absent session is a harness bug and must read as one — the same
# discipline as tally=missing. nosession.tsv differs from tokens.tsv in
# exactly one field, so a report that cannot separate them is not reading it.
want    "$out_nosession" "a missing session file is reported, not zeroed" '^session_found +1/2'
differs "session_found actually reads the column" \
  "$out_tokens" "$out_nosession"

# ---- An absent COLUMN is not an absent session. pretoken.tsv is a
# legitimate 20-wide file: it must be told what it is missing, and must not
# be accused of the two OTHER shapes of incompleteness.
want   "$out_pretoken" "a pre-token TSV says which columns are absent" \
  'NOTE: 4 row\(s\) predate the token columns'
want   "$out_pretoken" "and reads session_found as absent, not as 0 found" \
  '^session_found +n/a \(column absent\) +n/a \(column absent\)'
reject "$out_pretoken" "a pre-token TSV is not accused of predating gates" \
  'predate the gates_fired column'
reject "$out_pretoken" "and is not reported as a truncated write" \
  'had too few fields'
reject "$out_tokens"   "a full-width TSV carries no pre-token note" \
  'predate the token columns'
# A 19-wide legacy file predates BOTH columns and must say both, not one.
want "$out_legacy" "a pre-gates TSV also names the missing token columns" \
  'predate the token columns'

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

# ---- A row that is not a run record must not become one. A blank line used
# to mint an arm named "" and a model named ""; before the absent-arm guard it
# divided by zero and took the report down.
same "a trailing blank line changes nothing" "$out_blank" "$out_mech"
reject "$out_blank" "no arm is named after an empty field" '^  co-occurrence : '
want   "$out_truncated" "a truncated row is reported, not absorbed" 'WARNING: 1 row\(s\) had too few fields'
reject "$out_truncated" "and does not mint a phantom model"         '^== model:  ==$'
reject "$out_mech"      "a clean file carries no truncation warning" 'had too few fields'

# ---- dirge-l8l7.5: sum_fields has no hardcoded name list.
eq "sum_fields totals a prefix"        "$(sum_fields nudge_ ' nudge_a=1 nudge_b=2 gate_x=5')"  3
eq "sum_fields keeps prefixes apart"   "$(sum_fields gate_  ' nudge_a=1 nudge_b=2 gate_x=5')"  5
eq "sum_fields counts an UNKNOWN field" "$(sum_fields nudge_ ' nudge_track_work=1 nudge_brand_new_thing=4')" 5
eq "sum_fields reports 0, not empty"   "$(sum_fields gate_  ' nudge_a=1')"                     0
eq "sum_fields anchors the prefix"     "$(sum_fields tool_  ' errored_tool_calls=7 tool_x=1')"  1
# The status, not the value. loop-ab.sh runs under `set -euo pipefail`, so a
# helper that exits non-zero on "nothing matched" does not fail a check — it
# kills the harness mid-run, after the model calls have been paid for. The
# value check above cannot see this: a command substitution used as an
# ARGUMENT masks the exit status, which is exactly why it passed while the
# first `grep | grep | awk` cut was abort-prone.
if sum_fields gate_ ' nudge_a=1' >/dev/null 2>&1; then
  echo "  ok   sum_fields exits 0 when nothing matches"
else
  echo "  FAIL sum_fields exits non-zero when nothing matches (set -e would kill a real run)"
  fails=$((fails + 1))
fi

if [ "$fails" -ne 0 ]; then
  echo
  echo "$fails check(s) failed. Healthy-fixture report:"
  printf '%s\n' "$out_healthy"
  exit 1
fi
echo "all checks passed"

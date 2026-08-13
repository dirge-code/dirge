#!/opt/homebrew/bin/bash
# Mutation-testing harness: reintroduce a bug, assert the suite goes red.
#
# WHY THIS EXISTS
#
# A test that has never failed is a test that has never been shown to test
# anything. Every guard in this repo that turned out to be load-bearing was
# confirmed by breaking it on purpose and watching a specific test die; several
# that looked load-bearing had SURVIVING mutations, which is how we learned the
# fixture was passing for the wrong reason (see docs/verification-discipline.md).
#
# Doing that by hand has bitten twice:
#
#   1. DISK. Each mutated rebuild writes a fresh incremental-compilation
#      session. Thirty mutations in an afternoon is thirty of them, and
#      target/debug/incremental reached 157 GB and filled the disk mid-run —
#      rustc started dying with SIGBUS, which reads like a compiler bug rather
#      than a full volume. Mutation builds are throwaway and gain almost
#      nothing from incrementality (this is one large binary crate, so touching
#      any file invalidates most of it anyway), so this runs them with
#      CARGO_INCREMENTAL=0 and the growth stops.
#
#   2. RESTORE. Hand-written `cp` back after each mutation is fine until a
#      command times out or the shell is interrupted between the mutate and the
#      restore, and then a mutated file is sitting in the tree looking like real
#      work. `trap ... EXIT` here restores unconditionally.
#
# USAGE — source it, then declare what to protect and mutate:
#
#   source scripts/mutate.sh
#   mutate_protect src/agent/agent_loop/capability.rs
#
#   mutate "weight=1 (pre-split)" the_missing_info_weight_is_pinned <<'PY'
#   edit('src/agent/agent_loop/capability.rs',
#        'const W_ERRORED_MISSING_INFO: u32 = 2;',
#        'const W_ERRORED_MISSING_INFO: u32 = 1;')
#   PY
#
#   mutate_control agent_loop::capability::
#   mutate_summary        # exits non-zero if any mutation SURVIVED
#
# `edit(path, old, new)` is provided to the heredoc and asserts `old` was
# present, so a mutation that silently failed to apply is reported as a broken
# mutation rather than counted as a survivor — the difference matters, because
# "the test passed" and "the bug was never introduced" look identical from the
# outside.

set -uo pipefail

_MUT_PROTECTED=()
_MUT_BACKUP="$(mktemp -d "${TMPDIR:-/tmp}/mutate.XXXXXX")"
_MUT_KILLED=0
_MUT_SURVIVED=0
_MUT_BROKEN=0
_MUT_SURVIVORS=()

# Incremental is the whole reason the disk filled. Off for everything this
# script runs; the caller's interactive builds are unaffected.
export CARGO_INCREMENTAL=0

_mut_restore() {
  local f
  for f in "${_MUT_PROTECTED[@]+"${_MUT_PROTECTED[@]}"}"; do
    [ -f "$_MUT_BACKUP/$(basename "$f")" ] && cp "$_MUT_BACKUP/$(basename "$f")" "$f"
  done
}
_mut_cleanup() {
  _mut_restore
  rm -rf "$_MUT_BACKUP"
}
trap _mut_cleanup EXIT INT TERM

# Files the mutations may touch. Backed up once; restored after every mutation
# and again on exit.
mutate_protect() {
  local f
  for f in "$@"; do
    [ -f "$f" ] || { echo "mutate_protect: no such file: $f" >&2; exit 2; }
    _MUT_PROTECTED+=("$f")
    cp "$f" "$_MUT_BACKUP/$(basename "$f")"
  done
}

# Apply one mutation (python on stdin), run a test filter, restore.
# The mutation must make the filter FAIL. A pass means the mutation survived —
# nothing was testing that behaviour.
mutate() { # $1 = name, $2.. = nextest filters; python on stdin
  local name="$1"; shift
  local py
  py="$(cat)"
  if ! python3 - <<PYEOF
import sys
def edit(path, old, new, count=1):
    s = open(path).read()
    if old not in s:
        sys.exit("MUTATION DID NOT APPLY: pattern absent in %s" % path)
    open(path, 'w').write(s.replace(old, new, count))
$py
PYEOF
  then
    echo "  BROKEN   $name (mutation could not be applied)"
    _MUT_BROKEN=$((_MUT_BROKEN + 1))
    _mut_restore
    return
  fi
  if timeout 900 cargo nextest run "$@" >/dev/null 2>&1; then
    echo "  SURVIVED $name  <-- nothing tests this"
    _MUT_SURVIVED=$((_MUT_SURVIVED + 1))
    _MUT_SURVIVORS+=("$name")
  else
    echo "  killed   $name"
    _MUT_KILLED=$((_MUT_KILLED + 1))
  fi
  _mut_restore
}

# The unmutated tree must be green, or every "killed" above is meaningless —
# they would all be reporting a suite that was already failing.
mutate_control() { # $1.. = nextest filters
  if timeout 900 cargo nextest run "$@" >/dev/null 2>&1; then
    echo "  control  clean"
  else
    echo "  CONTROL FAILED — the unmutated tree is red, so every result above is void" >&2
    _MUT_SURVIVED=$((_MUT_SURVIVED + 1))
    _MUT_SURVIVORS+=("CONTROL")
  fi
}

mutate_summary() {
  echo "  ${_MUT_KILLED} killed, ${_MUT_SURVIVED} survived, ${_MUT_BROKEN} broken"
  if [ "$_MUT_SURVIVED" -gt 0 ] || [ "$_MUT_BROKEN" -gt 0 ]; then
    local s
    for s in "${_MUT_SURVIVORS[@]+"${_MUT_SURVIVORS[@]}"}"; do
      echo "    survived: $s"
    done
    return 1
  fi
  return 0
}

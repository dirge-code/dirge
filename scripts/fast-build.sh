#!/usr/bin/env bash
set -euo pipefail

# Build dirge with sccache wrapping rustc. Opt-in helper, NOT the committed
# default — a plain `cargo build`, the release build, and CI all run without a
# wrapper. See plans/compile-time-improvements.md for rationale and numbers.
#
# When this helps: *clean* rebuilds where cargo's own incremental cache is
# gone — a fresh `cargo clean`, a branch switch that changes Cargo.lock, or a
# second checkout sharing dependency versions. Measured ~40% here (85s -> 52s).
# It does NOT speed up the normal edit-one-file loop (~5.5s); that's cargo
# incremental, which sccache can't cache. Registry deps are non-incremental
# and get cached; our crate stays incremental — so leave CARGO_INCREMENTAL be.
#
# Extra args pass through to cargo:  scripts/fast-build.sh --features acp
#
# Needs sccache on PATH: `brew install sccache` (or `cargo install sccache`).

if ! command -v sccache >/dev/null 2>&1; then
  echo "sccache not found — install it first: brew install sccache" >&2
  exit 1
fi

RUSTC_WRAPPER="$(command -v sccache)"
export RUSTC_WRAPPER
echo "==> sccache wrapper: $RUSTC_WRAPPER"
echo "==> cargo build --bin dirge $*"
exec cargo build --bin dirge "$@"

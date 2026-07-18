#!/bin/bash
# Rustc wrapper for cargo — codesigns the macOS microVM runner after linking.
#
# Invoked by cargo (via build.rustc-wrapper) for EVERY rustc invocation.
# On macOS, when the crate name is dirge_microvm_runner, signs the linked
# binary with the hypervisor entitlement automatically.
# On non-macOS or for non-runner crates, passes through instantly.
#
# Only works while ensure_runner_signed does NOT use --options runtime
# (hardened runtime strips DYLD_FALLBACK_LIBRARY_PATH).
#
# Cargo invocations:
#   - Probe:        wrapper rustc - --crate-name ___ ...
#   - Compile:      wrapper rustc --crate-name crate --emit=dep-info,link ...
#   - Bin link:     wrapper --crate-name dirge_microvm_runner --out-dir ... -C extra-filename=...
#
# The first argument is always "rustc" (argv0-like), stripped below.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ENTITLEMENTS="$PROJECT_ROOT/dirge.entitlements"

ARGS=("$@")

# Strip leading "rustc" arg (cargo passes it as argv0-like).
if [[ "${ARGS[0]}" == rustc ]]; then
    ARGS=("${ARGS[@]:1}")
fi

# RUSTC is set by cargo for real compilations; fall back to bare `rustc`.
RUSTC_BIN="${RUSTC-rustc}"

# Run the real rustc.
set +e
"$RUSTC_BIN" "${ARGS[@]}"
RET=$?
set -e

# Only codesign on macOS when the build succeeded.
if [[ $RET -ne 0 || "$(uname)" != "Darwin" ]]; then
    exit $RET
fi

# Detect the runner by crate name (cargo uses --out-dir + -C extra-filename,
# not a single -o flag).
IS_RUNNER=0
OUT_DIR=""
CRATE_NAME=""
EXTRA=""
for ((i = 0; i < ${#ARGS[@]}; i++)); do
    case "${ARGS[$i]}" in
        --crate-name)
            CRATE_NAME="${ARGS[$i+1]}"
            if [[ "$CRATE_NAME" == dirge_microvm_runner ]]; then
                IS_RUNNER=1
            fi
            ;;
        --out-dir) OUT_DIR="${ARGS[$i+1]}" ;;
        -C)
            if [[ "${ARGS[$i+1]}" == extra-filename=* ]]; then
                EXTRA="${ARGS[$i+1]#extra-filename=}"
            fi
            ;;
    esac
done

if [[ $IS_RUNNER -eq 1 ]]; then
    # Sign the intermediate binary (cargo copies this to target dir; signature
    # is embedded in the Mach-O and survives the copy).
    BIN_PATH="${OUT_DIR}/${CRATE_NAME}${EXTRA}"
    if [[ -n "$BIN_PATH" && -f "$BIN_PATH" ]]; then
        codesign --force --sign - --entitlements "$ENTITLEMENTS" "$BIN_PATH" 2>/dev/null || true
    fi
fi

exit $RET
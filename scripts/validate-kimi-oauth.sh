#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/validate-kimi-oauth.sh [--login] [--model MODEL] [PROMPT...]

Runs the feature-worktree Dirge binary against the Kimi Code (Moonshot) OAuth
provider path. The script forces isolated DIRGE_DATA_DIR and DIRGE_CONFIG_DIR
values, unsets API-key and provider/model env vars that could mask OAuth
fallback, and passes --provider kimi before the prompt.

Options:
  --login        Run `dirge auth kimi` first using the same DIRGE_DATA_DIR.
  --model MODEL  Model id to request. Default: k3 or DIRGE_KIMI_MODEL.
  -h, --help     Show this help.

Environment:
  DIRGE_OAUTH_VALIDATION_DATA_DIR  Auth/data dir. Default: /var/tmp/opencode/dirge-kimi-oauth-validation
  DIRGE_OAUTH_VALIDATION_CONFIG_DIR  Config parent dir. Default: /var/tmp/opencode/dirge-kimi-oauth-validation-config
  DIRGE_KIMI_MODEL                 Default model when --model is omitted.
  CARGO                            Cargo executable. Default: /home/user/.cargo/bin/cargo

This script does not print token files or token values. Do not paste user codes,
auth file contents, or tokens into issue comments.
USAGE
}

run_login=0
model="${DIRGE_KIMI_MODEL:-k3}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --login)
      run_login=1
      shift
      ;;
    --model)
      if [[ $# -lt 2 || -z "$2" ]]; then
        printf 'error: --model requires a value\n' >&2
        exit 2
      fi
      model="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    *)
      break
      ;;
  esac
done

prompt="${*:-Reply with exactly: dirge-oauth-ok}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
worktree_dir="$(cd -- "$script_dir/.." && pwd)"
data_dir="${DIRGE_OAUTH_VALIDATION_DATA_DIR:-/var/tmp/opencode/dirge-kimi-oauth-validation}"
config_root="${DIRGE_OAUTH_VALIDATION_CONFIG_DIR:-/var/tmp/opencode/dirge-kimi-oauth-validation-config}"
config_dir="$config_root/empty-config"
cargo_bin="${CARGO:-/home/user/.cargo/bin/cargo}"

rm -rf -- "$config_dir"
mkdir -p -- "$data_dir" "$config_dir"
cd -- "$worktree_dir"

# Force OAuth fallback and avoid accidental provider/model defaults.
unset KIMI_CODE_API_KEY
unset OPENAI_API_KEY
unset DEEPSEEK_API_KEY
unset OPENROUTER_API_KEY
unset ANTHROPIC_API_KEY
unset GEMINI_API_KEY
unset GOOGLE_GENERATIVE_AI_API_KEY
unset GOOGLE_API_KEY
unset GLM_API_KEY
unset ZHIPU_API_KEY
unset DIRGE_PROVIDER
unset DIRGE_MODEL
export DIRGE_DATA_DIR="$data_dir"
export DIRGE_CONFIG_DIR="$config_dir"

printf 'Using worktree: %s\n' "$worktree_dir" >&2
printf 'Using DIRGE_DATA_DIR: %s\n' "$DIRGE_DATA_DIR" >&2
printf 'Using DIRGE_CONFIG_DIR: %s\n' "$DIRGE_CONFIG_DIR" >&2
printf 'Using provider: kimi\n' >&2
printf 'Using model: %s\n' "$model" >&2

if [[ "$run_login" -eq 1 ]]; then
  printf '\nStarting Kimi device-code login. Keep the user code private.\n' >&2
  RUSTFLAGS="" "$cargo_bin" run --quiet --bin dirge -- auth kimi
fi

printf '\nRunning Kimi OAuth validation request.\n' >&2
set +e
RUSTFLAGS="" "$cargo_bin" run --quiet --bin dirge -- \
  --provider kimi \
  --model "$model" \
  --print \
  --no-tools \
  --no-session \
  "$prompt"
status=$?
set -e

if [[ "$status" -ne 0 ]]; then
  cat >&2 <<'ERROR_HINT'

Validation command failed. Non-secret checks:
  - If it says no API key or OAuth login was found, rerun this script with --login.
  - If it says the stored OAuth credential is expired, rerun this script with --login.
  - If the model is rejected, rerun with --model kimi-for-coding (lower membership
    tiers may not have K3 access).
  - If output mentions a different provider, paste the exact command and first
    non-secret output line.

Do not paste token values, auth.json contents, or user codes.
ERROR_HINT
fi

exit "$status"

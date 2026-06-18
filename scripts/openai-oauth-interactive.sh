#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/openai-oauth-interactive.sh [--login] [--model MODEL] [-- DIRGE_ARGS...]

Starts an interactive Dirge session against the OpenAI OAuth/Codex provider path.
The script forces isolated DIRGE_DATA_DIR and DIRGE_CONFIG_DIR values, unsets
API-key/provider/model env vars that could mask OAuth fallback, and sets
RUSTFLAGS="" so local builds do not require the mold linker.

Options:
  --login        Run `dirge auth openai` first using the same DIRGE_DATA_DIR.
  --model MODEL  Model id to request. Default: gpt-5.5 or DIRGE_OPENAI_MODEL.
  -h, --help     Show this help.

Environment:
  DIRGE_OAUTH_VALIDATION_DATA_DIR  Auth/data dir. Default: /var/tmp/opencode/dirge-oauth-validation
  DIRGE_OAUTH_VALIDATION_CONFIG_DIR  Config parent dir. Default: /var/tmp/opencode/dirge-oauth-validation-config
  DIRGE_OPENAI_MODEL               Default model when --model is omitted.
  CARGO                            Cargo executable. Default: /home/user/.cargo/bin/cargo

Examples:
  scripts/openai-oauth-interactive.sh
  scripts/openai-oauth-interactive.sh --login
  scripts/openai-oauth-interactive.sh --model gpt-5.5 -- --no-tools

Do not paste token values, auth.json contents, user codes, or browser callback data.
USAGE
}

run_login=0
model="${DIRGE_OPENAI_MODEL:-gpt-5.5}"
dirge_args=()

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
      dirge_args=("$@")
      break
      ;;
    *)
      printf 'error: unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
worktree_dir="$(cd -- "$script_dir/.." && pwd)"
data_dir="${DIRGE_OAUTH_VALIDATION_DATA_DIR:-/var/tmp/opencode/dirge-oauth-validation}"
config_root="${DIRGE_OAUTH_VALIDATION_CONFIG_DIR:-/var/tmp/opencode/dirge-oauth-validation-config}"
config_dir="$config_root/empty-config"
cargo_bin="${CARGO:-/home/user/.cargo/bin/cargo}"

rm -rf -- "$config_dir"
mkdir -p -- "$data_dir" "$config_dir"
cd -- "$worktree_dir"

# Force OAuth fallback and avoid accidental provider/model defaults.
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
printf 'Using provider: openai\n' >&2
printf 'Using model: %s\n' "$model" >&2

if [[ "$run_login" -eq 1 ]]; then
  printf '\nStarting OpenAI device-code login. Keep the user code private.\n' >&2
  RUSTFLAGS="" "$cargo_bin" run --quiet --bin dirge -- auth openai
fi

printf '\nStarting interactive Dirge session.\n' >&2
exec env RUSTFLAGS="" "$cargo_bin" run --bin dirge -- \
  --provider openai \
  --model "$model" \
  "${dirge_args[@]}"

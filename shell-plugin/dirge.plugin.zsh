#!/usr/bin/env zsh
#
# dirge shell plugin — the `:` prefix.
#
# Type `:<prompt>` at your shell prompt and press Enter to send it to dirge
# without leaving the shell. Every `:` command in a given shell shares ONE
# dirge session (conversation), so follow-ups keep context. Adapted from
# Forge's shell-plugin (the `:`-dispatch mechanism), trimmed to dirge.
#
#   :<prompt>     send a prompt to dirge (headless `-p`, shared session)
#   :new [p]      start a fresh session (optionally with a first prompt)
#   :resume       open the full dirge TUI on this shell's session
#   :help         show this help
#
# Install: add `source /path/to/dirge/shell-plugin/dirge.plugin.zsh` to ~/.zshrc.
# Requires: dirge on PATH (or set DIRGE_BIN), built with `--session` resume-or-
# create support. zsh only (uses ZLE widgets).

# Guard: zsh + interactive only (ZLE has no meaning otherwise).
[[ -n "${ZSH_VERSION:-}" ]] || return 0
[[ -o interactive ]] || return 0

# ---------------------------------------------------------------------------
# Binary + per-shell session state
# ---------------------------------------------------------------------------
: ${DIRGE_BIN:=dirge}
typeset -g _DIRGE_BIN="$DIRGE_BIN"

# The conversation all `:` commands in this shell share. Generated lazily on
# first use; `dirge --session <id>` creates it then, resumes it after.
typeset -g _DIRGE_SESSION_ID=""

function _dirge_gen_id() {
    if command -v uuidgen >/dev/null 2>&1; then
        # lowercase; validate_session_id allows [A-Za-z0-9._-].
        print -r -- "dirge-shell-$(uuidgen | tr 'A-Z' 'a-z')"
    else
        print -r -- "dirge-shell-${EPOCHSECONDS:-$(date +%s)}-$$-${RANDOM}"
    fi
}

# Ensure a session id exists. MUST be called directly (not via `$(...)`):
# command-substitution runs in a subshell, so an assignment made there would
# not persist to this shell — which would mint a fresh id on every `:` command
# and break session continuity. Callers read `$_DIRGE_SESSION_ID` afterward.
function _dirge_ensure_session() {
    [[ -z "$_DIRGE_SESSION_ID" ]] && _DIRGE_SESSION_ID="$(_dirge_gen_id)"
}

function _dirge_new_session() {
    _DIRGE_SESSION_ID="$(_dirge_gen_id)"
    print -r -- "dirge: new session ${_DIRGE_SESSION_ID}"
}

# ---------------------------------------------------------------------------
# Invocations (connect to /dev/tty so they work from inside the ZLE widget)
# ---------------------------------------------------------------------------

# Headless one-shot: print the answer, return to the shell. The session keeps
# context across calls. `--` guards prompts that start with `-`.
function _dirge_send() {
    local prompt="$1"
    [[ -z "$prompt" ]] && return 0
    _dirge_ensure_session
    command "$_DIRGE_BIN" -p --session "$_DIRGE_SESSION_ID" -- "$prompt" </dev/tty >/dev/tty 2>&1
}

# Open the full TUI on this shell's session (e.g. to scroll back, switch model).
function _dirge_resume_tui() {
    _dirge_ensure_session
    command "$_DIRGE_BIN" --session "$_DIRGE_SESSION_ID" </dev/tty >/dev/tty 2>&1
}

function _dirge_plugin_help() {
    print -r -- "dirge shell plugin — the ':' prefix"
    print -r -- "  :<prompt>     send a prompt to dirge (headless, shares this shell's session)"
    print -r -- "  :new [p]      start a fresh session (optionally send a first prompt)"
    print -r -- "  :resume       open the full dirge TUI on this shell's session"
    print -r -- "  :help         this help"
    print -r --
    print -r -- "session: ${_DIRGE_SESSION_ID:-<none yet>}    binary: ${_DIRGE_BIN}"
}

# ---------------------------------------------------------------------------
# The accept-line widget: intercept lines starting with ':'
# ---------------------------------------------------------------------------
function dirge-accept-line() {
    emulate -L zsh
    # Anything not starting with ':' is a normal shell command — pass through.
    if [[ "$BUFFER" != :* ]]; then
        zle accept-line
        return
    fi

    local original="$BUFFER"
    local rest="${BUFFER#:}"   # drop the leading ':'
    rest="${rest# }"           # and one optional following space

    print -s -- "$original"    # keep the line the user typed in history
    BUFFER=""
    zle -I
    zle reset-prompt
    print ""                   # newline before output

    local action="${rest%% *}"   # first word
    local args="${rest#"$action"}"
    args="${args# }"

    case "$action" in
        new|n)      _dirge_new_session; [[ -n "$args" ]] && _dirge_send "$args" ;;
        resume|tui) _dirge_resume_tui ;;
        help|"")    _dirge_plugin_help ;;
        *)          _dirge_send "$rest" ;;   # whole line is the prompt
    esac

    zle reset-prompt
}
zle -N dirge-accept-line

function _dirge_apply_keybindings() {
    bindkey '^M' dirge-accept-line   # Enter
    bindkey '^J' dirge-accept-line   # Ctrl-J
}
_dirge_apply_keybindings

# Re-apply after zsh-vi-mode (jeffreytse/zsh-vi-mode) rebuilds the keymaps,
# which would otherwise clobber these bindings. Harmless no-op without it.
typeset -ga zvm_after_init_commands
zvm_after_init_commands+=('_dirge_apply_keybindings')

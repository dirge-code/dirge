# dirge shell plugin — the `:` prefix

A small zsh plugin that lets you talk to dirge **without leaving your shell**.
Type `:<prompt>` at your normal prompt, press Enter, and the prompt is sent to
dirge headlessly; the answer prints and you're back at the shell. Every `:`
command in a shell shares **one dirge session**, so follow-ups keep context.

Adapted from [Forge's](https://github.com/tailcallhq/forgecode) shell-plugin
(its `:`-dispatch mechanism), trimmed to dirge.

## Install

Requires zsh and `dirge` on your `PATH` (built with `--session` resume-or-create
support — i.e. recent enough that `dirge --session <id>` creates the session if
it doesn't exist yet). Then add to `~/.zshrc`:

```zsh
source /path/to/dirge/shell-plugin/dirge.plugin.zsh
```

Set `DIRGE_BIN` before sourcing if `dirge` isn't on your `PATH`:

```zsh
export DIRGE_BIN=~/.cargo/bin/dirge
source /path/to/dirge/shell-plugin/dirge.plugin.zsh
```

Open a new shell (or `source ~/.zshrc`).

## Usage

| Command | What it does |
|---------|--------------|
| `:<prompt>` | Send `<prompt>` to dirge headlessly, sharing this shell's session |
| `:new [prompt]` | Start a fresh session (optionally send a first prompt) |
| `:resume` | Open the full dirge TUI on this shell's session |
| `:help` | Show the command list + current session id |

Anything not starting with `:` runs as a normal shell command, untouched.

```zsh
$ : what does this repo's build pipeline do?      # asks dirge, prints answer
$ git status                                       # normal shell — unaffected
$ : now add a CI step that runs clippy             # same session → has context
$ :resume                                          # jump into the full TUI here
$ :new                                             # start a clean conversation
```

## How it works

- A ZLE `accept-line` widget (bound to Enter) checks whether the line starts
  with `:`. If not, it falls through to normal `accept-line`.
- A `:` line is parsed; reserved words (`new`, `resume`, `help`) run locally,
  everything else is treated as a prompt.
- The shell holds a stable per-session id in `_DIRGE_SESSION_ID` (generated on
  first use). It's passed as `dirge -p --session "$id" -- "<prompt>"`, so dirge
  creates the session on first use and resumes it on every subsequent `:`
  command — that's what gives follow-ups context.

## Notes

- Headless `-p` runs respect your dirge permission config. For unattended shell
  use you may want a more permissive mode (e.g. `--accept-all`) in your config
  so tool calls don't block on confirmation.
- zsh-vi-mode users: the bindings are re-applied after `zvm_init` so they
  aren't clobbered.
- This is zsh-only (it uses ZLE widgets). bash/fish ports welcome.

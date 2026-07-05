# Terminal UI reference

Key bindings, the inline avatar, and tool-output display behavior. For the
slash-command list, see the top-level [README](../README.md#slash-commands).

## Key bindings

The global command keys (toggle reasoning, scroll, chat navigation,
kill-subagent) are **rebindable** via the `keybindings` config — see
[config.md](config.md#key-bindings). The defaults are listed below.

### Input editing

| Key | Action |
|-----|--------|
| Ctrl+A / Home | Start of line |
| Ctrl+E / End | End of line |
| Ctrl+B / Left | Char left |
| Right | Char right |
| Option+Left / Meta+B | Skip to previous word |
| Option+Right / Meta+F | Skip to next word |
| Ctrl+K | Kill to end of line |
| Ctrl+U | Kill to start of line |
| Ctrl+W | Kill word before cursor |
| Ctrl+D | Delete the character at the cursor (forward; no-op at end of line) |
| Meta+Backspace | Delete word before cursor |
| Meta+D | Delete word after cursor |
| Ctrl+Y | Yank (paste) last kill |
| Meta+Y | Yank-pop (cycle kill ring after yank) |
| Ctrl+Z | Undo the last edit (typing is grouped by word; paste/kill undo as one step) |
| Ctrl+N / Down | History next (multi-line: next logical line, history at boundary) |
| Ctrl+P / Up | History previous (multi-line: previous logical line, history at boundary) |
| Ctrl+F | Reverse-i-search through history (bash-style): type to narrow, Ctrl+F to cycle, Enter to accept, Esc to cancel |
| Shift+Enter / Meta+Enter / Ctrl+J | Insert newline (input box expands; Ctrl+J works in any terminal) |
| Ctrl+G / `/edit` | Open the input buffer in `$EDITOR` (spawns temp file; replaces buffer on save) |
| Tab | Insert 2 spaces |
| `@<query>` | File picker (Tab/Enter select, Esc cancel) |
| Paste (≥4 lines) | Collapses to `[N lines pasted]`; re-paste same content to expand inline |

### Agent control

| Key | Action |
|-----|--------|
| Ctrl+C / Esc | Interrupt running agent (also clears queued interjections) |
| Type while running | Queues your message; runs after the current turn finishes. The runner also stops at the next tool-result boundary so the message is picked up quickly instead of waiting for the whole multi-turn run. Status line shows `q:N` for pending count. |
| Alt+X | Drop all queued interjections (without cancelling the running agent) |
| Ctrl+K | Kill subagent on focused chat tab |
| Ctrl+X | Close active chat window |
| Ctrl+N / Ctrl+P | Switch to next/previous chat window (when multiple subagent chats exist) |
| Esc-Esc (idle) | Open rewind picker (truncate history) |
| Ctrl+O | Toggle expand/collapse of the last collapsed block (tool result or thinking burst) |
| Ctrl+R | Toggle reasoning visibility |
| Shift+Tab | Cycle the active prompt layer to the next prompt (silent; status badge updates) |
| PgUp/PgDn | Scroll chat history |
| Ctrl+Home/End | Jump chat to top/bottom |
| Ctrl+L | Manual backstop for a redraw: re-enter the alternate screen, re-enable mouse capture + bracketed paste, and full-repaint. dirge already re-asserts these automatically when the window regains focus (the usual moment the terminal drops the alt screen — mouse wheel scrolls native scrollback, selection stops being captured), so this is rarely needed. |
| `! cmd` | Run shell command (visible, injected into chat) |
| `!! cmd` | Run shell command (invisible) |
| Mouse drag | Select text (copies to clipboard on release) |
| (input) | Live token count shown next to input bar (`N tk`) |

## Tool output display

| Feature | Detail |
|---------|--------|
| Tool results visible | Default on (`show_tool_details: true`), toggle in config |
| 4-line collapse | Tool result bodies default to the first 4 lines + a dim `↓ N more lines (Ctrl+O to expand)` footer. Configurable via `tool_result_max_lines` (default `4`). Exempt tools — body IS the value — render unchanged: `edit` (colorized diff), `read`, `question`, `task`, `task_status`. |
| Ctrl+O to toggle | Toggles the most-recent collapsed block — a tool/command result or a thinking burst — between collapsed and expanded. Expanding appends the full block at the bottom as a fresh chamber; a second press collapses (removes) it. The target resets on every new user prompt and on context-overflow auto-recovery. |
| Hard char cap | On top of the line cap, `tool_result_max_chars` (default `500`) trims a single pathological line so a 10 MB minified blob can't blow the chamber. |
| Colorized edit diffs | `edit` tool results render with `-` (red), `+` (green), `@@` (cyan) coloring (`show_edit_diff: true` in config) |

## Inline ASCII avatar

A 5-cell face lives in the left margin of the input row and reflects what the
agent is currently doing. Single-tick animation alternates between two poses
where applicable.

| State | Frames | Meaning |
|-------|--------|---------|
| **Idle** | `(o o)` / `(- -)` | Nothing happening — neutral blink |
| **Thinking** | `(o .)` / `(. o)` | Reasoning tokens streaming (eyes shifting) |
| **Speaking** | `(o o)` / `(o O)` | Regular tokens streaming (mouth opens) |
| **Reading** | `[@ @]` | `read` / `grep` / `find_files` / `list_dir` / `lsp` / `semantic` tool running |
| **Writing** | `(>_<)` / `(-_-)` | `write` / `edit` / `apply_patch` / `write_todo_list` tool running |
| **Bash** | `[$_$]` | `bash` shell command running |
| **Alert** | `(O_O)` | Permission prompt waiting on you — paints in the perm color |
| **Error** | `(x_x)` | Agent hit an error — paints in the error color |
| **Done** | `(^_^)` | Turn completed cleanly — paints in the accent color |

Unknown / plugin / MCP tools default to the `Reading` face since most are
observational. The avatar is purely informational — no functional dependence.

## Theme

dirge ships with an 80s-CRT phosphor green palette by default. To opt out, set
`"theme": "plain"` in `config.json` for the pre-theme white/cyan look:

```json
{ "theme": "plain" }
```

Errors stay red and warnings stay yellow under every theme — those colors are
part of the load-bearing semantic contract. For custom themes, see
[themes.md](themes.md).

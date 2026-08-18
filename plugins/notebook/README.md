# Notebook Plugin

A persistent Janet kernel the agent evaluates code against. State survives
between tool calls, so the model builds context up incrementally instead of
re-deriving it every call or writing throwaway scripts to disk.

## How it works

**The kernel is a second Janet VM.** dirge already runs one Janet VM for
plugins, hooks, slash commands and the `harness/block` permission gate. The
notebook gets its own, with a fresh root env and none of the host bridges —
no dialogs, no LSP, no DAP, no computer-use. Three things follow:

- A runaway cell cannot stall the permission gate or any plugin.
- Agent-authored code cannot redefine a hook or shadow a `harness/`
  function that the gate later calls.
- Recovering a wedged kernel does not destroy plugin state, and clearing
  plugin state does not destroy the notebook.

**Runaway cells are interrupted, not abandoned.** At the deadline the host
raises Janet's cross-thread interpreter interrupt. The cell stops, but the
VM keeps everything defined before it — which is the point, since a kernel
that loses its state on every bad cell is not a kernel. `(try …)` cannot
swallow the interrupt: it compiles to the error-only fiber mask, and the
interrupt arrives as a user signal.

The one case the interrupt cannot reach is a cell parked in a system call
(`os/execute` on a subprocess that never returns). That is what
`notebook_reset` with `scope="kernel"` is for — it restarts the VM, losing
notebook state but nothing else.

**Output is captured, not printed.** Janet's stdout is redirected to a
buffer, because in dirge's raw-mode TUI a bare `print` corrupts the screen.
Each cell resets the buffer, evaluates, and returns what was printed
alongside the value. Compile errors and stack traces are captured the same
way, so a broken cell reports instead of scribbling on the terminal.

**Sessions.** Cells evaluate into a per-session env created off the kernel
root, so two agents using the same variable name do not collide. `session`
defaults to `"main"`. `notebook/shared` is a table visible from every
session — the only state that crosses, and it has to be asked for.

**Delimiter repair.** Unbalanced `()`, `[]` and `{}` are closed before
eval, and the repair is reported rather than applied silently. The scanner
is Janet-aware: `#` starts a comment, `;` is the splice operator (not a
comment, as it would be in Clojure), and backtick long strings take no
escapes.

## Tools

| Tool | What it does |
| --- | --- |
| `notebook_eval` | Evaluate a cell. Args: `code`, optional `session`. |
| `notebook_reset` | Clear state. Args: optional `session`, optional `scope` (`session` \| `kernel`). |

A skill prompt is injected at session start showing the incremental-build
pattern. The tool existing is not enough — a model's default is to treat
every call as independent, which is exactly what this replaces.

## Limits

- **A cell is cut off after 20 seconds.** The bound sits under the plugin
  worker's own 30s budget for a tool handler; without it the outer eval
  would time out first and report the wrong VM as wedged. Long jobs belong
  in bash.
- **Cells queue behind the plugin worker.** The tool is delivered as a
  plugin, so a cell occupies the plugin VM's thread for its duration and
  concurrent hook dispatch waits, bounded by that 20s. The VM split fixes
  the *failure* case, not the queueing; removing the queueing entirely
  means making this a core tool.
- **Subagent isolation is by convention.** Nothing plumbs an agent
  identity to the tool layer yet, so distinct sessions depend on the model
  passing distinct `session` values as the description asks.
- **`mcp__dirge__delegate` sessions are separate processes** and do not
  share the kernel. Only in-process subagents do.

## Files

- `00-repair.janet` — Janet-aware delimiter repair
- `01-tools.janet` — `notebook_eval` / `notebook_reset` registration
- `02-hooks.janet` — skill prompt injected via `before-agent-start`

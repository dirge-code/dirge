# Permission ask on a `bash` write

The agent runs a `bash` mutation in standard permission mode. The
checker walks its ordered rule list; if the resolved action is `Ask`,
the UI prompts. `Allow once`, `Allow always`, and `Deny` map to the
three `UserDecision` variants. A deny rule (`external_directory`)
short-circuits before the prompt fires.

## Flow

1. Agent invokes `bash` with `rm /tmp/oldlogs/*.log`.
2. Checker resolves to `Ask`. UI renders a prompt box:
   `[a]llow once  [A]llow always  [d]eny`.
3. User presses `a`. The command runs; the session allowlist is not
   updated; the doom-loop counter increments.
4. Agent later runs `rm /tmp/oldlogs/server.log`. Different input
   string, so the allowlist entry from a hypothetical `Allow always`
   would not match — fresh prompt.
5. Agent tries `rm /etc/hosts`. The `external_directory: { "/etc/**":
   "deny" }` rule fires inside `check_path`. No prompt; the tool
   returns an error which the LLM sees as a tool result.

## Implementation

- `src/agent/tools/bash/mod.rs::BashTool::call` — splits the command into
  segments and submits each to `check_bash_segments`.
- `src/agent/tools/bash/check.rs::check_bash_segments` — calls `enforce`
  per segment and per extracted mutation path.
- `src/agent/tools/mod.rs::enforce` — public entry point; runs
  `PermissionChecker::check` / `check_path` and routes `Ask`
  outcomes through `handle_ask_inner`.
- `src/permission/checker.rs::PermissionChecker::check` — ordered
  rule evaluation (prompt deny-list, MCP concrete-name deny, yolo
  short-circuit, session allowlist, per-tool rules,
  `external_directory`, mode defaults, doom-loop check).
- `src/permission/checker.rs::check_path` — applies
  `external_directory` patterns to path-shaped inputs.
- `src/permission/checker.rs::track_doom_loop` — per-key counter
  via `repeat_counts`.
- `src/permission/ask.rs` — `AskRequest`, `UserDecision`, key bindings.
- `src/permission/engine/classify.rs` — high-risk-tool list that Accept mode
  still asks for (bash, webfetch, task, memory, skill, apply_patch).

## Edge cases

- `accept-all` mode: deny rules still fire — Accept only coerces
  `Ask` → `Allow` inside cwd.
- `yolo` mode: skips all rules except prompt deny-list and the
  active-prompt `deny_tools` frontmatter.
- Symlink swap between check and open: `check_perm_path_resolve`
  canonicalizes the path and the tool opens the canonical form;
  the cwd is also re-canonicalized on each check.
- Doom-loop: same `tool + input` key counted in a 32-call window;
  after 3 identical calls the `doom_loop_action` (default `Ask`)
  overrides.

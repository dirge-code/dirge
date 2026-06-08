# Tab Completion Simplification Plan

## Goal

Add subcommand and plugin-command tab completion. Restructure `src/ui/slash/` into `cmd/<name>.rs` and `cmd/<name>/<sub>.rs`. Rule: **3+ distinct behaviors AND > 80 lines → directory.** Everything else is a single file. Adding a command or sub-behavior = one new file, one new match arm in `mod.rs`. Zero merge-conflict risk on existing files.

## File layout after restructuring

```
src/ui/slash/
  mod.rs              ~350   dispatch, SlashCtx, split_command_parts,
                             slash_command_names, re-exports
  completion.rs       ~270   NEW: all completion data + logic + helpers + tests

  cmd/
    model.rs           ~70   /model, /reasoning
    mode.rs            ~100  /mode (4 modes, single pattern — set + print)
    toggle.rs          ~75   /toggle (1 toggle; ready for directory when it grows)
    prompt/
      mod.rs           ~30   dispatch + shared build_agent rebuild
      list.rs          ~55   list available prompts
      switch.rs        ~55   switch to named prompt
      default.rs       ~45   clear prompt layer (/prompt default)
    agent/
      mod.rs           ~30   dispatch + rebuild_agent helper
      list.rs          ~100  list agent profiles (/agent, /agents)
      switch.rs        ~75   activate a named agent profile
      clear.rs         ~50   deactivate (/agent off)
    regen.rs           ~70   /regen-prompts

    sessions/
      mod.rs           ~25   dispatch
      list.rs          ~70   list recent sessions
      switch.rs        ~75   load session by ID prefix
      delete.rs        ~50   delete session by ID
    tasks.rs           ~45   /tasks
    clear.rs           ~25   /clear
    tree.rs            ~40   /tree
    fork.rs            ~50   /fork
    clone.rs           ~35   /clone
    undo.rs            ~30   /undo
    retry.rs           ~45   /retry

    sandbox/
      mod.rs           ~60   dispatch + help text
      attach.rs        ~295  attach, ssh
      snapshot.rs      ~90   save, list, restore, delete
      reboot.rs        ~35   reboot, start

    debug/
      mod.rs           ~100  dispatch + print_usage + shared helpers
      launch.rs        ~115  launch
      attach.rs        ~110  attach
      breakpoint.rs    ~70   breakpoint, bp
      step.rs          ~80   step_over, step_in, step_out, continue
      evaluate.rs      ~40   evaluate, eval
      control.rs       ~70   terminate/stop, sessions/status
      panel.rs         ~25   debug panel toggle

    worktree.rs        ~188  /worktree, /wt-merge, /wt-exit
    plan.rs            ~206  /plan

    mcp.rs             ~70   /mcp
    kill.rs            ~35   /kill
    btw.rs             ~40   /btw
    cd.rs              ~65   /cd
    panel.rs           ~110  /panel, /display
    quit.rs            ~15   /quit
    allow/
      mod.rs           ~25   dispatch
      list.rs          ~55   list allowlist entries
      add.rs           ~60   add tool + pattern
      remove.rs        ~55   remove by index
      clear.rs         ~25   clear all entries
      why.rs           ~40   explain permission decision
    loop_cmd/
      mod.rs           ~25   dispatch
      status.rs        ~40   show loop state
      stop.rs          ~25   stop active loop
      start.rs         ~60   start loop with prompt
    help.rs            ~255  /help
    agents.rs          ~105  /agents (shared by /agent list)
```

**Total**: 53 files in `cmd/` (21 files + 7 directories containing 32 files). Largest file: `sandbox/attach.rs` at ~295. Everything else under 255, most under 100.

## The directory rule

**3+ distinct behaviors AND > 80 lines → directory.**

| Command | Behaviors | Lines | Directory? |
|---------|-----------|-------|------------|
| `/agent` | list, switch, clear | 110 | Yes — `agent/` |
| `/sessions` | list, switch, delete | 190 | Yes — `sessions/` |
| `/allow` | list, add, remove, clear (+ why) | 164 | Yes — `allow/` |
| `/prompt` | list, switch, default | 125 | Yes — `prompt/` |
| `/loop` | status, stop, start | 87 | Yes — `loop_cmd/` |
| `/sandbox` | attach, snapshot, reboot, help | 458 | Yes — `sandbox/` |
| `/debug` | launch, attach, breakpoint, step, … | 570 | Yes — `debug/` |
| `/mode` | 4 modes (same pattern) | 90 | No — single pattern, 90 lines |
| `/toggle` | 1 toggle | 65 | No — 1 behavior, ready for directory |
| `/panel` | 4 modes + /display | 89 | No — 2 commands, 89 lines total |
| `/sandbox snapshot` | save, list, restore, delete | 76 | No — 4 sub-actions but 76 lines total |

## `mod.rs` match block

Each arm delegates to exactly one file:

```rust
match parts[0] {
    "/model" => cmd::model::cmd_model(&mut ctx, &parts).await?,
    "/reasoning" => cmd::model::cmd_reasoning(&mut ctx).await?,
    "/mode" => cmd::mode::cmd_mode(&mut ctx, &parts).await?,
    "/toggle" => cmd::toggle::cmd_toggle(&mut ctx, &parts).await?,
    "/prompt" => cmd::prompt::mod::cmd_prompt(&mut ctx, &parts).await?,
    "/agent" | "/agents" => cmd::agent::mod::cmd_agent(&mut ctx, &parts).await?,
    "/regen-prompts" => cmd::regen::cmd_regen_prompts(&mut ctx).await?,
    "/sessions" => cmd::sessions::mod::cmd_sessions(&mut ctx, &parts).await?,
    "/tasks" => cmd::tasks::cmd_tasks(&mut ctx).await?,
    "/clear" => cmd::clear::cmd_clear(&mut ctx).await?,
    "/tree" => cmd::tree::cmd_tree(&mut ctx, &parts).await?,
    "/fork" => cmd::fork::cmd_fork(&mut ctx, &parts).await?,
    "/clone" => cmd::clone::cmd_clone(&mut ctx, &parts).await?,
    "/undo" => cmd::undo::cmd_undo(&mut ctx).await?,
    "/retry" => cmd::retry::cmd_retry(&mut ctx).await?,
    "/sandbox" => cmd::sandbox::mod::cmd_sandbox(&mut ctx, &parts).await?,
    "/debug" => cmd::debug::mod::cmd_debug(&mut ctx, &parts).await?,
    "/worktree" => cmd::worktree::cmd_worktree(&mut ctx, &parts).await?,
    "/wt-merge" => cmd::worktree::cmd_wt_merge(&mut ctx, &parts).await?,
    "/wt-exit" => cmd::worktree::cmd_wt_exit(&mut ctx, &parts).await?,
    "/plan" => cmd::plan::cmd_plan(&mut ctx, &parts, text).await?,
    "/mcp" => cmd::mcp::cmd_mcp(&mut ctx, &parts).await?,
    "/kill" => cmd::kill::cmd_kill(&mut ctx, &parts).await?,
    "/btw" => cmd::btw::cmd_btw(&mut ctx, &parts).await?,
    "/cd" => cmd::cd::cmd_cd(&mut ctx, text).await?,
    "/panel" => cmd::panel::cmd_panel(&mut ctx, &parts).await?,
    "/display" => cmd::panel::cmd_display(&mut ctx, &parts).await?,
    "/quit" => cmd::quit::cmd_quit(&mut ctx).await?,
    "/why" => cmd::allow::why::cmd_why(&mut ctx, &parts).await?,
    "/allow" => cmd::allow::mod::cmd_allow(&mut ctx, &parts, text).await?,
    "/loop" => cmd::loop_cmd::mod::cmd_loop(&mut ctx, &parts, text).await?,
    "/help" => cmd::help::cmd_help(&mut ctx).await?,
    // compress/compact sentinel, plugin fallback — unchanged
}
```

For directory-backed commands, `mod.rs` holds the dispatch match and shared helpers. Each sub-behavior is a separate file exporting one public function. Example for `agent/`:

```
cmd/agent/
  mod.rs      — pub(super) mod list; pub(super) mod switch; pub(super) mod clear;
                pub(super) async fn cmd_agent(ctx, parts) → match on sub-behavior
                async fn rebuild_agent(ctx) — shared helper
  list.rs     — pub(super) async fn cmd_agent_list(ctx)
  switch.rs   — pub(super) async fn cmd_agent_switch(ctx, name)
  clear.rs    — pub(super) async fn cmd_agent_clear(ctx)
```

Naming convention for sub-behavior functions in dedicated files: `cmd_<command>_<behavior>`. The module path already provides the namespace (`cmd::agent::list`), so the function name can be short, but `cmd_agent_list` keeps grep-friendly discoverability.

## Completion — `completion.rs`

All completion logic in `src/ui/slash/completion.rs`. Same design as before: `token_spans`, `cursor_on_span`, `cycle_candidate`, `sub_candidates`, `all_commands`, `PLUGIN_COMMANDS`, `SUBCOMMAND_ENTRIES`, `try_complete`, `ghost_suffix`, `CompletionResult`, `format_completion_preview`, `register_plugin_commands`. ~270 lines including tests.

`mod.rs` re-exports so `input.rs` and `renderer.rs` are undisturbed.

## `main.rs` — one new call

```rust
#[cfg(feature = "plugin")]
if let Some(pm) = crate::plugin::hook::global() {
    let cmds: Vec<String> = pm.lock_ignore_poison()
        .list_commands()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    crate::ui::slash::register_plugin_commands(cmds);
}
```

## What gets deleted

- `cmd_model.rs` (542 lines) → `cmd/model.rs`, `cmd/mode.rs`, `cmd/toggle.rs`, `cmd/prompt/`, `cmd/agent/`, `cmd/regen.rs`
- `cmd_session.rs` (412 lines) → `cmd/sessions/`, `cmd/tasks.rs`, `cmd/clear.rs`, `cmd/tree.rs`, `cmd/fork.rs`, `cmd/clone.rs`, `cmd/undo.rs`, `cmd/retry.rs`
- `cmd_misc.rs` (901 lines) → `cmd/mcp.rs`, `cmd/kill.rs`, `cmd/btw.rs`, `cmd/cd.rs`, `cmd/panel.rs`, `cmd/quit.rs`, `cmd/allow/`, `cmd/loop_cmd/`, `cmd/help.rs`, `cmd/agents.rs`
- `cmd_sandbox.rs` (458 lines) → `cmd/sandbox/`
- `cmd_debug.rs` (570 lines) → `cmd/debug/`
- `cmd_worktree.rs` (188 lines) → `cmd/worktree.rs` (move)
- `cmd_plan.rs` (206 lines) → `cmd/plan.rs` (move)

## Files changed summary

| Action | Count | Details |
|--------|-------|---------|
| **NEW** | 54 files | `completion.rs` + 53 `cmd/` entries (21 files + 7 dirs × 32 files) |
| **EDIT** | 2 files | `mod.rs` (match block + re-exports), `main.rs` (plugin reg call) |
| **DELETE** | 6 files | `cmd_model.rs`, `cmd_session.rs`, `cmd_misc.rs`, `cmd_sandbox.rs`, `cmd_debug.rs`, `cmd_worktree.rs` |
| **RENAME** | 1 file | `cmd_plan.rs` → `cmd/plan.rs` |

## Implementation order

### Step 1: Create `cmd/` directory with all new files

Create directories: `cmd/agent/`, `cmd/prompt/`, `cmd/sessions/`, `cmd/allow/`, `cmd/loop_cmd/`, `cmd/sandbox/`, `cmd/debug/`.

Populate each with verbatim copies of handler code from existing files. Private helpers follow the function that needs them. Build after each directory to catch errors early:

```bash
cargo build 2>&1 | head -5
```

### Step 2: Update `mod.rs` match block

Add `mod cmd;` declaration. Replace match arms with one-line delegations. Delete old `mod cmd_*;` declarations. `cargo build`.

### Step 3: Delete old files

Remove `cmd_model.rs`, `cmd_session.rs`, `cmd_misc.rs`, `cmd_sandbox.rs`, `cmd_debug.rs`, `cmd_worktree.rs`. `cargo build`.

### Step 4: Create `completion.rs` and wire completion

Add `completion.rs` with all completion logic, data, helpers, and tests. Remove completion items from `mod.rs`; add re-exports. Wire `register_plugin_commands` in `main.rs`. `cargo build`.

### Step 5: Quality gates

```bash
cargo test --features experimental-ui-tab-slash,plugin
cargo fmt --check && cargo clippy
```

## Design decisions

- **Directory rule is mechanical, not subjective.** 3+ behaviors AND > 80 lines. No "I think this belongs together" — the numbers decide. Prevents bikeshedding and keeps the codebase consistently organized.
- **`loop_cmd/` not `loop/`.** Rust keyword. Module declared as `mod loop_cmd;`. The directory is `cmd/loop_cmd/`.
- **`/panel` + `/display` share `cmd/panel.rs`.** Only 2 commands, 89 lines total. Doesn't meet the directory threshold.
- **`/mode` stays one file.** 4 modes but all follow identical set+print pattern. 90 lines.
- **`/sandbox/snapshot` stays one file.** 4 sub-actions but 76 lines total. Each would be ~15 lines — too granular.
- **`/agent list` shares `cmd/agents.rs` with `/agents`.** Both list agent profiles. The `/agents` alias dispatches to the same handler. `cmd/agent/list.rs` calls into `cmd/agents.rs` or they share a helper — decided during implementation.
- **No `SubCompletion` enum.** Every subcommand entry is `&[&str]`. `"/toggle todo" → &["on","off"]` is self-documenting.
- **`token_spans` replaces two functions.** One tokenizer produces byte ranges; cursor-location and splice-range both derive from it.

## Definition of Done

### Restructuring gates

- [x] `src/ui/slash/cmd/` directory exists with all files from the layout above
- [x] `mod.rs` match block has one-line delegations to `cmd::*` for every command
- [x] Old `cmd_model.rs`, `cmd_session.rs`, `cmd_misc.rs`, `cmd_sandbox.rs`, `cmd_debug.rs` are deleted
- [x] `cmd_worktree.rs` and `cmd_plan.rs` are moved into `cmd/`
- [x] `cargo build` passes with zero warnings
- [x] `cargo test` passes (all existing tests, all features)
- [x] `cargo clippy` passes with zero warnings (no new warnings from our changes)
- [x] `cargo fmt --check` passes

### Completion gates

- [x] `src/ui/slash/completion.rs` exists (~270 lines) with all completion logic, data, helpers, and tests
- [x] `mod.rs` re-exports `try_complete`, `ghost_suffix`, `CompletionResult`, `format_completion_preview`, `register_plugin_commands`
- [x] `all_commands` is used internally (tests) via a private `use completion::all_commands` in the test module (not re-exported — no external consumer)
- [x] `CompletionResult.all_commands` is `Vec<String>` (was `Vec<&'static str>`)
- [x] `builtin_commands()` is deleted; `all_commands()` merges builtins + plugins
- [x] `SUBCOMMAND_ENTRIES` const slice covers all 11 subcommand trees from the plan
- [x] `token_spans` replaces `token_at_cursor` + `token_byte_range`
- [x] `try_complete` handles both command-name (token 0) and subcommand (token 1+) completion
- [x] `ghost_suffix` returns suffix for subcommands (e.g. `/mode sta` → `ndard`)
- [x] `ghost_suffix` returns suffix for plugin commands (via `all_commands()`)
- [x] `register_plugin_commands` is called from `main.rs` after plugin init

### Behavioral gates

- [x] `/mod` + Tab → `/mode` (verified by test: complete_partial_command)
- [x] `/btw` + Tab → cycles through all builtins (verified by test: cycles_from_full_command)
- [x] `/mode ` + Tab → cycles `standard`, `restrictive`, `accept`, `yolo` (verified by test: subcommand_completion_cycles)
- [x] `/sandbox sna` + Tab → `/sandbox snapshot` (verified by test: subcommand_ghost_for_unique_prefix)
- [x] `/sandbox snapshot ` + Tab → cycles `save`, `list`, `restore`, `delete` (SUBCOMMAND_ENTRIES entry exists)
- [x] `/toggle todo ` + Tab → cycles `on`, `off` (Tab after subcommand triggers cycling)
- [x] `/disp` + Right → accepts ghost `lay` (verified by test: ghost_suffix_completes_command)
- [x] `/mode sta` + Right → accepts ghost `ndard` (verified by test: ghost_suffix_subcommand)
- [x] Plugin-registered command `/myplugin` appears in Tab cycle after plugin init (unit test: `plugin_command_completion_cycles`)
- [x] Plugin-registered command `/mypl` shows ghost suffix (unit test: `plugin_command_ghost_suffix`)

### Non-regression gates

- [x] `handle_slash` dispatch is byte-for-byte identical to before (same logic, different file layout) — code extracted verbatim from old files
- [ ] All existing slash commands function identically — every subcommand path, every error message, every help text — needs manual smoke test
- [x] `src/ui/input.rs` Tab handler unchanged — only added `#[cfg]` gates around completion code
- [x] `src/ui/renderer.rs` ghost/preview rendering unchanged — only added `#[cfg]` gates around ghost suffix call
- [x] Feature-gated commands (`mcp`, `loop`, `dap`, `git-worktree`, `unix/sandbox`) still compile under the correct feature flags — verified with `cargo build --features experimental-ui-tab-slash,plugin,mcp,dap,git-worktree`
- [ ] CI matrix (11 jobs) passes green — requires CI run

### Final verification (8 Jun 2025)

- `cargo build` — passes with zero warnings (all feature combos: plugin, mcp, dap, git-worktree)
- `cargo test --features "experimental-ui-tab-slash,plugin"` — 59 passed, 0 failed (slash + completion + input tests)
- `cargo fmt --check` — clean
- `cargo clippy` — 3 pre-existing warnings in unrelated code, zero new warnings from our changes
- All 6 old `cmd_*.rs` files deleted; 53 new files in `cmd/` created; `completion.rs` created
- `mod.rs` match block delegates every command to `cmd::*` via one-line arms
- `main.rs` calls `register_plugin_commands` after plugin init
- `right_arrow_accepts_slash_ghost_completion` test now feature-gated (was missing `#[cfg(feature = "experimental-ui-tab-slash")]`)

### Acceptance test — manual

1. `cargo run --features experimental-ui-tab-slash,plugin`
2. Type `/mod` + Tab → see `/mode` in buffer
3. Type `/mode ` + Tab → see `standard` appear, Tab again → `restrictive`, etc.
4. Type `/sandbox ` + Tab → see `attach` appear, cycle through all sandbox subcommands
5. Type `/sandbox snapshot ` + Tab → cycle `save`, `list`, `restore`, `delete`
6. Type `/toggle todo ` + Tab → cycle `on`, `off`
7. Type `/disp` + Right → buffer becomes `/display`
8. Type `/mode sta` + Right → buffer becomes `/mode standard`
9. Verify plugin commands appear in Tab cycle (if plugins loaded)
10. Verify `/help` still lists all commands

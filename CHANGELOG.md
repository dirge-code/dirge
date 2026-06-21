# Changelog

All notable changes to dirge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.10.4] - 2026-06-21

### Added
- **Debug a Python module, not just a file.** The DAP launch path now takes an
  optional `module`, so a debuggee can start as `python -m <module>` (e.g.
  `pytest`) instead of by program path — exposed via the `dap/launch-module`
  Janet binding and the debug slash command. The `debug` tool's launch args also
  gained an `env` map for per-launch environment variables. CI runs the new
  smoke/e2e tests under a `dap` feature-matrix entry. (#497)

## [0.10.3] - 2026-06-21

### Added
- **`/model` lists the models your config pins.** With no argument it now prints
  every model set across your `providers`, marks the active one, and tells you to
  switch with `/model <id>` — previously it only echoed the current model with
  nothing to pick from. (#495, issue #492)

### Changed
- **`Ctrl+D` is forward-delete in the input editor**, not a hard exit. It deletes
  the character under the cursor (rebindable as `delete_char_forward`); `Ctrl+C`
  and `Esc` remain the interrupt/exit gestures. (#493)
- **The context gauge reads 0–100%.** Its denominator is now the full effective
  window instead of the fold-trigger budget (~75% of it), so real usage past 75%
  no longer showed a confusing `>100%` (e.g. `90k/75k (120%)`). A compact
  `fold`/`fold!` marker flags when a fold is near/imminent. (dirge-l4rp, dirge-cx7t)

### Fixed
- **Auto-repair no longer silently mangles files.** Delimiter auto-close now only
  fixes a genuine *trailing* truncation; a mid-file imbalance that would swallow
  the following code is rejected and bounced back to the model instead of being
  "repaired" into valid-but-wrong code. (dirge-a0nl)
- **Auto-repairs are verified by the language server and rolled back when wrong.**
  After an auto-close, `write`/`edit` ask the LSP whether the result actually
  holds up; on error-severity diagnostics the change is reverted to its pre-write
  state and the model gets the diagnostics to fix its original text. A rollback
  that can't snapshot the prior content (e.g. an unreadable existing file) now
  leaves the file in place rather than deleting it. (dirge-p1ws, #501)
- **Nix release job no longer fails on every tag.** The `bin.nix` version bump is
  committed straight to `main` instead of trying to open a PR (which GitHub
  Actions can't do with the default token). (#491)

## [0.10.2] - 2026-06-20

### Added
- **Cycle prompts with Shift+Tab.** A hotkey (rebindable as `cycle_prompt`)
  steps through the configured prompt layers like a mode switcher, and now
  cycles back to the no-prompt base layer past the last one. (#485, #489)

### Fixed
- **Critic / verifier / todo nudge no longer vanishes from the log.** These
  finalization nudges re-enter as user-role messages without a Done/ToolCall to
  reset the stream anchor, so the next turn's render overwrote them — they
  disappeared on screen a moment later even though the model still had them in
  context. The in-flight response is now finalized before the nudge renders, so
  the next turn streams below it. (#488)

## [0.10.1] - 2026-06-20

### Added
- **Undo in the input editor.** `Ctrl+Z` reverts the last edit — a paste, a
  kill, or a run of typing (grouped by word). State resets on submit and
  `/fork`. Rebindable as the `undo` command.
- **Verification-aware finalization.** The in-loop critic now sees whether the
  run actually built/tested its code changes and nudges on an unverified or red
  change — with an explicit "nothing to run / not testable → fine" escape so it
  never forces a test that can't run. The goal gate gets the same signal as a
  soft advisory that can't trap the bounded loop. Active only when a critic
  provider is configured. (#484)
- **Experimental computer-use plugin.** Off by default behind the
  `experimental-ui-computer-use` build feature; intercepts `bash` commands
  prefixed `computer:` to drive a desktop (screenshot, type, click, navigate)
  via ydotool/xdotool plus a local vision backend. Gated by a confirm dialog,
  an explicit host-control opt-in, and a sandboxed-desktop image. (#415)

### Fixed
- **Copy wrapped prose as one line.** Selecting chat text the renderer
  soft-wrapped across rows no longer pastes a newline at every wrap point;
  real line breaks, paragraph breaks, and blank lines are preserved.
- **macOS (Apple Silicon) build.** The computer-use plugin used x86_64-only
  Janet FFI (`janet_wrap_integer`, raw union `.pointer` access), which broke the
  default plugin build on aarch64 — invisible to the Linux-only CI. Now uses
  portable janetrs access. (#483)
- **Shell injection in computer-use `focus`.** The model-controlled app name is
  validated against a safe character set before reaching `pgrep` / the vision
  call. (#482)

## [0.10.0] - 2026-06-20

### Added
- **Configurable key bindings everywhere.** One `keybindings` array now rebinds
  both the global command keys (scroll, chat nav, …) and the input-editor keys
  (cursor/word motion, kill-ring, history, …) — the text-box keys used to be
  hardcoded. Built-in defaults are declarative tables your config merges over;
  see [docs/config.md](docs/config.md#key-bindings) for the full command list.
  (#477)
- **Emacs-style chord sequences.** A binding key may be a sequence like
  `ctrl-x ctrl-s`; the footer shows the pending prefix and **Esc**/**Ctrl+G**
  cancels it. Optional `chord_timeout_ms` auto-cancels a pending prefix after a
  set idle time (default: wait indefinitely). (#477, #478, issue #234)
- **Plugins can remap built-in bindings.** `(harness/bind-key keys command)`
  binds a chord (or sequence) to a built-in command, or `"none"` to unbind one,
  merged under your config (defaults < plugin < user). `register-shortcut`
  (bind a key to plugin code) is unchanged. (#477, issue #476)
- **Visual-line cursor motion.** `Ctrl+A`/`Ctrl+E`, `Home`/`End`, and the
  `Ctrl+U` kill now act on the current soft-wrapped line rather than the whole
  buffer. (#474)

### Changed
- **`approval_provider` denials are advisory.** When an LLM approval evaluator
  denies a tool call, it now escalates to the normal permission prompt (showing
  *why* it was flagged) instead of hard-failing — so you can still allow it.
  It's terminal only in non-interactive mode. (#475)
- **Permission denials aren't treated as fixable failures.** The recovery
  checkpoint no longer tells the model to "try a different approach" after a
  permission block (which pushed it to route around the guardrail), and the
  critic treats a permission-denied capability as out of scope rather than
  unfinished work. (#475)

### Fixed
- **`/sessions delete` works when ids collide.** Compacted sessions are named
  `compacted-<uuid>`, so every one rendered as the identical 8-char stub
  "compacte" and "be more specific" was impossible. The list/switch/delete
  views now show ids at the shortest length that keeps them distinct, and never
  cut the leading marker mid-word. (#478)

## [0.9.1] - 2026-06-20

### Changed
- **`/sessions` uses explicit verbs.** `/sessions list | switch <id> | delete <id>`,
  with bare `/sessions <id>` still switching as a shortcut. The first argument no
  longer does double duty as both a `delete` sentinel and a session id, so a
  session can no longer shadow a subcommand. (dirge-aqi3)
- **The footer token budget reflects the compaction point.** The status line now
  measures usage against the budget where auto-compaction kicks in
  (`fold_threshold × min(model_window, context_target)`) instead of the raw
  advertised model window, so the percentage tracks how close the next fold is —
  at 100% a fold is imminent. (dirge-l4rp)

### Fixed
- **Deleting the current session no longer leaves a zombie.** Deleting the session
  you're in removed its file but left the in-memory session pointing at it; you're
  now booted into a fresh session (new id, same model/provider/cwd) with the agent
  rebuilt. (dirge-0cvk)
- **The goal gate no longer acts on a stale compaction summary.** After a resume,
  the merged system prompt carries a `[CONTEXT COMPACTION — REFERENCE ONLY]`
  summary whose `## Active Task` describes already-completed work; the goal judge
  now strips it (matching the critic), so it won't re-demand superseded work. It
  also no longer risks a panic truncating a multi-byte constraints block. (dirge-wp0e)
- **Pasting while answering a question no longer leaks into the main prompt.**
  When typing a free-form custom answer in the `question` modal, a paste went
  to the compose editor instead of the answer field — the modal dispatcher only
  routed key events, so pastes fell through. Pastes now land in the active
  answer (newlines flattened to spaces) and are swallowed for single-key
  modals. (dirge-7543)

## [0.9.0] - 2026-06-19

### Added
- **`show_reasoning` config flag.** Set it to `true` to make the model's
  thinking visible by default instead of pressing `Ctrl+O` each turn. Defaults
  to `false` (unchanged behavior); `Ctrl+O` still toggles per session. (#461)
- **Nix flake.** `nix build` / `nix run` build dirge from source, `nix develop`
  opens a Rust dev shell, and `nix build .#dirge-bin` installs the prebuilt
  release binary. Ships an overlay and `.envrc` for direnv, and supports
  x86_64/aarch64 on both Linux and macOS. A release-tag workflow refreshes
  `nix/bin.nix` hashes and opens a PR. (#462, #466)

### Fixed
- **Compaction no longer 400s under Anthropic OAuth.** dirge injects
  `role:"system"` entries into `messages[]` for compaction summaries and
  mid-session memory re-injection. The OAuth shaper only normalized the
  top-level `system` field, so a stray system turn reached the wire and the
  Claude-Code classifier rejected it as third-party traffic ("extra usage" 400)
  right after every compaction. System-role `messages[]` entries are now folded
  into the top-level `system` block first. (#463)
- **Global-tier skills are advertised in the system prompt.** The skill catalog
  listed only the project-local `.dirge/skills/`, while the `skill` tool could
  load from the global tiers too — so a globally installed skill was loadable
  but never advertised. The catalog now lists from the same source as the tool
  (`discover_skills`). (#464)

## [0.8.1] - 2026-06-18

### Fixed
- **Interactive prompts fail fast instead of hanging to the timeout.** A
  `git clone` that prompted for a username blocked for the full 120s bash
  timeout: git reads credentials from `/dev/tty`, not stdin, and in the Off
  sandbox the child shared dirge's controlling terminal. The bash child now
  runs in its own session (`setsid`) with no controlling terminal, so the
  prompt errors out immediately; `GIT_TERMINAL_PROMPT=0` and friends cover the
  non-tty askpass paths. (#460)

### Changed
- **The loop guard is cost-aware.** The repeat (storm) and failure-streak
  guards were count-based, so a command that burned its whole timeout counted
  the same as a millisecond error and neither escalated. Each result is now
  classified once (ok/error/timeout) and fed to both: a timeout weighs double
  toward the recovery-checkpoint nudge, and an identical retry of a timed-out
  command is suppressed one attempt sooner. (#460)

## [0.8.0] - 2026-06-19

### Fixed
- **Expanded thinking block keeps its box on wrapped lines.** In the Ctrl+O
  thinking panel, a long thought's continuation rows dropped the `│` bar and
  started at the left edge, so the text escaped the bounding box. Each line now
  wraps with the bar carried onto every row. (#459)

## [0.7.9] - 2026-06-19

### Added
- **Anthropic Claude Code OAuth.** `dirge auth anthropic` runs a PKCE
  loopback login against a Claude Pro/Max subscription and stores the token at
  `~/.claude/.credentials.json` (Claude Code-compatible), refreshed on expiry.
  Set the Anthropic provider's `auth` to `anthropic` / `claude-code` to use it.
  (#452, #454)
- **OpenAI / ChatGPT OAuth.** `dirge auth openai` (alias `dirge auth chatgpt`)
  runs OpenAI's device-code login for a ChatGPT subscription and stores the
  token in dirge's own credential file; the `auth: chatgpt` mode also still
  reads an existing `~/.codex/auth.json`. Requires enabling device-code auth in
  ChatGPT Codex security settings. (#455)
- **`/memory reload`** refreshes the frozen memory snapshot mid-session without
  restarting. (#435)

### Fixed
- **Long lines in fenced code blocks wrap instead of clipping.** The chat
  painter draws one row per buffer line and clips to width; code rows weren't
  pre-wrapped, so a long line inside a ``` block was cut off at the window
  edge. They now wrap like prose. (#453)
- **Anthropic OAuth credentials are written atomically.** The persist path used
  a non-atomic truncating write with a brief world-readable window before
  permissions were tightened; it now uses the same atomic 0600 write as the
  OpenAI store. (#457)
- **Manifest version restored to match the release tag.** #454 branched from a
  pre-0.7.8 base and regressed `Cargo.toml`/`Cargo.lock` to 0.7.7; bumped back
  so the tree matches the `v0.7.8` tag. (#456)

### Changed
- **Shared OAuth credential I/O across the Anthropic and OpenAI paths** (atomic
  0600 write, expiry check, account-id alias extraction) and unified the
  `dirge auth` dispatch through one config-free path. The two login *flows*
  (loopback vs device-code) stay provider-specific. (#457)

## [0.7.8] - 2026-06-18

### Added
- **Memory improvements.** Procedural playbooks now rank on measured
  effectiveness rather than recency, so a play that keeps working surfaces
  ahead of one that was merely used recently (#436). A confidence axis with
  contradiction-driven supersession lets a newer fact retire a stale one
  instead of both lingering (#437). An opt-in hybrid retrieval provider fuses
  dense embeddings with BM25 via reciprocal-rank fusion (#439). Verbatim
  pre-recall surfaces exact prior snippets as supplemental context without
  disturbing the frozen system-prompt snapshot (#440). Mark/supersede is gated
  to the background review runner so it can't stall the main loop (#441).
  (#442, #445)
- **Retrieval eval harnesses.** A Recall@K harness for the memory retriever
  (#438) and a compaction-recall harness that probes whether load-bearing
  facts survive a fold (#434).
- **Live thinking streams into the Ctrl+O panel.** Expanding an in-progress
  reasoning burst now updates in place as new tokens arrive, instead of
  freezing the snapshot you first opened (#444).

### Fixed
- **Compaction no longer resurrects finished tasks (#443).** After a fold the
  model could re-derive the original request as if still pending when it was
  already done and the live work had moved to a follow-up. The summary's
  `## Active Task` now describes the immediate in-flight work and marks the
  original complete, and the summary preamble warns not to redo finished work.
- **Blockquotes render the `│` bar on every line (#446).** The bar code ran
  after the paragraph had already flushed, so it was dead and multi-line
  quotes rendered as bar-less dim prose.
- **Live-thinking expansion hardening (#449, #450).** The in-place re-render
  no longer truncates content that scrolled below the block, the expansion
  state resets across chat switches, and the per-delta re-render is coalesced
  so a long reasoning burst is no longer O(n²). Quoted headings and code
  blocks now carry the quote bar too.

### Docs
- Documented how the context/compaction fold point is computed
  (`compaction_fold_threshold` × `min(model_window, context_target)`) (#447).

## [0.7.7] - 2026-06-17

### Added
- **ChatGPT/Codex authentication.** Run `codex login`, then `dirge` with the
  `openai` provider and `auth: chatgpt` — dirge reads the Codex bearer token
  from `~/.codex/auth.json` (or `$CODEX_HOME/auth.json` / `CODEX_ACCESS_TOKEN`)
  and talks to the Codex backend through a small request shim that adapts the
  `/responses` body shape. The token is sent only to the Codex endpoint over
  https, never logged, and ChatGPT auth is refused for any non-`openai`
  provider so the token can't leak to a third party. (#428, #433)
- **Skills discovered under `.agents/skills/` too**, alongside `.claude`,
  `.opencode`, and `.dirge`, at both home and per-project scope. (#432)

## [0.7.6] - 2026-06-17

### Fixed
- **Destructive git commands no longer run without a prompt (#429).**
  `git checkout`, `git switch`, and `git restore` were on the default
  auto-allow list, so they executed silently anywhere the `bash` tool was
  reachable — and they discard uncommitted work. An agent reverting its own
  edit with `git checkout -- file` could wipe a user's pending changes. They
  now require a permission prompt; `git pull`/`fetch` stay auto-allowed and
  `git reset`/`clean` already prompted.

### Changed
- **Plan mode is locked down comprehensively.** The plan prompt's tool
  denylist previously missed `task`, MCP, plugin, debug, and spec tools, so a
  "read-only" planning session wasn't fully read-only. It now denies every
  tool that can change the filesystem, run a command, reach the network, or
  delegate work. (`edit_lines`/`edit_minified` were already covered via the
  `edit` permission name.)

## [0.7.5] - 2026-06-17

### Changed
- **Edit tools auto-close an unbalanced delimiter instead of bouncing it
  back.** A truncated tool-call JSON argument was already repaired
  mechanically, but an unbalanced `()`/`[]`/`{}` in code the model wrote was
  only detected and the edit rejected — costing a model round-trip for a
  mechanical fix. Now `write`, `edit`, `edit_lines`, `apply_patch`, and
  `edit_minified` mechanically close a purely-unclosed delimiter imbalance and
  report the fix on the result (`[auto-repair] …`), the same way the JSON
  repair does. Safe by construction: only for languages whose comments/strings
  are understood (so a delimiter inside a string or comment is never
  miscounted), never for a stray/mismatched closer, and only when the closed
  result actually re-parses — tree-sitter is the oracle that rejects a nonsense
  close. A genuinely broken edit still gets the precise "the `(` at line N is
  never closed" rejection. All edit tools now share one pre-write gate.

## [0.7.4] - 2026-06-17

### Fixed
- **In-loop critic no longer judges a truncated, stale view of the run.** The
  transcript handed to the F6 critic (and the goal gate) rendered the whole run
  and then kept only the first ~8000 chars — so in a substantial run it saw the
  planning/scaffolding at the start and never the implementation and
  verification at the end, producing confidently wrong critiques ("no code
  created", "no demo run") about work that was actually complete. It now keeps
  the original request plus the most recent activity (head + tail, eliding the
  middle), since completion is decided by the latest work.

## [0.7.3] - 2026-06-17

### Added
- **Storm-breaker graceful failure.** When a run gives up because it's stuck
  repeating the same tool call (the repeat-loop guard's terminal case), it now
  appends a short first-person assistant message explaining that it stopped to
  avoid spinning, instead of ending on an empty/abrupt turn. The message names
  the tool(s) it looped on and is recorded in history, so the user gets a
  coherent reply and the model carries its own failure account into the next
  turn. The internal reflect-then-pivot nudge on the first trip is unchanged.
- **`/rewind` now rolls back files, not just the conversation.** Every
  write/edit/edit_lines/apply_patch (including delete and rename) snapshots the
  touched file's pre-mutation content, keyed by the user prompt that triggered
  it. Rewinding to a prompt restores the working tree to its state before that
  prompt ran, in lockstep with the conversation truncation — so a long
  autonomous run is safe to unwind. Content is deduplicated through a small
  content-addressed pool so a file edited many times across turns doesn't store
  many copies. In-memory and process-scoped (rewind works within a live
  session, not across a restart); a created file is deleted on restore, a
  deleted file is recreated. From the [howard chen
  writeup](https://howardchen.substack.com/p/deepseek-v4-pro-at-5-the-cost-of)'s
  rewind lever.
- **Hash-anchored editing (`edit_lines` + `read(line_hashes=true)`).** A new
  edit path aimed at cheaper models: `read` can prefix each line with a 3-char
  content hash (`42 a3f: ...`), and `edit_lines` replaces a line *range* by
  number — `start_line`, `end_line`, the `expected_hashes` for that range, and
  `new_text` — without retyping the old block. The tool recomputes the hashes
  from disk and rejects the edit (per-line diff) if any line drifted since the
  read, so it never clobbers content that changed underneath it. Reuses the
  existing read-before-edit gate, tree-sitter pre-write validation, and atomic
  write. The win, per the [howard chen
  writeup](https://howardchen.substack.com/p/deepseek-v4-pro-at-5-the-cost-of):
  fewer retries and markedly lower output tokens on models like DeepSeek, since
  the model emits line numbers + tiny hashes instead of reproducing the text it
  wants to replace. The existing exact-string `edit` is unchanged.
- **Prefix-cache hit accounting + `/cache` command.** Providers like DeepSeek
  and Anthropic serve repeated request prefixes from a cache at a steep
  discount (DeepSeek ~1/10 the input price), and dirge already holds the system
  prompt + sorted tool defs at a stable prefix to keep that cache warm — but
  there was no way to tell whether hits were actually landing. Real
  provider-reported usage (`cached_input_tokens`, `cache_creation_input_tokens`)
  now flows from the stream through to a cumulative per-session counter, and
  `/cache` prints the session's cumulative prefix-cache hit ratio. This is the
  instrument for the DeepSeek cost story in the [howard chen
  writeup](https://howardchen.substack.com/p/deepseek-v4-pro-at-5-the-cost-of):
  cache discipline is the headline lever for running cheaper models, and now
  you can measure it.
- **Working memory keeps a slice of the prompt as project facts accumulate.**
  Memory entries are evicted by kind-derived salience, which drops transient
  `working` notes before durable facts — so a project with many high-salience
  invariants could push working memory out of the injected context entirely.
  The hot-tier budget now reserves a small slice for `working` entries:
  long-term still uses the full budget when no working notes are present, but it
  can't evict working below the reserve, and a working note never displaces a
  long-term fact within its share either.
- **The memory curator promotes durable working notes and weighs usage.** A
  `working` note that turns out to be a lasting fact (a build command, a design
  decision) no longer just decays — the background curator now surfaces working
  entries that outlived their session and re-classifies the ones whose use count
  and content prove durable to `procedural`/`semantic`. Its consolidation input
  also annotates every entry with `[kind | uses | id]`, so keep/merge/remove
  decisions weigh how load-bearing an entry actually is, not just its text.

### Changed
- FNV-1a hashing is unified behind one internal module; the `/rewind` snapshot
  store's process-global scope is documented as intentional (subagent edits are
  rewindable by the parent). No behavior change.

## [0.7.2] - 2026-06-15

### Changed
- **Default and coding prompts now lead with a "reach for what exists" ladder.**
  Both prompts gained a short decision procedure run before writing any code:
  skip it (YAGNI) → stdlib → native platform feature → installed dependency →
  one line → minimum that works. The Code Style sections were previously all
  "don'ts"; this adds the positive directive that actually cuts output volume,
  plus an explicit list of what laziness never touches (trust-boundary
  validation, data-loss handling, security, accessibility). Adapted from the
  [ponytail](https://github.com/DietrichGebert/ponytail) skill.

## [0.7.1] - 2026-06-15

### Fixed
- **Resumed sessions restore the TODOS and MODIFIED panels past a
  compaction.** 0.7.0 rebuilt these panels by replaying the tool calls in the
  message history, but a destructive compaction drains those messages out of
  the session — so a resumed `compacted-*` session came back with empty TODOS
  and a near-empty MODIFIED list. The panel state is now snapshotted into the
  session file on every save (independent of message history), so resume
  restores it even after a fold. Sessions saved by an older binary have no
  snapshot and can't be recovered; only sessions saved going forward carry it.

### Changed
- **Faster `dirge --session <id>` resume.** Resolving a session id to its fold
  chain tip no longer fully deserializes every session file in the directory —
  it scans with a lightweight partial parse and fully loads only the winning
  tip. Resume startup no longer degrades as old session files accumulate.

## [0.7.0] - 2026-06-15

### Added
- **Spec-driven workflow tracker (SQLite-backed).** A dirge-native take on
  spec-driven development: align on what to build before writing how, tracked
  as rows in the per-project DB rather than a markdown-folder tree. Living
  specs (capability → requirement → scenario) are the current truth; a change
  carries requirement deltas plus a task checklist, and archiving folds the
  deltas into the living specs in one transaction. Real task status as a
  column, queryable specs, no silent parse failures. Exposed via the `spec`
  agent tool and a bundled spec-driven-workflow skill.
- **`/spec` command.** Read-only view of the tracker: list changes, show one
  change (proposal + deltas + tasks), and read living specs
  (`/spec specs [capability]`).
- **Active-change context injection.** The active change (why/what/design +
  recorded deltas + task status) is rendered into the agent preamble at build
  time, so a resumed or fresh session knows what it's implementing and where
  it left off without querying the tool.
- **Archive forms a memory.** Archiving a change folds its rationale and
  design decisions into durable project memory, so the reasoning outlives the
  change record.

### Fixed
- **Session resume restores the TODOS and MODIFIED panels.** The todo list and
  modified-files set live in process-global state that isn't part of the
  session schema, so resuming a session (`--session`) or switching with
  `/sessions` replayed the conversation but left those panels blank until the
  agent ran again. They're now reconstructed by replaying the session's
  recorded tool calls. `/clear` also now clears the todo list, not just the
  modified-files set.

## [0.6.5] - 2026-06-15

### Added
- **Visible compaction progress.** A destructive fold now shows
  `⟳ compacting context…` in the main pane while the summarizer runs,
  instead of the session appearing frozen. The result line follows when
  it finishes.
- **Mid-session memory awareness.** The system-prompt memory block is
  fixed at agent-build time, so memories written by background
  consolidation weren't visible until restart. Consolidation now flags the
  change and the loop re-injects the refreshed memory block at the next
  turn boundary, so the running agent sees newly consolidated memories
  without restarting.

### Changed
- **Faster compaction.** The background incremental checkpoint already
  summarizes a context snapshot off the loop; the destructive fold now
  reuses that precomputed summary (prune + splice, no inline LLM call)
  when it's current and clears the fold target. This is the common path
  under the 100k budget, where folds fire often.

### Fixed
- The inline compaction summarizer is now bounded by a timeout: a provider
  that stalls without erroring falls back to prune-only instead of
  freezing the session indefinitely. The background checkpoint summarizer
  is bounded too so a hung call can't leak the task.

## [0.6.4] - 2026-06-14

### Added
- **Working-context budget (`context_target`, default 100k tokens).** A
  model's effective quality degrades well before its advertised window
  fills — the usable "smart zone" runs out around 100k regardless of size.
  The compaction decision now treats the effective window as
  `min(model_window, context_target)`, so the live context is folded — and
  project memory formed — to stay inside the budget rather than drifting
  into the degradation zone on a 200k/1M model. Configurable in
  `config.json`; floored at 16k; composes with `compaction_fold_threshold`
  (fold point = `fraction × min(window, context_target)`).
- **Memory formation on compaction.** When a summary fold clears
  conversation context, dirge now runs the same background review/curate
  pass it runs at session end, so the session's learnings are captured into
  the durable per-project memory store before the fold discards them.
  Self-throttled and single-runner, so frequent folds don't pile up.

### Fixed
- The agent loop now honors an explicit `context_window` config override
  (it previously read only the built-in model table and ignored it).

## [0.6.3] - 2026-06-13

### Fixed
- **Mouse scroll/select stopped working mid-session.** Mouse capture and
  bracketed paste were enabled once at startup and never re-asserted, so a
  child program run through the bash tool (a pager/TUI like `git log` →
  `less`, `fzf`, `vim`) that reset terminal modes on exit silently turned
  dirge's off — the wheel then scrolled the whole UI and click-select
  stopped registering. The paint loop now re-asserts these modes on a 1s
  throttle so dirge self-heals; non-SGR escapes are also stripped before a
  chat line reaches a terminal cell so a leaked control sequence can't
  corrupt terminal state in the first place.

### Added
- **`Ctrl+O` toggles expand/collapse of the last truncated block** — a
  thinking burst (live or just completed) or a collapsed tool/command
  result. Thinking is now retained past the turn boundary, so it stays
  expandable once the response is showing; a second press collapses.
- **Bundled workflow skills starter pack** under `skills/` —
  `systematic-debugging`, `code-review-feedback`, and `writing-skills`,
  adapted from the superpowers collection (MIT). Opt-in: copy a skill dir
  into `.dirge/skills/`. See [skills/README.md](skills/README.md).
- **Short session id in the status bar** for quick reference.

## [0.6.2] - 2026-06-12

Long-horizon session work, porting ideas from MiMo-Code onto dirge's
existing loop and memory rather than bolting on parallel machinery.

### Added
- **Durable session checkpoint (schema v10).** Each conversation gets a
  `session_checkpoints` row holding the regenerated fold summary plus a
  write-once verbatim-intent slot, keyed by a stable `origin_id`. Written
  on every compaction fold; SQLite-backed in the per-project state.db.
- **Incremental checkpointing (default on).** Refreshes the durable
  checkpoint at 20%-interval usage thresholds (20/40/60/80% for windows up
  to 200K; 10% to 500K; 5% above; disabled under 25K) in a background
  task, without folding — so a resume after a quit/crash recovers a fresh
  state instead of falling back to lossy compaction. The destructive fold
  still fires at 0.75. Disabled in headless `-p`/`--loop` (nothing there
  persists it). Disable with `incremental_checkpoint = false`.
- **Goal gate (`--goal`).** Opt-in natural-language stop condition for
  autonomous runs: at each finalization an independent judge (the critic
  provider) rules whether the condition holds; if not, its reason
  re-enters the loop, bounded so a mis-stated goal can't spin forever.
  Requires a configured `critic_provider`.
- **Global memory tier (default on).** A cross-project memory store
  (single db in the user data dir) injected into the prompt under its own
  header; the `memory` tool gains a `scope: "global"` argument for durable
  user preferences that follow you across repos. Isolated from project
  memory.
- **`compaction_fold_threshold` config** (0.3–0.75) to fold — and thus
  checkpoint — earlier, from more coherent context.

### Fixed
- **Resume now picks up where it left off.** A fold rotates the session id
  and leaves the old file behind, so resuming by the id you started with
  loaded a stale pre-fold snapshot. Conversations now carry a stable
  `origin_id` across folds; resume resolves any id to the live chain tip,
  and the session list collapses a folded conversation to one entry
  instead of one per rotation.

## [0.6.1] - 2026-06-11

### Fixed
- **Headless `--session` now persists the full assistant turn.** `dirge -p
  --session <id>` saved only the assistant's final text, dropping every tool
  call and result — so a resumed session (notably an MCP delegation
  follow-up) lost the substance of prior work, and a tool-heavy final turn
  (which often has little trailing text) saved a thin/empty message that read
  as a cut-off end. The turn's tool calls are now saved too, so
  `convert_history` re-emits the `tool_use`/`tool_result` blocks on resume.
- **mermaid_diagram plugin: validation gaps.** The diagram edge check rejected
  valid `er`/`class`/`state` diagrams (and flowchart `-.->`/`==>`); broadened
  the connector match. Added a `{}`-balance check for `{decision}`/`{{hexagon}}`
  nodes, and dropped a stray `[` from an error message.

## [0.6.0] - 2026-06-11

### Added
- **Run dirge as an MCP server** (`dirge mcp`, `mcp-server` feature, default
  on). Another agent (e.g. Claude Code) can delegate implementation tasks to
  dirge and review them — the caller plans/architects, dirge implements.
  Keeps a persistent per-project session: `delegate` extends it, `new_session`
  rotates for a new task/thread, and the current session id is remembered in
  `.dirge/mcp_current_session.json` across restarts. Each `delegate` returns a
  bounded, review-friendly result — `status`, `summary`, `files_changed`,
  `turns` — not the raw transcript. Tools: `delegate`, `new_session`,
  `session_info`, `list_sessions`. Register with
  `claude mcp add dirge -- dirge mcp`. See [docs/mcp-server.md](docs/mcp-server.md).

### Fixed
- **`dirge -p --session <id>` now resumes the conversation.** Headless print
  mode persisted the session but never fed the prior turns back to the model,
  so each `--session` run started cold (a follow-up task had no memory of the
  previous one). The print path now resumes the loaded session's history; the
  `--loop` path is unchanged. Fixes multi-step continuity for the MCP
  delegation loop and any scripted `dirge -p --session` use.

## [0.5.2] - 2026-06-10

### Fixed
- **Tool chambers are restored when a session is reloaded.** The scrollback
  replay (`/sessions` switch, `-c`/`--session` resume, `/fork`) rendered only
  message prose and dropped every persisted tool call, so reloading a session
  lost all its edits/bash/reads from the visible transcript and showed
  pure-tool-call turns as a bare handle. Each call is now reconstructed as a
  chamber, matching the live view. (Data was never lost — the model always
  re-saw the calls; this was a display gap.)
- **Idle scroll no longer stalls.** The render loop's 8ms paint throttle could
  drop the final frame of a fast wheel/trackpad scroll, and with the agent
  idle there was no timer to repaint it — so the scrolled position sat
  unpainted until the next event. A pending dirty frame now repaints just past
  the throttle window even when idle.

### Changed
- Mouse-wheel scrolling over the chat moves 3 lines per tick (was 1), matching
  the side panel.
- On interactive exit, dirge prints a dark-gray hint —
  `Resume this session with: dirge --session <id>` — so it's easy to pick the
  session back up (skipped for `--no-session` / empty sessions).

## [0.5.1] - 2026-06-10

### Fixed
- **`sandbox-microvm` builds on macOS without OpenSSL.** The feature pulled
  `ssh2 → libssh2-sys → openssl-sys`, so `cargo build --features
  sandbox-microvm` (and `--all-features`) failed on machines without a
  system OpenSSL. The microVM SSH client is now `russh` (pure Rust, `ring`
  crypto backend) — no OpenSSL, no libssh2, no cmake. Host-key pinning and
  command execution behave as before.

### Added
- **Homebrew install.** `brew install dirge-code/dirge/dirge` installs a
  prebuilt binary (macOS + Linux); on macOS it avoids the Gatekeeper
  quarantine prompt of a downloaded tarball. The release workflow
  auto-bumps the tap formula on each tag.

## [0.5.0] - 2026-06-10

### Added
- **SQLite-backed project memory.** Memory moved off the flat `MEMORY.md` /
  `PITFALLS.md` files into a `memories` table in the per-project session DB
  (`.dirge/sessions/state.db`). Two-tier storage: hot entries inline in the
  prompt verbatim, and once the inline budget fills the least-salient entries
  demote to a searchable breadcrumb index (id + preview) the agent expands
  with `memory(action='expand')` or queries with `memory(action='search')`
  (FTS5). Removal tombstones rather than deletes (`restore` brings entries
  back). Legacy `MEMORY.md` / `PITFALLS.md` files are imported automatically
  on first load and parked as `*.imported`.
- **Cross-turn failure recovery checkpoint.** The repeat-loop guard only
  catches a model repeating the *same* call; a separate tracker counts
  consecutive errored tool results across turns (reset by any success) and,
  at a streak of three distinct failures, injects one structured recovery
  checkpoint asking the model to name the shared root cause and take a
  different next step. When one tool dominates the streak it's named so the
  model re-reads that tool's contract.
- **"Did you mean?" feedback.** An unknown tool name now gets a nearest-match
  suggestion (`ehco` → `echo`), an invalid enum argument lists the valid
  values plus the closest one, and `read` on a mistyped path suggests the
  near-miss neighbour in the same directory (`parserr.rs` → `parser.rs`).
- **Janet compression plugin + `/plugins` command.** Plugins can supply a
  custom compaction summary via an `on-compact` hook, and `/plugins` lists
  the loaded plugins.
- **Interactive DAP REPL** (`/dap-repl`) and a shared JSON-RPC framing layer
  for the debug-adapter client.

### Changed
- **Slash-command tab completion is on by default.** It was gated behind an
  experimental feature that never shipped in the default set; it's now a
  first-class `slash-completion` feature, default-on.
- **Memory guidance explains the frozen snapshot.** The agent is told its
  saved memory is injected as a session-start snapshot, so a fact saved
  mid-session becomes active next session — it no longer re-saves facts it
  doesn't see reappear.
- Oversized `bash` / `webfetch` output now truncates head + tail (was a
  buggier middle-out heuristic).

### Removed
- **Cross-session memory extractor** and the unused memory `confidence`
  field. The post-session orchestrator is now three stages (background
  review → skills curator → memory curator); the extractor re-derived what
  the per-session review already captures.

### Fixed
- A `read` cache hit returns the file content instead of a terse "unchanged"
  message, so a re-read after compaction isn't left empty.
- Compression: git-diff off-by-one and the truncation heuristics.
- Fresh-state panics, an FTS index issue, tool-status lifecycle bugs, and
  duplicate-result handling (DAP review round).
- Fail-closed permissions for the debug-adapter tools.

## [0.4.1] - 2026-06-08

### Fixed
- The `sandbox-microvm` feature now compiles on non-Linux hosts. The reflink
  fast path (`copy_file_range`) and the `dirge-microvm-runner` libkrun calls
  are Linux-only and were ungated, so `--features sandbox-microvm` (and
  `--all-features`) failed to build on macOS. They're now `cfg(target_os =
  "linux")`-gated, with a `std::fs::copy` fallback for file copies and a
  clear "Linux-only" stub for the runner. The sandbox itself still requires
  Linux + KVM at runtime; this only fixes the cross-platform build.

## [0.4.0] - 2026-06-08

### Added
- **Hardware-isolated microVM sandbox** (`--sandbox microvm`, opt-in behind
  the `sandbox-microvm` build feature). A per-session Linux microVM boots
  once via libkrun and runs every `bash` tool call inside it over SSH, with
  the workspace mounted via virtio-fs. Includes a pure-Rust OCI image puller
  (every layer SHA-256-verified against the manifest, no skopeo/buildah
  needed for remote images), ephemeral SSH keys with guest host-key pinning,
  rootfs snapshot/restore, the `/sandbox` slash commands (`attach`/`ssh`,
  `reboot`/`start`, `snapshot save|list|restore|delete`), the
  `dirge sandbox check` / `dirge sandbox setup` subcommands, and a
  `--microvm-image` flag. Requires `/dev/kvm` and `libkrun.so`. See
  [docs/microvm/](docs/microvm/INDEX.md).

  Experimental, and narrower than full isolation: only `bash` runs in the
  VM. The file tools (`read`/`write`/`edit`/`apply_patch`/`list_dir`/
  `find_files`) still operate directly on the host workspace, so the VM is a
  boundary for command execution, not for file access. See
  [docs/microvm/SECURITY.md](docs/microvm/SECURITY.md).
- **Memory entries now carry a UMP kind, identity, and lifecycle metadata.**
  Saved memories are classified by kind (working / semantic / procedural /
  identity / …) at capture time instead of all defaulting to one bucket, and
  record provenance and lifecycle fields used by eviction and recall.

### Changed
- **Memory compaction evicts by salience, not age.** When the store is full
  the least-salient entry is dropped first (ties broken oldest-first, so the
  prior behavior holds under uniform salience). Salience now derives from the
  entry's kind (transient working notes rank well below durable identity /
  semantic facts), so the useful memories survive longer.

### Security
- **Load-time threat scan on memory.** Entries that reach `MEMORY.md` /
  `PITFALLS.md` out-of-band (hand-edit, `git pull`) and so bypassed the
  capture-time check are now scanned on read, closing the rehydration gap for
  prompt-injection content smuggled into the memory files.

## [0.3.1] - 2026-06-05

### Changed
- **The TUI now renders as a single model-driven paint per event.** A
  `UiState` model is the single source of truth, and the screen (status
  line, input area, avatar, panels, scrollback) is painted as one effect
  when the model changes — with dirty-flag coalescing — replacing ~85
  scattered inline paint sites. No change in normal use; this is the
  groundwork for the input fix below.

### Fixed
- **Modal prompts no longer block the UI.** Permission prompts, the
  `question` questionnaire (including custom free-text answers), the
  `/plan` switch confirmation, and plugin confirm/select dialogs each ran
  in their own nested blocking read loop, which could freeze the interface
  while a prompt was up. They now route through a unified input state
  machine driven by the main event loop, so chat scroll, text
  selection/copy, and terminal resize stay live during a prompt, and user
  keystrokes take priority over the agent's output stream. Concurrent
  permission requests from a parallel tool batch queue safely instead of
  clobbering the active prompt.

## [0.3.0] - 2026-06-04

### Security
- **Plugin tools now route through the permission engine.** A Janet
  plugin-registered tool previously ran its handler with **no**
  allow/deny/ask check — arbitrary file/network/shell I/O bypassing
  authorization entirely. Plugin tools now go through the same `enforce`
  chokepoint as built-ins via a new high-risk `Operation::Plugin` (not
  builtin-allowed, not Accept-coerced, like MCP/bash): they prompt by
  default in standard mode, can be denied with `deny_tools`, and are
  allowed under yolo or a `/allow plugin_tool …` grant.

### Added
- **Agent profiles** (opt-in): `/agent <name>` activates a named persona
  that bundles a system prompt, a model, and a tool policy
  (`.dirge/agents/*.md`, `~/.config/dirge/agents/*.md`, or config `agents`,
  layered project > global > config). `/agent off` reverts. The `task` tool
  can spawn a subagent under a profile via `task(agent="<name>")`. See
  [docs/agents.md](docs/agents.md).
- **Shell integration** (the `:` prefix, zsh): an optional plugin to run
  `:<prompt>` from your normal shell — the answer prints and you stay in the
  shell; `:` commands share one session. See
  [shell-plugin/README.md](shell-plugin/README.md).
- **`--session <id>`**: resumes the session with that id if it exists, and
  **creates it under that exact id** otherwise — a stable id for scripts and
  the shell plugin.
- **`--no-color`**: collapses the entire TUI to the terminal's default
  foreground through a single theme chokepoint.
- **`Alt+X`**: drops queued mid-execution interjections without cancelling
  the running agent (`Ctrl+C` still cancels both).
- Role-routing keys `summarization_provider` and `subagent_provider` are now
  actually consumed: the former routes the in-loop compaction summarizer, the
  latter sets the default model for `task`-spawned subagents.

### Changed
- **Automatic compaction now runs LLM summarization.** Proactive
  turn-boundary folds at the high context-ratio threshold now produce a
  structured summary (via `summarization_provider` when configured, else the
  main model) instead of silently degrading to a prune-only pass — the
  in-loop summarizer was declared but never wired. The cheap tool-output
  cap/prune still runs first with no LLM call.
- **`/wt-merge` is now programmatic and conflict-safe.** It performs the
  merge directly (refuses on a dirty tree, `git merge --abort` on conflict,
  never force-deletes the worktree) instead of delegating to an unconstrained
  LLM prompt, and only returns to the main repo on a clean merge.
- **`/prompt` and `/agent` compose as independent layers.** Their
  `deny_tools` now union (an agent can no longer silently re-enable a tool an
  active prompt denied), and `/agent off` restores the prompt layer's prompt +
  denies and the pre-agent model.
- **`allow_tools` now caps MCP and plugin tools too** (via the `mcp_tool` /
  `plugin_tool` umbrellas), not just built-ins.
- **Phased `/plan`**: the plan phase now gets a true context reset
  (findings-only), matching its documented isolation; explore→plan forks run
  off the UI event loop.
- Model-family steering now tracks the active (swapped) model rather than the
  launch-time model, so `/model` and `/agent` swaps steer correctly.

### Fixed
- Compaction could leave an orphaned `tool_use`/`tool_result` pair
  (unbalanced history → provider 400); the fold window now snaps to
  user-message boundaries, and the tool-output prune handles block-array
  content (previously a silent no-op on real tool results).
- `edit_minified` is now classified as a mutating edit, so the
  verify-before-done gate and the repeat-loop/storm breaker treat it like
  `edit`/`apply_patch`.
- `--no-color` no longer leaks hard-coded green/red diff backgrounds.
- `/panel auto` threshold corrected to ≥152 cols (was documented as ≥100,
  where no gutter actually fits).
- The active DAP debug session is now torn down at process exit instead of
  relying on `Drop` of a `static` that never runs.
- Plugin `transform-context` / `on-compact` hooks no longer block a runtime
  worker thread (now `spawn_blocking` + timeout), and `message-end` fires in
  headless (`--print` / `--loop`) mode too.
- Background-shell concurrency cap is enforced atomically; `/mode yolo` warns
  at runtime when configured deny rules are made inert; `/allow add`
  validates against the single tool-name source of truth; a leaked git
  worktree is cleaned up if creation fails after `git worktree add`; an
  ignored plugin-provider re-registration is now surfaced.

## [0.2.4] - 2026-06-03

### Added
- **Debug Adapter Protocol (DAP) integration**: step-through debugging
  alongside the existing LSP client, hardened against the usual adapter
  failure modes (UB, hangs, panics).
- **Configurable terminal background**: themes can set a `background` color,
  and the built-in phosphor theme defaults to a soft charcoal `#222222`. The
  plain theme keeps the terminal's own background (`Reset`).
- **Snap-to-bottom on input**: typing in the prompt or pressing Down on a
  scrolled-up chat jumps straight back to the latest output instead of
  requiring a manual scroll.
- **Phased plan workflow** (`/plan <request>`, opt-in via
  `phased_workflow_enabled`): an explicit per-task command that runs
  explore → plan → implement → reviewer-runs-code loop. The explore and
  plan phases are context-isolated read-only forks; the implement phase
  is a normal streamed turn; a write-disabled reviewer fork then runs the
  code and emits a machine-parsed verdict, with `NEEDS_FIX` feeding a
  punch-list back for a bounded re-implement (`phased_workflow_max_review_cycles`,
  default 2). Ported from [vix](https://github.com/kirby88/vix). See
  [docs/agent-loop.md](docs/agent-loop.md#phased-plan-workflow-plan).
- **Minified tree-sitter read/edit** (`read_minified` / `edit_minified`):
  token-efficient file I/O that collapses a file to its structural
  skeleton — aggressive collapse for Rust/Java/Go, gap-preserving collapse
  for whitespace/ASI-sensitive grammars (Bash, Python, Ruby, Elixir, C/C++,
  TS, Clojure). Each gated on its `semantic-<lang>` feature.
- **Hard read-before-edit gate**: `edit`/`apply_patch` to a file never
  read this session is refused mechanically.
- **Thinking-stall watchdog**: the request-timeout backstop now injects a
  summary-reinjection nudge for graceful recovery from a stalled run.
- **Mandatory reason/intent fields** on the read/grep/glob/find/lsp tools
  (and bash anti-misuse fields), plus a **todo-completion nudge** that
  blocks a premature `end_turn` while todo items remain pending.
- Config keys `phased_workflow_enabled`, `phased_workflow_max_review_cycles`,
  and documentation for the pre-existing `dynamic_tool_search` and
  `context_depth_reminder_threshold` keys.

### Fixed
- **/plan busy indicator**: `/plan` now shows the standard busy state and
  clears the submitted text from the input box while the explore/plan forks
  run (previously it looked idle with the command still lingering), and the
  busy flag can no longer be stranded "running" by a failed repaint.
- **Resize scroll clamp**: enlarging the terminal while scrolled up no longer
  leaves a stale scroll offset that hid the newest output behind blank rows.
- **Idle Ctrl+C** now clears a typed-but-unsent draft instead of quitting
  outright; only an empty input line exits.
- **Home/End** moved to **Ctrl+Home/End** for chat scroll, freeing bare
  Home/End for the input editor's line-start/line-end as documented.
- Ctrl+J inside reverse-i-search no longer desyncs the search buffer.

### Acknowledgements
- Added [vix](https://github.com/kirby88/vix) — the battle-tested Go coding
  agent the above agentic-loop features were ported from.

## [0.2.3] - 2026-06-02

### Added
- Unified permission/authorization engine (single Policy Decision Point):
  op-based rules, `/why` decision-trace command, atomic multi-claim bash.
- Input box scrolls to keep the cursor visible past the height cap, and
  Up/Down navigate across soft-wrapped display rows.
- Cohesive low-saturation phosphor palette (hue = action, brightness =
  importance), a dedicated soft "thinking" color, and syntax-highlighted
  `read` boxes. Critic/thinking colors are config-themeable.
- Config-driven plugin toggles (`plugins.<name>.{enabled, auto_start}`)
  and a bundled `backpressured` validation-gated loop plugin.

### Changed
- Lighter terminal UI: the heavy double-line frame is now light
  single-line/rounded, the side panels follow the main frame's theme
  color, and the input prompt is a simple `> `.
- Reasoning/thinking is suppressed by default (spinner + Ctrl+O to
  expand) to keep the conversation focused.

### Fixed
- Secrets in tool output are redacted before reaching the LLM / session
  transcript.
- Transient LLM connection failures ("error sending request") now retry
  with exponential backoff.
- Questionnaire custom answers soft-wrap instead of running off-screen.
- Edit results collapse the appended LSP diagnostics into a one-line
  summary (Ctrl+O to expand); diagnostic floods are summarized and the
  per-file cap tightened, so an unsupported language server no longer
  floods the chat.
- A configured `deny` rule is now terminal above a session allowlist.
- Resumed sessions keep persisting after a save (loaded-mtime refresh).

### Packaging
- Published to crates.io as the **`dirge-agent`** crate (the short
  `dirge` name was taken); the installed binary is still `dirge`:
  `cargo install dirge-agent`.

## [1.0.0]

First tagged release. dirge is a minimalistic, memory-efficient coding
agent in Rust with:

- A terminal UI with markdown rendering, scrollback, and an info panel.
- Configurable permission modes (standard / restrictive / accept / yolo)
  with op-based rules and session allowlists.
- Tree-sitter bash permission parsing and semantic code tools for
  TypeScript, Python, Clojure, Go, Ruby, Rust, Java, C, and C++.
- Claude-compatible skills, persistent project memory, subagents, MCP and
  LSP integration, and a Janet plugin system.
- Session save/load/resume with LLM-summarization compaction.

[Unreleased]: https://github.com/dirge-code/dirge/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/dirge-code/dirge/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dirge-code/dirge/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/dirge-code/dirge/compare/v0.3.0...v0.3.1
[1.0.0]: https://github.com/dirge-code/dirge/releases/tag/v1.0.0

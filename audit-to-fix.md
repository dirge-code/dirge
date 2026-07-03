Epic is dirge-8gdv

re check after with:
let's spin up subagents to review the code based on the documented feature axis in the docs and look for bugs, code smells, and refactoring opportunities, the goal is to bring the project to highest
  professional standard for open source


Consolidated audit — dirge v0.16.0

  CRITICAL (3)

  1. [security] Permission bypass: command substitution inside an allow-listed head auto-executes. echo $(rm -rf ~) matches the builtin echo ** allow rule and runs with no prompt at all; the whole raw command
  becomes one Execute claim keyed on head="echo". Same for backticks, <(…), >(…), arithmetic expansion — every allow-listed prefix (cat, git status, cargo test, diff) is a vector. The mutation extractor doesn't
  descend into the substitution either. — src/agent/tools/bash/check.rs:185, src/semantic/adapters/bash.rs:468
  2. [bug] Plugin confirm-gate fails open. on-tool-start hooks get a 5s eval budget, but harness/confirm blocks up to 600s waiting for a human. Any answer slower than 5s times the hook out, the timeout arm
  continues, and the gated tool (e.g. rm) executes while the dialog is still on screen; a later "deny" is silently discarded. This is the documented flagship safety pattern. — src/plugin/mod.rs:650,706,
  src/agent/agent_loop/plugin_hooks.rs:126
  3. [bug] Compaction summaries are silently dropped from provider requests. The rig request builder has no system-role arm (_ => None), so after any auto-fold or /compress on the main provider path, the folded
  middle and its summary never reach the model — silent context loss exactly when compaction should preserve it. Masked in casual testing because session restore merges system history into the preamble. —
  src/agent/agent_loop/rig_stream_factory.rs:374-441

  HIGH (18)

  4. [security] Env/wrapper prefix defeats head-anchored deny rules. FOO=1 rm -rf /, nohup rm -rf /, time/nice/timeout … rm -rf / change the head token, so a configured hard-Deny silently downgrades to Ask and user
  edit … deny rules are bypassed. — src/semantic/adapters/bash.rs:320
  5. [security] Config yolo/accept_all overrides an explicit CLI --restrictive. resolve_mode checks yolo before restrictive; a project .dirge/config.json can therefore escalate an untrusted repo past a user's
  command-line restriction. — src/main.rs:114
  6. [bug] Code-review gate reviews the whole dirty tree with no run-start baseline. With critic_provider set and pre-existing WIP, a read-only "explain this function" turn triggers two judge calls over code the
  agent never touched, and any high/critical finding blocks the loop up to 3× — contradicting its own comment and both docs. — src/agent/agent_loop/run.rs:259, code_review.rs:525
  7. [bug] Interjection never stops the run at the tool-result boundary. The only is_interjected() check is after the inner loop exits, so the documented "halt at next boundary" (relied on by permission-denial
  cascading) never happens — the model keeps taking turns. — src/agent/agent_loop/run.rs:1877
  8. [bug] Post-fold DB session ids all collapse to dirge-compacte. short_id takes the first 8 chars of compacted-<hex>, so every folded session in every conversation persists as one row; the fold handler links an
  orphan raw id, so lineage dedup/rebinding silently never works. — src/ui/agent_io.rs:214, run_handlers/context_compacted.rs:96
  9. [bug] first_kept_index (loop-transcript space) applied to session.messages (different space) can drop the protected tail on resume after a fold. Tool-heavy sessions drain most/all messages including the
  verbatim-kept recent turns; crash-resume then recovers only the lossy summary. — run.rs:723, run_handlers/context_compacted.rs:126
  10. [bug] Byte-index slice panic on multibyte bash output during compaction pruning. &cmd[..77] after a byte-length check panics on non-ASCII at byte 77; runs on every fold/overflow-recovery, killing the loop
  before the rotated session saves. — src/agent/compression.rs:515
  11. [bug] OAuth tokens frozen at client build with no mid-session refresh. Long interactive/--loop/--goal runs crossing token expiry die with a non-retryable auth error despite a valid refresh token on disk. —
  src/provider/client.rs:313, auth.rs:191
  12. [bug] edit produces \r\r\n corruption. new_text is spliced raw, then the whole buffer's \n→\r\n on CRLF files doubles CRs on every replacement line — despite the tool advertising CRLF handling. Siblings
  (edit_lines, apply_patch) normalize first. — src/agent/tools/edit.rs:181,324
  13. [bug] edit_minified skips the /rewind snapshot. Every other mutator captures pre-state; this one doesn't, so /rewind silently leaves edit_minified-touched files mutated — a partially-reverted tree the user
  believes was rolled back. — src/agent/tools/edit_minified.rs:139
  14. [bug] Skills curator told delete is recoverable archive; it's remove_dir_all. The weekly LLM pass is instructed to delete siblings after absorbing them, on a false recoverability premise; incomplete
  absorption = permanent loss. The real archive/restore exist but are dead code. — src/extras/skills/curator.rs:228, manager.rs:184
  15. [bug] Protected-skills guarantee is prose-only. delete/edit/patch never consult source/pinned; one curator (or agent) prompt slip hard-deletes a user-authored or bundled skill. — src/agent/tools/skill.rs:242
  16. [bug] Plugin loader re-registers earlier plugins' bare hooks under every later plugin's name. Bare symbols are never unbound after aliasing, so plugin A's on-prompt fires once per subsequently-loaded plugin —
  duplicated notifications, doubled append-system-prompt, double timers. — src/plugin/loader.rs:122
  17. [bug] Dialogs from UI-thread hooks freeze the UI ~30s and lose the result. on-turn-start/-end, on-response, etc. dispatch synchronously inside the same task that drains dialog_rx; harness/confirm there can't
  render until the eval times out. Docs claim dialogs are "safe from any hook." — src/ui/mod.rs:4487, worker.rs:75
  18. [bug] /wt-merge broken end-to-end. detect() uses git rev-parse --git-dir's parent as the worktree path, yielding <main>/.git/worktrees; every merge and post-merge cleanup fails with "must be run in a work
  tree." detect() is never exercised by tests. — src/extras/git_worktree/mod.rs:21
  19. [bug] Malformed frame mid-session half-kills LSP and DAP clients. Non-EOF decode errors early-return past the cleanup block, so closed stays false and every in-flight + subsequent request waits out its full
  timeout. Same bug in both hand-rolled stacks. — src/lsp/rpc.rs:178, src/dap/client.rs:200
  20. [bug] ACP sessions are stateless and uncancellable. Every prompt runs with empty history (cwd ignored), so multi-turn editor conversations lose all context; session/cancel hits the "unhandled" catch-all while
  the runner keeps executing. — src/extras/acp/mod.rs:94,213
  21. [bug] MCP-server delegate child never killed on cancellation. No kill_on_drop/process group, so an auto-approving dirge -p --accept-all child is orphaned (keeps editing) if the client cancels or the server
  dies. — src/extras/mcp_server.rs:263

  MEDIUM (highlights — ~40 total)

  Correctness: verify-pass can't drop a FALSE_POSITIVE-annotated (vs omitted) finding, so it can wrongly block (code_review.rs:832); verifier substring heuristics fabricate red/green — "Exit code:" anywhere = red,
  "check" matches git checkout = green — fed to the critic (verifier.rs:171,181); insert_message runs 4 statements with no transaction, can corrupt the external-content FTS index (session_db.rs:1145); skill_db
  multi-step writes lack transactions unlike memory_db (skill_db.rs:238); legacy MEMORY.md import can index FTS under the wrong rowid via last_insert_rowid() after OR IGNORE, and partial import strands the rest
  (memory_db.rs:2000); restore_entry can demote the eviction-exempt overview (memory_db.rs:1268); restored skills stay archived forever (skill_db.rs:417); escalation route missed the retry wrapper every other route
  gets (build.rs:602); Anthropic OAuth token can go over plaintext http (guard only covers OpenAI) (client.rs:267); PKCE verifier reused as OAuth state (anthropic_oauth.rs:67); cross-process auth-store races
  clobber rotated refresh tokens (auth/store.rs:148); explicit --model gpt-4o silently rewritten to gpt-5.5 under Codex (dispatch.rs:134); invalid permission config block silently drops all rules instead of failing
  fast (main.rs:149); /loop start <prompt> leaks "start" into the prompt (loop_cmd/start.rs:15); DEFER_WT_* colon-sentinels break on paths containing : (worktree.rs:131); several DAP stop-event races (stale events
  satisfy later waits; pause blocked by continue's mutex; attach mislabels running as Stopped); edit/edit_lines write from_utf8_lossy output (destroys non-UTF-8 bytes) and rewrite mixed line-endings wholesale to
  CRLF; relay summary points at a file that was never written on write failure; apply_patch failures return Ok("FAILED…"), invisible to recovery machinery; /spec anchors at process cwd while sibling commands use
  session.working_dir.

  Doc-drift: retry policy is 5 retries→16s, docs say 3→4s (features.md:100 — flagged by three separate reviewers); storm window is 6, docs say 32; * permission default is Ask, docs say allow; permissions.md
  precedence table omits the configured-deny decider and misstates Restrictive; keybindings replaces wholesale, docs claim key-by-key merge; -r/--resume says "browse and select" but loads the most recent;
  stale-resume warnings only fire on --session; features.md /rewind command doesn't exist (it's Esc-Esc); DAP config section documents a system that doesn't exist; mcp-server.md misdescribes session storage/resume;
  tool-input-repair.md documents wrong relational key names (fields/default vs requires/defaults); harness/log documented as writing to log, is a no-op; harness/mutate-input rewrites approved args post-approval,
  undocumented; several undocumented config keys (tools.*_inline_max_bytes, top-level auth, context_window per-model table).

  Refactor (systemic): the file-mutation pipeline is hand-copied across 5 tools and the copies have drifted — every tools-layer bug is a divergence between siblings (consolidate into one mutate_file helper); the
  build_agent rebuild block is copy-pasted across 6+ slash sites despite an existing rebuild_agent helper (already caused one historical bug); LSP+DAP hand-roll near-identical correlation layers carrying the same
  cleanup bug (#19); three judge gates triplicate rules-truncation (with a bytes-vs-chars inconsistency) and fail-open shapes; the DEFER_* string-typed slash control flow should be a typed enum; run_interactive
  (~4,460 lines), run_loop inner body (~750), main() (~1,370), config/mod.rs, memory_db.rs, worker.rs are all monoliths with clear extraction seams.

  LOW (~25)

  /compress always falsely prints "prompt cleared"; /graph search swallows flags into the FTS query; TUI markdown wrapping is width-blind (CJK/emoji overflow); no panic hook (crashes vanish into the redirected
  log); --no-color bypassed by ~100 hardcoded Color:: literals; 12-bit line hash has no secondary staleness check; issue search doesn't escape LIKE wildcards; cancelled issue status undocumented; reasoning content
  duplicated when a provider sends both deltas and a complete block; blocking SQLite on the async loop task; .expect() on a mutex where the rest of the module ignores poison; plus assorted stale comments and dead
  code (SECURITY_PREAMBLE never wired).

  ---
  Cross-cutting themes

  - The engine cores are solid; the glue seams are where bugs live. Permission PDP, mutation primitives (atomic write, read gate), FFI safety, the shared framer, salience math, and memory_db lifecycle all came back
  clean. Every critical/high sits in a boundary: bash→PDP, loop-transcript→provider request, loop-transcript→session persistence, file-tree→skill DB, hook-budget→dialog-budget.
  - Copy-paste drift is the #1 root cause. The 5-way mutation pipeline, 6-way agent rebuild, 2-way correlation layer, and 3-way judge gate each produced at least one verified bug from divergence.
  - Doc drift is broad but shallow — mostly one-line corrections; the retry-policy 3-vs-5 drift was independently caught by three reviewers.

  Proposed TDD fix plan by rounds

  Per your global guidelines, each round is test-first (write the failing test, confirm red, fix, confirm green), one bd issue per finding, committed per round.

  - R1 — Security (blocking): #1 command-substitution bypass, #4 env/wrapper prefix, #5 config-vs-CLI mode precedence, plus the checker-level decision tests that were missing (which is why #1 shipped).
  - R2 — Silent data/context loss: #3 system-role drop, #8 dirge-compacte collapse, #9 index-space mismatch, #10 multibyte panic — with one fold→persist→resume integration test covering all four.
  - R3 — File-mutation safety + consolidation: #12 CRLF, #13 rewind snapshot, lossy-UTF-8 + mixed-endings, then extract the shared mutate_file helper so the class can't recur.
  - R4 — Plugin safety: #2 confirm-gate fail-closed, #16 loader aliasing, #17 UI-thread dialog dispatch, with timeout/two-plugin tests.
  - R5 — Gates & provider lifecycle: #6 diff baseline, #7 interjection boundary, #11 OAuth refresh, escalation retry-wrap, verifier heuristics.
  - R6 — Skills durability: #14/#15 wire archive/restore + source/pinned guard, dual-store reconciliation.
  - R7 — Integrations: #18 wt-merge, #19 framer cleanup (+ shared correlation layer), #20 ACP state, #21 delegate child, #22 DAP thread-id.
  - R8 — Doc drift sweep + remaining refactors: batch the ~15 doc corrections; tackle monolith extraction opportunistically.

  Want me to open bd issues for the findings and start on R1, or adjust the rounds/scope first? I'd recommend R1 goes first regardless — the command-substitution bypass is a real unauthenticated-execution hole in
  the product's core security boundary.


  <<<<<>>>>>

  File-mutation pipeline (R3) — self-contained, no network

  - 2hqv BOM files: edit_lines false-rejects any range including line 1
  - k32l mixed line-endings wholesale-rewritten to CRLF
  - yga0 non-UTF-8 bytes → U+FFFD via from_utf8_lossy
  - tc9l apply_patch failures return Ok("FAILED:…"), invisible to recovery
  - ol03 (refactor) consolidate the 5-way copy-pasted mutation pipeline · w9q9 (P3) 12-bit line-hash collisions

  Data / context loss (R2)

  - 9eyo get_session_meta only finds the 50 most-recent sessions
  - uzw4 discovery "window around match" actually anchored at first message
  - d04r restore_entry can demote the eviction-exempt overview
  - f67u / p53e insert_message / MEMORY.md-import non-transactional FTS corruption
  - x6yi blocking SQLite open+query inline on the async loop

  DAP debugger (R7)

  - acgj pause can never interrupt continue (mutex held across stop-wait)
  - p3r7 attach stalls full timeout, then records a running debuggee as Stopped
  - un0g stale queued stopped events satisfy later step/pause waits

  Provider / model routing (R5 leftovers)

  - ovjk explicit --model gpt-4o silently rewritten to gpt-5.5 under Codex
  - ej1o graceful AbortSignal::cancel not observed during retry backoff
  - dppc escalation stream route bypasses the retry wrapper
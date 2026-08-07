# Changelog

All notable changes to dirge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.21.11] - 2026-08-07

### Fixed
- A turn where the model emitted only reasoning — no prose, no tool calls —
  wedged the session permanently. It was persisted as an assistant message with
  empty content, and because history is rebuilt from the session on every
  submit, from then on it went back on the wire as
  `content: [{"type": "text", "text": ""}]`. Moonshot/Kimi and GLM answer that
  with `400 invalid_request_error: text content is empty`, and Anthropic 400s on
  it too, so every later prompt failed before the run even started. Reported
  against Kimi, GPT and GLM.

  A command that succeeds silently (`mkdir -p x`, `touch y`) hits the same 400
  from the other side: bash returns the empty string, so the tool result carries
  an empty text block. Unlike an empty assistant turn this one can't be dropped
  — Anthropic and OpenAI both reject a tool call with no matching result — so it
  now gets a `(no output)` body.

  The guards sit at each replay boundary (`convert_history`,
  `rig_message_to_loop_messages`, `value_to_rig_message_for_provider` — which
  already skipped empty *user* text parts, just not assistant ones), so a
  session that already carries an empty turn recovers on the next prompt instead
  of staying stuck. The four persistence sites stop writing empty turns at all.

## [0.21.10] - 2026-08-07

### Fixed
- Prompt compression no longer windows source files or the user's own messages.
  The tool-output stage's generic fallback fired on any content segment of 30+
  lines that its Drain pass could fold at all, which included file reads and
  pasted text, and then ranked lines with a *log* heuristic — errors 1.0,
  indented 0.5, everything else 0.3. On code that ranking is noise. A read of
  `src/agent/tools/read.rs` (1202 lines) reached the model as 45 lines; a
  176-line file as 40. It also destroyed the line numbers `edit_lines` and
  `line_hash` anchor on.

  Pasted text was worse, because the fold path emits no marker at all: 120
  pasted lines arrived as the single line
  `uniform vec{} u_color_{}; [×120: (3; 0..119)]`, with no header and no
  elision marker. That is the "llmtrim artifacts in the file" models kept
  reporting — they were reading a fold marker and concluding the file was
  corrupt, an edit had failed, or a brace was unmatched, then working around it.

  Tool results that really are logs, diffs and grep dumps still compress as
  before. On a mixed agent turn the honest saving is ~19%, against a reported
  ~92% when the reads and the paste were being destroyed.

- The "is this tool output?" test recognized only Anthropic/Gemini content
  blocks. OpenAI Chat Completions gives a tool result its own `role: "tool"`
  turn with no block type, so it now also resolves through `role_at`. Without
  both halves the exemption above would have read every OpenAI-shaped tool
  result as user text and switched tool-output compression off for OpenAI,
  DeepSeek, GLM, Cerebras, Kimi, Ollama and OpenRouter.

- The elision header no longer tells the model to "re-run the tool" when the
  segment it is attached to is a user message with no tool behind it.

- The turn-end per-result cap no longer guts large file reads (GH #755). This
  is a second, independent trimming layer from the one above:
  `cap_oversized_tool_results` head+tail truncates every tool result over 3000
  tokens on every turn after the one that called the tool.

  It cut at a UTF-8 boundary rather than a line boundary, so the head ended
  mid-row and the tail *started* mid-row — and a `read` row whose
  `<n> <hash>: ` prefix has been cut off is an anchor `edit_lines` cannot use.
  That is the "unable to properly edit based on line hashes" in the report.
  Both cuts now snap to a line boundary, with the old char-boundary cut kept as
  the fallback for content that has no line breaks (minified JS, a JSON blob).

  3000 tokens is also 12 KB, which is small for a source file. On a 1145-line
  JSX component (32.8 KB) the model kept the first of three edit targets and
  lost the other two. Because the capping is deterministic, re-reading returns
  the same cut view — hence the re-read loop, and the model eventually
  shelling out to `bash`/python to read the file. File excerpts now get their
  own cap (`file_excerpt_cap_tokens`, default 12000); the aggressive tier still
  overrides it, since a roomier allowance is worth nothing if the request stops
  fitting.

  The truncation marker told the model to "call the tool with a narrower scope
  (filter, head, pagination)" — which for a `read` is what it already did. For
  an excerpt it now names `offset`/`limit` and says the file on disk is
  complete.

### Added
- `read(verbatim=true)` returns an excerpt exempt from prompt compression, for
  when the model needs a guarantee that what it sees is what is on disk.

- The elision header states who authored the gaps and that the underlying
  content is complete: `[llmtrim compressed this tool output: N of M lines
  shown. The gaps and any [×N] markers are llmtrim's, not the file's — the
  underlying content is complete and unchanged. Re-run the same tool call for
  the full text.]` The old wording is still reachable as
  `[compression] header = "legacy"`.

- `file_excerpt_cap_tokens` (default 12000) sets the per-result cap for `read`
  excerpts. Raise it for a codebase of large files; set it to 3000 to hold
  reads to the same cap as any other tool output. Floored at 3000, and the
  aggressive tier still overrides it.

- `[compression]` gains `trim_user_text`, `window_code`, `header` and
  `verbatim`. They exist so `scripts/loop-ab.sh` can stand up a control arm on
  the pre-fix behavior; the defaults are the fixed ones and there is no reason
  to change them otherwise.

- Documented GitHub Copilot as a backend (GH #698). It needs no dedicated
  provider type — the endpoint is OpenAI-compatible, so `provider_type`
  `openai` / `openai-responses` with `base_url: https://api.githubcopilot.com`
  and a `gh auth token` reaches it. Recipe contributed by @dubchord, verified
  against Copilot Enterprise.

- `scripts/loop-ab.sh -s edit-large` — a scenario that reads a large,
  deeply-nested file and makes three precise edits to it. The existing
  scenarios all write a *new* file, so none of them exercises read-then-edit;
  an A/A on `recon-real` returned `errored_tool_calls`, `repair_invalid` and
  `scavenged_calls` at zero across all six runs, meaning the metrics that
  would register a mangled read were already on the floor and no sample size
  could separate the arms. The three targets sit at sections 7, 34 and 58 so
  head+tail truncation drops the middle ones.

## [0.21.9] - 2026-08-07

### Added
- A Home Manager module (`nix/home-manager.nix`, exposed as
  `homeModules.dirge`). `programs.dirge.enable` installs the package and
  `programs.dirge.settings` writes `$XDG_CONFIG_HOME/dirge/config.json`, which
  is where dirge already looks. Thanks to @notlyrix (GH #752).

  Follow-up on top of it: the `package` option gains `defaultText`, without
  which Home Manager's option-doc generation renders the whole derivation into
  the options table; and an unsupported system now fails with a message naming
  the option to set rather than a bare `attribute 'x86_64-darwin' missing` (the
  flake builds for x86_64-linux, aarch64-linux, and aarch64-darwin).

- `source_gate` — a deterministic gate over comments ADDED to source files this
  run. When one asserts an external source was consulted and no
  `webfetch`/`websearch` ran, it fires a nudge. **Off by default**, opt-in,
  with its own mode key rather than riding on `claim_gate`.

  It exists because the other two claim-checking surfaces read the model's
  final answer, and the fabrication that prompted this work was never there: a
  doc comment written into `src/config/mod.rs` asserting "Rates are the
  providers' published API list prices, checked Aug 2026" and naming six
  pricing pages, in a session with no network access and no fetch call. A claim
  written into the artifact is outside an answer scanner's field of view.

  Deliberately narrow. Only lines this run added count — a run-start baseline
  diff is subtracted — and RFC citations, bug IDs, spec sections, repo file
  references, and URLs the code fetches at runtime are excluded, because
  comments cite things legitimately all the time. One known over-detection is
  documented at the source rather than hidden: a bash `curl` records as `bash`,
  so a run that genuinely fetched that way reads as unsupported.

### Changed
- `claim_gate` now matches a claim against the KIND of evidence it needs. It
  previously took a single "did any build/test command run" bool and treated
  that as support for any verification claim, so a final answer asserting
  "5001 passed" counted as supported by a bare `cargo build` that ran no tests
  — the strongest claim validated by the weakest evidence.

  A test-count claim now requires an observed test command; a build or lint
  claim requires one of those. This is a coverage axis, deliberately separate
  from the verifier's `VerificationTier`, which is a cost axis: `cargo build`
  is Slow *and* build-kind, so a green build reports `VerifiedGreen` and the
  tier cannot answer what the evidence supports.

  Net effect should be *less* noise, not more — "compiles" backed by a real
  build now stays silent, where the vocabulary alone would have fired on a true
  claim.

- The critic's evidence block records which tools ran, not just how many.
  Claims about files and commands were checkable against it; claims about
  consulted sources were not, because tool usage was only a count. The set is
  sorted and deduped, and the preamble gains one clause making a
  consulted-source claim with no fetch/search tool unsupported.

### Fixed
- `GateTally` panicked at runtime when a `GateSource` variant was added: the
  backing array was fixed at 10 while the new variant pushed `None` to index
  10. Indexing is unchecked, so a stale length fails at runtime rather than at
  compile time; the array is now sized to the variant count with a comment
  saying it must grow.

## [0.21.8] - 2026-08-06

### Changed
- The status line shows real token usage (`tok:in/out`, plus a cache-hit
  percentage once there are cached reads) instead of a cost figure. `Done`
  carried a `text.len()/4` estimate and a cost pinned at 0.0, with
  `TODO(cost-tracking)` markers promising a per-provider pricing table.

  That promise is withdrawn rather than kept. Published rates move, dirge has no
  network to refresh them, and a table baked into the binary goes stale silently
  — the failure mode is a confident wrong number, which is worse than no number.
  A first attempt at building one invented a "checked Aug 2026" sourcing comment
  citing six pricing pages it had never fetched, which is the argument against
  the whole approach rather than an argument for a better table. Token counts
  come from the provider, cannot go stale, and anyone who wants dollars can
  multiply by their own rates. The segment stays hidden until the provider has
  actually reported usage, so a fresh session shows nothing rather than
  `tok:0/0`.

- `claim_gate` now defaults to `advisory` instead of `off`. The gate shipped
  opt-in on the reasoning that it nags the model — but in `advisory` the ceiling
  is one message per run, and "nags" describes `blocking` (up to three
  re-entries), which stays opt-in.

  What tipped it: a delegated run finalized with a broken test build while
  reporting "Compiles", and wrote a sourcing claim into a doc comment for pages
  it never fetched. Nothing fired, because all three claim-checking layers are
  off unless configured — the critic needs `critic_provider`,
  `verification_command` defaults to unset, and this gate defaulted to `off`.
  The one layer armed by default is the verifier nudge, which that run defeated
  by running a green build. One message per run is a cheap backstop against
  shipping on an unsupported claim.

  An unrecognized `claim_gate` value still falls back to `off` rather than the
  new default, so a typo cannot silently arm a gate. Explicit `off` remains
  byte-identical to the loop without the gate.

### Fixed
- `/cache` could report a hit ratio above 100% — 4000% was reachable.
  `cache_hit_ratio` divided cached tokens by `cumulative_input_tokens`, which
  holds for DeepSeek and OpenAI, where cached reads are a subset of the reported
  input count. Anthropic reports the three lanes disjoint: `input_tokens` is the
  uncached remainder, and the real prompt total is input + cache-read +
  cache-creation. On a warm cache the numerator dwarfed the denominator. The
  ratio now detects the disjoint shape and sums the lanes, and the `TokenUsage`
  docs say the relationship is provider-dependent instead of asserting "a subset
  of `input_tokens`".

## [0.21.7] - 2026-08-05

### Changed
- Dependency sweep. `rmcp` was on three versions at once — our own 1.7, a 2.2
  that `rig`'s unused `rmcp` feature dragged in, and a 1.8 from
  `agent-client-protocol`. Nothing here ever touched rig's MCP bridge, so that
  feature is gone; `agent-client-protocol` moves 0.12 → 2.0 (its types live under
  `schema::v1` now, and `respond_with_error` moved from `Dispatch` to the
  `Responder` that only the request variant carries); and `rmcp` goes to 3.1,
  where `Annotated<RawContent>` is flattened to `ContentBlock`. One rmcp, latest.

  Also current: `rusqlite` 0.31 → 0.40 (no `ToSql for usize`), `sysinfo`
  0.32 → 0.39 (`RefreshKind::new` → `nothing`), `sha2` 0.10 → 0.11 (digest output
  dropped `LowerHex`, so the two hex helpers encode explicitly), `jsonschema`
  0.46 → 0.49, `base64` 0.23, `compact_str` 0.10, `http` 1.5, `notify-rust` 4.18.
  `tree-sitter` stays at 0.25 — 0.26 does not resolve against the grammar crates.

### Fixed
- Reasoning is shown again for providers that stream it as `delta.reasoning`
  rather than `delta.reasoning_content` (GH #745, reported against LocalAI).
  rig 0.39 only deserialized `reasoning_content`, so serde dropped the field and
  the panel stayed empty while text and tool calls — carried in the same delta —
  worked fine. rig 0.40 added the fallback upstream; this bumps rig to 0.41.

  The bump is not small. 0.41 splits the classic runtime into `rig-agent` behind
  an `agent` feature, replaces `Tool::definition` with `description`/`parameters`
  across all 37 tools, drops `ToolDyn` in favour of a concrete struct, and makes
  `Agent::preamble` and `Agent::model` private. Dirge's tools move to the
  context-free `PortableTool` contract, which is the honest fit — the agent loop
  has driven its own dispatch since 4.5h-6 and never used rig's `ToolContext`.
  The erased-tool seam is now dirge's own `DynTool`; it has broken on two
  consecutive rig releases and does not need to be rig's. `AnyAgentInner` holds
  the completion model directly instead of a rig `Agent` it only ever read the
  model back out of.

- Cerebras no longer 400s on the turn after a reasoning turn. It streams
  reasoning as `delta.reasoning` — the same field LocalAI uses — so the fix
  above meant dirge started capturing it and replaying it as `reasoning_content`,
  which Cerebras rejects outright. The field is renamed to `reasoning` at the
  wire boundary rather than dropped, so the model keeps its own chain of thought
  in context on the next turn. Backends that want `reasoning_content` are
  untouched: DeepSeek needs it on tool-call turns, and llama.cpp/LocalAI chat
  templates read `message.reasoning_content` back out of the assistant turn.
  Only the OpenAI Responses API still has the block dropped, because it keys
  reasoning to encrypted ids dirge does not retain.

## [0.21.6] - 2026-08-04

### Added
- **A publish-state guard.** Once verification goes green, nothing stopped the
  agent throwing that work away — a `git reset --hard`, a `git checkout -- .`,
  or an `rm` of a file in the verified diff, reported as success on the
  discarding command's exit status. The safe-state rung only notices after six
  weighted failures, which a confident cleanup never produces. The guard arms at
  the fresh-green instant from the same worktree fingerprint safe-state already
  stamps, and blocks operations that *discard* verified work. Editing a
  protected file is untouched: dirge sessions keep working after green, and
  blocking rewrites would nag on every edit-test cycle. `publish_guard`:
  `off` (default) | `advisory` | `blocking`.
- **A claim/evidence gate.** Fires when the final answer makes a specific claim
  the run's evidence does not support — a test count or named-gate result when
  the verifier saw no verification command at all, or an applied/fixed/changed
  claim when zero files moved. Deterministic, with quoted and attributed claims
  carved out. `claim_gate`: `off` (default) | `advisory` | `blocking`.
- **Falsifiable expectations on learned memories.** A procedural memory can name
  a trigger and an expected outcome, settled deterministically against the
  session digest and the run's verification status. Success, failure, and "the
  situation never arose" stay distinct — collapsing the last two is what makes
  an expectation unfalsifiable.
- **Gate co-occurrence** in the per-run tally, plus `scripts/loop-ab-selftest.sh`
  covering the A/B harness's reporting layer.

### Fixed
- **Agent-authored check scripts could latch a green verification.** Script-name
  recognition accepted any path whose basename carried a marker, so a validator
  the model wrote this run satisfied the gate without the project's tests ever
  running. A script that *invokes* a real test command still counts — that's a
  wrapper, not a proxy. Related: the run-start marker is now backdated, because
  Linux sets filesystem timestamps from a clock cached at timer-tick
  granularity and a just-written script could read as pre-existing.
- **A newline is a command separator.** `masks_failure` handled `;` but not its
  bash synonym, so a validation block chained with newlines and ending in `echo
  passed` latched green with the status belonging to the echo. The same
  oversight let a newline bypass the new publish guard.
- **Two advisory gates steered nobody.** The open-issues and untracked-work
  reminders emitted a `SystemNotice`, which is a UI event the model never sees,
  so both fired, rendered, and changed nothing. They now inject a model-visible
  message; what the human sees is unchanged.
- **`delegate` reported the wrong files.** `files_changed` unioned a
  session-cumulative set, so a call that changed nothing still listed the
  previous call's files — the field a caller uses to check whether work
  happened. Now scoped to the delegation, alongside a new `evidence` object
  carrying the verification commands actually observed.
- **The default test suite failed on live-provider conditions.** A retired
  DeepSeek model name, and a Cerebras scenario asserting on the wording a live
  model returned. Environmental conditions now skip rather than fail, keyed on
  the provider's own rejection so the guard stays lenient instead of vacuous.
- **`cargo check --no-default-features`** built again — three call sites, not
  the one previously recorded.

## [0.21.5] - 2026-08-03

### Fixed
- **The agent could never connect to nREPL on its own.** With no connection,
  `nrepl_eval` returned "Use `/nrepl-connect` first" — but slash commands are
  typed by the user, and no harness call or builtin tool lets the agent issue
  one. The plugin registered no connect tool either, so the model's only move
  was to ask you and wait, which is exactly where sessions stalled. Auto-connect
  compounded it: `on-init` reads `.nrepl-port` once at startup, so a REPL the
  agent started itself a few turns later was never picked up even though the
  port file was sitting right there.

  `nrepl_eval` now re-reads `.nrepl-port` and connects on demand, and there's
  an `nrepl_connect` tool for a non-default host/port or a reconnect after a
  server restart. It accepts an unquoted port, which a model writes at least
  as often as it quotes one — the old string-only extractor dropped those
  silently. The error text and the injected skill prompt name the tools;
  slash commands remain for interactive use.

### Security
- **A worktree writer no longer inherits the LLM auto-approver.**
  `for_working_dir` drops the parent's session allowlist on purpose — a grant
  made against the parent checkout doesn't mean the same thing under a
  worktree root — but it kept `approval_provider`'s evaluator. Together those
  invert the trust model: your own "allow always" decisions vanish while the
  thing that can auto-allow *without asking you* survives. Calls you had
  already settled came back as `Ask` and the evaluator answered on your
  behalf, so a background writer ran with a weaker human backstop than the
  session that spawned it. Worktree writers now prompt you. Prompt deny-lists
  still propagate — they're terminal and path-independent.

## [0.21.4] - 2026-08-03

### Security
- Bumped `russh` to 0.62.5 for GHSA-m65r-rprj-r5rg (channel-scoped server
  callbacks reachable without an open channel). dirge uses russh as an SSH
  *client* into the microVM guest, so the server-side callback path isn't
  one we run, but the advisory fails the OSV scan and the fix is a patch bump.

### Fixed
- **The permission prompt hid most of the command it was asking about**
  (#744). The renderer sized the alert box to fit the whole tool call, but
  `Layout` then clamped it back to the input editor's 8-row cap, so on any
  terminal a bash command longer than about three wrapped rows was cut to a
  "↓ N more line(s)" hint. You could scroll it, but the box never grew — the
  usual outcome was approving a command you hadn't read. Overlays now get
  their own ceiling (everything but a 4-row chat floor); the editor keeps its
  cap so a pasted block still can't crowd out the chat. The same clamp was
  silently capping the shell overlay at 8 rows instead of its intended 12.

- **MCP tool calls prompted with no arguments at all.** The permission match
  key is `mcp_tool:<server>:<tool>`, and that was the entire prompt — nothing
  distinguished a harmless query from `execute_sql {"query": "DROP TABLE
  users"}`. The arguments now show under the tool name, pretty-printed, with
  an explicit marker if an oversized blob has to be cut. Display only: rule
  matching and the "allow always" pattern are unchanged.

### Added
- **Deny with a redirect.** `d` at a permission prompt opens a one-line field
  for a note that rides along with the denial ("edit `src/config.rs` instead,
  that file is generated"). It reaches the model in the tool result, so it
  can act on the correction instead of guessing its way around a bare
  refusal and retrying a variant of the same call. `n` / `Esc` still deny
  immediately.

## [0.21.3] - 2026-08-02

### Fixed
- **A subagent that ran out of turns reported back as if it had finished.**
  A coordinator dispatching a batch got `completed:` followed by whatever the
  agent happened to be narrating when its 25-turn cap hit — "Now let me check
  the next file" — and reconciled it as delivered work. Two things combined:
  the loop appends the max-turns notice as a *user* message, so the last
  *assistant* message (and therefore `Done.response`) is the in-flight
  tool-call turn's preamble; and the drain that carries a subagent's result
  back to its parent silently discarded `SystemNotice`, the only event
  carrying that notice. The headless path had detected this for a while via
  `MAX_TURNS_NOTICE_PREFIX`; the `task` tool had no equivalent.

  Cut-off payloads are now marked, positioned so the marker survives the
  disk relay's head/tail summary. The same applies to the empty-response
  fallback, which returns the accumulated multi-turn token stream — running
  narration rather than a report — and used to pass it off unlabelled. A
  clean answer still round-trips verbatim.

- **"Allow always" could save a rule that never fired, so the same command
  prompted forever.** The pattern was derived by skipping a hardcoded list of
  nine shell builtins, but the built-in rules auto-allow around twenty more
  (`cat`, `ls`, `grep`, `echo`, `head`, `tail`, `rg`, `find`, …). The two
  drifted. `cat f.txt | clojure -M -` therefore suggested `cat *` — already
  covered by the built-in `cat **` — so the grant added nothing, the
  `clojure` segment kept asking, and every subsequent invocation re-prompted.

  The suggestion is now derived from the rule set itself and targets the
  first segment that isn't already allowed, with a generated guard so the
  two can't drift again. Segments come from the same tree-sitter splitter
  the permission layer runs, so a quoted `|` or a heredoc body can't
  manufacture a phantom segment.

- **Allow-always is no longer offered for substitution/subshell commands.**
  Session grants are deliberately never honored for those — the inner command
  is invisible, so `echo *` must not cover `echo $(rm -rf ~)`. But the dialog
  offered it anyway, saved the entry, said so, and re-prompted on the next
  identical command. It now allows once and explains why.

- **The auto-approval evaluator could be talked into approving.**
  `approval_provider` verdicts were parsed by checking whether a line *started
  with* "ALLOW", so a chatty or reasoning model emitting "Allowing this would
  be risky because…" auto-approved the command with no human involved — the
  inverse of the parser's documented fail-safe contract, on the only gate
  between the evaluator and execution. ALLOW must now be the whole line;
  DENY stays prefix-matched, since it carries a reason and erring that way
  costs a prompt rather than an unreviewed command.

### Internal
- Three relay tests pushed 5000-line payloads through the real relay and never
  cleaned up, accumulating megabytes in `~/.dirge/transient` that look exactly
  like production payloads when triaging a subagent report. `transient_base`
  now takes a test-only redirect into a temp dir.

## [0.21.2] - 2026-08-01

### Fixed
- **Releases reached only some of their distribution channels, silently.**
  v0.20.0 shipped to 4 of 6: `cargo install dirge-agent` went straight from
  0.19.29 to 0.21.0, and the site still read v0.19.29, so it had missed 0.20.0
  as well. The split was clean — every automated channel fired, both manual
  ones were skipped — so both are automated now, `cargo publish` and the site
  version bump, off the tag like the rest.

  Automating them alone would only convert "someone forgot" into "a job
  silently no-opped": the homebrew and site steps rewrite markup with `sed`,
  and an unmatched `sed` exits 0. So a final job reads every channel back from
  its published source — the registry index, the tap's formula, the site's
  version span, `nix/bin.nix` on `main`, and every release asset with its
  checksum — and fails if any disagrees with the tag. A channel that didn't
  ship is now a red X on the release run rather than something found two
  releases later. See [docs/releasing.md](docs/releasing.md).

- **The macOS test suite was red on a correct tree**, in two unrelated ways.
  A dependency check asserted the unversioned `libkrunfw.dylib` while the code
  emitted `libkrunfw.5.dylib` — the code was right, since libkrun carries no
  `LC_RPATH` and dlopens libkrunfw by bare versioned name, and the unversioned
  symlink the loader never consults can exist where the dlopen still fails. The
  name was hand-written in four places, so it is one constant now, paired with
  a test pinning the versioned literal — de-duplication alone would make the
  decision unfalsifiable.

  Separately, a test hard-failed whenever libkrunfw simply was not installed,
  where its ~20 siblings skip on a missing prerequisite. It skips now. The
  precondition asks Homebrew directly rather than reusing the prefix resolution
  under test: had it reused it, the skip condition would have been the
  assertion, and the guard would have gone vacuous instead of red. CI has no
  macOS runner, so none of this was visible to the gate.

### Changed
- **The capability tier now derives the recovery-checkpoint threshold**
  (dirge-z85a). `FAILURE_REFLECTION_THRESHOLD` was consumed once, at tracker
  construction during run setup — where the estimator is always `Nominal` by
  warm-up, so a derivation there would read the neutral tier every time and be
  inert. It is read at every poll instead, and `Struggling` gets the checkpoint
  after 2 consecutive failures rather than 3.

  This is the one guard whose input and trigger are the same observation: the
  estimator is built from failure counts and streaks, and this fires on
  consecutive errored results. Adaptation stays one-directional — `Nominal` and
  `Strong` are bit-identical to before. Two things in the same tracker
  deliberately do not move: the permission checkpoint, because a denial streak
  is a policy wall and nothing the estimator counts measures it, and the
  safe-state abort's 2× signal, because that rung spends a hard-capped abort
  and writes to the tree in `auto` mode.

  No improvement is claimed. The measured ~2x noise floor makes an effect this
  size undetectable at any sample count the project can afford, so it ships on
  the structural argument or not at all.

### Internal
- **`--all-features` had accumulated 17 clippy findings** across 8 files, the
  same way `sandbox-microvm` accumulated 50 before it was gated. Cleared, and
  the configuration is now linted in CI — clearing without gating just restarts
  the clock. One `--all-features` pass rather than one per feature: it subsumes
  `dap`, `plugin` and `semantic`, and `--features dap` alone proved clean once
  these were fixed. The three narrower configurations stay, since
  `--all-features` cannot reach a `cfg(not(feature = …))` path and
  `windows-default` is subtractive.

## [0.21.1] - 2026-08-01

### Fixed
- **A capability signal never reached the estimator.**
  `record_hallucinated_tool_name` had zero production callers — its only caller
  was a unit test invoking the recorder directly, so it passed while the counter
  sat at 0 in every real run. A weighted input the estimator could never see,
  and undetectable, because a counter reading zero looks identical to the
  behaviour never happening. A hallucinated call now counts as both errored and
  hallucinated, matching how `repair_invalid` already stacks; with the current
  weights that orders the failure kinds — plain error 1, invented tool name 3,
  unrepairable arguments 5.

### Changed
- **The capability tier may add support, never remove it** (dirge-5mtx.7).
  `Strong` now scales identically to `Nominal`, and the
  `FAST_VERIFY_EDIT_THRESHOLD` derivation it drove is removed; only `Struggling`
  moves a threshold or a budget. `CapabilityCounters` observes tool-call
  mechanics only — errored calls, repaired arguments, invented names, scavenged
  text, storms, streaks — and nothing in that set changes based on whether the
  model verifies its work or makes progress. So a `Strong` reading is evidence
  about argument hygiene and cannot license relaxing a progress or verification
  guard. Both failures this work exists because of came from models the
  estimator reads as `Strong`: the 60-turn reconnaissance thrash was
  deepseek-flash at a 0% tool-call error rate. Defaults are untouched —
  `Nominal` and `Strong` are both bit-identical to the pre-estimator constants.

  The audit behind it, recorded in
  [docs/verification-discipline.md](docs/verification-discipline.md): of 13
  capability-tuned constants, two are structural no-ops (budgets of exactly 1,
  where the floor eats the scaling), two are cost ceilings that must not scale
  up, one is a category error (network retry budgets are not a model property),
  and one is inert where it is consumed.

### Internal
- **A test raced a Janet VM against a wall clock** and flaked twice on loaded
  CI. The timeout extension only engages if a dialog is already in flight when
  the base budget expires, so the test required the worker to cold-start and
  reach `harness/confirm` inside that budget — scheduler-dependent latency with
  no upper bound, which no choice of budget fixes. The decision is now a pure
  function with an exhaustive truth table, and the part that genuinely needs a
  worker is checked by sampling the in-flight flag when the dialog request is
  received, which is clock-free. Verified by mutation rather than by a green
  run.

## [0.21.0] - 2026-08-01

### Added
- **Verification discipline** — a set of loop and tooling changes with one thing
  in common: a check *ran*, reported success, and could not have failed for the
  reason that mattered. All six failures found while building this came from
  doing the work, not from looking for them; all six were verification
  failures, none was a steering failure. See
  [docs/verification-discipline.md](docs/verification-discipline.md).

  **Project gate** (`verification_command`, dirge-w2de). Names the command whose
  pass is the only honest green. When set and unmet, a `VerifiedGreen` becomes
  `FastGreenOnly` so the existing escalation carries it — no new status.
  Matching is by (program, subcommand) signature, since `RUSTFLAGS="-D warnings"
  cargo clippy --all-targets` and `cargo clippy --all-targets -- -D warnings`
  are the same gate. Unset by default and unchanged when unset.

  **CI command advisory.** Auto-detecting *the* gate from CI does not work:
  this repo's own `ci.yml` yields four recognized signatures, two equally
  strongest, so any rule picking one is guessing — and a wrong auto-gate nags
  forever. Real CI has several gates, all required. The recognized set is now
  surfaced as information in the verify nudge, touching no verdict.

  **Exploration-prologue bound** (`progress_prologue_cap`, dirge-t5dh). The
  progress monitor's stall counter armed only on a progress event, so a run that
  produced nothing never armed and the monitor could not report the case it most
  needed to. Observed: 60 turns of successful reads with nothing written, the
  threshold configured and on, and no other guard able to see it. An upper bound,
  not an eager nudge — it also counts tool calls, since the observed thrash
  batched 40+ into one turn.

  **Capability tier.** Estimated from observed failure rates, never from model
  identity — on the same task the stronger model was the one in trouble.
  Currently derives one threshold, and only for `Strong`, only toward less
  intervention. `Nominal` is bit-identical, so a default install is untouched.

  **A/B harness** (`scripts/loop-ab.sh`). Arbitrary config overrides per arm,
  per-model reporting, and three guards each earned by a failure: a mechanism
  check (a treatment arm whose nudge count is zero did not exercise the change),
  a noise floor (an A/A produced 18 vs 36 turns from identical config — any
  delta inside that is reported `~noise`), and an explicit warning that a
  single-model result is not evidence.

### Changed
- **Gate lifecycle state collapsed into `GateStates`** (dirge-5mtx.5).
  `poll_finalization_follow_up` took seventeen positional arguments, nine of
  them `&mut` counters, behind an `allow(clippy::too_many_arguments)`; it now
  takes one state struct plus a borrow of the read-only inputs, and the allow
  is gone. Behaviour-preserving — both structs are destructured at the top, so
  the gate bodies are unchanged. The value is that every gate's re-fire state
  now sits in one place, each labelled cost-ceiling (bounds LLM spend, stays)
  or re-fire-guard (compensates for a predicate that can't tell "happened"
  from "happened and I already reacted"). Relaxing any budget remains
  descoped: at a ~2x noise floor that claim is not measurable.

### Fixed
- **The branch failed `cargo fmt --all --check`** in 31 places across the eight
  files this work touched, while `clippy -D warnings` and the test suite were
  green throughout. CI runs rustfmt as its own job. Recorded as the sixth row
  of the pattern table, since it is the only one observed *after* the
  mitigation for its own family shipped.
- **Masked commands latched a false green** (dirge-w2de). `cargo test || true`,
  `cargo clippy | tail -2` and `cargo test; echo done` all recorded
  `VerifiedGreen` — the exit status belonged to `true`, to `tail`, to the
  `echo`. A masked *success* is no longer recorded at all (status stays
  `Unverified` and the gate asks again); a masked *failure* is still recorded
  red, since that direction is trustworthy. `&&` is not masking.
- **Verdict answer sets competed** (dirge-5mtx.3). `COMPLETE` is a substring of
  `INCOMPLETE` and `MET` of `UNMET`, `parse_verdict` read only the first
  non-empty line, and `"NOT COMPLETE"` parsed as *Complete* — an exact
  inversion. Measured against a 27-row corpus of judge phrasings the old parser
  got 11 wrong, every one failing in the same direction: reporting work complete
  that the judge had called incomplete.
- **The blocked-on-user gate misread completed work** (dirge-5mtx.4). Gate 0
  finalizes the run and skips the verifier, critic, todo and open-issues gates;
  it decided by testing for a trailing `?`. Five of five completed-work offers
  ("want me to wire it in too?") read as blocked. Now classified when a judge is
  configured, with a free structural pre-filter and the heuristic as fallback.
- **Boundary nudges stacked** (dirge-5mtx.2). Up to five harness messages could
  land before one assistant turn; the finalization boundary had picked exactly
  one since dirge-vcsn, the mid-turn boundary never did. One arbiter now chooses,
  in strict priority.
- **A failed judge call looked like a clean review** (dirge-q7vw). Both returned
  no messages and no findings, so a transient error recorded the reviewed-diff
  fingerprint and the next reaction skipped that diff forever.
- **The review dedupe skipped completeness too** (dirge-mu46). It now also
  requires that the previous reaction raised diff findings — the duplicate case
  it exists for. A completeness-only verdict has nothing to duplicate and is
  re-judged.

## [0.20.0] - 2026-07-31

### Added
- Four loop features adapted from the validation report for NASA's Deep Space 1
  Remote Agent Experiment (Nayak et al., ISAIRAS'99). A 1999 paper about a symbolic
  planner flying a spacecraft transfers less than you'd hope, but its control
  structure — the failure ladder, the fidelity pyramid, progress-versus-budget —
  maps onto an agent loop cleanly. All off or inert by default; see
  [docs/failure-ladder.md](docs/failure-ladder.md).

  **Tiered verification** (`verification_tiers`, dirge-uw2l.2). The verifier gate
  asked one question — did *some* build/test run and pass — and asked it only at
  finalization, so `cargo check` and `cargo test --all-features` were the same thing
  to it. Commands now classify Fast (typecheck, lint, one targeted test) or Slow
  (full suite or build). Three code edits with nothing run since gets one mid-run
  nudge for a fast check; fast-green with the suite never green escalates once at
  finalization. Unknown commands tier as Slow, so an unrecognized one costs a missed
  escalation rather than a false nag. RAX put ~200 planner variations on simulators
  running 7× real time and reserved flight hardware for nominal runs; the same
  section notes most bugs were caught during integration, not by the expensive
  end-stage campaign.

  **Progress monitor** (`progress_stall_threshold`, dirge-uw2l.3). Every existing
  guard keys on errors — the storm breaker needs an identical repeated call, the
  failure tracker needs errored results. A model making successful, varied, useless
  calls tripped none of them and simply burned the run. The monitor watches turn
  boundaries for a todo closed, a file touched for the first time, or verification
  going green, and asks what's blocking when enough pass without one. It arms only
  after the first progress event, so an exploration phase is never nudged. Also adds
  turn-budget notices at 60% and 85% of `max_agent_turns`: the cap was enforced but
  invisible to the model, and a silent hard stop can't prompt triage the way a
  countdown can.

  **Safe-state abort** (`safe_state_abort`, dirge-uw2l.4 / .6). The executive's
  failure ladder in the paper has three rungs; dirge had two — reflect-then-pivot,
  then a recovery checkpoint — and both keep the model digging in a tree that may be
  half-mutated. The third fires when the failure streak reaches twice the checkpoint
  threshold with unverified edits and a green point behind it, replacing that
  boundary's checkpoint (telling the model to both retry and abort is contradictory)
  and asking for one new approach rather than a menu. `advisory` writes nothing.
  `auto` restores the tree, but only after proving against git that the snapshot
  store covers every file changed since green: `snapshots::capture` is wired into the
  edit tools and not into `bash`, so a `sed -i` or an in-place formatter leaves no
  pre-state, and restoring around it would produce a tree that never existed. One
  uncaptured file and it declines to advisory.

  **Residual objectives** (always on, dirge-uw2l.5). A run cut short by `max_turns`
  now lists what's still outstanding on the todo board, so a resumed session doesn't
  re-derive scope from the transcript. RAX's 2-day scenario aborted at ~70% of its
  objectives; the team flew a 6-hour follow-up targeting precisely the remaining 30%,
  which is only possible if something says what's left.

- Prompt guidance from the same source. `debug.md` gets a confirm-the-inputs step
  ahead of hypothesis forming — in flight, the planner took an unexpected search path
  purely because the goals file on the spacecraft differed from the testbed copy, and
  no amount of logic debugging finds that. `code.md` and `default.md` replace "check
  for edge cases" with a method: enumerate the conditions in the code you changed,
  write a case at, just below and just above each boundary, and state the residual
  risk where one can't be covered. Both also gain an assumption ledger, and `code.md`
  a rule that passing tests are not evidence of absence for shared mutable state.

### Fixed
- `edits_since_verify` counted tool calls rather than code files, so a model asked to
  change four files in one `apply_patch` registered a single edit and the mid-run
  fast-check threshold was effectively unreachable for any model that batches — which
  is most of the capable ones. Found by an end-to-end run, not by the unit tests.
- Harness-injected messages were invisible to anything headless. They are user-role
  messages the TUI attributes to the system by matching their tag; `--print` shows
  only the final answer and the stream-json `user` event carries tool results only. A
  `--print` or `--loop` user watched the model abruptly change course with nothing to
  explain why, and no way to tell an injected steer from the model's own judgement.
  Every injection site now mirrors to a `SystemNotice`.
- The h7 smoke tests failed the whole suite when a provider hit a quota ceiling. They
  already skipped on a missing API key; a key that is set but out of quota is the same
  environmental unavailability, and a suite that goes red for something no code change
  can fix is one people learn to ignore. The auth-error scenario deliberately keeps
  asserting, since the error is what it tests.

## [0.19.29] - 2026-07-31

### Fixed
- The agent no longer reaches for `write_todo_list` when it should be writing a file
  (GH #734, dirge-w72q / dirge-5xvn / dirge-u1ay). Reported against a local Qwen over
  LocalAI, where the same prompt worked in opencode. Capturing both requests off the
  wire found four causes, all ours.

  With `dynamic_tool_search` on, the request shipped exactly three tools —
  `tool_search`, `task_status`, `write_todo_list` — because the always-on set omitted
  every core file tool. Asked to put code on disk, the model saw one write-shaped tool
  and called it. `read`, `write`, `edit`, `bash`, `grep`, `glob` and `list_dir` are
  always-on now; discovery is for the long tail.

  The system prompt separately told the model to put a non-trivial task on its active
  list *first*, and the tool description buried its one "skip this" clause after 700
  characters of board mechanics. The ordering mandate is gone and the when-not-to-use
  rules now lead the description, with `write`/`edit` named as what to call instead.

  The unfinished-todo nudge could not fire on a plan-only turn: it was gated on
  `turn_made_file_edits`, and `write_todo_list` is not an Edit operation, so "planned,
  did nothing, stopped" was the one failure with no backstop. It fires there now,
  worded toward the edit rather than the list, and one-shot — `new_messages`
  accumulates across finalization re-entries, so an unbounded branch would repeat the
  same nudge until the budget drained.

- A prompt's `deny_tools` now withholds the tool definition, not just the call
  (dirge-41al). `--prompt ask` shipped all 34 tools including `write`, `edit`, `bash`
  and `apply_patch` — schemas for calls the permission layer would refuse even under
  `--yolo`. Matching is by bare name and the qualified `mcp_tool:<server>:<name>` form;
  the `mcp_tool` / `plugin_tool` umbrellas stay enforcement-only, since a tool
  definition carries no marker of which server exported it. An agent profile's
  `allow_tools` becomes a real way to shrink the toolset as a result.

- Curator, skill and memory tests no longer leak into a shared `$TMPDIR/.dirge`
  (dirge-9cg2). They built `ProjectPaths::new` over a scratch directory, which resolves
  through the process-global `DIRGE_PROJECT_ROOT` — a variable the `dirge_paths` tests
  set concurrently. Their writes landed in the temp root instead, and once the leaked
  `sessions/state.db` held 10+ rows the curator first-run gate failed intermittently on
  every later run. Added `ProjectPaths::at` for a literal root.

### Security
- `event-listener` 5.4.1 → 5.4.2 (RUSTSEC-2026-0221). Transitive; lockfile-only.

## [0.19.28] - 2026-07-28

### Fixed
- Compaction no longer runs away on models missing from the context-window table
  (dirge-9jy7). `context_window_for_model` matches with `model.contains(key)`, so a
  short id can never match any key: `k3` fell through to the 128k default against a
  real 262144-token window. Every budget tier then ran at roughly twice the intended
  pressure. The lookup now falls back to the embedded models.dev snapshot — which
  already carried the right figure, but was only being read by the breakdown view.
  The hand-maintained table still wins on a conflict, so no existing window shifts;
  the snapshot can only turn a miss into a hit.

- A compaction pass that frees nothing no longer rotates the session or reports
  itself (dirge-kq3a). The fold trigger reads the API's `prompt_tokens`, which counts
  the system prompt and every tool schema, while the fold only rewrites the message
  history. Once the unfoldable fixed overhead alone cleared the threshold — a large
  MCP tool surface will do it — no fold could bring the ratio back down, so the loop
  re-fired every turn: rotate the session id, rebuild the agent, fire
  `on_session_switch`, print `context compacted: N → N tokens`, repeat. Sessions were
  observed rotating every ~6 seconds with identical before/after counts. Combined with
  the window fix above, a k3 session at ~100k tokens now sits well under the fold
  threshold instead of permanently over it.

  Relatedly, `HISTORY_FOLD_MIN_SAVINGS_FRACTION` is gone. The module doc described it
  as a live guard that skips a fold saving too little, but it was `#[cfg(test)]` and
  referenced only by a compile-time assert, so no release build ever consulted it. The
  doc now describes the guard that actually exists.

- Kimi OAuth tokens refresh 60s before they expire (dirge-iki5). Access tokens live 15
  minutes and the freshness check was a bare `now >= expires_at`, so a request could
  pass it with milliseconds to spare and still reach Kimi after the token died. The
  resulting 401 classifies as an auth error, which is never retried, so the turn failed
  outright. The OpenAI and Anthropic paths keep the exact comparison — their tokens are
  long-lived enough that the boundary is rarely crossed.

- The routed-Anthropic cache freeze no longer fires when the cache stage is off
  (dirge-01tu). Freezing predicted that Stage A would add a breakpoint, but Stage A is
  gated on `config.cache` — true in the `agent` / `aggressive` / `cache` presets, false
  in the lossless baseline `safe` / `rag` / `code` / `reasoning` inherit. Under those
  presets the history was frozen (so never compressed) while no marker was written (so
  never cached), which is strictly worse than not freezing. An explicit top-level
  `cache_control` still freezes unconditionally — its presence is evidence the upstream
  really holds a cached prefix.

## [0.19.27] - 2026-07-28

### Fixed
- dirge no longer sends `prompt_cache_key` to backends that reject it (dirge-07ew).
  The field was injected into every OpenAI-shaped request body, which covers the
  openai-compatible backends as well as OpenAI itself. A server that validates its
  request body strictly answers an unknown field by rejecting the whole request:
  Cerebras returned `body.prompt_cache_key: property 'body.prompt_cache_key' is
  unsupported` before it shipped caching, and Groq and Volcano Engine's DeepSeek
  still answer that way. It is now an allowlist covering OpenAI (where it is a
  documented routing hint, and required for reliable matching on GPT-5.6 and later)
  and OpenRouter (which falls back to it for sticky routing). Everywhere else the
  caching is automatic with no key to pin, so nothing is lost by leaving it out.

- Prompt caching now holds the prefix it pays to cache (dirge-mv8k). Enabling
  Anthropic automatic caching put a single `cache_control` at the top level of the
  request body, but the cache-zone guard only recognised breakpoints placed on a
  block, so it concluded nothing was cached and let the content stages rewrite the
  whole history on every turn. Tool-output windowing is scored against the short
  segments of the request, and that set grows with each turn, so an old tool result
  came back windowed differently, the cached prefix changed mid-history, and the
  cache hit truncated at the first altered block while everything after it was
  re-billed at the 1h write rate (2x base input). A top-level marker now freezes the
  system prompt and every message except the newest, which is the one the provider
  has not seen yet and so is still ours to compress, exactly once.

- Anthropic models reached through OpenRouter are cached (dirge-mv8k). OpenRouter
  forwards to whichever provider owns the model, and Anthropic caches nothing without
  an explicit breakpoint, so a Claude route was re-billing the full system prompt and
  tool block every turn. Those requests now carry the top-level `cache_control`
  OpenRouter documents for automatic caching. Gated on the `anthropic/` vendor segment,
  since the field is an unrecognised argument to a genuine OpenAI-shaped endpoint and
  the other providers cache the longest matching prefix by themselves.

- Gemini's tool block is canonicalised (dirge-mv8k). Gemini nests its tools one level
  down as `tools[].functionDeclarations[]`, so the sort that makes the prefix
  byte-identical across restarts saw a single unnamed element and left the declaration
  order as the SDK emitted it. Gemini's implicit cache is pure prefix matching, so a
  tool block that reshuffles between runs never matched the earlier prefix. Ordering
  the declarations is the one caching lever that provider has.

- The `prompt-caching-scope-2026-01-05` beta is no longer sent (dirge-mv8k). It only
  permits a `scope` field inside a `cache_control` block, which dirge never sends, so
  it bought nothing while making every request unsendable through an
  Anthropic-compatible relay that does not recognise the value: those reject the whole
  request rather than ignoring the flag.

### Added
- Kimi Code (Moonshot) is a supported provider, with a device OAuth login: `dirge auth
  kimi` (alias `dirge auth kimi-code`) prints a verification URL and code, then stores
  the credential under the `kimi` key in the same `auth.json` the other OAuth providers
  use. Set `provider = "kimi"` (aliases `kimi-code`, `moonshot`) to use a Kimi
  membership's managed coding API instead of a raw API key. Models are `k3` (default),
  `kimi-for-coding`, and `kimi-for-coding-highspeed` against
  `https://api.kimi.com/coding/v1`, with a 262144-token context; tiers without K3 access
  want `--model kimi-for-coding`. Access tokens last 15 minutes and are refreshed
  automatically mid-session. `KIMI_CODE_API_KEY` takes a static bearer instead of the
  OAuth login, and `KIMI_CODE_BASE_URL` / `KIMI_CODE_OAUTH_HOST` override the endpoints.
  The separate Kimi Platform API (`api.moonshot.ai`) is not covered. See
  [config.md](docs/config.md#kimi-code-moonshot-oauth).

- Qwen and Gemini routes through OpenRouter are cached (dirge-gcxb). Both upstreams
  need an explicit breakpoint like Anthropic does, but neither accepts the top-level
  form, so they get a `cache_control` on the end of the system message instead. A
  string `content` is lifted to a block array to carry it. No TTL, since the 1h
  option is Anthropic-only. Gemma and the other families that cache the longest
  matching prefix on their own are left untouched.

- The Anthropic prompt-cache TTL is configurable (dirge-cbgz), via
  `[prompt_cache] ttl` or `DIRGE_PROMPT_CACHE_TTL`, as `"5m"` or `"1h"`. The default
  is unchanged at 1h. A 1h cache write costs 2x the base input rate against 1.25x
  for 5m, while reads are a tenth either way, and a read refreshes a 5m entry. So a
  continuously active session stays warm on 5m and never pays the premium, while 1h
  earns it across idle gaps longer than five minutes, when a lapsed entry means
  re-writing the whole prefix instead of the turn's delta. See
  [config.md](docs/config.md#prompt-caching).

### Changed
- The Anthropic OAuth transport parses each request body once instead of twice. The
  beta-header decision reads `thinking.type` and the payload shaping needs the same
  JSON, and an agent turn's body is the largest allocation on that path.

- Provider clients build their compression interceptor through one helper rather than
  repeating the same four arguments at each of fourteen arms. The wire shape a body is
  parsed as is now derived from the provider kind in one place, which is also where
  the per-backend caching policy above is applied.

## [0.19.26] - 2026-07-28

### Fixed
- The dead-tty watchdog no longer kills every macOS TUI session shortly after
  startup (#731; thanks @nikolap). The watchdog added in 0.19.24 polls
  `/dev/tty` every 250ms and treated `POLLNVAL` as terminal death — but on macOS
  `/dev/tty` is the controlling-terminal redirect device and *always* reports
  `POLLNVAL` to `poll(2)`, healthy terminal or not. So dirge ran its SIGHUP
  teardown and exited 129 on a perfectly live tty. 0.19.24 and 0.19.25 are
  effectively unusable on macOS; upgrade past them.

  `tty_is_dead` now ignores `POLLNVAL` and adds an `ioctl(TIOCGWINSZ)` probe: a
  hung-up line discipline fails that ioctl with `EIO`, which covers the
  `/dev/tty` hangup that poll can't see there. pty secondaries still report
  `POLLHUP` as before, so the hangup teardown 0.19.24 added is unchanged.

## [0.19.25] - 2026-07-28

### Fixed
- dirge no longer aborts when its terminal goes away (#730). `tty_size()`
  reports 0x0 once the tty is gone, so ratatui allocated a 0x0 buffer while the
  layout — built from the cached size — still handed widgets a full-size rect.
  The first `buf[(x, y)]` write was out of bounds and killed the process with
  `index outside of buffer: the area is Rect { 0, 0, 0, 0 }`, exit 101, before
  `signal.rs` could run its teardown and reap the detached child groups (LSP,
  MCP, DAP, bash subtrees) that a hangup otherwise strands.

  `paint_frame` had a `width < 2 || height < 2` guard already, but it tested the
  widget's rect rather than the buffer's, so it never fired. Every painter now
  clamps to `buf.area`, and `tui_redraw` skips the frame outright when the
  terminal reports a zero dimension — that second guard keeps the whole widget
  tree out of the degenerate case instead of trusting each painter. dirge exits
  through the normal error path now, so `Drop` cleanup runs.

- A narrow terminal could panic the permission/questionnaire overlay.
  `paint_overlay_box` computed `area.width as usize - 2` with no lower bound, so
  any overlay rect under two cells wide underflowed. Found while fixing the
  above; it needs only a very narrow terminal, no hangup.

## [0.19.24] - 2026-07-28

### Fixed
- The PTY relay no longer spins at 100% CPU when the tty hangs up (#728).
  0.19.16 changed the tty read's `Ok(0)` arm from `break` to `continue`, and a
  hung-up tty polls `POLLIN|POLLHUP` and reads 0 forever — so the loop made no
  progress while pinning a core, and the `continue` skipped the `POLLHUP` branch
  that kills the child. Two relay threads in that state burned 17h of CPU on an
  orphaned process. EOF is now terminal and kills the child; a bare `break` is
  not enough, because the relay holds the PTY secondary open so the child never
  sees EOF and `wait()` would block forever.

- `POLLNVAL` is treated as fatal on both relay fds. `relay()` switched from
  opening `/dev/tty` to `dup(0)` in the same release, so a backgrounded dirge
  gets `/dev/null` as its tty, where poll reports `POLLNVAL` — matching neither
  the `POLLIN` arm nor the hangup branch, and spinning with nothing to do. Both
  relay defects shipped in 0.19.16.

- The input reader no longer pins a core when the terminal goes away.
  crossterm 0.29 never returns from `event::poll()` once the tty is gone — its
  internal read loop retries on EOF/EIO with no timeout check
  (crossterm-rs/crossterm#793) — so the reader thread spun and never got back
  to re-check its shutdown flag, which also wedged `join_reader` and the TUI
  suspend path.

  Three changes. The reader probes the tty (`POLLHUP`/`POLLERR`/`POLLNVAL`)
  before each poll. It now polls with a zero timeout and owns the 1ms wait
  itself, so crossterm holds the thread for microseconds rather than a
  millisecond, shrinking the window in which a dying tty can trap it by roughly
  three orders of magnitude; idle CPU is unchanged. And a watchdog thread
  checks every 250ms, running the same teardown `signal.rs` does for SIGHUP
  when the tty is gone — dirge already means to exit on hangup, but an orphaned
  background process never receives the signal. Headless modes have no probe fd
  and never self-exit.

  This does not fix the upstream bug; crossterm-rs/crossterm#1067 is the real
  fix. Until it lands, the window is narrow and the watchdog guarantees dirge
  cannot spin indefinitely.

### Changed
- Relay byte counters are gated behind `timing-diagnostics` again. They were
  un-gated in 0.19.16, so release builds wrote to stderr on every relay exit and
  ran two `assert!`s inside a `Drop` impl.

- `clippy -D warnings` is clean under `sandbox-microvm` (50 findings across 7
  files that default CI never saw). Mechanical only, but `tests.rs`'s inner
  module is renamed `tests` -> `integration`, moving test filters to
  `sandbox::microvm::tests::integration::*`.

## [0.19.23] - 2026-07-27

### Fixed
- A model change now rebuilds the client that serves it (#711). A
  `task(agent=…)` profile pinning a model from another family was built against
  the ACTIVE client, so `model: glm-5.2` under a Codex/ChatGPT client was POSTed
  to the ChatGPT endpoint and rejected on the subagent's first turn — the common
  "main session on a subscription, cheap subagents on a separate provider" setup
  failed 100%. `/agent <name>` had the same defect on the main session, and
  `/agent off` restored the pre-agent model without restoring the client it came
  from. `/model` and the plugin model swap already cross-routed, each with its
  own copy of the logic.

  Changing the model and changing the client are one operation, so all five
  paths now share one seam that keeps them together. `/agent off` restores the
  captured (provider, model) pair rather than re-deriving it from the id, which
  can pick a different alias of the same family or refuse outright when the
  pre-agent provider was a built-in with no `providers` entry.

- A `vendor/model` id no longer reroutes away from the gateway that serves it.
  The family inference behind `/model` read `anthropic/claude-opus-4` as an
  Anthropic id, so on an OpenRouter session with an Anthropic provider also
  configured it switched to the direct Anthropic client, which rejects the
  prefixed id. A prefixed id now stays on an active provider whose own pinned or
  default model is prefixed too.

- Cross-provider switching picks its target deterministically. When two aliases
  pinned the same model id the choice came from `HashMap` iteration order, so
  the same `/model` command could land on a different provider between runs.

- The Codex `/responses` streaming path sends `Content-Type` again (#722), which
  #718 dropped when it re-routed streaming through a header-preserving seam.

### Changed
- Failed LLM requests now say what the provider said. A rejection fails before
  any event streams and its body carries the only actionable detail — which
  model, which account, which limit — but only the classified error kind was
  logged, at debug, and the message surfaced downstream as a capped
  `[task <id>] failed: …`. The body is now logged at warn with the provider and
  model that produced it, stream errors carry their message alongside the kind,
  and each retry attempt's decision is recorded at info.

## [0.19.22] - 2026-07-26

### Fixed
- Retry logic now honours rate-limit reset headers instead of backing off blind
  (#718). OpenRouter's 429 carries no `Retry-After` — the reset is epoch millis
  in the body, under `error.metadata.headers.X-RateLimit-Reset` — and dirge
  never parsed it, so every 429 fell through to plain exponential backoff. That
  hurt twice. The per-minute budget (~31s over five retries) expired before a
  42s window rolled, and the rate-limit-to-usage-cap promotion never fired for a
  `free-models-per-day` 429 whose reset was 15h out, so a daily cap got retried
  like a transient blip. In the reported log 118 of 154 requests were 429s, and
  the retries alone burned the account's 50/day free quota in under three
  minutes.

  `Retry-After` parsing now also reads `x-ratelimit-reset` and the
  `anthropic-ratelimit-*-reset` family, telling epoch millis, epoch seconds,
  relative seconds, Go durations (`2m59.56s`, `6m0s`) and RFC 3339 apart by
  shape. A reset pairs with the dimension whose `remaining` hit 0, so an
  exhausted request window no longer makes dirge wait out an unrelated token
  window. An explicit `Retry-After` still wins, and an already-elapsed reset is
  not treated as hard data — it falls through to the wording heuristic, which
  now recognises `per-day` phrasing.

  When a provider definitively reports a window exhausted, further requests to
  that host are answered locally instead of sent. The wasted requests came from
  independent retry budgets — agent turn, run-level recovery, summarizer,
  post-session — that could not see each other being throttled.

### Changed
- Provider rate-limit headers now survive the streaming path. rig flattens any
  non-2xx into status plus body text and drops the header map before dirge sees
  it, and streaming carries every provider request — so providers that report
  their limits only in real headers (OpenAI's `x-ratelimit-reset-*`, Groq's
  `retry-after`, Anthropic's `anthropic-ratelimit-*-reset`) were invisible.
  Only OpenRouter, which nests its headers in the 429 body, and Zhipu, which
  writes the reset into the message, were covered. For a plain reqwest client
  dirge now drives the streaming request itself: the success path mirrors rig
  exactly, and only the non-2xx branch differs, keeping the headers and
  reproducing rig's error verbatim.

## [0.19.21] - 2026-07-26

### Fixed
- A pending question to the user now outranks the finalization gates (#717).
  Ask the agent something mid-task, let it reach a point where it needs your
  decision, and the critic would keep reminding it about unfinished work until
  it gave up waiting and guessed. Nothing downstream could tell a turn that
  stopped to ask from a turn that stopped because it was done — every prompt
  tells the model to clarify in prose, and the `question` tool blocks and
  resolves in-turn, so it never reaches finalization. Each gate then re-entered
  on its own budget: critic 1-3x, todo 3x, open-issues 2x.

  A turn that ends by asking now finalizes and hands control back. Detection is
  a trailing question mark on the last meaningful line, after dropping trailing
  option-list lines (so a question followed by a multiple-choice block still
  counts) and markdown decoration; an unterminated code fence is left alone.
  The goal gate is the one exception and still runs, since `--goal` is an
  explicit autonomous stop condition and there may be no user present to
  answer.

  The unfinished-todo and open-issues nudges now also require the turn to have
  edited a file. The todo count comes from a cross-turn process-global mirror,
  so a read-only Q&A turn was being nagged about stale coding todos left over
  from before the interruption.

## [0.19.20] - 2026-07-25

### Fixed
- Pre-write syntax gate (#716), four defects that between them made Lisp files
  hard or impossible to edit:
  - A raw NUL byte anywhere in a file blocked every edit to it, permanently.
    tree-sitter can't distinguish NUL from end-of-input, so the file parsed as
    truncated and the first form was flagged at a line/col unrelated to the
    edit. A NUL inside a string or regex is valid Clojure (there is no `\0`
    escape) and occurs in real code. Such content now skips the grammar and
    falls through to the delimiter scanner, which handles it correctly.
  - The mechanical delimiter repair could silently re-nest code. It appends
    the missing closers at EOF and re-validates with tree-sitter, which proves
    nothing for a Lisp — any balanced arrangement parses. A closer dropped
    mid-form was "repaired" into a file where a following top-level form had
    been swallowed by the one above it. The repair now refuses when closing at
    EOF would pull a column-1 opener inside the form being closed.
  - The delimiter hint named the outermost unclosed opener, which in a Lisp is
    almost always the enclosing top-level `defn` rather than the mistake. It
    now leads with the innermost opener and keeps the outermost as context.
  - The gate had no pre-edit baseline, so a file that already failed the check
    could not be edited at all — including files that legitimately never parse,
    such as templates with `<% %>` markers or linter fixtures. When the file
    was already broken before the edit, the gate now stands down with a warning
    instead of blocking, and does not auto-repair on top of it.

## [0.19.19] - 2026-07-24

### Added
- ACP slash commands (#714): the ACP bridge now advertises a curated set of
  slash commands to editor clients via `available_commands_update` after a
  session is created, and handles them locally instead of forwarding them to
  the model: `/help`, `/clear`, `/cd`, `/model`, and `/mode`. Command names are
  advertised without the leading `/` (the client adds that affordance).
  `/model` and `/mode` switches are session-scoped and persist across turns.
  TUI-only commands (`/panel`, `/tree`, `/fork`, …) aren't advertised because
  they have no meaning without the interactive UI; any unrecognized `/…` input
  still flows through to the model unchanged.
- `no-plugin` build feature (#712, #713): the default feature set minus
  `plugin`, for a source build that needs no C/LLVM toolchain (the `plugin`
  feature compiles Janet from C via bindgen, which needs a C compiler +
  libclang). Install with `cargo install dirge-agent --no-default-features
  --features no-plugin`. `windows-default` now aliases to it.

### Security
- Bumped russh 0.61 → 0.62.4, clearing three advisories (GHSA-5xvq-cp9x-6p6r,
  GHSA-cqjc-rmpq-xprq, GHSA-g9hv-x236-4qp3) against the SSH client used by the
  optional `sandbox-microvm` feature. The API is unchanged across the bump.

## [0.19.18] - 2026-07-23

### Added
- Subagent MCP tools (#701): an agent profile can grant its `task(agent=…)`
  subagent selected MCP tools with `subagent_mcp` — `all` for every connected
  MCP tool, or a list of names. Resolved against the live MCP-tool set at fork
  time (so background-loaded servers are covered) and validated against it, so
  it can only ever add real MCP tools, never a built-in past the tier cap.
  Honored on the `readonly`/`readwrite` tiers.
- `openai-responses` provider type (#703): a custom OpenAI-compatible provider
  can target the Responses API (`/v1/responses`) instead of chat/completions by
  setting `provider_type: "openai-responses"`. Uses a plain api-key bearer
  against the configured `base_url` — no OAuth/Codex login — so it works against
  any `/v1/responses` server; `allow_insecure` is honored.
- Optional `/plan` approval gate (#622): with `phased_workflow_plan_approval`
  on, the phased workflow pauses after the plan phase to approve / edit / cancel
  before any code is written. Edit takes free-form feedback and re-plans, so you
  can steer the plan before implementation. Default off (implement immediately).
- `code_mode_rubric` config flag (default off): appends a "code mode" rubric to the
  system prompt telling the model to collapse a bulk/fan-out of similar tool calls
  (roughly 10+ items) into ONE `bash` script that returns only the distilled result,
  keeping raw per-item output out of context. A/B measured on deepseek-flash with
  the reproducible harness `scripts/code-mode-ab.sh` (three scenarios). The payoff
  scales with how much naive fan-out the model would do without it: on an
  already-greppable aggregate it is ~flat (nothing to capture); on an obvious
  count task ~19% fewer input tokens; on a task that forces genuine per-file
  fan-out (two markers on different lines, no single-line grep possible) −45%
  input tokens, tool calls 8.0 → 4.5, and correctness up from 5/8 to 8/8 (scripting
  the intersection beats eyeballing 40 files). Methodology in
  `docs/code-mode-rubric.md`.

### Fixed
- Headless `-p` / `--loop` sessions now persist provider token usage. `run_print`
  dropped `AgentEvent::Usage` on the floor, so every headless session saved
  `cumulative_input_tokens = 0`, breaking `/cache` on resumed sessions and any
  token accounting off the session file. It now folds per-turn usage into the
  session like the interactive UI does.

## [0.19.17] - 2026-07-18

### Fixed
- Provider usage-cap 429s (e.g. GLM/Zhipu's 5-hour limit, error code 1308) are no
  longer retried as transient rate-limits. Their quota resets hours out while
  backoff caps at 5 minutes, so retrying only re-hit the cap, burned the retry
  budget, and killed the run mid-task. They're now classified as a non-retryable
  usage cap; headless surfaces it as a distinct, resumable outcome (envelope
  subtype `error_usage_cap`) that prints the reset time and a `--continue` hint,
  with the session already saved (#689).

## [0.19.16] - 2026-07-18

### Added
- macOS microVM sandbox backend via libkrun on Hypervisor.framework: a pure-Rust
  OCI image puller (blob digests verified, tar entries path-checked), ephemeral
  SSH host-key generation with exact host-key verification, PTY-based attach, and
  runtime codesigning of the runner with the hypervisor entitlement. See
  `docs/microvm/SETUP.md` for prerequisites. Opt-in behind the `sandbox-microvm`
  feature (#688).

## [0.19.15] - 2026-07-18

### Added
- Custom per-provider HTTP headers via `providers.<name>.headers` (a
  name → value map). A value that is exactly `${ENV_VAR}` is resolved from the
  environment; header names and values are validated before the client is built
  (#686).
- `--verbose` now logs HTTP request method/URI/status and classifies stream and
  retry errors (abort, timeout, network, auth, rate-limit), making API failures
  easier to diagnose. The logged URI has its query string stripped so a
  credential embedded there (e.g. Gemini's `?key=`) never reaches the logs
  (#685).

## [0.19.14] - 2026-07-18

### Added
- Cerebras as a first-class provider. Set `CEREBRAS_API_KEY` and select it with
  `--provider cerebras` (or a `providers.cerebras` entry); defaults to
  `gemma-4-31b` against `https://api.cerebras.ai/v1`. Reasoning effort is clamped
  to `low`/`medium`/`high`, and image input is model-specific (`gemma-4-31b`
  only). `CEREBRAS_API_KEY` is isolated from other providers (#683).

## [0.19.13] - 2026-07-17

### Fixed
- Blocking code-review mode no longer repeats the same finalization message. The
  review judge is stateless and re-read the run's diff on every finalization, so
  when the model declined a finding and changed nothing on disk the identical
  finding was re-raised and the model repeated its rebuttal. The judge now skips
  an unchanged diff (falling through to the goal/todo gates) and, on a changed
  diff, is given the prior findings so it re-raises one only when it's still
  present and the model neither fixed nor justified it (#681).

## [0.19.12] - 2026-07-17

### Fixed
- A parent turn that dispatched coordinated background subagents no longer runs
  the completion critic — or any lower "are we done?" gate — while that batch is
  still running. It waits for the batch to finish and reconcile instead of being
  judged prematurely; the turn resumes when results are deliverable (#679).
- A blank or whitespace-only `retry_of` on a `task` call is now treated as
  omitted, so a generated tool call carrying an empty retry id no longer enters
  coordinator retry validation for a nonexistent task. Real ids are trimmed and
  still validated strictly (#679).

## [0.19.11] - 2026-07-17

### Added
- `keyboard_enhancement` config (default on). Enables the terminal's enhanced
  keyboard (kitty) protocol on terminals that support it — kitty, Ghostty,
  WezTerm, foot, rio — so distinct chords like Shift+Enter reach the input
  editor instead of collapsing onto plain Enter. A no-op on terminals that
  don't advertise support; set `false` to disable.

### Changed
- Shift+Enter now inserts a newline in the input box instead of submitting, so
  you can write multi-line prompts. The newline gesture is a rebindable
  `insert_newline` command (defaults Shift+Enter, Alt+Enter, Ctrl+J) rather than
  a hardcoded key, so it can be remapped from `keybindings`. Alt+Enter and
  Ctrl+J work in every terminal; Shift+Enter needs `keyboard_enhancement`. Plain
  Enter still submits. Home/End continue to move within the current line.

## [0.19.10] - 2026-07-16

### Added
- Coordinated subagent dispatch. The `task` tool can fan out background
  subagents as a batch and deliver one reconciliation update once the whole
  batch reaches terminal state, routed through configured `readonly` /
  `readwrite` agent profiles. Read-write subagents run isolated in rooted Git
  worktrees when available; `auto` mode warns and serializes writers in the
  parent checkout when no worktree can be created. Reconciliation preserves each
  writer's branch, path, commit, dirty-state, and salvage metadata. See
  `docs/subagent-dispatch-strategy.md` (#670, resolves #660).

### Changed
- The post-turn code reviewer is now folded into the completeness critic as a
  single finalization judge. One judge call both checks the task is done and
  reviews the run's diff for defects, then re-enters the loop with one
  consolidated follow-up the agent acts on. Previously the default `advisory`
  mode ran detached and surfaced findings — even high-severity ones — as a
  display-only notice the model never saw, so a review that flagged a real bug
  right after the agent said "done" changed nothing. Now any finding re-enters
  (high/critical as must-fix, medium/low as optional notes); `advisory`
  re-enters once, `blocking` persists until the diff is clean, `off` reviews
  completeness only.

### Fixed
- `/model <id>` now routes a free-form model id to the right provider. It used
  to switch the live client only on an exact match against another provider's
  pinned `model`; any other id (a version bump, a typo, `glm-4.6`) was renamed
  on the active client and its next turn hit the wrong endpoint. Ids are now
  matched by family (`glm-*`, `deepseek*`, `claude-*`, `gemini-*`, `gpt-*`) and
  routed to a configured provider of that kind; an id whose family has no
  provider is refused with a warning instead of silently mis-routing. The
  session banner also reads the live provider/model instead of re-resolving
  from CLI/config, so a switch or resume is reflected.
- A plugin `prepare-next-run` model swap can now hop providers. The swap builds
  and installs the target provider's client in place, so a follow-up turn
  actually runs there; it no longer clobbers `session.provider` on a
  same-client rename.

## [0.19.9] - 2026-07-16

### Added
- Prompt compression. dirge compresses outgoing request bodies with a
  deterministic, no-model transform (an inlined build of llmtrim-core) before
  they reach the provider: tool-output windowing for logs/diffs/grep plus
  lossless JSON/columnar packing, guarded by a token gate that reverts any
  change that doesn't pay off and cache-zones that keep provider prompt caching
  warm. On by default for every provider; disable with `DIRGE_COMPRESSION=0` or
  `[compression] enabled = false`, or choose a profile with `[compression]
  preset`. Tool-heavy turns typically shrink ~60%.

### Fixed
- The agent no longer breaks or repeats itself on phantom tool calls. When a
  reasoning model left tool-call-shaped JSON in its answer text, the scavenger
  promoted it to a real call; if the args didn't validate, the error result
  could desync the message history into a provider 400 and a duplicated
  response. Scavenged calls are now validated before promotion and dropped when
  invalid; native tool-call errors are unchanged.
- The OpenCode provider (`opencode.ai/zen`, which proxies to DeepSeek models)
  sent the wrong reasoning-control keys — the vLLM/SGLang self-hosted keys
  (`reasoning_level`, `chat_template_kwargs`) that DeepSeek's hosted API ignores
  — so deep-thinking mode and one-shot reasoning-disable didn't take. It now
  uses `reasoning_effort` + `thinking: {type: disabled}`, matching the built-in
  DeepSeek provider (#671).

### Changed
- Work tracking nudges earlier. After the first file edit with no active todo,
  dirge injects a model-visible reminder to `write_todo_list` and mark the item
  in_progress, instead of only a user-facing notice at the end of the turn.

## [0.19.8] - 2026-07-15

### Changed
- Issues and todos are now distinct. Creating an issue files it to a passive
  backlog (unassigned) instead of the active work queue, so filing something
  for later no longer trips the finish-your-work nudge (the loop the model got
  stuck in when you asked it to "add an issue for X"). `issue start` claims a
  backlog item onto your active queue; `write_todo_list` still writes active
  work. The turn-start reminder is split into an "Active work queue" section
  (with a "Currently in progress" callout) and a "Backlog" section, so the
  model has a clear current task each turn (#668).
- Issue ids are now beads-style hashes (`drg-a1b2`) instead of integers.
  Existing issue databases migrate on first open.

### Added
- Issues can be grouped under an epic: `issue create epic=<id>` files under a
  parent, and `issue show <id>` lists an epic's children.
- An advisory nudge when the model edits files without any tracked todo, so
  active work reliably lands on the list.

### Fixed
- Restating a todo as completed with different casing or spacing now closes the
  existing item instead of leaving the finished task stuck on the board behind
  a duplicate.

## [0.19.7] - 2026-07-14

### Fixed
- LSP servers (rust-analyzer and friends), MCP servers, DAP adapters, and
  background shells were orphaned when dirge itself was killed by a signal —
  SIGTERM (`kill`), SIGHUP (terminal or tab closed), or SIGINT. They are
  spawned with `setsid` so terminal-delivered signals never reach them, and
  the `kill(-pgid)` that reaps them only runs from a guard's `Drop`, which a
  signal exit skips. A leaked rust-analyzer kept indexing the workspace and
  held 1 GB+ of RAM. dirge now tracks every detached child process group and,
  on SIGINT/SIGTERM/SIGHUP, kills them all, restores the terminal, and exits
  (dirge-6klk).
- LSP servers were SIGKILLed on shutdown without the `shutdown`/`exit`
  handshake, so they never got to flush or persist state. dirge now sends the
  handshake on the normal teardown path, falling back to the group kill
  (dirge-8m69).

## [0.19.6] - 2026-07-14

### Added
- `animations_enabled` config toggle to turn off UI animations (#664).

### Fixed
- The TODOS panel went empty after a compaction fold or a resume and did not
  come back when the agent picked an existing issue up. It was scoped to the
  live `session.id`, which a fold rotates, so the board query stopped matching
  the rows it had stamped under the old id; and starting an issue created in
  another session never pulled it onto the current board. The board is now
  scoped by the stable lineage origin (so it survives folds/resumes), and
  starting an issue (`issue start` / an update to `in_progress`) claims it for
  the current conversation so a picked-up issue appears (GH #663).
- Redacted secrets stayed searchable in the session transcript index: the v6
  FTS redaction rebuild cleared the external-content index with a plain
  `DELETE`, which left phantom postings, so secrets in folded `tool_calls`
  remained matchable. It now uses the FTS5 `'delete-all'` command
  (dirge-vpma.4).
- Opening the composer in an external editor (Ctrl+G) destroyed text and image
  pastes held in the buffer; they are preserved and re-attached now
  (dirge-vpma.5).
- A corrupt project `config.json` could be rewritten with only the sandbox
  keys, dropping the rest of the configuration (dirge-vpma.6).
- Resizing the terminal could wipe a background subagent's transcript because
  inactive-chat writes skipped their source buffer (dirge-vpma.7).
- Setting one DAP breakpoint replaced the file's whole breakpoint set (DAP
  replace semantics), so adding or removing one dropped the rest; incoming
  breakpoints now merge with the cached set (dirge-vpma.12).
- `--loop-max N` ran N-1 iterations (and zero for `N=1`) (dirge-vpma.15).
- Ctrl+K killed to the end of the whole buffer instead of the end of the line
  (dirge-vpma.16).
- Deleting a session removed only a single file, leaving fold-chain rotations
  resumable; the whole lineage is removed now (dirge-vpma.13).
- Three P1 crash / data-loss bugs: a shell-exit drain race could start a second
  agent run; restoring after removing a chat could overwrite surviving history;
  and the checkpoint-reuse fold spliced snapshots at the wrong index (#665,
  dirge-vpma.1/.2/.3).
- Further Wavescope entropy-audit correctness fixes across the compaction hook
  path, stream-abort racing, plugin session adoption, and rewind bucketing
  (dirge-vpma.8/.9/.10/.11/.14/.19).
- External-editor buffers now open with the markdown filetype (#662).

### Changed
- The TODOS panel now marks the issue the agent is actively on: the
  `in_progress` item is pinned at the top (it already sorted first) with a `▶`
  marker and a distinct focus color, so the current task stands out from the
  queued work. The panel still mirrors this session's live issue board; use
  `/issues` for the full project board (GH #663).
- Project-scoped `.dirge/plugins/`, `.dirge/config.json`, `.dirge/prompts/`,
  and `.dirge/agents/` are now resolved at the project root — the enclosing
  git repository, or `DIRGE_PROJECT_ROOT` when set — instead of the launch
  directory. Previously these four looked only in `<cwd>/.dirge/`, so starting
  dirge from a subdirectory of a repo silently loaded none of them, even
  though the session database and per-project memory already anchored at the
  repo root. Launching in `<repo>/sub/` now loads `<repo>/.dirge/` config,
  plugins, prompts, and agent profiles, matching where sessions and memory
  live. Skills already walked up the ancestor chain (merging inner-over-outer),
  so their behavior is unchanged (dirge-vpma.17).

## [0.19.5] - 2026-07-13

### Fixed
- AltGr-composed characters (`@ # [ ] { }` on the Italian, German, Spanish, …
  layouts) could not be typed or pasted into the input box on Windows. Windows
  reports AltGr as Ctrl+Alt, so those keystrokes arrived with both modifiers
  set and the editor's text-insertion path — which ignored anything Ctrl- or
  Alt-modified — dropped them. Pasting lost the same characters because the
  Windows console backend delivers a paste as synthesized keystrokes. `@`
  needing AltGr also meant the `@`-file picker could never open. The editor now
  folds an AltGr-composed printable character back to plain text before
  dispatch; real Ctrl/Alt keybindings are unaffected (GH #659).
- The input box was unreadable on a light terminal: the composer text is white
  and relied on the theme's dark background fill, which `plain` (and any
  light/`reset` background) doesn't paint, so it vanished against a light
  terminal. The input box now paints its own dark surface (`input_bg`, default
  `#222222` on both presets) over the background fill, keeping it a dark field
  regardless of the terminal theme. Themeable via `input_bg` in a custom theme;
  set `"input_bg": "reset"` to opt out (GH #628).

## [0.19.4] - 2026-07-09

### Fixed
- A transient transport error ("error decoding response body", network blip,
  rate-limit) arriving after the model had already streamed content killed the
  whole run: the streamed partial was erased from the transcript (the turn
  finalized with empty content), the run tore down to idle, and any steering
  queued mid-run was dropped. The stream layer now preserves the last streamed
  partial (text + thinking, with incomplete tool-call blocks stripped so they
  don't orphan the `tool_use`), and the run loop treats a transient error as
  recoverable — the partial stays in the transcript, a continue nudge drives
  the next turn, a retry banner surfaces, and queued steering is delivered.
  Bounded by three consecutive recoveries; explicit cancel and non-transient
  errors (auth, context-length) still terminate as before (#626).

## [0.19.3] - 2026-07-08

### Added
- Opt-in desktop notifications on macOS, Linux, and Windows, backed by
  `notify-rust`: a notification banner on macOS, a freedesktop D-Bus
  notification on Linux (pure-Rust `zbus`, no libdbus dependency; needs a
  running notification daemon), and a WinRT toast on Windows. Enable with
  `desktop_notifications.enabled`; notifies on run completion and when a
  permission prompt, question, or plugin dialog is waiting for input. Extends
  the macOS-only backend from #611 to all three platforms (GH #594).

### Fixed
- Background review left the avatar showing the idle `(^_^)` face while the
  runner was still busy (the `/plan` reviewer or a plugin hook chain keeps
  `is_running` true after a turn's visible output ends). Since `is_running`
  also routes a typed message to send-vs-queue, messages typed against the
  idle-looking avatar were queued behind the busy runner and could be dropped.
  The idle face is now deferred until the turn actually settles idle, so the
  indicator matches the real run state (dirge-79ue, GH #621).
- Three `cargo audit` advisories in the dependency graph: `anyhow`
  (RUSTSEC-2026-0190), `crossbeam-epoch` (RUSTSEC-2026-0204, via an `ignore`
  pin), and `quinn-proto` (RUSTSEC-2026-0185) bumped to fixed versions (#623).

## [0.19.2] - 2026-07-08

### Fixed
- `cargo install dirge-agent` failed to compile against rig. rig ships
  breaking API changes in patch releases (0.37.1 added a required
  `Text.additional_params` field), and `cargo install` resolves without the
  lockfile, so the caret dependency drifted onto an incompatible rig. rig and
  rig-core are now pinned to an exact version so published installs build
  against the rig dirge was tested with (dirge-omgq, GH #616).
- `dirge auth openai` (or `dirge auth anthropic`) alone wasn't enough to
  launch dirge: provider selection consulted only `--provider`/config and
  API-key env vars before defaulting to OpenRouter, so a user who logged in
  via OAuth but set no env key was still asked for an OpenRouter key. Provider
  resolution now falls back to a stored `dirge auth` login before the
  OpenRouter default (dirge-ppie, GH #617).

## [0.19.1] - 2026-07-07

### Fixed
- Slow startup / laggy rendering on terminals that echo focus events (e.g.
  Ghostty). Such terminals reply with a `FocusGained` whenever focus reporting
  (`?1004h`) is enabled; dirge's `FocusGained` handler re-armed the terminal
  modes — including `?1004h` — unthrottled, so the reply drove an unbounded
  re-arm → echo → re-arm loop that saturated the UI loop and delayed output
  (MCP server logs could take ~a minute to appear). The mode-only re-assert is
  now throttled the same way the full re-assert already was. Terminals that
  don't echo focus (e.g. iTerm2) were unaffected either way (dirge-np9o).

## [0.19.0] - 2026-07-07

### Added
- Paste a clipboard image into the prompt (Ctrl+V) and send it to a
  multimodal provider. User messages are now multipart, so a turn can
  interleave text and images. Images are stored once under the session's
  `assets/` dir and re-read at the provider boundary each turn they stay
  in context, keeping transcripts small. The paste UX is gated by whether
  the active provider/model supports vision (with a `multimodal` override
  on the provider entry for local vision models). Clipboard capture uses
  the platform's built-in scripting host — `osascript` on macOS,
  PowerShell on Windows — or `wl-paste`/`xclip` on Linux; no image on the
  clipboard falls back to a normal text paste.

### Fixed
- A caption-less image paste (image + Enter, no text) no longer aborts the
  turn — the empty text part ahead of the image was serializing to an empty
  text content block that Anthropic rejects with a 400. Empty text parts are
  now dropped at the provider boundary and the seed.
- Pasted images no longer drop out of context on continuation, interject,
  retry, plan-review, and compaction-resume turns; those paths were replaying
  history without the session asset dir, degrading each image to a placeholder.
- DeepSeek vision models (`deepseek-vl*`) are no longer misclassified as
  text-only, so the image-paste UX is offered for them.

## [0.18.13] - 2026-07-07

### Fixed
- Render-loop CPU waste (three fixes from a repaint-loop audit):
  - A no-op or duplicate terminal resize no longer triggers a full scrollback
    re-render. crossterm — and terminals that fire a resize event on focus and
    other non-size changes — emit resizes with unchanged dimensions; each was
    doing a full markdown re-parse + re-wrap of the whole scrollback. Resizes
    now carry the new dimensions and rebuild only when the size actually
    changed (dirge-8p0h).
  - The terminal size is cached instead of re-`open()`ing `/dev/tty` ~8× per
    paint (via panel visibility / width / layout probes). It's refreshed at the
    top of each paint and on resize — the size only changes on resize
    (dirge-jisr).
  - Ambient panel data (session tokens, git, tool activity) is recomputed at
    most every 100ms instead of on every event-loop iteration; during token
    streaming the loop wakes 50–200×/s, so the per-token recompute was pure
    waste. Keystrokes, paints, and the input editor are unchanged (dirge-b14h).

## [0.18.12] - 2026-07-07

### Fixed
- Continuous TUI strobe on gnome-terminal / VTE 0.76 (Linux). The `FocusGained`
  recovery re-entered the alternate screen (`?1049h`), which re-enters *and
  clears* it — and on VTE, processing `?1049h` emits a synthetic `FocusGained`
  as a side effect, so focus-in fed itself into a strobe (and flashed on every
  alt-tab even after the 0.18.11 throttle bounded the loop). dirge never leaves
  the alt screen on a focus change, so re-entry was unnecessary; the focus-in
  recovery is now mode-only — it re-arms mouse capture, bracketed paste, and
  focus reporting without `?1049h` or a screen clear, then repaints. The full
  clear-and-re-enter recovery remains bound to the manual `Ctrl+L` hatch
  (dirge-1f2a). Reported against gnome-terminal 3.52 / VTE 0.76.

## [0.18.11] - 2026-07-07

### Fixed
- Throttle the full terminal re-assert to break a FocusGained feedback loop. On
  terminals that echo a focus event when the re-assert escape sequence is sent,
  the `FocusGained → force_terminal_reassert → …` recovery could re-trigger
  itself in a tight loop — rebuilding the ratatui backend, writing to
  `/dev/tty`, and dirtying a frame every iteration — spinning the event loop and
  causing visible jank. `force_terminal_reassert` now no-ops if it fired within
  the last 500ms, which is imperceptible for a genuine refocus or Ctrl+L but
  stops the loop. Thanks @allen-munsch (#606).

## [0.18.10] - 2026-07-06

### Fixed
- Mouse capture and text selection could stay dead until a manual `Ctrl+L`
  after an external terminal reset (a terminal `reset`, a tmux/screen
  detach+reattach, or a multiplexer that drops private modes). The automatic
  recovery is event-driven (`FocusGained` re-asserts the terminal modes), but
  that event only fires while focus reporting (`?1004`) is armed, and the
  periodic self-heal re-armed mouse + paste but not focus reporting — so once
  `?1004` was dropped the recovery went dark. The periodic reassert now also
  re-arms `?1004`, so focus reporting self-heals within one interval and the
  recovery keeps working (dirge-tc2q follow-up).

## [0.18.9] - 2026-07-06

### Fixed
- Delimiter auto-repair now handles an over-close, not just an under-close. The
  pre-write syntax gate already appended missing closers for a trailing
  truncation; it now also mechanically removes a trailing run of extra closing
  delimiters (an extra `)`/`]`/`}` at EOF) and re-validates. A mid-file stray
  closer is still rejected with its exact line:col, since removing it could
  change what the following forms belong to. This stops the model from dropping
  into hand-counting parentheses to fix an off-by-one imbalance. The DeepSeek
  steering prompt also now directs the model to create/modify source with
  `write`/`edit` (which validate and localize delimiter errors) rather than
  assembling files in a shell here-doc (dirge-1m36).

## [0.18.8] - 2026-07-06

### Added
- Prompt-injection scanning of untrusted tool results (file reads, MCP, web
  search). Detects role-override, summarisation-survival (instructions crafted
  to persist through compaction), link-exfiltration, and invisible-Unicode /
  Unicode-tag-block smuggling. Advisory by default (flagged content is fenced
  as untrusted data); opt-in hard-block via the `injection_scan` config
  (`off`/`advisory`/`block`). Read-scanning matches only the prompt-injection
  patterns, so ordinary source is not fenced (dirge-5ig9).
- Critic `ABSTAIN` verdict: when the spec and evidence don't let the critic
  establish correctness, it now asks the model to add a focused test (or state
  the missing detail) rather than false-passing or wrongly claiming the work is
  unfinished (dirge-ahyh).
- Opt-in open-issues finalization gate: nudges when this session left issues
  open, via the existing issue store (session-scoped, not the global backlog).
  Config `open_issues_gate` (`off`/`advisory`/`blocking`, default off); the
  blocking mode is bounded so it can't trap the loop (dirge-ksjl).
- Recurrence-weighted memory salience graduation: when the same learning has
  been stored as near-duplicate entries, the curator boosts the representative
  entry's salience so it's retained and surfaced. Non-destructive (nothing is
  merged or deleted) and recorded in a ledger so a cluster graduates once.
  Config `memory_graduation` (default on) (dirge-4nix).

### Changed
- Internal: `CodeReviewMode` generalized to a reusable `GateMode` primitive
  shared by the finalization gates (dirge-hsvw).

## [0.18.7] - 2026-07-06

### Fixed
- DeepSeek/GLM reasoning was not being controlled as intended, verified against
  the live hosted APIs. The tool-less one-shots (summarizer, critic, approval
  evaluator) tried to disable reasoning with `chat_template_kwargs` — which the
  hosted DeepSeek and GLM APIs silently ignore, so those calls kept reasoning
  and paid the latency. They now send `thinking:{type:"disabled"}`. And DeepSeek
  reasoning-effort was sent in the nested `reasoning:{effort}` shape it doesn't
  honor; it's now sent top-level as `reasoning_effort` (which it does honor) and
  can reach DeepSeek's `"max"` tier at the highest thinking level (dirge-r9k8,
  dirge-f1su).

### Changed
- Internal: per-provider reasoning wire tuning (enable + disable shapes, effort
  maps) is consolidated into a single `provider::adapter` module, so adding or
  retuning a provider's reasoning is a one-line change instead of edits across
  two files. Behavior-preserving (dirge-md5d).

## [0.18.6] - 2026-07-06

### Fixed
- Question modal: in a multi-select question that allows a custom answer, you
  couldn't submit once you'd typed one — Enter re-opened the text editor
  instead of confirming, and Space did nothing on the `(custom)` row. Enter now
  confirms once custom text exists; Space (re)opens the editor to edit the
  answer in place; the footer shows `Space edit` on the custom row (dirge-72h2).

### Changed
- `review` and `review-security` prompts no longer deny the whole `bash` tool.
  A whole-tool `bash` deny is terminal and pre-empted the default rules that
  already allow read-only git, so the reviewer couldn't even run `git diff` /
  `git log` to inspect the change under review. `bash` now flows through the
  normal permission engine: read-only git and other pre-trusted commands are
  allowed, while effectful/destructive commands (`git push`, `rm`, `curl`, …)
  still prompt. `plan` mode keeps its full read-only lock (dirge-k265).

## [0.18.5] - 2026-07-06

### Added
- External editor for composing messages: `Ctrl+G` or `/edit` opens `$EDITOR`
  with the current input buffer and replaces it on save. Thanks @blob42 (#592).
- Opt-in editor follow-along: set `editor_open_command` (a `{path}`/`{line}`
  template, e.g. `zed {path}:{line}` or `code --goto {path}:{line}`) and dirge
  opens the file it's reading/editing in your GUI editor — detached and
  non-blocking, so the editor follows along like Zed's AI panel. Off by
  default; deduped so repeat reads don't spawn a burst (#572).

## [0.18.4] - 2026-07-05

### Fixed
- The agent no longer silently halts after a failed tool call. When the model's
  last action was a rejected/errored tool call and it then stopped with a
  text-only reply, a deterministic nudge now re-prompts it to complete the step
  (or explain why it can't) instead of waiting for the user to say "continue".
  Bounded and self-limiting — it only fires when a tool-error immediately
  precedes the stopped turn, so it can't loop, and runs before the LLM critic
  so obvious failures are handled without a judge call. Ambiguous "is it
  actually done?" cases remain the critic's job (dirge-rwru).

## [0.18.3] - 2026-07-04

### Changed
- Default working-context budget (`context_target`) raised from 100k to **250k**
  tokens. Context quality degrades gradually past ~100k rather than falling off
  a cliff, and capable newer models (DeepSeek V4, Claude Opus 4.x, Gemini) hold
  up well into the mid-hundreds of thousands — so a flat 100k cap left usable
  range on the table. The effective window is still `min(model_window,
  context_target)`, so smaller models are unaffected; set `context_target`
  lower (e.g. `100000`) for small local models or cost-sensitive routes. Thanks
  @gretel (#585).

## [0.18.2] - 2026-07-04

### Added
- `DIRGE_DUMP_REQUESTS` request-dump lines now carry the `provider` and `model`
  each request went to, plus `messages_bytes` for the conversation history —
  so when debugging multi-provider or mid-turn side-completion behavior you can
  see which backend served each call, not just the byte sizes. `=1` logs the
  summary line, `=full` also logs the prompt body; off by default, emitted at
  INFO on `dirge::wire` (dirge-iis0).

### Fixed
- Tool chambers now re-box to the terminal width on resize, like the rest of
  the scrollback. Previously a chamber laid out at a wide width stayed that
  width (and clipped) after narrowing. The whole chamber — top border, body,
  colorized diffs, LSP-tail rows, and bottom border — reflows together, so the
  box never ends up with a mismatched header (dirge-ghpf).
- Permission prompt no longer hides part of a long or multi-line command. The
  alert box now scrolls: the command detail can be paged with ↑/↓, PgUp/PgDn,
  or Ctrl+O while the `[y]/[a]/[n]/[ESC]` action row stays pinned, and a status
  line shows how much is off-screen. Previously a tall command was cut with a
  bare `…` and the modal swallowed scroll keys, so you had to approve a `bash`
  command you couldn't fully read (#587).

## [0.18.1] - 2026-07-04

### Fixed
- Mouse/selection breaking after switching away from and back to the terminal
  window now recovers automatically. Focus reporting is enabled, and on
  focus-in dirge re-asserts the terminal modes (re-enter the alternate screen,
  mouse capture, bracketed paste) wrapped in a synchronized update, so the alt
  screen is restored the instant the window regains focus — no keypress. The
  wheel-scrolls-native-scrollback / dead-selection state that used to require a
  manual Ctrl+L now heals itself. Ctrl+L remains as a manual backstop.

## [0.18.0] - 2026-07-04

Three focused changes on top of 0.17.0: the diff-aware code reviewer is now
configurable and non-blocking by default, a terminal-recovery escape hatch,
and a new provider.

### Added
- `code_review` config (`off` | `advisory` | `blocking`, default `advisory`)
  plus a per-prompt `code_review` front-matter override, controlling how the
  diff-aware reviewer engages. Advisory runs the review in the background after
  a turn and surfaces all findings (high/critical included) as one non-blocking
  `<system>` notice; blocking is the previous synchronous behavior (re-enters
  the loop on high/critical); off disarms it entirely — no diff capture, no
  judge call. Still requires `critic_provider`.
- OpenCode provider — `--provider opencode` (the opencode.ai zen endpoint,
  default model `deepseek-v4-flash`). Thanks @gretel.
- Ctrl+L forces a full terminal redraw: re-enter the alternate screen,
  re-enable mouse capture and bracketed paste, and repaint. Recovers a session
  that was dropped to the main screen — mouse wheel scrolling the native
  scrollback (the whole TUI scrolls off) and selection no longer being
  captured, while the keyboard still works.

### Changed
- The diff-aware code reviewer no longer blocks the turn by default. It now
  runs in the background (advisory mode) and reports findings as a non-blocking
  notice instead of re-entering the loop and spending a react budget, so a
  tight back-and-forth debug loop isn't held up waiting on a review. Set
  `code_review = blocking` to restore the previous synchronous, re-entering
  behavior.

## [0.17.0] - 2026-07-03

A large security- and correctness-hardening release: the findings from a
multi-round internal audit (rounds R1–R8) plus follow-up remediation. Almost
all of it is bug fixes, robustness, and behavior-preserving refactors; a few
user-visible behaviors changed for the better (below). No breaking config or
CLI changes.

### Added
- `timeouts.request_establish_secs` (default 300) bounds the request-establish
  phase — the connection/handshake and wait for the first response event — so a
  stalled connection can't hang a run indefinitely. It sits alongside the other
  named `timeouts.*` keys and retries as a transient network error.

### Changed
- An explicit model choice under a ChatGPT/Codex login is now honored. Previously
  `--model gpt-4o` (or `/model gpt-4o`) was silently rewritten to the Codex
  subscription default; the explicit-vs-default distinction is now resolved once,
  up front, and stored on the session — so it survives resume too.
- `--no-color` now collapses the *entire* TUI (chat, side panels, frames, tool
  output), not just themed text. Color is stripped at the paint chokepoints, so
  the ~100 raw color literals that previously leaked are covered.
- `edit_lines`' staleness hash is now coupled to each line's predecessor. A
  single line drifting perturbs two adjacent hashes, so slipping a stale edit
  past the guard needs two independent collisions (~1/16.7M instead of ~1/4096)
  — with no extra tokens. The first line, and hashes for unchanged regions, are
  unaffected.
- Typing while scrolled up in the chat log no longer jumps you to the bottom —
  you can read back through history while composing. Press Down (when scrolled
  up) to return to the newest content.
- Restored `harness/log` as a working plugin debug sink (drains to the host log
  under `--verbose`/`RUST_LOG`); it had regressed to a no-op.

### Fixed
- **Auth & providers.** OAuth transport, PKCE state, and refresh-race hardening;
  a ChatGPT/Codex token-rotation TOCTOU that could freeze an expired bearer for a
  whole session; the escalation route now runs under the retry policy; retry
  backoff observes cancellation promptly instead of waiting out the full delay.
- **Windows.** Path resolution no longer leaks `\\?\` verbatim prefixes, and a
  brand-new deep file path stays verbatim past MAX_PATH so writes don't fail on
  machines without the long-path opt-in.
- **Compaction & context.** Keep-recent is clamped to half the window on
  small-context models; token totals are recomputed after a fold instead of
  applying a cross-accounting delta; discovery-anchor, overview-eviction, and
  duplicate-reasoning fixes; a fold→persist→resume round-trip is now covered by
  tests.
- **Debugger & language servers.** DAP pause-vs-continue locking, attach status,
  and stale stop events; `continue_` is guarded against session replacement while
  parked. The LSP and DAP clients now share one JSON-RPC correlation core, so the
  drain-on-close fix lives in one place.
- **File mutation.** BOM handling, mixed line endings, non-UTF-8 refusal, and
  correct error propagation from `apply_patch` / result relay.
- **Plugins.** Multi-line steering/followup messages are escaped (no longer
  shredded); a per-process error sentinel prevents a plugin result that looks
  like an internal error marker from being dropped; the renderer registry uses
  the shared escaped format; deny-tools names are escaped before Janet
  interpolation.
- **Skills.** Create/register are transactional; archive failures are logged, not
  swallowed; recovered file skills reactivate on re-register; a force-deleted
  pinned skill no longer leaves a ghost active row.
- **MCP.** The process-group guard is disarmed on the success path (pid-reuse
  race); a delegated child is killed on cancellation instead of orphaned.
- **Sessions, CLI & slash.** Warn on a stale resume for `-c`/`-r`; refuse to start
  on an invalid `permission` config instead of silently dropping all rules;
  `/issues search` treats `%`/`_` as literals; `/loop start` no longer leaks the
  `start` verb into the prompt; `/graph` search stops leaking flags into the FTS
  query; `/wt-merge` / `/wt-exit` handle `:` in paths; `/model` no longer
  mislabels the provider on a same-provider swap; a forbidden `!` command hidden
  behind a `VAR=value` prefix is caught.
- **TUI.** Panic hook that restores the terminal; width-aware (double-width-glyph)
  wrapping; a non-blocking plugin shortcut; the side panels no longer reserve a
  blank right gutter at the show threshold.

### Internal
- Behavior-preserving refactors that removed duplication behind several of the
  above fixes: one shared content-guard module, one judge-gate truncation +
  fail-open wrapper, one agent-rebuild helper, one user-boundary cut-snapping
  helper, a typed `SlashOutcome` replacing the `DEFER_*` error-string protocol,
  and the shared JSON-RPC correlation layer.

## [0.16.0] - 2026-07-01

### Added
- **Diff-aware code reviewer.** A new finalization gate reviews the actual diff a
  run produced (not just the transcript) and surfaces severity-ranked findings.
  It runs two passes — review, then a verify/dedupe pass that drops false
  positives — and splits the results: high/critical findings re-enter the loop
  (fix or justify), medium/low surface as non-blocking advisories. Also runnable
  on demand with `/code-review`. It reuses the `critic_provider` judge, so it's
  the same opt-in as the critic with no cost when off. Prompt craft and the
  finding/verdict model are ported from [roborev](https://go.kenn.io/roborev).

## [0.15.0] - 2026-07-01

### Added
- **Reusable skill learning with salience.** Skills now carry the same salience
  machinery as memory. `/learn` distills a reusable skill from any source — a
  directory, a URL, pasted notes, or the current conversation (bare `/learn`) —
  by running one standards-guided turn with the agent's own tools (`read`,
  `grep`, `find_files`, `webfetch`) and saving through the `skill` tool; there's
  no separate distillation engine. Skill telemetry (use/view/patch counts,
  provenance, success/failure record) moves from the `.usage.json` sidecar into
  the project's SQLite database, and the system-prompt skill list is ordered by
  effective salience so the most useful skills surface first. (#557)
- **Verification gate.** Creating a skill now requires a `## Verification`
  section — a single command that proves the skill works — and a freshly learned
  skill is seeded one grounding success, since it was validated in the session
  that produced it. (#557)

### Changed
- **Skill curation is salience-driven, not age-based.** The old
  active/stale/archived state machine is replaced: the curator decays
  unconsulted agent-created skills and archives those whose effective salience
  (decay folded with a proven success/failure record and confidence) falls to
  the archival threshold. A skill that keeps working survives on its track
  record even when untouched; pinned skills and skills you didn't author are
  never auto-archived. The `.usage.json` sidecar is retired. (#557)

## [0.14.2] - 2026-06-30

### Added
- **[AGENTS] left-panel box.** A fourth box below [GIT] lists the running
  subagents by profile name. When two subagents share a profile the name alone
  is ambiguous, so each row appends its `id_short` (e.g. `architect aaa111`);
  the unnamed-agent fallback shows just the `id_short`. Real subagent data is
  threaded through `LeftPanelInfo`, and the old separate subagent region
  collapses into the vitals card. (#553, #554)

### Changed
- **Slash commands have a single source of truth.** The parallel name-only and
  (name, description) lists are merged into one `slash_commands()`; tab
  completion / `is_known_slash_command` and the `/help` render both derive from
  it, so adding a command is one list entry plus one match arm instead of three
  synchronized edits. name↔description drift is now structurally impossible
  (a no-duplicate-names guard replaces the old drift test); `/help` renders
  "name  description". (#553)
- **Critic scoped or disabled for read-only and design prompts.** The built-in
  critic is tuned for coding mode, where it checks that code, tests, and
  verification all landed. In the read-only/design prompts every mutating tool
  is denied, so it systematically false-positived — blocking turns to demand
  builds or runs the denylist makes impossible. `ask`, `plan`, and
  `write-prompt` keep the critic with a scoped preamble that judges only the
  actual deliverable (answer completeness, plan actionability, prompt-contract
  conformance); `brainstorm`, `review`, and `review-security` set
  `critic: false`. Default coding mode is unchanged. (#549)

### Fixed
- **No duplicate output after a mid-response provider stall.** 0.14.0 (#547)
  made stream-chunk timeouts retryable even after text had committed, but the
  retry reset only clears the message history — not text already rendered — so
  the second attempt re-streamed the whole response and appended a duplicate
  copy. Retry is gated on `!committed` again; the pre-commit mid-assembly-stall
  case (#545) still retries. Also narrows the "stuck in a long reasoning loop"
  nudge so it no longer mis-fires on plain transport stream-chunk timeouts, and
  corrects the now-inaccurate "retried automatically" wording in the timeout
  error message and docs/config.md. (#552)

### Documentation
- **Building with newer libclang.** README notes the Janet build failure with
  newer libclang and a documented workaround for pointing at a compatible
  libclang. (#548)

## [0.14.1] - 2026-06-29

### Fixed
- **Fix the `x86_64-unknown-linux-musl` build** (broken in 0.14.0). The new PTY
  bang-command code cast `TIOCSCTTY` to `c_ulong` for the `ioctl` request arg,
  which only matches glibc/macOS — musl declares that arg as `c_int`, so the
  musl release binary (and `cargo install` on musl) failed to compile. Use
  `as _` so each target infers the right width. (No behavior change on other
  platforms.)

## [0.14.0] - 2026-06-29

### Added
- **`!`/`!!` bang commands run on a real PTY.** They reuse the `/sandbox` attach
  path — suspend the TUI, attach the command to `/dev/tty`, relay I/O, resume —
  so commands that need terminal input (`gh auth login`, editors, interactive
  prompts) work instead of hanging to the 120s timeout. Interactive output
  renders through a vt100 screen parser, so cursor-moving TUIs (arrow-key menus)
  update in place rather than stacking redraws; the avatar stays idle while you
  drive the shell. (#544, #546)
- **`/prompt <name> <text>` switches *and* runs the text.** Previously everything
  after the prompt name was silently dropped; it now switches to the named
  prompt and launches a streamed turn on the trailing text (via a
  `DEFER_PROMPT_RUN` sentinel, gated on the name resolving). (#538)
- **Clipboard "Copied!" tooltip** in the chat area, bridging the lack of native
  terminal copy feedback. (#542)
- **CI clippy gate** — clippy runs with `-D warnings` and the lint backlog was
  cleared across all feature builds. (#540)

### Changed
- **`write_todo_list` is backed by the issue board.** The TODOS panel and the
  end-of-turn nudge were driven by a separate in-memory checklist; they now share
  the durable issue tracker — a todo *is* an issue. Bulk planning gains the full
  lifecycle (open/in_progress/blocked/done/cancelled), items match by title
  across calls (scoped to the session), and omitted items are no longer silently
  dropped (close one by restating it completed/cancelled). (#539)

### Fixed
- **Failed MCP servers are visible in the info panel.** A server that failed its
  initial connect was invisible (rendered as `(none)`); it now shows as broken
  (`○`), matching how LSP surfaces broken servers. The live connection is still
  dropped, so `/mcp reconnect` keeps working. (#541)
- **Stream retries on a mid-tool-call chunk timeout.** A chunk timeout that fired
  while a tool call was mid-assembly halted the run — prior thinking/text had
  marked the attempt "committed" and the retry layer refused to replay it. Timeout
  errors now retry even after content commits, so a single provider stall no
  longer kills a run on reasoning models that emit thinking before a tool call.
  (#547)
- **Agent construction can't wedge on a stuck DB.** The memory, global-memory,
  and spec-store loads in `build_agent_inner` are now bounded by a 5s timeout
  (graceful degradation on timeout), so a locked/slow SQLite no longer hangs
  agent builds — a contributor to the `/prompt`-rebuild hang.

## [0.13.9] - 2026-06-28

### Changed
- **The `issue` tool is auto-allowed by default.** It's now classified as a
  builtin-allowed meta operation (like `write_todo_list`), so the agent records
  its own work (create/start/block/close/update) without a permission prompt
  each time — the tracker is a local, user-viewable (`/issues`), reversible DB,
  so the prompts were friction without security value. Still overridable via an
  explicit deny rule. (#537)

## [0.13.8] - 2026-06-28

### Added
- **Steerable critic preamble.** The in-loop critic's system preamble was
  hardcoded; it's now exposable at two tiers — a global `critic_preamble` in
  config (`resolve_critic_preamble()`) and a per-prompt frontmatter
  `critic_preamble:` (inline or a YAML block scalar; empty inherits). A
  `critic: false` frontmatter suppresses the critic for that prompt only. See
  [docs/prompts.md](docs/prompts.md). (e0f684a)

### Changed
- **Goal gate decoupled from the critic.** They previously shared one judge
  closure (and preamble), so a critic override or `critic: false` leaked into
  goal judgements. `build_critic_fn` is generalized to a `build_judge_fn` that
  bakes an independent preamble; the critic and the `--goal` gate now share a
  client but judge under separate preambles, and the gate always uses its own
  fixed `GOAL_PREAMBLE`. `CRITIC_FORMAT` and `GOAL_PREAMBLE` remain
  non-overridable by design. (e0f684a)

## [0.13.7] - 2026-06-27

### Fixed
- **`/model <id>` switches providers, not just the model name.** The `/model`
  list shows one row per configured provider's pinned model; selecting a model
  that belongs to a *different* provider now rebuilds the live client for that
  provider instead of sending its model name to the active provider's endpoint
  (which 400'd — e.g. an ollama model sent to GLM). Free-form ids and
  same-provider models keep the current client. (#535)
- **`--provider` now selects that provider's model.** With no `--model`,
  `dirge --provider <p>` took the config default provider's model rather than
  `<p>`'s, so `--provider ollama` switched the endpoint but still loaded the
  default model. It now reads the overridden provider's pinned model. (#535)
- **`XDG_CONFIG_HOME` is honored for the config directory.** Resolution is now
  `DIRGE_CONFIG_DIR` → `$XDG_CONFIG_HOME/dirge` → `~/.config/dirge` (relative
  `XDG_CONFIG_HOME` ignored per the XDG spec). (#535)

### Changed
- **Clearer "no API key" error for keyless OpenAI-compatible endpoints.** When
  a `provider_type: "openai"` provider has no key, the error now points local
  ollama/vLLM users at `provider_type: "custom"` (or `"ollama"`), which are
  keyless. (#535)

## [0.13.6] - 2026-06-27

### Added
- **Native issue tracker** — a persistent, agent-facing kanban in the existing
  per-project session DB (`.dirge/sessions/state.db`, `issues` table), framed as
  a stateful extension of the memory model. The new `issue` tool creates /
  starts / blocks / closes / updates / lists / searches durable tasks (status
  `open`/`in_progress`/`blocked`/`done`, priority `high`/`normal`/`low`); unlike
  the ephemeral `write_todo_list`, issues persist across sessions. The harness
  injects the top open issues as a board at the **start of each turn** (bounded,
  with a "+N more" hint) so the model works its backlog without polling — gated
  so forked review/curator runners don't receive it. View it from the TUI with
  `/issues` (`/issues list`, `/issues <id>`, `/issues search <q>`). The store
  owns its schema and opens lock-free, so it adds no migration contention to the
  shared session DB. See [docs/issues.md](docs/issues.md). (#534)

## [0.13.5] - 2026-06-27

### Fixed
- **`auth: "chatgpt"` falls back to legacy `codex login` storage when a Dirge
  OpenAI OAuth credential is present but unusable** (expired with a failed or
  absent refresh, or an unreadable store). It previously hard-errored the
  session in that case even when a valid `~/.codex/auth.json` could have served
  it; now it warns and uses the legacy file. (#533)

## [0.13.4] - 2026-06-26

### Changed
- **`dirge auth openai` now uses a browser-based OAuth (PKCE) flow** instead of a
  device password, matching standard Codex authentication — so you no longer have
  to enable device passwords in the OpenAI console. The browser flow opens a
  loopback redirect, validates a CSRF `state`, and exchanges the code with PKCE
  S256. The previous device-code flow is retained behind
  `dirge auth openai --device-code`. (#532)

### Fixed
- **`auth: "chatgpt"` now honors fresh Dirge OpenAI OAuth credentials before
  falling back to legacy Codex storage.** This prevents a stale
  `~/.codex/auth.json` access token from causing repeated `token_expired` 401s
  after `dirge auth openai` has already produced a fresh, refreshable Dirge
  credential. Stored credentials are refreshed on expiry (and re-persisted)
  rather than served stale. (#532)

## [0.13.3] - 2026-06-26

### Fixed
- **Compaction no longer makes side-LLM summary calls with Anthropic OAuth
  credentials.** A bare summarization request doesn't look like a normal Claude
  Code turn and could trip Anthropic's third-party-use detection, so explicit
  `/compress`, preemptive compaction, reactive overflow recovery, and in-loop
  folding now route their summary call through `summarization_provider` and
  refuse to fall back to Anthropic OAuth. Configure a non-Anthropic-OAuth
  `summarization_provider` to keep high-fidelity LLM summaries. (#529)
- **OAuth sessions without a summarizer degrade gracefully instead of breaking.**
  When no safe summarizer is configured, reactive overflow and explicit
  `/compress` now fall back to a local prune-only emergency compaction (drop the
  oldest turns with a deterministic note) so the session keeps going, rather than
  hard-erroring. The disabled-compaction error is also matched through its full
  source chain so that fallback can't be silently skipped by a wrapped error.
  (#530)

### Added
- **Startup notice when compaction can only prune.** On an Anthropic OAuth
  session with no non-OAuth `summarization_provider`, the interactive UI now warns
  once at launch that context folds will be lossy prune-only, so it isn't a
  surprise at the first fold. The notice self-clears once a summarizer is
  configured. (#531)

## [0.13.2] - 2026-06-26

### Added
- **Cross-session command history.** Up/Down and Ctrl+F recall now seed from the
  most-recent prior sessions in the same project, not just the current one — so a
  fresh session in a project starts with your earlier prompts already in history
  instead of an empty pool. The new top-level `max_sessions` config (default `3`)
  caps how many prior sessions are mined; set `0` to keep recall scoped to the
  current session. Prior prompts are seeded oldest-first ahead of the current
  session's own, so Up still starts from your newest command and walks back.
  Compaction-fold rotations are collapsed so one conversation counts once, and
  synthetic turns (system-reminder wrappers, mid-turn steers, auto-continues)
  never enter history. (#527, #528; thanks @nikolap)

## [0.13.1] - 2026-06-26

### Fixed
- **Headless `--print`/`--loop` no longer hangs forever on a tool that needs
  permission confirmation.** These modes have no UI loop to service the
  permission channel, so a tool routing to a confirmation prompt (e.g. a `bash`
  command `--accept-all` doesn't auto-allow) sent an `AskRequest` and blocked on
  the reply forever — suspending the agent loop with no output and no `result`.
  Headless runs now drain that channel with a deny-all responder (mirroring the
  ACP path), so a not-auto-allowed tool call fails fast instead of hanging; the
  model sees the denial and re-plans. For fully unattended runs that must never
  block, use `--yolo` (which allows every tool and never prompts) or configure
  explicit allow rules. This is the real fix for the deadlock first reported in
  #523 (the earlier 0.12.5 plugin-worker fix addressed a different path).
  (#523, dirge-3oy0)

## [0.13.0] - 2026-06-26

### Added
- **Project-local config overlay.** A project can ship a partial
  `<project>/.dirge/config.json` that deep-merges on top of the global
  `~/.config/dirge/config.json` instead of duplicating the whole file. Scalar
  fields override; map-valued fields (`providers`, `mcp_servers`, `agents`,
  `slash_aliases`, `keybindings`) union key-by-key, so a project can add or
  override a single entry without redeclaring the map. An empty object is a
  no-op, never a wipe. CLI flags and env vars still take precedence over both
  files. (#526; thanks @nikolap)
- **Project-local prompts tier with `/prompt` provenance.** Custom prompts can
  live in `<project>/.dirge/prompts/` and override global or built-in prompts of
  the same name. `/prompt` now shows a `[global]`/`[project]` badge next to
  overridden built-ins, mirroring `/agents`. (#526)

### Changed
- **BREAKING: project-local prompts moved from `./prompts/` to
  `.dirge/prompts/`.** The old cwd-relative `prompts/` directory is no longer
  read. If you relied on a project-level `prompts/` directory, move it to
  `.dirge/prompts/` (matching `.dirge/agents`, `.dirge/plugins`, `.dirge/skills`).
  Global (`~/.config/dirge/prompts/`) and built-in prompts are unaffected.

## [0.12.6] - 2026-06-26

### Fixed
- **Tool chambers and chat now span the full chat band.** On wide terminals
  with the side panels hidden, the chat band reclaims the panel gutters but the
  chamber boxes (and chat text) were still pinned to the gutter-blind, 120-capped
  `content_width`, leaving a dead strip on the right where stale border glyphs
  rendered. Chamber width and chat wrapping now derive from the actual painted
  band (`Layout::chat.width`), so the right border lands on the last painted cell
  and the box matches the left edge. When panels are visible the band is capped
  as before, so that mode is unchanged.
- **`--no-default-features --features mcp` builds again.** The MCP setup
  referenced `cli.loop_mode` and an `ansi` import that only exist with the `loop`
  feature, so that combination failed under `-D warnings`. Both are now gated on
  `loop`. (dirge-oae9)

### Changed
- Bumped `crypto-bigint` 0.7.3 → 0.7.5 (0.7.3 was yanked upstream).

## [0.12.5] - 2026-06-26

### Fixed
- **Headless runs no longer deadlock mid-task on multi-turn agentic work.** In
  `-p` mode a run could freeze permanently after several tool-call turns (alive,
  0% CPU, no `result`). Three compounding issues in the plugin worker path are
  fixed: (1) the before/after tool hooks awaited their `spawn_blocking` dispatch
  via a `tokio::time::timeout` that, on expiry, *detached* a still-running,
  uncancellable blocking task — which kept holding the global plugin-manager
  mutex and the single Janet worker, stalling the next hook; the dispatch is now
  awaited to completion (it's already bounded internally). (2) `harness/confirm`
  / `harness/select` (`send_dialog`) was the one host call with no wall-clock
  bound, so a dialog whose responder never answered pinned the worker forever;
  it now gives up after a generous timeout. (3) the tool hooks round-tripped to
  the worker on *every* tool call even with zero plugins registered;
  `dispatch_tool_hook` now skips the worker entirely when no plugin subscribes.
  (#523, dirge-u5ig)

## [0.12.4] - 2026-06-25

### Added
- **Config-driven aliases for built-in slash commands.** A top-level
  `slash_aliases` map renames a built-in or adds a short alias — `{"exit":
  "quit", "q": "/quit"}` makes `/exit` and `/q` both run `/quit`. A leading `/`
  on either side is optional, arguments after the alias pass through verbatim,
  and an alias inherits its target's safety class (so an alias for `/quit` works
  mid-run). Resolved once at startup; bad targets and keys that shadow a built-in
  warn on the same path as keymap warnings. Aliases register in tab-completion,
  the ghost suffix, and `/help`. (#522; thanks @nikolap)

### Fixed
- **`--print --output-format stream-json` now streams incrementally.** The
  headless run loop received the agent's per-turn and per-tool events but, in
  `stream-json` mode, emitted nothing for them — only a `system` init line, then
  silence for the whole run, then one final `assistant` + `result` at the end.
  Multi-turn agentic runs looked frozen to any consumer parsing the stream. The
  loop now emits a Claude-compatible `assistant` event at each turn boundary
  (text + `tool_use` blocks) and a `user` event carrying that turn's
  `tool_result` blocks, so live-progress UIs get real incremental output. The
  single-turn shape (`system` → `assistant` → `result`) is unchanged. (#520,
  dirge-kuqp)

## [0.12.3] - 2026-06-25

### Added
- **SQL semantic support** behind the `semantic-sql` feature (in `default` /
  `windows-default`), backed by the `tree-sitter-sequel` grammar: `list_symbols`
  / `get_symbol_body` / `find_definition` over DDL objects (tables, views,
  materialized views, functions, indexes, types). (#516; thanks @nikolap)

### Fixed
- **SQL writes are no longer blocked by the generic SQL grammar.** `.sql` was
  wired into the pre-write syntax gate, which hard-rejects parse errors — but
  `tree-sitter-sequel` flags valid mainstream SQL (`CREATE PROCEDURE`, the whole
  T-SQL dialect) as errors, so the agent couldn't save it. `.sql` is dropped
  from the write gate; semantic indexing (which tolerates parse errors) is
  unaffected.
- **SQL adapter: anonymous `CREATE INDEX ON t(col)`** no longer emits the table
  as a bogus index symbol — an unnamed index has no symbol to record.
- **SQL adapter: DDL nested in a function body** (e.g. a `CREATE TABLE` inside a
  PL/pgSQL `$$ … $$` body) no longer leaks in as a spurious top-level symbol.

## [0.12.2] - 2026-06-24

### Fixed
- **Proactive compaction now actually runs near the limit.** The pre-send
  trigger fires at 85% of the usable budget, but `prepare_compaction` re-gated
  every non-forced call behind a stricter 100% within-limits check — so in the
  85–100% band the UI announced "preemptive compaction… compressing…" and then
  printed "context within limits, no compression needed" without compacting,
  leaving proactive compaction effectively dead until a hard overflow. The
  trigger now owns the decision (it alone accounts for the incoming prompt) and
  compacts without being re-gated. (dirge-rz4i)

## [0.12.1] - 2026-06-24

### Fixed
- **Mid-task context overflow now resumes the task after compaction instead of
  stranding it.** When a turn overflowed the context mid-work, reactive
  compaction ran but then sat idle because recovery refused to retry once any
  tool had run. With eager post-turn compaction gone (0.12.0), that mid-turn
  path became common, so the agent routinely stopped after compacting. The
  partial assistant turn (streamed text + completed tool calls) is now recorded
  into history before compacting, and recovery resumes as a continuation — the
  already-run tools are not re-executed. (dirge-b899)
- **Queuing a steering message mid-stream no longer duplicates the response.**
  Echoing the queued message sealed the open stream block, and the next token
  re-opened a new block with the whole accumulated response, painting the
  partial `<dirge>` reply twice. The partial is now sealed and the render buffer
  reset so post-queue tokens render as a clean continuation.

## [0.12.0] - 2026-06-24

### Added
- **Experimental entity/relation graph storage** behind the
  `experimental-graph-search` feature gate: entity/relation recording, FTS5 +
  recursive-CTE graph search, a `/graph` command, and Janet harness hooks
  (schema v14). Opt-in; no effect on the default build. (#513, closes #393;
  thanks @allen-munsch)

### Fixed
- **Computer-use hardening** (experimental `experimental-ui-computer-use`):
  closed a single-character shell-injection path in key validation, replaced
  guessable `os/time` temp files with `mktemp`, deduplicated the bash CONTRACT
  hint, and routed desktop actions through the permission PDP (`deny_tools`).
  (#514, follow-up to #415; thanks @allen-munsch)

### Changed
- **The UI no longer freezes during background work.** Long operations used to
  be `.await`ed inline in the single-threaded event loop, freezing rendering,
  input, and Ctrl+C for their whole duration. They now run on spawned tasks
  drained by dedicated `select!` arms, so the loop stays responsive and
  Ctrl+C/Esc abort the work. Converted: compaction (the summarizer — explicit
  `/compress`, preemptive pre-prompt, and reactive overflow recovery), the
  `/plan` reviewer loop (a write-disabled reviewer that runs the code), `/btw`
  side queries, `!cmd` shell commands (up to the 120s cap), and `/wt-merge`
  (the git merge). The post-turn auto-compaction that used to block at every
  turn end was dropped — the now-async preemptive pass covers the next user
  prompt and reactive recovery covers automated follow-ups.
- **`build_agent` no longer re-handshakes MCP servers on every rebuild.** It
  ran a `tools/list` round-trip per connected server, uncached and with no
  timeout, on each of ~9 inline sites (down to a prompt-cycle keystroke) — so a
  rebuild froze the UI for the round-trip, unbounded if a server was wedged.
  Tool definitions are now cached per server (invalidated on `/mcp reconnect`)
  and each fetch is bounded by a 5s timeout.
- **The post-session `git diff --stat` runs off the event loop.** The turn-end
  review digest shelled out on the loop at every turn; the subprocess now runs
  inside the already-spawned post-session task.

## [0.11.3] - 2026-06-22

### Added
- **Read the current session id.** `/sessions current` prints the full session
  id with a `dirge --session <id>` resume hint, and `/sessions list` marks the
  live session. The status footer's session badge now shows a *distinct*
  compact id (keeps a `compacted-`/`forked-` prefix plus the unique uuid head)
  instead of collapsing every compacted session to "compacte".

### Changed
- **One-shot side-LLM calls no longer burn reasoning tokens.** The summarizer,
  critic, and approval-evaluator one-shots now request the model's
  extended-reasoning trace OFF (provider-appropriate param: chat_template_kwargs
  for the openai-compat/DeepSeek family, `think:false` for Ollama, zero thinking
  budget for Gemini). On reasoning-by-default models this roughly halves a
  context-checkpoint summary's latency. Anthropic (off by default) and OpenAI
  (no safe "off") are untouched.

## [0.11.2] - 2026-06-22

### Fixed
- **OpenAI Responses streaming.** Historical reasoning blocks (which the
  Responses API rejects without provider-generated IDs) are dropped and tool
  `call_id`s are synthesized for OpenAI requests, fixing broken streaming /
  tool-use against OpenAI. Applies to all three OpenAI client variants — API
  key, ChatGPT-OAuth, and Codex — regardless of the configured provider alias.
  (#510 + follow-up, issue #480; thanks @ericschmar)
- **Preserve partial answers when escaping a multi-question prompt.** Pressing
  Esc after answering at least one question in a batch now returns the answers
  given (remaining questions marked `[no answer]`) instead of discarding
  everything. Esc on the first question still cancels. (#508; thanks @rgkirch)

## [0.11.1] - 2026-06-22

### Added
- **Hot-load plugins mid-session with `/plugins load`.** `/plugins load <path>`
  loads a single `.janet` file or plugin directory; `/plugins load all` loads
  every `.janet` from `.dirge/plugins/` and `~/.config/dirge/plugins/` (cwd wins
  on same-name). Previously `/plugins` only listed already-loaded plugins. Works
  in release builds. (#505, thanks @allen-munsch)
- **See which prompt goes to which provider, and why.** New opt-in request dump
  behind `DIRGE_DUMP_REQUESTS`: `=1` logs one summary line per outgoing provider
  request (purpose label, provider, tool count/names, reasoning flag, byte
  sizes); `=full` also dumps the system / one-shot prompt body. Emitted at INFO
  on the `dirge::wire` target, so it lands in `dirge.log` (`-v` / `RUST_LOG` /
  `DIRGE_LOG`) or via `RUST_LOG=dirge::wire=info`. Instrumented at both
  request-build choke points — the side-LLM one-shot path (summarizer / critic /
  approval evaluator) and the agent stream factory (turns / escalation /
  subagents / forked review) — so a mystery secondary completion is now
  attributable. Off by default. (#507, dirge-iis0)

## [0.11.0] - 2026-06-22

### Added
- **Scrollback reflows on terminal resize.** The chat buffer became a derived
  cache over a width-independent source log, so prose and markdown — tables
  especially — re-wrap to the new width on resize instead of keeping the
  column widths they were first rendered at. Streamed reasoning/response is
  source-tracked too; tool-chamber borders are preserved verbatim (re-boxing is
  a follow-up). (#504, dirge-qy3y)
- **Memory: a pinned project overview.** A new `overview` memory kind holds a
  single high-level orientation (stack, layout, how to build/test) that is
  exempt from eviction and rendered first in the system prompt, refreshed by the
  background review. (#504, dirge-pkqi)
- **Memory: deterministic session ground-truth.** A model-free digest (goal,
  files touched, commands run, todos, `git diff --stat`) is prepended to the
  background-review transcript so it ranks known facts instead of rediscovering
  them. (#504, dirge-a62g)
- **Memory: open-thread carry-over.** The background review records genuinely
  unfinished work as short `working` entries so a fresh session resumes where
  the last one stopped, and clears them once the work lands. (#504, dirge-hcv8)

### Changed
- **Rebind a key across contexts without unbinding it first.** A user (or
  plugin) keybinding now clears the chord from both the global and input keymaps
  before inserting the resolved command, so e.g. `ctrl-r → reverse_search` takes
  effect without a preceding `unbind`. (#504, dirge-z2p6)

### Fixed
- **Secrets in memory entries are redacted before storage**, not just in the
  full-text index — closing a leak path into the system prompt and global-scope
  memory. (#504, dirge-n3qf)

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

[Unreleased]: https://github.com/dirge-code/dirge/compare/v0.21.3...HEAD
[0.21.3]: https://github.com/dirge-code/dirge/compare/v0.21.2...v0.21.3
[0.16.0]: https://github.com/dirge-code/dirge/compare/v0.15.0...v0.16.0
[0.4.1]: https://github.com/dirge-code/dirge/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dirge-code/dirge/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/dirge-code/dirge/compare/v0.3.0...v0.3.1
[1.0.0]: https://github.com/dirge-code/dirge/releases/tag/v1.0.0

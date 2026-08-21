# Harness review, August 2026

What running dirge end-to-end against a small local model showed, what was
fixed, and what is left. Evidence is JSONL traces (`--trace`, see
`docs/loop-trace.md`); every fix has a test that fails without it.

The model is a 27B Qwen served by llama.cpp at ~9 tok/s. That matters twice
over: a small model exercises the steering features the way they were designed
for (`capability.rs` — "help the failing run, leave the coping run alone"), and
a slow one makes a wasted turn expensive enough to notice.

## The finding

**Every guard's reasoning was right. Three of them described themselves
wrongly, and in each measured case the model did not comply — it worked around
the guard.** That is the same shape as `docs/verification-discipline.md`'s
"a guard that cannot explain its scope gets routed around, not satisfied", and
it is now the third time it has come up.

The corollary is uncomfortable and worth stating plainly: the steering features
are not what failed. What failed was the *sentence*, and a wrong sentence to a
model is a wrong behaviour, exactly as a wrong condition would be.

## Phase 1 — make the loop legible (done)

Nothing here was diagnosable before. `RUST_LOG=debug` produced 384 lines for
one small task, six of them from the loop, and the ones it did carry omitted
the numbers that mattered: the force-summary warning logged `ratio=1.0173125`
and neither operand, so recovering "the window is 32000 and our own prompt is
32554" meant solving the division by hand.

`--trace <path>` writes one JSON record per loop decision, tapping the
`LoopEvent` stream at the single point every event passes through. Harness
interventions are attributed to the guard that sent them, from the existing tag
registry.

**Verification:** four bugs found on the first live run, listed below.

## Phase 2 — the bugs the first run found (done)

| | Defect | Fix |
| --- | --- | --- |
| `dirge-2js0` P0 | A force-ended turn ended the whole RUN. The `ExitWithSummary` tier `break`s the inner loop, which *is* the turn loop, so control fell to finalization and stopped. A model over the threshold got one turn: its tool calls ran, results were appended, run over before it saw them. It only appeared to work because a critic or verifier gate happened to fire and restart the outer loop. | `continue 'outer` when the fold made room; stop with a notice naming `prompt_tokens` and `ctx_max` when it did not. |
| `dirge-sjxq` P1 | `glm-5.3` matched nothing in the context-window table (`glm-5.2` is listed at 1M) → 128k default. A local model is named by its FILE PATH, so `…/Qwen3.8-27B-Q8_0.gguf` matched `qwen` → 32000, smaller than dirge's own prompt. Every context tier divides by this number. | Family prefixes so point releases match; a model neither lookup knows warns once. |
| `dirge-g4lk` P1 | The verifier declined a masked test run — correctly, a piped exit status is `tail`'s — and then said "you didn't run the tests", after four passing pytest runs. | Record *why* the decline happened; the nudge quotes the command and says what to run. |
| `dirge-6gpr` P2 | `turns=` read 0 for any run that force-ends. It is the denominator every other count is read against. | Count the turn where the turn happens. |

## Phase 3 — the second run's findings (done)

Re-running the same task with the corrected verifier message is a clean A/B:
same task, same model, same prompt.

- **Old message** → the model re-ran the masked command twice more, then added
  `; echo "exit=$?"` (which reports `echo`'s status), and its final answer
  asserted "exit status 0" — a number it never had. Run ended `Unverified`
  with a genuinely green suite.
- **New message** → the model ran `cd … && python3 -m pytest -v`, clean, on the
  first try. Run ended `VerifiedGreen`.

That second run then exposed the mirror-image bug:

`dirge-hwk9.3` — **`claim_gate` and the verifier disagreed about the same
command.** `segment_kind` took the segment's FIRST token, got `python3`, and
returned `None`; the verifier matches on ANY token, found `pytest`, and
recorded the run green. So the model — which had *just* been corrected by the
verifier and complied exactly — was told its truthful report was unsupported.
Neither recogniser knew `unittest` at all.

Fixed by peeling interpreter and runner prefixes (`python -m <module>`, `npx`,
`poetry/uv/pipenv run`), deliberately not `python script.py` or `python -c`,
and a test asserts the two recognisers agree where they overlap.

## Phase 4 — context accounting (done)

`dirge-hwk9.1` (GH #772) — **per-provider `context_window`.** It was top-level
only, so with more than one provider configured there was no way to correct one
model's window without corrupting the others'. Precedence:
`providers.<name>.context_window` → top-level → model table → 128k. The loop's
override and the session's gauge now resolve from the same call, at the one
point where provider and model are both final.

`dirge-hwk9.2` — **the context gauge measured the wrong thing entirely.**
Reported live: `226.9k/128.0k, 100%, compaction soon`, one compaction, still
climbing. Three numbers are involved and the gauge combined two that do not
belong together:

- the model's advertised window (128k here, the fallback for an unknown model);
- `context_target`, default 250k, a working-budget cap —
  `effective_ctx_max = min(window, target)` is what the loop folds against;
- `session.total_estimated_tokens`, a chars/4 heuristic plus per-tool-call
  overhead over the *persisted transcript*, tool results at full length.

Compaction compares the provider's `prompt_tokens` for the **request** — whose
oversized results have been snipped and whose old turns are a summary — against
`effective_ctx_max`. So 226.9k of transcript was entirely consistent with a
request comfortably under the fold threshold, which is why only one compaction
fired. Nothing was wrong with that context. The gauge now reads
`last_prompt_tokens / effective_ctx_max`, the same two numbers, with the
estimate standing in only before the first response.

## Phase 5 — a long-horizon run (done)

A 22-test specification the model may not edit, with two traps a naive
"extract all the pieces" parser passes early and fails late (`1m30` and `1m 2`
must be rejected), forcing a restructure to whole-string validation. Steering
features enabled: `verification_tiers`, `safe_state_abort`, `claim_gate`,
`completeness_gate`, `source_gate` all advisory, `progress_stall_threshold: 3`.

Outcome: 22/22 pass, `duration.py` the only file touched — the spec was
respected. 9 turns, 8 tool calls, 0 errored, context peak 36%.

**What worked.** `boundaries=Verifier` — one gate at one boundary, which is
PR #739's arbiter doing its job (run 3 and run 4 both read
`Verifier;ClaimGate`, semicolons meaning separate boundaries, never a
collision). The progress monitor armed and fired. The per-provider window
resolved to 65,536 from `providers.qwen-local.context_window` with no
top-level key. The streaming heartbeat turned a silent 13-minute thinking turn
into a visible `ThinkingDelta` series.

**What it exposed** — `dirge-hwk9.4`, a three-feature cascade:

1. The model went green at 345s via `pytest -v 2>&1 | tail -28`.
2. `masks_failure` correctly declined it, so `verified_green` never latched.
3. The progress monitor's three progress events are a todo closed, a first-time
   file touch, or *verification going green*. None could fire.
4. `nudge_progress_stall` fired twice — the second at 618.0s of a 618.1s run —
   telling a model with a green suite it had made no progress for three turns.

Fixed by standing the stall checkpoint down while a masked decline is
outstanding — the verify nudge owns that state and has the actionable message.
**My first attempt was wrong and is worth
recording:** I proposed clearing `edits_since_verify` on a masked decline,
citing the verifier's own rule that "any verification attempt clears the
mid-run counter". Two things killed it. The stall nudge reads the *latched
green* (`run.rs:2336`), not that counter, so the change would have fixed
nothing; and `masked_command_does_not_clear_edits_since_verify` already asserts
the opposite deliberately. Reverted. What remains is a design question — should
a verification *attempt* count as progress? — and the comment at
`run.rs:2328-2335` explains why the monitor reads the latched green in the
first place, so it is not a free change.

`dirge-hwk9.5` — boundary nudges inject at `run.rs:3591` by pushing straight
into context, emitting only a `SystemNotice`; the finalization path emits
`MessageStart`/`MessageEnd`. So stall, budget, prologue, track-work,
file-touch, safe-state and reflection nudges are absent from the message
stream: the tally read `nudge_progress_stall=2` while the trace recorded one
intervention. Not user-visible (the notice covers the human), and making the
paths match risks double-rendering in the TUI, so it is filed rather than
folded in.

**One message reworded on evidence.** Told to drop the `|` or `;`, one model
produced a clean `pytest -v` (run 4) and another produced
`pytest -v; echo "EXIT=$?"` (run 6) — which obeys the letter, tries to surface
the status, and masks anyway. The nudge is now positional ("the build/test
command LAST, nothing after it") and names the three idioms that have actually
shown up.

## Phase 6 — three models, so the fixes aren't shaped to one (done)

The same 22-test task on qwen3.8-27b (local, llama.cpp), deepseek-v4-flash and
glm-5.3. All three passed 22/22 and touched only `duration.py`.

**Every one of them piped its test output through `tail`.** That is the single
most useful result here: masking is not a small-model quirk, it is what models
do, and the guard that declines it therefore fires on everyone. Three of the
four observed nudges produced a clean re-run on the first try (qwen with the
reworded message, deepseek, glm); the fourth was qwen under the earlier
wording, which is what motivated the rewording.

The cross-model runs also found `dirge-hwk9.6`, and it was self-inflicted:
deepseek ran a clean `pytest -q` and reported *"Confirmed with a real exit
status: 22 passed in 0.01s, exit 0"* — and the claim gate fired, because
`"exit 0"` sat in the build/lint list and no build had run. The verify nudge is
what asks for the exit status in the first place, so one guard requested a
number and another penalised the answer. `"exit 0"` is now kind-agnostic:
satisfied by any observed verification, while a build still cannot support
"N passed" and a test cannot support "clippy clean".

Two fixes were confirmed live rather than only in tests: `glm-5.3` resolved to
a 250k effective window (it would have been the 128k fallback before), and
`[stall]` appeared as a properly attributed intervention in the deepseek trace,
which is `dirge-hwk9.5` working.

## The small-window path (dirge-tva8, done)

Breadcrumb tool schemas below a 48k window: each tool's description trimmed to
its first sentence, each parameter's to a clause, everything structural — names,
types, enums, required-ness — untouched. No tool is dropped; a model that
cannot see a tool cannot ask for it, and that failure is silent and looks like
incapability, whereas losing the prose about *when* to prefer a tool degrades
gracefully.

Measured on the same task and model at a 32k window: **16,202 → 12,249 prompt
tokens**, context peak 51% → 39.5%. The model still reached for `list_symbols`
unprompted and answered correctly, which is the part that mattered — the long
descriptions exist to improve tool *selection*, and this trades some of that
for fitting at all.

`compact_tool_schemas`: `auto` (default) / `on` / `off`. Sized against the same
window the session gauge and compaction use, passed in rather than re-derived.

## What the TUI actually renders

Tracing the *front-end* event stream (not just the loop's decisions) showed
every harness intervention rendering its body **twice** in the TUI: the notice
carries `"harness intervention: {summary}\n{body}"` because headless sees only
it — `--print` renders `SystemNotice` and ignores `UserMessage` entirely —
while the TUI gets both and renders the body from the message as well. The
notice now shows its summary line only in the TUI; the body stays on the
message path, which is the copy `dirge-m10x` guarantees survives the next
turn's stream anchor. Headless output is unchanged.

Fixing that is what made `dirge-hwk9.5` safe: boundary nudges can now emit
`MessageStart`/`MessageEnd` like the finalization path without putting the body
on screen a third time.

## Phase 7 — the stall on a finishing run (dirge-hwk9.7, done)

Filed as a judgement call: the stall checkpoint fired as a *successful* run
concluded, qwen at 618.0s of a 618.1s run and deepseek at 55.3s of 55.4s. The
report assumed a coincidence of timing. It is not one.

**A run's endgame is barren by definition.** By the time a run is finishing its
todos are closed (so they cannot decrease), its files are touched (so they
cannot increase) and its green is latched (so there is no fresh edge). Every
one of the three progress signals is structurally unable to move. Any run with
a multi-turn endgame was therefore *guaranteed* to be told it had stalled,
given enough turns — which is why two different models produced the same
symptom to within 0.1s of the end.

The monitor's own opening paragraph says what it is for: *successful*, varied,
useless tool calls, the one failure mode no other guard can see. It nevertheless
scored every boundary, including two kinds that are not that — a turn with no
tool calls (the model wrote prose; in practice its final answer) and a turn
whose calls all failed (the failure tracker's and the storm breaker's
territory). The second is worse than noise: the stall text asserts "the calls
are succeeding", and a traced run shows it delivered on a boundary whose single
call was permission-denied, after which the model spent its last words arguing
that nothing was blocking it.

Three changes, one seam:

- **Only a boundary with a successful tool call is judged.** This is the
  module's stated contract, finally encoded.
- **The boundary that ends the inner loop belongs to the finalization arbiter.**
  Both were polling it, unranked, so two harness messages could land before one
  assistant turn. `dirge-5mtx.2` closed exactly this at the mid-turn boundary
  and left the seam between the two arbiters open. Safe-state is exempt — an
  abort with a tree restore is not steering — and the exemption is read from the
  policy at the safe-state branch, because encoded only by ordering it was dead
  code that no mutation could kill.
- **`record_turn` offers, `commit` spends.** A checkpoint the arbiter declines
  is no longer charged. That was the wart documented at `poll_boundary_nudge`,
  and it meant the masked-verification decline silently burned one of a run's
  two stall nudges.

Rejected on the evidence: the bead's own first suggestion, that a boundary
*following a verification* should count differently. Resetting on any
verification run kills `green_suite_thrash_on_one_file_still_stalls` —
edit/test/edit/test on one file is precisely the case the monitor exists for.

Measured, same task and model before and after: `[stall]` at 56.9s of a 58.9s
run → no stall, `VerifiedGreen`, with the trace showing the offer stood down
once as `masked-verification` (budget kept) and twice as `concluding`, and
`[verify-before-done]` doing the work alone on both terminal boundaries.
glm-5.3 the same, 22/22.

**The discrimination control matters more than the fix.** A task that writes a
file (arming the monitor) and then searches for a symbol that does not exist
fired `[stall]` twice — at 13.3s and 38.2s of a 241s run, both mid-run where the
model still had turns — and stood down on the final boundary. Narrowing the
monitor did not mute it.

## The bypass the trace turned up (dirge-5flx)

The barren boundary that fired the stall was barren because a `bash` call had
been permission-denied — and chasing *that* found a permission-containment bug.

`parse_bash_segments_full("a && b 2>&1 | c")` returned `(["c"], false)`. The
`redirected_statement` arm recursed only into children whose kind was
`command`/`pipeline`/`compound_statement`/`subshell` and dropped everything else
through a bare `_ => {}`; tree-sitter parses that input as
`redirected_statement(body: list(a && b))`, so both commands vanished and the
engine authorized a command it had never seen. End to end against the release
binary:

```
python3 -c "open('pwned_a','w')"                        -> denied
echo hi && python3 -c "open('pwned_b','w')" 2>&1 | cat  -> ran, wrote the file
```

Any denied command runs by prefixing `echo hi &&` and appending `2>&1 | cat`.

Two fixes. The arm now recurses into everything that is *not* a redirect
operand, so an unknown grammar node over-collects rather than disappearing —
the safe direction for a permission input. And a command the splitter could not
decompose at all is marked **complex**, which is the backstop that would have
contained this bug instead of letting it become a bypass.

A filter whose reject path is silent is a filter nobody can audit. That
sentence is already in this repo's memory twice, from `hallucinated_tool_names`
and the scavenger's dropped names. This is the third, and the first where the
thing being dropped was a permission input.

## The harness demanding what it forbade (dirge-e1nv, done)

`pytest **` is allowed; `python`/`python3` deliberately are not, because
`python -c "…"` runs anything. Nothing bridged the two, so `python3 -m pytest`
— the commonest way a model runs pytest, and the shape the verify nudge asks
for — prompted, which headless turns into a denial. Until `dirge-5flx` the
bypass hid it: the only form that ran was `… 2>&1 | tail`, the masked shape the
verifier declines. Third instance of one guard punishing what another demands,
after `dirge-hwk9.6` and `dirge-yv0d`.

**The filed design was wrong, and the existing code says why.** The plan was to
match allow rules against the exec-prefix-stripped form as well as the raw.
`match_candidates` exposes commands raw *on purpose* (`dirge-8zem`):
`PATH=/tmp/evil git push` and `./env git push` run a different binary under an
allowed name, and `env_and_wrapper_prefixes_do_not_ride_an_allow_rule` pins it.
The proposal would have reopened that hole to close this one. Reading the
comment on the thing you are about to generalise is worth more than the
generalisation.

What shipped instead names the module form explicitly, generated from
`PYTHON_MODULE_TOOLS` so the eight rules are not a second list to keep in step
with the first, with a test that every entry is still allowed under its own
name — the derivation source can go stale too. `-m` does not make the
interpreter safe; it names a module that must still be allowed on its own, so
`python3 -m http.server` and `python3 -m pip install` keep prompting. The deny
side sees through the module runner as well, or the new allows would be a way
around a deny that used to hold.

Measured on the same task and stock config that had denied every verification:
3 errored bash calls and an unverified run → 0 errored, 22/22, model correcting
its masked command on the first nudge.

## Method notes

- **The premise check was the baseline run.** Doing it before building anything
  is what established that the observability gap was real rather than assumed.
- **The reporter is unverified code, every time.** `loop-trace.py` counted turns
  from assistant `message` records; when the trace stopped writing those it read
  0 turns for a 15-turn run. Two of the claim-gate tests first went green
  *against the very bug they were written for*, because `"4 passed"` is not a
  detected claim — `claims_test_result` requires two digits. Both were caught by
  pairing an assertion with its negation, which is the only thing that has ever
  caught this class here.
- **A stale binary reads exactly like a broken feature.** The per-provider
  window appeared not to work; the binary predated the change.

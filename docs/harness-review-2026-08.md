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

Filed, not patched. **My first attempt at a fix was wrong and is worth
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

## Still open

`dirge-tva8` P0 — **dirge's own prompt does not fit a small window.** Measured,
same task and model, two configs:

| tools | first request |
| --- | --- |
| 34 (built-in only) | 16,172 prompt tokens |
| 75 (plus global MCP servers) | 32,621 prompt tokens |

32,621 exceeds a 32k window in its entirety: the run cannot take a single turn.
Nothing checks the assembled prompt against the resolved window at run start,
and nothing trims the tool surface for a small-window model. Now *visible*
rather than silent, but not solved — and the two halves (a startup check;
what to do about the tool surface) are separable decisions.

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

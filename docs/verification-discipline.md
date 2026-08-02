# Verification discipline

A set of loop and tooling changes with one thing in common: they exist because a
check *ran*, reported success, and could not have failed for the reason that
mattered.

Every one came out of building the loop-control work in `dirge-5mtx`, and not
from looking for them. Six distinct failures surfaced during that work; all six
were verification failures, none was a steering failure. That ratio is the
reason this page exists as its own document rather than a paragraph in
[agent-loop.md](agent-loop.md).

## Quick reference

| Feature | Config key | Default |
|---|---|---|
| Project gate | `verification_command` | absent (off) |
| CI command advisory | — | always on, advisory only |
| Masked-command guard | — | always on |
| Exploration-prologue bound | `progress_prologue_cap` | `24` |
| Capability tier | — | observed always; only `Struggling` changes behaviour |

## The pattern

Nine failures, one shape:

| | What happened |
|---|---|
| Wrong gate | `cargo test` passed; the real gate, `cargo clippy -- -D warnings`, had six hard errors |
| Result ignored | A test suite went red and the commit landed anyway — the commands were chained with newlines, not `&&` |
| Status masked | `cargo clippy \| tail -2` reported zero, because that zero was `tail`'s |
| Can't discriminate | An A/B with no arm overrides "passed" while shipping a broken multi-value parser — neither path was exercised |
| Mechanism unconfirmed | Arms compared on outcomes without checking whether the code under test ever ran. It hadn't |
| Partial gate set | clippy and the suite were green for the whole epic on a branch that failed `cargo fmt --all --check` in 31 places |
| Signal never fed | A counter weighted into the capability formula had zero production callers. Its test called the recorder directly, so it passed while the counter was structurally always 0 |
| Plumbing misreported | `gh api … \| grep -q` under `pipefail`: grep exits on match, the producer takes SIGPIPE, and a healthy channel reports as missing |
| Prerequisite as outcome | A test hard-failed when a library wasn't installed, so the macOS suite was permanently red on a correct tree |

A gate that cannot fail is worse than no gate, because it is trusted.

The last row is the one to take seriously, because it happened on the branch
that added this page, after everything above it had already shipped. The
tooling was not at fault: this repo's CI advisory already names `cargo fmt
--all --check`, so a model verifying here is told about it. A human ran clippy
and the tests instead, every time, and both were honestly green. Which is the
point — **a single-command gate is a habit, not a design**, and no amount of
knowing better substitutes for running the whole set.

The last three came later, and they move the problem one layer out. The first
six are about running the wrong check, or ignoring its answer. These three are
about a check that ran, answered, and whose answer meant nothing:

- **Signal never fed** — the producer was never wired, and a unit test that
  calls the recorder directly cannot tell. Test the function the production
  path calls, not the one you wish it called.
- **Plumbing misreported** — the exit status carried "I could not ask the
  question" in the same channel as "the answer is no". A check whose failure to
  run is indistinguishable from a finding cannot be believed on a red. Capture,
  then match; never `cmd | grep -q` when `cmd`'s output can outlive the match.
- **Prerequisite as outcome** — a missing environment dependency is a skip, not
  a failure. But converting one to a skip has its own trap: if the skip
  condition implies the assertion, the guard becomes vacuous rather than
  lenient, and nothing announces that it can no longer fail. Establish a
  precondition with a mechanism independent of the thing under test.

The general form, which every row of the table satisfies: **a check is only
worth its verdict if you know what would have made it say the other thing.**
Running it once against a known-good input and once against a known-bad one is
cheap and answers that directly. The `pipefail` bug above was found exactly
that way — the known-good run went red on one channel out of five.

## Project gate (`verification_command`)

Names the command whose pass is the only honest green. When it is set and that
command has not passed, a result that would have been `VerifiedGreen` becomes
`FastGreenOnly` instead, so the existing full-suite escalation carries it at
finalization — no new status, no new message type.

Matching is by `(program, subcommand)` signature rather than string equality,
since `RUSTFLAGS="-D warnings" cargo clippy --all-targets` and `cargo clippy
--all-targets -- -D warnings` are the same gate. Environment assignments are
skipped, flags ignored.

A gate *specification* naming a chain means its last segment: `cargo fmt &&
cargo clippy` specifies the clippy gate. An *observed* command is matched
against every segment, because the caller only inspects commands that passed,
and under `&&` a passing chain means every segment passed. `cargo clippy &&
cargo test` therefore satisfies a clippy gate.

Unset by default, and behaviour is unchanged when unset — including in `off`
mode. Setting it is an opt-in that may change `off`-mode results for that user;
the byte-identical guarantee is for people who did not opt in.

## CI command advisory

Auto-detecting *the* project gate from CI does not work, and this repository is
the proof. Its `ci.yml` yields four distinct recognized signatures — `cargo fmt`
and `cargo clippy` (Fast), `cargo build` and `cargo nextest` (Slow). Two are
equally "strongest", so any rule that picks one is guessing, and a wrong
auto-gate is worse than none: it downgrades every honest green and nags forever.
Refusing to guess returns nothing on exactly the repository the feature was
written for.

The premise was wrong. Real CI does not have one gate; it has several, all
required. So the resolver returns the recognized set as *information*, and the
verifier folds it into its nudge:

> This project's CI runs: `cargo clippy --all-targets -- -D warnings`, … — a
> green check that isn't one of those may not be what gets enforced.

That addresses the original failure — an agent ran `cargo test`, saw it pass,
and never knew clippy was what CI enforced — while touching no verdict, so it
cannot cause a false green or a false nag.

The scan is line-oriented rather than a YAML dependency: a mis-parse degrades to
"no advice", never to a wrong verdict. It handles `run: cmd` and `run: |` block
form, and both the list-item (`- run:`) and plain-key spellings. Commands
carrying `${{ … }}` are skipped, since the expansion is unknown and a
half-substituted command is not something to hand a model as fact. Entries are
deduped by signature, so three clippy invocations read as one instruction.

## Masked-command guard

Measured, before this existed:

```
cargo test || true                    -> VerifiedGreen
cargo clippy --all-targets | tail -2  -> VerifiedGreen
cargo test; echo done                 -> VerifiedGreen
```

The gate reads pass/fail from the exit status of the whole command, and in each
of those the status belongs to `true`, to `tail`, to the `echo`. A red build
latched green.

A masked command reporting **success** is now not recorded at all: the status
stays `Unverified`, the gate asks again, and `edits_since_verify` keeps
counting. "We don't know" is the honest answer, and it fails toward nagging
rather than toward a false green.

A masked command reporting **failure** is still recorded red — something in the
chain genuinely failed, and that direction is trustworthy.

`&&` is deliberately not masking: it short-circuits, so a failing left side *is*
the exit status. Redirections carry no pipe. Over-detecting would decline good
verifications and nag forever, which is the same harm pointed the other way.

## Exploration-prologue bound (`progress_prologue_cap`)

The progress monitor's stall counter arms only on a progress event, so a run
that produced *nothing* never armed and the monitor was structurally incapable
of reporting the one case it most needed to. Observed: 60 turns and eight
minutes of successful, varied grep/read calls with nothing written,
`progress_stall_threshold` set and on, and no other guard able to see it — storm
needs identical repeats, the failure tracker needs errors, safe-state needs a
failure streak.

The arming rule is right; nagging a run that opens with twenty reads would fire
on every research task. What was missing is a ceiling on how long that prologue
may last.

This is an **upper bound, not an eager nudge**. Success is "never fires on
normal work, fires on the runaway", so validation is asymmetric rather than a
comparison of means. Past the cap, a run that has produced nothing gets one
checkpoint pushing for the smallest possible first write — deliberately worded
differently from the stall message, because "you haven't produced anything yet"
is a different diagnosis from "you were producing and stopped", and collapsing
them would tell a run that had written files that it had written none.

It counts **tool calls as well as boundaries**. `record_turn` fires once per
boundary, and the observed thrash batched 40+ calls into a single turn — one
barren boundary by that measure. Same granularity bug as the verifier's
batched-edit miss, and the models that batch hardest are the ones that thrash.

## Capability tier

Estimated from what the run is actually doing — weighted failure rates over tool
calls — and **never** from model identity. Keying on the provider name would be
the baked-in decision this work exists to remove, and the data says it would be
wrong anyway: on the same task, the stronger model was the one in trouble.

Measured across the supported range on a reconnaissance-heavy scenario:

| model | calls | errored | max streak | rep_invalid | tier |
|---|---|---|---|---|---|
| deepseek-flash | ~22 | 0% | 0 | 0 | strong (6/6 runs) |
| glm | ~19 | ~4% | 1 | 1 | nominal (3/6 runs) |
| Qwen3.6-27B-Q8 (local) | ~20 | 10–15% | 0–2 | 0–1 | nominal (4/6), strong (1/6), **struggling (1/6)** |

Qwen3.6-27B is treated as the **low bound** of supported models: whatever works
with it is good enough generally. So `Nominal` is the bottom of the supported
range, and `Struggling` sits below it — a safety net for a model doing
materially worse than the low bound, or a long-horizon task where a capable
model degrades.

The single `Struggling` observation is the run that **failed the task and never
wrote a file**; the other five qwen runs all succeeded. That is what a safety net
should look like: rare, and correlated with real failure.

**Do not tune the weights to make it fire more often.** An earlier reading of
n=1 data suggested `repair_invalid` was overweighted at 4 and plain errors
underweighted; more data showed the opposite — one unrepairable argument
contributed more than both errors combined, and that weight is exactly what
selected the failing run. Retuning would have removed the discrimination that
worked.

Adaptation is **one-directional**: the tier may add support, never remove it.
`Nominal` and `Strong` are both bit-identical to the pre-estimator constants,
so a default install is untouched; only `Struggling` moves anything, and only
toward earlier and more frequent help.

Two thresholds are derived, both scaled down by `Struggling` alone:

| Constant | Base | `Struggling` | Guard |
|---|---|---|---|
| `FAST_VERIFY_EDIT_THRESHOLD` | 3 | 2 | verify after N edits with nothing run |
| `FAILURE_REFLECTION_THRESHOLD` | 3 | 2 | recovery checkpoint after N consecutive failures |

The second is the best-matched derivation in the loop and the only one where the
signal and the trigger are the same observation: the estimator is *built* from
failure counts and streaks, and this guard fires on consecutive errored results.
It is read at every poll rather than at tracker construction — the tracker is
built at run start, where the estimator is always `Nominal` by warm-up, so a
threshold fixed there would read the neutral tier every time and be inert by
construction. That is the shape to check for before wiring anything else here.

Two things the tier deliberately does **not** move, both inside that same
tracker: the permission checkpoint (a denial streak is a policy wall, and
nothing the estimator counts measures how often the user's rules block a call)
and the safe-state abort's 2× signal (that rung spends one of two hard-capped
aborts and, in `auto` mode, writes to the tree — pulling it forward is not
"support"). And the derived value is floored at 2: at 1 the checkpoint fires on
the first errored call, which is not what a *repeated*-failure guard is for.

`Strong` driving nothing is the part that took a correction to get right. The
counters observe **tool-call mechanics only** — errored calls, repaired
arguments, invented tool names, scavenged text, storms, streaks. Nothing in
that set moves based on whether the model verifies its work or makes progress.
So a `Strong` reading is evidence about argument hygiene and nothing else.

An earlier cut used it to relax `FAST_VERIFY_EDIT_THRESHOLD`, reasoning that
extra latitude for a model with no observed failures could not cause a nudge
storm. That much is true, but it inverts the risk, and this page's own table is
the counter-example: both failures worth having a guard for came from models
the estimator reads as `Strong`. The 60-turn reconnaissance thrash was
deepseek-flash at a 0% tool-call error rate — flawless mechanics, nothing
written. The wrong-gate green came from the same tier. Relaxing verification
pressure on exactly that class is backwards, so the derivation was removed.

The general rule, worth applying before wiring any new threshold to the tier:
**a signal may only tune a guard that fires on the same thing the signal
measures.** And note that a budget of exactly 1 cannot be scaled at all —
`1 × 3/2` truncates back to 1 — so routing a one-shot budget through the
estimator looks like adaptation and does nothing.

## Measuring a loop-control change

`scripts/loop-ab.sh` runs the same task N times per arm and compares. Three
things it enforces, each from a failure above:

**Mechanism check.** Every run reports how many harness nudges fired. A
treatment arm showing zero did not exercise the change, and its deltas are
noise. Without this, an A/B comparing prologue cap 9999 against 24 read as
"turns better in every model" when the prologue had fired zero times in either
arm.

**Noise floor.** Run an A/A first — both arms configured identically. On
`recon-real` that produced 18 vs 36 turns on one model and 15 vs 33 on another:
same config, roughly double. Any delta inside the control arm's own spread is
reported as `~noise` rather than given a direction. Proportions use a `1/n`
floor instead, since they are binary per run — one run flipping is the smallest
movement the sample can express.

**Per-model reporting.** Absolute numbers are not comparable across models, so
arms are compared within each model and the summary states whether the direction
held across them. A single-model result is not evidence for a steering change,
and the output says so.

The practical consequence: at n≤3 against a ~2× floor, effects justified by
"this reduces turns" are not measurable at any sample size worth paying for.
Prefer changes whose success criterion is **structural** — did the mechanism
fire when it should, and stay silent otherwise — because those hold at n=1.

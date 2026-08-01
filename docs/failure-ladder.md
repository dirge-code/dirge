# The failure ladder and progress signals

Four loop features that watch a run for trouble the ordinary gates miss: tiered
verification, a progress monitor, a safe-state abort rung, and a
residual-objective handoff. All are off or inert by default.

They come from the validation report for NASA's Deep Space 1 Remote Agent
Experiment (Nayak et al., *Validating the DS1 Remote Agent Experiment*,
ISAIRAS'99) — a 1999 paper about a symbolic planner flying a spacecraft, not
about language models. What transfers is the control structure, not the
techniques. Each section below says which finding it came from, because the
rationale is the part worth keeping when someone later wants to change the
behaviour.

## Quick reference

| Feature | Config key | Default | Writes files |
|---|---|---|---|
| Verification tiers | `verification_tiers` | `off` | no |
| Progress monitor | `progress_stall_threshold` | absent (off) | no |
| Safe-state abort | `safe_state_abort` | `off` | only in `auto` |
| Residual objectives | — | always on | no |

Enable the lot:

```json
{
  "verification_tiers": "advisory",
  "progress_stall_threshold": 4,
  "safe_state_abort": "advisory"
}
```

Every injected message is prefixed with a tag (`[verify-before-done]`,
`[stall]`, `[budget]`, `[safe-state]`). The TUI renders these under a system
handle rather than as your input; headless runs mirror them to stderr as
`SystemNotice` so a `--print` or `--loop` consumer can see why the model
changed course.

---

## Verification tiers

**Config:** `verification_tiers`: `off` (default) | `advisory` | `blocking`

The verifier gate used to ask one question — did *some* build/test command run,
and did it pass — and only when the run tried to finish. `cargo check` and
`cargo test --all-features` were indistinguishable to it, so verification was
untiered and end-loaded.

With tiers on, each recognized verification command is classified:

- **Fast** — typecheck, lint, format-check, a single targeted test.
  `cargo check`, `cargo clippy`, `tsc --noEmit`, `ruff`, `eslint`,
  `cargo test some_name`, `pytest tests/foo.py::test_bar`, `go test -run X`.
- **Slow** — the full suite or a full build. Bare `cargo test`, `npm test`,
  `make`, `cargo build --release`, bare `pytest`.

Two things change. Mid-run, once three code edits pile up with nothing run
since, one nudge asks for a *fast* check now. At finalization, if only the fast
tier ever passed, one escalation asks for the full suite.

**Unknown commands default to Slow.** The tier signal only ever adds nudges, so
an unrecognized command errs toward silence — a missed escalation, never a
false nag.

**Tier flags record green coverage, not invocation.** A red `cargo test`
followed by a green `cargo check` still reads as fast-green-only, because the
suite was never actually seen passing.

Per-run message ceiling: `off` 1, `advisory` 2, `blocking` 3. The legacy
red/unverified nudge and the tier escalation hold separate budgets — they
answer different questions ("did anything run?" vs "did the suite run?") and
the states are mutually exclusive at any instant.

> **Paper §4.2.** RAX ran a fidelity pyramid: ~200 planner variations and
> hundreds of failure contexts on cheap simulators running 7× real time, six
> off-nominal scenarios on the medium testbed, nominal-only on flight hardware.
> What licensed the split was that the interfaces were identical across
> platforms — only the fidelity of the responses improved. The same section
> notes most bugs were caught by developers testing *during integration*; the
> expensive end-stage campaign "found few".

## Progress monitor

**Config:** `progress_stall_threshold`: absent (off) | a number ≥ 2

Every other loop guard keys on **errors**. The storm breaker needs an identical
repeated call; the failure tracker needs errored results; the file-touch tracker
needs the same file touched repeatedly. A model making *successful*, varied,
useless calls trips none of them and just burns the run until `max_turns` cuts
it off.

The monitor watches turn boundaries for a **progress event**:

- a todo item closed (the unfinished count went *down* — writing more todos is
  planning, not progress)
- a file mutated that was never mutated before (re-editing one file is flat, and
  that is the thrash being watched for)
- verification going green

Two signals, both bounded:

- **stall** — `progress_stall_threshold` barren boundaries → one checkpoint
  asking what is blocking, and to change approach or cut scope. At most twice
  per run, re-arming for a full threshold in between.
- **budget** — crossing 60% and 85% of `max_turns` → one notice each, stating
  turns used and remaining. Requires `max_agent_turns` to be set; without a cap
  there is nothing to report.

**The stall counter arms only after the first progress event.** A run that opens
with twenty reads is exploring, not stalling. Without this the monitor would
fire on every research task.

One subtlety worth knowing if you touch this: the monitor reads the *latched*
green, not the tier-aware one. The tiered staleness rule flips green false after
any post-green edit, so reading it here would manufacture a fresh false→true
edge on every edit-then-test cycle and reset the counter forever — silently
disabling the monitor for green-but-not-converging runs, exactly the case it
exists for. Staleness answers "verify again?"; progress answers "did this run
reach a new state?".

> **Paper §4.4.** The planner's dominant late failure was "operating correctly
> but being unable to find a plan within the allocated time limit since its
> search was thrashing" — not an error, a non-result within budget. §4.1
> separately documents that RAX ran against measured resource envelopes (32 MB
> of RAM, 45% of CPU, a peak of 29 MB actually observed); dirge enforces
> `max_turns` but never told the model, and a silent hard stop can't prompt
> triage the way a visible countdown can.

## Safe-state abort

**Config:** `safe_state_abort`: `off` (default) | `advisory` | `auto`

The executive's failure ladder in the paper has three rungs. dirge already had
the first two:

1. **Try an alternate method** — the storm breaker's reflect-then-pivot, with
   the reflexion log accumulating every abandoned approach.
2. **Request a recovery** — the failure tracker's checkpoint at three
   consecutive errored results.
3. **Abort, reach a safe state, re-plan** — this feature.

Rung 3 fires when *all three* hold: the failure streak reached twice the
checkpoint threshold, unverified edits sit on the tree, and a verified-green
point exists behind the run. It **replaces** that boundary's rung-2 checkpoint
rather than adding to it — telling the model to both retry and abort in one
breath is contradictory.

The message carries the reflexion log and the streak's failure excerpts so the
re-plan doesn't walk back into a dead end, and asks for **one** new approach
rather than a menu (the paper's recovery expert recommends a single action; a
menu invites cycling).

Bounded three ways: a hard cap of two aborts per run, once per failure streak,
and a can't-loop argument written out in the module docs.

### advisory vs auto

**advisory** performs no file writes. It tells the model the tree is unverified
on top of a known-good point and lets it decide. It deliberately names no
restore command — `/rewind` is not a slash command (it's an Esc-Esc picker keyed
by user-message *index*), and rewinding *to* the green turn would revert the
green-making work itself, since snapshots hold pre-mutation state.

**auto** restores the tree itself — but only after proving it can. This gate is
the whole reason auto exists at all:

`snapshots::capture` is wired into the edit tools and **not** into `bash`. A
`sed -i`, a `>` redirect, or an in-place formatter mutates a file with no
pre-state recorded. Restoring the captured edits while leaving that alone
produces a tree in a state that never existed — half green, half post-green,
quite possibly not compiling — arrived at behind the model's back while the
failure streak keeps climbing. Strictly worse than the broken tree it started
from.

So before writing anything, the loop:

1. fingerprints (via git) every file differing from `HEAD` at the green moment,
2. fingerprints again at abort time,
3. treats anything that changed, appeared, or vanished in between as mutated
   since green — whichever tool did it,
4. proceeds **only** if the snapshot store can put back every one of them.

No repo, no fingerprint, or one uncaptured file → it declines and behaves
exactly like `advisory`. The failure mode is "auto didn't fire", never "auto
left a broken tree".

Baselining against a fingerprint taken at green — rather than against `HEAD`
directly — is what keeps a dirty starting tree safe: uncommitted work predating
the run hashes the same at both samples, so it never reads as changed and is
never a restore target.

On a successful restore the message swaps its closing paragraph; telling the
model to undo edits the harness already removed would send it chasing changes
that no longer exist.

> **Paper §2.** "If the EXEC is unable to execute or repair the current plan, it
> cleanly aborts the plan and attempts to bring the spacecraft into a safe state
> while requesting a new plan from MM."

**A caveat on maturity.** The coverage gate is well covered by tests — including
against real git repos, and including the case where a `bash`-style mutation is
present and nothing is touched. What has *not* happened is auto firing
end-to-end against a live model: triggering it needs six weighted failures plus
a green point plus unverified edits, which is hard to provoke deliberately. If
you turn it on, `advisory` first is the conservative order.

## Residual objectives

**Always on. No config key** — it adds no message, only enriches one that
already fires, and returns nothing when the board is empty, so the no-op comes
from the data rather than a flag nobody would set.

When a run is cut short by `max_turns`, the truncation notice now lists what is
still outstanding on the todo board:

```
[dirge] Max agent turns (5) reached. Stopping the run. …

Objectives still outstanding (5):
- Add a unit test for each function
- Make alpha_at return Option<i32>
…
```

Outstanding-only, not a done/remaining split: the board mirror drops terminal
items, so a "done" count would be invented from data that isn't there. The same
block is reused in the session digest.

The `[dirge]` prefix stays first — the headless truncation detector matches on
it, and a test guards that specifically.

> **Paper §3.4/§5.** The 2-day scenario aborted at roughly 70% of its validation
> objectives. Within about 10 hours the team designed, tested and flew a 6-hour
> scenario targeting precisely the remaining 30%, and the pair reached 100%.
> That follow-up is only possible if something states what is outstanding.

## What was deliberately not built

**Horizon replanning.** The paper's Mission Manager re-planned per horizon,
folding in the executive's projected state, and the second plan contained an
activity that generated the third. That mattered because the plan was a fixed
artifact computed ahead of time against a state that drifted. dirge's todo list
is re-read into the prompt every turn and rewritable at will, so there is no
stale plan to refresh. An item-count boundary would fire whether or not anything
was wrong, and the progress monitor already catches the case where a run stops
converging.

## Related

- [`docs/agent-loop.md`](agent-loop.md) — turn structure and the finalization
  gate ordering these hook into.
- [`docs/config.md`](config.md) — the full config surface.

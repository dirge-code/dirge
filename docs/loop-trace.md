# The loop trace

`dirge --trace run.jsonl` writes one JSON record per decision the agentic loop
makes. `scripts/loop-trace.py run.jsonl` renders it as a timeline plus a
summary.

```
dirge -p "fix the three bugs in inventory.py" --trace run.jsonl
scripts/loop-trace.py run.jsonl
scripts/loop-trace.py run.jsonl --summary
scripts/loop-trace.py run.jsonl --kind tool_start,tool_end
```

`DIRGE_TRACE=<path>` is the env twin of the flag, for runs you do not control
the argv of.

## What it is for

The loop already emits a per-run aggregate on the `dirge::gates` target — turns,
errored calls, which gates fired. That answers *how did this run go*. It cannot
answer *what happened, in what order, and why*, and those are the questions a
harness review is made of. A run where the critic fired and the model then
edited a file is identical on the tally to one where the model edited the file
and the critic fired afterwards; one of those is the harness working and the
other is a bug.

The trace is aimed squarely at four questions:

- **Which feature moved the model?** Every harness intervention is recorded with
  the guard that sent it and that guard's own account of why. In a raw
  transcript an injected steer is just another user message.
- **Could the model see the feature at all?** `run_start` lists the tool names
  the run actually offered. A tool the model was never given is not a tool it
  failed to use.
- **What did the context manager decide?** Every turn, including the turns where
  it decided nothing — the silent verdict is the one no log line carries.
- **Did the model get honest feedback?** Tool calls and their results are joined
  by id, so a call that failed, or one whose result never came back, is visible.

## Records

One JSON object per line. Every record carries `ms` (since run start), `seq`
(monotonic), and `kind`.

| kind | what it says |
| --- | --- |
| `run_start` | model, resolved `ctx_max`, `max_turns`, and the tool names offered |
| `turn_end` | the finalized assistant message: its text, tool-call count, stop reason |
| `message` | a user turn, a tool result, or an **intervention** (`guard`, `why`) |
| `tool_start` / `tool_end` | name, arguments, result, error flag, joined by `id` |
| `usage` | provider-reported input / output / cached tokens for the turn |
| `context` | the context manager's verdict, with `prompt_tokens`, `ctx_max`, `ratio` |
| `compaction_start` / `compacted` | tokens before and after, and which kind of fold |
| `retry`, `system_notice`, `repairs`, `escalation`, `checkpoint` | as named |

Payloads are excerpted to 400 bytes and a truncated excerpt ends in `…`, so a
`read` of a large file cannot drown the trace.

## How it stays honest

The trace taps the `LoopEvent` stream at the single point every event passes
through on its way to every consumer — the pump in `integration.rs`. It is
therefore not a second set of call sites to keep in step with the first: an
event the UI can see is an event the trace can see. `describe()` matches
exhaustively with no `_` arm, so a new `LoopEvent` variant does not compile
until it says how it traces.

Interventions are attributed through `intervention::HARNESS_TAGS`, the registry
the headless notice mirror and the TUI's attribution already share, so a guard
added later is traced without anyone editing the trace module.

Two things never become events and are recorded explicitly: the tool set at
`run_start`, and the context manager's verdict each turn.

When tracing is off, `enabled()` is a relaxed atomic load and the tap returns
before touching the event.

## Reading a trace

The summary is usually enough to tell a healthy run from a sick one:

```
model        /Users/…/Qwen3.8-27B-Q8_0.gguf
window       65536 tokens
tools        34
turns        15
tool calls   12 (0 errored)
  by tool    bash×6, read×2, edit×2, list_dir×1, write×1
interventions 2
  [verify-before-done] ×1 — asked the model to verify before finishing
  [claim-check] ×1 — the answer asserted something the run never checked
context peak 37.6% of window
tokens       306687 in / 6219 out / 303807 cached
```

Things worth looking at directly:

- **`FORCE-ENDED n turn(s)`** in the summary means the context manager cut turns
  short. On a correctly-resolved window this should be rare; if it is every
  turn, check `ctx_max` on the `run_start` line against the model's real
  window.
- **`context peak` near or above 100%** with a small transcript means the window
  was mis-resolved, not that the context is full. `run_start`'s `ctx_max` is the
  number to check, and `context_window` in config.json is the override.
- **An intervention firing on something the model demonstrably did** is a
  steering bug. Cross-check the guard against the `tool_start` records before
  it — that is how the masked-exit-status case (dirge-hy4k) was found: the model
  had run pytest four times and was being told it had not run the tests.
- **`cached` far below `input`** after the first turn means the prompt prefix is
  moving between turns and the provider cache is being rewritten.

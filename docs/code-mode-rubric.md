# Code-mode rubric

The `code_mode_rubric` config flag (default off) appends a short block to the
system prompt that nudges the model, on a **bulk or fan-out** operation — the
same kind of tool call repeated across many items — to write **one `bash`
script** that does the whole sweep and prints only the distilled result,
instead of issuing the calls one at a time.

The idea is the one measured in Anthropic's ["Code execution with
MCP"](https://www.anthropic.com/engineering/code-execution-with-mcp) and
Cloudflare's "Code Mode": raw intermediate data stays in the execution
environment, and only what the script returns reaches the model's context.
dirge already has the *mechanism* — the `bash` tool's stdout is all that comes
back, so a script that greps/counts/munges and prints a summary never puts the
raw bytes in context. What was missing was the *rubric* telling the model to
reach for that path. This flag adds it.

## The rubric

Lives in `CODE_MODE_GUIDANCE` (`src/agent/prompt.rs`), appended to the preamble
by `append_code_mode_guidance` (`src/agent/builder/preamble.rs`) only when the
flag is on. Gist:

- For ~10+ similar items, or any scan whose raw per-item output you don't need
  to read, write ONE `bash` script that returns only the distilled result.
- Only the script's stdout enters context; the raw data stays in the subprocess.
- It refines the base "don't use bash for single-file ops" rule — one file at a
  time still uses `read`/`grep`/`edit`; the script is for the aggregate across
  many items. It does not apply to a handful of calls (run those in parallel) or
  to edits you review individually.

## Why it saves tokens

Input tokens scale with tool-call count: each turn re-sends the whole growing
context, so every extra tool call whose raw result sits in the transcript is
paid for again on every later turn. Collapsing N per-item calls into one script
call removes both the per-call results and the extra turns that carried them.
The rubric text itself is part of the stable system-prompt prefix, so on
providers with prefix caching (DeepSeek, Anthropic) its own cost is cached away
after the first turn — the savings come from the tool-result tokens it avoids.

## A/B measurement

`scripts/code-mode-ab.sh` measures the effect empirically. It runs one fixed
task headless N times with the rubric off (control) and N times with it on
(treatment), fully isolated (`DIRGE_CONFIG_DIR` / `DIRGE_DATA_DIR` per run, so
your real config and session history are never touched), and reports mean input
tokens and mean tool-call count per arm. Ground truth is computed from the
generated fixture, so the correctness gate can't drift from the harness.

It ships three scenarios (`-s`), chosen to bracket the effect from "nothing to
save" to "large win":

- **`small`** (default) — 30 `.log` files, "how many contain `FATAL`?". A single
  `grep` answers it; the naive path greps/reads a few files one at a time.
- **`large`** — 120 `.log` files, "which `ERROR` code is most frequent, and how
  many files have errors?". Still a one-line `grep | sort | uniq -c` pipeline.
- **`fanout`** — 120 `.log` files, each with a `STATUS=` line and, on a *different
  line*, a `REGION=` line; "how many files are both `STATUS=down` and
  `REGION=eu`?". No single-line `grep` can AND two markers across lines, so the
  model either scripts the intersection of two `grep -l` lists (one call, no file
  bodies in context) or opens each of the ~40 `STATUS=down` candidates to check
  its region — a real per-file fan-out.

Metrics: `cumulative_input_tokens` from the persisted session file (this is why
the headless usage-persistence fix was needed — see the changelog), and
tool-call count from the `--output-format stream-json` `tool_use` blocks. Each
run is gated on the correct answer, so a "saving" from the model doing less work
/ getting it wrong is caught.

Run it yourself (needs a built binary, `jq`, and a working provider in your
config):

```
cargo build --bin dirge
scripts/code-mode-ab.sh -s fanout -n 8 -p deepseek-flash
```

### Results

Measured on `deepseek-v4-flash`. The three scenarios bracket the effect:

| Scenario | n/arm | Arm | Mean input | Mean calls | Correct |
|----------|------:|-----|-----------:|-----------:|--------:|
| small  | 15 | control   | 103,106 | 2.7 | 14/15 |
| small  | 15 | treatment |  83,456 | 1.8 | 15/15 |
| large  | 10 | control   | 163,260 | 4.9 | 10/10 |
| large  | 10 | treatment | 159,120 | 5.5 | 10/10 |
| fanout |  8 | control   | 257,622 | 8.0 |  5/8  |
| fanout |  8 | treatment | 142,432 | 4.5 |  8/8  |

- **small:** −19% input tokens, 2.7 → 1.8 calls. A modest win — the model
  *sometimes* takes a multi-call path on an obvious task, and the rubric steers
  it to the one-`grep` path.
- **large:** essentially flat (−2.5% mean; the median is −8%, but mean tool calls
  tick *up*, dragged by one treatment run that looped). deepseek-flash already
  reaches for `grep` on an obviously-greppable directory, so there is almost no
  naive fan-out left for the rubric to remove.
- **fanout:** −44.7% input tokens, 8.0 → 4.5 calls — **and correctness rises from
  5/8 to 8/8.** Here the model genuinely fans out without the rubric (6–10 calls,
  118k–372k input tokens) and miscounts 3 of 8 runs while eyeballing 40 files;
  the rubric collapses that to one scripted intersection that is both cheaper and
  exact.

The through-line: **the rubric's payoff is proportional to how much naive
fan-out the model would do without it.** On a task the model already one-shots
(`large`), there is nothing to capture and the effect is ~0. On a task that
forces per-item work (`fanout`), the win is large *and* correctness improves,
because scripting the aggregate is less error-prone than manually inspecting many
items. The `small` case sits in between: a mild lure into multi-call behavior, a
mild win.

That is also why the flag is off by default and why enabling it is close to
free: worst case (already-greppable work) it is roughly neutral — the rubric text
itself lives in the cached system-prompt prefix — and best case (genuine fan-out)
it saves ~45% of input tokens and reduces mistakes. Turn it on for MCP-heavy or
bulk-scan workloads, measure on your own traffic with this harness, and decide.

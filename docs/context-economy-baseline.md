# Context-economy protocol and partial baseline: measuring the built-in fold

This document is the measurement protocol for any change to how dirge keeps a
long session inside its context budget: the fold ladder
(`src/agent/agent_loop/context_manager.rs`), the compaction pass
(`run_compaction_pass_with_focus`), the summary prompt
(`src/agent/compression.rs`), and anything that later replaces or augments
them (a different summarizer, eviction with recall handles, compression of tool
output on arrival).

It also records a **partial baseline**: what the built-in fold scored on the
runs collected so far. The collection is short of the sample size the protocol
requires (15 runs against 20 per arm, and no live recall probe), so these
numbers are provisional. They show the instrument works and what the built-in
fold looks like under it; they are not yet the baseline the decision rule
judges a candidate against. A candidate is judged against the completed
baseline, in the regime it claims to help, and never on token counts alone.

Every result below says which rung of evidence it stands on:

- **measured**: produced by the commands in this document, raw numbers given;
- **reproduced**: re-run and matched;
- **inferred**: reasoned from code or from other numbers, not observed.

## Instruments

There are two, because they answer different questions.

### I1. Single-call recall (does a fold keep the facts?)

`src/agent/compaction_recall.rs` plants facts in the region a fold removes and
scores verbatim survival after the summarizer runs. Two layers:

- **Deterministic, offline** (`cargo test compaction_recall`): every planted
  fact must reach the prompt handed to the summarizer. This is the part dirge
  controls (window selection, serialization, per-turn truncation). No model,
  no network. A candidate that changes the window or the serializer must keep
  these green.
- **Live** (`src/agent/compaction_bakeoff.rs`, off unless `DIRGE_BAKEOFF=1`):
  one summarizer call per repeat against a real model.
  - *hard recall*: 20 unannounced facts buried in noisy tool output; score is
    facts kept out of 20, plus the tail (runs losing 2 or more).
  - *tool-call probe*: 6 facts that exist only in tool-call arguments; scored
    both in the prompt (serializer) and in the summary (model).
  - *coverage probe*: the prompt is clipped in half; does the summary say its
    source was partial?

The live probe holds the transcript, window, budget and scorer byte-identical
across arms, so a difference is attributable to the summarizer path alone. It
does not see agent behaviour.

### I2. Long-horizon pass rate and cost-per-pass (does the session still work?)

`scripts/loop-ab.sh -s fold-chain` runs dirge headless on a task that cannot
be finished without surviving several folds:

- a pointer chain of 24 small files, each naming the next under an unguessable
  name, so the reads are forced to be sequential (one turn per file);
- each file carries one hash-derived `VALUE`; the answer is their sum, so every
  value read before a fold must survive it;
- nothing may be written (a run that parks values on disk bypasses the thing
  under test and fails);
- `context_target=26000` pins the budget identically for every arm; each file
  stays under the aggressive per-result cap so no value is lost to result
  truncation, only to a fold.

The existing `compact` scenario is not sufficient: a model that reads its eight
files in one parallel turn answers on turn two and never folds (measured: 2
turns, 0 compactions on the model below).

`scripts/fold-report.py` reads the kept run directories and reports per arm.

## Metrics

| Metric | Source | Direction |
|---|---|---|
| pass rate, Wilson 95% CI | `check_correct`: exact sum, tree untouched | higher |
| folds per run | `context compacted` log lines | mechanism gate (see below) |
| input / cached / output tokens, mean with 95% CI | session file counters | lower (cached: higher) |
| cost per run (token-priced) | tokens x published price | lower |
| **measured spend** | provider balance before and after the batch | lower |
| **cost per pass** | measured spend / passes | lower; the headline cost metric |
| re-reads per run | `read` calls on a path already read | lower; the visible cost of a fold that lost something |
| turns | gates tally | context only |
| recall (I1) | facts kept / 20, runs losing 2+ | higher / lower |

**Mechanism gate.** A run with zero folds did not exercise the fold. Such runs
are counted and reported, never averaged in as evidence about folding. If a
candidate changes how often the fold fires, report folds per run beside the
pass rate so a "win" by folding less is visible as that.

**Why measured spend, not token-priced cost.** The session counters record the
main loop's usage. The summarizer's side calls are not in them, so token-priced
cost undercounts. In the first two smoke runs the provider balance fell by
$0.080 while the counters priced the same runs at $0.041: about half the spend
of a folding session is invisible to the counters (measured). Cost per pass
uses the balance delta for that reason. When batches run concurrently the
delta covers all of them together; attribute it per arm only when arms run in
separate batches.

## Arms

- **builtin** (the baseline): current ladder, `SummarySchema::Sections`,
  default thresholds. Nothing overridden except `context_target`.
- **Later arms** plug in by configuration, not by code edits to the harness:
  `loop-ab.sh -A <control overrides> -B <treatment overrides> -C name:<overrides>`
  apply config keys per arm (for example a compaction hook or an addon switched
  on in the treatment only). For I1, a candidate summarizer is a different
  `SummarizeFn` handed to `run_hard_recall_eval_with`.

A flag that ships ON cannot be A/B'd by setting it in the treatment; put the
disable on the control (see the header of `loop-ab.sh`).

## Pairing, sample size, test

- **Pairing.** Arms run on the same pinned binary copy, the same byte-identical
  fixture, the same base config, and the same model. Runs are paired by
  (batch, repeat index). The provider does not honour a sampling seed, so
  "paired seeds" here means paired fixture and position, not identical
  sampling; the pairing removes fixture and drift variance, not model
  sampling variance.
- **Order.** `loop-ab.sh` runs every control repeat before any treatment
  repeat. Run at least two batches concurrently so arms overlap in time, and
  swap `-A`/`-B` between batches when the arms differ, so provider drift over
  the run cannot line up with the arm.
- **n = 20 per arm.** With 20 pairs, the exact sign test reaches p < 0.05 only
  when at least 6 discordant pairs all favour one arm; smaller effects are
  reported as ties, not as directions.
- **Test.** Pass rate: exact two-sided sign test on discordant pairs (McNemar
  exact). Tokens: paired mean delta with a t 95% CI. Cost per pass: reported,
  not tested (it is a ratio of totals).
- **A/A first.** Before trusting any A/B, the baseline is itself run as an A/A
  (two identical arms, which is what the commands below do). The split-half
  difference is the noise floor.
- **Decision rule.** A candidate **ties or beats** the baseline when its pass
  rate is not significantly worse (sign test) *and* its cost per pass is not
  higher, in the fold-chain regime. It **beats** the baseline when, in
  addition, either its pass rate is significantly better or its paired
  input-token delta CI lies wholly below zero. A tie goes to the simpler
  implementation. I1 recall must not regress (mean facts kept and runs losing
  2+ no worse), but a recall win alone is not a win.

## Running it

Build (any toolchain meeting the locked dependencies), then use a minimal base
config so personal MCP servers, hooks and provider routes stay out of every
arm:

```bash
cat > /tmp/base.json <<'EOF'
{
  "provider": "venice",
  "providers": {
    "venice": {
      "provider_type": "openai",
      "base_url": "https://api.venice.ai/api/v1",
      "api_key_env": "VENICE_API_KEY",
      "model": "deepseek-v4-flash",
      "context_window": 1000000
    }
  }
}
EOF

# I2: two concurrent batches, A/A of the baseline, 20 runs per arm
for b in 1 2; do
  LOOP_AB_KEEP=1 LOOP_AB_BASE_CONFIG=/tmp/base.json \
    scripts/loop-ab.sh -n 10 -s fold-chain -t 60 > batch-$b.txt 2>&1 &
done; wait

scripts/fold-report.py /tmp/loop-ab.<b1> /tmp/loop-ab.<b2> \
  --price-in 0.138 --price-cached 0.028 --price-out 0.275 \
  --pool builtin --compare control treatment

# I1: live recall, 10 calls per schema arm
cargo test compaction_recall                     # offline layer
DIRGE_BAKEOFF=1 DIRGE_BAKEOFF_PROVIDER=venice \
  DIRGE_BAKEOFF_BASE_URL=https://api.venice.ai/api/v1 \
  DIRGE_BAKEOFF_API_KEY_ENV=VENICE_API_KEY \
  DIRGE_BAKEOFF_MODEL=deepseek-v4-flash DIRGE_BAKEOFF_REPEATS=10 \
  cargo test compaction_bakeoff -- --nocapture --test-threads=1
```

For measured spend, poll the provider balance every 20 s during each batch and
sum the drops between consecutive samples. Do not subtract the last sample from
the first: a top-up during the batch would then read as negative spend.

## Partial baseline results

**Status: partial and provisional, 15 runs of the 40 the protocol requires.**
I2 has 15 valid runs (an A/A split of 10 control and 5 treatment, both arms the
unmodified builtin) against n = 20 per arm. The remaining runs and the I1 live
recall probe are still open; see "What is still open" below. The collection
used `-n 5` per batch, half of what "Running it" now specifies.

Until the collection is complete, the decision rule does not apply to these
numbers: no candidate ties or beats the baseline on the strength of them. Five
pairs cannot reach p < 0.05 on the exact sign test whatever the outcome. Treat
the A/A comparison as a rough noise floor only: one half has five runs.

### I2, fold-chain, builtin arm

deepseek-v4-flash via Venice, `context_target=26000`, debug build of
`economy-p0` @ 914b09de, base config as in "Running it", `-n 5 -s fold-chain
-t 60`, two concurrent batches (measured; `scripts/fold-report.py
/tmp/loop-ab.yjdCaH /tmp/loop-ab.cO6a0U --price-in 0.138 --price-cached 0.028
--price-out 0.275 --pool builtin --compare control treatment`):

| Metric | Value |
|---|---|
| runs | 15 (every run folded: 15/15) |
| pass | 13/15 = 0.87, Wilson 95% [0.62, 0.96] |
| passes without a bypass | 6/15 = 0.40, Wilson 95% [0.20, 0.64] |
| runs that bypassed (shell, grep or find outside the reads under test) | 7/15 |
| halted by the ladder | 0/15 (other failures 2) |
| folds per run | 3.53 [3.03, 4.04] |
| re-reads per run | 24.7 [17.4, 32.0] |
| turns | 30.0 [27.4, 32.6] |
| input tokens per run | 520k [474k, 565k] (cached 410k) |
| output tokens per run | 8.4k [7.1k, 9.7k] |
| cost per run (token-priced) | $0.0290 [0.0258, 0.0322] |
| cost per run (balance-measured) | at most $0.0367 |
| cost per pass (token-priced) | $0.0334 |
| cost per pass (balance-measured) | at most $0.0423 |

**Measured spend.** The Venice balance was polled every 20 s during the
batches (`GET /api/v1/api_keys/rate_limits`, `data.balances.USD`). Spend is the
sum of the drops between consecutive samples, never the first sample minus the
last: the account was topped up by about $100 at 13:57, mid-batch, and a
start-minus-end delta would have read that as negative spend. The two batches
spent $0.5502 between 13:41 and 15:18. That figure also covers two runs that
were killed mid-flight when the session was interrupted (see below), so
$0.5502 / 15 = $0.0367 per completed run is an upper bound. The session
counters price the same 15 runs at $0.4347, so they undercount a folding
session by at most 1.27x here (earlier smokes said 1.4x to 2x; the
summarizer's side calls are the part the counters miss).

### A/A noise floor (control vs treatment, identical configuration)

| Metric | control (n = 10) | treatment (n = 5) | difference |
|---|---|---|---|
| pass | 8/10 [0.49, 0.94] | 5/5 [0.57, 1.00] | 0.20 |
| passes without a bypass | 4/10 | 2/5 | 0.00 |
| re-reads per run | 27.4 [16.5, 38.3] | 19.2 [13.0, 25.4] | 8.2 |
| folds per run | 3.50 | 3.60 | 0.10 |
| turns | 29.9 | 30.2 | 0.3 |
| input tokens per run | 520k | 518k | 2k |
| cost per run (token-priced) | $0.0288 | $0.0294 | $0.0006 |

Paired by (batch, repeat): 5 pairs, discordant 0 control-only vs 1
treatment-only pass, exact sign test p = 1.0; paired input-token delta +1.5k
[-195k, +198k]. Two identical arms differ by 0.20 in pass rate and by 8 re-reads
per run at this sample size, so a candidate must beat those gaps before either
metric can be read as an effect. Token means agree to within 0.5%, but the
per-run spread is wide (the paired CI spans about +-200k), so a token claim
needs many more pairs than five.

Observation (measured, n = 15): every run folded 3 to 4 times, and after a fold
the model goes back for pages it had already read (about 25 re-reads per run on
a 24-file chain). Nearly half the runs (7/15) also escaped to a shell or grep
traversal of the chain; only 6/15 passed while relying on the fold alone. The
raw pass rate (0.87) is inflated by those escapes. Pass-without-bypass and
re-reads per run are the numbers a fold replacement has to move.

### How this collection ran, and why it stopped at 15

- Two concurrent batches (`/tmp/loop-ab.yjdCaH`, `/tmp/loop-ab.cO6a0U`) were
  started at 13:41. The session running them was killed at about 15:18, after
  batch 1 had finished control 5/5 and treatment 3/5, and batch 2 control 5/5
  and treatment 2/5. The two runs in flight (batch 1 treatment 4, batch 2
  treatment 3) were discarded; their spend is inside the $0.5502 above.
- A resume attempt ran a third batch (`/tmp/loop-ab.Ll5qUA`) to supply the
  missing 5 treatment runs. `loop-ab.sh` cannot run one arm alone, so the
  control arm was disabled on purpose with
  `-A providers.venice.base_url=http://127.0.0.1:9/v1`: dirge rejects the
  insecure URL at config load and exits before any request, so those five
  control rows read turns=0, no gates line, no session file and cost nothing.
  They are not runs and are excluded. The resume was then stopped by request
  while its first treatment run was in flight; that run was discarded too. The
  balance fell $0.0054 while no batch was running (15:18 to 15:32, most likely
  late billing of the killed runs) and $0.0072 during the resume, so total
  measured spend for the whole collection is $0.5628.
- Earlier partial collections (n = 7, then n = 4, on earlier builds of this
  protocol) are superseded by this one and are not pooled into it.

### I1 recall

Offline layer (`cargo test compaction_recall`): 12/12 pass (measured).

Live layer: not run. At REPEATS=10 the `compaction_bakeoff` filter runs three
tests (coverage probe over three schema arms, tool-call probe over one, hard
recall over three), about 70 summarizer calls.

### What is still open

- 10 more control and 15 more treatment runs, to bring I2 to n = 20 per arm;
- the I1 live bakeoff (`DIRGE_BAKEOFF=1 ... DIRGE_BAKEOFF_REPEATS=10`, the
  command under "Running it").

When they are run, record the balance every 20 s and sum the drops, as above.

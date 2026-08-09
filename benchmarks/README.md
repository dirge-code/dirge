# benchmarks

An Aider Polyglot driver for dirge (dirge-4ga8).

## Why

dirge ships roughly fifty behavioural guards — storm breaker, failure tracker,
progress monitor, capability tier, claim/source/completeness gates, safe-state,
reflexion, verifier — all tuned by hand against reasoning and anecdote. Until
now nothing measured whether any of them help, and nothing would notice when one
regresses.

Polyglot is the cheapest useful starting point: 225 self-contained exercises
across six languages, each with a spec, a stub, and a real test suite, needing no
infrastructure beyond the language toolchains.

## Running

```bash
git clone https://github.com/Aider-AI/polyglot-benchmark /tmp/polyglot
cargo build --release
python3 benchmarks/polyglot.py \
  --exercises /tmp/polyglot \
  --dirge-bin target/release/dirge \
  --out polyglot-results.json
```

A full run is 225 exercises × up to 2 model calls, so start small:

```bash
# one language, five exercises, to check the plumbing
python3 benchmarks/polyglot.py --exercises /tmp/polyglot \
  --languages python --limit 5 --dirge-bin target/release/dirge
```

Watch a run in progress (results are rewritten atomically after every exercise,
so this is safe at any point, including against a killed run):

```bash
python3 benchmarks/status.py polyglot-results.json
```

## Protocol

Matches upstream so the numbers are comparable:

1. copy the exercise to a scratch directory — the source tree is never mutated,
   so a run is repeatable and a crash leaves nothing behind;
2. strip skip markers from the test files (Exercism disables all but the first
   test; left in place, an agent "passes" by satisfying one assertion);
3. run the agent with the instructions and the list of files it may edit;
4. restore the test files from a snapshot, then run the suite;
5. if it failed, run the agent once more with the test output appended and score
   that.

The agent is driven through `--print --output-format stream-json`, so this
exercises the same headless path `--loop` and the MCP server use.

`status.py` reports the first-attempt rate separately. That is the more honest
signal for scaffold work — the retry hands the model its own test output, which
papers over a bad first move.

## Tests

```bash
python3 -m pytest benchmarks/test_polyglot.py
```

These cover the paths that can be wrong *silently*: the skip-marker transforms,
the pass/fail classification (`npm test` exits 0 when the script is missing;
gradle can exit 0 on a failed build), reference-solution exclusion, test-file
restore, and harness-error propagation. A benchmark that reports a plausible
wrong number is worse than one that refuses to report at all, so those get
end-to-end coverage against a stub agent — no model, no network, no toolchain.

## Status

The driver and its tests are written and green. **It has not yet been run
against a real clone of the benchmark**, so treat the per-language details as
unverified until a first run happens:

- The C++ command builds via CMake and relies on the Exercism `CMakeLists.txt`
  running the test binary as part of the build, with `EXERCISM_RUN_ALL_TESTS=1`.
  If a given exercise registers tests with `ctest` instead, that needs a second
  command.
- The Java command assumes a `./gradlew` wrapper in the exercise directory.
- Solution/test glob pairs are from the upstream track layouts and may need
  per-exercise exceptions.

Each of these fails loudly as a `harness_error` rather than as a miss, so a first
run surfaces them in `status.py` instead of quietly deflating the score.

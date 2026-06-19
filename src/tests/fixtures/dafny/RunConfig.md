# `RunConfig.dfy` — run configuration (module `RunConfig`)

Optional configuration for the predicate/method run methods in [`DafnyCheck.dfy`](DafnyCheck.md).
Start from `DefaultConfig()` and override individual fields with datatype-update syntax.

```dafny
datatype Verbosity = Off | Low | Medium | High

datatype RunConfig<!T> = RunConfig(
  numRuns: nat,                    // generated examples to attempt
  seed: Option<bv64>,              // RNG seed; None => library default (42)
  examples: seq<T>,                // concrete inputs always tested before random generation
  classifier: Option<T -> string>, // optional bucket labeller for distribution statistics
  useColor: bool,                  // ANSI color in reporting
  verbosity: Verbosity)            // Off | Low (default) | Medium | High

function DefaultConfig<T>(): RunConfig<T>
  // RunConfig(numRuns := 100, seed := None, examples := [], classifier := None,
  //           useColor := true, verbosity := Low)

predicate AtLeast(v: Verbosity, threshold: Verbosity)   // verbosity ordering used by reporting
```

Verbosity levels: `Off` prints nothing, `Low` prints the pass/fail summary + counterexamples,
`Medium` adds classification statistics, `High` adds shrink steps.

```dafny
var cfg := DefaultConfig<int>()
  .(seed := Some(7), numRuns := 200, verbosity := Medium,
    examples := [0, 99],
    classifier := Some((n: int) => if n < 50 then "low" else "high"));
var ok := RunTestWithConfig((n: int) => 0 <= n < 100, arb, "in range", cfg);
```

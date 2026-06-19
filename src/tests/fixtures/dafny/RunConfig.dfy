module RunConfigs {
  import opened Std.Wrappers

  // Verbosity levels for test run reporting.
  //   Off    - print nothing
  //   Low    - (default) pass/fail summary + counterexamples
  //   Medium - + classification statistics
  //   High   - + shrink steps and extra detail
  datatype Verbosity = Off | Low | Medium | High

  function VerbosityRank(v: Verbosity): nat {
    match v
    case Off => 0
    case Low => 1
    case Medium => 2
    case High => 3
  }

  // True when `v` is at least as verbose as `threshold`.
  predicate AtLeast(v: Verbosity, threshold: Verbosity) {
    VerbosityRank(v) >= VerbosityRank(threshold)
  }

  // Optional configuration for a test run. Every field has a sensible default
  // via DefaultConfig(); callers override individual fields with datatype-update
  // syntax, e.g. DefaultConfig().(seed := Some(7), verbosity := High).
  //
  //   numRuns    - number of generated examples to attempt
  //   seed       - RNG seed; None uses the library default (42)
  //   examples   - concrete inputs always tested before random generation
  //   classifier - optional T -> bucket-label function for distribution stats
  //   useColor   - whether reporting emits ANSI color escapes
  //   verbosity  - how much the run prints (see Verbosity)
  datatype RunConfig<!T> = RunConfig(
    numRuns: nat,
    seed: Option<bv64>,
    examples: seq<T>,
    classifier: Option<T -> string>,
    useColor: bool,
    verbosity: Verbosity
  )

  function DefaultConfig<T>(): RunConfig<T> {
    RunConfig(100, None, [], None, true, Low)
  }
}

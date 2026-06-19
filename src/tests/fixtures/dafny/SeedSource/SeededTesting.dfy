include "../DafnyCheck.dfy"
include "./SeedSource.dfy"

// ============================================================================
// SeededTesting — property-based test runners seeded from a *fresh* platform
// random value each run, via SeedSource.GetSeed().
//
// This is deliberately a SEPARATE module rather than baked into DafnyCheck:
// GetSeed is an `{:extern}`, so any module that references it forces the native
// SeedSource file (SeedSource.cs / .py / …) to be supplied at compile time for
// every consumer. Keeping it here means only tests that *opt in* to random
// seeding need a native file; plain DafnyCheck.RunTest stays extern-free and the
// rest of the suite compiles/runs with no extra inputs.
//
// Compile/run a test that uses these with the matching native file, e.g.
//   dafny test --target:cs MyTest.dfy --input src/SeedSource/SeedSource.cs
// ============================================================================
module SeededTesting {
  import opened DafnyCheck
  import opened Arbitraries
  import opened RunConfigs
  import opened Std.Wrappers
  import opened SeedSource

  // Like RunTest, but draws a fresh random seed for this run.
  method RunTestRandom<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string)
    returns (passed: bool)
    requires arb.Valid()
  {
    var seed := GetSeed();
    passed := RunTestWithConfig(pred, arb, name, DefaultConfig<T>().(seed := Some(seed)));
  }

  // Like RunTestWithExamples, but with a fresh random seed.
  method RunTestRandomWithExamples<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string, examples: nat)
    returns (passed: bool)
    requires arb.Valid()
    requires 0 < examples
  {
    var seed := GetSeed();
    passed := RunTestWithConfig(pred, arb, name,
                                DefaultConfig<T>().(seed := Some(seed), numRuns := examples));
  }

  // Like RunTestWithConfig, but if the config leaves the seed unset (None), fill
  // it with a fresh random seed; an explicit seed is respected (reproducible).
  method RunTestRandomWithConfig<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string, cfg: RunConfig<T>)
    returns (passed: bool)
    requires arb.Valid()
  {
    var cfg' := cfg;
    if cfg.seed.None? {
      var seed := GetSeed();
      cfg' := cfg.(seed := Some(seed));
    }
    passed := RunTestWithConfig(pred, arb, name, cfg');
  }

  // ──────────────────────────────────────────────────────────────────────────
  // Seeded *method*-test runners — siblings of the predicate runners above, for
  // testing heap-mutating methods wrapped in a MethodUnderTest. Each draws a
  // fresh seed from SeedSource.GetSeed() and delegates to RunMethodTestWithConfig.
  // ──────────────────────────────────────────────────────────────────────────

  // Like RunMethodTest, but draws a fresh random seed for this run.
  method RunMethodTestRandom<Input(!new), E(==)>(arb: Arbitrary<Input>, sut: MethodUnderTest<Input, E>, name: string)
    returns (passed: bool)
    requires arb.Valid()
    requires sut.Valid()
  {
    var seed := GetSeed();
    passed := RunMethodTestWithConfig(arb, sut, name, DefaultConfig<Input>().(seed := Some(seed)));
  }

  // Like RunMethodTestWithExamples, but with a fresh random seed.
  method RunMethodTestRandomWithExamples<Input(!new), E(==)>(
      arb: Arbitrary<Input>, sut: MethodUnderTest<Input, E>, name: string, examples: nat)
    returns (passed: bool)
    requires arb.Valid()
    requires sut.Valid()
    requires 0 < examples
  {
    var seed := GetSeed();
    passed := RunMethodTestWithConfig(arb, sut, name,
                                      DefaultConfig<Input>().(seed := Some(seed), numRuns := examples));
  }

  // Like RunMethodTestWithConfig, but if the config leaves the seed unset (None),
  // fill it with a fresh random seed; an explicit seed is respected (reproducible).
  method RunMethodTestRandomWithConfig<Input(!new), E(==)>(
      arb: Arbitrary<Input>, sut: MethodUnderTest<Input, E>, name: string, cfg: RunConfig<Input>)
    returns (passed: bool)
    requires arb.Valid()
    requires sut.Valid()
  {
    var cfg' := cfg;
    if cfg.seed.None? {
      var seed := GetSeed();
      cfg' := cfg.(seed := Some(seed));
    }
    passed := RunMethodTestWithConfig(arb, sut, name, cfg');
  }
}

# `DafnyCheck.dfy` — run methods + engine (module `DafnyCheck`)

The top-level entry points that drive generation, shrinking, and reporting. Every run method
returns `bool` — `true` iff every always-tested example and every generated case passed.

## Predicate tests

```dafny
method RunTest<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string) returns (passed: bool)
  requires arb.Valid()

method RunTestWithExamples<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string, examples: nat)
  returns (passed: bool)
  requires arb.Valid() && 0 < examples

method RunTestWithConfig<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string, cfg: RunConfig<T>)
  returns (passed: bool)
  requires arb.Valid()
```

`RunTest`/`RunTestWithExamples` are thin delegators over `RunTestWithConfig`. The config-driven
form (1) evaluates `pred` directly on each `cfg.examples` value, (2) runs randomized generation
+ shrinking honoring `numRuns`/`seed`/`classifier`, (3) reports via [`Reporting`](Reporting.md).
See [`RunConfig.md`](RunConfig.md) for the config fields.

## Method-under-test tests

For testing heap-mutating methods: wrap the method in a `MethodUnderTest` subclass.

```dafny
trait MethodUnderTest<Input(!new), E(==)> {
  ghost predicate Valid() reads this
  method run(input: Input) returns (result: Result<bool, E>)   // Success(true)=pass, Success(false)=fail, Failure(e)=errored
    requires Valid() ensures Valid()
}

method RunMethodTest<Input(!new), E(==)>(arb: Arbitrary<Input>, sut: MethodUnderTest<Input, E>, name: string)
  returns (passed: bool)
  requires arb.Valid() && sut.Valid()

method RunMethodTestWithExamples<Input(!new), E(==)>(arb, sut, name, examples: nat) returns (passed: bool)
method RunMethodTestWithConfig<Input(!new), E(==)>(arb, sut, name, cfg: RunConfig<Input>) returns (passed: bool)
```

## Internals (not usually called directly)

- `class TestingState<T(!new)>` — the generation + shrinking engine (`Run()` → `Generate()` +
  `Shrink()`). Tracks the run count, RNG, classifier statistics, and the minimised counterexample.
- `trait TestFunction<T(!new)>` — the side-effecting test body driven per `TestCase`; implemented by
  `PredicateTest<T>` and `MethodTest<Input, E>`.
- `trait RandomGen` / `class SimpleRandomGen` — RNG wrapper (`SimpleRandomGen(seed: bv64)`).

For the generators you pass in, see [`Arbitrary.md`](Arbitrary.md); for stateful/model-based
testing, see [`Stateful.md`](Stateful.md).

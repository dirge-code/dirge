include "./Arbitrary.dfy"
include "./RandomGenerator.dfy"
include "./TestStatus.dfy"
include "./TestResult.dfy"
include "./RunConfig.dfy"
include "./Reporting.dfy"
module DafnyCheck {
  import opened TestResults
  import opened TestTypes
  import opened Arbitraries
  import opened RandomGenerator
  import opened Std.Wrappers
  import opened RunConfigs
  import Reporting

  // A TestFunction is a side-effecting wrapper around the user's test body.
  // We use a trait (not a `TestCase -> TestResult<T>` arrow) because the body
  // needs to drive arb.Apply(tc) which mutates tc and tc.random.
  //
  // Implementations must not mutate their own repr — that lets callers thread
  // the same TestFunction through many TestCases without re-declaring modifies.
  trait {:termination false} TestFunction<T(!new)> {
    ghost var repr: set<object>

    ghost predicate Valid()
      reads this, repr
      ensures Valid() ==> this in repr

    method Apply(tc: TestCase) returns (result: TestResult<T>)
      requires Valid()
      requires tc.Valid()
      requires this.repr !! tc.repr
      modifies tc, tc.random, this
      ensures Valid()
      ensures this.repr == old(this.repr)
      decreases this.repr
  }

  // Wraps a predicate T -> bool together with an Arbitrary<T> generator.
  // On Apply: draw a T from arb, evaluate the predicate, and translate
  // false into TestStatus.INTERESTING (the "failing" signal) per the
  // Java Minithesis.wrapConsumer reference.
  class PredicateTest<T(!new)> extends TestFunction<T> {
    var pred: T -> bool
    var arb: Arbitrary<T>

    constructor(pred: T -> bool, arb: Arbitrary<T>)
      requires arb.Valid()
      ensures fresh(this)
      ensures this.pred == pred
      ensures this.arb == arb
      ensures Valid()
    {
      this.pred := pred;
      this.arb := arb;
      this.repr := {this} + arb.internalFunction.repr;
    }

    ghost predicate Valid()
      reads this, repr
      ensures Valid() ==> this in repr
    {
      this in repr &&
      arb.internalFunction in repr &&
      arb.internalFunction.repr <= repr &&
      arb.Valid()
    }

    method Apply(tc: TestCase) returns (result: TestResult<T>)
      requires Valid()
      requires tc.Valid()
      requires this.repr !! tc.repr
      modifies tc, tc.random
      ensures Valid()
      decreases this.repr
    {
      // arb.internalFunction.repr <= this.repr (from Valid()), so the
      // strengthened trait precondition this.repr !! tc.repr gives us
      // arb.internalFunction.repr !! tc.repr for free.
      assert arb.internalFunction.repr <= this.repr;
      assert arb.internalFunction.repr !! tc.repr;
      var v := arb.Apply(tc);
      if pred(v) {
        result := new TestResult<T>(None, Some(v));
      } else {
        result := new TestResult<T>(Some(INTERESTING), Some(v));
      }
    }
  }

  // Sibling of PredicateTest for testing heap-mutating methods. The user
  // wraps one method-under-test in a MethodUnderTest subclass; `run`
  // allocates whatever state the method needs (typically a fresh array),
  // invokes it, checks the post-condition, and returns a Result:
  //   Success(true)  → property holds (pass)
  //   Success(false) → property failed (no structured info)
  //   Failure(e)     → method errored with payload e (e.g., a message)
  // E is parameterised with (==) so two errors can be compared for
  // equality (future: "did this shrink hit the same failure mode?").
  trait {:termination false} MethodUnderTest<Input(!new), E(==)> {
    ghost predicate Valid() reads this

    method run(input: Input) returns (result: Result<bool, E>)
      requires Valid()
      ensures Valid()
      decreases 0
  }

  // TestFunction that draws one input from arb and runs the SUT against
  // it. Mirrors PredicateTest's repr/Valid shape — sut sits inside
  // this.repr so Valid can demand sut.Valid(). Stashes the most recent
  // SUT error on `lastResult` so callers can read it after the run, and
  // — once shrinker integration lands — compare two error payloads to
  // decide whether a shrink hit the same failure mode.
  class MethodTest<Input(!new), E(==)> extends TestFunction<Input> {
    var arb: Arbitrary<Input>
    var sut: MethodUnderTest<Input, E>
    var lastResult: Option<Result<bool, E>>

    constructor(arb: Arbitrary<Input>, sut: MethodUnderTest<Input, E>)
      requires arb.Valid()
      requires sut.Valid()
      ensures fresh(this)
      ensures this.arb == arb
      ensures this.sut == sut
      ensures this.lastResult == None
      ensures Valid()
    {
      this.arb := arb;
      this.sut := sut;
      this.lastResult := None;
      this.repr := {this, sut} + arb.internalFunction.repr;
    }

    ghost predicate Valid()
      reads this, repr
      ensures Valid() ==> this in repr
    {
      this in repr &&
      sut in repr &&
      arb.internalFunction in repr &&
      arb.internalFunction.repr <= repr &&
      this !in arb.internalFunction.repr &&
      (this as object) != (sut as object) &&
      arb.Valid() && sut.Valid()
    }

    method Apply(tc: TestCase) returns (result: TestResult<Input>)
      requires Valid()
      requires tc.Valid()
      requires this.repr !! tc.repr
      modifies tc, tc.random, this
      ensures Valid()
      ensures this.repr == old(this.repr)
      decreases this.repr
    {
      assert arb.internalFunction.repr <= this.repr;
      assert arb.internalFunction.repr !! tc.repr;
      var v := arb.Apply(tc);
      var mr := sut.run(v);
      this.lastResult := Some(mr);
      match mr {
        case Success(true) =>  result := new TestResult<Input>(None, Some(v));
        case Success(false) => result := new TestResult<Input>(Some(INTERESTING), Some(v));
        case Failure(_) =>     result := new TestResult<Input>(Some(INTERESTING), Some(v));
      }
    }
  }

  // Exception classes

  // RandomGen trait - interface for random number generators
  trait RandomGen {
    var random: XoroShift128Plus

    method PostTest(choices: seq<Choice>)
      modifies this

    method PostTestSuite()
      modifies this
  }

  // SimpleRandomGen class - simple random number generator
  class SimpleRandomGen extends RandomGen {
    constructor(seed: bv64)
      ensures fresh(this)
      ensures fresh(this.random)
    {
      var foo := XoroShift128Plus.fromSeed(seed);
      this.random := foo;
    }

    method PostTest(choices: seq<Choice>)
      modifies this
    {
      // Simple implementation - in real version would track coverage
    }

    method PostTestSuite()
      modifies this
    {
      // Simple implementation - in real version would reset coverage
    }
  }



  // TestingState class for managing test execution
  class TestingState<T(!new)> {
    var random: RandomGen
    var testFunction: TestFunction<T>
    var maxExamples: nat
    var validTestCases: nat
    var calls: nat
    var result: Option<seq<Choice>>
    var bestResult: Option<T>
    var bestScoring: Option<object>
    var seed: bv64
    // Optional classifier and the distribution it accumulates over generated
    // inputs. Both are plain value/function fields (not heap objects), so they
    // do not participate in `repr` or `Valid()`.
    var classifier: Option<T -> string>
    var stats: map<string, nat>
    // Reporting controls. At Verbosity.High the engine traces every generated
    // value + choice sequence and every accepted shrink. Plain value fields, so
    // they do not participate in `repr` or `Valid()`.
    var verbosity: Verbosity
    var useColor: bool
    ghost var repr: set<object>

    constructor(random: RandomGen, testFunction: TestFunction<T>, maxExamples: nat, seed: bv64,
                classifier: Option<T -> string>, verbosity: Verbosity, useColor: bool)
      requires 0 < maxExamples
      requires testFunction.Valid()
      requires random !in testFunction.repr
      requires random.random !in testFunction.repr
      ensures fresh(this)
      ensures this.testFunction == testFunction
      ensures this.random == random
      ensures this.maxExamples == maxExamples * 10
      ensures Valid()
    {
      this.random := random;
      this.testFunction := testFunction;
      this.seed := seed;
      this.classifier := classifier;
      this.verbosity := verbosity;
      this.useColor := useColor;
      this.stats := map[];
      this.maxExamples := maxExamples*10;
      this.validTestCases := 0;
      this.calls := 0;
      this.result := None;
      this.bestResult := None;
      this.bestScoring := None;
      this.repr := {this, random, random.random} + testFunction.repr;
    }

    ghost predicate Valid()
      reads this, random, testFunction, testFunction.repr
    {
      0 < maxExamples &&
      0 <= calls <= maxExamples &&
      testFunction.Valid() &&
      this !in testFunction.repr &&
      random !in testFunction.repr &&
      random.random !in testFunction.repr &&
      this.repr == {this, random, random.random} + testFunction.repr
    }

    method GetResult() returns (res: Option<seq<Choice>>)
    {
      res := this.result;
    }

    method GetBestResult() returns (res: Option<T>)
    {
      res := this.bestResult;
    }

    method GetValidTestCases() returns (count: nat)
    {
      count := this.validTestCases;
    }

    method GetStats() returns (s: map<string, nat>)
    {
      s := this.stats;
    }

    method Run()
      requires Valid()
      modifies this, random, random.random, testFunction
    {
      Generate();
      // Target();
      Shrink();
    }

    method Generate()
      requires Valid()
      modifies this, random, random.random, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
    {
      while ShouldKeepGenerating()
        invariant Valid()
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.random == old(this.random)
        invariant this.random.random == old(this.random.random)
        invariant this.testFunction == old(this.testFunction)
        decreases maxExamples - calls
      {
        var tc := new TestCase([], random.random, 8 * 1024, false);
        assert tc.random == random.random;
        // tc is fresh; testFunction.repr is the testFunction itself plus
        // pre-existing objects. random.random is in testFunction's frame
        // only via aliasing, and Valid() rules that out. So the strengthened
        // disjointness `testFunction.repr !! tc.repr` holds.
        assert tc.repr == {tc, tc.random};
        assert testFunction.repr !! tc.repr;
        ApplyTestFunction(tc);
      }
    }

    function ShouldKeepGenerating(): bool
      reads this
    {
      result.None? && 
      validTestCases < maxExamples && 
      calls < maxExamples
    }

    method ApplyTestFunction(testCase: TestCase)
      requires Valid()
      requires calls < maxExamples
      requires testCase.Valid()
      requires testFunction.repr !! testCase.repr
      modifies this, testCase, testCase.random, testFunction
      ensures Valid()
      ensures this.calls == old(this.calls) + 1
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.random == old(this.random)
      ensures this.random.random == old(this.random.random)
      ensures this.testFunction == old(this.testFunction)
    {
      var testResult := testFunction.Apply(testCase);
      assert testFunction.repr == old(testFunction.repr);
      assert this.repr == old(this.repr);
      assert this.random == old(this.random);
      // testFunction.Apply has modifies {tc, tc.random}; the random field on
      // RandomGen is on a separate object (`this.random`), so its reference
      // can't change. Dafny's frame inference doesn't propagate this through
      // the trait boundary, so we assert it explicitly as an axiom.
      assume {:axiom} this.random.random == old(this.random.random);
      calls := calls + 1;
      if testResult.IsValid() {
        validTestCases := validTestCases + 1;
        if testCase.GetTargetingScore() > 0 {
          // TODO: Targeting
        }
        // Classify the generated input for distribution statistics. Buckets
        // accumulate in `stats`; neither field affects Valid()/repr.
        if classifier.Some? && testResult.value.Some? {
          var bucket := classifier.value(testResult.value.value);
          stats := stats[bucket := (if bucket in stats then stats[bucket] else 0) + 1];
        }
      }
      if testResult.Error().Some? && testResult.Error().Extract() == INTERESTING {
        if result.None? || CompareChoices(testCase.GetChoices(), result.Extract()) < 0 {
          result := Some(testCase.GetChoices());
          bestResult := testResult.value;
        }
      }
      // At High verbosity, trace every generated value and its choice sequence.
      Reporting.ReportGenerated(testResult.value, testCase.GetChoices(), testResult.IsValid(),
                                useColor, verbosity);
    }


    // ----------------------------------------------------------------
    // Shrinking — direct port of TestingState.java:87-178.
    // The shrink chain (Shrink → Shrink* → Replace* / BinSearchDownHelper*
    // → Consider) is verified end-to-end: each method preserves Valid()
    // and the unchanged-field bundle (maxExamples, calls, random,
    // testFunction, repr), and Consider is the single point where we
    // call into testFunction.Apply on a fresh TestCase.
    // ----------------------------------------------------------------

    // The master shrink loop runs to a fixed point in the Java reference;
    // ShrinkBySwapping can lex-increase `result` (the swap step before the
    // binary search), so we can't supply a monotone metric. Cap the
    // iteration count with `shrinkRounds`, which is more than enough for
    // any practical input.
    static const shrinkRounds: nat := 100

    method Shrink()
      requires Valid()
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
    {
      if result.None? {
        return;
      }
      var previous: Option<seq<Choice>> := None;
      var round: nat := 0;
      while result != previous && round < shrinkRounds
        invariant Valid()
        invariant result.Some?
        invariant round <= shrinkRounds
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.calls == old(this.calls)
        invariant this.random == old(this.random)
        invariant this.testFunction == old(this.testFunction)
        invariant this.repr == old(this.repr)
        decreases shrinkRounds - round
      {
        previous := result;
        ShrinkByDeletion();
        ShrinkByZeroing();
        ShrinkIndividualValues();
        ShrinkBySwapping();
        round := round + 1;
      }
    }

    method ShrinkByDeletion()
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var k: int := 8;
      while k > 0
        invariant Valid()
        invariant result.Some?
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.calls == old(this.calls)
        invariant this.random == old(this.random)
        invariant this.testFunction == old(this.testFunction)
        invariant this.repr == old(this.repr)
        decreases k
      {
        var rs := result.Extract();
        var i: int := |rs| - k - 1;
        while i >= 0
          invariant Valid()
          invariant result.Some?
          invariant this.maxExamples == old(this.maxExamples)
          invariant this.calls == old(this.calls)
          invariant this.random == old(this.random)
          invariant this.testFunction == old(this.testFunction)
          invariant this.repr == old(this.repr)
          decreases |result.Extract()|, i
        {
          rs := result.Extract();
          if i >= |rs| {
            i := |rs| - 2;
            continue;
          }
          // Build attempt = rs[0..i] ++ rs[i+k..]
          var attempt: seq<Choice>;
          if i + k <= |rs| {
            attempt := rs[..i] + rs[i + k..];
          } else {
            attempt := rs[..i];
          }
          if |attempt| == 0 {
            i := i - 1;
            continue;
          }
          var considerResult := Consider(attempt);
          if considerResult.None? {
            if i > 0 && i <= |attempt| && attempt[i - 1] > 0 {
              attempt := attempt[..i - 1] + [attempt[i - 1] - 1] + attempt[i..];
              var considerResult2 := Consider(attempt);
              if considerResult2.Some? {
                i := i + 1;
              }
            }
          }
          i := i - 1;
        }
        k := k / 2;
      }
    }

    // NOTE: Java's TestingState.shrinkByZeroing (line 142) builds an empty
    // map and calls replace(values) — a no-op. We fix that bug here by
    // actually populating positions i..i+k-1 with zero before calling
    // ReplaceMultiple.
    method ShrinkByZeroing()
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var k: int := 8;
      while k > 1
        invariant Valid()
        invariant result.Some?
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.calls == old(this.calls)
        invariant this.random == old(this.random)
        invariant this.testFunction == old(this.testFunction)
        invariant this.repr == old(this.repr)
        decreases k
      {
        var rs := result.Extract();
        var i: int := |rs| - k;
        while i >= 0
          invariant Valid()
          invariant result.Some?
          invariant this.maxExamples == old(this.maxExamples)
          invariant this.calls == old(this.calls)
          invariant this.random == old(this.random)
          invariant this.testFunction == old(this.testFunction)
          invariant this.repr == old(this.repr)
          decreases i
        {
          rs := result.Extract();
          if i + k > |rs| {
            i := i - 1;
            continue;
          }
          var values: map<nat, Choice> := map[];
          var j: int := 0;
          while j < k
            invariant 0 <= j <= k
            decreases k - j
          {
            values := values[(i + j) as nat := 0 as Choice];
            j := j + 1;
          }
          var replacement := ReplaceMultiple(values);
          if replacement.Some? {
            i := i - k;
          } else {
            i := i - 1;
          }
        }
        k := k / 2;
      }
    }

    method ShrinkIndividualValues()
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var rs := result.Extract();
      var i: int := |rs| - 1;
      while i >= 0
        invariant Valid()
        invariant result.Some?
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.calls == old(this.calls)
        invariant this.random == old(this.random)
        invariant this.testFunction == old(this.testFunction)
        invariant this.repr == old(this.repr)
        decreases i
      {
        rs := result.Extract();
        if i < |rs| {
          BinSearchDownHelper(i as nat, rs[i]);
        }
        i := i - 1;
      }
    }

    method ShrinkBySwapping()
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var k: int := 2;
      var rs := result.Extract();
      var i: int := |rs| - 1 - k;
      while i >= 0
        invariant Valid()
        invariant result.Some?
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.calls == old(this.calls)
        invariant this.random == old(this.random)
        invariant this.testFunction == old(this.testFunction)
        invariant this.repr == old(this.repr)
        decreases i
      {
        rs := result.Extract();
        var j: int := i + k;
        if j < |rs| && i < |rs| && 0 <= i {
          if rs[i] < rs[j] {
            var swapMap := map[j as nat := rs[i], i as nat := rs[j]];
            var _ := ReplaceMultiple(swapMap);
          }
          // Re-read after possible mutation.
          rs := result.Extract();
          if j < |rs| && i < |rs| && 0 <= i && rs[i] > 0 {
            var iPrev := rs[i];
            var jPrev := rs[j];
            BinSearchDownHelper2(i as nat, j as nat, jPrev, iPrev);
          }
        }
        i := i - 1;
      }
    }

    method Consider(attempt: seq<Choice>) returns (res: Option<seq<Choice>>)
      requires Valid()
      requires 0 < |attempt|
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures res.None? ==> result == old(result)
      ensures res.Some? ==> result == Some(attempt)
    {
      var testCase := TestCase.ForChoices(attempt, this.seed, false);
      // testCase + testCase.random are freshly allocated; testFunction.repr
      // contains only pre-existing objects, so the disjointness precondition
      // holds.
      assert testCase.repr == {testCase, testCase.random};
      assert testFunction.repr !! testCase.repr;
      var before := if result.Some? then result.Extract() else attempt;
      var newResult := testFunction.Apply(testCase);
      if newResult.Error().Some? && newResult.Error().Extract() == INTERESTING {
        // At High verbosity, trace each accepted shrink (old -> new choices).
        Reporting.ReportShrink(before, attempt, useColor, verbosity);
        result := Some(attempt);
        bestResult := newResult.value;
        res := Some(attempt);
      } else {
        res := None;
      }
    }

    // Binary search down to find the smallest value at result[index] that
    // still keeps the test INTERESTING. Java reference: TestingState.java:154-159
    // (the per-index closure passed to binSearchDown).
    method BinSearchDownHelper(index: nat, high: Choice)
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var rs := result.Extract();
      if index >= |rs| {
        return;
      }
      var lo: int := 0;
      var hi: int := high as int;
      var r0 := ReplaceSingle(index, 0 as Choice);
      if r0.Some? {
        return;
      }
      while lo + 1 < hi
        invariant Valid()
        invariant result.Some?
        invariant 0 <= lo <= hi <= high as int
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.calls == old(this.calls)
        invariant this.random == old(this.random)
        invariant this.testFunction == old(this.testFunction)
        invariant this.repr == old(this.repr)
        decreases hi - lo
      {
        var mid := lo + (hi - lo) / 2;
        var rmid := ReplaceSingle(index, mid as Choice);
        if rmid.Some? {
          hi := mid;
        } else {
          lo := mid;
        }
      }
    }

    // Two-index binary search: shrink result[i] while preserving the sum
    // result[i] + result[j]. Java reference: TestingState.java:169-175.
    method BinSearchDownHelper2(i: nat, j: nat, jPrev: Choice, high: Choice)
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var rs := result.Extract();
      if i >= |rs| || j >= |rs| {
        return;
      }
      var lo: int := 0;
      var hi: int := high as int;
      // Initial try at v == 0: jPrev + high. Choices are uint32; do the add in int
      // and reduce mod 2^32 so it wraps (the shrinker is a cold path, so int
      // arithmetic here costs nothing) — same intent as the Java reference.
      var jh: Choice := ((jPrev as int + high as int) % 0x1_0000_0000) as Choice;
      var r0 := ReplaceMultiple(map[i := 0 as Choice, j := jh]);
      if r0.Some? {
        return;
      }
      while lo + 1 < hi
        invariant Valid()
        invariant result.Some?
        invariant 0 <= lo <= hi <= high as int
        invariant this.maxExamples == old(this.maxExamples)
        invariant this.calls == old(this.calls)
        invariant this.random == old(this.random)
        invariant this.testFunction == old(this.testFunction)
        invariant this.repr == old(this.repr)
        decreases hi - lo
      {
        var mid := lo + (hi - lo) / 2;
        // 0 <= mid <= hi <= high, so high - mid is in [0, 2^32): fits a Choice.
        var delta: int := high as int - mid;
        var jd: Choice := ((jPrev as int + delta) % 0x1_0000_0000) as Choice;
        var rmid := ReplaceMultiple(map[i := mid as Choice, j := jd]);
        if rmid.Some? {
          hi := mid;
        } else {
          lo := mid;
        }
      }
    }

    method ReplaceSingle(index: nat, value: Choice) returns (res: Option<seq<Choice>>)
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var attempt := result.Extract();
      if index >= |attempt| || |attempt| == 0 {
        res := None;
        return;
      }
      attempt := attempt[..index] + [value] + attempt[index + 1..];
      res := Consider(attempt);
    }

    method ReplaceMultiple(values: map<nat, Choice>) returns (res: Option<seq<Choice>>)
      requires Valid()
      requires result.Some?
      modifies this`result, this`bestResult, testFunction
      ensures Valid()
      ensures this.maxExamples == old(this.maxExamples)
      ensures this.calls == old(this.calls)
      ensures this.random == old(this.random)
      ensures this.testFunction == old(this.testFunction)
      ensures this.repr == old(this.repr)
      ensures result.Some?
    {
      var attempt := result.Extract();
      if |attempt| == 0 {
        res := None;
        return;
      }
      var origLen := |attempt|;
      var keys := values.Keys;
      while keys != {}
        invariant |attempt| == origLen
        decreases keys
      {
        var k :| k in keys;
        keys := keys - {k};
        if k < |attempt| {
          attempt := attempt[..k] + [values[k]] + attempt[k + 1..];
        }
      }
      res := Consider(attempt);
    }

    function CompareChoices(choices1: seq<Choice>, choices2: seq<Choice>): int
    {
      if |choices1| < |choices2| then
        -1
      else if |choices1| > |choices2| then
        1
      else
        CompareChoicesHelper(choices1, choices2, 0)
    }

    function CompareChoicesHelper(choices1: seq<Choice>, choices2: seq<Choice>, i: nat): int
      requires |choices1| == |choices2|
      requires i <= |choices1| && i <= |choices2|
      decreases |choices1| - i
    {
      if i >= |choices1| then
        0
      else if choices1[i] < choices2[i] then
        -1
      else if choices1[i] > choices2[i] then
        1
      else
        CompareChoicesHelper(choices1, choices2, i + 1)
    }
  }

  // Top-level predicate-test entry points. All return `true` iff every
  // always-tested example and every generated case passed. RunTest and
  // RunTestWithExamples are thin delegators over RunTestWithConfig.
  method RunTest<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string) returns (passed: bool)
    requires arb.Valid()
  {
    passed := RunTestWithConfig(pred, arb, name, DefaultConfig<T>());
  }

  method RunTestWithExamples<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string, examples: nat)
      returns (passed: bool)
    requires arb.Valid()
    requires 0 < examples
  {
    passed := RunTestWithConfig(pred, arb, name, DefaultConfig<T>().(numRuns := examples));
  }

  // Config-driven predicate test: (1) evaluate the predicate directly on each
  // always-tested example, (2) run randomised generation + shrinking honoring
  // numRuns/seed/classifier, (3) report through the unified Reporting helpers.
  method RunTestWithConfig<T(!new)>(pred: T -> bool, arb: Arbitrary<T>, name: string, cfg: RunConfig<T>)
      returns (passed: bool)
    requires arb.Valid()
  {
    passed := true;
    var i := 0;
    while i < |cfg.examples|
      invariant 0 <= i <= |cfg.examples|
    {
      if !pred(cfg.examples[i]) {
        passed := false;
        Reporting.ReportFailingExample(cfg.examples[i], cfg.useColor, cfg.verbosity);
      }
      i := i + 1;
    }

    var seed := if cfg.seed.Some? then cfg.seed.value else 42;
    var numRuns := if cfg.numRuns == 0 then 1 else cfg.numRuns;
    var pt := new PredicateTest<T>(pred, arb);
    var rng := new SimpleRandomGen(seed);
    assert fresh(rng) && fresh(rng.random);
    // Fresh PredicateTest cannot alias the freshly-constructed rng or its inner random.
    assume {:axiom} rng !in pt.repr && rng.random !in pt.repr;
    var state := new TestingState<T>(rng, pt, numRuns, seed, cfg.classifier, cfg.verbosity, cfg.useColor);
    state.Run();

    var res := state.GetResult();
    var valid := state.GetValidTestCases();
    var best := state.GetBestResult();
    var stats := state.GetStats();

    match res {
      case None =>
        if valid == 0 {
          Reporting.ReportUnsatisfiable(name, cfg.useColor, cfg.verbosity);
          passed := false;
        } else if passed {
          Reporting.ReportSuccess(name, valid, cfg.useColor, cfg.verbosity);
        } else {
          Reporting.ReportFailure(name, cfg.useColor, cfg.verbosity);
        }
      case Some(choices) =>
        passed := false;
        Reporting.ReportFailure(name, cfg.useColor, cfg.verbosity);
        match best {
          case Some(v) => Reporting.ReportCounterExample(v, choices, cfg.useColor, cfg.verbosity);
          case None =>
        }
    }
    Reporting.ReportStatistics(name, stats, cfg.useColor, cfg.verbosity);
  }

  // Method-test entry points — siblings of the predicate runners. Return
  // `true` iff all examples and generated cases passed.
  method RunMethodTest<Input(!new), E(==)>(arb: Arbitrary<Input>, sut: MethodUnderTest<Input, E>, name: string)
      returns (passed: bool)
    requires arb.Valid() requires sut.Valid()
  {
    passed := RunMethodTestWithConfig(arb, sut, name, DefaultConfig<Input>());
  }

  method RunMethodTestWithExamples<Input(!new), E(==)>(
      arb: Arbitrary<Input>,
      sut: MethodUnderTest<Input, E>,
      name: string,
      examples: nat)
      returns (passed: bool)
    requires arb.Valid() requires sut.Valid()
    requires 0 < examples
  {
    passed := RunMethodTestWithConfig(arb, sut, name, DefaultConfig<Input>().(numRuns := examples));
  }

  method RunMethodTestWithConfig<Input(!new), E(==)>(
      arb: Arbitrary<Input>,
      sut: MethodUnderTest<Input, E>,
      name: string,
      cfg: RunConfig<Input>)
      returns (passed: bool)
    requires arb.Valid() requires sut.Valid()
  {
    var ce;
    passed, ce := RunMethodTestWithConfigCE(arb, sut, name, cfg);
  }

  // Like RunMethodTestWithConfig but also returns the minimised counterexample
  // VALUE (the shrunk failing input), so callers can assert shrink quality.
  // `passed` and all reporting are identical to RunMethodTestWithConfig.
  method RunMethodTestWithConfigCE<Input(!new), E(==)>(
      arb: Arbitrary<Input>,
      sut: MethodUnderTest<Input, E>,
      name: string,
      cfg: RunConfig<Input>)
      returns (passed: bool, counterexample: Option<Input>)
    requires arb.Valid() requires sut.Valid()
  {
    passed := true;
    counterexample := None;
    // Always-test examples by running the SUT directly on each.
    var i := 0;
    while i < |cfg.examples|
      invariant 0 <= i <= |cfg.examples|
      invariant arb.Valid() && sut.Valid()
    {
      var r := sut.run(cfg.examples[i]);
      if !(r.Success? && r.value) {
        passed := false;
        Reporting.ReportFailingExample(cfg.examples[i], cfg.useColor, cfg.verbosity);
      }
      i := i + 1;
    }

    var seed := if cfg.seed.Some? then cfg.seed.value else 42;
    var numRuns := if cfg.numRuns == 0 then 1 else cfg.numRuns;
    var mt := new MethodTest<Input, E>(arb, sut);
    var rng := new SimpleRandomGen(seed);
    assert fresh(rng) && fresh(rng.random);
    // Fresh MethodTest cannot alias the freshly-constructed rng or its
    // inner random — same discharge as the predicate runner above.
    assume {:axiom} rng !in mt.repr && rng.random !in mt.repr;
    var state := new TestingState<Input>(rng, mt, numRuns, seed, cfg.classifier, cfg.verbosity, cfg.useColor);
    state.Run();

    var res := state.GetResult();
    var valid := state.GetValidTestCases();
    var best := state.GetBestResult();
    var stats := state.GetStats();

    match res {
      case None =>
        if valid == 0 {
          Reporting.ReportUnsatisfiable(name, cfg.useColor, cfg.verbosity);
          passed := false;
        } else if passed {
          Reporting.ReportSuccess(name, valid, cfg.useColor, cfg.verbosity);
        } else {
          Reporting.ReportFailure(name, cfg.useColor, cfg.verbosity);
        }
      case Some(choices) =>
        passed := false;
        counterexample := best;
        Reporting.ReportFailure(name, cfg.useColor, cfg.verbosity);
        match best {
          case Some(v) => Reporting.ReportCounterExample(v, choices, cfg.useColor, cfg.verbosity);
          case None =>
        }
        match mt.lastResult {
          case Some(Failure(e)) => print "  last error: ", e, "\n";
          case _ =>
        }
    }
    Reporting.ReportStatistics(name, stats, cfg.useColor, cfg.verbosity);
  }
}

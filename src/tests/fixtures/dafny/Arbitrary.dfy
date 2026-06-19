include "./RandomGenerator.dfy"
include "./TestStatus.dfy"
include "./TestResult.dfy"
module Arbitraries {
  import opened TestResults
  import opened RandomGenerator
  import opened TestTypes
  import opened Std.Wrappers
  class TestCase {
    ghost var repr: set<object>
    var prefix: seq<Choice>
    var random: XoroShift128Plus
    var maxSize: nat
    var printResults: bool
    var depth: nat
    var targetingScore: nat
    var choices: seq<Choice>

    ghost predicate Valid()
      reads this
    {
      this.repr == {this, random}
    }

    constructor(prefix: seq<Choice>, random: XoroShift128Plus, maxSize: nat, printResults: bool)
      requires 0 < maxSize
      ensures this.random == random
      ensures this.prefix == prefix
      ensures this.maxSize == maxSize
      ensures this.printResults == printResults
      ensures this.Valid()
      ensures fresh(this)
    //   ensures fresh(repr)
    {
      this.prefix := prefix;
      this.random := random;
      this.maxSize := maxSize;
      this.printResults := printResults;
      this.depth := 0;
      this.targetingScore := 0; // Integer.MIN_VALUE equivalent
      this.choices := [];
      this.repr := {this, random};
    }

    // Static factory method for creating TestCase from choices
    static method ForChoices(choices: seq<Choice>, seed: bv64, printResults: bool) returns (tc: TestCase)
      requires 0 < |choices|
      ensures fresh(tc)
      ensures fresh(tc.random)
      ensures tc.Valid()
      ensures tc.prefix == choices
    {
      var random := XoroShift128Plus.fromSeed(seed);
      tc := new TestCase(choices, random, |choices|, printResults);
    }

    // Make a choice between 0 and n-1
    // method Choice(n: bv64) returns (result: bv64)
    //   requires 0 <= n
    //   modifies this
    // {
    //   var testResult := MakeChoice(n, () => this.random.unsafeNext() % n);
    //   if (ShouldPrint()) {
    //     print "choice(", n, "): ", testResult.Unwrap();
    //   }
    //   if (testResult.err.Some?) {
    //     // TODO: Handle unsatisfiable test case exception
    //     assert false; // Placeholder for exception
    //   }
    //   result := testResult.Unwrap();
    // }

    // // Force a specific choice
    method ForcedChoice(n: Choice) returns (result: TestResult<Choice>)
      requires 0 <= n
      ensures result.value.Some? ==> |choices| > old(|choices|)
      ensures old(this.Valid()) ==> this.Valid()
      ensures old(repr) == repr
      modifies this
    {
      if (n < 0) {
        // TODO: Handle IllegalArgumentException
        assert false;
      }
      if (|choices| > maxSize) {
        result := new TestResult<Choice>(Some(OVERRUN), None);
        return;
      }
      choices := choices + [n];
      result := new TestResult<Choice>(None, Some(n));
    }

    // Boolean choice with 50% probability. Native: one Choice draw, compare against
    // MOD/2. No real, no bv64 — a boolean is not "an arbitrary for real".
    method BooleanChoice() returns (result: bool)
      ensures old(this.Valid()) ==> this.Valid()
      // ensures old(repr) == repr
      modifies this, random
    {
      var rnd := this.random.unsafeNextChoice();
      result := rnd < 1073741823;   // floor((2^31-1)/2); ~50%
    }

    // Weighted boolean choice
    method Weighted(p: real) returns (result: bool)
      requires 0.0 <= p <= 1.0
      ensures old(this.Valid()) ==> this.Valid()
      // ensures old(repr) == repr
      modifies this, random
    {
      var testResult := WeightedInternal(p);
      if (!testResult.value.Some?) {
        result := false;
      }else {
        result := testResult.Unwrap();
      }
    }

    // Internal weighted choice that returns TestResult
    method WeightedInternal(p: real) returns (result: TestResult<bool>)
      requires 0.0 <= p <= 1.0
      ensures old(this.Valid()) ==> this.Valid()
      ensures old(repr) == repr
    //   ensures result.value.Some?
      modifies this, random
    {
      var intResult: TestResult<Choice>;
      if (p <= 0.0) {
        intResult := ForcedChoice(0);
      } else if (p >= 1.0) {
        intResult := ForcedChoice(1);
      } else {
        // Convert the real probability to a native threshold ONCE (the only real op,
        // at the boundary where the caller supplied a real `p`), then draw + compare
        // entirely in native Choice arithmetic. 0 < p < 1 => 0 <= thr < MOD < 2^32.
        var rnd := this.random.unsafeNextChoice();
        var thr: Choice := (p * 2147483647.0).Floor as Choice;
        var choice: Choice := if rnd < thr then 1 else 0;
        intResult := new TestResult<Choice>(None, Some(choice));
      }
      result := intResult.Map<bool>((i) => i == 1);
    }

    // Internal method to make a choice
    method MakeChoice(n: Choice) returns (result: TestResult<Choice>)
      requires 0 < n
      ensures old(this.Valid()) ==> this.Valid()
      ensures old(repr) == repr
      modifies this`choices, random
      // ensures result.value.Some? ==> result.value.Extract() < n
    {
      result := MakeChoice_(n, (rand) => rand % n);
    }

    // Internal method to make a choice with custom random function
    method MakeChoice_(n: Choice, randomFunc: Choice -> Choice) returns (result: TestResult<Choice>)
      requires 0 < n
      ensures old(this.Valid()) ==> this.Valid()
      ensures old(repr) == repr
      modifies this`choices, random
      // ensures result.value.Some? ==> exists x: Choice :: randomFunc(x) == result.value.Extract();
    {
      if (|choices| >= maxSize) {
        result := new TestResult<Choice>(Some(OVERRUN), None);
        return;
      }

      var choiceResult: Choice;
      if (|choices| < |prefix|) {
        choiceResult := prefix[|choices|];
      } else {
        var rand := this.random.unsafeNextChoice();
        choiceResult := randomFunc(rand);
      }
      // Valid choices index 0..n-1 (the random path uses rand % n, and callers
      // like OfTransformable index args[choice] with n == |args|). A replayed
      // prefix choice equal to n must therefore be rejected, not just one > n.
      if (choiceResult >= n) {
        result := new TestResult<Choice>(Some(INVALID), None);
        return;
      }
      choices := choices + [choiceResult];
      result := new TestResult<Choice>(None, Some(choiceResult));
    }

    // Check if should print debug information
    function ShouldPrint(): bool
      reads this
    {
      printResults && depth == 0
    }

    // Reject the current test case
    method Reject()
      modifies this
    {
      // TODO: Handle InvalidTestCaseException
    //   assert false;
    }

    // Assume a precondition
    method Assume(precondition: bool)
      modifies this
    {
      if (!precondition) {
        Reject();
      }
    }

    // Set targeting score
    method Target(n: nat)
      modifies this
    {
      this.targetingScore := n;
    }

    // Get targeting score
    function GetTargetingScore(): int
      reads this
    {
      targetingScore
    }

    // Get choices
    function GetChoices(): seq<Choice>
      reads this
    {
      choices
    }

    // Apply a possibility to generate a value
    method Any<T>(possibility: Arbitrary<T>) returns (result: T)
      requires possibility.internalFunction.repr !! this.repr
      requires possibility.Valid()
      requires Valid()
      // decreases parentRepr
      modifies this, random
      ensures Valid()
      ensures this.random == old(this.random)
      ensures this.repr == old(this.repr)
      ensures possibility.Valid()
      ensures possibility.internalFunction.repr == old(possibility.internalFunction.repr)
    {
      this.depth := this.depth + 1;
      result := possibility.Apply(this);
    //   this.depth := this.depth - 1;
    }
  }

    // Choice bound for generator arguments. Was MaxLong (2^64-1, the old bv64 choice
    // base); now MaxChoice (2^32-1) since choices are uint32 — imported from
    // RandomGenerator. Generator args (|args|, max-min, |possibilities|, list bound)
    // are cast `as Choice` for MakeChoice, so they must fit a uint32.
    trait Transformable<T> {
        ghost var repr: set<object>
        ghost var childRepr: set<object>
        method Apply(tc: TestCase) returns (result: T)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random

        ghost predicate Valid()
            reads this, repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
    }

    class OfTransformable<T> extends Transformable<T> {
        var args: seq<T>
        constructor(args: seq<T>)
            ensures fresh(this)
            ensures fresh(this.repr)
            requires 0 < |args| <= MaxChoice
            ensures Valid()
        {
            this.args := args;
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this in repr && 0 < |args| <= MaxChoice && childRepr < this.repr && this.repr == {this} + childRepr
        }

        method Apply(tc: TestCase) returns (result: T)
            requires allocated(tc)
            requires this.Valid()
            requires tc.Valid()
            requires tc.repr !! this.repr
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var choiceResult := tc.MakeChoice(|args| as Choice);
            if choiceResult.value.Some? {
                expect choiceResult.value.Extract() as int < |args|;
                return args[choiceResult.Unwrap() as int];
            }else{
                return args[0];
            }
        }
    }

    class JustTransformable<T> extends Transformable<T> {
        var value: T
        constructor(value: T)
            ensures fresh(this)
            ensures fresh(this.repr)
            ensures Valid()
        {
            this.value := value;
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this in this.repr && childRepr < this.repr && this.repr == {this} + childRepr
        }

        method Apply(tc: TestCase) returns (result: T)
            requires allocated(tc)
            requires this.Valid()
            requires tc.Valid()
            requires tc.repr !! this.repr
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            result := this.value;
        }
    }

    class RangeTransformable extends Transformable<int> {
        var min: int
        var max: int
        constructor(min: int, max: int)
            ensures fresh(this)
            ensures fresh(this.repr)
            requires min <= max && (0 < max - min <= MaxChoice)
            ensures Valid()
        {
            this.min := min;
            this.max := max;
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this in repr && min <= max && (0 < max - min <= MaxChoice) && childRepr < this.repr && this.repr == {this} + childRepr
        }

        method Apply(tc: TestCase) returns (result: int)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var choiceResult := tc.MakeChoice((max - min) as Choice);
            if choiceResult.value.Some? {
                result := min + (choiceResult.Unwrap() as int);
            } else {
                result := min;
            }
        }
    }

    // class NothingTransformable<T> extends Transformable<T> {
    //     constructor()
    //         ensures fresh(this)
    //         ensures Valid()
    //     {
    //     }

    //     predicate Valid()
    //         reads this
    //     {
    //         true
    //     }

    //     method Apply(tc: TestCase) returns (result: T)
    //         requires allocated(tc)
    //         requires this.Valid()
    //         modifies tc, tc.random
    //     {
    //         tc.Reject();
    //         // This will never return due to Reject()
    //         result := *;
    //     }
    // }

    class MixTransformable<T> extends Transformable<T> {
        var possibilities: seq<Arbitrary<T>>
        constructor(possibilities: seq<Arbitrary<T>>)
            ensures fresh(this)
            // ensures fresh(this.repr)
            requires 0 < |possibilities| < MaxChoice
            requires forall x,y :: x in possibilities && y in possibilities && x != y ==> x.internalFunction.repr !! y.internalFunction.repr
            requires forall i :: 0 <= i < |possibilities| ==> possibilities[i].Valid()
            ensures Valid()
        {
            this.possibilities := possibilities;
            this.childRepr := set p, q | p in possibilities && q in p.internalFunction.repr :: q;
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            reads this, repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this in repr && 
            0 < |possibilities| < MaxChoice && 
            (forall i :: 0 <= i < |possibilities| ==> possibilities[i].internalFunction in childRepr) &&
            (forall i :: 0 <= i < |possibilities| ==> possibilities[i].internalFunction.repr <= childRepr) &&
            (forall x,y :: x in possibilities && y in possibilities && x != y ==> x.internalFunction.repr !! y.internalFunction.repr) &&
            //New errror exception until further research
            (forall i :: 0 <= i < |possibilities| ==> possibilities[i].Valid()) &&
            childRepr < this.repr && 
            this.repr == {this} + childRepr
        }

        method Apply(tc: TestCase) returns (result: T)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var choiceResult := tc.MakeChoice(|possibilities| as Choice);
            if choiceResult.value.Some? {
                var choice := choiceResult.Unwrap() as int;
                expect choice < |possibilities|;
                // result := tc.Any(possibilities[choice]);
                result := possibilities[choice].internalFunction.Apply(tc);
            } else {
                // result := tc.Any(possibilities[0]);
                result := possibilities[0].internalFunction.Apply(tc);
            }
        }
    }

    class BoolsTransformable extends Transformable<bool> {
        constructor()
            ensures fresh(this)
            ensures fresh(this.repr)
            ensures Valid()
        {
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this.repr == {this} + childRepr && childRepr < this.repr
        }

        method Apply(tc: TestCase) returns (result: bool)
            requires allocated(tc)
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var choiceResult := tc.MakeChoice(2);
            if choiceResult.value.Some? {
                result := choiceResult.Unwrap() == 1;
            } else {
                result := false;
            }
        }
    }

    class ListsTransformable<S> extends Transformable<seq<S>> {
        var elementGenerator: Arbitrary<S>
        var minSize: int
        var maxSize: int
        constructor(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int)
            requires 0 <= minSize <= maxSize
            requires elementGenerator.Valid()
            ensures elementGenerator.internalFunction.repr < this.repr
            ensures this.repr == {this}+elementGenerator.internalFunction.repr
            ensures fresh(this)
            ensures Valid()
        {
            this.elementGenerator := elementGenerator;
            this.minSize := minSize;
            this.maxSize := maxSize;
            this.childRepr := elementGenerator.internalFunction.repr;
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            decreases repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            reads this, repr, childRepr
        {
            this in this.repr &&
            elementGenerator.internalFunction in this.repr &&
            elementGenerator.internalFunction.repr < this.repr &&
            0 <= minSize <= maxSize && elementGenerator.Valid() &&
            childRepr < this.repr && this.repr == {this} + childRepr
        }

        method Apply(tc: TestCase) returns (result: seq<S>)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            // requires elementGenerator.internalFunction.repr == childRepr;
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            result := [];
            while true
                invariant |result| <= maxSize
                invariant tc.Valid()
                invariant tc.repr == old(tc.repr)
                modifies tc, tc.random
                decreases maxSize-|result|
            {
                if |result| < minSize {
                    // Force continue
                    var forceResult := tc.ForcedChoice(1);
                    if forceResult.err.Some? {
                        break;
                    }
                } else if |result| >= maxSize {
                    // Force stop
                    var forceResult := tc.ForcedChoice(0);
                    if forceResult.err.Some? {
                        break;
                    }
                    break;
                } else {
                    // Weighted choice
                    var weightedResult := tc.WeightedInternal(0.9);
                    if !weightedResult.value.Some? || !weightedResult.Unwrap() {
                        break;
                    }
                }
                var element := elementGenerator.internalFunction.Apply(tc);
                result := result + [element];
            }
        }
    }

    class StringsTransformable extends Transformable<string> {
        var minLength: int
        var maxLength: int
        var ascii: bool
        constructor(minLength: int, maxLength: int, ascii: bool)
            ensures fresh(this)
            ensures fresh(this.repr)
            requires 0 <= minLength <= maxLength
            ensures Valid()
        {
            this.minLength := minLength;
            this.maxLength := maxLength;
            this.ascii := ascii;
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            decreases repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            reads this, repr, childRepr
        {
          this in repr && 0 <= minLength <= maxLength && childRepr < this.repr && this.repr == {this} + childRepr
        }

        method {:isolate_assertions} Apply(tc: TestCase) returns (result: string)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var chars: seq<nat> := [];
            while true
                invariant |chars| <= maxLength
                invariant tc.Valid()
                invariant tc.repr == old(tc.repr)
                invariant forall x :: x in chars ==> x < 0x0000D800
                modifies tc, tc.random
                decreases maxLength-|chars|
            {
                if |chars| < minLength {
                    // Force continue
                    var forceResult := tc.ForcedChoice(1);
                    if forceResult.err.Some? {
                        break;
                    }
                } else if |chars| >= maxLength {
                    // Force stop
                    var forceResult := tc.ForcedChoice(0);
                    if forceResult.err.Some? {
                        break;
                    }
                    break;
                } else {
                    // Weighted choice
                    var weightedResult := tc.WeightedInternal(0.9);
                    if !weightedResult.value.Some? || !weightedResult.Unwrap() {
                        break;
                    }
                }
                var charChoice := if ascii then 128 else 0x0000D800;
                var choiceResult := tc.MakeChoice(charChoice as Choice);
                var charValue := if choiceResult.value.Some? then choiceResult.Unwrap() else 0;
                assume {:axiom} 0 <= charValue < 0x0000D800;
                chars := chars + [charValue as nat];
            }
            result := "";
            for i := 0 to |chars|
                invariant |result| == i
            {
                assert chars[i] in chars; 
                result := result + [chars[i] as char];
            }
        }
    }

    class MapTransformable<T, U> extends Transformable<T> {
      var elementGenerator: Arbitrary<U>
      var fn: U-> T
      constructor(elementGenerator: Arbitrary<U>, fn: U->T)
          requires elementGenerator.Valid()
          ensures elementGenerator.internalFunction.repr < this.repr
          ensures this.repr == {this}+elementGenerator.internalFunction.repr
          ensures this.fn == fn
          ensures fresh(this)
          ensures Valid()
      {
        this.elementGenerator := elementGenerator;
        this.childRepr := elementGenerator.internalFunction.repr;
        this.fn := fn;
        this.repr := {this}+this.childRepr;
      }

        ghost predicate Valid()
            decreases repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            reads this, repr, childRepr
        {
          this in repr && this.repr == {this}+childRepr &&
            childRepr < repr &&
            elementGenerator.internalFunction in this.repr &&
            elementGenerator.internalFunction.repr < this.repr &&
            elementGenerator.Valid()
        }

        method Apply(tc: TestCase) returns (result: T)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
          var element := elementGenerator.internalFunction.Apply(tc);
          result := fn(element);
        }
    }

    class TupleTransformable<T, U> extends Transformable<(T, U)> {
        var firstGenerator: Arbitrary<T>
        var secondGenerator: Arbitrary<U>
        
        constructor(firstGenerator: Arbitrary<T>, secondGenerator: Arbitrary<U>)
            requires firstGenerator.Valid()
            requires secondGenerator.Valid()
            ensures firstGenerator.internalFunction.repr < this.repr
            ensures secondGenerator.internalFunction.repr < this.repr
            ensures this.repr == {this} + firstGenerator.internalFunction.repr + secondGenerator.internalFunction.repr
            ensures fresh(this)
            ensures Valid()
        {
            this.firstGenerator := firstGenerator;
            this.secondGenerator := secondGenerator;
            this.childRepr := firstGenerator.internalFunction.repr + secondGenerator.internalFunction.repr;
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            decreases repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            reads this, repr, childRepr
        {
            this in repr && 
            this.repr == {this} + childRepr &&
            childRepr < repr &&
            firstGenerator.internalFunction in this.repr &&
            secondGenerator.internalFunction in this.repr &&
            firstGenerator.internalFunction.repr < this.repr &&
            secondGenerator.internalFunction.repr < this.repr &&
            firstGenerator.Valid() &&
            secondGenerator.Valid()
        }

        method Apply(tc: TestCase) returns (result: (T, U))
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var first := firstGenerator.internalFunction.Apply(tc);
            var second := secondGenerator.internalFunction.Apply(tc);
            result := (first, second);
        }
    }


    trait FlatMapFn<T, U> {
        // This method creates a new Arbitrary<U> based on an input T
        method CreateArbitrary(t: T) returns (p: Arbitrary<U>)
            ensures p.Valid()
            ensures fresh(p.internalFunction.repr)
    }

    class FlatMapTransformable<T, U> extends Transformable<U> {
        var baseGenerator: Arbitrary<T> // The original generator
        var flatMapFn: FlatMapFn<T, U> // The factory function

        constructor(baseGenerator: Arbitrary<T>, flatMapFn: FlatMapFn<T, U>)
            requires baseGenerator.Valid()
            requires flatMapFn !in baseGenerator.internalFunction.repr
            ensures fresh(this)
            ensures this.childRepr == baseGenerator.internalFunction.repr+{flatMapFn}
            ensures Valid()
        {
            this.baseGenerator := baseGenerator;
            this.flatMapFn := flatMapFn;
            this.childRepr := baseGenerator.internalFunction.repr+{flatMapFn};
            this.repr := {this} + this.childRepr;
        }

        ghost predicate Valid()
            reads this, repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this in repr && this.repr == {this} + childRepr &&
            childRepr < repr &&
            baseGenerator.internalFunction in childRepr &&
            baseGenerator.internalFunction.repr < childRepr &&
            baseGenerator.Valid()
        }

        method Apply(tc: TestCase) returns (result: U)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var intermediateValue := baseGenerator.internalFunction.Apply(tc); // First, generate T
            var nextArbitrary := flatMapFn.CreateArbitrary(intermediateValue); // Then, create Arbitrary<U>
            assert tc.repr !! nextArbitrary.internalFunction.repr; // Example assertion, might need to be required
            // KNOWN: "decreases clause might not decrease" verification error here is expected.
            // The repr/childRepr metric can't yet prove termination through the flatMap'd
            // nextArbitrary. Expected to fail until the termination proof is completed (future work).
            result := nextArbitrary.internalFunction.Apply(tc);

        }
    }

    // Heap-allocated 1-D array generator. Mirrors ListsTransformable to build a
    // seq<S>, then copies it into a freshly-allocated array (the trait's Apply
    // places no constraint on `result`, so returning a fresh array is sound).
    class ArraysTransformable<S> extends Transformable<array<S>> {
        var elementGenerator: Arbitrary<S>
        var minSize: int
        var maxSize: int
        constructor(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int)
            requires 0 <= minSize <= maxSize
            requires elementGenerator.Valid()
            ensures elementGenerator.internalFunction.repr < this.repr
            ensures this.repr == {this}+elementGenerator.internalFunction.repr
            ensures fresh(this)
            ensures Valid()
        {
            this.elementGenerator := elementGenerator;
            this.minSize := minSize;
            this.maxSize := maxSize;
            this.childRepr := elementGenerator.internalFunction.repr;
            this.repr := {this} + this.childRepr;
        }
        ghost predicate Valid()
            decreases repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            reads this, repr, childRepr
        {
            this in this.repr &&
            elementGenerator.internalFunction in this.repr &&
            elementGenerator.internalFunction.repr < this.repr &&
            0 <= minSize <= maxSize && elementGenerator.Valid() &&
            childRepr < this.repr && this.repr == {this} + childRepr
        }
        method Apply(tc: TestCase) returns (result: array<S>)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var xs: seq<S> := [];
            while true
                invariant |xs| <= maxSize
                invariant tc.Valid()
                invariant tc.repr == old(tc.repr)
                modifies tc, tc.random
                decreases maxSize-|xs|
            {
                if |xs| < minSize {
                    var forceResult := tc.ForcedChoice(1);
                    if forceResult.err.Some? { break; }
                } else if |xs| >= maxSize {
                    var forceResult := tc.ForcedChoice(0);
                    if forceResult.err.Some? { break; }
                    break;
                } else {
                    var weightedResult := tc.WeightedInternal(0.9);
                    if !weightedResult.value.Some? || !weightedResult.Unwrap() { break; }
                }
                var element := elementGenerator.internalFunction.Apply(tc);
                xs := xs + [element];
            }
            result := new S[|xs|](idx requires 0 <= idx < |xs| => xs[idx]);
        }
    }

    // Heap-allocated 2-D / 3-D array generators of FIXED size: the caller fixes
    // the dimensions (rows x cols, rows x cols x layers) and every generated
    // array has exactly that shape. Elements are generated into a flat seq and
    // read through a guarded init function (so no nonlinear index-bound proof is
    // needed); the flat seq always covers the whole array.
    class Array2Transformable<S> extends Transformable<array2<S>> {
        var elementGenerator: Arbitrary<S>
        var rows: nat
        var cols: nat
        constructor(elementGenerator: Arbitrary<S>, rows: nat, cols: nat)
            requires elementGenerator.Valid()
            ensures elementGenerator.internalFunction.repr < this.repr
            ensures this.repr == {this}+elementGenerator.internalFunction.repr
            ensures fresh(this)
            ensures Valid()
        {
            this.elementGenerator := elementGenerator;
            this.rows := rows;
            this.cols := cols;
            this.childRepr := elementGenerator.internalFunction.repr;
            this.repr := {this} + this.childRepr;
        }
        ghost predicate Valid()
            decreases repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            reads this, repr, childRepr
        {
            this in this.repr &&
            elementGenerator.internalFunction in this.repr &&
            elementGenerator.internalFunction.repr < this.repr &&
            elementGenerator.Valid() &&
            childRepr < this.repr && this.repr == {this} + childRepr
        }
        method {:isolate_assertions} Apply(tc: TestCase) returns (result: array2<S>)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            ensures result.Length0 == rows && result.Length1 == cols
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var m: nat := rows;
            var n: nat := cols;
            var dflt := elementGenerator.internalFunction.Apply(tc);
            var total := m * n;
            var flat: seq<S> := [dflt];
            while |flat| < total
                invariant tc.Valid()
                invariant tc.repr == old(tc.repr)
                invariant 1 <= |flat|
                decreases total - |flat|
                modifies tc, tc.random
            {
                var el := elementGenerator.internalFunction.Apply(tc);
                flat := flat + [el];
            }
            result := new S[m, n]((i: nat, j: nat) => if i * n + j < |flat| then flat[i * n + j] else dflt);
        }
    }

    class Array3Transformable<S> extends Transformable<array3<S>> {
        var elementGenerator: Arbitrary<S>
        var rows: nat
        var cols: nat
        var layers: nat
        constructor(elementGenerator: Arbitrary<S>, rows: nat, cols: nat, layers: nat)
            requires elementGenerator.Valid()
            ensures elementGenerator.internalFunction.repr < this.repr
            ensures this.repr == {this}+elementGenerator.internalFunction.repr
            ensures fresh(this)
            ensures Valid()
        {
            this.elementGenerator := elementGenerator;
            this.rows := rows;
            this.cols := cols;
            this.layers := layers;
            this.childRepr := elementGenerator.internalFunction.repr;
            this.repr := {this} + this.childRepr;
        }
        ghost predicate Valid()
            decreases repr, childRepr
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            reads this, repr, childRepr
        {
            this in this.repr &&
            elementGenerator.internalFunction in this.repr &&
            elementGenerator.internalFunction.repr < this.repr &&
            elementGenerator.Valid() &&
            childRepr < this.repr && this.repr == {this} + childRepr
        }
        method {:isolate_assertions} Apply(tc: TestCase) returns (result: array3<S>)
            requires allocated(tc)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.repr == old(this.repr)
            ensures result.Length0 == rows && result.Length1 == cols && result.Length2 == layers
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var m: nat := rows;
            var n: nat := cols;
            var o: nat := layers;
            var dflt := elementGenerator.internalFunction.Apply(tc);
            var total := m * n * o;
            var flat: seq<S> := [dflt];
            while |flat| < total
                invariant tc.Valid()
                invariant tc.repr == old(tc.repr)
                invariant 1 <= |flat|
                decreases total - |flat|
                modifies tc, tc.random
            {
                var el := elementGenerator.internalFunction.Apply(tc);
                flat := flat + [el];
            }
            result := new S[m, n, o]((i: nat, j: nat, k: nat) =>
                var idx := i * n * o + j * o + k;
                if idx < |flat| then flat[idx] else dflt);
        }
    }

    // ================================================================
    // Core leaf generators for additional Dafny value types. Each follows
    // the BoolsTransformable template (no child generators: childRepr = {},
    // repr = {this} + childRepr). MakeChoice has no proven upper bound on its
    // result, so where a cast needs one we discharge it with the same
    // {:axiom} assume the StringsTransformable above uses for chars.
    // ================================================================

    // Natural numbers in [0, bound). `bound` is informational; values are
    // drawn modulo it at runtime (bv64 -> nat is always nonnegative).
    class NatsTransformable extends Transformable<nat> {
        var bound: nat
        constructor(bound: nat)
            requires 0 < bound <= MaxChoice
            ensures fresh(this)
            ensures fresh(this.repr)
            ensures Valid()
        {
            this.bound := bound;
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this.repr == {this} + childRepr && childRepr < this.repr && 0 < bound <= MaxChoice
        }
        method Apply(tc: TestCase) returns (result: nat)
            requires allocated(tc)
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var c := tc.MakeChoice(bound as Choice);
            if c.value.Some? {
                result := c.Unwrap() as nat;
            } else {
                result := 0;
            }
        }
    }

    // Printable-ASCII characters in [32, 127).
    class CharsTransformable extends Transformable<char> {
        constructor()
            ensures fresh(this)
            ensures fresh(this.repr)
            ensures Valid()
        {
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this.repr == {this} + childRepr && childRepr < this.repr
        }
        method Apply(tc: TestCase) returns (result: char)
            requires allocated(tc)
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var c := tc.MakeChoice(95);
            var v := if c.value.Some? then c.Unwrap() as int else 0;
            assume {:axiom} 0 <= v < 95;
            result := (32 + v) as char;
        }
    }

    // Non-negative rationals, generated as numerator / (denominator + 1).
    class RealsTransformable extends Transformable<real> {
        constructor()
            ensures fresh(this)
            ensures fresh(this.repr)
            ensures Valid()
        {
            this.childRepr := {};
            this.repr := {this} + this.childRepr;
        }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this.repr == {this} + childRepr && childRepr < this.repr
        }
        method Apply(tc: TestCase) returns (result: real)
            requires allocated(tc)
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var cn := tc.MakeChoice(1000000);
            var num := if cn.value.Some? then cn.Unwrap() as int else 0;
            var cd := tc.MakeChoice(1000);
            var den := if cd.value.Some? then cd.Unwrap() as int else 0;
            result := (num as real) / ((den + 1) as real);
        }
    }

    // ----------------------------------------------------------------
    // Fixed-width bit-vector generators. Widths up to 64 draw one choice and
    // narrow it (bound discharged by {:axiom} like the char path); 128 and 256
    // assemble multiple 64-bit words with widening casts (no bound needed).
    // ----------------------------------------------------------------
    class BitVectors1Transformable extends Transformable<bv1> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv1)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            var c := tc.MakeChoice(2);
            var v := if c.value.Some? then c.Unwrap() else 0;
            assume {:axiom} v < 2;
            result := v as bv1;
        }
    }

    class BitVectors2Transformable extends Transformable<bv2> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv2)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            var c := tc.MakeChoice(4);
            var v := if c.value.Some? then c.Unwrap() else 0;
            assume {:axiom} v < 4;
            result := v as bv2;
        }
    }

    class BitVectors8Transformable extends Transformable<bv8> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv8)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            var c := tc.MakeChoice(256);
            var v := if c.value.Some? then c.Unwrap() else 0;
            assume {:axiom} v < 256;
            result := v as bv8;
        }
    }

    class BitVectors16Transformable extends Transformable<bv16> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv16)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            var c := tc.MakeChoice(0x10000);
            var v := if c.value.Some? then c.Unwrap() else 0;
            assume {:axiom} v < 0x10000;
            result := v as bv16;
        }
    }

    class BitVectors32Transformable extends Transformable<bv32> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv32)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            // The native lane only spans 31 bits, so assemble 32 bits from two 16-bit
            // Choice chunks. Each chunk is < 2^16 (MakeChoice rejects >= n), so the
            // Choice -> bv32 casts are total and the chunks never overlap.
            var c0 := tc.MakeChoice(0x10000);
            var w0 := if c0.value.Some? then c0.Unwrap() else 0;
            var c1 := tc.MakeChoice(0x10000);
            var w1 := if c1.value.Some? then c1.Unwrap() else 0;
            result := ((w0 as bv32) << 16) | (w1 as bv32);
        }
    }

    class BitVectors64Transformable extends Transformable<bv64> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv64)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            // 64 bits from four 16-bit Choice chunks (the native lane spans only 31).
            var c0 := tc.MakeChoice(0x10000);
            var w0 := if c0.value.Some? then c0.Unwrap() else 0;
            var c1 := tc.MakeChoice(0x10000);
            var w1 := if c1.value.Some? then c1.Unwrap() else 0;
            var c2 := tc.MakeChoice(0x10000);
            var w2 := if c2.value.Some? then c2.Unwrap() else 0;
            var c3 := tc.MakeChoice(0x10000);
            var w3 := if c3.value.Some? then c3.Unwrap() else 0;
            result := ((w0 as bv64) << 48) | ((w1 as bv64) << 32) |
                      ((w2 as bv64) << 16) | (w3 as bv64);
        }
    }

    class BitVectors128Transformable extends Transformable<bv128> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv128)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            // 128 bits from eight 16-bit Choice chunks.
            var c0 := tc.MakeChoice(0x10000);
            var w0 := if c0.value.Some? then c0.Unwrap() else 0;
            var c1 := tc.MakeChoice(0x10000);
            var w1 := if c1.value.Some? then c1.Unwrap() else 0;
            var c2 := tc.MakeChoice(0x10000);
            var w2 := if c2.value.Some? then c2.Unwrap() else 0;
            var c3 := tc.MakeChoice(0x10000);
            var w3 := if c3.value.Some? then c3.Unwrap() else 0;
            var c4 := tc.MakeChoice(0x10000);
            var w4 := if c4.value.Some? then c4.Unwrap() else 0;
            var c5 := tc.MakeChoice(0x10000);
            var w5 := if c5.value.Some? then c5.Unwrap() else 0;
            var c6 := tc.MakeChoice(0x10000);
            var w6 := if c6.value.Some? then c6.Unwrap() else 0;
            var c7 := tc.MakeChoice(0x10000);
            var w7 := if c7.value.Some? then c7.Unwrap() else 0;
            result := ((w0 as bv128) << 112) | ((w1 as bv128) << 96) |
                      ((w2 as bv128) << 80)  | ((w3 as bv128) << 64) |
                      ((w4 as bv128) << 48)  | ((w5 as bv128) << 32) |
                      ((w6 as bv128) << 16)  | (w7 as bv128);
        }
    }

    class BitVectors256Transformable extends Transformable<bv256> {
        constructor() ensures fresh(this) ensures fresh(this.repr) ensures Valid()
        { this.childRepr := {}; this.repr := {this} + this.childRepr; }
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        { this.repr == {this} + childRepr && childRepr < this.repr }
        method Apply(tc: TestCase) returns (result: bv256)
            requires allocated(tc) requires this.Valid() requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid() ensures tc.Valid() ensures tc.repr == old(tc.repr)
            decreases repr, childRepr modifies tc, tc.random
        {
            // 256 bits from sixteen 16-bit Choice chunks.
            var c0 := tc.MakeChoice(0x10000);  var w0 := if c0.value.Some? then c0.Unwrap() else 0;
            var c1 := tc.MakeChoice(0x10000);  var w1 := if c1.value.Some? then c1.Unwrap() else 0;
            var c2 := tc.MakeChoice(0x10000);  var w2 := if c2.value.Some? then c2.Unwrap() else 0;
            var c3 := tc.MakeChoice(0x10000);  var w3 := if c3.value.Some? then c3.Unwrap() else 0;
            var c4 := tc.MakeChoice(0x10000);  var w4 := if c4.value.Some? then c4.Unwrap() else 0;
            var c5 := tc.MakeChoice(0x10000);  var w5 := if c5.value.Some? then c5.Unwrap() else 0;
            var c6 := tc.MakeChoice(0x10000);  var w6 := if c6.value.Some? then c6.Unwrap() else 0;
            var c7 := tc.MakeChoice(0x10000);  var w7 := if c7.value.Some? then c7.Unwrap() else 0;
            var c8 := tc.MakeChoice(0x10000);  var w8 := if c8.value.Some? then c8.Unwrap() else 0;
            var c9 := tc.MakeChoice(0x10000);  var w9 := if c9.value.Some? then c9.Unwrap() else 0;
            var c10 := tc.MakeChoice(0x10000); var w10 := if c10.value.Some? then c10.Unwrap() else 0;
            var c11 := tc.MakeChoice(0x10000); var w11 := if c11.value.Some? then c11.Unwrap() else 0;
            var c12 := tc.MakeChoice(0x10000); var w12 := if c12.value.Some? then c12.Unwrap() else 0;
            var c13 := tc.MakeChoice(0x10000); var w13 := if c13.value.Some? then c13.Unwrap() else 0;
            var c14 := tc.MakeChoice(0x10000); var w14 := if c14.value.Some? then c14.Unwrap() else 0;
            var c15 := tc.MakeChoice(0x10000); var w15 := if c15.value.Some? then c15.Unwrap() else 0;
            result := ((w0 as bv256) << 240)  | ((w1 as bv256) << 224) |
                      ((w2 as bv256) << 208)  | ((w3 as bv256) << 192) |
                      ((w4 as bv256) << 176)  | ((w5 as bv256) << 160) |
                      ((w6 as bv256) << 144)  | ((w7 as bv256) << 128) |
                      ((w8 as bv256) << 112)  | ((w9 as bv256) << 96)  |
                      ((w10 as bv256) << 80)  | ((w11 as bv256) << 64)  |
                      ((w12 as bv256) << 48)  | ((w13 as bv256) << 32)  |
                      ((w14 as bv256) << 16)  | (w15 as bv256);
        }
    }

    // Build a map from a sequence of key/value pairs (later-listed keys win,
    // matching Dafny map-update order). Used to derive the Maps generator from
    // a Lists-of-Tuples generator without a bespoke Transformable.
    function SeqToMap<K, V>(pairs: seq<(K, V)>): map<K, V> {
        if |pairs| == 0 then map[]
        else SeqToMap(pairs[1..])[pairs[0].0 := pairs[0].1]
    }

    // ================================================================
    // letrec-style recursive generators (à la fast-check's letrec).
    //
    // A `Registry<T>` ties mutually-recursive arbitraries together by name.
    // Build each named arbitrary using `reg.Tie(name)` wherever a recursive
    // position appears, `Register` them, then `Lookup` the entry point:
    //
    //   var reg := new Registry<Tree>("Leaf", 5);            // base key + maxDepth
    //   var node := Arbitrary.Tuple(reg.Tie("Tree"), reg.Tie("Tree"))
    //                 .Map((t) => Node(t.0, t.1));
    //   reg.Register("Leaf", Arbitrary.Just(Leaf));
    //   reg.Register("Node", node);
    //   reg.Register("Tree", Arbitrary.Mix([reg.Tie("Leaf"), reg.Tie("Node")]));
    //   var arb := reg.Lookup("Tree");
    //
    // Each `Tie(name)` returns a FRESH lazy node with a singleton repr, so
    // distinct ties satisfy the pairwise-disjointness preconditions of Tuple/Mix
    // even though they alias the same registry slot. At Apply time a lazy resolves
    // its name in the registry; the TestCase `depth` field is the recursion
    // budget — at `maxDepth` the lazy yields the registry's base-case arbitrary
    // instead of recursing, which is what makes generation terminate (the base
    // case must be non-recursive). Like FlatMap, the resolve-through-dispatch step
    // carries an accepted "decreases" obligation and a couple of framing axioms.
    // ================================================================
    class Registry<T(!new)> {
        var arbs: map<string, Arbitrary<T>>
        const baseKey: string
        const maxDepth: nat

        constructor(baseKey: string, maxDepth: nat)
            ensures fresh(this)
            ensures this.baseKey == baseKey && this.maxDepth == maxDepth
        {
            this.arbs := map[];
            this.baseKey := baseKey;
            this.maxDepth := maxDepth;
        }

        // Register (or replace) the arbitrary bound to `key`.
        method Register(key: string, arb: Arbitrary<T>)
            modifies this`arbs
            ensures key in this.arbs && this.arbs[key] == arb
        {
            this.arbs := this.arbs[key := arb];
        }

        // A lazy reference to the arbitrary bound to `key`, resolved at Apply time.
        // Fresh + singleton repr so multiple ties are mutually disjoint.
        method Tie(key: string) returns (a: Arbitrary<T>)
            ensures a.Valid()
            ensures fresh(a.internalFunction)
            ensures fresh(a.internalFunction.repr)
        {
            var lz := new LazyArbitrary<T>(this, key);
            a := Arbitrary(lz);
        }

        function Lookup(key: string): Arbitrary<T>
            reads this
            requires key in this.arbs
        {
            this.arbs[key]
        }
    }

    class LazyArbitrary<T(!new)> extends Transformable<T> {
        const registry: Registry<T>
        const key: string

        constructor(registry: Registry<T>, key: string)
            ensures fresh(this)
            ensures fresh(this.repr)
            ensures Valid()
        {
            this.registry := registry;
            this.key := key;
            this.childRepr := {};
            this.repr := {this};
        }

        // The lazy node owns nothing but itself, so Valid()/repr stay acyclic and
        // trivially well-formed regardless of the (possibly self-referential)
        // registry it points into.
        ghost predicate Valid()
            reads this
            ensures Valid() ==> this in repr
            ensures Valid() ==> childRepr < this.repr
            ensures Valid() ==> this.repr == {this} + childRepr
            decreases repr, childRepr
        {
            this.repr == {this} + childRepr && childRepr < this.repr
        }

        method Apply(tc: TestCase) returns (result: T)
            requires allocated(tc)
            requires this.Valid()
            requires tc.repr !! this.repr
            ensures this.repr == old(this.repr)
            requires tc.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            decreases repr, childRepr
            modifies tc, tc.random
        {
            var d0 := tc.depth;
            // At the depth budget, force the registry's base case (must be
            // non-recursive) so generation terminates; otherwise descend one level.
            var useBase := d0 >= registry.maxDepth;
            var k := if useBase then registry.baseKey else key;
            // Framing axioms: the named arbitrary exists and is well-formed, and a
            // registry built before this run cannot alias the fresh-per-run
            // TestCase. (Same style as RunTest's rng-disjointness assumption.)
            assume {:axiom} k in registry.arbs;
            var target := registry.arbs[k];
            assume {:axiom} target.Valid() && tc.repr !! target.internalFunction.repr;
            if !useBase {
                tc.depth := d0 + 1;
            }
            // KNOWN: "decreases clause might not decrease" here is expected — through
            // the registry this dispatches back into LazyArbitrary.Apply (the letrec
            // cycle), which the trait's repr-based metric can't reconcile. Termination
            // is enforced at runtime by the `depth`/maxDepth budget above. Future work.
            result := target.internalFunction.Apply(tc);
            tc.depth := d0;  // restore so siblings recurse to the same budget
        }
    }

    datatype Arbitrary<T> = Arbitrary(internalFunction: Transformable<T>) {


        ghost predicate Valid()
            reads internalFunction, internalFunction.repr
            decreases internalFunction.repr, internalFunction.childRepr
        {
            internalFunction.repr > internalFunction.childRepr &&
            // KNOWN: "decreases clause might not decrease" verification error here is expected.
            // Same root cause as Transformable.Apply above; expected to fail until the
            // termination proof is completed (future work).
            this.internalFunction.Valid()
        }

        method Apply(tc: TestCase) returns (result: T)
            requires tc.Valid()
            requires this.Valid()
            requires tc.repr !! this.internalFunction.repr
            ensures this.Valid()
            ensures tc.Valid()
            ensures tc.repr == old(tc.repr)
            ensures this.internalFunction.repr == old(this.internalFunction.repr)
            decreases this, internalFunction.repr, internalFunction.childRepr
            modifies tc, tc.random
        {
          result := this.internalFunction.Apply(tc);
        }

        method Map<U>(fn: T -> U) returns (p: Arbitrary<U>)
            requires Valid()
            ensures p.Valid()
            ensures p.internalFunction.repr == {p.internalFunction}+this.internalFunction.repr
            ensures fresh(p.internalFunction)
            ensures fresh(this.internalFunction.repr) ==> fresh(p.internalFunction.repr)
        {
          var mapTransformable := new MapTransformable(this, fn);
          p := Arbitrary(mapTransformable);
        }
        // 

        static method Of<T>(args: seq<T>) returns (p: Arbitrary<T>)
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
            requires 0 < |args| <= MaxChoice
        {
            var ofTransformable := new OfTransformable<T>(args);
            p := Arbitrary(ofTransformable);
        }

        static method Just<T>(value: T) returns (p: Arbitrary<T>)
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
        {
            var justTransformable := new JustTransformable<T>(value);
            p := Arbitrary(justTransformable);
        }

        static method Range(min: int, max: int) returns (p: Arbitrary<int>)
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
            requires min <= max && (0 < max- min < MaxChoice)
        {
            var rangeTransformable := new RangeTransformable(min, max);
            p := Arbitrary(rangeTransformable);
        }

        // static method Nothing<T>() returns (p: Arbitrary<T>)
        //     ensures fresh(p)
        // {
        //     var nothingTransformable := new NothingTransformable<T>();
        //     p := new Arbitrary<T>(nothingTransformable);
        // }

        static method Mix<T>(possibilities: seq<Arbitrary<T>>) returns (p: Arbitrary<T>)
            requires 0 < |possibilities| < MaxChoice
            requires forall i :: 0 <= i < |possibilities| ==> possibilities[i].Valid()
            requires forall x,y :: x in possibilities && y in possibilities && x != y ==> x.internalFunction.repr !! y.internalFunction.repr
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures forall x :: x in possibilities && fresh(x.internalFunction.repr) ==> fresh(p.internalFunction.repr)
        {
            var mixTransformable := new MixTransformable<T>(possibilities);
            p := Arbitrary(mixTransformable);
        }

        static method Bools() returns (p: Arbitrary<bool>)
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
        {
            var boolsTransformable := new BoolsTransformable();
            p := Arbitrary(boolsTransformable);
        }

        static method Nats(bound: nat) returns (p: Arbitrary<nat>)
            requires 0 < bound <= MaxChoice
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
        {
            var t := new NatsTransformable(bound);
            p := Arbitrary(t);
        }

        static method Chars() returns (p: Arbitrary<char>)
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
        {
            var t := new CharsTransformable();
            p := Arbitrary(t);
        }

        static method Reals() returns (p: Arbitrary<real>)
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
        {
            var t := new RealsTransformable();
            p := Arbitrary(t);
        }

        static method BitVectors1() returns (p: Arbitrary<bv1>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors1Transformable(); p := Arbitrary(t); }

        static method BitVectors2() returns (p: Arbitrary<bv2>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors2Transformable(); p := Arbitrary(t); }

        static method BitVectors8() returns (p: Arbitrary<bv8>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors8Transformable(); p := Arbitrary(t); }

        static method BitVectors16() returns (p: Arbitrary<bv16>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors16Transformable(); p := Arbitrary(t); }

        static method BitVectors32() returns (p: Arbitrary<bv32>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors32Transformable(); p := Arbitrary(t); }

        static method BitVectors64() returns (p: Arbitrary<bv64>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors64Transformable(); p := Arbitrary(t); }

        static method BitVectors128() returns (p: Arbitrary<bv128>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors128Transformable(); p := Arbitrary(t); }

        static method BitVectors256() returns (p: Arbitrary<bv256>)
            ensures p.Valid() ensures fresh(p.internalFunction) ensures fresh(p.internalFunction.repr)
        { var t := new BitVectors256Transformable(); p := Arbitrary(t); }

        // Collections derived from the verified Lists/Tuple/Map combinators —
        // generate a seq and project it to the target collection type.
        static method Sets<S(==)>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int)
            returns (p: Arbitrary<set<S>>)
            requires 0 <= minSize <= maxSize
            requires elementGenerator.Valid()
            ensures p.Valid()
        {
            var listGen := Lists<S>(elementGenerator, minSize, maxSize);
            p := listGen.Map((xs: seq<S>) => (set x | x in xs));
        }

        static method Multisets<S(==)>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int)
            returns (p: Arbitrary<multiset<S>>)
            requires 0 <= minSize <= maxSize
            requires elementGenerator.Valid()
            ensures p.Valid()
        {
            var listGen := Lists<S>(elementGenerator, minSize, maxSize);
            p := listGen.Map((xs: seq<S>) => multiset(xs));
        }

        static method Maps<K(==), V>(keyGen: Arbitrary<K>, valGen: Arbitrary<V>, minSize: int, maxSize: int)
            returns (p: Arbitrary<map<K, V>>)
            requires 0 <= minSize <= maxSize
            requires keyGen.Valid() && valGen.Valid()
            requires keyGen.internalFunction.repr !! valGen.internalFunction.repr
            ensures p.Valid()
        {
            var pairGen := Tuple<K, V>(keyGen, valGen);
            var listGen := Lists<(K, V)>(pairGen, minSize, maxSize);
            p := listGen.Map((pairs: seq<(K, V)>) => SeqToMap(pairs));
        }

        static method Arrays<S>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int)
            returns (p: Arbitrary<array<S>>)
            requires 0 <= minSize <= maxSize
            requires elementGenerator.Valid()
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(elementGenerator.internalFunction) ==> fresh(p.internalFunction.repr)
        {
            var t := new ArraysTransformable<S>(elementGenerator, minSize, maxSize);
            p := Arbitrary(t);
        }

        // Fixed-size 2-D array generator: every value is a rows x cols array2<S>.
        static method Array2<S>(elementGenerator: Arbitrary<S>, rows: nat, cols: nat)
            returns (p: Arbitrary<array2<S>>)
            requires elementGenerator.Valid()
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(elementGenerator.internalFunction) ==> fresh(p.internalFunction.repr)
        {
            var t := new Array2Transformable<S>(elementGenerator, rows, cols);
            p := Arbitrary(t);
        }

        // Fixed-size 3-D array generator: every value is a rows x cols x layers array3<S>.
        static method Array3<S>(elementGenerator: Arbitrary<S>, rows: nat, cols: nat, layers: nat)
            returns (p: Arbitrary<array3<S>>)
            requires elementGenerator.Valid()
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(elementGenerator.internalFunction) ==> fresh(p.internalFunction.repr)
        {
            var t := new Array3Transformable<S>(elementGenerator, rows, cols, layers);
            p := Arbitrary(t);
        }

        // (Recursive/letrec generators are provided via the Registry class above,
        // not as a static factory here — see Registry.Tie / Register / Lookup.)

        // n-tuple generators (3..10). Each is defined in terms of the previous
        // one: TupleN(a1..aN) = Map(Tuple(a1, Tuple{N-1}(a2..aN))) flattened.
        // Because the (N-1)-tuple is already flat, the flatten lambda is shallow.
        // Inputs must have pairwise-disjoint representation sets (typically true
        // for independently-built arbitraries). Each method exposes a small
        // repr-shape postcondition (inputs' reprs are contained, everything else
        // is fresh) so the next level can discharge Tuple's disjointness.
        static method Tuple3<A, B, C>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>)
            returns (p: Arbitrary<(A, B, C)>)
            requires a.Valid() && b.Valid() && c.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr
                    <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr))
        {
            var rest := Tuple<B, C>(b, c);
            var u := Tuple<A, (B, C)>(a, rest);
            p := u.Map((x: (A, (B, C))) => (x.0, x.1.0, x.1.1));
        }

        static method Tuple4<A, B, C, D>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>, d: Arbitrary<D>)
            returns (p: Arbitrary<(A, B, C, D)>)
            requires a.Valid() && b.Valid() && c.Valid() && d.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires a.internalFunction.repr !! d.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! d.internalFunction.repr
            requires c.internalFunction.repr !! d.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                    d.internalFunction.repr <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                     d.internalFunction.repr))
        {
            var rest := Tuple3<B, C, D>(b, c, d);
            var u := Tuple<A, (B, C, D)>(a, rest);
            p := u.Map((x: (A, (B, C, D))) => (x.0, x.1.0, x.1.1, x.1.2));
        }

        static method Tuple5<A, B, C, D, E>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>,
                d: Arbitrary<D>, e: Arbitrary<E>)
            returns (p: Arbitrary<(A, B, C, D, E)>)
            requires a.Valid() && b.Valid() && c.Valid() && d.Valid() && e.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires a.internalFunction.repr !! d.internalFunction.repr
            requires a.internalFunction.repr !! e.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! d.internalFunction.repr
            requires b.internalFunction.repr !! e.internalFunction.repr
            requires c.internalFunction.repr !! d.internalFunction.repr
            requires c.internalFunction.repr !! e.internalFunction.repr
            requires d.internalFunction.repr !! e.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                    d.internalFunction.repr + e.internalFunction.repr <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                     d.internalFunction.repr + e.internalFunction.repr))
        {
            var rest := Tuple4<B, C, D, E>(b, c, d, e);
            var u := Tuple<A, (B, C, D, E)>(a, rest);
            p := u.Map((x: (A, (B, C, D, E))) => (x.0, x.1.0, x.1.1, x.1.2, x.1.3));
        }

        static method Tuple6<A, B, C, D, E, F>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>,
                d: Arbitrary<D>, e: Arbitrary<E>, f: Arbitrary<F>)
            returns (p: Arbitrary<(A, B, C, D, E, F)>)
            requires a.Valid() && b.Valid() && c.Valid() && d.Valid() && e.Valid() && f.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires a.internalFunction.repr !! d.internalFunction.repr
            requires a.internalFunction.repr !! e.internalFunction.repr
            requires a.internalFunction.repr !! f.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! d.internalFunction.repr
            requires b.internalFunction.repr !! e.internalFunction.repr
            requires b.internalFunction.repr !! f.internalFunction.repr
            requires c.internalFunction.repr !! d.internalFunction.repr
            requires c.internalFunction.repr !! e.internalFunction.repr
            requires c.internalFunction.repr !! f.internalFunction.repr
            requires d.internalFunction.repr !! e.internalFunction.repr
            requires d.internalFunction.repr !! f.internalFunction.repr
            requires e.internalFunction.repr !! f.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                    d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr
                    <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                     d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr))
        {
            var rest := Tuple5<B, C, D, E, F>(b, c, d, e, f);
            var u := Tuple<A, (B, C, D, E, F)>(a, rest);
            p := u.Map((x: (A, (B, C, D, E, F))) => (x.0, x.1.0, x.1.1, x.1.2, x.1.3, x.1.4));
        }

        static method Tuple7<A, B, C, D, E, F, G>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>,
                d: Arbitrary<D>, e: Arbitrary<E>, f: Arbitrary<F>, g: Arbitrary<G>)
            returns (p: Arbitrary<(A, B, C, D, E, F, G)>)
            requires a.Valid() && b.Valid() && c.Valid() && d.Valid() && e.Valid() && f.Valid() && g.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires a.internalFunction.repr !! d.internalFunction.repr
            requires a.internalFunction.repr !! e.internalFunction.repr
            requires a.internalFunction.repr !! f.internalFunction.repr
            requires a.internalFunction.repr !! g.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! d.internalFunction.repr
            requires b.internalFunction.repr !! e.internalFunction.repr
            requires b.internalFunction.repr !! f.internalFunction.repr
            requires b.internalFunction.repr !! g.internalFunction.repr
            requires c.internalFunction.repr !! d.internalFunction.repr
            requires c.internalFunction.repr !! e.internalFunction.repr
            requires c.internalFunction.repr !! f.internalFunction.repr
            requires c.internalFunction.repr !! g.internalFunction.repr
            requires d.internalFunction.repr !! e.internalFunction.repr
            requires d.internalFunction.repr !! f.internalFunction.repr
            requires d.internalFunction.repr !! g.internalFunction.repr
            requires e.internalFunction.repr !! f.internalFunction.repr
            requires e.internalFunction.repr !! g.internalFunction.repr
            requires f.internalFunction.repr !! g.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                    d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                    g.internalFunction.repr <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                     d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                     g.internalFunction.repr))
        {
            var rest := Tuple6<B, C, D, E, F, G>(b, c, d, e, f, g);
            var u := Tuple<A, (B, C, D, E, F, G)>(a, rest);
            p := u.Map((x: (A, (B, C, D, E, F, G))) => (x.0, x.1.0, x.1.1, x.1.2, x.1.3, x.1.4, x.1.5));
        }

        static method Tuple8<A, B, C, D, E, F, G, H>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>,
                d: Arbitrary<D>, e: Arbitrary<E>, f: Arbitrary<F>, g: Arbitrary<G>, h: Arbitrary<H>)
            returns (p: Arbitrary<(A, B, C, D, E, F, G, H)>)
            requires a.Valid() && b.Valid() && c.Valid() && d.Valid() && e.Valid() && f.Valid() && g.Valid() && h.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires a.internalFunction.repr !! d.internalFunction.repr
            requires a.internalFunction.repr !! e.internalFunction.repr
            requires a.internalFunction.repr !! f.internalFunction.repr
            requires a.internalFunction.repr !! g.internalFunction.repr
            requires a.internalFunction.repr !! h.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! d.internalFunction.repr
            requires b.internalFunction.repr !! e.internalFunction.repr
            requires b.internalFunction.repr !! f.internalFunction.repr
            requires b.internalFunction.repr !! g.internalFunction.repr
            requires b.internalFunction.repr !! h.internalFunction.repr
            requires c.internalFunction.repr !! d.internalFunction.repr
            requires c.internalFunction.repr !! e.internalFunction.repr
            requires c.internalFunction.repr !! f.internalFunction.repr
            requires c.internalFunction.repr !! g.internalFunction.repr
            requires c.internalFunction.repr !! h.internalFunction.repr
            requires d.internalFunction.repr !! e.internalFunction.repr
            requires d.internalFunction.repr !! f.internalFunction.repr
            requires d.internalFunction.repr !! g.internalFunction.repr
            requires d.internalFunction.repr !! h.internalFunction.repr
            requires e.internalFunction.repr !! f.internalFunction.repr
            requires e.internalFunction.repr !! g.internalFunction.repr
            requires e.internalFunction.repr !! h.internalFunction.repr
            requires f.internalFunction.repr !! g.internalFunction.repr
            requires f.internalFunction.repr !! h.internalFunction.repr
            requires g.internalFunction.repr !! h.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                    d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                    g.internalFunction.repr + h.internalFunction.repr <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                     d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                     g.internalFunction.repr + h.internalFunction.repr))
        {
            var rest := Tuple7<B, C, D, E, F, G, H>(b, c, d, e, f, g, h);
            var u := Tuple<A, (B, C, D, E, F, G, H)>(a, rest);
            p := u.Map((x: (A, (B, C, D, E, F, G, H))) =>
                (x.0, x.1.0, x.1.1, x.1.2, x.1.3, x.1.4, x.1.5, x.1.6));
        }

        static method Tuple9<A, B, C, D, E, F, G, H, I>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>,
                d: Arbitrary<D>, e: Arbitrary<E>, f: Arbitrary<F>, g: Arbitrary<G>, h: Arbitrary<H>, i: Arbitrary<I>)
            returns (p: Arbitrary<(A, B, C, D, E, F, G, H, I)>)
            requires a.Valid() && b.Valid() && c.Valid() && d.Valid() && e.Valid() && f.Valid() && g.Valid() && h.Valid() && i.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires a.internalFunction.repr !! d.internalFunction.repr
            requires a.internalFunction.repr !! e.internalFunction.repr
            requires a.internalFunction.repr !! f.internalFunction.repr
            requires a.internalFunction.repr !! g.internalFunction.repr
            requires a.internalFunction.repr !! h.internalFunction.repr
            requires a.internalFunction.repr !! i.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! d.internalFunction.repr
            requires b.internalFunction.repr !! e.internalFunction.repr
            requires b.internalFunction.repr !! f.internalFunction.repr
            requires b.internalFunction.repr !! g.internalFunction.repr
            requires b.internalFunction.repr !! h.internalFunction.repr
            requires b.internalFunction.repr !! i.internalFunction.repr
            requires c.internalFunction.repr !! d.internalFunction.repr
            requires c.internalFunction.repr !! e.internalFunction.repr
            requires c.internalFunction.repr !! f.internalFunction.repr
            requires c.internalFunction.repr !! g.internalFunction.repr
            requires c.internalFunction.repr !! h.internalFunction.repr
            requires c.internalFunction.repr !! i.internalFunction.repr
            requires d.internalFunction.repr !! e.internalFunction.repr
            requires d.internalFunction.repr !! f.internalFunction.repr
            requires d.internalFunction.repr !! g.internalFunction.repr
            requires d.internalFunction.repr !! h.internalFunction.repr
            requires d.internalFunction.repr !! i.internalFunction.repr
            requires e.internalFunction.repr !! f.internalFunction.repr
            requires e.internalFunction.repr !! g.internalFunction.repr
            requires e.internalFunction.repr !! h.internalFunction.repr
            requires e.internalFunction.repr !! i.internalFunction.repr
            requires f.internalFunction.repr !! g.internalFunction.repr
            requires f.internalFunction.repr !! h.internalFunction.repr
            requires f.internalFunction.repr !! i.internalFunction.repr
            requires g.internalFunction.repr !! h.internalFunction.repr
            requires g.internalFunction.repr !! i.internalFunction.repr
            requires h.internalFunction.repr !! i.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                    d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                    g.internalFunction.repr + h.internalFunction.repr + i.internalFunction.repr
                    <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                     d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                     g.internalFunction.repr + h.internalFunction.repr + i.internalFunction.repr))
        {
            var rest := Tuple8<B, C, D, E, F, G, H, I>(b, c, d, e, f, g, h, i);
            var u := Tuple<A, (B, C, D, E, F, G, H, I)>(a, rest);
            p := u.Map((x: (A, (B, C, D, E, F, G, H, I))) =>
                (x.0, x.1.0, x.1.1, x.1.2, x.1.3, x.1.4, x.1.5, x.1.6, x.1.7));
        }

        static method Tuple10<A, B, C, D, E, F, G, H, I, J>(a: Arbitrary<A>, b: Arbitrary<B>, c: Arbitrary<C>,
                d: Arbitrary<D>, e: Arbitrary<E>, f: Arbitrary<F>, g: Arbitrary<G>, h: Arbitrary<H>,
                i: Arbitrary<I>, j: Arbitrary<J>)
            returns (p: Arbitrary<(A, B, C, D, E, F, G, H, I, J)>)
            requires a.Valid() && b.Valid() && c.Valid() && d.Valid() && e.Valid() && f.Valid() && g.Valid() && h.Valid() && i.Valid() && j.Valid()
            requires a.internalFunction.repr !! b.internalFunction.repr
            requires a.internalFunction.repr !! c.internalFunction.repr
            requires a.internalFunction.repr !! d.internalFunction.repr
            requires a.internalFunction.repr !! e.internalFunction.repr
            requires a.internalFunction.repr !! f.internalFunction.repr
            requires a.internalFunction.repr !! g.internalFunction.repr
            requires a.internalFunction.repr !! h.internalFunction.repr
            requires a.internalFunction.repr !! i.internalFunction.repr
            requires a.internalFunction.repr !! j.internalFunction.repr
            requires b.internalFunction.repr !! c.internalFunction.repr
            requires b.internalFunction.repr !! d.internalFunction.repr
            requires b.internalFunction.repr !! e.internalFunction.repr
            requires b.internalFunction.repr !! f.internalFunction.repr
            requires b.internalFunction.repr !! g.internalFunction.repr
            requires b.internalFunction.repr !! h.internalFunction.repr
            requires b.internalFunction.repr !! i.internalFunction.repr
            requires b.internalFunction.repr !! j.internalFunction.repr
            requires c.internalFunction.repr !! d.internalFunction.repr
            requires c.internalFunction.repr !! e.internalFunction.repr
            requires c.internalFunction.repr !! f.internalFunction.repr
            requires c.internalFunction.repr !! g.internalFunction.repr
            requires c.internalFunction.repr !! h.internalFunction.repr
            requires c.internalFunction.repr !! i.internalFunction.repr
            requires c.internalFunction.repr !! j.internalFunction.repr
            requires d.internalFunction.repr !! e.internalFunction.repr
            requires d.internalFunction.repr !! f.internalFunction.repr
            requires d.internalFunction.repr !! g.internalFunction.repr
            requires d.internalFunction.repr !! h.internalFunction.repr
            requires d.internalFunction.repr !! i.internalFunction.repr
            requires d.internalFunction.repr !! j.internalFunction.repr
            requires e.internalFunction.repr !! f.internalFunction.repr
            requires e.internalFunction.repr !! g.internalFunction.repr
            requires e.internalFunction.repr !! h.internalFunction.repr
            requires e.internalFunction.repr !! i.internalFunction.repr
            requires e.internalFunction.repr !! j.internalFunction.repr
            requires f.internalFunction.repr !! g.internalFunction.repr
            requires f.internalFunction.repr !! h.internalFunction.repr
            requires f.internalFunction.repr !! i.internalFunction.repr
            requires f.internalFunction.repr !! j.internalFunction.repr
            requires g.internalFunction.repr !! h.internalFunction.repr
            requires g.internalFunction.repr !! i.internalFunction.repr
            requires g.internalFunction.repr !! j.internalFunction.repr
            requires h.internalFunction.repr !! i.internalFunction.repr
            requires h.internalFunction.repr !! j.internalFunction.repr
            requires i.internalFunction.repr !! j.internalFunction.repr
            ensures p.Valid()
            ensures a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                    d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                    g.internalFunction.repr + h.internalFunction.repr + i.internalFunction.repr +
                    j.internalFunction.repr <= p.internalFunction.repr
            ensures fresh(p.internalFunction.repr -
                    (a.internalFunction.repr + b.internalFunction.repr + c.internalFunction.repr +
                     d.internalFunction.repr + e.internalFunction.repr + f.internalFunction.repr +
                     g.internalFunction.repr + h.internalFunction.repr + i.internalFunction.repr +
                     j.internalFunction.repr))
        {
            var rest := Tuple9<B, C, D, E, F, G, H, I, J>(b, c, d, e, f, g, h, i, j);
            var u := Tuple<A, (B, C, D, E, F, G, H, I, J)>(a, rest);
            p := u.Map((x: (A, (B, C, D, E, F, G, H, I, J))) =>
                (x.0, x.1.0, x.1.1, x.1.2, x.1.3, x.1.4, x.1.5, x.1.6, x.1.7, x.1.8));
        }

        static method Lists<S>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int) returns (p: Arbitrary<seq<S>>)
            requires 0 <= minSize <= maxSize
            requires elementGenerator.Valid()
            ensures p.Valid()
            ensures p.internalFunction.repr == {p.internalFunction}+elementGenerator.internalFunction.repr
            ensures fresh(p.internalFunction)
            // ensures fresh(p.internalFunction.repr)
            ensures fresh(elementGenerator.internalFunction) ==>  fresh(p.internalFunction.repr)
        {
            var listsTransformable := new ListsTransformable<S>(elementGenerator, minSize, maxSize);
            p := Arbitrary(listsTransformable);
        }

        static method Strings(minLength: int, maxLength: int, ascii: bool) returns (p: Arbitrary<string>)
            requires 0 <= minLength <= maxLength
            ensures p.Valid()
            ensures fresh(p.internalFunction)
            ensures fresh(p.internalFunction.repr)
        {
            var stringsTransformable := new StringsTransformable(minLength, maxLength, ascii);
            p := Arbitrary(stringsTransformable);
        }

        static method Tuple<T, U>(firstGenerator: Arbitrary<T>, secondGenerator: Arbitrary<U>) returns (p: Arbitrary<(T, U)>)
            requires firstGenerator.Valid()
            requires secondGenerator.Valid()
            requires firstGenerator.internalFunction.repr !! secondGenerator.internalFunction.repr
            ensures p.Valid()
            ensures p.internalFunction.repr == {p.internalFunction} + firstGenerator.internalFunction.repr + secondGenerator.internalFunction.repr
            ensures fresh(p.internalFunction)
            ensures fresh(firstGenerator.internalFunction) && fresh(secondGenerator.internalFunction) ==> fresh(p.internalFunction.repr)
        {
            var tupleTransformable := new TupleTransformable<T, U>(firstGenerator, secondGenerator);
            p := Arbitrary(tupleTransformable);
        }

        method FlatMap<U>(f: FlatMapFn<T, U>) returns (p: Arbitrary<U>)
            requires Valid()
            requires f !in this.internalFunction.repr
            ensures p.Valid()
            ensures p.internalFunction.repr == {p.internalFunction}+this.internalFunction.repr+{f}
            ensures fresh(p.internalFunction)
            ensures fresh(this.internalFunction.repr) ==> fresh(p.internalFunction.repr)
        {
            // We need a new Transformable that knows how to do the FlatMap logic.
            var flatMapTransformable := new FlatMapTransformable(this, f);
            p := Arbitrary(flatMapTransformable);
        }

    }
}
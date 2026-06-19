# `Arbitrary.dfy` — generators (module `Arbitrary`)

The generator library. An `Arbitrary<T>` produces values of type `T` from a `TestCase`'s
random/choice stream, and is the value you hand to the run methods in
[`DafnyCheck.dfy`](DafnyCheck.md). Almost all use goes through the **static factory methods**
and the **combinators** below; you only need the `Transformable` trait if you are writing a
genuinely custom generator.

```dafny
datatype Arbitrary<T> = Arbitrary(internalFunction: Transformable<T>)
```

## Primitive generators (static methods on `Arbitrary`)

```dafny
static method Bools() returns (p: Arbitrary<bool>)
static method Nats(bound: nat) returns (p: Arbitrary<nat>)            // [0, bound), requires 0 < bound <= MaxLong
static method Range(min: int, max: int) returns (p: Arbitrary<int>)  // [min, max), requires min <= max && 0 < max-min < MaxLong
static method Chars() returns (p: Arbitrary<char>)                   // printable ASCII [32,127)
static method Reals() returns (p: Arbitrary<real>)                   // non-negative rationals
static method Just<T>(value: T) returns (p: Arbitrary<T>)            // constant
static method Of<T>(args: seq<T>) returns (p: Arbitrary<T>)          // pick one of args, requires 0 < |args| <= MaxLong
static method Strings(minLength: int, maxLength: int, ascii: bool) returns (p: Arbitrary<string>)

// Fixed-width bit vectors:
static method BitVectors1()  returns (p: Arbitrary<bv1>)
static method BitVectors2()  returns (p: Arbitrary<bv2>)
static method BitVectors8()  returns (p: Arbitrary<bv8>)
static method BitVectors16() returns (p: Arbitrary<bv16>)
static method BitVectors32() returns (p: Arbitrary<bv32>)
static method BitVectors64() returns (p: Arbitrary<bv64>)
static method BitVectors128() returns (p: Arbitrary<bv128>)
static method BitVectors256() returns (p: Arbitrary<bv256>)
```

## Collection generators

```dafny
static method Lists<S>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int) returns (p: Arbitrary<seq<S>>)
static method Sets<S(==)>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int) returns (p: Arbitrary<set<S>>)
static method Multisets<S(==)>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int) returns (p: Arbitrary<multiset<S>>)
static method Maps<K(==), V>(keyGen: Arbitrary<K>, valGen: Arbitrary<V>, minSize: int, maxSize: int) returns (p: Arbitrary<map<K, V>>)

// Heap arrays. 1-D is sized like Lists; 2-D/3-D are FIXED size (rows/cols/layers).
static method Arrays<S>(elementGenerator: Arbitrary<S>, minSize: int, maxSize: int) returns (p: Arbitrary<array<S>>)
static method Array2<S>(elementGenerator: Arbitrary<S>, rows: nat, cols: nat) returns (p: Arbitrary<array2<S>>)
static method Array3<S>(elementGenerator: Arbitrary<S>, rows: nat, cols: nat, layers: nat) returns (p: Arbitrary<array3<S>>)
```

## Tuples

```dafny
static method Tuple<T, U>(firstGenerator: Arbitrary<T>, secondGenerator: Arbitrary<U>) returns (p: Arbitrary<(T, U)>)
static method Tuple3<A, B, C>(a, b, c) returns (p: Arbitrary<(A, B, C)>)
// ... Tuple4 ... Tuple10, each taking N independently-built generators.
```

`Tuple`/`Tuple3..Tuple10` require their argument generators to have **pairwise-disjoint
representation sets** — independently-built generators satisfy this automatically.

## Combinators (instance methods)

```dafny
method Map<U>(fn: T -> U) returns (p: Arbitrary<U>)          // transform generated values
method FlatMap<U>(f: FlatMapFn<T, U>) returns (p: Arbitrary<U>)
static method Mix<T>(possibilities: seq<Arbitrary<T>>) returns (p: Arbitrary<T>)  // pick one generator (oneof)
```

`Mix` requires the possibilities to have pairwise-disjoint reprs (distinct independently-built
generators, or distinct `Registry.Tie` nodes, satisfy this). `FlatMap` takes a
`trait FlatMapFn<T, U> { method CreateArbitrary(t: T) returns (p: Arbitrary<U>) }`.

## Recursive generators / letrec — `Registry<T>`

fast-check-style `letrec` for recursive datatypes. Build each named arbitrary, writing
`reg.Tie(name)` at recursive positions, `Register` them, then `Lookup` the entry point. The
`TestCase` depth budget (`maxDepth`) falls back to the registered base case so generation
terminates. See [`../test/RecursiveTest.dfy`](../test/RecursiveTest.dfy) and
[`../test/WeirdTreeTest.dfy`](../test/WeirdTreeTest.dfy).

```dafny
class Registry<T(!new)> {
  constructor(baseKey: string, maxDepth: nat)
  method Register(key: string, arb: Arbitrary<T>)        // bind/replace a named arbitrary
  method Tie(key: string) returns (a: Arbitrary<T>)      // lazy reference to a named arbitrary
  function Lookup(key: string): Arbitrary<T>             // requires key in arbs
}
```

```dafny
var reg := new Registry<Tree>("Leaf", 4);          // base-case key + max depth
var elem := reg.Tie("Tree");
var nodes := Arbitrary<Tree>.Lists(elem, 0, 3);
var nodeArb := nodes.Map((ks) => Node(ks));
reg.Register("Leaf", leafArb);
reg.Register("Node", nodeArb);
var tieL := reg.Tie("Leaf"); var tieN := reg.Tie("Node");
reg.Register("Tree", Arbitrary<Tree>.Mix([tieL, tieN]));
var tree := reg.Lookup("Tree");
```

## Drawing values — `TestCase`

You rarely construct a `TestCase` directly except in unit tests of a generator; the run
methods build one for you. Its primitives, used by `Transformable.Apply`:

```dafny
class TestCase {
  constructor(prefix: seq<bv64>, random: XoroShift128Plus, maxSize: nat, printResults: bool)
  static method ForChoices(choices: seq<bv64>, seed: bv64, printResults: bool) returns (tc: TestCase)
  method MakeChoice(n: bv64) returns (result: TestResult<bv64>)        // a draw in [0, n)
  method ForcedChoice(n: bv64) returns (result: TestResult<bv64>)      // force the next choice to n
  method WeightedInternal(p: real) returns (result: TestResult<bool>)  // weighted coin, requires 0.0 <= p <= 1.0
}
```

## Writing a custom generator — the `Transformable` trait

Only needed when no combination of the above suffices. Implement the trait, modeling on the
simplest existing leaf generator (`BoolsTransformable`), then wrap it with `Arbitrary(yourTransformable)`:

```dafny
trait Transformable<T> {
  ghost var repr: set<object>
  ghost var childRepr: set<object>
  ghost predicate Valid()
    reads this, repr, childRepr
    ensures Valid() ==> this in repr
    ensures Valid() ==> childRepr < this.repr
    ensures Valid() ==> this.repr == {this} + childRepr
  method Apply(tc: TestCase) returns (result: T)
    requires tc.Valid() && this.Valid() && tc.repr !! this.repr
    ensures this.Valid() && tc.Valid() && tc.repr == old(tc.repr) && this.repr == old(this.repr)
    modifies tc, tc.random
}
```

`Valid()`/`repr`/`childRepr` track a generator's object graph so the engine can keep it disjoint
from the `TestCase` it draws from; a leaf generator uses `childRepr == {}`, `repr == {this}`.

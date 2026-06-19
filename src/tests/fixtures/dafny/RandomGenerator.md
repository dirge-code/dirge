# `RandomGenerator.dfy` — PRNG (module `RandomGenerator`)

The xoroshiro128+ pseudo-random generator backing every `TestCase`. The engine wires this up for
you; the only piece you touch directly is `fromSeed`, when constructing a `TestCase` in a
generator unit test.

```dafny
class XoroShift128Plus {
  constructor(s0: bv64, s1: bv64)
  static method fromSeed(seed: bv64) returns (c: XoroShift128Plus)   // seed a fresh generator

  method unsafeNext() returns (result: bv64)        // next 64-bit value, mutating state
  method next() returns (out: bv64, c: XoroShift128Plus)             // immutable: value + advanced clone
  method unsafeNextReal() returns (out: real)        // next value in [0, 1), mutating state
  method nextReal() returns (out: real, c: XoroShift128Plus)
  method clone() returns (c: XoroShift128Plus)        // copy with identical state
  method unsafeJump()                                  // advance 2^64 steps (mutating)
  method jump() returns (c: XoroShift128Plus)          // advanced clone (immutable)
}
```

`fromSeed` is what tests use:

```dafny
var rng := XoroShift128Plus.fromSeed(42);
var tc := new TestCase([], rng, 100, true);
var v := myArb.Apply(tc);
```

(The `RandomGen` trait / `SimpleRandomGen` wrapper used by the run engine live in
[`DafnyCheck.dfy`](DafnyCheck.md), not here.)

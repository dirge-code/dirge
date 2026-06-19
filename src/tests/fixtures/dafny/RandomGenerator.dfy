
// A multiplicative/linear-congruential PRNG whose entire state evolves with only
// `+`, `*`, and `%` over a native {:nativeType "number"} newtype. On the Dafny JS
// backend that lowers to plain IEEE-double arithmetic (no BigNumber). The previous
// xoroshiro128+ implementation used bitvector xor/shift/rotate, and Dafny lowers
// *every* bvN to a BigNumber on JS, so each draw cost a handful of BigNumber ops —
// that was the JS PBT bottleneck. The hot-path draw (unsafeNextChoice) now returns a
// native `Choice` (uint32) with ZERO BigNumber; bv64/real survive only in the legacy
// unsafeNext/unsafeNextReal helpers, kept off every hot path.
//
// Original reference (now superseded): https://prng.di.unimi.it/xoroshiro128plus.c
module RandomGenerator {

    // Native lane. Bounded < 2^53 so any product of two reduced lanes is exactly
    // representable as a double. The intermediate s*48271 reaches ~2^47, which does
    // NOT fit 32 bits — that is why the STATE lane is U53, not Choice.
    // "number" is the native JS double (what we want for fast PBT); "long" is its
    // C#/JVM/Go equivalent (64-bit, comfortably holds < 2^53) so the SAME source still
    // verifies and compiles on the default (C#) backend. Dafny picks per target.
    newtype {:nativeType "number", "long"} U53 = x: int | 0 <= x < 0x20_0000_0000_0000

    // The public draw/choice type threaded through the PBT engine's HOT path
    // (TestCase.choices/prefix, MakeChoice, TestResult, the shrinker, reporting).
    // A 32-bit unsigned native int: "number" (a plain double, no BigNumber) on JS,
    // "uint" on C#. Every lane output is < MOD (< 2^31) so it always fits exactly.
    newtype {:nativeType "number", "uint"} Choice = x: int | 0 <= x < 0x1_0000_0000

    const MaxChoice: int := 0xFFFFFFFF   // largest int representable as a Choice

    const MOD:  U53 := 2147483647   // 2^31 - 1, prime
    const MULT: U53 := 48271        // Park-Miller "minstd" multiplier
    const INC0: U53 := 12345        // distinct increments decorrelate the two lanes
    const INC1: U53 := 6789

    // One LCG step. Reduces its argument first, so it is TOTAL and the product never
    // overflows 2^53: (s % MOD) < 2^31, times 48271 (<2^16), plus inc (<2^16) < 2^47.
    // The modulus/multiplier are written as literals (not the named consts) so Z3 sees
    // `x * 48271` as linear rather than a symbolic-times-symbolic nonlinear product.
    function lcg(s: U53, inc: U53): U53
        requires 0 <= inc < 0x10000
        ensures 0 <= lcg(s, inc) < MOD
    {
        ((s % 2147483647) * 48271 + inc) % 2147483647
    }

    class XoroShift128Plus {
        var s0: U53
        var s1: U53

        constructor(s0: U53, s1: U53)
            ensures fresh(this)
            ensures this.s0 == s0 && this.s1 == s1
        {
            this.s0 := s0;
            this.s1 := s1;
        }

        method clone() returns (c: XoroShift128Plus)
            ensures fresh(c)
        {
            c := new XoroShift128Plus(s0, s1);
        }

        // THE hot-path draw: advance both lanes and return the native Choice directly.
        // s0 < MOD < 2^31 < 2^32, so the U53 -> Choice narrowing is always in range,
        // and on JS it is a no-op between two `number`s (no BigNumber anywhere).
        method unsafeNextChoice() returns (c: Choice)
            modifies this
        {
            s0 := lcg(s0, INC0);
            s1 := lcg(s1, INC1);
            c := (s0 as Choice);
        }

        // Legacy bv64 draw (kept for API completeness; off every hot path now).
        method unsafeNext() returns (result: bv64)
            modifies this
        {
            s0 := lcg(s0, INC0);
            s1 := lcg(s1, INC1);
            // s0 < MOD < 2^31 < 2^64, so the conversion is always in range.
            result := (s0 as bv64);
        }

        method next() returns (out: bv64, c: XoroShift128Plus)
            ensures fresh(c)
            ensures unchanged(this)
        {
            c := new XoroShift128Plus(s0, s1);
            out := c.unsafeNext();
        }

        // Hot path (Weighted, booleans): stays entirely in native `number` — no bv64.
        method unsafeNextReal() returns (out: real)
            modifies this
        {
            s0 := lcg(s0, INC0);
            s1 := lcg(s1, INC1);
            out := (s0 as real) / (MOD as real);   // in [0, 1)
        }

        method nextReal() returns (out: real, c: XoroShift128Plus)
            ensures fresh(c)
            ensures unchanged(this)
        {
            c := new XoroShift128Plus(s0, s1);
            out := c.unsafeNextReal();
        }

        // Advance the stream far enough to give a decorrelated subsequence. The old
        // xoroshiro jump polynomial relied on bitvector xor; an MCG has no cheap
        // closed-form jump, so we just step a fixed large number of times. This is
        // not on any hot path (no caller in Arbitrary/DafnyCheck uses jump()).
        method unsafeJump()
            modifies this
        {
            for i: int := 0 to 1009 {
                s0 := lcg(s0, INC0);
                s1 := lcg(s1, INC1);
            }
        }

        method jump() returns (c: XoroShift128Plus)
            ensures fresh(c)
            ensures unchanged(this)
        {
            c := new XoroShift128Plus(s0, s1);
            c.unsafeJump();
        }

        // Bridge the bv64 seed API to native lanes. The bv64->int conversion happens
        // exactly once at construction, so its BigNumber cost is irrelevant.
        static method fromSeed(seed: bv64) returns (c: XoroShift128Plus)
            ensures fresh(c)
        {
            var raw: int := (seed as int) % 2147483647;   // [0, MOD)
            var a: U53 := raw as U53;
            var b: U53 := lcg(a, INC1);                    // distinct second lane
            c := new XoroShift128Plus(a, b);
        }
    }

    method {:test} reals() {
        var test := XoroShift128Plus.fromSeed(42);
        var te, re := test.nextReal();
        print(te);
        print("\n");
        print(te < 0.9);
        print("\n");
    }
}

// ============================================================================
// SeedSource — a non-deterministic 64-bit seed via foreign-function interface.
//
// Dafny's executable subset is deterministic (the havoc operator `*` and
// unconstrained `:|` compile to a fixed default, not runtime entropy). To get a
// genuinely random per-run seed — e.g. to drive property-based testing from a
// fresh stream each run — we declare an `{:extern}` method and supply a tiny
// native implementation for every Dafny backend.
//
// `GetSeed()` returns a `bv64` so it can feed RandomGenerator / TestingState
// directly. The module is marked `{:extern "SeedSource"}` so its compiled name
// is stable across backends; the method is `{:extern}` (no body) so each backend
// links to the native function in this folder:
//
//   backend   native file                        compiled symbol
//   -------   --------------------------------   --------------------------------
//   C#        SeedSource.cs                       SeedSource.__default.GetSeed : ulong
//   Java      java/SeedSource/__default.java      SeedSource.__default.GetSeed : long
//   Go        SeedSource.go                       SeedSource.GetSeed          : uint64
//   Python    SeedSource.py                       SeedSource.default__.GetSeed: int
//   JavaScript SeedSource.js                      SeedSource.GetSeed          : BigInt
//
// Pass the matching file with `--input` when you build/run (see README.md).
// Because GetSeed has no Dafny body, code that *uses* it still verifies; only
// compilation/execution needs the native file for the chosen target.
// ============================================================================
module {:extern "SeedSource"} SeedSource {
  // A fresh, non-deterministic 64-bit value drawn from a platform entropy source.
  method {:extern} GetSeed() returns (s: bv64)
}

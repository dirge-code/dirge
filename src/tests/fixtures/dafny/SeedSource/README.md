# `SeedSource` — non-deterministic seeds via FFI

Dafny's executable subset is deterministic: the havoc operator `*` and an
unconstrained `:|` compile to a fixed default (`0`), not runtime entropy. So to
seed property-based tests (or anything) from a *fresh stream each run*, we expose
one `{:extern}` method and implement it natively for every Dafny backend.

```dafny
module {:extern "SeedSource"} SeedSource {
  method {:extern} GetSeed() returns (s: bv64)   // fresh 64-bit value from a platform CSPRNG
}
```

Code that *uses* `GetSeed()` still **verifies** without any native file (the
method has no body). Only **compiling/running** for a target needs that target's
native file, passed with `--input`.

## Files & per-target build/run

`SeedSourceDemo.dfy` prints two seeds. Run it from this directory:

| Target | Native file | Command |
|---|---|---|
| C# | `SeedSource.cs` | `dafny run --target:cs SeedSourceDemo.dfy --input SeedSource.cs` |
| Python | `SeedSource.py` | `dafny run --target:py SeedSourceDemo.dfy --input SeedSource.py` |
| Go | `SeedSource.go` | `dafny run --target:go SeedSourceDemo.dfy --input SeedSource.go` |
| JavaScript | `SeedSource.js` | `dafny run --target:js SeedSourceDemo.dfy --input SeedSource.js` |
| Java | `java/SeedSource/__default.java` | `dafny run --target:java SeedSourceDemo.dfy --input java/SeedSource/__default.java` |

All five have been verified to produce a fresh random 64-bit value per call.

### How the native symbol is named on each backend

`bv64` maps to the backend's natural 64-bit type, and the module/method compile
to these symbols (which is why the native files look the way they do):

| Target | Symbol | Return type |
|---|---|---|
| C# | `SeedSource.__default.GetSeed` | `ulong` |
| Java | `SeedSource.__default.GetSeed` | `long` |
| Go | `SeedSource.GetSeed` | `uint64` |
| Python | `SeedSource.default__.GetSeed` | `int` (64-bit) |
| JavaScript | `SeedSource.GetSeed` | `BigInt` |

## Toolchain notes

- **Java:** the file must be named `__default.java` inside a `SeedSource/`
  package directory (hence `java/SeedSource/__default.java`). Keep `Main` in the
  *default* module (as `SeedSourceDemo.dfy` does), not a module named after the
  file — otherwise the Java launcher class collides with a same-named package.
- **Go:** Dafny formats Go output with `goimports`; install it and put it on
  PATH: `go install golang.org/x/tools/cmd/goimports@latest`.
- **JavaScript:** Dafny's JS runtime needs `bignumber.js`
  (`npm install bignumber.js`) resolvable from the run directory.

## Using it in property-based tests

`SeededTesting.dfy` wires `GetSeed` into the predicate-test runners so you don't
have to thread the seed yourself:

```dafny
import opened SeededTesting
...
var ok := RunTestRandom(pred, arb, name);                       // fresh seed each run
var ok := RunTestRandomWithExamples(pred, arb, name, 500);      // + run count
var ok := RunTestRandomWithConfig(pred, arb, name, cfg);        // fills cfg.seed only if None
```

`SeededDemo.dfy` is a runnable example (`dafny run --standard-libraries
--target:cs SeededDemo.dfy --input SeedSource.cs`); run it twice to see a fresh
seed each time.

This lives in its own module on purpose: because `GetSeed` is `{:extern}`, any
module that references it forces the native file at compile time for *every*
consumer. Keeping the wiring in `SeededTesting` means the core `DafnyCheck`
runners stay extern-free — only tests that import `SeededTesting` need a native
file. (Verification needs nothing extra; only compile/run does.)

For **model-based** tests, the runner (`RunModelTestWithConfig`) lives in the
abstract `StatefulModelTest` module, so wiring `GetSeed` in there would force the
native file on every refinement. Instead, seed a model test from its refinement:

```dafny
import opened SeedSource
...
var seed := GetSeed();
var ok := RunModelTestWithConfig(name, cmds, prop, maxSteps, 100, seed, true, Low);
```

Trade-off either way: runs become **non-reproducible**. The case-study tests
deliberately use a fixed `SEEDS` list so failures reproduce; reach for
`GetSeed()` when you specifically want fresh entropy each run.

> Build outputs (`SeedSourceDemo-*`, `node_modules/`, etc.) are throwaway and
> should not be committed.

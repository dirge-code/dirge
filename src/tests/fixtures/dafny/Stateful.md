# `Stateful.dfy` — model-based testing (abstract module `StatefulModelTest`)

Stateful / model-based property testing via **module refinement**. You write a test by `refines`-ing
the abstract `StatefulModelTest` module: you supply the concrete `Model`, system-under-test `SUT`,
and command `Cmd` types plus the operations over them, and you inherit the whole runner. The engine
generates command sequences, drives them against a fresh (possibly mutable, heap) system, samples it
into an immutable model after each command, and checks an [LTL temporal property](LTL.md) over the
model trace — all on top of the [`DafnyCheck`](DafnyCheck.md) engine (generation, shrinking, seeding,
reporting). It is the Dafny analogue of
[`@fast-check/LTL` (LTLTS)](https://github.com/hath995/LTLTS)'s `temporalModelRun`.

## Why refinement (and no casts, no test damage)

A *stateful* test drives a **mutable** SUT in place. Mutating a heap object needs a `modifies`
clause, and `modifies` requires a *reference*. Neither a generic **type parameter** nor an abstract
**type** is assignable to `object` in Dafny, so `modifies sys` / `sys in repr` are type errors for
both — you cannot frame a generic SUT's heap directly.

Module refinement sidesteps this: in the *refining* module the deferred `type SUT` becomes a
**concrete class**, so command bodies call its methods directly — **no downcasts**. The only thing
that must cross the abstract/concrete boundary is the SUT's heap footprint, and that is threaded as
an explicit ghost `set<object>` (`repr`), which *is* allowed in `modifies`/`reads`. The result:

- the production class stays **completely clean** — it extends nothing and has no testing concepts;
- commands are a plain **datatype** (no per-command classes, no `repr`/`Valid` boilerplate);
- the runner (`Apply` loop, `RunModelTest`) is written **once** in the abstract module.

## The abstract module — what you refine

```dafny
abstract module StatefulModelTest {
  type Model(!new)            // immutable observation of the system
  type SUT                    // the system under test (refined to a concrete class)
  type Cmd(!new)              // the command alphabet (refined to a datatype)

  // The SUT's heap footprint is an explicit ghost set, so the abstract `SUT`
  // never appears in a reads/modifies clause.
  ghost predicate ValidSUT(s: SUT, repr: set<object>) reads repr
  function InitialModel(): Model

  method MakeSUT() returns (s: SUT, ghost repr: set<object>)
    ensures ValidSUT(s, repr) ensures fresh(repr)
  // `prev` (model before this command) and `cmd` let Sample build a *transition*
  // model — e.g. carry the previous observation + an event for `cmd`.
  method Sample(prev: Model, cmd: Cmd, s: SUT, ghost repr: set<object>) returns (m: Model)
    requires ValidSUT(s, repr) ensures ValidSUT(s, repr)
  predicate Check(cmd: Cmd, m: Model)                       // precondition over the model
  method RunCmd(cmd: Cmd, m: Model, s: SUT, ghost repr: set<object>)
    returns (ghost repr': set<object>)                      // footprint may grow
    requires Check(cmd, m) requires ValidSUT(s, repr)
    modifies repr
    ensures ValidSUT(s, repr') ensures repr <= repr' ensures fresh(repr' - repr)
  function CmdString(cmd: Cmd): string                      // label for the failure trace

  // ... inherited runner ...
  method RunModelTest(name: string, cmds: Arbitrary<Cmd>, ltlProperty: LTLFormula<Model>, maxSteps: nat)
    returns (passed: bool)
    requires cmds.Valid() && WellFormedFormula(ltlProperty) && 0 < maxSteps
  method RunModelTestWithConfig(name, cmds, ltlProperty, maxSteps,
    numRuns: nat, seed: bv64, useColor: bool, verbosity: Verbosity) returns (passed: bool)
}
```

The runner repeats, for each generated command sequence:

```
s, repr := MakeSUT()                  // fresh SUT + its footprint
m := InitialModel()
for each generated cmd, up to maxSteps:
    if Check(cmd, m):                     // precondition over the model; if false, skip
        repr := RunCmd(cmd, m, s, repr)   // *** mutate the SUT in place (footprint may grow) ***
        m := Sample(m, cmd, s, repr)      // project the mutated system into the next model
        step the LTL formula on m
    stop early once the formula is determined
```

The model is **sampled out of the system**, not computed by the command — so a command can't lie
about its effect. `Sample` receives the command so the model can carry an **event** for the
transition that produced it (e.g. `Enqueued(v)` / `Dequeued`). Relational step properties are then
expressed with LTL's **`Comparison`** operator — `ComparisonOf((p, c) => …)`, where `p` is the
current state and `c` the next — so the model needs *only* the current observation plus the event;
it does **not** store the previous observation (the temporal operator supplies it). For example,
"after an enqueue, `next.current == cur.current + [v]`" is
`Implies(WeakNext(PredOf(c => c.event.Enqueued?)), ComparisonOf((p, c) => … p.current + [v] == c.current))`.
None of this requires the SUT to log its own history. `Check` is a skip-precondition: a command
whose `Check` is false in the current model is silently skipped (not a failure). `RunCmd` returns the
(possibly grown) footprint `repr'`, so SUTs that allocate as they run (linked lists, trees, …) are
supported; a preallocated/bounded SUT just returns `repr` unchanged.

## Failure reporting

On failure the report prints the **violated LTL tags**, the **command trace**, and the **minimised
choices**, all from the *minimal* counterexample. Each test case returns a
`ModelTestOutcome(tags, commandTrace)` as its `TestResult` payload; `TestingState` tracks that
payload as `bestResult` in lockstep with the minimised choice sequence, so the trace is never a stale
shrink probe. Tag atomic properties with `.Tag("name")` (see [LTL.md](LTL.md)).

## Worked example — a circular (ring-buffer) FIFO queue (`test/StatefulTest.dfy`)

The production class extends nothing **and stores only the ring buffer** — no enqueue/dequeue logs
(those would be pure test instrumentation). The model carries only the current contents plus an
`event`; the LTL **`Comparison`** operator relates each state to the next, so no `previous` field is
needed. Commands are a datatype, and `SUT` is refined to the class, so `RunCmd` drives it with no
casts.

```dafny
module CircularQueueModelTest refines StatefulModelTest {
  const CAP: nat := 4
  datatype Event = Init | Enqueued(v: int) | Dequeued | Other
  datatype QueueModel = QueueModel(current: seq<int>, count: nat, expectedCount: nat, event: Event)
  type Model = QueueModel
  datatype QueueCmd = Enq(v: nat) | Deq | BuggyDeq | BumpCount
  type Cmd = QueueCmd

  class CircularQueue {                          // clean: ONLY a ring buffer, no logs
    var data: array<int>
    var head: nat
    var count: nat
    predicate Inv() reads this { data.Length > 0 && head < data.Length && count <= data.Length }
    function contentsFrom(i: nat): seq<int> reads this, data requires Inv() requires i <= count { /* … */ }
    method enqueue(v: int)        /* … */ { /* … */ }
    method dequeue() returns (v: int) /* … */ { /* … */ }
    method dequeueLifo() returns (v: int) /* buggy: removes the back */ { /* … */ }
    method bumpCount()            /* buggy: count++ without adding an element */ { count := count + 1; }
  }
  type SUT = CircularQueue

  ghost predicate ValidSUT(s: SUT, repr: set<object>) { s in repr && s.data in repr && s.Inv() }
  function InitialModel(): Model { QueueModel([], 0, 0, Init) }
  method MakeSUT() returns (s: SUT, ghost repr: set<object>) { s := new CircularQueue(CAP); repr := {s, s.data}; }

  // Sample carries the observed contents, the SUT's count field, an *independently
  // tracked* expectedCount, and an event for `cmd`.
  method Sample(prev: Model, cmd: Cmd, s: SUT, ghost repr: set<object>) returns (m: Model) {
    var cur := s.contentsFrom(0);
    var ev := match cmd { case Enq(v) => Enqueued(v) case Deq => Dequeued case BuggyDeq => Dequeued case BumpCount => Other };
    var expected := match cmd {
      case Enq(_)    => prev.expectedCount + 1
      case Deq       => if prev.expectedCount > 0 then prev.expectedCount - 1 else 0
      case BuggyDeq  => if prev.expectedCount > 0 then prev.expectedCount - 1 else 0
      case BumpCount => prev.expectedCount       // a count bump is no logical op
    };
    m := QueueModel(cur, s.count, expected, ev);
  }
  predicate Check(cmd: Cmd, m: Model) {
    match cmd { case Enq(_) => |m.current| < CAP case Deq => |m.current| > 0
                case BuggyDeq => |m.current| > 0 case BumpCount => |m.current| < CAP }
  }
  method RunCmd(cmd: Cmd, m: Model, s: SUT, ghost repr: set<object>) returns (ghost repr': set<object>) {
    match cmd {                                  // s IS a CircularQueue — no casts
      case Enq(v)    => if s.count < s.data.Length { s.enqueue(v); }
      case Deq       => if s.count > 0 { var _ := s.dequeue(); }
      case BuggyDeq  => if s.count > 0 { var _ := s.dequeueLifo(); }
      case BumpCount => if s.count < s.data.Length { s.bumpCount(); }
    }
    repr' := repr;
  }
  function CmdString(cmd: Cmd): string { /* "Enq(n)" / "Deq" / "BuggyDeq" / "BumpCount" */ }

  // The sub-properties are bare formulas; a single `Always` in QueueProps lifts
  // the whole bundle over the trace.
  // (1) transition consistency — Next + Implies + Comparison; `p` is current, `c` next.
  function StepConsistent(): LTLFormula<QueueModel> /* WellFormed */ {
    And(
      Implies(WeakNext(PredOf((c: QueueModel) => c.event.Enqueued?)),
              ComparisonOf((p: QueueModel, c: QueueModel) =>
                c.event.Enqueued? ==> c.current == p.current + [c.event.v]).Tag("enqueue-step")),
      Implies(WeakNext(PredOf((c: QueueModel) => c.event.Dequeued?)),
              ComparisonOf((p: QueueModel, c: QueueModel) =>
                c.event.Dequeued? ==> |p.current| > 0 && c.current == p.current[1..]).Tag("dequeue-step"))
    )
  }
  // (2) tautological sanity invariant (always holds — sampling forces it).
  function CountMatches(): LTLFormula<QueueModel> /* WellFormed */ {
    PredOf((m: QueueModel) => m.count == |m.current|).Tag("count-matches")
  }
  // (3) real cross-check: SUT count vs. independently tracked op count.
  function CountTracksOps(): LTLFormula<QueueModel> /* WellFormed */ {
    PredOf((m: QueueModel) => m.count == m.expectedCount).Tag("count-tracks-ops")
  }
  function QueueProps(): LTLFormula<QueueModel> /* WellFormed */ {
    Always(AndSeq([StepConsistent(), CountMatches(), CountTracksOps()]), 0)
  }

  // Pattern: `expect` the outcome of every run — `expect ok` normally, or
  // `expect !ok` (with a message) when the run is meant to expose a bug.
  method {:test} QueueModelTest() {
    var okGood := RunModelTest("queue (correct)", Arbitrary<QueueCmd>.Of([Enq(1), Enq(2), Enq(3), Deq]), QueueProps(), 30);
    expect okGood, "correct queue should satisfy all properties";

    var okLifo := RunModelTest("queue (buggy LIFO)", Arbitrary<QueueCmd>.Of([Enq(1), Enq(2), Enq(3), BuggyDeq]), QueueProps(), 30);
    expect !okLifo, "buggy LIFO dequeue should violate step-consistency (dequeue-step)";

    var okBump := RunModelTest("queue (count corruption)", Arbitrary<QueueCmd>.Of([Enq(1), Deq, BumpCount]), QueueProps(), 30);
    expect !okBump, "BumpCount should desync count from expectedCount (count-tracks-ops)";
  }
}
```

The two count properties are complementary. The **buggy LIFO** dequeue breaks ordering but keeps the
counts right, so it trips `dequeue-step` only; the **BumpCount** bug desyncs the count field while
emitting no enqueue/dequeue event, so it trips `count-tracks-ops` only:

```
[queue (correct)]          PASS (1000 valid examples)

[queue (buggy LIFO)]       FAIL  violated tags: {dequeue-step}      commands: [Enq(2), Enq(1), BuggyDeq]
[queue (count corruption)] FAIL  violated tags: {count-tracks-ops}  commands: [BumpCount]
```

Because each run's outcome is checked with `expect` (inverted for the runs meant to fail), the whole
`{:test}` method passes — that's the pattern to copy when writing your own model tests.

`count == |current|` (`CountMatches`) holds on every run, but note it is in fact guaranteed by the
sampling — `contentsFrom(0)` returns exactly `count` elements — so it documents the invariant and
guards against regressions in the observation logic rather than catching a bug here.

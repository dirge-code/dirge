include "./Arbitrary.dfy"
include "./DafnyCheck.dfy"
include "./LTL.dfy"

// Stateful (model-based) property testing via *module refinement*.
//
// `StatefulModelTest` is an abstract module declaring the test-specific pieces
// as deferred types/operations: the immutable `Model`, the (possibly mutable,
// heap) system `SUT`, the command `Cmd`, and the operations over them. The
// generic runner (ModelTestFunction + RunModelTest, built on DafnyCheck's
// TestingState) is fully defined here in terms of those deferred operations.
//
// A user `refines` this module, supplying concrete `Model`/`SUT`/`Cmd` and the
// operation bodies. Because `SUT` becomes a concrete type in the refinement,
// command bodies touch it directly — *no downcasts* — and because the heap
// footprint is threaded as an explicit ghost `set<object>`, a mutable heap SUT
// can be driven in place without ever putting an abstract type in a
// `modifies`/`reads` clause (which Dafny forbids).
abstract module StatefulModelTest {
  import opened Arbitraries
  import opened TestResults
  import opened TestTypes
  import opened RandomGenerator
  import opened Std.Wrappers
  import M = DafnyCheck
  import opened LTL
  import opened LTLUtils
  import opened RunConfigs
  import Reporting

  // ===========================================================================
  // The deferred test specification — provided by the refining module.
  // ===========================================================================

  // Immutable abstraction of the system's observable state.
  type Model(!new)
  // The system under test. May be a heap class; the refinement decides.
  type SUT
  // The command alphabet — typically a datatype the runner draws and matches on.
  type Cmd(!new)

  // `repr` is the SUT's heap footprint, threaded explicitly as a ghost set so
  // the abstract type `SUT` never appears in a reads/modifies clause.
  ghost predicate ValidSUT(s: SUT, repr: set<object>)
    reads repr

  // The model the trace starts from (should match a Sample of a fresh SUT).
  function InitialModel(): Model

  // Allocate a fresh SUT and report its footprint.
  method MakeSUT() returns (s: SUT, ghost repr: set<object>)
    ensures ValidSUT(s, repr)
    ensures fresh(repr)
    decreases 0

  // Project the current system state into the next model. Receives the previous
  // model `prev` and the command `cmd` that was just run, so the model can be a
  // *transition record* — e.g. carry the previous observation plus an event for
  // `cmd` — letting the LTL property relate consecutive states (current vs.
  // previous-given-the-command) without the SUT logging its own history.
  method Sample(prev: Model, cmd: Cmd, s: SUT, ghost repr: set<object>) returns (m: Model)
    requires ValidSUT(s, repr)
    ensures ValidSUT(s, repr)
    decreases 0

  // Precondition over the model: a command whose Check is false is skipped.
  predicate Check(cmd: Cmd, m: Model)

  // Drive one command against the system, mutating it in place. The footprint
  // may grow (e.g. a command allocates internal nodes): RunCmd returns the
  // updated `repr'`, which contains `repr` plus only freshly-allocated objects.
  method RunCmd(cmd: Cmd, m: Model, s: SUT, ghost repr: set<object>)
    returns (ghost repr': set<object>)
    requires Check(cmd, m)
    requires ValidSUT(s, repr)
    modifies repr
    ensures ValidSUT(s, repr')
    ensures repr <= repr'
    ensures fresh(repr' - repr)
    decreases 0

  // Label for the failure trace.
  function CmdString(cmd: Cmd): string

  // ===========================================================================
  // The generic runner (defined here, inherited by the refinement).
  // ===========================================================================

  // Outcome of one test case, carried as the TestResult payload so it rides
  // along with the minimal counterexample TestingState tracks in `bestResult`.
  datatype ModelTestOutcome = ModelTestOutcome(tags: set<string>, commandTrace: seq<string>)

  class ModelTestFunction extends M.TestFunction<ModelTestOutcome> {
    var cmds: Arbitrary<Cmd>
    var ltlProperty: LTLFormula<Model>
    var maxSteps: nat

    constructor(cmds: Arbitrary<Cmd>, ltlProperty: LTLFormula<Model>, maxSteps: nat)
      requires cmds.Valid()
      requires WellFormedFormula(ltlProperty)
      ensures fresh(this)
      ensures this.cmds == cmds
      ensures Valid()
    {
      this.cmds := cmds;
      this.ltlProperty := ltlProperty;
      this.maxSteps := maxSteps;
      this.repr := {this} + {cmds.internalFunction} + cmds.internalFunction.repr;
    }

    ghost predicate Valid()
      reads this, repr
      ensures Valid() ==> this in repr
    {
      this in repr
      && cmds.internalFunction in repr
      && cmds.internalFunction.repr <= repr
      && this !in cmds.internalFunction.repr
      && this.repr == {this} + {cmds.internalFunction} + cmds.internalFunction.repr
      && cmds.Valid()
      && WellFormedFormula(ltlProperty)
    }

    method Apply(tc: TestCase) returns (result: TestResult<ModelTestOutcome>)
      requires Valid()
      requires tc.Valid()
      requires this.repr !! tc.repr
      modifies tc, tc.random, this
      ensures Valid()
      ensures this.repr == old(this.repr)
      ensures this.cmds == old(this.cmds)
      ensures this.ltlProperty == old(this.ltlProperty)
      decreases this.repr
    {
      var s, sysRepr := MakeSUT();
      var m: Model := InitialModel();
      var residual: LTLFormula<Model> := Step(ltlProperty, m);
      var step: nat := 0;
      var commandTrace: seq<string> := [];

      while step < maxSteps && !isDetermined(residual)
        invariant Valid()
        invariant tc.Valid()
        invariant tc.random == old(tc.random)
        invariant this.repr !! tc.repr
        invariant WellFormedFormula(residual)
        invariant this.repr == old(this.repr)
        invariant this.cmds == old(this.cmds)
        invariant this.ltlProperty == old(this.ltlProperty)
        // `sysRepr` is allocated inside Apply (by MakeSUT) and only mutated in
        // place, so it stays fresh and disjoint from tc/this — that is what lets
        // RunCmd modify it without it being in Apply's modifies clause.
        invariant ValidSUT(s, sysRepr)
        invariant fresh(sysRepr)
        invariant sysRepr !! tc.repr
        invariant sysRepr !! this.repr
        decreases maxSteps - step
      {
        assert cmds.internalFunction.repr <= this.repr;
        assert cmds.internalFunction.repr !! tc.repr;
        var cmd := tc.Any(cmds);
        if Check(cmd, m) {
          // Mutate the system in place (footprint may grow with fresh objects),
          // then sample it back into a model.
          sysRepr := RunCmd(cmd, m, s, sysRepr);
          m := Sample(m, cmd, s, sysRepr);
          commandTrace := commandTrace + [CmdString(cmd)];
          if isGuarded(residual) {
            residual := StepResidual(residual, m);
          } else {
            residual := Step(residual, m);
          }
        }
        step := step + 1;
      }

      var evaluated := EvaluateValidity(residual);
      var validity := evaluated.0;
      var tags := evaluated.1;
      var outcome := ModelTestOutcome(tags, commandTrace);

      if validity.value {
        result := new TestResult<ModelTestOutcome>(None, Some(outcome));
      } else {
        result := new TestResult<ModelTestOutcome>(Some(INTERESTING), Some(outcome));
      }
    }
  }

  method RunModelTest(
    name: string,
    cmds: Arbitrary<Cmd>,
    ltlProperty: LTLFormula<Model>,
    maxSteps: nat)
    returns (passed: bool)
    requires cmds.Valid()
    requires WellFormedFormula(ltlProperty)
    requires 0 < maxSteps
    decreases 0
  {
    passed := RunModelTestWithConfig(name, cmds, ltlProperty, maxSteps, 100, 42, true, Low);
  }

  method RunModelTestWithConfig(
    name: string,
    cmds: Arbitrary<Cmd>,
    ltlProperty: LTLFormula<Model>,
    maxSteps: nat,
    numRuns: nat,
    seed: bv64,
    useColor: bool,
    verbosity: Verbosity)
    returns (passed: bool)
    requires cmds.Valid()
    requires WellFormedFormula(ltlProperty)
    requires 0 < maxSteps
    decreases 0
  {
    var mtf := new ModelTestFunction(cmds, ltlProperty, maxSteps);
    var rng := new M.SimpleRandomGen(seed);
    var nr := if numRuns == 0 then 1 else numRuns;
    var state := new M.TestingState<ModelTestOutcome>(rng, mtf, nr, seed, None, verbosity, useColor);
    state.Run();

    var res := state.GetResult();
    var valid := state.GetValidTestCases();
    var best := state.GetBestResult();
    match res {
      case None =>
        if valid == 0 {
          Reporting.ReportUnsatisfiable(name, useColor, verbosity);
          passed := false;
        } else {
          Reporting.ReportSuccess(name, valid, useColor, verbosity);
          passed := true;
        }
      case Some(choices) =>
        passed := false;
        Reporting.ReportFailure(name, useColor, verbosity);
        if verbosity != Off {
          match best {
            case Some(outcome) =>
              print "  violated tags: ", TagsToString(outcome.tags), "\n";
              print "  commands: [", StringJoin(outcome.commandTrace, ", "), "]\n";
            case None =>
          }
          print "  minimised choices: ", choices, "\n";
        }
    }
  }
}

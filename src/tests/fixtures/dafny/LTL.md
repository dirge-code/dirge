# `LTL.dfy` — linear temporal logic (module `LTL`)

A linear-temporal-logic formula library used to express temporal properties for
[model-based testing](Stateful.md). This is the Dafny port of
[`@fast-check/LTL` (LTLTS)](https://github.com/hath995/LTLTS), itself based on the work of Oskar
Wickström (Quickstrom) and Liam O'Connor. Formulas are values of `LTLFormula<A>` over a state type `A`; build them with the
constructor functions below (which all establish `WellFormedFormula`), then hand one to
`StatefulModelTest.RunModelTest`.

```dafny
datatype LTLFormula<!A> =
  | LTLPred(pred: PredFn<A>, tags: set<string>)
  | LTLTrue(tags) | LTLFalse(tags)
  | LTLAnd(term1, term2, tags) | LTLOr(term1, term2, tags)
  | LTLImplies(term1, term2, tags) | LTLNot(term, tags)
  | LTLComparison(cmp: CmpFn<A>, tags)
  | LTLEventually(term, steps: nat, tags) | LTLAlways(term, steps: nat, tags)
  | LTLUntil(condition, term, steps: nat, tags) | LTLRelease(condition, term, steps: nat, tags)
  | LTLReqNext(term, tags) | LTLWeakNext(term, tags) | LTLStrongNext(term, tags)
  | LTLBind(fn: BindFn<A>, depth: nat, tags)

type PredFn<-A> = A -> bool          // predicate on a state
type CmpFn<-A>  = (A, A) -> bool     // relation between a state and the next state
type BindFn<!A> = A -> LTLFormula<A>
```

## Building formulas

Each returns `LTLFormula<A>` and `ensures WellFormedFormula(r)`:

```dafny
function True<A(!new)>(): LTLFormula<A>
function False<A(!new)>(): LTLFormula<A>
function PredOf<A(!new)>(p: PredFn<A>): LTLFormula<A>           // predicate on the current state
function ComparisonOf<A(!new)>(c: CmpFn<A>): LTLFormula<A>      // relation between consecutive states

function And<A(!new)>(t1, t2): LTLFormula<A>
function Or<A(!new)>(t1, t2): LTLFormula<A>
function Implies<A(!new)>(t1, t2): LTLFormula<A>
function Not<A(!new)>(t): LTLFormula<A>
function AndSeq<A(!new)>(ts: seq<LTLFormula<A>>): LTLFormula<A> // n-ary And
function OrSeq<A(!new)>(ts: seq<LTLFormula<A>>): LTLFormula<A>  // n-ary Or

function Always<A(!new)>(t, steps: nat): LTLFormula<A>          // G (henceforth)
function Eventually<A(!new)>(t, steps: nat): LTLFormula<A>      // F
function Until<A(!new)>(cond, t, steps: nat): LTLFormula<A>     // U
function Release<A(!new)>(cond, t, steps: nat): LTLFormula<A>   // R
function ReqNext<A(!new)>(t): LTLFormula<A>                     // strong next (X)
function WeakNext<A(!new)>(t): LTLFormula<A>
function StrongNext<A(!new)>(t): LTLFormula<A>
function BindOf<A(!new)>(f: BindFn<A>, depth: nat): LTLFormula<A>
```

`steps` bounds how far a temporal operator unfolds (the model-test step budget). Tags annotate
sub-formulas so failure reports can say *which* clause was violated; add them with
`expr.Tag("name")` or `WithTags(expr, {...})`.

## Using formulas

```dafny
ghost predicate WellFormedFormula<A(!new)>(expr: LTLFormula<A>)        // required by the run methods
function Step<A(!new)>(expr: LTLFormula<A>, state: A): (r: LTLFormula<A>)   // advance one state
  requires WellFormedFormula(expr) ensures WellFormedFormula(r)
method LtlEvaluate<A(!new)>(states: seq<A>, formula: LTLFormula<A>) returns (r: PartialValidity)
  requires WellFormedFormula(formula)                                 // evaluate over a state sequence
function Contramap<B(!new), A(!new)>(fn: A -> B, expr: LTLFormula<B>): (r: LTLFormula<A>)   // retarget a formula
  requires WellFormedFormula(expr) ensures WellFormedFormula(r)

datatype Validity = Definitely(value: bool) | Probably(value: bool)
datatype PartialValidity = PartialValidity(requiresNext: bool, validity: Validity, tags: set<string>)
```

`isDetermined`/`isTrue`/`isFalse`/`GetTags` and the structural metrics (`FormulaSize`, `MTC`,
`MPP`, `UTW`, …) are internal evaluation/termination helpers and aren't needed to write properties.

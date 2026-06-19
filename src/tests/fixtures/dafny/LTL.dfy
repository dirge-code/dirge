include "./utils.dfy"
// LinearTemporalLogic 
module LTL {
    import opened LTLUtils

	// Predicates over a state 'A'
	type PredFn<-A> = A -> bool

	// Comparison between a state and the next state
	type CmpFn<-A> = (A, A) -> bool

	// Bind function from a state to a formula
	type BindFn<!A> = A -> LTLFormula<A>
	type Tags = set<string>

	datatype LTLFormula<!A> =
		| LTLPred(pred: PredFn<A>, tags: set<string>)
		| LTLTrue(tags: set<string>)
		| LTLFalse(tags: set<string>)
		| LTLAnd(term1: LTLFormula<A>, term2: LTLFormula<A>, tags: set<string>)
		| LTLOr(term1: LTLFormula<A>, term2: LTLFormula<A>, tags: set<string>)
		| LTLImplies(term1: LTLFormula<A>, term2: LTLFormula<A>, tags: set<string>)
		| LTLNot(term: LTLFormula<A>, tags: set<string>)
		| LTLBind(fn: BindFn<A>, depth: nat, tags: set<string>)
		| LTLComparison(cmp: CmpFn<A>, tags: set<string>)
		| LTLEventually(term: LTLFormula<A>, steps: nat, tags: set<string>)
		| LTLAlways(term: LTLFormula<A>, steps: nat, tags: set<string>)
		| LTLRelease(condition: LTLFormula<A>, term: LTLFormula<A>, steps: nat, tags: set<string>)
		| LTLUntil(condition: LTLFormula<A>, term: LTLFormula<A>, steps: nat, tags: set<string>)
		| LTLReqNext(term: LTLFormula<A>, tags: set<string>)
		| LTLWeakNext(term: LTLFormula<A>, tags: set<string>)
		| LTLStrongNext(term: LTLFormula<A>, tags: set<string>) {

            function Tag(tag: string): LTLFormula<A> {
                match this {
                    case LTLPred(pred, tags) => LTLPred(pred, tags + {tag})
                    case LTLTrue(tags) => LTLTrue(tags + {tag})
                    case LTLFalse(tags) => LTLFalse(tags + {tag})
                    case LTLAnd(term1, term2, tags) => LTLAnd(term1, term2, tags + {tag})
                    case LTLOr(term1, term2, tags) => LTLOr(term1, term2, tags + {tag})
                    case LTLImplies(term1, term2, tags) => LTLImplies(term1, term2, tags + {tag})
                    case LTLNot(term, tags) => LTLNot(term, tags + {tag})
                    case LTLBind(fn, depth, tags) => LTLBind(fn, depth, tags + {tag})
                    case LTLComparison(cmp, tags) => LTLComparison(cmp, tags + {tag})
                    case LTLEventually(term, steps, tags) => LTLEventually(term, steps, tags + {tag})
                    case LTLAlways(term, steps, tags) => LTLAlways(term, steps, tags + {tag})
                    case LTLRelease(condition, term, steps, tags) => LTLRelease(condition, term, steps, tags + {tag})
                    case LTLUntil(condition, term, steps, tags) => LTLUntil(condition, term, steps, tags + {tag})
                    case LTLReqNext(term, tags) => LTLReqNext(term, tags + {tag})
                    case LTLWeakNext(term, tags) => LTLWeakNext(term, tags + {tag})
                    case LTLStrongNext(term, tags) => LTLStrongNext(term, tags + {tag})
                }
            }

            function ToString(): string {
                match this {
                    case LTLPred(pred, tags) => "LTLPred(<pred>, " + TagsToString(tags) + ")"
                    case LTLTrue(tags) => "LTLTrue(" + TagsToString(tags) + ")"
                    case LTLFalse(tags) => "LTLFalse(" + TagsToString(tags) + ")"
                    case LTLAnd(term1, term2, tags) => "LTLAnd(" + term1.ToString() + ", " + term2.ToString() + ", " + TagsToString(tags) + ")"
                    case LTLOr(term1, term2, tags) => "LTLOr(" + term1.ToString() + ", " + term2.ToString() + ", " + TagsToString(tags) + ")"
                    case LTLImplies(term1, term2, tags) => "LTLImplies(" + term1.ToString() + ", " + term2.ToString() + ", " + TagsToString(tags) + ")"
                    case LTLNot(term, tags) => "LTLNot(" + term.ToString() + ", " + TagsToString(tags) + ")"
                    case LTLBind(fn, depth, tags) => "LTLBind(<fn>, " + IntToString(depth) + ", " + TagsToString(tags) + ")"
                    case LTLComparison(cmp, tags) => "LTLComparison(<cmp>, " + TagsToString(tags) + ")"
                    case LTLEventually(term, steps, tags) => "LTLEventually(" + term.ToString() + ", " + IntToString(steps) + ", " + TagsToString(tags) + ")"
                    case LTLAlways(term, steps, tags) => "LTLAlways(" + term.ToString() + ", " + IntToString(steps) + ", " + TagsToString(tags) + ")"
                    case LTLRelease(condition, term, steps, tags) => "LTLRelease(" + condition.ToString() + ", " + term.ToString() + ", " + IntToString(steps) + ", " + TagsToString(tags) + ")"
                    case LTLUntil(condition, term, steps, tags) => "LTLUntil(" + condition.ToString() + ", " + term.ToString() + ", " + IntToString(steps) + ", " + TagsToString(tags) + ")"
                    case LTLReqNext(term, tags) => "LTLReqNext(" + term.ToString() + ", " + TagsToString(tags) + ")"
                    case LTLWeakNext(term, tags) => "LTLWeakNext(" + term.ToString() + ", " + TagsToString(tags) + ")"
                    case LTLStrongNext(term, tags) => "LTLStrongNext(" + term.ToString() + ", " + TagsToString(tags) + ")"
                }
            }
        }

	// Helper constructors (default to empty tag sets)
	function PredOf<A(!new)>(p: PredFn<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(r)
		{ LTLPred(p, {}) }

	function True<A(!new)>(): (r: LTLFormula<A>)
		ensures WellFormedFormula(r)
		{ LTLTrue({}) }

	function False<A(!new)>(): (r: LTLFormula<A>)
		ensures WellFormedFormula(r)
		{ LTLFalse({}) }

	function And<A(!new)>(t1: LTLFormula<A>, t2: LTLFormula<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(t1) && WellFormedFormula(t2) ==> WellFormedFormula(r)
		{ LTLAnd(t1, t2, {}) }

	// Variadic (seq-based) constructor for And, recursively builds a binary tree
	function AndSeq<A(!new)>(ts: seq<LTLFormula<A>>): (r: LTLFormula<A>)
		ensures (forall i :: 0 <= i < |ts| ==> WellFormedFormula(ts[i])) ==> WellFormedFormula(r)
		{ if |ts| == 0 then LTLTrue({})
		  else if |ts| == 1 then ts[0]
		  else LTLAnd(ts[0], AndSeq(ts[1..]), {})
        } by method {
		if |ts| == 0 {
			r := LTLTrue({});
			return;
		}
		var i := |ts| - 1;
		var acc := ts[i];
		while i > 0
			invariant 0 <= i < |ts|
			invariant acc == AndSeq(ts[i..])
			decreases i
		{
			i := i - 1;
			acc := LTLAnd(ts[i], acc, {});
		}
		r := acc;
	}

	function Or<A(!new)>(t1: LTLFormula<A>, t2: LTLFormula<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(t1) && WellFormedFormula(t2) ==> WellFormedFormula(r)
		{ LTLOr(t1, t2, {}) }

	// Variadic (seq-based) constructor for Or, recursively builds a binary tree
	function OrSeq<A(!new)>(ts: seq<LTLFormula<A>>): (r: LTLFormula<A>)
		ensures (forall i :: 0 <= i < |ts| ==> WellFormedFormula(ts[i])) ==> WellFormedFormula(r)
		{ if |ts| == 0 then LTLFalse({})
		  else if |ts| == 1 then ts[0]
		  else LTLOr(ts[0], OrSeq(ts[1..]), {})
		} by method {
		if |ts| == 0 {
			r := LTLFalse({});
			return;
		}
		var i := |ts| - 1;
		var acc := ts[i];
		while i > 0
			invariant 0 <= i < |ts|
			invariant acc == OrSeq(ts[i..])
			decreases i
		{
			i := i - 1;
			acc := LTLOr(ts[i], acc, {});
		}
		r := acc;
	}

	function Implies<A(!new)>(t1: LTLFormula<A>, t2: LTLFormula<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(t1) && WellFormedFormula(t2) ==> WellFormedFormula(r)
		{ LTLImplies(t1, t2, {}) }

	function Not<A(!new)>(t: LTLFormula<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLNot(t, {}) }

	function BindOf<A(!new)>(f: BindFn<A>, depth: nat): (r: LTLFormula<A>)
		ensures (forall a: A :: MaxBindDepth(f(a)) < depth && WellFormedFormula(f(a))) ==> WellFormedFormula(r)
		{ LTLBind(f, depth, {}) }

	function ComparisonOf<A(!new)>(c: CmpFn<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(r)
		{ LTLComparison(c, {}) }

	function Eventually<A(!new)>(t: LTLFormula<A>, steps: nat): (r: LTLFormula<A>)
		ensures WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLEventually(t, steps, {}) }

	function Always<A(!new)>(t: LTLFormula<A>, steps: nat): (r: LTLFormula<A>)
		ensures WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLAlways(t, steps, {}) }

	function Release<A(!new)>(cond: LTLFormula<A>, t: LTLFormula<A>, steps: nat): (r: LTLFormula<A>)
		ensures WellFormedFormula(cond) && WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLRelease(cond, t, steps, {}) }

	function Until<A(!new)>(cond: LTLFormula<A>, t: LTLFormula<A>, steps: nat): (r: LTLFormula<A>)
		ensures WellFormedFormula(cond) && WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLUntil(cond, t, steps, {}) }

	function ReqNext<A(!new)>(t: LTLFormula<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLReqNext(t, {}) }

	function WeakNext<A(!new)>(t: LTLFormula<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLWeakNext(t, {}) }

	function StrongNext<A(!new)>(t: LTLFormula<A>): (r: LTLFormula<A>)
		ensures WellFormedFormula(t) ==> WellFormedFormula(r)
		{ LTLStrongNext(t, {}) }



	// Validity type and helpers (four-valued logic layering)
	datatype Validity =
		| Definitely(value: bool)
		| Probably(value: bool)

	// PartialValidity type for intermediate evaluation results
	datatype PartialValidity =
		| PartialValidity(requiresNext: bool, validity: Validity, tags: set<string>)

	function DT(): Validity { Definitely(true) }
	function PT(): Validity { Probably(true) }
	function PF(): Validity { Probably(false) }
	function DF(): Validity { Definitely(false) }

	function FVNot(v: Validity): Validity
    { match v
        case Definitely(b) => Definitely(!b)
        case Probably(b) => Probably(!b)
    }

	function FVOr(v1: Validity, v2: Validity): Validity
		// Mirrors TS FVOr priority: DT > PT > PF > DF
    { if (v1.Definitely? && v1.value) || (v2.Definitely? && v2.value) then DT()
        else if (v1.Probably? && v1.value) || (v2.Probably? && v2.value) then PT()
        else if (v1.Probably? && !v1.value) || (v2.Probably? && !v2.value) then PF()
        else DF()
    }

	function FVAnd(v1: Validity, v2: Validity): Validity
		// Mirrors TS FVAnd priority: DT only if both DT true; DF if any DT false; PF if any PF; else PT
    { if (v1.Definitely? && v2.Definitely? && v1.value && v2.value) then DT()
        else if (v1.Definitely? && !v1.value) || (v2.Definitely? && !v2.value) then DF()
        else if (v1.Probably? && !v1.value) || (v2.Probably? && !v2.value) then PF()
        else PT()
    }

	// Structural size of a formula, used for termination metrics
	function FormulaSize<A>(expr: LTLFormula<A>): (n: nat)
		ensures n >= 1
    { match expr
        case LTLPred(_, _) => 1
        case LTLTrue(_) => 1
        case LTLFalse(_) => 1
        case LTLAnd(t1, t2, _) => 1 + FormulaSize(t1) + FormulaSize(t2)
        case LTLOr(t1, t2, _) => 1 + FormulaSize(t1) + FormulaSize(t2)
        case LTLImplies(t1, t2, _) => 1 + FormulaSize(t1) + FormulaSize(t2)
        case LTLNot(t, _) => 1 + FormulaSize(t)
        case LTLBind(_, _, _) => 1
        case LTLComparison(_, _) => 1
        case LTLEventually(t, _, _) => 1 + FormulaSize(t)
        case LTLAlways(t, _, _) => 1 + FormulaSize(t)
        case LTLRelease(c, t, _, _) => 1 + FormulaSize(c) + FormulaSize(t)
        case LTLUntil(c, t, _, _) => 1 + FormulaSize(c) + FormulaSize(t)
        case LTLReqNext(t, _) => 1 + FormulaSize(t)
        case LTLWeakNext(t, _) => 1 + FormulaSize(t)
        case LTLStrongNext(t, _) => 1 + FormulaSize(t)
    }

	function FormulaMeasure<A>(expr: LTLFormula<A>): (nat, nat)
	// dec
	{
	match expr
		case LTLPred(_, _) => (0, 1)
		case LTLTrue(_) => (0, 1)
		case LTLFalse(_) => (0, 1)
		case LTLAnd(t1, t2, _) => (FormulaMeasure(t1).0 + FormulaMeasure(t2).0, 1 + FormulaMeasure(t1).1 + FormulaMeasure(t2).1) // Sum of CountSteps
		case LTLOr(t1, t2, _) => (FormulaMeasure(t1).0 + FormulaMeasure(t2).0, 1 + FormulaMeasure(t1).1 + FormulaMeasure(t2).1)
		case LTLImplies(t1, t2, _) => (FormulaMeasure(t1).0 + FormulaMeasure(t2).0, 1 + FormulaMeasure(t1).1 + FormulaMeasure(t2).1)
		case LTLNot(t, _) => (FormulaMeasure(t).0, 1 + FormulaMeasure(t).1)
		case LTLBind(fn, _, _) => (0, 1) // Assuming BindFn evaluation doesn't add temporal steps
		case LTLComparison(_, _) => (0, 1)

		case LTLEventually(t, s, _) => (1 + FormulaMeasure(t).0, 1 + FormulaMeasure(t).1)
		case LTLAlways(t, s, _) => (1 + FormulaMeasure(t).0, 1 + FormulaMeasure(t).1)
		case LTLUntil(c, t, s, _) => (1 + FormulaMeasure(c).0 + FormulaMeasure(t).0, 1 + FormulaMeasure(c).1 + FormulaMeasure(t).1)
		case LTLRelease(c, t, s, _) => (1 + FormulaMeasure(c).0 + FormulaMeasure(t).0, 1 + FormulaMeasure(c).1 + FormulaMeasure(t).1)

		case LTLReqNext(t, _) => (1 + FormulaMeasure(t).0, 1 + FormulaMeasure(t).1)
		case LTLWeakNext(t, _) => (1 + FormulaMeasure(t).0, 1 + FormulaMeasure(t).1)
		case LTLStrongNext(t, _) => (1 + FormulaMeasure(t).0, 1 + FormulaMeasure(t).1)
	}


	// --- Termination metric helpers ---------------------------------------
	// These are used as the lexicographic decreases tuple for Step and its
	// helpers: (MaxBindDepth, MPP, UTW, FormulaSize).
	//
	// - MaxBindDepth: handles the LTLBind recursive expansion. The LTLBind
	//   `depth` field is a structural bound on the bind chain length: any
	//   formula `fn(a)` produced by a bind has MaxBindDepth(fn(a)) < depth
	//   (enforced by WellFormedFormula).
	// - MPP ("Max Push Potential"): sum over LTLNot nodes of the consecutive
	//   temporal-operator chain at the wrapped child. Strictly decreases
	//   when StepNot pushes a Not through a temporal operator via
	//   NegatedFormula. Cuts off completely at Next wrappers so the
	//   self-reference inside StepAlways s==0 doesn't blow it up.
	// - UTW ("Unguarded Temporal Weight"): sums weighted temporal operators
	//   not currently behind a Next wrapper. Strictly decreases when
	//   StepAlways/Until/Release unrolls (outer temporal becomes Next-wrapped).
	// - FormulaSize: existing structural size, handles all the boring
	//   subterm recursions in StepAnd/Or/Implies/etc.

	function Max(a: nat, b: nat): nat { if a >= b then a else b }

	function MaxBindDepth<A>(expr: LTLFormula<A>): (n: nat)
		decreases FormulaSize(expr)
		ensures expr.LTLNot? ==> n == MaxBindDepth(expr.term)
		ensures expr.LTLAlways? ==> n == MaxBindDepth(expr.term)
		ensures expr.LTLEventually? ==> n == MaxBindDepth(expr.term)
		ensures expr.LTLUntil? ==> n == Max(MaxBindDepth(expr.condition), MaxBindDepth(expr.term))
		ensures expr.LTLRelease? ==> n == Max(MaxBindDepth(expr.condition), MaxBindDepth(expr.term))
		ensures expr.LTLAnd? ==> n == Max(MaxBindDepth(expr.term1), MaxBindDepth(expr.term2))
		ensures expr.LTLOr? ==> n == Max(MaxBindDepth(expr.term1), MaxBindDepth(expr.term2))
		ensures expr.LTLReqNext? ==> n == MaxBindDepth(expr.term)
		ensures expr.LTLWeakNext? ==> n == MaxBindDepth(expr.term)
		ensures expr.LTLStrongNext? ==> n == MaxBindDepth(expr.term)
	{ match expr
		case LTLPred(_, _) => 0
		case LTLTrue(_) => 0
		case LTLFalse(_) => 0
		case LTLAnd(t1, t2, _) => Max(MaxBindDepth(t1), MaxBindDepth(t2))
		case LTLOr(t1, t2, _) => Max(MaxBindDepth(t1), MaxBindDepth(t2))
		case LTLImplies(t1, t2, _) => Max(MaxBindDepth(t1), MaxBindDepth(t2))
		case LTLNot(t, _) => MaxBindDepth(t)
		case LTLBind(_, d, _) => d
		case LTLComparison(_, _) => 0
		case LTLEventually(t, _, _) => MaxBindDepth(t)
		case LTLAlways(t, _, _) => MaxBindDepth(t)
		case LTLRelease(c, t, _, _) => Max(MaxBindDepth(c), MaxBindDepth(t))
		case LTLUntil(c, t, _, _) => Max(MaxBindDepth(c), MaxBindDepth(t))
		case LTLReqNext(t, _) => MaxBindDepth(t)
		case LTLWeakNext(t, _) => MaxBindDepth(t)
		case LTLStrongNext(t, _) => MaxBindDepth(t)
	}

	// Max consecutive temporal-operator chain at the root of `expr`.
	function MTC<A>(expr: LTLFormula<A>): (n: nat)
		decreases FormulaSize(expr)
		ensures expr.LTLEventually? ==> n == 1 + MTC(expr.term)
		ensures expr.LTLAlways? ==> n == 1 + MTC(expr.term)
		ensures expr.LTLUntil? ==> n == 1 + MTC(expr.condition) + MTC(expr.term)
		ensures expr.LTLRelease? ==> n == 1 + MTC(expr.condition) + MTC(expr.term)
	{ match expr
		case LTLEventually(t, _, _) => 1 + MTC(t)
		case LTLAlways(t, _, _) => 1 + MTC(t)
		case LTLUntil(c, t, _, _) => 1 + MTC(c) + MTC(t)
		case LTLRelease(c, t, _, _) => 1 + MTC(c) + MTC(t)
		case _ => 0
	}

	// Sum over every LTLNot node of MTC(child). Full cutoff at Next wrappers
	// is critical: it lets StepAlways/Until/Release expansions stay <= MPP
	// of the original even though the expansion structurally embeds the
	// original under LTLWeakNext (s==0) or LTLReqNext (s>0).
	function MPP<A>(expr: LTLFormula<A>): (n: nat)
		decreases FormulaSize(expr)
		ensures expr.LTLNot? ==> n == MTC(expr.term) + MPP(expr.term)
		ensures expr.LTLAlways? ==> n == MPP(expr.term)
		ensures expr.LTLEventually? ==> n == MPP(expr.term)
		ensures expr.LTLUntil? ==> n == MPP(expr.condition) + MPP(expr.term)
		ensures expr.LTLRelease? ==> n == MPP(expr.condition) + MPP(expr.term)
		ensures expr.LTLAnd? ==> n == MPP(expr.term1) + MPP(expr.term2)
		ensures expr.LTLOr? ==> n == MPP(expr.term1) + MPP(expr.term2)
		ensures expr.LTLImplies? ==> n == MPP(expr.term1) + MPP(expr.term2)
		ensures expr.LTLReqNext? ==> n == 0
		ensures expr.LTLWeakNext? ==> n == 0
		ensures expr.LTLStrongNext? ==> n == 0
	{ match expr
		case LTLPred(_, _) => 0
		case LTLTrue(_) => 0
		case LTLFalse(_) => 0
		case LTLAnd(t1, t2, _) => MPP(t1) + MPP(t2)
		case LTLOr(t1, t2, _) => MPP(t1) + MPP(t2)
		case LTLImplies(t1, t2, _) => MPP(t1) + MPP(t2)
		case LTLNot(t, _) => MTC(t) + MPP(t)
		case LTLBind(_, _, _) => 0
		case LTLComparison(_, _) => 0
		case LTLEventually(t, _, _) => MPP(t)
		case LTLAlways(t, _, _) => MPP(t)
		case LTLRelease(c, t, _, _) => MPP(c) + MPP(t)
		case LTLUntil(c, t, _, _) => MPP(c) + MPP(t)
		case LTLReqNext(_, _) => 0
		case LTLWeakNext(_, _) => 0
		case LTLStrongNext(_, _) => 0
	}

	// Unguarded Temporal Weight. Temporal operators contribute 3 or 4 plus
	// the weight of their children; Next wrappers cut off; And/Or/Implies/Not
	// are transparent.
	function UTW<A>(expr: LTLFormula<A>): (n: nat)
		decreases FormulaSize(expr)
		ensures expr.LTLNot? ==> n == UTW(expr.term)
		ensures expr.LTLAlways? ==> n == 3 + UTW(expr.term)
		ensures expr.LTLEventually? ==> n == 3 + UTW(expr.term)
		ensures expr.LTLUntil? ==> n == 4 + UTW(expr.condition) + UTW(expr.term)
		ensures expr.LTLRelease? ==> n == 4 + UTW(expr.condition) + UTW(expr.term)
		ensures expr.LTLAnd? ==> n == UTW(expr.term1) + UTW(expr.term2)
		ensures expr.LTLOr? ==> n == UTW(expr.term1) + UTW(expr.term2)
		ensures expr.LTLImplies? ==> n == UTW(expr.term1) + UTW(expr.term2)
		ensures expr.LTLReqNext? ==> n == 0
		ensures expr.LTLWeakNext? ==> n == 0
		ensures expr.LTLStrongNext? ==> n == 0
	{ match expr
		case LTLPred(_, _) => 0
		case LTLTrue(_) => 0
		case LTLFalse(_) => 0
		case LTLAnd(t1, t2, _) => UTW(t1) + UTW(t2)
		case LTLOr(t1, t2, _) => UTW(t1) + UTW(t2)
		case LTLImplies(t1, t2, _) => UTW(t1) + UTW(t2)
		case LTLNot(t, _) => UTW(t)
		case LTLBind(_, _, _) => 0
		case LTLComparison(_, _) => 0
		case LTLEventually(t, _, _) => 3 + UTW(t)
		case LTLAlways(t, _, _) => 3 + UTW(t)
		case LTLRelease(c, t, _, _) => 4 + UTW(c) + UTW(t)
		case LTLUntil(c, t, _, _) => 4 + UTW(c) + UTW(t)
		case LTLReqNext(_, _) => 0
		case LTLWeakNext(_, _) => 0
		case LTLStrongNext(_, _) => 0
	}

	function repeatfn<A>(expr: LTLFormula<A>, n: nat, a: A): LTLFormula<A>
		requires expr.LTLBind?
		decreases n
	{
		if n == 0 && expr.LTLBind? then expr.fn(a)
		else if repeatfn(expr, n-1, a).LTLBind? then repeatfn(expr, n-1, a).fn(a)
		else expr
	}

	ghost predicate WellFormedFormula<A(!new)>(expr: LTLFormula<A>)
		decreases MaxBindDepth(expr), FormulaSize(expr)
	{
		match expr {
			case LTLPred(_,_) => true
			case LTLTrue(_) => true
			case LTLFalse(_) => true
			case LTLAnd(t1, t2, _) => WellFormedFormula(t1) && WellFormedFormula(t2)
			case LTLOr(t1, t2, _) => WellFormedFormula(t1) && WellFormedFormula(t2)
			case LTLImplies(t1, t2, _) => WellFormedFormula(t1) && WellFormedFormula(t2)
			case LTLNot(t, _) => WellFormedFormula(t)
			case LTLBind(f, d, _) =>
				// LTLBind's `depth` field bounds the bind chain. Any formula
				// produced by f must have strictly smaller MaxBindDepth, which
				// gives Step a real structural decreasing measure on LTLBind.
				(forall a: A :: MaxBindDepth(f(a)) < d)
				&& (forall a: A :: MaxBindDepth(f(a)) < d ==> WellFormedFormula(f(a)))
			case LTLComparison(_, _) => true
			case LTLEventually(t, _, _) => WellFormedFormula(t)
			case LTLAlways(t, _, _) => WellFormedFormula(t)
			case LTLRelease(c, t, _, _) => WellFormedFormula(c) && WellFormedFormula(t)
			case LTLUntil(c, t, _, _) => WellFormedFormula(c) && WellFormedFormula(t)
			case LTLReqNext(t, _) => WellFormedFormula(t)
			case LTLWeakNext(t, _) => WellFormedFormula(t)
			case LTLStrongNext(t, _) => WellFormedFormula(t)
		}
	}

	ghost predicate finiteBind<A(!new)>(expr: LTLFormula<A>) {
		expr.LTLBind? && WellFormedFormula(expr)
	}

	// Contramap: map formula expecting B-states into one expecting A-states via fn: A -> B.
	// Requires the input to be WellFormed so the LTLBind case has a real
	// structural decreasing measure via the LTLBind.depth invariant:
	// MaxBindDepth(f(fn(a))) < depth = MaxBindDepth(expr).
	// Output is WellFormed with the same MaxBindDepth, so chains of Contramap
	// calls (e.g. Contramap(f, Contramap(g, expr))) verify their preconditions
	// without separately invoking ContramapPreserves* lemmas.
	function Contramap<B(!new), A(!new)>(fn: A -> B, expr: LTLFormula<B>): (r: LTLFormula<A>)
		requires WellFormedFormula(expr)
		ensures WellFormedFormula(r)
		ensures MaxBindDepth(r) == MaxBindDepth(expr)
		decreases MaxBindDepth(expr), FormulaSize(expr)
    {
            match expr
			case LTLPred(pred, tags) => LTLPred((a: A) => pred(fn(a)), tags)
			case LTLTrue(tags) => LTLTrue(tags)
			case LTLFalse(tags) => LTLFalse(tags)
			case LTLAnd(t1, t2, tags) => LTLAnd(Contramap(fn, t1), Contramap(fn, t2), tags)
			case LTLOr(t1, t2, tags) => LTLOr(Contramap(fn, t1), Contramap(fn, t2), tags)
			case LTLImplies(t1, t2, tags) => LTLImplies(Contramap(fn, t1), Contramap(fn, t2), tags)
			case LTLNot(t, tags) => LTLNot(Contramap(fn, t), tags)
			case LTLBind(f, depth, tags) => LTLBind((a: A) =>
                Contramap(fn, f(fn(a))), depth, tags)
			case LTLComparison(cmp, tags) => LTLComparison((s: A, n: A) => cmp(fn(s), fn(n)), tags)
			case LTLEventually(t, steps, tags) => LTLEventually(Contramap(fn, t), steps, tags)
			case LTLAlways(t, steps, tags) => LTLAlways(Contramap(fn, t), steps, tags)
			case LTLRelease(c, t, steps, tags) => LTLRelease(Contramap(fn, c), Contramap(fn, t), steps, tags)
			case LTLUntil(c, t, steps, tags) => LTLUntil(Contramap(fn, c), Contramap(fn, t), steps, tags)
			case LTLReqNext(t, tags) => LTLReqNext(Contramap(fn, t), tags)
			case LTLWeakNext(t, tags) => LTLWeakNext(Contramap(fn, t), tags)
			case LTLStrongNext(t, tags) => LTLStrongNext(Contramap(fn, t), tags)
    }

	// Predicates mirroring TypeScript helpers
	function isTrue<A>(expr: LTLFormula<A>): bool
    { match expr
        case LTLTrue(_) => true
        case _ => false
    }

	function isFalse<A>(expr: LTLFormula<A>): bool
    { match expr
        case LTLFalse(_) => true
        case _ => false
    }

	function isTemporalOperator<A>(expr: LTLFormula<A>): bool
    { match expr
        case LTLEventually(_, _, _) => true
        case LTLAlways(_, _, _) => true
        case LTLUntil(_, _, _, _) => true
        case LTLRelease(_, _, _, _) => true
        case _ => false
    }

	function containsTemporalOperator<A>(expr: LTLFormula<A>): bool
    { if isTemporalOperator(expr) then true
        else match expr
            case LTLAnd(t1, t2, _) => containsTemporalOperator(t1) || containsTemporalOperator(t2)
            case LTLOr(t1, t2, _) => containsTemporalOperator(t1) || containsTemporalOperator(t2)
            case LTLNot(t, _) => containsTemporalOperator(t)
            case _ => false
    }

	function isGuarded<A>(expr: LTLFormula<A>): bool
    { match expr
        case LTLReqNext(_, _) => true
        case LTLWeakNext(_, _) => true
        case LTLStrongNext(_, _) => true
        case LTLAnd(t1, t2, _) => isGuarded(t1) && isGuarded(t2)
        case LTLOr(t1, t2, _) => isGuarded(t1) && isGuarded(t2)
        case LTLImplies(t1, t2, _) => isGuarded(t1)
        case LTLNot(t, _) => isGuarded(t)
        case _ => false
    }

	function isDetermined<A>(expr: LTLFormula<A>): bool
    { isTrue(expr) || isFalse(expr) }

	// Determines if a formula requires the next state to be evaluated
	function RequiresNext<A>(expr: LTLFormula<A>): bool
        decreases FormulaSize(expr)
    { match expr
        case LTLReqNext(_, _) => true
        case LTLWeakNext(_, _) => false
        case LTLStrongNext(_, _) => false
        case LTLEventually(term, _, _) => RequiresNext(term)
        case LTLAlways(term, _, _) => RequiresNext(term)
        case LTLUntil(condition, term, _, _) => RequiresNext(condition) || RequiresNext(term)
        case LTLRelease(condition, term, _, _) => RequiresNext(condition) || RequiresNext(term)
        case LTLAnd(term1, term2, _) => RequiresNext(term1) || RequiresNext(term2)
        case LTLOr(term1, term2, _) => RequiresNext(term1) || RequiresNext(term2)
        case LTLNot(term, _) => RequiresNext(term)
        case _ => false
    }

	// Tag utilities
	function GetTags<A>(expr: LTLFormula<A>): set<string>
		{ match expr
			case LTLPred(_, tags) => tags
			case LTLTrue(tags) => tags
			case LTLFalse(tags) => tags
			case LTLAnd(_, _, tags) => tags
			case LTLOr(_, _, tags) => tags
			case LTLImplies(_, _, tags) => tags
			case LTLNot(_, tags) => tags
			case LTLBind(_, _, tags) => tags
			case LTLComparison(_, tags) => tags
			case LTLEventually(_, _, tags) => tags
			case LTLAlways(_, _, tags) => tags
			case LTLRelease(_, _, _, tags) => tags
			case LTLUntil(_, _, _, tags) => tags
			case LTLReqNext(_, tags) => tags
			case LTLWeakNext(_, tags) => tags
			case LTLStrongNext(_, tags) => tags
		}

	function WithTags<A(!new)>(expr: LTLFormula<A>, extra: set<string>): (r: LTLFormula<A>)
		// WithTags only updates the tag set; structurally it returns the same
		// constructor with the same children, so WellFormedFormula is preserved.
		ensures WellFormedFormula(expr) ==> WellFormedFormula(r)
		{ match expr
			case LTLPred(p, tags) => LTLPred(p, tags + extra)
			case LTLTrue(tags) => LTLTrue(tags + extra)
			case LTLFalse(tags) => LTLFalse(tags + extra)
			case LTLAnd(t1, t2, tags) => LTLAnd(t1, t2, tags + extra)
			case LTLOr(t1, t2, tags) => LTLOr(t1, t2, tags + extra)
			case LTLImplies(t1, t2, tags) => LTLImplies(t1, t2, tags + extra)
			case LTLNot(t, tags) => LTLNot(t, tags + extra)
			case LTLBind(f, depth, tags) => LTLBind(f, depth, tags + extra)
			case LTLComparison(c, tags) => LTLComparison(c, tags + extra)
			case LTLEventually(t, s, tags) => LTLEventually(t, s, tags + extra)
			case LTLAlways(t, s, tags) => LTLAlways(t, s, tags + extra)
			case LTLRelease(c, t, s, tags) => LTLRelease(c, t, s, tags + extra)
			case LTLUntil(c, t, s, tags) => LTLUntil(c, t, s, tags + extra)
			case LTLReqNext(t, tags) => LTLReqNext(t, tags + extra)
			case LTLWeakNext(t, tags) => LTLWeakNext(t, tags + extra)
			case LTLStrongNext(t, tags) => LTLStrongNext(t, tags + extra)
		}

	function UnionTags(a: set<string>, b: set<string>): set<string> { a + b }

	// Dual of temporal operators when under Not
	function NegatedFormula<A>(expr: LTLFormula<A>): LTLFormula<A>
		{ match expr
			case LTLEventually(t, s, _) => LTLAlways(LTLNot(t, {}), s, {})
			case LTLAlways(t, s, _) => LTLEventually(LTLNot(t, {}), s, {})
			case LTLUntil(c, t, s, _) => LTLRelease(LTLNot(c, {}), LTLNot(t, {}), s, {})
			case LTLRelease(c, t, s, _) => LTLUntil(LTLNot(c, {}), LTLNot(t, {}), s, {})
			case _ => expr
		}

	// Step is a pure function over WellFormed formulas. Termination is the
	// lexicographic tuple (MaxBindDepth, MPP, UTW, FormulaSize):
	//   - StepBind  strictly decreases MaxBindDepth (by the LTLBind depth invariant)
	//   - StepAlways/Until/Release strictly decreases UTW (the outer temporal
	//     is consumed; the inner copy is behind a Next wrapper, cut off in MPP/UTW)
	//   - StepNot temporal branch strictly decreases MPP (one Not-temporal pair consumed)
	//   - everything else strictly decreases FormulaSize (subterm recursion)
	function StepTrue<A>(expr: LTLFormula<A>): LTLFormula<A>
		{ expr }

	function StepFalse<A>(expr: LTLFormula<A>): LTLFormula<A>
		requires expr.LTLFalse?
		{ expr }

	function StepPred<A>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLPred?
		{ if expr.pred(state) then LTLTrue({}) else LTLFalse(expr.tags) }

	function StepBind<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLBind?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
		{ WithTags(Step(expr.fn(state), state), expr.tags) }

	function StepComparison<A>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLComparison?
		{ LTLWeakNext(LTLPred((next: A) => expr.cmp(state, next), expr.tags), {}) }

	function StepNext<A>(expr: LTLFormula<A>): LTLFormula<A>
		requires expr.LTLReqNext?
		{ expr }

	function StepWeakNext<A>(expr: LTLFormula<A>): LTLFormula<A>
		requires expr.LTLWeakNext?
		{ expr }

	function StepStrongNext<A>(expr: LTLFormula<A>): LTLFormula<A>
		requires expr.LTLStrongNext?
		{ expr }

	function StepAnd<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLAnd?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		var s1 := Step(expr.term1, state);
		var s2 := Step(expr.term2, state);
		if isFalse(s1) || isFalse(s2) then
			LTLFalse(UnionTags(expr.tags, UnionTags(GetTags(s1), GetTags(s2))))
		else if isTrue(s1) && isTrue(s2) then
			LTLTrue({})
		else if isTrue(s1) then
			WithTags(s2, expr.tags)
		else if isTrue(s2) then
			WithTags(s1, expr.tags)
		else if isGuarded(s1) && isGuarded(s2) then
			LTLAnd(WithTags(s1, GetTags(s1)), WithTags(s2, GetTags(s2)), expr.tags)
		else
			LTLAnd(s1, s2, expr.tags)
	}

	function StepOr<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLOr?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		var s1 := Step(expr.term1, state);
		var s2 := Step(expr.term2, state);
		if isTrue(s1) || isTrue(s2) then
			LTLTrue({})
		else if isFalse(s1) && isFalse(s2) then
			LTLFalse(UnionTags(expr.tags, UnionTags(GetTags(s1), GetTags(s2))))
		else if isFalse(s1) then
			WithTags(s2, UnionTags(expr.tags, GetTags(s1)))
		else if isFalse(s2) then
			WithTags(s1, UnionTags(expr.tags, GetTags(s2)))
		else if isGuarded(s1) && isGuarded(s2) then
			LTLOr(WithTags(s1, GetTags(s1)), WithTags(s2, GetTags(s2)), expr.tags)
		else
			LTLOr(s1, s2, expr.tags)
	}

	function StepNot<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLNot?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		// Inlined NegatedFormula match — making each negated formula concrete
		// lets Dafny verify WellFormedFormula and the MPP-strict-decrease at
		// the recursive Step call. MPP strictly decreases by 1: original MPP
		// includes MTC(expr.term) for the outer Not-temporal pair; the
		// negated form has the Not pushed one level inside, so its MTC
		// contribution is one shorter. The single `assert` below unfolds
		// WellFormedFormula one level past the LTLNot wrapper; the rest of
		// WellFormedness threads through the helper-function ensures clauses.
		assert WellFormedFormula(expr.term);
		match expr.term
			case LTLEventually(t, s, _) =>
				Step(WithTags(Always(Not(t), s), expr.tags), state)
			case LTLAlways(t, s, _) =>
				Step(WithTags(Eventually(Not(t), s), expr.tags), state)
			case LTLUntil(c, t, s, _) =>
				Step(WithTags(Release(Not(c), Not(t), s), expr.tags), state)
			case LTLRelease(c, t, s, _) =>
				Step(WithTags(Until(Not(c), Not(t), s), expr.tags), state)
			case _ =>
				// Non-temporal: recurse on the inner term (FormulaSize decreases).
				var stepped := Step(expr.term, state);
				if isTrue(stepped) then
					LTLFalse(expr.tags + GetTags(stepped) + GetTags(expr.term))
				else if isFalse(stepped) then
					LTLTrue(expr.tags + GetTags(stepped) + GetTags(expr.term))
				else
					LTLNot(stepped, expr.tags + GetTags(stepped) + GetTags(expr.term))
	}

	function StepImplies<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLImplies?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		var s1 := Step(expr.term1, state);
		var s2 := Step(expr.term2, state);
		if isTrue(s1) then
			WithTags(s2, expr.tags)
		else if isFalse(s1) then
			LTLTrue({})
		else if isGuarded(s1) && !isGuarded(s2) then
			LTLImplies(s1, LTLWeakNext(expr.term2, {}), expr.tags)
		else
			LTLImplies(s1, s2, expr.tags)
	}

	function StepEventually<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLEventually?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		var t := expr.term;
		var s := expr.steps;
		var st := Step(t, state);
		if s == 0 then
			if isTrue(st) then LTLTrue({})
			else if isFalse(st) then LTLStrongNext(expr, expr.tags)
			else LTLOr(st, LTLStrongNext(expr, {}), expr.tags)
		else
			if isTrue(st) then st
			else if isFalse(st) then LTLReqNext(LTLEventually(t, s-1, expr.tags), {})
			else LTLOr(st, LTLReqNext(LTLEventually(t, s-1, expr.tags), {}), expr.tags)
	}

	function StepAlways<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLAlways?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		var t := expr.term;
		var s := expr.steps;
		var st := Step(t, state);
		if isFalse(st) then
			LTLFalse(UnionTags(expr.tags, GetTags(st)))
		else if s == 0 then
			// UTW strictly decreases: UTW(this) = 3 + UTW(t) > UTW(t) + 0 =
			// UTW(LTLAnd(t, LTLWeakNext(expr, _), _)). MPP and MaxBindDepth
			// are weakly preserved by the full Next cutoff.
			Step(WithTags(And(t, WeakNext(expr)), expr.tags), state)
		else
			Step(WithTags(And(t, ReqNext(WithTags(Always(t, s-1), expr.tags))), expr.tags), state)
	}

	function StepUntil<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLUntil?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		var c := expr.condition;
		var t := expr.term;
		var s := expr.steps;
		if s == 0 then
			Step(WithTags(Or(t, And(c, StrongNext(expr))), expr.tags), state)
		else
			Step(WithTags(Or(t, And(c, ReqNext(WithTags(Until(c, t, s-1), expr.tags)))), expr.tags), state)
	}

	function StepRelease<A(!new)>(expr: LTLFormula<A>, state: A): LTLFormula<A>
		requires expr.LTLRelease?
		requires WellFormedFormula(expr)
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		var c := expr.condition;
		var t := expr.term;
		var s := expr.steps;
		if s == 0 then
			Step(WithTags(And(t, Or(c, WeakNext(expr))), expr.tags), state)
		else
			Step(WithTags(And(t, Or(c, ReqNext(WithTags(Release(c, t, s-1), expr.tags)))), expr.tags), state)
	}

	function Step<A(!new)>(expr: LTLFormula<A>, state: A): (r: LTLFormula<A>)
		requires WellFormedFormula(expr)
		ensures WellFormedFormula(r)
		// Phase number `1` is a tiebreaker: Step → Step* (same expr) is
		// strictly decreasing (phase 1 → 0). Step* helpers strictly decrease
		// the earlier 4-tuple components when they call back into Step on a
		// smaller subterm or expansion, so phase doesn't have to compete.
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 1
	{
		match expr
			case LTLPred(_, _) => StepPred(expr, state)
			case LTLBind(_, _, _) => StepBind(expr, state)
			case LTLTrue(_) => StepTrue(expr)
			case LTLFalse(_) => StepFalse(expr)
			case LTLAnd(_, _, _) => StepAnd(expr, state)
			case LTLOr(_, _, _) => StepOr(expr, state)
			case LTLNot(_, _) => StepNot(expr, state)
			case LTLImplies(_, _, _) => StepImplies(expr, state)
			case LTLComparison(_, _) => StepComparison(expr, state)
			case LTLReqNext(_, _) => StepNext(expr)
			case LTLWeakNext(_, _) => StepWeakNext(expr)
			case LTLStrongNext(_, _) => StepStrongNext(expr)
			case LTLEventually(_, _, _) => StepEventually(expr, state)
			case LTLAlways(_, _, _) => StepAlways(expr, state)
			case LTLUntil(_, _, _, _) => StepUntil(expr, state)
			case LTLRelease(_, _, _, _) => StepRelease(expr, state)
	}

	// StepResidual handles guarded formulas — one Step past the Next wrapper.
	// Pure: recurses on its own subterms (smaller FormulaSize) and calls Step
	// on the rebuilt formula. The rebuilt formula's WellFormedness is preserved
	// because StepResidual returns WellFormed outputs (Step preserves WellFormed
	// for its inputs, and the constructors LTLOr/LTLAnd/LTLImplies of WellFormed
	// children are WellFormed by definition).
	function StepResidual<A(!new)>(expr: LTLFormula<A>, state: A): (r: LTLFormula<A>)
		requires WellFormedFormula(expr)
		ensures WellFormedFormula(r)
		// Same lex tuple as Step* helpers — StepResidual's recursive calls are
		// always on structural subterms (FormulaSize strictly decreases) and
		// the leading metric components are weakly preserved.
		decreases MaxBindDepth(expr), MPP(expr), UTW(expr), FormulaSize(expr), 0
	{
		match expr
			case LTLOr(t1, t2, tags) =>
				var s1 := StepResidual(t1, state);
				var s2 := StepResidual(t2, state);
				// s1, s2 are WellFormed by StepResidual's ensures; LTLOr of two
				// WellFormed children is WellFormed by definition.
				Step(LTLOr(s1, s2, UnionTags(tags, UnionTags(GetTags(s1), GetTags(s2)))), state)
			case LTLAnd(t1, t2, tags) =>
				var s1 := StepResidual(t1, state);
				var s2 := StepResidual(t2, state);
				Step(LTLAnd(s1, s2, UnionTags(tags, UnionTags(GetTags(s1), GetTags(s2)))), state)
			case LTLImplies(t1, t2, tags) =>
				var s1 := StepResidual(t1, state);
				var s2 := if isGuarded(t2) then StepResidual(t2, state) else Step(t2, state);
				Step(LTLImplies(s1, s2, UnionTags(tags, GetTags(s1))), state)
			case LTLNot(t, tags) =>
				Not(StepResidual(t, state))
			case LTLReqNext(t, tags) =>
				// WithTags's ensures clause guarantees WellFormedness is preserved.
				Step(WithTags(t, UnionTags(tags, GetTags(t))), state)
			case LTLWeakNext(t, tags) =>
				Step(WithTags(t, UnionTags(tags, GetTags(t))), state)
			case LTLStrongNext(t, tags) =>
				Step(WithTags(t, UnionTags(tags, GetTags(t))), state)
			case LTLPred(_, _) =>
				Step(expr, state)
			case _ =>
				// This should not happen for guarded formulas
				expr
	}

	// EvaluateValidity function that returns validity and tags (Dafny version)
	function EvaluateValidity<A>(expr: LTLFormula<A>): (Validity, set<string>)
        decreases FormulaSize(expr)
		{ match expr
			case LTLTrue(tags) => (DT(), {})
			case LTLFalse(tags) => (DF(), tags)
			case LTLAnd(t1, t2, tags) =>
				var eval1 := EvaluateValidity(t1);
				var eval2 := EvaluateValidity(t2);
				var result := FVAnd(eval1.0, eval2.0);
				var resultTags := if result.value then {} else tags + eval1.1 + eval2.1;
				(result, resultTags)
			case LTLOr(t1, t2, tags) =>
				var eval1 := EvaluateValidity(t1);
				var eval2 := EvaluateValidity(t2);
				var result := FVOr(eval1.0, eval2.0);
				var resultTags := if result.value then {} else tags + eval1.1 + eval2.1;
				(result, resultTags)
			case LTLImplies(t1, t2, tags) =>
				var eval1 := EvaluateValidity(t1);
				var eval2 := EvaluateValidity(t2);
				var result := FVOr(FVNot(eval1.0), eval2.0);
				var resultTags := if result.value then {} else tags + (if eval1.0.value then eval1.1 else {}) + (if eval2.0.value then {} else eval2.1);
				(result, resultTags)
			case LTLNot(t, tags) =>
				var eval := EvaluateValidity(t);
				(FVNot(eval.0), eval.1)
			case LTLReqNext(_, tags) => (PT(), tags)
			case LTLWeakNext(_, tags) => (PT(), tags)
			case LTLStrongNext(_, tags) => (PF(), tags)
			case _ =>
				// All non-determined cases: LTLPred, LTLBind, LTLComparison, LTLEventually, LTLAlways, LTLRelease, LTLUntil
				// These should not happen in normal evaluation of determined formulas
				var tags := GetTags(expr);
				(DF(), tags)
		}

	function EvaluateValidityTS<A>(expr: LTLFormula<A>): (Validity, set<string>)
        decreases FormulaSize(expr)
		{ match expr
			case LTLTrue(tags) => (DT(), {})
			case LTLFalse(tags) => (DF(), tags)
			case LTLAnd(t1, t2, tags) =>
				var eval1 := EvaluateValidityTS(t1);
				var eval2 := EvaluateValidityTS(t2);
				var result := FVAnd(eval1.0, eval2.0);
				var resultTags := if result.value then {} else tags + (if eval1.0.value then {} else eval1.1) + (if eval2.0.value then {} else eval2.1);
				(result, resultTags)
			case LTLOr(t1, t2, tags) =>
				var eval1 := EvaluateValidityTS(t1);
				var eval2 := EvaluateValidityTS(t2);
				var result := FVOr(eval1.0, eval2.0);
				var resultTags := if result.value then {} else tags + (if eval1.0.value then {} else eval1.1) + (if eval2.0.value then {} else eval2.1);
				(result, resultTags)
			case LTLImplies(t1, t2, tags) =>
				var eval1 := EvaluateValidityTS(t1);
				var eval2 := EvaluateValidityTS(t2);
				var result := FVOr(FVNot(eval1.0), eval2.0);
				var resultTags := if result.value then {} else tags + (if eval1.0.value then eval1.1 else {}) + (if eval2.0.value then {} else eval2.1);
				(result, resultTags)
			case LTLNot(t, tags) =>
				var eval := EvaluateValidityTS(t);
				(FVNot(eval.0), eval.1)
			case LTLReqNext(_, tags) => (PT(), tags)
			case LTLWeakNext(_, tags) => (PT(), tags)
			case LTLStrongNext(_, tags) => (PF(), tags)
			case _ =>
				// All non-determined cases: LTLPred, LTLBind, LTLComparison, LTLEventually, LTLAlways, LTLRelease, LTLUntil
				// These should not happen in normal evaluation of determined formulas
				var tags := GetTags(expr);
				(DF(), tags)
		}

	// PartialValidity function that creates a PartialValidity from a formula
	function CreatePartialValidity<A>(expr: LTLFormula<A>): PartialValidity
        decreases FormulaSize(expr)
		{ if isDetermined(expr) then
			var validity := EvaluateValidityTS(expr);
			var formulaTags := GetTags(expr);
			var resultTags := if isFalse(expr) || !validity.0.value then formulaTags + validity.1 else {};
			PartialValidity(false, validity.0, resultTags)
		  else
			var validity := EvaluateValidityTS(expr);
			var formulaTags := GetTags(expr);
			var resultTags := formulaTags + validity.1;
			PartialValidity(RequiresNext(expr), validity.0, resultTags)
		}

	// Main ltlEvaluate function
	method LtlEvaluate<A(!new)>(states: seq<A>, formula: LTLFormula<A>) returns (r: PartialValidity)
		requires WellFormedFormula(formula)
	{
		if |states| == 0 {
			// r := DF();
			r := PartialValidity(false, DF(), {});
		} else {
			var expr := Step(formula, states[0]);
			var i := 1;
			while !isDetermined(expr) && i < |states|
				invariant 1 <= i <= |states|
				invariant WellFormedFormula(expr)
				decreases |states| - i
			{
				if isGuarded(expr) {
					expr := StepResidual(expr, states[i]);
				} else {
					// This should not happen due to invariant
					expr := expr;
				}
				i := i + 1;
			}
			var evalResult := EvaluateValidity(expr);
			r := PartialValidity(false, evalResult.0, evalResult.1);
		}
	}

	method LtlEvalState<A(!new)>(state: A, formula: LTLFormula<A>) returns (r: PartialValidity, expr: LTLFormula<A>)
		requires WellFormedFormula(formula)
		ensures WellFormedFormula(expr)
	{
		if isGuarded(formula) {
			expr := StepResidual(formula, state);
		} else {
			expr := Step(formula, state);
		}
		// print("\n");
		// print(expr);
		// print("\n");
		// print("Creating partial validity");
		r := CreatePartialValidity(expr);
		// print("\n");
		// print(validity);
	}
}
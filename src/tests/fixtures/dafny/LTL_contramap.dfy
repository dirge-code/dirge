
include "./LTL.dfy"
module Contramap_extra { 
    import opened LTL
    function Id<A>(x: A): A { x }

    lemma ContramapIdPred<A(!new)>(expr: LTLFormula<A>, a: A)
        requires expr.LTLPred?
        requires WellFormedFormula(expr)
        ensures Contramap(Id, expr).pred(a) == expr.pred(a)
    {
        var contraExpr := Contramap(Id, expr);
        var p' := contraExpr.pred;
        calc{
            p'(a);
            contraExpr.pred(a);
            expr.pred(Id(a));
            expr.pred(a);
        }
    }

    lemma ContramapIdComparison<A(!new)>(expr: LTLFormula<A>, a1: A, a2: A)
        requires expr.LTLComparison?
        requires WellFormedFormula(expr)
        ensures Contramap(Id, expr).cmp(a1, a2) == expr.cmp(a1, a2)
    {
        var contraExpr := LTLComparison((s: A, n: A) => expr.cmp(Id(s), Id(n)), expr.tags);
        var c' := (s: A, n: A) => expr.cmp(Id(s), Id(n));
        assert c' == contraExpr.cmp;
        calc{
            c'(a1, a2);
            expr.cmp(Id(a1), Id(a2));
            expr.cmp(a1, a2);
        }
    }

    lemma ContramapIdBind<A(!new)>(expr: LTLFormula<A>, a: A)
        requires expr.LTLBind?
        requires WellFormedFormula(expr)
        ensures Contramap(Id, expr).fn(a) == expr.fn(a)
    {
        var contraExpr := Contramap(Id, expr);
        var f' := contraExpr.fn;
        assert f' == contraExpr.fn;
        calc{
            Contramap(Id, expr).fn(a);
            f'(a);
            Contramap(Id, expr.fn(Id(a)));
            Contramap(Id, expr.fn(a));
            {ContramapId(expr.fn(a));}
            expr.fn(a);
        }
    }

    lemma ContramapId<A(!new)>(expr: LTLFormula<A>)
        requires WellFormedFormula(expr)
        decreases FormulaSize(expr)
        ensures Contramap(Id, expr) == expr
    {
        match expr {
            case LTLPred(p, tags) =>
                // assume p == ((x: A) => p(Id(x)));
                // assert Contramap(Id, LTLPred(p, tags)) == LTLPred((a: A) => p(Id(a)), tags);
                //This property is not provable in Dafny because of the lack of function extensionality
                assume {:axiom} Contramap(Id, LTLPred(p, tags)) == LTLPred(p, tags);
                assert Contramap(Id, expr) == expr;
            case LTLTrue(tags) =>
                assert Contramap(Id, expr) == expr;
            case LTLFalse(tags) =>
                assert Contramap(Id, expr) == expr;
            case LTLAnd(t1, t2, tags) =>
                ContramapId(t1);
                ContramapId(t2);
                assert Contramap(Id, expr) == expr;
            case LTLOr(t1, t2, tags) =>
                ContramapId(t1);
                ContramapId(t2);
                assert Contramap(Id, expr) == expr;
            case LTLImplies(t1, t2, tags) =>
                ContramapId(t1);
                ContramapId(t2);
                assert Contramap(Id, expr) == expr;
            case LTLNot(t, tags) =>
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
            case LTLBind(f, depth, tags) =>
                // assert f == ((a: A) => f(Id(a)));
                //This property is not provable in Dafny because of the lack of function extensionality
                assume {:axiom} Contramap(Id, LTLBind(f, depth, tags)) == LTLBind(f, depth, tags);
                assert Contramap(Id, expr) == expr;
            case LTLComparison(c, tags) =>
                // assert c == ((s: A, n: A) => c(Id(s), Id(n)));
                //This property is not provable in Dafny because of the lack of function extensionality
                assume {:axiom} Contramap(Id, LTLComparison(c, tags)) == LTLComparison(c, tags);
                assert Contramap(Id, expr) == expr;
            case LTLEventually(t, steps, tags) =>
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
            case LTLAlways(t, steps, tags) =>
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
            case LTLRelease(c, t, steps, tags) =>
                ContramapId(c);
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
            case LTLUntil(c, t, steps, tags) =>
                ContramapId(c);
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
            case LTLReqNext(t, tags) =>
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
            case LTLWeakNext(t, tags) =>
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
            case LTLStrongNext(t, tags) =>
                ContramapId(t);
                assert Contramap(Id, expr) == expr;
        }
    }

    function Compose<A(!new),B(!new),C(!new)>(f: B -> C, g: A -> B): A -> C
    {
        (a: A) => f(g(a))
    }

    lemma ContramapCompositionPredicate<A(!new), B(!new), C(!new)>(f: A -> B, g: B -> C, expr: LTLFormula<C>, a: A)
        requires expr.LTLPred?
        requires WellFormedFormula(expr)
        // ensures expr.pred(Compose(g, f)(a)) == expr.pred(g(f(a)))
        ensures Contramap(f, Contramap(g, expr)).pred(a) == Contramap(Compose(g, f), expr).pred(a)
    {
    }

    lemma ContramapCompositionComparison<A(!new), B(!new), C(!new)>(f: A -> B, g: B -> C, expr: LTLFormula<C>, a1: A, a2: A)
        requires expr.LTLComparison?
        requires WellFormedFormula(expr)
        // ensures expr.cmp(Compose(g, f)(a1), Compose(g, f)(a2)) == expr.cmp(g(f(a1)), g(f(a2)))
        ensures Contramap(f, Contramap(g, expr)).cmp(a1, a2) == Contramap(Compose(g, f), expr).cmp(a1, a2)
    {
    }

    lemma ContramapNonBind<A(!new), B(!new)>(fn: A -> B, expr: LTLFormula<B>)
    requires !expr.LTLBind?
    requires WellFormedFormula(expr)
    ensures !Contramap(fn, expr).LTLBind?
{
    match expr {
        case LTLBind(_, _, _) => assert false; // Contradicts requires
        case _ => // All other cases for LTLFormula
            // By Contramap definition, it returns a constructor other than LTLBind
            // if the input expr is not LTLBind.
            // For example, if expr is LTLPred, Contramap returns LTLPred.
            // If expr is LTLAnd, Contramap returns LTLAnd.
            // So the result cannot be LTLBind.
            assert !Contramap(fn, expr).LTLBind?;
    }
}

// Helper lemma: If a formula IS LTLBind, Contramapping it also results in LTLBind.
lemma ContramapLTLBindIfBound<A(!new), B(!new)>(fn: A -> B, expr: LTLFormula<B>)
    requires expr.LTLBind?
    requires WellFormedFormula(expr)
    ensures Contramap(fn, expr).LTLBind?
{
    // Direct from Contramap definition for LTLBind:
    // case LTLBind(f_B, depth, tags) => LTLBind((a_inner => Contramap(fn, f_B(fn(a_inner)))), depth, tags)
    // This explicitly returns an LTLBind constructor.
    assert Contramap(fn, expr).LTLBind?;
}

// Contramap is contravariant on the state type but leaves the structural
// "shape" of the formula intact — in particular, the LTLBind.depth field on
// any LTLBind subterm is preserved. So MaxBindDepth, which only inspects
// structural shape and depth fields (never invoking the bind function),
// passes through unchanged.
lemma ContramapPreservesMaxBindDepth<A(!new), B(!new)>(fn: A -> B, expr: LTLFormula<B>)
    requires WellFormedFormula(expr)
    decreases FormulaSize(expr)
    ensures MaxBindDepth(Contramap(fn, expr)) == MaxBindDepth(expr)
{
    match expr {
        case LTLPred(_, _) =>
        case LTLTrue(_) =>
        case LTLFalse(_) =>
        case LTLAnd(t1, t2, _) =>
            ContramapPreservesMaxBindDepth(fn, t1);
            ContramapPreservesMaxBindDepth(fn, t2);
        case LTLOr(t1, t2, _) =>
            ContramapPreservesMaxBindDepth(fn, t1);
            ContramapPreservesMaxBindDepth(fn, t2);
        case LTLImplies(t1, t2, _) =>
            ContramapPreservesMaxBindDepth(fn, t1);
            ContramapPreservesMaxBindDepth(fn, t2);
        case LTLNot(t, _) =>
            ContramapPreservesMaxBindDepth(fn, t);
        case LTLBind(_, _, _) =>
            // MaxBindDepth(LTLBind(_, d, _)) = d on both sides — Contramap
            // copies the depth field through.
        case LTLComparison(_, _) =>
        case LTLEventually(t, _, _) =>
            ContramapPreservesMaxBindDepth(fn, t);
        case LTLAlways(t, _, _) =>
            ContramapPreservesMaxBindDepth(fn, t);
        case LTLRelease(c, t, _, _) =>
            ContramapPreservesMaxBindDepth(fn, c);
            ContramapPreservesMaxBindDepth(fn, t);
        case LTLUntil(c, t, _, _) =>
            ContramapPreservesMaxBindDepth(fn, c);
            ContramapPreservesMaxBindDepth(fn, t);
        case LTLReqNext(t, _) =>
            ContramapPreservesMaxBindDepth(fn, t);
        case LTLWeakNext(t, _) =>
            ContramapPreservesMaxBindDepth(fn, t);
        case LTLStrongNext(t, _) =>
            ContramapPreservesMaxBindDepth(fn, t);
    }
}

// Contramap preserves WellFormedness. The non-trivial case is LTLBind: the
// contramapped bind body `(a: A) => Contramap(fn, g(fn(a)))` still respects
// the original LTLBind.depth invariant because (i) the depth field is copied
// and (ii) Contramap preserves MaxBindDepth on `g(fn(a))`. Decreases is
// (MaxBindDepth, FormulaSize) so the recursive call on g(fn(a)) — which has
// strictly smaller MaxBindDepth than expr by the LTLBind invariant — is
// well-founded.
lemma ContramapPreservesWellFormedFormula<A(!new), B(!new)>(fn: A -> B, expr: LTLFormula<B>)
    decreases MaxBindDepth(expr), FormulaSize(expr)
    requires WellFormedFormula(expr)
    ensures WellFormedFormula(Contramap(fn, expr))
{
    match expr {
        case LTLPred(_, _) =>
        case LTLTrue(_) =>
        case LTLFalse(_) =>
        case LTLAnd(t1, t2, _) =>
            ContramapPreservesWellFormedFormula(fn, t1);
            ContramapPreservesWellFormedFormula(fn, t2);
        case LTLOr(t1, t2, _) =>
            ContramapPreservesWellFormedFormula(fn, t1);
            ContramapPreservesWellFormedFormula(fn, t2);
        case LTLImplies(t1, t2, _) =>
            ContramapPreservesWellFormedFormula(fn, t1);
            ContramapPreservesWellFormedFormula(fn, t2);
        case LTLNot(t, _) =>
            ContramapPreservesWellFormedFormula(fn, t);
        case LTLBind(g, d, _) =>
            forall a: A
                ensures MaxBindDepth(Contramap(fn, g(fn(a)))) < d
                ensures WellFormedFormula(Contramap(fn, g(fn(a))))
            {
                var b := fn(a);
                // From WellFormedFormula(expr) — both conjuncts of the LTLBind clause:
                assert MaxBindDepth(g(b)) < d;
                assert WellFormedFormula(g(b));
                ContramapPreservesMaxBindDepth(fn, g(b));
                // Decreases: MaxBindDepth(g(b)) < d = MaxBindDepth(expr). Strict.
                ContramapPreservesWellFormedFormula(fn, g(b));
            }
        case LTLComparison(_, _) =>
        case LTLEventually(t, _, _) =>
            ContramapPreservesWellFormedFormula(fn, t);
        case LTLAlways(t, _, _) =>
            ContramapPreservesWellFormedFormula(fn, t);
        case LTLRelease(c, t, _, _) =>
            ContramapPreservesWellFormedFormula(fn, c);
            ContramapPreservesWellFormedFormula(fn, t);
        case LTLUntil(c, t, _, _) =>
            ContramapPreservesWellFormedFormula(fn, c);
            ContramapPreservesWellFormedFormula(fn, t);
        case LTLReqNext(t, _) =>
            ContramapPreservesWellFormedFormula(fn, t);
        case LTLWeakNext(t, _) =>
            ContramapPreservesWellFormedFormula(fn, t);
        case LTLStrongNext(t, _) =>
            ContramapPreservesWellFormedFormula(fn, t);
    }
}


// WellFormedness is preserved under repeated applications of an LTLBind's
// own bind function — useful for reasoning about iterated bind chains.
lemma RepeatfnPreservesWellFormed<A(!new)>(expr: LTLFormula<A>, n: nat, a: A)
    requires expr.LTLBind?
    requires WellFormedFormula(expr)
    ensures WellFormedFormula(repeatfn(expr, n, a))
    decreases n
{
    if n > 0 {
        RepeatfnPreservesWellFormed(expr, n-1, a);
    }
}


    // Contramap preserves finiteBind. Under the new finiteBind semantics
    // (`expr.LTLBind? && WellFormedFormula(expr)`), this reduces to:
    //   1) Contramap(f, expr).LTLBind?  (LTLBind constructor preserved by Contramap), and
    //   2) WellFormedFormula(Contramap(f, expr))  (Contramap preserves WellFormedness).
    // Both follow directly from helper lemmas; the previous repeatfn-chain
    // formulation is no longer needed.
    lemma FiniteBindContramapIsFinitelyBound<A(!new), B(!new)>(f: A -> B, expr: LTLFormula<B>)
        requires finiteBind(expr)
        ensures finiteBind(Contramap(f, expr))
    {
        ContramapLTLBindIfBound(f, expr);
        ContramapPreservesWellFormedFormula(f, expr);
    }

    // lemma {:isolate_assertions} LemmaRepeatBindApplication<A(!new)>(expr: LTLFormula<A>, a: A, n: nat) 
    //     requires expr.LTLBind?
    //     requires !repeatfn(expr, n, a).LTLBind? && forall m: nat :: m < n ==> repeatfn(expr, m, a).LTLBind?
    //     ensures expr.fn(a) == repeatfn(expr, n, a)
    // {}
    lemma ContramapCompositionPredicate1<A(!new), B(!new), C(!new)>(f: B -> C, g: A -> B, expr: LTLFormula<C>, a: A) 
        requires expr.LTLPred?
        ensures expr.pred(Compose(f, g)(a)) == expr.pred(f(g(a)))
    {
    }
 
    lemma ContramapCompositionComparison1<A(!new), B(!new), C(!new)>(f: B -> C, g: A -> B, expr: LTLFormula<C>, a1: A, a2: A) 
        requires expr.LTLComparison?
        ensures expr.cmp(Compose(f, g)(a1), Compose(f, g)(a2)) == expr.cmp(f(g(a1)), f(g(a2)))
    {
    }
 
    lemma ContramapCompositionBind1<A(!new), B(!new), C(!new)>(f: B -> C, g: A -> B, expr: LTLFormula<C>, a: A) 
        requires expr.LTLBind?
        ensures expr.fn(Compose(f, g)(a)) == expr.fn(f(g(a)))
    {
    }
    lemma ContramapCompositionBind<A(!new), B(!new), C(!new)>(f: A -> B, g: B -> C, expr: LTLFormula<C>)
        requires expr.LTLBind?
        requires WellFormedFormula(expr)
        // Phase 0 — strictly smaller than ContramapComposition's (..., 1) on the
        // same expr, so ContramapComposition → ContramapCompositionBind verifies.
        // Recursive call back to ContramapComposition on `expr.fn(g(f(a')))`
        // strictly decreases MaxBindDepth (the first component).
        decreases MaxBindDepth(expr), FormulaSize(expr), 0
        ensures Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr)
    {
        // The two contramapped LTLBind formulas have bind functions that agree at every argument.
        // The inductive call is on `expr.fn(g(f(a')))`, which has strictly smaller
        // MaxBindDepth than `expr` (by WellFormed's LTLBind clause: MaxBindDepth(fn(_)) < depth).
        forall a': A
            ensures Contramap(f, Contramap(g, expr)).fn(a')
                 == Contramap(Compose(g, f), expr).fn(a')
        {
            calc {
                Contramap(f, Contramap(g, expr)).fn(a');
                Contramap(f, Contramap(g, expr).fn(f(a')));
                Contramap(f, Contramap(g, expr.fn(g(f(a')))));
                { ContramapComposition(f, g, expr.fn(g(f(a')))); }
                Contramap(Compose(g, f), expr.fn(g(f(a'))));
                Contramap(Compose(g, f), expr.fn(Compose(g, f)(a')));
                Contramap(Compose(g, f), expr).fn(a');
            }
        }
        // Lifting pointwise equality of the inner bind functions to equality of
        // the LTLBind formulas requires function extensionality on the arrow type
        // A -> LTLFormula<A>. Dafny does not provide this for arrow values stored
        // in datatypes, so we close with the same axiom pattern used by
        // ContramapId (LTLBind case) and ContramapComposition (LTLBind case).
        assume {:axiom} Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);
    }

    //fmap (g . f)  ==  fmap f . fmap g
    lemma ContramapComposition<A(!new), B(!new), C(!new)>(f: A -> B, g: B -> C, expr: LTLFormula<C>)
        requires WellFormedFormula(expr)
        decreases MaxBindDepth(expr), FormulaSize(expr), 1
        ensures Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr)
    {
         match expr {
        case LTLPred(pred, tags) => 
            // Contramap(g, Contramap(f, LTLPred(pred, tags)))
            // = Contramap(g, LTLPred((b: B) => pred(f(b)), tags))
            // = LTLPred((a: A) => pred(f(g(a))), tags)
            // = LTLPred((a: A) => pred(Compose(f,g)(a)), tags)
            // = Contramap(Compose(f, g), LTLPred(pred, tags))
            //This property is not provable in Dafny because of the lack of function extensionality
            //Though we can prove it in Dafny if we take an arbitrary a and prove the property for it
            // See: ContramapCompositionPredicate(f, g, expr, a);
            assume {:axiom} Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);
            // assert Contramap(g, Contramap(f, expr)) == Contramap(Compose(f, g), expr);

        case LTLTrue(tags) =>
            // Both sides equal LTLTrue(tags)
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLFalse(tags) =>
            // Both sides equal LTLFalse(tags)  
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLAnd(t1, t2, tags) =>
            // Apply induction hypothesis to subterms
            ContramapComposition(f, g, t1);
            ContramapComposition(f, g, t2);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLOr(t1, t2, tags) =>
            ContramapComposition(f, g, t1);
            ContramapComposition(f, g, t2);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLImplies(t1, t2, tags) =>
            ContramapComposition(f, g, t1);
            ContramapComposition(f, g, t2);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLNot(t, tags) =>
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLBind(fn, depth, tags) =>
            // Delegate to ContramapCompositionBind, which does the pointwise-equality
            // proof (using the IH on `expr.fn(g(f(a')))` whose MaxBindDepth is strictly
            // less than `expr`'s) and then closes with the function-extensionality axiom.
            ContramapCompositionBind(f, g, expr);

        case LTLComparison(cmp, tags) =>
            // Contramap(g, Contramap(f, LTLComparison(cmp, tags)))
            // = Contramap(g, LTLComparison((b1: B, b2: B) => cmp(f(b1), f(b2)), tags))
            // = LTLComparison((a1: A, a2: A) => cmp(f(g(a1)), f(g(a2))), tags)
            // = LTLComparison((a1: A, a2: A) => cmp(Compose(f,g)(a1), Compose(f,g)(a2)), tags)
            // = Contramap(Compose(f, g), LTLComparison(cmp, tags))
            //This property is not provable in Dafny because of the lack of function extensionality
            // See: ContramapCompositionComparison(f, g, expr, a1, a2);
            assume {:axiom} Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);
            // assert Contramap(g, Contramap(f, expr)) == Contramap(Compose(f, g), expr);

        case LTLEventually(t, steps, tags) =>
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLAlways(t, steps, tags) =>
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLRelease(condition, t, steps, tags) =>
            ContramapComposition(f, g, condition);
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLUntil(condition, t, steps, tags) =>
            ContramapComposition(f, g, condition);
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLReqNext(t, tags) =>
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLWeakNext(t, tags) =>
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);

        case LTLStrongNext(t, tags) =>
            ContramapComposition(f, g, t);
            assert Contramap(f, Contramap(g, expr)) == Contramap(Compose(g, f), expr);
        }
    }
}
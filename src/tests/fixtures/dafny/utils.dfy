
module LTLUtils {

    function IntToString(n: int): string
        requires n >= 0  // simplified version for non-negative integers
    {
        if n == 0 then "0"
        else IntToStringHelper(n, "")
    }

    function IntToStringHelper(n: int, acc: string): string
        requires n >= 0
    {
        if n == 0 then acc
        // else if n < 0 then "-" + IntToStringHelper(-n, "")
        else IntToStringHelper(n / 10, [((n % 10) as char + '0' as char)] + acc)
    }

    predicate StringCompare(s1: string, s2: string)
    {
        |s1| < |s2| || 
        (|s1| == |s2| && 
        (s1 == s2 || exists i :: 0 <= i < |s1| && (forall j :: 0 <= j < i ==> s1[j] == s2[j]) && s1[i] < s2[i]))
    }


    lemma StringCompareTransitive(s1: string, s2: string, s3: string)
        requires StringCompare(s1, s2) && StringCompare(s2, s3)
        ensures StringCompare(s1, s3)
    {}

    lemma FirstDifferingPosition(s1: string, s2: string) returns (i: int)
        requires |s1| == |s2| && s1 != s2
        requires |s1| > 0
        ensures 0 <= i < |s1|
        ensures s1[i] != s2[i]
        ensures forall j :: 0 <= j < i ==> s1[j] == s2[j]
    {
        // Find the set of positions where strings differ
        var differing_positions := set k | 0 <= k < |s1| && s1[k] != s2[k];
        
        // Since s1 != s2 and they have same length, this set is non-empty
        assert differing_positions != {} by {
            if differing_positions == {} {
                assert forall k :: 0 <= k < |s1| ==> s1[k] == s2[k] by {
                    forall k | 0 <= k < |s1|
                        ensures s1[k] == s2[k]
                    {
                        if s1[k] != s2[k] {
                            assert k in differing_positions;
                        }
                    }
                }
                assert s1 == s2;

                assert false;
            }
        }
        
        // Take the minimum element
        i := 0;
        while i < |s1| && s1[i] == s2[i]
            invariant 0 <= i <= |s1|
            invariant forall j :: 0 <= j < i ==> s1[j] == s2[j]
        {
            i := i + 1;
        }
        
        // At this point, either i == |s1| (impossible since s1 != s2) or s1[i] != s2[i]
        assert i < |s1|; // This follows because s1 != s2 with same length
        assert s1[i] != s2[i];
    }

    lemma StringCompareTotalOrder(s1: string, s2: string)
        ensures StringCompare(s1, s2) || StringCompare(s2, s1)
    {
        if |s1| < |s2| {
            assert StringCompare(s1, s2);
        } else if |s1| > |s2| {
            assert StringCompare(s2, s1);
        } else {
            // |s1| == |s2|
            assert |s1| == |s2|;
            if s1 == s2 {
                assert StringCompare(s1, s2);
            } else {
                assert s1 != s2;
                // Find the first differing position
                    var i := FirstDifferingPosition(s1, s2);
                if s1[i] < s2[i] {
                    assert StringCompare(s1, s2);
                } else {
                    assert s1[i] > s2[i];  // since s1[i] != s2[i] and not 
                    assert StringCompare(s2, s1);
                }
            }
        }
    }


    lemma ThereIsAMinimum(s: set<string>)
        requires s != {}
        ensures exists x :: x in s && forall y :: y in s ==> StringCompare(x, y)
    {
        assert s != {};
        var x :| x in s;
        if s == {x} {
        } else {
            var s' := s - {x};
            assert s == s' + {x};
            ThereIsAMinimum(s');
            var y :| y in s' && forall z :: z in s' ==> StringCompare(y, z);
            StringCompareTotalOrder(x, y);
            if StringCompare(x, y) {
                // assert forall z :: z in s' ==> StringCompare(x, z);
                assert forall z :: z in s ==> StringCompare(x, z) by {
                    forall z | z in s
                        ensures StringCompare(x, z)
                    {
                        if z in s' {
                            StringCompareTransitive(x, y, z);
                        }
                    }
                }
            } else {
                // StringCompareAsymmetric(y,x);
                assert StringCompare(y, x);
                assert forall z :: z in s' ==> StringCompare(y, z);
                assert forall z :: z in s ==> StringCompare(y, z);
            }
            // assert StringCompare(x, y);
        }
    }

    function SetToSequence(s: set<string>): seq<string>
        ensures var q := SetToSequence(s); forall i :: 0 <= i < |q| ==> q[i] in s
        ensures |SetToSequence(s)| == |s|
        ensures forall p :: p in s ==> p in SetToSequence(s)
    {
    if s == {} then [] else
        ThereIsAMinimum(s);
        var x :| x in s && forall y :: y in s ==> StringCompare(x, y);
        [x] + SetToSequence(s - {x})
    }

	// LTL formula datatype mirroring the TS union, with tags on every variant

	// Helper to join strings with a separator
	function StringJoin(ss: seq<string>, sep: string): string
    { 
        if |ss| == 0 then ""
        else if |ss| == 1 then ss[0]
        else ss[0] + sep + StringJoin(ss[1..], sep)
    }

	// Helper to stringify a set of strings in a readable way
	function TagsToString(tags: set<string>): string
		{ if tags == {} then "{}" else "{" + StringJoin(SetToSequence(tags), ", ") + "}" }
}
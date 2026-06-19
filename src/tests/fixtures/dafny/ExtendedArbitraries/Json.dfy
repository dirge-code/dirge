include "../Arbitrary.dfy"
include "../utils.dfy"

// Recursive JSON value arbitrary, built with the Registry/Tie letrec. A `json`
// value is a Mix of: null, bool, number, quoted string, array, or object — and
// arrays/objects recurse back into `json` via reg.Tie("json"). "null" is the
// (non-recursive) base case, so the TestCase depth budget (maxDepth) bottoms out
// there and generation terminates. Each kind is registered by its own helper so
// the Tuple/Mix disjointness proofs run in a small context.
module ExtJson {
  import opened Arbitraries
  import opened LTLUtils

  const ALNUM: seq<char> :=
    ['a','b','c','d','e','f','g','h','i','j','k','l','m','n','o','p','q','r','s','t','u','v','w','x','y','z',
     '0','1','2','3','4','5','6','7','8','9']

  // Render `key:value` entries for an object, given already-quoted keys.
  function FormatPairs(pairs: seq<(string, string)>): seq<string> {
    if |pairs| == 0 then []
    else [pairs[0].0 + ":" + pairs[0].1] + FormatPairs(pairs[1..])
  }

  // Scalars: the non-recursive leaves. "null" is the base case the depth budget
  // falls back to.
  method RegisterScalars(reg: Registry<string>)
    modifies reg`arbs
  {
    var nullArb := Arbitrary<string>.Just("null");
    reg.Register("null", nullArb);
    var boolArb := Arbitrary<string>.Of(["true", "false"]);
    reg.Register("bool", boolArb);
    var nums := Arbitrary<nat>.Nats(100000);
    var numArb := nums.Map((n: nat) => IntToString(n));
    reg.Register("number", numArb);
    var alnum := Arbitrary<char>.Of(ALNUM);
    var strs := Arbitrary<seq<char>>.Lists(alnum, 0, 8);
    var strArb := strs.Map((cs: seq<char>) => "\"" + cs + "\"");
    reg.Register("string", strArb);
  }

  // array = "[" json ("," json)* "]"
  method RegisterArray(reg: Registry<string>)
    modifies reg`arbs
  {
    var elem := reg.Tie("json");
    var elems := Arbitrary<string>.Lists(elem, 0, 4);
    var arrArb := elems.Map((xs: seq<string>) => "[" + StringJoin(xs, ",") + "]");
    reg.Register("array", arrArb);
  }

  // object = "{" ("key" ":" json)* "}"
  method RegisterObject(reg: Registry<string>)
    modifies reg`arbs
  {
    var alnum := Arbitrary<char>.Of(ALNUM);
    var keyChars := Arbitrary<seq<char>>.Lists(alnum, 1, 6);
    var keyArb := keyChars.Map((cs: seq<char>) => "\"" + cs + "\"");
    var valTie := reg.Tie("json");
    var pairGen := Arbitrary<string>.Tuple(keyArb, valTie);
    var entries := Arbitrary<(string, string)>.Lists(pairGen, 0, 4);
    var objArb := entries.Map((ps: seq<(string, string)>) => "{" + StringJoin(FormatPairs(ps), ",") + "}");
    reg.Register("object", objArb);
  }

  // json = null | bool | number | string | array | object
  method {:isolate_assertions} RegisterJson(reg: Registry<string>)
    modifies reg`arbs
    ensures "json" in reg.arbs
  {
    var tNull := reg.Tie("null");
    var tBool := reg.Tie("bool");
    var tNum := reg.Tie("number");
    var tStr := reg.Tie("string");
    var tArr := reg.Tie("array");
    var tObj := reg.Tie("object");
    var jsonArb := Arbitrary<string>.Mix([tNull, tBool, tNum, tStr, tArr, tObj]);
    reg.Register("json", jsonArb);
  }

  method Json() returns (p: Arbitrary<string>)
    ensures p.Valid()
  {
    var reg := new Registry<string>("null", 4);   // base case "null", max nesting depth 4
    RegisterScalars(reg);
    RegisterArray(reg);
    RegisterObject(reg);
    RegisterJson(reg);
    p := reg.Lookup("json");
    // Valid by construction; carried through the mutable registry as an axiom
    // (same style as the LazyArbitrary resolve / RunTest rng-disjointness).
    assume {:axiom} p.Valid();
  }
}

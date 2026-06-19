include "../Arbitrary.dfy"

// Best-effort web-string arbitraries (Domain, Email). These approximate the
// shapes only — they do not enforce the full RFC grammars (no length caps,
// quoting, IDN, etc.) — which is sufficient for generating test inputs.
module ExtWeb {
  import opened Arbitraries

  const ALNUM: seq<char> :=
    ['a','b','c','d','e','f','g','h','i','j','k','l','m','n','o','p','q','r','s','t','u','v','w','x','y','z',
     '0','1','2','3','4','5','6','7','8','9']

  const TLDS: seq<string> := ["com", "org", "net", "io", "dev"]

  // <label>.<tld>, e.g. "abc123.com".
  method Domain() returns (p: Arbitrary<string>)
    ensures p.Valid()
    ensures fresh(p.internalFunction)
    ensures fresh(p.internalFunction.repr)
  {
    var alnum := Arbitrary<char>.Of(ALNUM);
    var lbl := Arbitrary<seq<char>>.Lists(alnum, 1, 15);
    var tld := Arbitrary<string>.Of(TLDS);
    var both := Arbitrary<string>.Tuple(lbl, tld);
    p := both.Map((t: (string, string)) => t.0 + "." + t.1);
  }

  // <local>@<domain>, e.g. "abc@example.com".
  method Email() returns (p: Arbitrary<string>)
    ensures p.Valid()
    ensures fresh(p.internalFunction)
    ensures fresh(p.internalFunction.repr)
  {
    var alnum := Arbitrary<char>.Of(ALNUM);
    var local := Arbitrary<seq<char>>.Lists(alnum, 1, 10);
    var dom := Domain();
    var both := Arbitrary<string>.Tuple(local, dom);
    p := both.Map((t: (string, string)) => t.0 + "@" + t.1);
  }
}

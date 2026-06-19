include "../Arbitrary.dfy"
include "../utils.dfy"

// Network-address arbitraries (IPv4, IPv6), composed from core generators.
module ExtNetwork {
  import opened Arbitraries
  import opened LTLUtils

  const HEX_DIGITS: seq<char> :=
    ['0','1','2','3','4','5','6','7','8','9','a','b','c','d','e','f']

  // Dotted-decimal IPv4, e.g. "192.168.1.1": four octets in [0,256).
  method IPv4() returns (p: Arbitrary<string>)
    ensures p.Valid()
  {
    var o0 := Arbitrary<nat>.Nats(256);
    var o1 := Arbitrary<nat>.Nats(256);
    var o2 := Arbitrary<nat>.Nats(256);
    var o3 := Arbitrary<nat>.Nats(256);
    var quad := Arbitrary<nat>.Tuple4(o0, o1, o2, o3);
    p := quad.Map((t: (nat, nat, nat, nat)) =>
      IntToString(t.0) + "." + IntToString(t.1) + "." + IntToString(t.2) + "." + IntToString(t.3));
  }

  // Best-effort full-form IPv6, e.g. "1a2b:0000:....:f00d": eight groups of
  // four hex digits joined by ":". Does not generate "::" compressed forms.
  method IPv6() returns (p: Arbitrary<string>)
    ensures p.Valid()
  {
    var hex := Arbitrary<char>.Of(HEX_DIGITS);
    var group := Arbitrary<seq<char>>.Lists(hex, 4, 4);
    var groups := Arbitrary<seq<char>>.Lists(group, 8, 8);
    p := groups.Map((gs: seq<string>) => StringJoin(gs, ":"));
  }
}

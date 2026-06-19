include "../Arbitrary.dfy"

// Identifier-shaped arbitraries (UUID, ULID), composed from the core
// generators rather than bespoke Transformables.
module ExtIdentifiers {
  import opened Arbitraries

  const HEX_DIGITS: seq<char> :=
    ['0','1','2','3','4','5','6','7','8','9','a','b','c','d','e','f']

  // Crockford base32 alphabet (excludes I, L, O, U).
  const BASE32_DIGITS: seq<char> :=
    ['0','1','2','3','4','5','6','7','8','9',
     'A','B','C','D','E','F','G','H','J','K','M','N','P','Q','R','S','T','V','W','X','Y','Z']

  // Character at index `i`, defaulting to '0' past the end, so formatting is
  // total regardless of the generated sequence length.
  function CharAt(cs: seq<char>, i: nat): char { if i < |cs| then cs[i] else '0' }

  function TakeChars(cs: seq<char>, start: nat, len: nat): string
    decreases len
  {
    if len == 0 then "" else [CharAt(cs, start)] + TakeChars(cs, start + 1, len - 1)
  }

  // 8-4-4-4-12 hex layout, e.g. "1a2b3c4d-5e6f-7081-92a3-b4c5d6e7f809".
  function FormatUUID(cs: seq<char>): string {
    TakeChars(cs, 0, 8) + "-" + TakeChars(cs, 8, 4) + "-" + TakeChars(cs, 12, 4) + "-" +
    TakeChars(cs, 16, 4) + "-" + TakeChars(cs, 20, 12)
  }

  // RFC-9562-shaped UUID string (random hex, not version/variant constrained).
  method UUID() returns (p: Arbitrary<string>)
    ensures p.Valid()
  {
    var hex := Arbitrary<char>.Of(HEX_DIGITS);
    var raw := Arbitrary<seq<char>>.Lists(hex, 32, 32);
    p := raw.Map((cs: seq<char>) => FormatUUID(cs));
  }

  // 26-character Crockford-base32 ULID (string is a seq<char>, so the
  // Lists generator already produces the right type).
  method ULID() returns (p: Arbitrary<string>)
    ensures p.Valid()
  {
    var b32 := Arbitrary<char>.Of(BASE32_DIGITS);
    p := Arbitrary<seq<char>>.Lists(b32, 26, 26);
  }
}

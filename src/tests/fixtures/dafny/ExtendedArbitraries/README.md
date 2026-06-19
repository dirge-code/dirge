# `ExtendedArbitraries/` — domain-specific generators

Higher-level string generators composed from the core combinators in
[`../Arbitrary.md`](../Arbitrary.md). Each is a `method ... returns (p: Arbitrary<string>)` with
`ensures p.Valid()`. The web/JSON generators approximate their shapes rather than enforcing the
full RFC grammars.

## `Identifiers.dfy` (module `ExtIdentifiers`)

```dafny
method UUID() returns (p: Arbitrary<string>)   // 8-4-4-4-12 hex layout, e.g. "1a2b3c4d-5e6f-7081-92a3-b4c5d6e7f809"
method ULID() returns (p: Arbitrary<string>)   // 26-char Crockford base32
```

## `Network.dfy` (module `ExtNetwork`)

```dafny
method IPv4() returns (p: Arbitrary<string>)   // dotted-decimal, e.g. "192.168.1.1"
method IPv6() returns (p: Arbitrary<string>)   // full-form, eight ":"-separated hex groups
```

## `Web.dfy` (module `ExtWeb`)  — best-effort shapes

```dafny
method Domain() returns (p: Arbitrary<string>) // <label>.<tld>, e.g. "abc123.com"
method Email() returns (p: Arbitrary<string>)  // <local>@<domain>, e.g. "abc@example.com"
```

## `Json.dfy` (module `ExtJson`)

```dafny
method Json() returns (p: Arbitrary<string>)   // recursive JSON value
```

A `json` value is a `Mix` of null / bool / number / quoted string / array / object, where
arrays and objects recurse back into `json` (built with the [`Registry`](../Arbitrary.md) letrec).
`"null"` is the base case the depth budget bottoms out at, so generation terminates — e.g.
`[{"nv9ad":null,"esiqtt":null},null,[null,null]]`.

Usage mirrors the core generators — build, then apply via a `TestCase` or hand to a run method:

```dafny
var uuid := ExtIdentifiers.UUID();
var s := uuid.Apply(tc);
```

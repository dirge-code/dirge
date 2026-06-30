// Compact fixture exercising the declaration kinds and imports that the
// semantic adapter extracts. It is intentionally not a real proof (nothing
// here is meant to verify) — its only job is to give the tree-sitter walk a
// representative range of Dafny constructs:
//   module / nested dotted module, plain + opened + aliased imports,
//   class (with fields, const, constructor, function, method),
//   trait, datatype, codatatype, newtype, type synonym, predicate,
//   function, lemma, iterator, and a method body that calls other decls.

module Features {
  import Std.Collections.Seq
  import opened Std.Wrappers
  import Lib = Some.Other.Module

  const MaxItems: nat := 100

  newtype Small = x: int | 0 <= x < 256

  type Name = string

  datatype Tree = Leaf | Node(left: Tree, value: int, right: Tree)

  codatatype Stream = Cons(head: int, tail: Stream)

  trait Shape {
    function Area(): real
    method Describe() returns (s: string)
  }

  class Circle extends Shape {
    var radius: real
    const Pi: real := 3.14159

    constructor(r: real) {
      radius := r;
    }

    function Area(): real {
      Pi * radius * radius
    }

    method Describe() returns (s: string) {
      s := "circle";
    }

    method Grow(by: real)
      modifies this
    {
      radius := radius + by;
    }
  }

  predicate IsEven(n: int) {
    n % 2 == 0
  }

  function Triple(n: int): int {
    n * 3
  }

  lemma TripleMonotonic(a: int, b: int)
    requires a <= b
    ensures Triple(a) <= Triple(b)
  {
  }

  method Main() {
    var t := Triple(7);
    var e := IsEven(t);
  }

  iterator Counter(limit: int) yields (n: int) {
  }
}

module Nested.Deep {
  function Helper(): int { 42 }
}

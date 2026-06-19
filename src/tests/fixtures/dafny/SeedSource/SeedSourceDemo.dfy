include "./SeedSource.dfy"

// Tiny runnable demo: prints two seeds (they differ between calls and across
// runs). Build/run it per target with the matching native file, e.g.
//
//   dafny run --target:cs SeedSourceDemo.dfy --input SeedSource.cs
//
// See README.md for every backend's command. Main lives in the default module
// (not its own module) so the Java backend's file-derived launcher class does
// not collide with a same-named package.
import opened SeedSource

method Main() {
  var a := GetSeed();
  var b := GetSeed();
  print "seed a = ", a, "\n";
  print "seed b = ", b, "\n";
}

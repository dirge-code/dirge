// Java implementation of SeedSource.GetSeed (bv64 -> long).
// Build/run:
//   dafny run --target:java SeedSourceDemo.dfy --input src/SeedSource/java/SeedSource/__default.java
// Java requires this file be named __default.java inside a `SeedSource/` package directory.
package SeedSource;

public class __default {
  public static long GetSeed() {
    return new java.security.SecureRandom().nextLong();
  }
}

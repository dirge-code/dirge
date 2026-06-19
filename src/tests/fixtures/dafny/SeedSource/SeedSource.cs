// C# implementation of SeedSource.GetSeed (bv64 -> ulong).
// Build/run:  dafny run --target:cs SeedSourceDemo.dfy --input SeedSource.cs
using System;

namespace SeedSource {
  public partial class __default {
    public static ulong GetSeed() {
      var bytes = new byte[8];
      System.Security.Cryptography.RandomNumberGenerator.Fill(bytes);
      return BitConverter.ToUInt64(bytes, 0);
    }
  }
}

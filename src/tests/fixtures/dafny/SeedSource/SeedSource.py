# Python implementation of SeedSource.GetSeed (bv64 -> int, masked to 64 bits).
# Build/run:  dafny run --target:py SeedSourceDemo.dfy --input SeedSource.py
import secrets


class default__:
    @staticmethod
    def GetSeed():
        return secrets.randbits(64)

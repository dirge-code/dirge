//! FNV-1a hashing, shared by the few non-cryptographic hash sites.
//!
//! FNV-1a is small, fast, and seed-free: the same bytes hash the same
//! on every call and every process. That stability is load-bearing for
//! every caller here — line hashes shown by one `read` must match the
//! next, and the semantic index / memory-db keys are persisted across
//! runs — so these functions must never grow a process-random seed.
//!
//! Two widths because callers differ: the line-hash guard folds the
//! 32-bit variant down to 12 bits for a 3-char token, while the
//! content-addressed pools want the full 64-bit space.

/// FNV-1a 32-bit hash of `bytes`.
pub fn fnv32(bytes: &[u8]) -> u32 {
    const OFFSET: u32 = 0x811c_9dc5;
    const PRIME: u32 = 0x0100_0193;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// FNV-1a 64-bit hash of `bytes`.
pub fn fnv64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical FNV-1a test vectors (the published reference values).
    #[test]
    fn fnv32_known_vectors() {
        assert_eq!(fnv32(b""), 0x811c_9dc5);
        assert_eq!(fnv32(b"a"), 0xe40c_292c);
        assert_eq!(fnv32(b"foobar"), 0xbf9c_f968);
    }

    #[test]
    fn fnv64_known_vectors() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv64(b"foobar"), 0x8594_4171_f739_67e8);
    }
}

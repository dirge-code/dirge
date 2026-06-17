//! Per-line content hashes for hash-anchored editing.
//!
//! Hash-anchored editing (see `edit_lines`) lets a model edit a file
//! by line *range* plus a tiny per-line content hash instead of
//! reproducing the exact old text. `read(line_hashes=true)` shows the
//! hash beside each line; the model echoes the hashes for the range
//! it wants to replace, and the edit tool recomputes them from disk
//! and rejects the edit if any line drifted. The hash is a *guard*
//! (did this line change since you read it?), not a locator — lines
//! are addressed by number, the hash only confirms.
//!
//! Why 3 hex chars: the model has to read and echo these, so they
//! must be short. 12 bits (4096 buckets) is plenty to catch a line
//! that changed out from under the model — a collision would require
//! a *different* line that happens to hash identically at the *same*
//! line number, which a real edit essentially never produces. Output
//! tokens, not collision resistance, is the constraint here.
//!
//! The function is FNV-1a (32-bit) folded to 12 bits and is stable
//! forever — it must not depend on a process-random seed, or a hash
//! shown by one `read` call wouldn't match the next.

/// FNV-1a 32-bit offset basis / prime.
const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

/// Content hash for a single line, as exactly 3 lowercase hex chars.
///
/// `line` is the line content with no trailing newline; callers
/// normalize CRLF to LF first so the same logical line hashes
/// identically regardless of on-disk line endings.
pub fn line_hash(line: &str) -> String {
    let mut h = FNV_OFFSET;
    for &b in line.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(FNV_PRIME);
    }
    // Fold to 12 bits: xor the top bits down so all input bits
    // influence the 3-char output.
    let folded = (h ^ (h >> 12) ^ (h >> 24)) & 0xfff;
    format!("{folded:03x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_three_lowercase_hex_chars() {
        for s in ["", "x", "fn main() {}", "    let y = 1;", "🦀 unicode"] {
            let h = line_hash(s);
            assert_eq!(h.len(), 3, "hash {h:?} for {s:?} not 3 chars");
            assert!(
                h.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
                "hash {h:?} not lowercase hex"
            );
        }
    }

    #[test]
    fn hash_is_deterministic() {
        // Stability is the whole contract: the same line must hash
        // the same on every call / process.
        assert_eq!(
            line_hash("let total = a + b;"),
            line_hash("let total = a + b;")
        );
    }

    #[test]
    fn distinct_lines_usually_differ() {
        // Not a guarantee (12-bit space), but these common cases must
        // separate or the guard is useless in practice.
        let a = line_hash("    return Ok(());");
        let b = line_hash("    return Err(e);");
        assert_ne!(a, b);
    }

    #[test]
    fn whitespace_is_significant() {
        // A re-indented line *did* change, so its hash must change —
        // the guard protects against editing stale content.
        assert_ne!(line_hash("x = 1"), line_hash("  x = 1"));
    }
}

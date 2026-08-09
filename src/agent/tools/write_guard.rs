//! Pre-write content and path guards (dirge-m8d0, dirge-4afz).
//!
//! [`super::write`] already refuses *syntactically broken* content through the
//! tree-sitter gate, writes atomically, snapshots for `/rewind`, and verifies
//! with the LSP. None of that sees the failure this module exists for: a model
//! asked to change one function regenerates the whole file from memory and
//! emits a shorter, perfectly well-formed version. Every existing check passes.
//! The only trace is the word `Wrote` instead of `Created`.
//!
//! That is also the asymmetry with `edit`, which since the read-before-edit
//! gate refuses to touch a file the session hasn't read: you cannot *modify* a
//! file you haven't looked at, but you can *replace* it with anything.
//!
//! # Why a shrink ratio and not a capability tier
//!
//! little-coder refuses `write` on ANY existing file and hands back the `edit`
//! call-shape instead; on Aider Polyglot that fires on ~57% of exercises and is
//! the single biggest lever in their scaffold. dirge can't take it flat —
//! rewriting a file wholesale is legitimate work here, and a blanket refusal
//! would fight the model on every generated file.
//!
//! The obvious narrowing is to gate on [`crate::agent::agent_loop::capability`]
//! and only refuse for a struggling model. This module deliberately does not:
//!
//!   - The shrink ratio is a *direct observation* of the failure. The tier is a
//!     proxy for who tends to commit it. `capability.rs`'s own design principle
//!     is "observe, don't assume" — and a strong model that drops 400 lines has
//!     destroyed exactly as much as a weak one.
//!   - The tier lives in `GateTally`, a local of the agent loop. Reading it from
//!     a leaf tool means making it shared state and threading it through the
//!     builder — real cost in the hot loop for a weaker signal.
//!
//! If the fixed ratio proves noisy in practice, scaling it by tier is the
//! natural follow-up; the threshold is one constant.
//!
//! # The shell can still get around this
//!
//! Blocking a tool moves the model to the shell — little-coder watched `write`
//! refusals turn into `cat > main.py << 'EOF'` five times in one session. Here
//! the bypass can't be closed the same way: a redirect's content is the
//! command's stdout, so there is nothing to measure before it runs. What IS
//! knowable statically — the destination path — is checked for every writer,
//! shell included, by `ReservedDeviceNamePolicy` in the permission engine.
//! Content-destroying shell writes remain the business of the snapshot layer
//! and [`crate::agent::agent_loop::worktree_probe`].

use std::path::Path;

/// Files at or below this many lines are never shrink-guarded. Small files are
/// legitimately rewritten all the time (config, fixtures, a stub being filled
/// in), and the destruction a mistake causes there is small and obvious.
pub const MIN_GUARDED_LINES: usize = 30;

/// Ceiling on the pre-write baseline read.
///
/// The shrink guard needs the existing file's line count, which means reading
/// it on every overwrite — where dirge-ytu1 previously deferred that read to
/// the reject path only. Unbounded, that turns "overwrite a 900 MB log" into
/// holding the log and its replacement in memory at once.
///
/// Above this size the guard declines rather than reads. That is the right
/// trade: a multi-megabyte file is not the failure this exists for (a model
/// regenerating a source file from memory and losing most of it), and a missed
/// guard is far cheaper than an OOM in the middle of a run.
pub const MAX_BASELINE_BYTES: u64 = 8 * 1024 * 1024;

/// The file's current text, when it exists and is small enough to judge.
///
/// `None` covers all three "can't judge" cases — absent, too large, or not
/// valid UTF-8 (a binary file, where a line count means nothing anyway).
pub fn baseline_for_guard(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BASELINE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Refuse when the replacement keeps less than this fraction of the original's
/// lines. Half is deliberately generous: this is meant to catch "regenerated
/// from memory and lost most of it", not to police ordinary editing.
pub const MAX_SHRINK_RATIO: f64 = 0.5;

/// Windows reserved device names. A file whose basename *stem* is one of these
/// resolves to a DOS device rather than a real file on Windows, leaving an
/// undeletable artifact behind. The stem is what matters — `NUL.txt` and
/// `com1.log` are devices too.
///
/// Checked on every platform, not just Windows: a POSIX run must not author a
/// file that becomes a landmine the moment the repo is cloned on Windows. It is
/// also a near-certain mistake everywhere else — usually the model treating
/// `nul` the way it would treat `/dev/null`.
const RESERVED_DEVICE_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// True when `path`'s final segment names a Windows reserved device.
/// Case-insensitive, extension-insensitive.
pub fn is_reserved_device_name(path: &Path) -> bool {
    let Some(base) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let stem = base.split('.').next().unwrap_or(base).to_ascii_lowercase();
    RESERVED_DEVICE_STEMS.contains(&stem.as_str())
}

/// The refusal text for a reserved device name.
pub fn reserved_device_message(path: &Path) -> String {
    let base = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>");
    format!(
        "refused: {base:?} is a Windows reserved device name (CON, PRN, AUX, NUL, COM1-9, \
         LPT1-9, with or without an extension). On Windows this creates an undeletable \
         device-named file rather than a real one, so it is refused on every platform.\n\n\
         If you meant to discard output, don't write a file at all. If you meant a real \
         file, pick a normal name."
    )
}

/// Rewrite a root-anchored bare filename (`/notes.md`) to sit under `cwd`.
///
/// Returns `Some(rewritten)` only when the rewrite applies, `None` otherwise.
///
/// The failure this fixes is induced by our own schema: `write`'s `path` says
/// *"must be absolute, not relative"*, and a model with no obvious directory
/// anchor satisfies that by prefixing a slash. A genuine system-path write
/// always carries at least one intermediate directory (`/etc/hosts`,
/// `/tmp/x/y`), so root + a bare filename is very nearly always this mistake —
/// and on a normal box it fails with EACCES anyway, so the rewrite turns a hard
/// error into the intended write.
pub fn rewrite_root_bare_path(path: &str, cwd: &Path) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(cwd.join(rest).to_string_lossy().into_owned())
}

/// Refuse a `write` that would drop most of an existing file's content.
///
/// `existing` is the file's current text, `new_content` the replacement.
/// Returns the refusal message, or `None` when the write may proceed.
pub fn shrink_verdict(path: &str, existing: &str, new_content: &str) -> Option<String> {
    let before = existing.lines().count();
    let after = new_content.lines().count();
    if before < MIN_GUARDED_LINES {
        return None;
    }
    if (after as f64) >= (before as f64) * MAX_SHRINK_RATIO {
        return None;
    }
    let lost = before.saturating_sub(after);
    Some(format!(
        "write refused — this would replace {path} ({before} lines) with {after} lines, \
         discarding {lost}.\n\n\
         A write that drops most of a file is usually a regeneration from memory that \
         lost content it never read. If you meant to change part of the file, use `edit` \
         with the exact text to replace:\n\
         \x20 {{\"name\": \"edit\", \"input\": {{\"path\": {path:?}, \
         \"old_text\": \"<exact text currently in the file>\", \
         \"new_text\": \"<replacement>\"}}}}\n\n\
         Read the file first if you don't already have its current text — `old_text` must \
         match byte for byte, including indentation.\n\n\
         If replacing the file wholesale really is the intent, say so by removing it first \
         and then writing the new file."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reserved_device_names_are_detected_with_and_without_extension() {
        for name in ["nul", "NUL", "con.txt", "com1", "LPT9.log", "Aux"] {
            assert!(
                is_reserved_device_name(&PathBuf::from("/tmp").join(name)),
                "{name} should be reserved"
            );
        }
    }

    #[test]
    fn ordinary_names_are_not_reserved() {
        for name in [
            "console.rs",
            "nullable.py",
            "com10",
            "lpt",
            "notes.md",
            "aux.d/x.rs",
        ] {
            assert!(
                !is_reserved_device_name(&PathBuf::from("/tmp").join(name)),
                "{name} should not be reserved"
            );
        }
    }

    #[test]
    fn root_bare_path_is_rewritten_to_cwd() {
        let cwd = PathBuf::from("/home/u/proj");
        assert_eq!(
            rewrite_root_bare_path("/notes.md", &cwd).as_deref(),
            Some("/home/u/proj/notes.md")
        );
    }

    /// A real system path has an intermediate directory — leave it alone.
    #[test]
    fn genuine_absolute_paths_are_left_alone() {
        let cwd = PathBuf::from("/home/u/proj");
        for path in ["/etc/hosts", "/tmp/x/y.md", "/", "relative.md", "./x.md"] {
            assert_eq!(
                rewrite_root_bare_path(path, &cwd),
                None,
                "{path} should not be rewritten"
            );
        }
    }

    #[test]
    fn halving_a_large_file_is_refused() {
        let existing = "line\n".repeat(500);
        let new_content = "line\n".repeat(100);
        let msg = shrink_verdict("/p/src/lib.rs", &existing, &new_content)
            .expect("dropping 400 of 500 lines must be refused");
        assert!(msg.contains("500 lines"), "{msg}");
        assert!(msg.contains("100 lines"), "{msg}");
        assert!(msg.contains("discarding 400"), "{msg}");
        // The model needs the way forward, not just a refusal.
        assert!(msg.contains("\"name\": \"edit\""), "{msg}");
    }

    #[test]
    fn modest_shrink_is_allowed() {
        let existing = "line\n".repeat(100);
        let new_content = "line\n".repeat(60);
        assert!(shrink_verdict("/p/x.rs", &existing, &new_content).is_none());
    }

    #[test]
    fn growth_is_always_allowed() {
        let existing = "line\n".repeat(100);
        let new_content = "line\n".repeat(400);
        assert!(shrink_verdict("/p/x.rs", &existing, &new_content).is_none());
    }

    /// Small files are rewritten legitimately all the time; nagging there
    /// would cost more than it saves.
    #[test]
    fn small_files_are_never_guarded() {
        let existing = "line\n".repeat(MIN_GUARDED_LINES - 1);
        assert!(shrink_verdict("/p/x.rs", &existing, "").is_none());
    }

    /// Truncating a large file to nothing is the extreme of the same mistake.
    /// A file too big to hold in memory alongside its replacement is not what
    /// this guard is for, and reading it would be worse than missing it.
    #[test]
    fn an_oversized_file_yields_no_baseline() {
        let dir = std::env::temp_dir().join(format!("dirge-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.log");
        std::fs::write(&big, vec![b'x'; (MAX_BASELINE_BYTES + 1) as usize]).unwrap();
        assert!(baseline_for_guard(&big).is_none());

        let small = dir.join("small.rs");
        std::fs::write(&small, "fn main() {}\n").unwrap();
        assert_eq!(
            baseline_for_guard(&small).as_deref(),
            Some("fn main() {}\n")
        );

        assert!(baseline_for_guard(&dir.join("absent.rs")).is_none());
        assert!(
            baseline_for_guard(&dir).is_none(),
            "a directory is not a baseline"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emptying_a_large_file_is_refused() {
        let existing = "line\n".repeat(200);
        assert!(shrink_verdict("/p/x.rs", &existing, "").is_some());
    }
}

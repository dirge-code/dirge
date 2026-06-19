//! Path resolution helpers for the permission engine.
//!
//! Provides canonicalisation and symlink resolution. `resolve_absolute`
//! is the entry point — resolves a possibly-relative path through the
//! working directory, follows symlinks, and normalises `..` / `.`
//! components. Used by the engine's path classifier and by external
//! callers that need the same canonical path the permission check ran
//! against (closing the symlink-swap TOCTOU between check and open).

use std::path::Path;

/// One-shot canonicalize for the working-directory cache. Best
/// effort: if canonicalize fails (cwd doesn't exist on disk, e.g.
/// in tests that pass a fixture path), fall back to the literal
/// string so the `starts_with` comparisons in `is_external_path`
/// still work for the literal form.
pub(crate) fn canonicalize_for_cache(working_dir: &str) -> String {
    std::fs::canonicalize(working_dir)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| working_dir.to_string())
}

/// Best-effort canonicalize a `Path`, falling back to the path itself
/// when it can't be resolved (doesn't exist on disk, permission error).
/// The `PathBuf` analogue of [`canonicalize_for_cache`] and the single
/// home for the `path.canonicalize().unwrap_or_else(|_| path.into())`
/// idiom that was copy-pasted with varied fallbacks (dirge-b2g7).
pub fn canonical_or_self(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Normalize a path string for prefix comparison. On Windows
/// `canonicalize` yields a `\\?\C:\...` verbatim path with backslashes,
/// and callers may pass `/`-vs-`\` mixed separators or differing
/// drive-letter case; left unnormalized, the slash-anchored prefix test
/// in [`path_is_within`] never matches and every in-cwd file is
/// misclassified as external. Strip the `\\?\` / `\\?\UNC\` prefix,
/// unify separators to `/`, and lowercase (NTFS is case-insensitive).
/// No-op on Unix, so the existing forward-slash logic is unchanged.
#[cfg(windows)]
fn normalize_for_prefix(s: &str) -> String {
    let stripped = s
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(s).to_string());
    stripped.replace('\\', "/").to_lowercase()
}

#[cfg(not(windows))]
fn normalize_for_prefix(s: &str) -> String {
    s.to_string()
}

/// Whether `child` is `base` itself or a path nested under it. The
/// comparison is boundary-safe — `/foo` does NOT contain `/foobar` —
/// because the prefix test requires a trailing separator. On Windows it
/// tolerates the `\\?\` verbatim prefix, `/`-vs-`\` separators, and
/// drive-letter case (see [`normalize_for_prefix`]). An empty or root
/// `base` matches nothing (callers treat "no usable cwd" as external).
pub(crate) fn path_is_within(child: &str, base: &str) -> bool {
    let child = normalize_for_prefix(child);
    let base = normalize_for_prefix(base);
    let base = base.trim_end_matches('/');
    // Refuse filesystem roots as a containing dir: empty, `/`, and the
    // Windows drive-root form (`c:` after the trailing-slash trim). Without
    // the drive-root guard, `c:/` would match every path on the drive and
    // a `/` working_dir would silently allow everything.
    if base.is_empty() || base == "/" || is_drive_root(base) {
        return false;
    }
    child == base || child.starts_with(&format!("{base}/"))
}

/// `c:` / `Z:` — a normalized drive root with no path component.
fn is_drive_root(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

pub(crate) fn resolve_absolute(path: &str, working_dir: &str) -> String {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(working_dir).join(p)
    };
    match std::fs::canonicalize(&joined) {
        Ok(canonical) => canonical.to_string_lossy().to_string(),
        Err(_) => {
            // The full path doesn't exist on disk (e.g. a write to a
            // brand-new file, possibly in a brand-new nested dir).
            // Canonicalize the DEEPEST existing ancestor so any
            // symlink in the existing prefix is resolved to its
            // realpath, then re-attach the still-nonexistent tail.
            //
            // Why the deepest ancestor and not just the immediate
            // parent: when the agent writes to `new_a/new_b/file.rs`
            // inside a symlinked cwd, neither `file.rs`'s parent
            // (`new_b`) nor its grandparent (`new_a`) exist, so the
            // old single-level parent canonicalize failed and we fell
            // through to a purely lexical result that kept the SYMLINK
            // form of the cwd. That form never matches the CWD-allow
            // rule (built from the canonical cwd), so in-project writes
            // re-prompted. Walking to the nearest existing ancestor
            // fixes that while staying inside the project subtree.
            //
            // The tail is lexically normalized FIRST (resolving `.` /
            // `..` without touching disk) so an attacker can't smuggle
            // an escape through `existing_dir/../../etc/passwd`: the
            // `..` are collapsed against the lexical components, and
            // any that climb above the existing ancestor are preserved
            // as `..` (see `lexical_normalize`), so the result escapes
            // the cwd subtree and is correctly classified external.
            let normalized = lexical_normalize(&joined);
            let mut ancestor = normalized.as_path();
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            loop {
                if let Ok(canonical_ancestor) = std::fs::canonicalize(ancestor) {
                    let mut out = canonical_ancestor;
                    for seg in tail.iter().rev() {
                        out.push(seg);
                    }
                    return out.to_string_lossy().to_string();
                }
                match (ancestor.parent(), ancestor.file_name()) {
                    (Some(parent), Some(name)) => {
                        tail.push(name.to_os_string());
                        ancestor = parent;
                    }
                    // Reached the root with nothing canonicalizable
                    // (e.g. a fully bogus working_dir). Fall back to
                    // the lexical form so the literal-prefix checks in
                    // `is_external_path` still have something to match.
                    _ => return normalized.to_string_lossy().to_string(),
                }
            }
        }
    }
}

/// Resolve `.` and `..` components of `p` without touching the
/// filesystem. `..` pops the previous `Normal` component; consecutive
/// `..` at the start (i.e. attempting to climb above root) are
/// retained as `..` so an attacker can't disguise an escape by
/// chaining enough `..` to underflow a real-path prefix check.
/// Doesn't follow symlinks — callers that need symlink resolution
/// should use `std::fs::canonicalize`; this helper exists for the
/// nonexistent-path fallback where canonicalize is impossible.
fn lexical_normalize(p: &Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for c in &out {
        buf.push(c.as_os_str());
    }
    buf
}

/// Reject paths that are clearly LLM hallucinations before they
/// trigger permission dialogs.  Relative single-segment paths
/// that are purely numeric ("1", "42") or trivially short
/// ("a", "x") are never valid file names a well-behaved
/// agent would genuinely want to use; the model is confusing
/// a counter, index, or file-descriptor number with a file
/// path.
///
/// Returns `Ok(())` for plausible paths, `Err(reason)` for
/// paths that should be hard-rejected.
#[allow(dead_code)] // Phase 4: only reached via the legacy check_path facade
pub fn validate_path(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Ok(());
    }
    // Has a directory component — plausible relative path.
    if path.contains('/') || path.contains('\\') {
        return Ok(());
    }
    // Has a file extension — plausible filename.
    if path.contains('.') {
        return Ok(());
    }
    // Just a bare name.  Reject single-segment names that are
    // purely numeric ("1", "42") or a single short token
    // with no extension ("a", "xy").
    if path.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "Refusing to use numeric path {:?}. Use an absolute path with a real file name.",
            path,
        ));
    }
    if path.chars().count() <= 2 {
        return Err(format!(
            "Refusing to use trivial path {:?}. Use an absolute path with a real file name.",
            path,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_within_basic_and_boundary() {
        assert!(path_is_within("/proj/src/a.rs", "/proj"));
        assert!(path_is_within("/proj", "/proj"));
        assert!(path_is_within("/proj/", "/proj"));
        // Boundary-safe: a sibling sharing a name prefix is NOT within.
        assert!(!path_is_within("/proj-other/a.rs", "/proj"));
        assert!(!path_is_within("/elsewhere/a.rs", "/proj"));
        // Empty / root base matches nothing.
        assert!(!path_is_within("/a", ""));
        assert!(!path_is_within("/a", "/"));
    }

    /// The Windows regression: a `\\?\C:\...` verbatim canonical path,
    /// mixed `/`-vs-`\` separators, and drive-letter case must all still
    /// resolve as in-cwd. Before the fix the slash-anchored prefix test
    /// failed on every Windows in-project file → "outside project".
    #[cfg(windows)]
    #[test]
    fn path_is_within_windows_verbatim_separators_and_case() {
        assert!(path_is_within(r"\\?\C:\proj\src\a.dfy", r"C:\proj"));
        assert!(path_is_within(r"\\?\C:\proj/src/a.dfy", r"C:\proj"));
        assert!(path_is_within(r"\\?\c:\proj\src\a.dfy", r"C:\proj"));
        assert!(path_is_within(r"C:\proj\a.dfy", r"\\?\C:\proj"));
        // Boundary still enforced under normalization.
        assert!(!path_is_within(r"\\?\C:\proj-other\a.dfy", r"C:\proj"));
    }

    /// F7: `resolve_absolute` must follow symlinks so a symlink
    /// pointing at a deny-listed path can't bypass the rule.
    #[test]
    fn resolve_absolute_follows_symlinks() {
        let dir =
            std::env::temp_dir().join(format!("dirge-f7-symlink-test-{}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real-secret.txt");
        std::fs::write(&target, "hunter2").unwrap();
        let link = dir.join("benign-name.txt");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();

        let resolved = resolve_absolute(link.to_str().unwrap(), "/");
        let expected = std::fs::canonicalize(&target)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(resolved, expected, "symlink should resolve to its target",);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: a write to a brand-new file in a brand-new
    /// NESTED directory inside a symlinked cwd must resolve through
    /// the symlink to the real path. The immediate parent doesn't
    /// exist (so single-level parent canonicalize fails); the fix
    /// walks up to the deepest existing ancestor, canonicalizes
    /// that (resolving the symlinked cwd), and re-attaches the
    /// nonexistent tail. Without this the result kept the symlink
    /// form, the CWD-allow rule (built from the canonical cwd) never
    /// matched, and in-project writes re-prompted.
    #[test]
    fn resolve_absolute_walks_to_deepest_existing_ancestor_through_symlink() {
        let base = std::env::temp_dir().join(format!("dirge-deepancestor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real_proj");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link_proj");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // newdir/sub do NOT exist yet; only `link_proj` (→ real_proj) does.
        let target = link.join("newdir/sub/file.rs");
        let resolved = resolve_absolute(target.to_str().unwrap(), link.to_str().unwrap());

        let expected = std::fs::canonicalize(&real)
            .unwrap()
            .join("newdir/sub/file.rs")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            resolved, expected,
            "deep new path under symlinked cwd must resolve to realpath form",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Security: the deepest-ancestor walk must NOT let a `..`
    /// traversal disguise an escape as internal. `..` is lexically
    /// collapsed BEFORE the ancestor canonicalize, so a path that
    /// climbs out of the (symlinked) cwd resolves outside the
    /// subtree and is classified external (→ prompt), not allowed.
    #[test]
    fn resolve_absolute_dotdot_escape_through_symlinked_cwd_still_escapes() {
        let base = std::env::temp_dir().join(format!("dirge-escape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real_proj");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link_proj");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let cwd = link.to_string_lossy().into_owned();
        // newdir doesn't exist; the `..` chain climbs above the project.
        let traversal = "newdir/../../../../../../etc/passwd";
        let resolved = resolve_absolute(traversal, &cwd);

        let cwd_canonical = std::fs::canonicalize(&real).unwrap();
        let resolved_path = std::path::PathBuf::from(&resolved);
        assert!(
            !resolved_path.starts_with(&cwd_canonical),
            "escape via .. must resolve outside the cwd subtree; got {resolved:?}",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// F7: nonexistent paths (writes to new files) must still
    /// resolve sensibly. They can't canonicalize fully but we
    /// canonicalize the parent so `/real/parent/../../etc/passwd`
    /// becomes `/etc/passwd` instead of staying lexical.
    #[test]
    fn resolve_absolute_handles_nonexistent_via_parent_canonicalize() {
        let dir =
            std::env::temp_dir().join(format!("dirge-f7-newfile-test-{}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let new_file = dir.join("does-not-exist-yet.txt");

        let resolved = resolve_absolute(new_file.to_str().unwrap(), "/");
        let expected_parent = std::fs::canonicalize(&dir).unwrap();
        let expected = expected_parent
            .join("does-not-exist-yet.txt")
            .to_string_lossy()
            .into_owned();
        assert_eq!(resolved, expected);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression for audit C3: when BOTH `canonicalize(joined)` AND
    /// `canonicalize(parent)` fail, the previous fallback returned
    /// the joined path with `..` components intact. Since
    /// `Path::starts_with` operates on path *components*, a crafted
    /// path like `/cwd/nonexistent_subdir/../../etc/passwd` would
    /// classify as internal because the first three components match
    /// `/cwd`. Attacker (LLM/agent) can synthesize such a path
    /// trivially. After the fix, `..` components are lexically
    /// resolved before the fallback returns, so the path escapes
    /// the cwd subtree.
    #[test]
    fn resolve_absolute_normalizes_dotdot_in_full_lexical_fallback() {
        let dir = std::env::temp_dir().join(format!("dirge-c3-traversal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();

        let traversal = "no_such_dir/no_such_subdir/../../../etc/passwd";

        let resolved = resolve_absolute(traversal, &cwd);

        let cwd_canonical = std::fs::canonicalize(&cwd).unwrap();
        let resolved_path = std::path::PathBuf::from(&resolved);
        assert!(
            !resolved_path.starts_with(&cwd_canonical) && !resolved_path.starts_with(&cwd),
            "lexical-fallback path-traversal should escape cwd subtree; got {:?}, cwd {:?}",
            resolved_path,
            cwd_canonical,
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── validate_path ────────────────────────────────────────────

    #[test]
    fn validate_accepts_absolute_paths() {
        assert!(validate_path("/etc/hosts").is_ok());
        assert!(validate_path("/Users/bob/src/main.rs").is_ok());
    }

    #[test]
    fn validate_accepts_relative_paths_with_separator() {
        assert!(validate_path("src/main.rs").is_ok());
        assert!(validate_path("lib/core.js").is_ok());
        assert!(validate_path("..\\windows\\path").is_ok());
    }

    #[test]
    fn validate_accepts_relative_names_with_extension() {
        assert!(validate_path("Cargo.toml").is_ok());
        assert!(validate_path("README.md").is_ok());
        assert!(validate_path("build.sh").is_ok());
    }

    #[test]
    fn validate_accepts_extensionless_names_that_are_not_trivial() {
        // Common extensionless filenames.
        assert!(validate_path("Makefile").is_ok());
        assert!(validate_path("Dockerfile").is_ok());
        assert!(validate_path("README").is_ok());
        assert!(validate_path("LICENSE").is_ok());
        assert!(validate_path("abc").is_ok());
    }

    #[test]
    fn validate_rejects_numeric_paths() {
        assert!(validate_path("1").is_err());
        assert!(validate_path("42").is_err());
        assert!(validate_path("007").is_err());
    }

    #[test]
    fn validate_rejects_short_nonsense_paths() {
        assert!(validate_path("a").is_err());
        assert!(validate_path("xy").is_err());
    }
}

//! Working-tree fingerprinting for the safe-state auto-restore coverage
//! check (dirge-uw2l.6).
//!
//! # Why this exists
//!
//! The safe-state rung ([`super::safe_state`]) shipped advisory-only because
//! auto-restore was not provably safe. `snapshots::capture` is wired into the
//! edit tools (`write`/`edit`/`apply_patch`/`edit_lines`/`edit_minified`) and
//! **not** into `bash`. So `sed -i`, a `>` redirect, `cargo fmt`, `prettier
//! --write` or any in-place formatter mutates a file with no pre-state
//! recorded. An auto restore would put the captured edits back and leave
//! those alone, producing a tree in a state that never existed — half green,
//! half post-green, quite possibly not compiling. The model would then debug
//! a mess the harness made, behind its back, while the failure streak kept
//! climbing.
//!
//! # The approach: detect, don't guess
//!
//! Rather than trying to capture what `bash` touched (which would mean
//! knowing what an arbitrary command wrote), this asks git what ACTUALLY
//! differs, by any means. At the green moment we fingerprint every file that
//! differs from `HEAD`; at abort time we fingerprint again. Anything whose
//! content changed, appeared, or vanished in between is a file mutated since
//! green — no matter which tool did it.
//!
//! Auto-restore is then allowed only when that set is a SUBSET of what the
//! snapshot store can put back ([`crate::agent::tools::snapshots::restorable_paths_after`]).
//! If even one file changed that the store never captured, coverage is
//! incomplete and the rung declines to advisory. The failure mode is "auto
//! didn't fire", never "auto left a broken tree".
//!
//! Baselining against a fingerprint taken at green — rather than against
//! `HEAD` directly — is what makes a dirty starting tree safe: uncommitted
//! work that predates the run has the same hash at both samples, so it never
//! reads as changed and is never a restore target.
//!
//! No git, or not a repo → [`fingerprint`] returns `None` and auto declines.
//! Pure detection: this module never mutates the repo or the index.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Content fingerprint of the files that differ from `HEAD`: path → blob
/// hash. `BTreeMap` so comparisons and any rendering are deterministic.
pub type TreeFingerprint = BTreeMap<PathBuf, String>;

/// Fingerprint every file currently differing from `HEAD` — modified,
/// staged, or untracked.
///
/// Uses `git status --porcelain=v1 -z --untracked-files=all` to enumerate,
/// then `git hash-object` to content-hash each one. Read-only: no index
/// writes, no worktree writes. Returns `None` when git is absent, `repo`
/// isn't a work tree, or the enumeration fails — every one of which makes
/// the caller decline auto rather than proceed blind.
pub fn fingerprint(repo: &Path) -> Option<TreeFingerprint> {
    let paths = changed_paths(repo)?;
    let mut out = TreeFingerprint::new();
    for rel in paths {
        // A deleted file hashes to None; record a sentinel so a delete still
        // reads as a change against a green state that had content.
        let hash = hash_object(repo, &rel).unwrap_or_else(|| "<absent>".to_string());
        out.insert(rel, hash);
    }
    Some(out)
}

/// Paths differing from `HEAD`, relative to the repo root.
fn changed_paths(repo: &Path) -> Option<Vec<PathBuf>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_porcelain_z(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `git status --porcelain=v1 -z` into paths.
///
/// The `-z` form is NUL-separated with no quoting, which is what makes paths
/// containing spaces, quotes or newlines safe to handle — the default
/// (non-`-z`) form C-quotes those and would need unescaping. Each record is
/// `XY<space><path>`; `--no-renames` keeps every record single-path, so
/// there is no `orig -> new` second field to consume.
fn parse_porcelain_z(stdout: &str) -> Vec<PathBuf> {
    stdout
        .split('\0')
        .filter(|rec| rec.len() > 3)
        .filter_map(|rec| rec.get(3..))
        .map(PathBuf::from)
        .collect()
}

/// Content hash of a worktree file via `git hash-object`. `None` when the
/// file is gone (a deletion) or unreadable.
fn hash_object(repo: &Path, rel: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["hash-object", "--"])
        .arg(rel)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Paths whose content differs between two fingerprints — changed, added,
/// or removed. These are the files mutated between the two samples, by any
/// tool.
pub fn changed_between(green: &TreeFingerprint, now: &TreeFingerprint) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for (path, hash) in now {
        if green.get(path) != Some(hash) {
            out.push(path.clone());
        }
    }
    // A file that differed from HEAD at green but no longer appears in
    // `now` was restored-to-HEAD or deleted since — also a change.
    for path in green.keys() {
        if !now.contains_key(path) {
            out.push(path.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Whether the snapshot store can put back everything that changed.
///
/// `mutated` comes from [`changed_between`] (repo-relative); `restorable`
/// from the snapshot store (absolute). Compared by suffix so the two path
/// shapes line up without canonicalizing every entry.
///
/// Empty `mutated` is covered — nothing to restore is trivially complete.
/// A `restorable` that is empty while `mutated` is not means the store has
/// nothing for files that demonstrably changed: not covered.
pub fn coverage_is_complete(mutated: &[PathBuf], restorable: &[PathBuf]) -> bool {
    mutated
        .iter()
        .all(|m| restorable.iter().any(|r| r.ends_with(m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(pairs: &[(&str, &str)]) -> TreeFingerprint {
        pairs
            .iter()
            .map(|(p, h)| (PathBuf::from(p), h.to_string()))
            .collect()
    }

    #[test]
    fn porcelain_z_parses_status_codes_and_keeps_spaces() {
        // ` M src/a.rs`, `?? new file.rs`, `A  src/b.rs`
        let raw = " M src/a.rs\0?? new file.rs\0A  src/b.rs\0";
        assert_eq!(
            parse_porcelain_z(raw),
            vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("new file.rs"),
                PathBuf::from("src/b.rs"),
            ]
        );
    }

    /// `-z` is unquoted, so a path with a quote or newline survives intact.
    /// The non-`-z` form would C-quote these and need unescaping.
    #[test]
    fn porcelain_z_handles_awkward_paths() {
        let raw = " M src/we\"ird.rs\0?? a\nb.rs\0";
        assert_eq!(
            parse_porcelain_z(raw),
            vec![PathBuf::from("src/we\"ird.rs"), PathBuf::from("a\nb.rs")]
        );
    }

    #[test]
    fn porcelain_z_ignores_empty_and_short_records() {
        assert!(parse_porcelain_z("").is_empty());
        assert!(parse_porcelain_z("\0\0").is_empty());
        assert!(parse_porcelain_z(" M\0").is_empty());
    }

    #[test]
    fn changed_between_detects_modify_add_and_remove() {
        let green = fp(&[("src/a.rs", "h1"), ("src/b.rs", "h2")]);
        let now = fp(&[
            ("src/a.rs", "h1"),
            ("src/b.rs", "CHANGED"),
            ("src/c.rs", "h3"),
        ]);
        assert_eq!(
            changed_between(&green, &now),
            vec![PathBuf::from("src/b.rs"), PathBuf::from("src/c.rs")]
        );
        // Dropping out of the changed-vs-HEAD set is itself a change.
        let now2 = fp(&[("src/a.rs", "h1")]);
        assert_eq!(
            changed_between(&green, &now2),
            vec![PathBuf::from("src/b.rs")]
        );
    }

    #[test]
    fn changed_between_is_empty_when_nothing_moved() {
        let green = fp(&[("src/a.rs", "h1")]);
        assert!(changed_between(&green, &green.clone()).is_empty());
        // A tree dirty from before the run stays dirty at both samples, so
        // pre-existing uncommitted work is never a restore target.
        assert!(changed_between(&green, &fp(&[("src/a.rs", "h1")])).is_empty());
    }

    /// The whole point: a file mutated by `bash` (never captured by the
    /// snapshot store) makes coverage incomplete, so auto declines.
    #[test]
    fn bash_mutated_file_breaks_coverage() {
        let mutated = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/sedded.rs")];
        let restorable = vec![PathBuf::from("/repo/src/a.rs")];
        assert!(
            !coverage_is_complete(&mutated, &restorable),
            "a file the store never captured must block auto"
        );
    }

    #[test]
    fn coverage_complete_when_store_has_every_mutated_path() {
        let mutated = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")];
        let restorable = vec![
            PathBuf::from("/repo/src/a.rs"),
            PathBuf::from("/repo/src/b.rs"),
            PathBuf::from("/repo/src/extra.rs"),
        ];
        assert!(
            coverage_is_complete(&mutated, &restorable),
            "a store superset is still complete coverage"
        );
    }

    #[test]
    fn nothing_mutated_is_trivially_covered() {
        assert!(coverage_is_complete(&[], &[]));
    }

    /// An empty store with real mutations is the decline case, not a pass —
    /// this is what stops "no snapshots" reading as "nothing to restore".
    #[test]
    fn mutations_with_empty_store_are_not_covered() {
        assert!(!coverage_is_complete(&[PathBuf::from("src/a.rs")], &[]));
    }

    // ── against a real git repo ─────────────────────────────────────────
    // The pure tests above cover parsing and set logic. These cover the
    // claim the whole feature rests on: that git actually SEES a mutation
    // made outside the edit tools.

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn temp_repo() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "dirge-wtp-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).ok()?;
        if !git(&dir, &["init", "-q"]) {
            return None; // no git available — skip
        }
        let _ = git(&dir, &["config", "user.email", "t@t"]);
        let _ = git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").ok()?;
        std::fs::write(dir.join("src/b.rs"), "fn b() {}\n").ok()?;
        git(&dir, &["add", "-A"]).then_some(())?;
        git(&dir, &["commit", "-qm", "base"]).then_some(())?;
        Some(dir)
    }

    /// The load-bearing claim: a file rewritten in place — the `sed -i` /
    /// formatter case the snapshot store is blind to — shows up as changed.
    #[test]
    fn detects_a_mutation_the_snapshot_store_would_miss() {
        let Some(repo) = temp_repo() else { return };
        let green = fingerprint(&repo).expect("git available");
        // A clean tree at green: nothing differs from HEAD.
        assert!(green.is_empty(), "clean tree fingerprints empty: {green:?}");

        // Mutate OUT OF BAND — no snapshots::capture, as `bash` would.
        std::fs::write(repo.join("src/a.rs"), "fn a() { /* sed */ }\n").unwrap();
        let now = fingerprint(&repo).expect("git available");
        let mutated = changed_between(&green, &now);
        assert_eq!(mutated, vec![PathBuf::from("src/a.rs")]);

        // The store never captured it, so coverage is incomplete → decline.
        assert!(!coverage_is_complete(&mutated, &[]));
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Work already uncommitted BEFORE the run must never become a restore
    /// target: it hashes the same at both samples, so it isn't "changed".
    #[test]
    fn pre_existing_dirty_work_is_not_a_restore_target() {
        let Some(repo) = temp_repo() else { return };
        // Dirty the tree BEFORE green.
        std::fs::write(repo.join("src/b.rs"), "fn b() { /* user's wip */ }\n").unwrap();
        let green = fingerprint(&repo).expect("git available");
        assert_eq!(green.len(), 1, "b.rs differs from HEAD at green");

        // Now mutate a DIFFERENT file after green.
        std::fs::write(repo.join("src/a.rs"), "fn a() { changed }\n").unwrap();
        let now = fingerprint(&repo).expect("git available");

        let mutated = changed_between(&green, &now);
        assert_eq!(
            mutated,
            vec![PathBuf::from("src/a.rs")],
            "the user's pre-run WIP must not be listed"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A brand-new untracked file created after green is detected too
    /// (`--untracked-files=all`), so auto can't miss a created file.
    #[test]
    fn detects_untracked_file_created_after_green() {
        let Some(repo) = temp_repo() else { return };
        let green = fingerprint(&repo).expect("git available");
        std::fs::write(repo.join("src/new.rs"), "fn n() {}\n").unwrap();
        let now = fingerprint(&repo).expect("git available");
        assert_eq!(
            changed_between(&green, &now),
            vec![PathBuf::from("src/new.rs")]
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Outside a git repo there is no ground truth, so fingerprinting fails
    /// and the caller declines auto rather than proceeding blind.
    #[test]
    fn non_repo_yields_none_so_auto_declines() {
        let dir = std::env::temp_dir().join(format!("dirge-wtp-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(fingerprint(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

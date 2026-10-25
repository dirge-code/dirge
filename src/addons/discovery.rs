//! Finds manifests and source roots on disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::domain::AddonPlan;
use super::layout;

/// How deep a search directory is walked. Deep enough for
/// `<dir>/<repo>/resources/META-INF/addons/x.edn` behind a symlink,
/// shallow enough that pointing it at a home directory cannot hang startup.
const MAX_DEPTH: usize = 6;

/// Directories never worth descending into, beside every hidden one
/// (`.git`, `.cpcache`, or a `.worktrees` checkout that would otherwise
/// offer a second copy of each manifest).
const SKIP: &[&str] = &["target", "node_modules"];

/// True for a child directory the walks do not enter.
fn skipped(name: &str) -> bool {
    name.starts_with('.') || SKIP.contains(&name)
}

/// How deep an addon's `src` is walked for its own sources: namespaces
/// nest deeper than manifests do.
const MAX_SOURCE_DEPTH: usize = 12;

/// Every manifest under `dirs` in a `META-INF/<one of manifest_dirs>`,
/// sorted so load order is stable.
pub fn manifests(dirs: &[PathBuf], manifest_dirs: &[String]) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    let keep = |path: &Path| layout::is_manifest(path, manifest_dirs);
    for dir in dirs {
        walk(dir, 0, MAX_DEPTH, &keep, &mut found);
    }
    found.into_iter().collect()
}

/// The portable source files of the repositories `manifests` come from,
/// canonical and sorted: each repository's own `src`, never the
/// `:local/root` libraries it depends on. What a reload evaluates again.
pub fn own_sources(manifests: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    for manifest in manifests {
        if let Some(src) =
            layout::repo_root(manifest).and_then(|r| r.join("src").canonicalize().ok())
        {
            walk(
                &src,
                0,
                MAX_SOURCE_DEPTH,
                &layout::is_portable_source,
                &mut found,
            );
        }
    }
    found.into_iter().collect()
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    keep: &dyn Fn(&Path) -> bool,
    found: &mut BTreeSet<PathBuf>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(skipped);
            if !skip {
                walk(&path, depth + 1, max_depth, keep, found);
            }
        } else if keep(&path) {
            found.insert(path);
        }
    }
}

/// `<repo>/deps.edn` text, when there is one.
fn deps_edn(repo: &Path) -> Option<String> {
    std::fs::read_to_string(repo.join("deps.edn")).ok()
}

/// Source roots reachable from `repo` through `:local/root` deps,
/// transitively, each visited once.
fn dependency_roots(repo: &Path, seen: &mut BTreeSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let Some(text) = deps_edn(repo) else { return };
    for local in layout::local_roots(&text) {
        let src = layout::dependency_root(repo, &local);
        let Some(dep_repo) = src.parent().map(Path::to_path_buf) else {
            continue;
        };
        let key = dep_repo.canonicalize().unwrap_or_else(|_| dep_repo.clone());
        if seen.insert(key) {
            out.push(src);
            dependency_roots(&dep_repo, seen, out);
        }
    }
}

/// True when `ns` has a `.cljc` or `.cljrs` source under one of `roots`.
pub fn has_portable_source(roots: &[PathBuf], ns: &str) -> bool {
    let candidates = layout::portable_sources(ns);
    roots
        .iter()
        .any(|root| candidates.iter().any(|rel| root.join(rel).is_file()))
}

/// Assemble the load plan: manifests found under `search_dirs`, and the
/// existing source roots their repositories and `:local/root` deps provide,
/// plus `extra_roots` from configuration. `manifest_dirs` names the
/// directories under `META-INF` that hold manifests.
pub fn plan(
    search_dirs: &[PathBuf],
    extra_roots: &[PathBuf],
    manifest_dirs: &[String],
) -> AddonPlan {
    let manifests = manifests(search_dirs, manifest_dirs);
    let mut roots: Vec<PathBuf> = extra_roots.to_vec();
    let mut seen = BTreeSet::new();
    for manifest in &manifests {
        roots.extend(layout::own_roots(manifest));
        if let Some(repo) = layout::repo_root(manifest) {
            dependency_roots(repo, &mut seen, &mut roots);
        }
    }
    let existing = roots
        .into_iter()
        .filter(|r| r.is_dir())
        .map(|r| r.canonicalize().unwrap_or(r));
    AddonPlan {
        manifests,
        source_roots: layout::merge_roots(existing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn dirge_dirs() -> Vec<String> {
        layout::default_manifest_dirs()
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "dirge-addons-discovery-{id}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn portable_source_is_found_only_as_cljc_or_cljrs() {
        let tmp = fleet();
        let roots = [tmp.path().join("addons/hd/src")];
        assert!(has_portable_source(&roots, "hd.core"));
        assert!(!has_portable_source(&roots, "hd.missing"));
        write(&tmp.path().join("addons/hd/src/hd/jvm.clj"), "(ns hd.jvm)");
        assert!(!has_portable_source(&roots, "hd.jvm"));
    }

    /// An addon beside the protocol library it depends on.
    fn fleet() -> TempDir {
        let tmp = TempDir::new();
        let root = tmp.path();
        write(
            &root.join("addons/hd/resources/META-INF/addons/hd.edn"),
            "{:addon/id \"hd\"}",
        );
        write(&root.join("addons/hd/src/hd/core.cljc"), "(ns hd.core)");
        write(
            &root.join("addons/hd/deps.edn"),
            "{:deps {x/proto {:local/root \"../../lib/proto\"}}}",
        );
        write(&root.join("lib/proto/src/p.cljc"), "(ns p)");
        write(
            &root.join("lib/proto/deps.edn"),
            "{:deps {y/base {:local/root \"../base\"}}}",
        );
        write(&root.join("lib/base/src/b.cljc"), "(ns b)");
        write(
            &root.join("addons/hd/target/META-INF/addons/stale.edn"),
            "{}",
        );
        write(
            &root.join("addons/hd/.worktrees/branch/resources/META-INF/addons/hd.edn"),
            "{:addon/id \"hd\"}",
        );
        write(
            &root.join("addons/hd/.worktrees/branch/src/hd/core.cljc"),
            "(ns hd.core)",
        );
        tmp
    }

    #[test]
    fn own_sources_are_the_repos_portable_files_only() {
        let tmp = fleet();
        let root = tmp.path().canonicalize().unwrap();
        write(&root.join("addons/hd/src/hd/jvm.clj"), "(ns hd.jvm)");
        write(
            &root.join("addons/hd/src/hd/deep/x.cljrs"),
            "(ns hd.deep.x)",
        );
        let manifest = root.join("addons/hd/resources/META-INF/addons/hd.edn");
        assert_eq!(
            own_sources(&[manifest]),
            vec![
                root.join("addons/hd/src/hd/core.cljc"),
                root.join("addons/hd/src/hd/deep/x.cljrs"),
            ],
            "no .clj, nothing from lib/ (a dependency), nothing hidden"
        );
    }

    #[test]
    fn plans_manifests_and_transitive_local_roots() {
        let tmp = fleet();
        let root = tmp.path().canonicalize().unwrap();
        let plan = plan(&[root.join("addons")], &[], &dirge_dirs());
        assert_eq!(
            plan.manifests,
            vec![root.join("addons/hd/resources/META-INF/addons/hd.edn")]
        );
        assert_eq!(
            plan.source_roots,
            vec![
                root.join("addons/hd/resources"),
                root.join("addons/hd/src"),
                root.join("lib/base/src"),
                root.join("lib/proto/src"),
            ]
        );
    }

    #[test]
    fn missing_dirs_and_roots_are_skipped_not_errors() {
        let tmp = fleet();
        let plan = plan(
            &[tmp.path().join("nowhere")],
            &[tmp.path().join("also-nowhere")],
            &dirge_dirs(),
        );
        assert!(plan.is_empty());
        assert!(plan.source_roots.is_empty());
    }

    #[test]
    fn a_dependency_cycle_terminates() {
        let tmp = TempDir::new();
        let root = tmp.path();
        write(
            &root.join("a/resources/META-INF/addons/a.edn"),
            "{:addon/id \"a\"}",
        );
        write(
            &root.join("a/deps.edn"),
            "{:deps {b/b {:local/root \"../b\"}}}",
        );
        write(
            &root.join("b/deps.edn"),
            "{:deps {a/a {:local/root \"../a\"}}}",
        );
        std::fs::create_dir_all(root.join("a/src")).unwrap();
        std::fs::create_dir_all(root.join("b/src")).unwrap();
        let plan = plan(&[root.to_path_buf()], &[], &dirge_dirs());
        assert_eq!(plan.manifests.len(), 1);
        assert_eq!(plan.source_roots.len(), 3);
    }
}

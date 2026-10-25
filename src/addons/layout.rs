//! Addon filesystem layout:
//!
//! ```text
//! <repo>/deps.edn                             :local/root deps
//! <repo>/src/...                              the .cljc sources
//! <repo>/resources/META-INF/addons/<id>.edn   the manifest
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Directories under `META-INF` that hold manifests by default: dirge's
/// `addons` and hive-addon's own `hive-addons` (its mount layout). Others
/// are added by `addons.manifest_dirs`.
pub const MANIFEST_DIRS: [&str; 2] = ["addons", "hive-addons"];

/// [`MANIFEST_DIRS`] as the owned list [`is_manifest`] takes.
pub fn default_manifest_dirs() -> Vec<String> {
    MANIFEST_DIRS.iter().map(|d| d.to_string()).collect()
}

/// True when `path` is `.../META-INF/<one of manifest_dirs>/<name>.edn`.
pub fn is_manifest(path: &Path, manifest_dirs: &[String]) -> bool {
    let is_edn = path.extension().is_some_and(|e| e == "edn");
    let mut dirs = path.parent().into_iter().flat_map(Path::iter).rev();
    let parent = dirs.next();
    let grandparent = dirs.next();
    is_edn
        && parent.is_some_and(|p| manifest_dirs.iter().any(|d| p == d.as_str()))
        && grandparent.is_some_and(|g| g == "META-INF")
}

/// The resources root a manifest sits in (the parent of `META-INF`).
pub fn resources_root(manifest: &Path) -> Option<&Path> {
    manifest.parent()?.parent()?.parent()
}

/// The addon repository: the parent of `resources/`, or the resources root
/// itself when the manifest is not under a directory named `resources`.
pub fn repo_root(manifest: &Path) -> Option<&Path> {
    let resources = resources_root(manifest)?;
    if resources.file_name().is_some_and(|n| n == "resources") {
        resources.parent()
    } else {
        Some(resources)
    }
}

/// Source roots a manifest's own repository contributes, before existence
/// is checked: `<repo>/src` and the resources root.
pub fn own_roots(manifest: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(repo) = repo_root(manifest) {
        roots.push(repo.join("src"));
    }
    if let Some(resources) = resources_root(manifest) {
        roots.push(resources.to_path_buf());
    }
    roots
}

/// Every `:local/root "<dir>"` in a deps.edn text, in order of appearance.
pub fn local_roots(deps_edn: &str) -> Vec<String> {
    const KEY: &str = ":local/root";
    let mut out = Vec::new();
    let mut rest = deps_edn;
    while let Some(at) = rest.find(KEY) {
        rest = &rest[at + KEY.len()..];
        let trimmed = rest.trim_start();
        if let Some(body) = trimmed.strip_prefix('"')
            && let Some(end) = body.find('"')
        {
            out.push(body[..end].to_string());
        }
    }
    out
}

/// A `:local/root` dependency's source root, resolved against the repo that
/// declared it.
pub fn dependency_root(repo: &Path, local_root: &str) -> PathBuf {
    let dep = Path::new(local_root);
    let dep = if dep.is_absolute() {
        dep.to_path_buf()
    } else {
        repo.join(dep)
    };
    dep.join("src")
}

/// Relative paths a namespace can load from in cljrs: `a.b-c.d` gives
/// `a/b_c/d.cljc` and `a/b_c/d.cljrs`.
pub fn portable_sources(ns: &str) -> [PathBuf; 2] {
    let stem = ns.replace('.', "/").replace('-', "_");
    [
        PathBuf::from(format!("{stem}.cljc")),
        PathBuf::from(format!("{stem}.cljrs")),
    ]
}

/// True for a file cljrs can load a namespace from.
pub fn is_portable_source(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e == "cljc" || e == "cljrs")
}

/// The namespace `file` holds when loaded from `root`, the inverse of
/// [`portable_sources`]: `root/a/b_c/d.cljc` is `a.b-c.d`. `None` when the
/// file is not a portable source under `root`.
pub fn namespace_of(root: &Path, file: &Path) -> Option<String> {
    if !is_portable_source(file) {
        return None;
    }
    let rel = file.strip_prefix(root).ok()?.with_extension("");
    let parts: Vec<String> = rel
        .iter()
        .map(|p| p.to_str().map(|s| s.replace('_', "-")))
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("."))
}

/// The namespace `file` holds under the most specific of `roots` that
/// contains it, so `<repo>/src` wins over `<repo>` whatever their order.
/// `None` when no root contains it as a portable source.
pub fn namespace_in(roots: &[PathBuf], file: &Path) -> Option<String> {
    roots
        .iter()
        .filter_map(|root| namespace_of(root, file).map(|ns| (root.components().count(), ns)))
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, ns)| ns)
}

/// Sorted, de-duplicated source roots.
pub fn merge_roots<I: IntoIterator<Item = PathBuf>>(roots: I) -> Vec<PathBuf> {
    roots
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "/w/my-addon/resources/META-INF/addons/my-addon.edn";

    #[test]
    fn namespace_of_inverts_portable_sources() {
        let root = Path::new("/w/a/src");
        for ns in ["my-addon.core", "a.b-c.d", "x"] {
            for rel in portable_sources(ns) {
                assert_eq!(namespace_of(root, &root.join(rel)).as_deref(), Some(ns));
            }
        }
        assert_eq!(namespace_of(root, Path::new("/w/a/src/a/jvm.clj")), None);
        assert_eq!(namespace_of(root, Path::new("/elsewhere/a.cljc")), None);
    }

    /// `<repo>` and `<repo>/src` are both roots of a flat-layout addon.
    #[test]
    fn namespace_in_names_a_file_by_the_most_specific_root() {
        let file = Path::new("/w/flat/src/flat/core.cljc");
        let roots = [PathBuf::from("/w/flat"), PathBuf::from("/w/flat/src")];
        assert_eq!(namespace_in(&roots, file).as_deref(), Some("flat.core"));
        let reversed = [roots[1].clone(), roots[0].clone()];
        assert_eq!(namespace_in(&reversed, file).as_deref(), Some("flat.core"));
        assert_eq!(namespace_in(&roots, Path::new("/elsewhere/a.cljc")), None);
        assert_eq!(
            namespace_in(&roots, Path::new("/w/flat/src/flat/jvm.clj")),
            None
        );
    }

    #[test]
    fn recognizes_only_manifests_under_meta_inf() {
        let dirs = default_manifest_dirs();
        assert!(is_manifest(Path::new(MANIFEST), &dirs));
        assert!(is_manifest(
            Path::new("/w/x/META-INF/hive-addons/a.edn"),
            &dirs
        ));
        assert!(!is_manifest(
            Path::new("/w/x/resources/addons/a.edn"),
            &dirs
        ));
        assert!(!is_manifest(Path::new("/w/x/META-INF/addons/a.clj"), &dirs));
        assert!(!is_manifest(Path::new("/w/x/META-INF/other/a.edn"), &dirs));
        assert!(!is_manifest(Path::new("a.edn"), &dirs));
    }

    #[test]
    fn another_hosts_manifest_dir_counts_only_when_configured() {
        let other = Path::new("/w/x/META-INF/other-addons/a.edn");
        assert!(!is_manifest(other, &default_manifest_dirs()));
        let mut dirs = default_manifest_dirs();
        dirs.push("other-addons".to_string());
        assert!(is_manifest(other, &dirs));
        assert!(is_manifest(Path::new(MANIFEST), &dirs));
    }

    #[test]
    fn repo_layout_yields_src_and_resources() {
        let m = Path::new(MANIFEST);
        assert_eq!(repo_root(m), Some(Path::new("/w/my-addon")));
        assert_eq!(
            own_roots(m),
            vec![
                PathBuf::from("/w/my-addon/src"),
                PathBuf::from("/w/my-addon/resources")
            ]
        );
    }

    #[test]
    fn flat_layout_treats_the_resources_root_as_the_repo() {
        let m = Path::new("/w/flat/META-INF/addons/flat.edn");
        assert_eq!(repo_root(m), Some(Path::new("/w/flat")));
    }

    #[test]
    fn reads_local_roots_and_ignores_other_coordinates() {
        let deps = r#"{:deps {org.example/proto {:local/root "../proto"}
                          metosin/malli {:mvn/version "0.20.1"}
                          x/y {:local/root   "/abs/y"}}}"#;
        assert_eq!(local_roots(deps), vec!["../proto", "/abs/y"]);
        assert!(local_roots("{:deps {}}").is_empty());
        assert!(local_roots(":local/root").is_empty());
    }

    #[test]
    fn portable_sources_munge_the_namespace() {
        assert_eq!(
            portable_sources("my-addon.core"),
            [
                PathBuf::from("my_addon/core.cljc"),
                PathBuf::from("my_addon/core.cljrs")
            ]
        );
    }

    #[test]
    fn dependency_roots_resolve_relative_to_the_declaring_repo() {
        let repo = Path::new("/w/my-addon");
        assert_eq!(
            dependency_root(repo, "../proto"),
            PathBuf::from("/w/my-addon/../proto/src")
        );
        assert_eq!(dependency_root(repo, "/abs/y"), PathBuf::from("/abs/y/src"));
    }

    /// The classpath is a function of the SET of roots, never of the order
    /// they were found in.
    #[test]
    fn merge_is_order_independent_and_idempotent() {
        let a = vec![
            PathBuf::from("/b"),
            PathBuf::from("/a"),
            PathBuf::from("/b"),
        ];
        let mut reversed = a.clone();
        reversed.reverse();
        assert_eq!(merge_roots(a.clone()), merge_roots(reversed));
        assert_eq!(merge_roots(merge_roots(a.clone())), merge_roots(a.clone()));
        assert_eq!(
            merge_roots(a),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }
}

//! Extension → LSP language identifier mapping.
//!
//! Returned values are the LSP `languageId` strings (see the LSP spec §3.18.1)
//! used in `textDocument/didOpen`. Unknown extensions return `"plaintext"` so
//! `notify.open` always has a well-formed payload.

use std::path::Path;

/// Returns the LSP `languageId` for the given file path.
///
/// Looks at the lowercased file extension. Files with no extension match the
/// filename (e.g. `Makefile` → `makefile`). Returns `"plaintext"` for any
/// unrecognised extension/filename.
pub fn language_for_path(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if let Some(ext) = path.extension().and_then(|s| s.to_str())
        && let Some(lang) = LANGUAGES.iter().find(|(e, _)| *e == ext.to_lowercase())
    {
        return lang.1;
    }

    // Some filenames are themselves the marker (Makefile, Dockerfile, etc).
    if let Some(lang) = FILENAMES.iter().find(|(n, _)| *n == name) {
        return lang.1;
    }
    "plaintext"
}

/// Extension → languageId. Lowercase keys; lookups lowercase the input.
const LANGUAGES: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("ts", "typescript"),
    ("tsx", "typescriptreact"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("js", "javascript"),
    ("jsx", "javascriptreact"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("py", "python"),
    ("pyi", "python"),
    ("clj", "clojure"),
    ("cljs", "clojure"),
    ("cljc", "clojure"),
    ("edn", "clojure"),
    ("bb", "clojure"),
    ("go", "go"),
    ("c", "c"),
    ("h", "c"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("ixx", "cpp"),
    ("cc", "cpp"),
    ("hpp", "cpp"),
    ("hxx", "cpp"),
    ("hh", "cpp"),
    // Objective-C / Objective-C++. `clangd` has claimed `.m`/`.mm` since the
    // audit M5 additions, but there were no languageId entries — so every
    // Objective-C file was opened as `plaintext` and clangd answered nothing,
    // exactly the Swift failure and shipped for just as long. Found by
    // `every_served_extension_has_a_language_id`, not by anyone using it.
    ("m", "objective-c"),
    ("mm", "objective-cpp"),
    ("java", "java"),
    ("rb", "ruby"),
    // ruby-lsp also claims these two, and they had the same gap as `.m`/`.mm`.
    // `Rakefile` and `.gemspec` are Ruby source; the languageId is `ruby`.
    ("rake", "ruby"),
    ("gemspec", "ruby"),
    ("sh", "shellscript"),
    ("bash", "shellscript"),
    ("zsh", "shellscript"),
    ("json", "json"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "toml"),
    ("md", "markdown"),
    ("html", "html"),
    ("css", "css"),
    ("scss", "scss"),
    ("xml", "xml"),
    ("nix", "nix"),
    ("zig", "zig"),
    ("dfy", "dafny"),
    ("cmake", "cmake"),
    // GH #778. Without this the `didOpen` for a `.swift` file carried
    // `languageId: "plaintext"`, so sourcekit-lsp accepted the document and
    // then answered nothing — the server spawned, the request succeeded, and
    // every query came back empty. See
    // `every_served_extension_has_a_language_id`.
    ("swift", "swift"),
];

const FILENAMES: &[(&str, &str)] = &[
    ("makefile", "makefile"),
    ("dockerfile", "dockerfile"),
    ("cmakelists.txt", "cmake"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lang(p: &str) -> &'static str {
        language_for_path(&PathBuf::from(p))
    }

    #[test]
    fn rs_is_rust() {
        assert_eq!(lang("src/main.rs"), "rust");
        assert_eq!(lang("main.rs"), "rust");
    }

    #[test]
    fn ts_and_tsx_are_distinct() {
        assert_eq!(lang("a.ts"), "typescript");
        assert_eq!(lang("a.tsx"), "typescriptreact");
        assert_eq!(lang("a.mts"), "typescript");
    }

    #[test]
    fn jsx_is_javascriptreact_not_javascript() {
        assert_eq!(lang("a.jsx"), "javascriptreact");
        assert_eq!(lang("a.js"), "javascript");
    }

    #[test]
    fn clojure_dialects_all_clojure() {
        for ext in &["clj", "cljs", "cljc", "edn", "bb"] {
            assert_eq!(lang(&format!("foo.{ext}")), "clojure", "ext={ext}");
        }
    }

    #[test]
    fn python_extensions() {
        assert_eq!(lang("a.py"), "python");
        assert_eq!(lang("a.pyi"), "python");
    }

    #[test]
    fn extension_lookup_is_case_insensitive() {
        // LSP language IDs are stable identifiers; pathological capitalisation
        // in filenames must not break the mapping.
        assert_eq!(lang("README.MD"), "markdown");
        assert_eq!(lang("Main.RS"), "rust");
    }

    #[test]
    fn dfy_is_dafny() {
        assert_eq!(lang("src/Spec.dfy"), "dafny");
        assert_eq!(lang("Spec.DFY"), "dafny");
    }

    #[test]
    fn unknown_extension_returns_plaintext() {
        assert_eq!(lang("a.unknown_ext_42"), "plaintext");
    }

    #[test]
    fn missing_extension_returns_plaintext() {
        assert_eq!(lang("just_a_filename"), "plaintext");
    }

    #[test]
    fn filenames_without_extension_match_by_name() {
        assert_eq!(lang("Makefile"), "makefile");
        assert_eq!(lang("Dockerfile"), "dockerfile");
        // Case insensitive.
        assert_eq!(lang("makefile"), "makefile");
        assert_eq!(lang("path/to/Makefile"), "makefile");
        // CMakeLists.txt has a generic .txt extension; it must match by filename.
        assert_eq!(lang("CMakeLists.txt"), "cmake");
        assert_eq!(lang("cmakelists.txt"), "cmake");
        assert_eq!(lang("path/to/CMakeLists.txt"), "cmake");
    }

    #[test]
    fn empty_path_returns_plaintext() {
        assert_eq!(lang(""), "plaintext");
    }
}

#[cfg(test)]
mod registry_agreement {
    use super::*;
    use std::path::PathBuf;

    /// Adding a language server takes entries in THREE tables that do not
    /// reference each other: `builtin_servers()` (who claims the extension),
    /// `default_commands()` (how to launch it), and `LANGUAGES` (the
    /// `languageId` sent in `didOpen`). Miss the third and the failure is
    /// SILENT in the worst way — the server spawns, accepts the document as
    /// `plaintext`, answers every query with nothing, and no error is raised
    /// anywhere.
    ///
    /// That is what shipped for Swift until GH #778 measured it against a real
    /// sourcekit-lsp. This derives the check from the server registry, so the
    /// next language cannot repeat it: claim an extension and you must say what
    /// language it is.
    #[test]
    fn every_served_extension_has_a_language_id() {
        for server in crate::lsp::server::builtin_servers() {
            for ext in &server.extensions {
                let probe = PathBuf::from(format!("a.{ext}"));
                let lang = language_for_path(&probe);
                assert_ne!(
                    lang, "plaintext",
                    "server {:?} claims .{ext} but LANGUAGES has no entry, so its \
                     didOpen would say plaintext and the server would answer nothing",
                    server.id
                );
            }
            for name in &server.filenames {
                let probe = PathBuf::from(name);
                assert_ne!(
                    language_for_path(&probe),
                    "plaintext",
                    "server {:?} claims {name:?} but LANGUAGES/FILENAMES has no entry",
                    server.id
                );
            }
        }
    }

    /// The other half of the same wiring: a server that claims extensions must
    /// have a launch command, or it can never start.
    #[test]
    fn every_builtin_server_has_a_launch_command() {
        let commands = crate::lsp::spawn::ProcessSpawner::default_commands();
        for server in crate::lsp::server::builtin_servers() {
            assert!(
                commands.contains_key(server.id),
                "server {:?} has no entry in default_commands()",
                server.id
            );
        }
    }
}

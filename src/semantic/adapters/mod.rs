pub mod bash;
#[cfg(feature = "semantic-c")]
mod c;
#[cfg(feature = "semantic-clojure")]
mod clojure;
#[cfg(feature = "semantic-cpp")]
mod cpp;
#[cfg(feature = "semantic-dafny")]
mod dafny;
#[cfg(feature = "semantic-elixir")]
mod elixir;
#[cfg(feature = "semantic-go")]
mod go;
#[cfg(feature = "semantic-java")]
mod java;
#[cfg(feature = "semantic-python")]
mod python;
#[cfg(feature = "semantic-ruby")]
mod ruby;
#[cfg(feature = "semantic-rust")]
mod rust;
#[cfg(feature = "semantic-sql")]
mod sql;
#[cfg(feature = "semantic-ts")]
mod typescript;

#[cfg(feature = "semantic-c")]
pub use c::CAdapter;
#[cfg(feature = "semantic-clojure")]
pub use clojure::ClojureAdapter;
#[cfg(feature = "semantic-cpp")]
pub use cpp::CppAdapter;
#[cfg(feature = "semantic-dafny")]
pub use dafny::DafnyAdapter;
#[cfg(feature = "semantic-elixir")]
pub use elixir::ElixirAdapter;
#[cfg(feature = "semantic-go")]
pub use go::GoAdapter;
#[cfg(feature = "semantic-java")]
pub use java::JavaAdapter;
#[cfg(feature = "semantic-python")]
pub use python::PythonAdapter;
#[cfg(feature = "semantic-ruby")]
pub use ruby::RubyAdapter;
#[cfg(feature = "semantic-rust")]
pub use rust::RustAdapter;
#[cfg(feature = "semantic-sql")]
pub use sql::SqlAdapter;
#[cfg(feature = "semantic-ts")]
pub use typescript::TypescriptAdapter;

use std::path::Path;

use crate::semantic::adapter::LanguageAdapter;

pub struct AdapterRegistry {
    adapters: Vec<Box<dyn LanguageAdapter>>,
}

impl AdapterRegistry {
    pub fn new(adapters: Vec<Box<dyn LanguageAdapter>>) -> Self {
        Self { adapters }
    }

    pub fn find_for_file(&self, file_path: &Path) -> Option<&dyn LanguageAdapter> {
        self.find_for_file_with_content(file_path, None)
    }

    /// Same as `find_for_file` but takes optional file content for
    /// extension tie-breaks. Used by audit L3: `.h` headers can be C
    /// or C++. With the C adapter listed first, every C++ project's
    /// public headers parsed as C and classes/namespaces silently
    /// vanished from `list_symbols`. When content is provided AND
    /// the extension is `.h`, sniff for C++-only constructs (class,
    /// namespace, template, ::) and prefer a C++ adapter if found.
    /// Pure-path callers (no content yet) fall back to the prior
    /// first-match behavior.
    pub fn find_for_file_with_content(
        &self,
        file_path: &Path,
        content: Option<&str>,
    ) -> Option<&dyn LanguageAdapter> {
        let ext = file_path.extension()?.to_str()?.to_lowercase();
        if ext == "h"
            && let Some(src) = content
            && self.looks_like_cpp_header(src)
        {
            // Prefer a C++ adapter for `.h` files whose content shows
            // C++-only tokens. Falls through to the regular search if
            // no C++ adapter is registered.
            if let Some(cpp) = self.adapters.iter().find(|a| {
                a.extensions().iter().any(|e| {
                    e.trim_start_matches('.') == "cpp" || e.trim_start_matches('.') == "hpp"
                })
            }) {
                return Some(cpp.as_ref());
            }
        }
        self.adapters
            .iter()
            .find(|a| {
                a.extensions()
                    .iter()
                    .any(|e| e.trim_start_matches('.') == ext)
            })
            .map(|a| a.as_ref())
    }

    /// Cheap sniff: scan a prefix of the source for C++-only tokens.
    /// Whole-token match against `class `, `namespace `, `template`,
    /// and `::` (scope resolution) — none of these appear in valid C
    /// outside of comments/strings, so a single hit is a strong
    /// signal. Caps the scan at 32 KiB so a huge header doesn't slow
    /// the registry call.
    ///
    /// EXT-1: pre-strip C/C++ comments and string literals before
    /// the substring check so a benign C header with a comment
    /// mentioning "class" or a string containing "::" doesn't get
    /// misclassified as C++. The stripping is single-pass and
    /// conservative — it errs toward keeping content (so we may
    /// over-classify, never under) but consumes the obvious
    /// comment/string forms that cause false positives in practice.
    fn looks_like_cpp_header(&self, src: &str) -> bool {
        const SNIFF_BYTES: usize = 32 * 1024;
        let head = if src.len() > SNIFF_BYTES {
            // Cut on a UTF-8 boundary; ASCII-only is the common case.
            let mut cut = SNIFF_BYTES;
            while cut > 0 && !src.is_char_boundary(cut) {
                cut -= 1;
            }
            &src[..cut]
        } else {
            src
        };
        let cleaned = strip_c_comments_and_strings(head);
        cleaned.contains("class ")
            || cleaned.contains("namespace ")
            || cleaned.contains("template<")
            || cleaned.contains("template <")
            || cleaned.contains("::")
    }

    pub fn all_extensions(&self) -> Vec<String> {
        self.adapters
            .iter()
            .flat_map(|a| {
                a.extensions()
                    .iter()
                    .map(|e| e.trim_start_matches('.').to_string())
            })
            .collect()
    }
}

/// Strip C/C++ block comments (`/* … */`), line comments (`// … \n`),
/// and double-quoted string literals from `src`. Used by the C++
/// header sniff so tokens inside comments/strings don't false-trigger
/// classification. Single-pass, conservative: doesn't try to handle
/// raw strings, character literals, or nested comment edge cases —
/// those can cause us to KEEP slightly more content than necessary
/// (a tolerable failure mode), never to strip valid code.
fn strip_c_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // /* … */ block comment
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push(' ');
            continue;
        }
        // // … line comment
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
            continue;
        }
        // "..." string literal (handles \" escape)
        if b == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i = (i + 1).min(bytes.len());
            out.push(' ');
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod sniff_tests {
    use super::strip_c_comments_and_strings;

    #[test]
    fn strip_block_comment() {
        let stripped = strip_c_comments_and_strings("int x; /* class Foo */ int y;");
        assert!(!stripped.contains("class"));
    }

    #[test]
    fn strip_line_comment() {
        let stripped = strip_c_comments_and_strings("int x; // namespace foo\nint y;");
        assert!(!stripped.contains("namespace"));
    }

    #[test]
    fn strip_string_literal() {
        let stripped = strip_c_comments_and_strings(r#"printf("a::b\n");"#);
        assert!(!stripped.contains("::"));
    }

    #[test]
    fn keeps_real_cpp_class() {
        let stripped = strip_c_comments_and_strings("class Foo { int x; };");
        assert!(stripped.contains("class "));
    }
}

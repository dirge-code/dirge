//! Rust binding for the vendored Mojo tree-sitter grammar.
//!
//! Upstream `tree-sitter-mojo` ships its own binding, but the crate isn't
//! published to crates.io under any name — and a git dependency makes
//! `cargo publish` reject the entire package, which would break the release
//! job. So the generated parser lives in `grammars/tree-sitter-mojo`, gets
//! compiled by `build.rs`, and this module is the binding that upstream's
//! `bindings/rust/lib.rs` would otherwise provide.
//!
//! See `grammars/tree-sitter-mojo/README.md` for provenance and how to
//! refresh the vendored sources.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    /// Defined by the vendored `parser.c`, linked in as `libtree-sitter-mojo.a`.
    fn tree_sitter_mojo() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for the Mojo grammar.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_mojo) };

#[cfg(test)]
mod tests {
    /// The binding is hand-written against a vendored, separately-generated
    /// parser, so the symbol actually resolving and producing a usable
    /// grammar is worth asserting directly — a bad vendor refresh shows up
    /// here rather than as a confusing failure in every Mojo test.
    #[test]
    fn the_vendored_grammar_loads() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("vendored Mojo grammar loads");
        let tree = parser
            .parse("fn main():\n    pass\n", None)
            .expect("parses");
        assert!(!tree.root_node().has_error());
    }
}

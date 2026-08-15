//! Swift adapter (GH #778, dirge-3cfq).
//!
//! Two things depend on this beyond symbol listing: the pre-write syntax gate
//! validates `.swift` edits through it, and `find_definition` / `list_symbols`
//! / `get_symbol_body` are tree-sitter backed rather than LSP backed — a model
//! asked to find a Swift symbol reaches for those first, and before this
//! adapter they returned nothing while the `lsp` tool answered the same
//! question correctly.
//!
//! # Reading the grammar
//!
//! `tree-sitter-swift` folds several declarations into one node kind, so the
//! shapes below are not guesses — they were dumped from the parser:
//!
//! - `class_declaration` covers `struct`, `class`, `enum`, `actor` AND
//!   `extension`. The keyword is an UNNAMED child; the name is a
//!   `type_identifier` for a declaration and a `user_type` for an extension.
//!   An extension is deliberately not emitted as a type — it declares no new
//!   type, and emitting one would duplicate the `struct`/`class` it extends —
//!   but its members ARE attributed to the type it extends, which is where a
//!   reader expects to find them.
//! - Bodies are `class_body`, `enum_class_body` or `protocol_body`.
//! - Members are `function_declaration`, `protocol_function_declaration`,
//!   `property_declaration` and `enum_entry`.
//! - `import_declaration` holds the module in an `identifier`, which may be
//!   dotted (`import struct SwiftUI.Color`).

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::node_text;
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

pub struct SwiftAdapter;

/// Access-level modifiers that make a declaration visible outside its module.
/// `internal` (the default) and `fileprivate`/`private` are not exported.
const EXPORTED_MODIFIERS: &[&str] = &["public", "open", "package"];

impl SwiftAdapter {
    /// The keyword that opened a `class_declaration`: `struct`, `class`,
    /// `enum`, `actor` or `extension`. It is an unnamed child, so
    /// `child_by_field_name` cannot reach it.
    fn declaration_keyword<'a>(&self, node: Node, source: &'a [u8]) -> &'a str {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i)
                && !child.is_named()
            {
                let text = std::str::from_utf8(&source[child.start_byte()..child.end_byte()])
                    .unwrap_or("")
                    .trim();
                if matches!(text, "struct" | "class" | "enum" | "actor" | "extension") {
                    return text;
                }
            }
        }
        ""
    }

    fn is_exported(&self, node: Node, source: &[u8]) -> bool {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i)
                && child.kind() == "modifiers"
            {
                let text = node_text(child, source);
                return EXPORTED_MODIFIERS
                    .iter()
                    .any(|m| text.split_whitespace().any(|w| w == *m));
            }
        }
        false
    }

    /// Everything up to the body, trimmed — the declaration as a reader would
    /// quote it. Swift bodies are `function_body` / `class_body` /
    /// `enum_class_body` / `protocol_body`; a protocol requirement has none,
    /// in which case the whole node IS the signature.
    fn signature(&self, node: Node, source: &[u8]) -> String {
        let mut end = node.end_byte();
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i)
                && matches!(
                    child.kind(),
                    "function_body" | "class_body" | "enum_class_body" | "protocol_body"
                )
            {
                end = child.start_byte();
                break;
            }
        }
        String::from_utf8_lossy(&source[node.start_byte()..end])
            .trim()
            .to_string()
    }

    /// The declared name. A declaration carries `type_identifier`; a
    /// function carries `simple_identifier`; an extension carries `user_type`
    /// (the type being extended).
    fn declared_name(&self, node: Node, source: &[u8]) -> Option<String> {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i)
                && matches!(
                    child.kind(),
                    "type_identifier" | "simple_identifier" | "user_type"
                )
            {
                return Some(node_text(child, source).trim().to_string());
            }
        }
        None
    }

    fn walk_body(&self, body: Node, source: &[u8], symbols: &mut Vec<Symbol>, owner: &str) {
        for i in 0..body.named_child_count() {
            let Some(child) = body.named_child(i) else {
                continue;
            };
            match child.kind() {
                "function_declaration" | "protocol_function_declaration" => {
                    if let Some(name) = self.declared_name(child, source) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Method,
                            name,
                            range: ByteRange::from(child),
                            signature: self.signature(child, source),
                            is_exported: self.is_exported(child, source),
                            parent_class: Some(owner.to_string()),
                        });
                    }
                }
                "property_declaration" => {
                    // The name lives in a `pattern`, not an identifier field.
                    if let Some(name) = self.property_name(child, source) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Variable,
                            name,
                            range: ByteRange::from(child),
                            signature: self.signature(child, source),
                            is_exported: self.is_exported(child, source),
                            parent_class: Some(owner.to_string()),
                        });
                    }
                }
                // `case good, bad` is one node naming several cases.
                "enum_entry" => {
                    for j in 0..child.named_child_count() {
                        if let Some(c) = child.named_child(j)
                            && c.kind() == "simple_identifier"
                        {
                            symbols.push(Symbol {
                                kind: SymbolKind::Variable,
                                name: node_text(c, source).trim().to_string(),
                                range: ByteRange::from(child),
                                signature: self.signature(child, source),
                                is_exported: true,
                                parent_class: Some(owner.to_string()),
                            });
                        }
                    }
                }
                // A type nested inside another type.
                "class_declaration" | "protocol_declaration" => {
                    self.push_type(child, source, symbols);
                }
                _ => {}
            }
        }
    }

    fn property_name(&self, node: Node, source: &[u8]) -> Option<String> {
        for i in 0..node.named_child_count() {
            let child = node.named_child(i)?;
            if child.kind() == "pattern" {
                // The pattern wraps a `simple_identifier`; fall back to the
                // pattern's own text for shapes this doesn't model.
                for j in 0..child.named_child_count() {
                    if let Some(inner) = child.named_child(j)
                        && inner.kind() == "simple_identifier"
                    {
                        return Some(node_text(inner, source).trim().to_string());
                    }
                }
                return Some(node_text(child, source).trim().to_string());
            }
        }
        None
    }

    fn body_of<'t>(&self, node: Node<'t>) -> Option<Node<'t>> {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i)
                && matches!(
                    child.kind(),
                    "class_body" | "enum_class_body" | "protocol_body"
                )
            {
                return Some(child);
            }
        }
        None
    }

    /// A `class_declaration` or `protocol_declaration`, plus its members.
    fn push_type(&self, node: Node, source: &[u8], symbols: &mut Vec<Symbol>) {
        let Some(name) = self.declared_name(node, source) else {
            return;
        };
        let keyword = self.declaration_keyword(node, source);
        let is_extension = keyword == "extension";

        // An extension declares no type; only its members are new. Emitting a
        // symbol here would report a second `Greeter` alongside the struct.
        if !is_extension {
            let kind = if node.kind() == "protocol_declaration" {
                SymbolKind::Interface
            } else {
                SymbolKind::Class
            };
            symbols.push(Symbol {
                kind,
                name: name.clone(),
                range: ByteRange::from(node),
                signature: self.signature(node, source),
                is_exported: self.is_exported(node, source),
                parent_class: None,
            });
        }

        if let Some(body) = self.body_of(node) {
            self.walk_body(body, source, symbols, &name);
        }
    }

    fn walk_top_level(
        &self,
        root: Node,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
    ) {
        for i in 0..root.named_child_count() {
            let Some(child) = root.named_child(i) else {
                continue;
            };
            match child.kind() {
                "import_declaration" => {
                    if let Some(module) = self.import_module(child, source) {
                        imports.push(Import {
                            // Dotted (`SwiftUI.Color`) or bare (`Foundation`);
                            // segment-based hierarchy, like Python's `os.path`.
                            kind: ImportKind::Module,
                            source: module,
                            names: Vec::new(),
                        });
                    }
                }
                "class_declaration" | "protocol_declaration" => {
                    self.push_type(child, source, symbols);
                }
                "function_declaration" => {
                    if let Some(name) = self.declared_name(child, source) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Function,
                            name,
                            range: ByteRange::from(child),
                            signature: self.signature(child, source),
                            is_exported: self.is_exported(child, source),
                            parent_class: None,
                        });
                    }
                }
                "property_declaration" => {
                    if let Some(name) = self.property_name(child, source) {
                        symbols.push(Symbol {
                            kind: SymbolKind::Variable,
                            name,
                            range: ByteRange::from(child),
                            signature: self.signature(child, source),
                            is_exported: self.is_exported(child, source),
                            parent_class: None,
                        });
                    }
                }
                "typealias_declaration" => {
                    if let Some(name) = self.declared_name(child, source) {
                        symbols.push(Symbol {
                            kind: SymbolKind::TypeAlias,
                            name,
                            range: ByteRange::from(child),
                            signature: self.signature(child, source),
                            is_exported: self.is_exported(child, source),
                            parent_class: None,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn import_module(&self, node: Node, source: &[u8]) -> Option<String> {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i)
                && child.kind() == "identifier"
            {
                return Some(node_text(child, source).trim().to_string());
            }
        }
        None
    }
}

impl LanguageAdapter for SwiftAdapter {
    fn extensions(&self) -> &[&str] {
        &[".swift"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_swift::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;

        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let mut warnings = Vec::new();

        if root.has_error() {
            warnings.push("tree-sitter reported syntax errors".to_string());
        }

        self.walk_top_level(root, source_bytes, &mut symbols, &mut imports);

        let exports = symbols
            .iter()
            .filter(|s| s.is_exported)
            .map(|s| s.name.clone())
            .collect();

        Ok(ExtractedFile {
            file_path: file_path.to_path_buf(),
            symbols,
            imports,
            exports,
            warnings,
            mtime: std::time::SystemTime::now(),
            size: 0,
            head_hash: 0,
        })
    }

    fn find_callees_in_range(
        &self,
        source: &str,
        _file_path: &Path,
        range: ByteRange,
    ) -> Result<Vec<String>, String> {
        let lang: tree_sitter::Language = tree_sitter_swift::LANGUAGE.into();
        // Two shapes, dumped from the parser. A bare call has the name as a
        // DIRECT child of `call_expression`, so an argument that happens to be
        // an identifier (`greet(x)` → `x`) cannot match. A method call wraps it
        // in a `navigation_expression`, and for a chain (`a.b.c()`) the
        // `navigation_suffix` that is a direct child of the outermost
        // navigation expression is the one being CALLED — `c`, not `b`.
        //
        // Same gap the Python adapter had to close (B3-7): capturing only the
        // bare form silently drops every method call.
        let query_str = r#"
            (call_expression (simple_identifier) @callee)
            (call_expression (navigation_expression (navigation_suffix (simple_identifier) @callee)))
        "#;
        crate::semantic::common::run_callee_query(&lang, query_str, source, range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
import Foundation
import struct SwiftUI.Color

public struct Greeter {
    public let name: String
    public func greet(_ who: String) -> String { "hi" }
    private func secret() -> Int { 0 }
}

public protocol Speaker {
    func speak() -> String
}

public enum Mood { case good, bad }

extension Greeter: Speaker {
    public func speak() -> String { "yo" }
}

final class Loud {
    func shout() {}
}

public func topLevel(x: Int) -> Int { x }
var counter = 0
"#;

    fn extract() -> ExtractedFile {
        SwiftAdapter
            .extract(Path::new("Greeter.swift"), SRC)
            .expect("swift source parses")
    }

    fn named<'a>(f: &'a ExtractedFile, name: &str) -> Vec<&'a Symbol> {
        f.symbols.iter().filter(|s| s.name == name).collect()
    }

    #[test]
    fn types_and_their_kinds() {
        let f = extract();
        assert!(f.warnings.is_empty(), "clean source: {:?}", f.warnings);
        assert_eq!(named(&f, "Greeter")[0].kind, SymbolKind::Class);
        assert_eq!(named(&f, "Speaker")[0].kind, SymbolKind::Interface);
        assert_eq!(named(&f, "Mood")[0].kind, SymbolKind::Class);
        assert_eq!(named(&f, "topLevel")[0].kind, SymbolKind::Function);
    }

    /// `struct`, `class`, `enum`, `actor` and `extension` are ALL
    /// `class_declaration` in this grammar. An extension declares no type, so
    /// emitting one would report a second `Greeter` beside the struct — and a
    /// reader asking "where is Greeter defined" would get two answers, one of
    /// them wrong.
    #[test]
    fn an_extension_is_not_a_second_declaration_of_its_type() {
        let f = extract();
        let greeters = named(&f, "Greeter");
        assert_eq!(
            greeters.len(),
            1,
            "expected one Greeter type, got {:?}",
            greeters.iter().map(|s| &s.signature).collect::<Vec<_>>()
        );
        assert!(greeters[0].signature.starts_with("public struct Greeter"));
    }

    /// …but its members belong to the type it extends, which is where someone
    /// looking for `speak` on a `Greeter` expects to find it.
    #[test]
    fn extension_members_attribute_to_the_extended_type() {
        let f = extract();
        let speak: Vec<_> = f
            .symbols
            .iter()
            .filter(|s| s.name == "speak" && s.parent_class.as_deref() == Some("Greeter"))
            .collect();
        assert_eq!(speak.len(), 1, "extension method attributes to Greeter");
        assert_eq!(speak[0].kind, SymbolKind::Method);
    }

    #[test]
    fn members_are_methods_of_their_type() {
        let f = extract();
        let greet = named(&f, "greet");
        assert_eq!(greet[0].kind, SymbolKind::Method);
        assert_eq!(greet[0].parent_class.as_deref(), Some("Greeter"));
        assert_eq!(
            greet[0].signature,
            "public func greet(_ who: String) -> String"
        );
        // A protocol requirement has no body; the whole declaration is the
        // signature.
        let speak_req: Vec<_> = f
            .symbols
            .iter()
            .filter(|s| s.name == "speak" && s.parent_class.as_deref() == Some("Speaker"))
            .collect();
        assert_eq!(speak_req.len(), 1);
        assert_eq!(speak_req[0].signature, "func speak() -> String");
    }

    /// Swift's default access level is `internal` — visible in the module,
    /// not outside it. Treating an unmarked declaration as exported would make
    /// `exports` meaningless, since most Swift code carries no modifier.
    #[test]
    fn only_public_declarations_are_exported() {
        let f = extract();
        assert!(named(&f, "Greeter")[0].is_exported);
        assert!(named(&f, "topLevel")[0].is_exported);
        assert!(!named(&f, "Loud")[0].is_exported, "no modifier = internal");
        assert!(!named(&f, "secret")[0].is_exported, "private");
        assert!(!named(&f, "counter")[0].is_exported, "no modifier");
        assert!(f.exports.contains(&"Greeter".to_string()));
        assert!(!f.exports.contains(&"Loud".to_string()));
    }

    /// `case good, bad` is ONE node naming two cases; a per-node symbol would
    /// lose `bad`.
    #[test]
    fn every_enum_case_gets_a_symbol() {
        let f = extract();
        for case in ["good", "bad"] {
            let hits = named(&f, case);
            assert_eq!(hits.len(), 1, "case {case} missing");
            assert_eq!(hits[0].parent_class.as_deref(), Some("Mood"));
        }
    }

    #[test]
    fn imports_carry_the_module_including_dotted_ones() {
        let f = extract();
        let modules: Vec<&str> = f.imports.iter().map(|i| i.source.as_str()).collect();
        assert!(modules.contains(&"Foundation"), "{modules:?}");
        assert!(modules.contains(&"SwiftUI.Color"), "{modules:?}");
        assert!(f.imports.iter().all(|i| i.kind == ImportKind::Module));
    }

    /// The syntax gate depends on this: a broken edit must be REPORTED as
    /// broken, or the gate passes everything through.
    #[test]
    fn a_syntax_error_is_reported_as_a_warning() {
        let broken = "public struct Greeter {\n    public func greet( -> String {\n";
        let f = SwiftAdapter
            .extract(Path::new("Broken.swift"), broken)
            .expect("the adapter must not error out on invalid source");
        assert!(
            !f.warnings.is_empty(),
            "invalid Swift must produce a warning, or the syntax gate is blind"
        );
    }
}

#[cfg(test)]
mod callee_tests {
    use super::*;

    /// Capturing only the bare form silently drops every method call — the
    /// exact gap the Python adapter had (B3-7). A chain resolves to the name
    /// actually being CALLED, not an intermediate link.
    #[test]
    fn callees_cover_bare_method_and_chained_calls() {
        let src = "func f() { greet(x); obj.method(1); Type.make(); a.b.c() }";
        let range = ByteRange {
            start_byte: 0,
            end_byte: src.len(),
            start_line: 0,
            end_line: 1,
        };
        let mut found = SwiftAdapter
            .find_callees_in_range(src, Path::new("a.swift"), range)
            .expect("query runs");
        found.sort();
        assert!(found.contains(&"greet".to_string()), "{found:?}");
        assert!(found.contains(&"method".to_string()), "{found:?}");
        assert!(found.contains(&"make".to_string()), "{found:?}");
        assert!(found.contains(&"c".to_string()), "chained call: {found:?}");
        // An argument that happens to be an identifier is not a callee.
        assert!(
            !found.contains(&"x".to_string()),
            "argument leaked: {found:?}"
        );
        // …nor is an intermediate link in the chain.
        assert!(
            !found.contains(&"b".to_string()),
            "chain link leaked: {found:?}"
        );
    }
}

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::node_text;
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

/// Mojo. The grammar (lsh/tree-sitter-mojo) is a fork of tree-sitter-python,
/// so the shape of this adapter follows `PythonAdapter` closely; the
/// differences are the Mojo-only definition kinds:
///
/// - `struct Foo:` parses as `class_definition` (the grammar treats `class`
///   and `struct` as one rule), so structs come for free.
/// - `trait_definition` → [`SymbolKind::Interface`].
/// - `__extension Foo:` (`extension_definition`) → its methods, attributed to
///   the extended type as `parent_class`.
/// - `comptime X[...] = V` (`parameterized_alias_statement`) →
///   [`SymbolKind::TypeAlias`].
pub struct MojoAdapter;

impl MojoAdapter {
    fn signature_from_node(&self, node: Node, source: &[u8]) -> String {
        let body = node.child_by_field_name("body");
        let end = body.map(|b| b.start_byte()).unwrap_or(node.end_byte());
        let sig_bytes = &source[node.start_byte()..end];
        String::from_utf8_lossy(sig_bytes).trim().to_string()
    }

    /// Methods inside a struct/class/trait/extension body. `container` is the
    /// enclosing type's name, recorded as `parent_class`.
    fn walk_type_body(&self, node: Node, source: &[u8], symbols: &mut Vec<Symbol>, container: &str) {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                match child.kind() {
                    "function_definition" => {
                        self.push_method(child, source, symbols, container);
                    }
                    "decorated_definition" => {
                        if let Some(inner) = child.child_by_field_name("definition")
                            && inner.kind() == "function_definition"
                        {
                            let range = ByteRange::from(child);
                            self.push_method_with_range(inner, range, source, symbols, container);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn push_method(&self, node: Node, source: &[u8], symbols: &mut Vec<Symbol>, container: &str) {
        let range = ByteRange::from(node);
        self.push_method_with_range(node, range, source, symbols, container);
    }

    fn push_method_with_range(
        &self,
        node: Node,
        range: ByteRange,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
        container: &str,
    ) {
        if let Some(name_node) = node.child_by_field_name("name") {
            let name = node_text(name_node, source).to_string();
            let signature = self.signature_from_node(node, source);
            symbols.push(Symbol {
                kind: SymbolKind::Method,
                name,
                range,
                signature,
                is_exported: false,
                parent_class: Some(container.to_string()),
            });
        }
    }

    /// Shared handler for the named type-introducing definitions
    /// (`class_definition` covers both `class` and `struct`;
    /// `trait_definition`; `extension_definition`). `range_node` differs from
    /// `node` when the definition is decorated — the symbol's range then spans
    /// the decorators too, matching the Python adapter.
    fn push_type_definition(
        &self,
        node: Node,
        range_node: Node,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        // Extensions name a possibly-generic type (`__extension List[T]:`);
        // attribute methods to the base name.
        let name = node_text(name_node, source).to_string();
        let container = name
            .split_once('[')
            .map(|(base, _)| base.trim())
            .unwrap_or(&name)
            .to_string();
        let is_exported = !name.starts_with('_') || node.kind() == "extension_definition";
        let range = ByteRange::from(range_node);
        let signature = self.signature_from_node(node, source);
        // Extensions don't declare a new type — index only their methods.
        if node.kind() != "extension_definition" {
            let kind = if node.kind() == "trait_definition" {
                SymbolKind::Interface
            } else {
                SymbolKind::Class
            };
            symbols.push(Symbol {
                kind,
                name,
                range,
                signature,
                is_exported,
                parent_class: None,
            });
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_type_body(body, source, symbols, &container);
        }
    }

    fn walk_top_level(
        &self,
        node: Node,
        source: &[u8],
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
    ) {
        for i in 0..node.named_child_count() {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            match child.kind() {
                "import_statement" => {
                    let mut names = Vec::new();
                    for j in 0..child.named_child_count() {
                        if let Some(c) = child.named_child(j) {
                            match c.kind() {
                                "dotted_name" => {
                                    names.push(node_text(c, source).to_string());
                                }
                                "aliased_import" => {
                                    if let Some(alias) = c.child_by_field_name("alias") {
                                        names.push(node_text(alias, source).to_string());
                                    } else if let Some(n) = c.child_by_field_name("name") {
                                        names.push(node_text(n, source).to_string());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    let module = child
                        .child_by_field_name("name")
                        .map(|n| node_text(n, source).to_string())
                        .unwrap_or_default();
                    if names.is_empty() && !module.is_empty() {
                        names.push(module.clone());
                    }
                    imports.push(Import {
                        names,
                        source: module,
                        kind: ImportKind::Module,
                    });
                }
                "import_from_statement" => {
                    let module = child
                        .child_by_field_name("module_name")
                        .map(|n| node_text(n, source).to_string())
                        .unwrap_or_default();
                    let mut names = Vec::new();
                    for j in 0..child.named_child_count() {
                        if let Some(c) = child.named_child(j) {
                            if c.kind() == "dotted_name" {
                                names.push(node_text(c, source).to_string());
                            } else if c.kind() == "aliased_import" {
                                if let Some(alias) = c.child_by_field_name("alias") {
                                    names.push(node_text(alias, source).to_string());
                                } else if let Some(n) = c.child_by_field_name("name") {
                                    names.push(node_text(n, source).to_string());
                                }
                            }
                        }
                    }
                    imports.push(Import {
                        names,
                        source: module,
                        kind: ImportKind::Module,
                    });
                }
                "function_definition" | "decorated_definition" => {
                    let inner = if child.kind() == "decorated_definition" {
                        child.child_by_field_name("definition")
                    } else {
                        Some(child)
                    };
                    let Some(node) = inner else { continue };
                    match node.kind() {
                        "function_definition" => {
                            if let Some(name_node) = node.child_by_field_name("name") {
                                let name = node_text(name_node, source).to_string();
                                // Same convention as Python: dunders are the
                                // public protocol despite the underscores.
                                let is_dunder = name.starts_with("__") && name.ends_with("__");
                                let is_exported = is_dunder || !name.starts_with('_');
                                let range = ByteRange::from(child);
                                let signature = self.signature_from_node(node, source);
                                symbols.push(Symbol {
                                    kind: SymbolKind::Function,
                                    name,
                                    range,
                                    signature,
                                    is_exported,
                                    parent_class: None,
                                });
                            }
                        }
                        // `@value struct Foo:` / `@register_passable trait …`
                        // — decorated type definitions are the norm in Mojo,
                        // unlike Python where the adapter skips them.
                        "class_definition" | "trait_definition" | "extension_definition" => {
                            self.push_type_definition(node, child, source, symbols);
                        }
                        _ => {}
                    }
                }
                "class_definition" | "trait_definition" | "extension_definition" => {
                    self.push_type_definition(child, child, source, symbols);
                }
                // `comptime Name[params...] = value` — a parameterized
                // compile-time alias (what `alias` became).
                "parameterized_alias_statement" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(name_node, source).to_string();
                        let is_exported = !name.starts_with('_');
                        symbols.push(Symbol {
                            kind: SymbolKind::TypeAlias,
                            name,
                            range: ByteRange::from(child),
                            signature: node_text(child, source).trim().to_string(),
                            is_exported,
                            parent_class: None,
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

impl LanguageAdapter for MojoAdapter {
    fn extensions(&self) -> &[&str] {
        &[".mojo", ".🔥"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang = tree_sitter_mojo::LANGUAGE.into();
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

        let exports: Vec<String> = symbols
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
        let lang: tree_sitter::Language = tree_sitter_mojo::LANGUAGE.into();
        // Same two alternatives as Python (B3-7): direct identifier calls AND
        // attribute-access (method) calls, so `obj.method()` isn't dropped.
        let query_str = r#"
            (call function: (identifier) @callee)
            (call function: (attribute attribute: (identifier) @callee))
        "#;
        crate::semantic::common::run_callee_query(&lang, query_str, source, range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
from collections import Dict
import math

comptime Vec[dt: DType, width: Int] = SIMD[dt, width]

trait Shape:
    fn area(self) -> Float64:
        ...

@fieldwise_init
struct Pair(Copyable, Movable):
    var first: Int
    var second: Int

    fn sum(self) -> Int:
        return self.first + self.second

    fn _hidden(self) -> Int:
        return 0

__extension Pair:
    fn product(self) -> Int:
        return self.first * self.second

fn add[T: Intable](a: T, b: T) raises -> Int:
    return Int(a) + Int(b)

fn _private_helper() -> Int:
    return 1

def main():
    var p = Pair(1, 2)
    print(add(p.sum(), p.product()))
";

    fn extract() -> ExtractedFile {
        MojoAdapter
            .extract(Path::new("pair.mojo"), SRC)
            .expect("mojo source parses")
    }

    fn named<'a>(f: &'a ExtractedFile, name: &str) -> Vec<&'a Symbol> {
        f.symbols.iter().filter(|s| s.name == name).collect()
    }

    #[test]
    fn kinds_and_exports() {
        let f = extract();
        assert!(f.warnings.is_empty(), "clean source: {:?}", f.warnings);
        assert_eq!(named(&f, "Pair")[0].kind, SymbolKind::Class);
        assert_eq!(named(&f, "Shape")[0].kind, SymbolKind::Interface);
        assert_eq!(named(&f, "Vec")[0].kind, SymbolKind::TypeAlias);
        assert_eq!(named(&f, "add")[0].kind, SymbolKind::Function);
        assert_eq!(named(&f, "main")[0].kind, SymbolKind::Function);
        assert!(named(&f, "add")[0].is_exported);
        assert!(!named(&f, "_private_helper")[0].is_exported);
        assert!(f.exports.iter().any(|e| e == "Pair"));
        assert!(!f.exports.iter().any(|e| e == "_private_helper"));
    }

    /// A decorated struct is the norm in Mojo (`@fieldwise_init`,
    /// `@register_passable`); it must be indexed like an undecorated one.
    #[test]
    fn decorated_struct_is_indexed_with_methods() {
        let f = extract();
        let pairs = named(&f, "Pair");
        assert_eq!(pairs.len(), 1, "one definition for Pair: {pairs:?}");
        let sum = named(&f, "sum");
        assert_eq!(sum[0].kind, SymbolKind::Method);
        assert_eq!(sum[0].parent_class.as_deref(), Some("Pair"));
    }

    /// `__extension Pair:` declares no new type — its methods attach to the
    /// extended type, and no second `Pair` symbol appears.
    #[test]
    fn extension_methods_attach_to_extended_type() {
        let f = extract();
        let product = named(&f, "product");
        assert_eq!(product[0].kind, SymbolKind::Method);
        assert_eq!(product[0].parent_class.as_deref(), Some("Pair"));
    }

    #[test]
    fn trait_default_method_attaches_to_trait() {
        let f = extract();
        let area = named(&f, "area");
        assert_eq!(area[0].kind, SymbolKind::Method);
        assert_eq!(area[0].parent_class.as_deref(), Some("Shape"));
    }

    #[test]
    fn imports_are_collected() {
        let f = extract();
        assert!(
            f.imports
                .iter()
                .any(|i| i.source == "collections" && i.names.iter().any(|n| n == "Dict")),
            "{:?}",
            f.imports
        );
        assert!(f.imports.iter().any(|i| i.names.iter().any(|n| n == "math")));
    }

    #[test]
    fn callees_include_method_calls() {
        let f = extract();
        let main = named(&f, "main")[0];
        let callees = MojoAdapter
            .find_callees_in_range(SRC, Path::new("pair.mojo"), main.range)
            .expect("callee query runs");
        for expected in ["Pair", "print", "add", "sum", "product"] {
            assert!(
                callees.iter().any(|c| c == expected),
                "missing callee {expected}: {callees:?}"
            );
        }
    }

    /// Both extension spellings are claimed; the registry strips the dot.
    #[test]
    fn claims_both_extensions() {
        assert_eq!(MojoAdapter.extensions(), &[".mojo", ".🔥"]);
    }
}

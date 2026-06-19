use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{find_node_at_range, node_text, signature_first_line};
use crate::semantic::types::{ByteRange, ExtractedFile, Import, ImportKind, Symbol, SymbolKind};

/// Tree-sitter adapter for Dafny (`.dfy`). Uses the local
/// `tree-sitter-dafny` grammar (exports `LANGUAGE`, same shape as the
/// other grammars).
///
/// Symbol extraction walks the tree by node kind rather than using the
/// `Query` API: the grammar author flagged (in `queries/tags.scm`) that
/// querying `call_expression` trips a tree-sitter assertion failure on
/// this grammar's GLR / external-scanner structure, so `find_callees`
/// also walks manually. Every declaration exposes its identifier via a
/// `name` field, which keeps the manual walk simple.
///
/// `SymbolKind` has no Module/Field variant, so Dafny modules map to
/// `Class` (the closest "container") and fields to `Variable`. Dafny's
/// `export` sets are not modeled — every symbol is reported as exported
/// (the symbol index is used for navigation, where over-reporting is
/// harmless).
pub struct DafnyAdapter;

impl DafnyAdapter {
    fn language() -> tree_sitter::Language {
        tree_sitter_dafny::LANGUAGE.into()
    }

    fn new_parser() -> Result<Parser, String> {
        let mut parser = Parser::new();
        parser
            .set_language(&Self::language())
            .map_err(|e| format!("Failed to set language: {e}"))?;
        Ok(parser)
    }

    /// Declaration node kind → symbol kind. `None` for non-declaration
    /// nodes. `iterator_decl` is a Method (it has an implicit receiver /
    /// stateful body, closer to a method than a pure function).
    fn symbol_kind(kind: &str) -> Option<SymbolKind> {
        Some(match kind {
            "function_decl" => SymbolKind::Function,
            "method_decl" | "constructor_decl" | "iterator_decl" => SymbolKind::Method,
            "class_decl" | "datatype_decl" | "newtype_decl" | "module_definition" => {
                SymbolKind::Class
            }
            "trait_decl" => SymbolKind::Interface,
            "synonym_type_decl" => SymbolKind::TypeAlias,
            "field_decl" | "constant_field_decl" => SymbolKind::Variable,
            _ => return None,
        })
    }

    /// Type-like containers whose name becomes the `parent_class` of the
    /// members nested inside them.
    fn is_container(kind: &str) -> bool {
        matches!(
            kind,
            "class_decl" | "trait_decl" | "datatype_decl" | "module_definition"
        )
    }

    /// Dotted text of a `qualified_name` node (its `identifier` children
    /// joined with `.`), falling back to the raw node text.
    fn qualified_text(node: Node, src: &[u8]) -> String {
        let mut parts = Vec::new();
        for i in 0..node.named_child_count() {
            if let Some(c) = node.named_child(i)
                && c.kind() == "identifier"
            {
                parts.push(node_text(c, src).to_string());
            }
        }
        if parts.is_empty() {
            node_text(node, src).to_string()
        } else {
            parts.join(".")
        }
    }

    /// The declared name of a node via its `name` field. Modules carry a
    /// `qualified_name`; everything else an `identifier`.
    fn decl_name(node: Node, src: &[u8]) -> Option<String> {
        let name = node.child_by_field_name("name")?;
        if name.kind() == "qualified_name" {
            Some(Self::qualified_text(name, src))
        } else {
            Some(node_text(name, src).to_string())
        }
    }

    fn extract_import(node: Node, src: &[u8], imports: &mut Vec<Import>) {
        // `import [opened] A.B.C` / `import X = A.B`. The imported path is
        // a `qualified_name` child; fall back to the `name` identifier.
        let source = (0..node.named_child_count())
            .filter_map(|i| node.named_child(i))
            .find(|c| c.kind() == "qualified_name")
            .map(|qn| Self::qualified_text(qn, src))
            .or_else(|| {
                node.child_by_field_name("name")
                    .map(|n| node_text(n, src).to_string())
            })
            .unwrap_or_default();
        if !source.is_empty() {
            imports.push(Import {
                names: vec![source.clone()],
                source,
                kind: ImportKind::Qualified,
            });
        }
    }

    fn walk(
        &self,
        node: Node,
        src: &[u8],
        parent: Option<&str>,
        symbols: &mut Vec<Symbol>,
        imports: &mut Vec<Import>,
    ) {
        let kind = node.kind();

        if kind == "module_import" {
            Self::extract_import(node, src, imports);
            return; // no nested declarations to recurse into
        }

        // `container_name` outlives the recursion loop so `&str` borrows
        // into it stay valid for the children pass.
        let mut container_name: Option<String> = None;
        let mut child_parent = parent;

        if let Some(sym_kind) = Self::symbol_kind(kind)
            && let Some(name) = Self::decl_name(node, src)
        {
            symbols.push(Symbol {
                kind: sym_kind,
                name: name.clone(),
                range: ByteRange::from(node),
                signature: signature_first_line(node, src),
                is_exported: true,
                parent_class: parent.map(str::to_string),
            });
            if Self::is_container(kind) {
                container_name = Some(name);
            }
        }
        if let Some(ref c) = container_name {
            child_parent = Some(c.as_str());
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, src, child_parent, symbols, imports);
        }
    }

    /// Collect callee names under `node`. A call in this grammar is a
    /// `suffix_expression` with one or more `actual_binding` (argument)
    /// children; the callee is the base sub-expression. The name is the
    /// rightmost `identifier` among the non-argument children — which
    /// yields `Triple` for `Triple(x)` and `Method` for `obj.Method(x)`.
    /// Manual walk — no `Query` (the grammar asserts on call queries).
    ///
    /// Zero-argument calls (`f()`) are not detected, since without an
    /// `actual_binding` they're indistinguishable here from a plain
    /// reference; this favors precision over recall in the call graph.
    fn collect_callees(node: Node, src: &[u8], out: &mut Vec<String>) {
        if node.kind() == "suffix_expression" {
            let mut args = node.walk();
            let has_args = node
                .children(&mut args)
                .any(|c| c.kind() == "actual_binding");
            if has_args {
                let mut best: Option<Node> = None;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "actual_binding" {
                        continue;
                    }
                    if let Some(id) = Self::rightmost_identifier_node(child)
                        && best.is_none_or(|b| id.start_byte() > b.start_byte())
                    {
                        best = Some(id);
                    }
                }
                if let Some(id) = best {
                    out.push(node_text(id, src).to_string());
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_callees(child, src, out);
        }
    }

    /// The textually-last `identifier` descendant of `node` (by start
    /// byte), inclusive of `node` itself. `None` when none exists.
    fn rightmost_identifier_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
        let mut best: Option<Node> = None;
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "identifier" && best.is_none_or(|b| n.start_byte() > b.start_byte()) {
                best = Some(n);
            }
            let mut cursor = n.walk();
            for c in n.children(&mut cursor) {
                stack.push(c);
            }
        }
        best
    }
}

impl LanguageAdapter for DafnyAdapter {
    fn extensions(&self) -> &[&str] {
        &[".dfy"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let mut parser = Self::new_parser()?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let mut warnings = Vec::new();

        if root.has_error() {
            warnings.push("tree-sitter reported syntax errors".to_string());
        }

        self.walk(root, source_bytes, None, &mut symbols, &mut imports);

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
        let mut parser = Self::new_parser()?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let target = find_node_at_range(root, range.start_byte, range.end_byte)
            .ok_or("Could not find node at given range")?;

        let mut callees = Vec::new();
        Self::collect_callees(target, source_bytes, &mut callees);
        callees.sort();
        callees.dedup();
        Ok(callees)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    const SAMPLE: &str = r#"
module M {
  import opened Std.Collections.Seq

  class C {
    var count: int
    const Limit: int := 10

    constructor() { count := 0; }

    method Bump(by: int) returns (r: int) {
      r := Triple(by);
    }

    function Get(): int { count }
  }

  trait Shape {
    function Area(): real
  }

  datatype Color = Red | Green | Blue

  type SmallNat = x: int | 0 <= x < 10

  function Triple(n: int): int { n * 3 }

  method Caller() {
    var y := Triple(2);
  }
}
"#;

    fn extract(src: &str) -> ExtractedFile {
        DafnyAdapter.extract(&pb("sample.dfy"), src).unwrap()
    }

    fn find<'a>(f: &'a ExtractedFile, name: &str) -> &'a Symbol {
        f.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbol {name:?} not found; got {:?}", names(f)))
    }

    fn names(f: &ExtractedFile) -> Vec<&str> {
        f.symbols.iter().map(|s| s.name.as_str()).collect()
    }

    fn dfy_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                dfy_files(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("dfy") {
                out.push(p);
            }
        }
    }

    #[test]
    fn smoke_fixtures() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tests/fixtures/dafny");
        let mut files = Vec::new();
        dfy_files(&dir, &mut files);
        files.sort();
        let mut total = 0usize;
        for p in &files {
            let src = std::fs::read_to_string(p).unwrap();
            let f = DafnyAdapter.extract(p, &src).unwrap();
            total += f.symbols.len();
            eprintln!(
                "{}: {} symbols, {} imports, warnings={:?}",
                p.file_name().unwrap().to_string_lossy(),
                f.symbols.len(),
                f.imports.len(),
                f.warnings
            );
        }
        eprintln!(
            "TOTAL symbols across {} fixture files: {total}",
            files.len()
        );
        assert!(
            total > 0,
            "expected to extract symbols from real Dafny fixtures"
        );
    }

    #[test]
    fn extension_is_dfy() {
        assert!(DafnyAdapter.extensions().contains(&".dfy"));
    }

    #[test]
    fn module_is_class_kind() {
        let f = extract(SAMPLE);
        assert!(matches!(find(&f, "M").kind, SymbolKind::Class));
    }

    #[test]
    fn function_and_method_kinds() {
        let f = extract(SAMPLE);
        assert!(matches!(find(&f, "Triple").kind, SymbolKind::Function));
        assert!(matches!(find(&f, "Bump").kind, SymbolKind::Method));
    }

    #[test]
    fn constructor_is_method() {
        let f = extract(SAMPLE);
        // Constructors surface under the kind name the grammar gives the
        // node; we map `constructor_decl` → Method.
        assert!(f.symbols.iter().any(
            |s| matches!(s.kind, SymbolKind::Method) && s.parent_class.as_deref() == Some("C")
        ));
    }

    #[test]
    fn trait_is_interface_and_datatype_is_class() {
        let f = extract(SAMPLE);
        assert!(matches!(find(&f, "Shape").kind, SymbolKind::Interface));
        assert!(matches!(find(&f, "Color").kind, SymbolKind::Class));
    }

    #[test]
    fn synonym_type_is_type_alias() {
        let f = extract(SAMPLE);
        assert!(matches!(find(&f, "SmallNat").kind, SymbolKind::TypeAlias));
    }

    #[test]
    fn field_and_const_are_variables() {
        let f = extract(SAMPLE);
        assert!(matches!(find(&f, "count").kind, SymbolKind::Variable));
        assert!(matches!(find(&f, "Limit").kind, SymbolKind::Variable));
    }

    #[test]
    fn nested_member_has_parent_class() {
        let f = extract(SAMPLE);
        assert_eq!(find(&f, "Bump").parent_class.as_deref(), Some("C"));
        assert_eq!(find(&f, "count").parent_class.as_deref(), Some("C"));
    }

    #[test]
    fn import_is_captured_qualified() {
        let f = extract(SAMPLE);
        assert!(
            f.imports.iter().any(
                |i| i.source.contains("Std.Collections.Seq") && i.kind == ImportKind::Qualified
            ),
            "imports: {:?}",
            f.imports
        );
    }

    #[test]
    fn find_callees_in_method_body() {
        let f = extract(SAMPLE);
        let bump = find(&f, "Bump");
        let callees = DafnyAdapter
            .find_callees_in_range(SAMPLE, &pb("sample.dfy"), bump.range)
            .unwrap();
        assert!(
            callees.contains(&"Triple".to_string()),
            "callees: {callees:?}"
        );
    }
}

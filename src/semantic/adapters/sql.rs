use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{node_text, signature_first_line};
use crate::semantic::types::{ByteRange, ExtractedFile, Symbol, SymbolKind};

/// Tree-sitter adapter for SQL (DerekStride/tree-sitter-sql grammar,
/// published as the `tree-sitter-sequel` crate). SQL has no call
/// graph, so `find_callees_in_range` returns an empty list — the
/// value of this adapter is `list_symbols` / `get_symbol_body` /
/// `find_definition` over DDL objects.
///
/// `SymbolKind` is reused (SQL has no native fit, same as Ruby maps
/// `module`→Interface): tables/views/materialized views → `Class`
/// (a named schema object); functions → `Function`;
/// indexes → `Variable`; types → `TypeAlias`. The `signature` field
/// carries the full `CREATE …` line so the kind is only a coarse
/// filter.
pub struct SqlAdapter;

impl SqlAdapter {
    /// The object name is the first `identifier` leaf in document
    /// order: for `CREATE TABLE/VIEW/FUNCTION/TYPE` the name lives in an
    /// `object_reference` that precedes any column / parameter / body
    /// list, so the first identifier encountered is the object name
    /// (`CREATE TABLE users (...)` → `users`). Keywords are `keyword_*`
    /// nodes, never `identifier`, so `IF NOT EXISTS` / `TEMPORARY` don't
    /// interfere. (Not used for `CREATE INDEX` — see `direct_identifier`.)
    fn first_identifier(&self, n: Node, s: &[u8]) -> Option<String> {
        if n.kind() == "identifier" {
            return Some(node_text(n, s).to_string());
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            if let Some(name) = self.first_identifier(child, s) {
                return Some(name);
            }
        }
        None
    }

    /// The first `identifier` that is a DIRECT child of `n`. For
    /// `CREATE INDEX` the optional index name is a direct `identifier`
    /// child (the indexed table sits one level down inside an
    /// `object_reference`), so this returns the index name when named
    /// and `None` for an anonymous `CREATE INDEX ON t(col)` — which has
    /// no name to index. `first_identifier` would instead walk into the
    /// `object_reference` and wrongly return the table name.
    fn direct_identifier(&self, n: Node, s: &[u8]) -> Option<String> {
        let mut cursor = n.walk();
        n.named_children(&mut cursor)
            .find(|c| c.kind() == "identifier")
            .map(|c| node_text(c, s).to_string())
    }

    fn emit(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>, kind: SymbolKind, name: String) {
        symbols.push(Symbol {
            kind,
            is_exported: true,
            name,
            range: ByteRange::from(n),
            signature: signature_first_line(n, s),
            parent_class: None,
        });
    }

    /// Recursively scan for top-level DDL nodes. The grammar wraps each
    /// statement in `statement`, which we recurse through transparently.
    /// `create_*` nodes CAN nest (e.g. DDL inside a PL/pgSQL function
    /// body), so once one is emitted we STOP descending — otherwise a
    /// helper object created inside a function body would leak in as a
    /// spurious top-level symbol.
    fn walk(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        let resolved = match n.kind() {
            "create_table" | "create_view" | "create_materialized_view" => {
                Some((SymbolKind::Class, self.first_identifier(n, s)))
            }
            "create_function" => Some((SymbolKind::Function, self.first_identifier(n, s))),
            "create_type" => Some((SymbolKind::TypeAlias, self.first_identifier(n, s))),
            // Index name is the direct child, and is optional.
            "create_index" => Some((SymbolKind::Variable, self.direct_identifier(n, s))),
            _ => None,
        };
        if let Some((kind, name)) = resolved {
            // A named DDL object → emit; an anonymous one (e.g.
            // `CREATE INDEX ON t(col)`) has no name, so skip. Either way
            // do not descend into a create_* node.
            if let Some(name) = name {
                self.emit(n, s, symbols, kind, name);
            }
            return;
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            self.walk(child, s, symbols);
        }
    }
}

impl LanguageAdapter for SqlAdapter {
    fn extensions(&self) -> &[&str] {
        &[".sql"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let mut symbols = Vec::new();
        let mut warnings = Vec::new();

        if root.has_error() {
            warnings.push("tree-sitter reported syntax errors".to_string());
        }

        self.walk(root, source_bytes, &mut symbols);

        let exports: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();

        Ok(ExtractedFile {
            file_path: file_path.to_path_buf(),
            symbols,
            imports: vec![],
            exports,
            warnings,
            mtime: std::time::SystemTime::now(),
            size: 0,
            head_hash: 0,
        })
    }

    fn find_callees_in_range(
        &self,
        _source: &str,
        _file_path: &Path,
        _range: ByteRange,
    ) -> Result<Vec<String>, String> {
        // SQL has no call graph — DDL objects aren't invoked. Return
        // empty so find_callees / find_callers no-op for SQL.
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    #[test]
    fn extracts_create_table_and_view() {
        let src = "CREATE TABLE users (\n  id INT PRIMARY KEY,\n  email TEXT NOT NULL\n);\n\
                   CREATE VIEW active_users AS SELECT * FROM users WHERE active;\n";
        let f = SqlAdapter.extract(&pb("schema.sql"), src).unwrap();
        let t = f.symbols.iter().find(|s| s.name == "users").unwrap();
        assert!(matches!(t.kind, SymbolKind::Class));
        let v = f.symbols.iter().find(|s| s.name == "active_users").unwrap();
        assert!(matches!(v.kind, SymbolKind::Class));
    }

    #[test]
    fn extracts_function() {
        let src =
            "CREATE FUNCTION add(a INT, b INT) RETURNS INT AS $$ SELECT a + b $$ LANGUAGE SQL;\n";
        let f = SqlAdapter.extract(&pb("fns.sql"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "add" && matches!(s.kind, SymbolKind::Function))
        );
    }

    #[test]
    fn extracts_index_and_type() {
        let src = "CREATE INDEX idx_email ON users(email);\n\
                   CREATE TYPE mood AS ENUM ('happy','sad');\n";
        let f = SqlAdapter.extract(&pb("misc.sql"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "idx_email" && matches!(s.kind, SymbolKind::Variable))
        );
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "mood" && matches!(s.kind, SymbolKind::TypeAlias))
        );
    }

    #[test]
    fn find_callees_is_empty() {
        let src = "CREATE FUNCTION f() RETURNS INT AS $$ SELECT 1 $$ LANGUAGE SQL;\n";
        let f = SqlAdapter.extract(&pb("f.sql"), src).unwrap();
        let sym = f.symbols.first().unwrap();
        let callees = SqlAdapter
            .find_callees_in_range(src, &pb("f.sql"), sym.range)
            .unwrap();
        assert!(callees.is_empty());
    }

    #[test]
    fn extracts_materialized_view() {
        let src = "CREATE MATERIALIZED VIEW mv_stats AS SELECT count(*) FROM events;\n";
        let f = SqlAdapter.extract(&pb("mv.sql"), src).unwrap();
        assert!(f.warnings.is_empty());
        let mv = f.symbols.iter().find(|s| s.name == "mv_stats").unwrap();
        assert!(matches!(mv.kind, SymbolKind::Class));
    }

    #[test]
    fn broken_sql_emits_warning() {
        let src = "CREATE TABLE users (id INT;\n";
        let f = SqlAdapter.extract(&pb("bad.sql"), src).unwrap();
        assert!(!f.warnings.is_empty());
    }

    #[test]
    fn pure_select_extracts_nothing() {
        let src = "SELECT id, name FROM users WHERE active = TRUE;\n";
        let f = SqlAdapter.extract(&pb("q.sql"), src).unwrap();
        assert!(f.symbols.is_empty());
        assert!(f.warnings.is_empty());
    }

    #[test]
    fn qualified_name_yields_schema_not_table() {
        // Known v1 limitation: the first identifier leaf in document
        // order is the schema qualifier, not the table name. Locks in
        // current behavior so a grammar fix surfaces as a failure.
        let src = "CREATE TABLE public.users (id INT);\n";
        let f = SqlAdapter.extract(&pb("schema.sql"), src).unwrap();
        assert!(f.symbols.iter().any(|s| s.name == "public"));
        assert!(!f.symbols.iter().any(|s| s.name == "users"));
    }

    #[test]
    fn anonymous_index_is_not_misattributed_to_table() {
        // Postgres allows omitting the index name. The indexed table sits
        // inside an `object_reference` one level down, so the old
        // first-identifier-leaf logic emitted a bogus `users` Variable.
        // An anonymous index has no name to index → emit nothing.
        let src = "CREATE INDEX ON users(email);\n";
        let f = SqlAdapter.extract(&pb("idx.sql"), src).unwrap();
        assert!(
            !f.symbols
                .iter()
                .any(|s| s.name == "users" && matches!(s.kind, SymbolKind::Variable)),
            "anonymous index must not be attributed to the table: {:?}",
            f.symbols
        );
        assert!(f.symbols.is_empty(), "got {:?}", f.symbols);
    }

    #[test]
    fn nested_ddl_in_function_body_does_not_leak() {
        // `create_*` nodes nest: a function body can contain DDL. Only the
        // function itself is a top-level object; the helper table inside
        // the body must NOT surface as its own symbol.
        let src = "CREATE FUNCTION f() RETURNS void AS $$ CREATE TABLE inner_t (id INT); $$ LANGUAGE SQL;\n";
        let f = SqlAdapter.extract(&pb("fn.sql"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "f" && matches!(s.kind, SymbolKind::Function))
        );
        assert!(
            !f.symbols.iter().any(|s| s.name == "inner_t"),
            "nested DDL leaked: {:?}",
            f.symbols
        );
    }
}

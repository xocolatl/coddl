//! Typed AST view over a parsed `.cdstore` document.
//!
//! Mirrors the productions in [`parser_cdstore`](crate::parser_cdstore):
//! [`CdstoreRoot`] wraps the document; [`CdstoreHeader`] is the
//! `store for <db>;` header; [`BackendDecl`] declares the backend kind
//! and its operational fields; [`RelvarBinding`] binds a catalog base
//! relvar to a physical table; [`ColumnsBlock`] holds the column
//! mappings.

use crate::ast::{self, AstNode};
use crate::ast_node;
use crate::cst::SyntaxToken;
use crate::syntax_kind::SyntaxKind;

ast_node!(pub CdstoreRoot, CDSTORE_ROOT);

impl CdstoreRoot {
    /// The `store for <database>;` header.
    pub fn header(&self) -> Option<CdstoreHeader> {
        ast::child(&self.syntax)
    }

    /// The single `backend <kind> { … };` declaration (v1: exactly
    /// one). Returns `None` if missing or malformed.
    pub fn backend(&self) -> Option<BackendDecl> {
        ast::child(&self.syntax)
    }

    /// Iterate the per-relvar bindings in source order.
    pub fn bindings(&self) -> impl Iterator<Item = RelvarBinding> + '_ {
        ast::children(&self.syntax)
    }
}

ast_node!(pub CdstoreHeader, CDSTORE_HEADER);

impl CdstoreHeader {
    /// The declared database name. Token order in the header is
    /// `store`, `for`, IDENT — the database name is the third IDENT.
    pub fn database_name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 2)
    }
}

ast_node!(pub BackendDecl, BACKEND_DECL);

impl BackendDecl {
    /// The backend kind token (e.g. `sqlite`, `postgres`). Token order:
    /// `backend` keyword, then the kind IDENT.
    pub fn kind(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }

    /// Iterate the named-field children (`file: …`, `dsn: …`, etc.).
    pub fn fields(&self) -> impl Iterator<Item = CdstoreField> + '_ {
        ast::children(&self.syntax)
    }
}

ast_node!(pub RelvarBinding, RELVAR_BINDING);

impl RelvarBinding {
    /// The catalog-side relvar name (the LHS of the `:`). Token order:
    /// `relvar` keyword, name IDENT, `:`, `table` keyword, …
    pub fn name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }

    /// The physical table name (the string literal after `table`).
    pub fn table_name(&self) -> Option<SyntaxToken> {
        ast::first_token_of_kind(&self.syntax, SyntaxKind::STRING_LIT)
    }

    /// The `columns: { … }` block, if present.
    pub fn columns_block(&self) -> Option<ColumnsBlock> {
        ast::child(&self.syntax)
    }
}

ast_node!(pub ColumnsBlock, COLUMNS_BLOCK);

impl ColumnsBlock {
    /// Iterate the `<attr>: "<col>"` field children.
    pub fn fields(&self) -> impl Iterator<Item = CdstoreField> + '_ {
        ast::children(&self.syntax)
    }
}

ast_node!(pub CdstoreField, CDSTORE_FIELD);

impl CdstoreField {
    /// The field name (LHS of `:`).
    pub fn name(&self) -> Option<SyntaxToken> {
        ast::first_token_of_kind(&self.syntax, SyntaxKind::IDENT)
    }

    /// The field value.
    pub fn value(&self) -> Option<CdstoreValue> {
        // The value is the first non-name, non-trivia element after
        // the `:`. Walk the children-with-tokens stream, skipping the
        // field name and the colon.
        let mut seen_name = false;
        let mut seen_colon = false;
        for el in self.syntax.children_with_tokens() {
            if el.kind().is_trivia() {
                continue;
            }
            if !seen_name {
                seen_name = true;
                continue;
            }
            if !seen_colon {
                seen_colon = true;
                continue;
            }
            return match el.kind() {
                SyntaxKind::STRING_LIT => el.into_token().map(CdstoreValue::String),
                SyntaxKind::IDENT => el.into_token().map(CdstoreValue::Ident),
                SyntaxKind::CALL_EXPR => el
                    .into_node()
                    .and_then(EnvCall::cast)
                    .map(CdstoreValue::Env),
                _ => None,
            };
        }
        None
    }
}

/// The right-hand side of a `.cdstore` field. Narrow on purpose —
/// `.cdstore` is declarative configuration, not a programming surface.
#[derive(Debug, Clone)]
pub enum CdstoreValue {
    /// A literal string (e.g. `"greetings.sqlite"`, `"id"`).
    String(SyntaxToken),
    /// A bare identifier (e.g. `sqlite`, `pooled`).
    Ident(SyntaxToken),
    /// An `env("NAME" [, default: "fallback"])` call — operational
    /// late-bind to an environment variable at runtime startup.
    Env(EnvCall),
}

ast_node!(pub EnvCall, CALL_EXPR);

impl EnvCall {
    /// The env-var name string literal (the first argument).
    pub fn name(&self) -> Option<SyntaxToken> {
        ast::first_token_of_kind(&self.syntax, SyntaxKind::STRING_LIT)
    }

    /// The optional default-value string literal, if the `default:` arg
    /// is present. Returns the second STRING_LIT under the call node.
    pub fn default(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::STRING_LIT, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_kind::FileKind;
    use crate::parse;
    use coddl_diagnostics::FileId;

    fn ast(src: &str) -> CdstoreRoot {
        let out = parse(src, FileId(0), FileKind::Cdstore);
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        CdstoreRoot::cast(out.tree).expect("CdstoreRoot")
    }

    #[test]
    fn header_name_resolves() {
        let root = ast("store for greetings;");
        assert_eq!(
            root.header().unwrap().database_name().unwrap().text(),
            "greetings"
        );
    }

    #[test]
    fn backend_kind_and_string_field() {
        let src = "store for d;\nbackend sqlite { file: \"x.sqlite\" };\n";
        let root = ast(src);
        let backend = root.backend().expect("backend");
        assert_eq!(backend.kind().unwrap().text(), "sqlite");
        let field = backend.fields().next().unwrap();
        assert_eq!(field.name().unwrap().text(), "file");
        match field.value().unwrap() {
            CdstoreValue::String(s) => assert_eq!(s.text(), "\"x.sqlite\""),
            other => panic!("expected string value, got {other:?}"),
        }
    }

    #[test]
    fn backend_env_value_with_default() {
        let src =
            "store for d;\nbackend sqlite { file: env(\"CODDL_DB\", default: \"x.sqlite\") };\n";
        let root = ast(src);
        let field = root.backend().unwrap().fields().next().unwrap();
        let CdstoreValue::Env(env) = field.value().unwrap() else {
            panic!("expected env() value");
        };
        assert_eq!(env.name().unwrap().text(), "\"CODDL_DB\"");
        assert_eq!(env.default().unwrap().text(), "\"x.sqlite\"");
    }

    #[test]
    fn ident_value_recognized() {
        let src = "store for d;\nbackend postgres { mode: pooled };\n";
        let root = ast(src);
        let field = root.backend().unwrap().fields().next().unwrap();
        match field.value().unwrap() {
            CdstoreValue::Ident(t) => assert_eq!(t.text(), "pooled"),
            other => panic!("expected ident value, got {other:?}"),
        }
    }

    #[test]
    fn relvar_binding_walks_to_columns() {
        let src = "store for d;\n\
                   relvar Greetings: table \"greetings\" {\n\
                       columns: { id: \"id\", message: \"message\" }\n\
                   };\n";
        let root = ast(src);
        let binding = root.bindings().next().expect("binding");
        assert_eq!(binding.name().unwrap().text(), "Greetings");
        assert_eq!(binding.table_name().unwrap().text(), "\"greetings\"");
        let cols = binding.columns_block().expect("columns");
        let fields: Vec<_> = cols.fields().collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name().unwrap().text(), "id");
    }
}

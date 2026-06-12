//! Typed AST view over a parsed `.cddb` document.
//!
//! Mirrors the productions in [`parser_cddb`](crate::parser_cddb):
//! [`CddbRoot`] wraps the document; [`DatabaseDecl`] is the required
//! header; [`CddbItem`] enumerates the post-header items (today: base
//! and virtual relvar declarations); [`BaseRelvarDecl`] and
//! [`VirtualRelvarDecl`] are the typed views over those.

use crate::ast::{self, AstNode, Heading, KeyClause};
use crate::ast_node;
use crate::cst::{SyntaxNode, SyntaxToken};
use crate::syntax_kind::SyntaxKind;

ast_node!(pub CddbRoot, CDDB_ROOT);

impl CddbRoot {
    /// The `database <Name>;` header. Missing → `None` (the parser
    /// emits PB0001 in that case).
    pub fn database(&self) -> Option<DatabaseDecl> {
        ast::child(&self.syntax)
    }

    /// Iterate the catalog items in source order. `PARSE_ERROR`
    /// recovery placeholders are skipped.
    pub fn items(&self) -> impl Iterator<Item = CddbItem> + '_ {
        self.syntax.children().filter_map(CddbItem::cast)
    }
}

ast_node!(pub DatabaseDecl, DATABASE_DECL);

impl DatabaseDecl {
    /// The declared database name.
    pub fn name(&self) -> Option<SyntaxToken> {
        // First IDENT is the `database` keyword; second is the name.
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }
}

/// A catalog item: a base or virtual relvar declaration.
#[derive(Debug, Clone)]
pub enum CddbItem {
    BaseRelvar(BaseRelvarDecl),
    VirtualRelvar(VirtualRelvarDecl),
}

impl AstNode for CddbItem {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(
            kind,
            SyntaxKind::BASE_RELVAR_DECL | SyntaxKind::VIRTUAL_RELVAR_DECL
        )
    }

    fn cast(syntax: SyntaxNode) -> Option<Self> {
        match syntax.kind() {
            SyntaxKind::BASE_RELVAR_DECL => BaseRelvarDecl::cast(syntax).map(CddbItem::BaseRelvar),
            SyntaxKind::VIRTUAL_RELVAR_DECL => {
                VirtualRelvarDecl::cast(syntax).map(CddbItem::VirtualRelvar)
            }
            _ => None,
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            CddbItem::BaseRelvar(decl) => decl.syntax(),
            CddbItem::VirtualRelvar(decl) => decl.syntax(),
        }
    }
}

ast_node!(pub BaseRelvarDecl, BASE_RELVAR_DECL);

impl BaseRelvarDecl {
    /// The declared relvar name. The keyword tokens `base` and `relvar`
    /// occupy the first two IDENT slots; the name is the third.
    pub fn name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 2)
    }

    /// The relvar's heading (`{ a: T, b: U, … }`). Reuses the shared
    /// `.cd` AST type since the production is identical.
    pub fn heading(&self) -> Option<Heading> {
        ast::child(&self.syntax)
    }

    /// All candidate-key clauses in source order. Multi-key parses;
    /// the typechecker validates the first one for v1 (per Phase 15).
    pub fn key_clauses(&self) -> impl Iterator<Item = KeyClause> + '_ {
        self.syntax.children().filter_map(KeyClause::cast)
    }
}

ast_node!(pub VirtualRelvarDecl, VIRTUAL_RELVAR_DECL);

impl VirtualRelvarDecl {
    /// The declared view name. The keyword tokens `virtual` and
    /// `relvar` occupy the first two IDENT slots; the name is the
    /// third.
    pub fn name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_kind::FileKind;
    use crate::parse;
    use coddl_diagnostics::FileId;

    fn ast(src: &str) -> CddbRoot {
        let out = parse(src, FileId(0), FileKind::Cddb);
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        CddbRoot::cast(out.tree).expect("CddbRoot")
    }

    #[test]
    fn database_decl_name_resolves() {
        let root = ast("database greetings;");
        let db = root.database().expect("DatabaseDecl");
        assert_eq!(db.name().expect("name").text(), "greetings");
    }

    #[test]
    fn base_relvar_walks_to_name_heading_key() {
        let src = "database d;\n\
                   base relvar Greetings { id: Integer, message: Text } key { id };\n";
        let root = ast(src);
        let mut items = root.items();
        let item = items.next().expect("item");
        let CddbItem::BaseRelvar(decl) = item else {
            panic!("expected base relvar item");
        };
        assert_eq!(decl.name().expect("name").text(), "Greetings");
        let heading = decl.heading().expect("heading");
        assert_eq!(heading.params().count(), 2);
        let keys: Vec<_> = decl.key_clauses().collect();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].attrs().any(|t| t.text() == "id"));
    }

    #[test]
    fn base_relvar_supports_multi_key() {
        let src = "database d;\n\
                   base relvar X { a: Integer, b: Integer } key { a } key { b };\n";
        let root = ast(src);
        let CddbItem::BaseRelvar(decl) = root.items().next().unwrap() else {
            panic!("expected base relvar");
        };
        assert_eq!(decl.key_clauses().count(), 2);
    }

    #[test]
    fn public_relvar_in_cddb_dialect_parses() {
        // `.cddb` parses public/private so the typechecker can emit
        // T0014; here we just confirm the tree shape.
        let src = "database d;\npublic relvar X { a: Integer } key { a };\n";
        let out = parse(src, FileId(0), FileKind::Cddb);
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![SyntaxKind::DATABASE_DECL, SyntaxKind::PUBLIC_RELVAR_DECL]
        );
    }

    #[test]
    fn virtual_relvar_is_recognized_as_item() {
        let src = "database d;\nvirtual relvar V = X where p;\n";
        let root = ast(src);
        let item = root.items().next().expect("item");
        assert!(matches!(item, CddbItem::VirtualRelvar(_)));
    }
}

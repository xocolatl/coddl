//! Typed AST view over a parsed `.cdmap` document.
//!
//! Mirrors the productions in [`parser_cdmap`](crate::parser_cdmap):
//! [`CdmapRoot`] wraps the document; [`CdmapHeader`] is the
//! `map <prog> to <db>;` header; [`CdmapEntry`] is one identity
//! mapping entry.

use crate::ast;
use crate::ast_node;
use crate::cst::SyntaxToken;
use crate::syntax_kind::SyntaxKind;

#[cfg(test)]
use crate::ast::AstNode;

ast_node!(pub CdmapRoot, CDMAP_ROOT);

impl CdmapRoot {
    /// The `map <program> to <database>;` header.
    pub fn header(&self) -> Option<CdmapHeader> {
        ast::child(&self.syntax)
    }

    /// Iterate the mapping entries in source order.
    pub fn entries(&self) -> impl Iterator<Item = CdmapEntry> + '_ {
        ast::children(&self.syntax)
    }
}

ast_node!(pub CdmapHeader, CDMAP_HEADER);

impl CdmapHeader {
    /// The declared program name (LHS of the `to` keyword). Tokens
    /// inside the header occur as: `map` IDENT (program), `to` IDENT
    /// (database). The program name is the first IDENT after the
    /// `map` keyword — i.e. the second IDENT overall.
    pub fn program_name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }

    /// The declared database name (RHS of `to`). Tokens: `map`,
    /// program, `to`, database. The database name is the fourth IDENT.
    pub fn database_name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 3)
    }
}

ast_node!(pub CdmapEntry, CDMAP_ENTRY);

impl CdmapEntry {
    /// The application-side relvar name (LHS of `=`).
    pub fn app_name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 0)
    }

    /// The catalog-side relvar name (RHS of `=`).
    pub fn catalog_name(&self) -> Option<SyntaxToken> {
        ast::nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_kind::FileKind;
    use crate::parse;
    use coddl_diagnostics::FileId;

    fn ast(src: &str) -> CdmapRoot {
        let out = parse(src, FileId(0), FileKind::Cdmap);
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        CdmapRoot::cast(out.tree).expect("CdmapRoot")
    }

    #[test]
    fn header_names_resolve() {
        let root = ast("map myapp to mydb;");
        let header = root.header().expect("header");
        assert_eq!(header.program_name().expect("prog").text(), "myapp");
        assert_eq!(header.database_name().expect("db").text(), "mydb");
    }

    #[test]
    fn entry_names_resolve() {
        let root = ast("map a to b;\nGreetings = CatGreetings;\n");
        let entry = root.entries().next().expect("entry");
        assert_eq!(entry.app_name().expect("lhs").text(), "Greetings");
        assert_eq!(entry.catalog_name().expect("rhs").text(), "CatGreetings");
    }

    #[test]
    fn multiple_entries_iterate_in_order() {
        let src = "map a to b;\nA = A;\nB = B;\nC = C;\n";
        let root = ast(src);
        let names: Vec<_> = root
            .entries()
            .map(|e| e.app_name().unwrap().text().to_string())
            .collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }
}

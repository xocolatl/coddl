//! Typed AST view over a parsed `.cdstore` document.
//!
//! A `.cdstore` is DML into `coddl::storage` — a bare sequence of statements
//! (see [`parser_cdstore`](crate::parser_cdstore)). [`CdstoreRoot`] wraps the
//! document; its statements are the shared [`Stmt`] grammar `.cd` operator
//! bodies use.

use crate::ast::Stmt;
use crate::ast_node;

ast_node!(pub CdstoreRoot, CDSTORE_ROOT);

impl CdstoreRoot {
    /// The document's statements, in source order. A `.cdstore` is a bare
    /// sequence of DML statements (`insert …;`, `R := …;`) over the implicit
    /// `coddl::storage` module — the same [`Stmt`] grammar `.cd` operator bodies
    /// use, just at file top level.
    pub fn stmts(&self) -> impl Iterator<Item = Stmt> + '_ {
        self.syntax.children().filter_map(Stmt::cast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{self, AstNode};
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
    fn stmts_are_dml_in_source_order() {
        let src = "insert Backends Relation { { database: \"g\", backend: \"sqlite\" }, };\n\
                   ConnDefault := ConnDefault union Relation { { database: \"g\", backend: \"sqlite\", field: \"file\", value: \"g.sqlite\" }, };\n";
        let root = ast(src);
        let stmts: Vec<_> = root.stmts().collect();
        assert_eq!(stmts.len(), 2);
        assert!(matches!(stmts[0], Stmt::Insert(_)));
        assert!(matches!(stmts[1], Stmt::Assign(_)));
    }

    #[test]
    fn insert_stmt_exposes_target_and_source() {
        let root = ast("insert Backends Relation { { database: \"g\", backend: \"sqlite\" }, };\n");
        let Some(Stmt::Insert(ins)) = root.stmts().next() else {
            panic!("expected an insert statement");
        };
        // The target is the bare relvar name reference `Backends`.
        let Some(ast::Expr::NameRef(target)) = ins.target() else {
            panic!("expected a name-ref target");
        };
        assert_eq!(target.ident().unwrap().text(), "Backends");
    }
}

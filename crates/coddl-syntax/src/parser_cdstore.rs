//! Recursive-descent parser for the `.cdstore` dialect.
//!
//! A `.cdstore` document is **DML into `coddl::storage`** — the storage
//! meta-catalog. It is a bare sequence of statements (`insert Backends
//! Relation { … };`, `ConnDefault := ConnDefault union Relation { … };`) that
//! the compiler evaluates at compile time to populate the `coddl::storage`
//! builtin relvars. `use module coddl::storage;` is **implicit** — the dialect
//! auto-activates that module — so a `.cdstore` needs no import line.
//!
//! Shape:
//!
//! ```text
//! <cdstore-root> ::= <stmt>* EOF
//! ```
//!
//! There is no `.cdstore`-specific grammar: every statement is parsed by the
//! shared [`Parser::parse_stmt`](crate::parser::Parser::parse_stmt), the same
//! production `.cd` operator bodies use. The only difference from `.cd` is the
//! container — statements sit at file top level here, rather than inside an
//! `oper` body — so this root mirrors [`Parser::parse_root`] but drives
//! `parse_stmt` instead of `parse_item`.

use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;

/// Parse a `.cdstore` document: a bare sequence of statements, terminated by
/// EOF. Losslessly mirrors [`Parser::parse_root`] — leading trivia flushes into
/// the root, each statement drives the shared `parse_stmt`, and any trailing
/// trivia is flushed before the root closes.
pub(crate) fn parse_cdstore_root(p: &mut Parser) {
    p.start_node(SyntaxKind::CDSTORE_ROOT);
    p.bump_trivia();

    while p.current() != SyntaxKind::EOF {
        p.parse_stmt();
    }

    p.bump_trivia();
    p.finish_node();
}

#[cfg(test)]
mod tests {
    use crate::file_kind::FileKind;
    use crate::parse;
    use crate::syntax_kind::SyntaxKind;
    use crate::ParseOutput;
    use coddl_diagnostics::FileId;

    fn parse_str(src: &str) -> ParseOutput {
        parse(src, FileId(0), FileKind::Cdstore)
    }

    fn child_kinds(out: &ParseOutput) -> Vec<SyntaxKind> {
        out.tree.children().map(|n| n.kind()).collect()
    }

    #[test]
    fn empty_input_only_root() {
        let out = parse_str("");
        assert_eq!(out.tree.kind(), SyntaxKind::CDSTORE_ROOT);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn insert_statement_parses() {
        let src = "insert Backends Relation {\n\
                   { database: \"greetings\", backend: \"sqlite\" },\n\
                   };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(child_kinds(&out), vec![SyntaxKind::INSERT_STMT]);
    }

    #[test]
    fn assignment_statement_parses() {
        let src = "ConnDefault := ConnDefault union Relation {\n\
                   { database: \"greetings\", backend: \"sqlite\", field: \"file\", value: \"greetings.sqlite\" },\n\
                   };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(child_kinds(&out), vec![SyntaxKind::ASSIGN_STMT]);
    }

    #[test]
    fn multiple_statements_and_comments_round_trip() {
        // The greetings.cdstore shape: two inserts and one assignment,
        // interleaved with comments. Losslessly preserved and diagnostic-free.
        let src = "// a storage file\n\
                   insert Backends Relation { { database: \"g\", backend: \"sqlite\" }, };\n\
                   \n\
                   // the default\n\
                   insert ConnEnv Relation { { database: \"g\", backend: \"sqlite\", field: \"file\", env_var: \"G_PATH\" }, };\n\
                   ConnDefault := ConnDefault union Relation { { database: \"g\", backend: \"sqlite\", field: \"file\", value: \"g.sqlite\" }, };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(
            child_kinds(&out),
            vec![
                SyntaxKind::INSERT_STMT,
                SyntaxKind::INSERT_STMT,
                SyntaxKind::ASSIGN_STMT,
            ]
        );
        assert_eq!(out.tree.text(), src, "CST must be lossless");
    }

    #[test]
    fn missing_semicolon_diagnoses() {
        // A statement missing its terminating `;` is the shared parser's P0013,
        // not a `.cdstore`-specific code.
        let src = "insert Backends Relation { { database: \"g\", backend: \"sqlite\" }, }\n";
        let out = parse_str(src);
        assert!(out.diagnostics.iter().any(|d| d.code == "P0013"));
    }
}

//! Recursive-descent parser for the `.cddb` dialect — database
//! catalog declarations.
//!
//! Shape:
//!
//! ```text
//! <cddb-root>            ::= <database-decl> <cddb-item>* EOF
//! <database-decl>        ::= 'database' IDENT ';'
//! <cddb-item>            ::= <base-relvar-decl>
//!                          | <virtual-relvar-decl>
//! <base-relvar-decl>     ::= 'base' 'relvar' IDENT <heading> <key-clause>? ';'
//! <virtual-relvar-decl>  ::= 'virtual' 'relvar' IDENT '=' <unknown-body> ';'
//! ```
//!
//! `<heading>` and `<key-clause>` are shared with `.cd` via the
//! corresponding `Parser` methods. `<unknown-body>` is parsed by
//! consuming tokens to the next top-level `;` — virtual relvar RHS
//! semantics land in Phase 16.

use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;

/// Parse a `.cddb` document: a `database <Name>;` header followed by
/// zero or more catalog items (base / virtual relvar declarations).
pub(crate) fn parse_cddb_root(p: &mut Parser) {
    p.start_node(SyntaxKind::CDDB_ROOT);
    p.bump_trivia();

    if p.at_keyword("database") {
        parse_database_decl(p);
    } else if p.current() != SyntaxKind::EOF {
        p.error("PB0001", "expected `database <Name>;` header");
    }

    while p.current() != SyntaxKind::EOF {
        parse_cddb_item(p);
    }
    p.bump_trivia();
    p.finish_node();
}

/// `database <Name>;` — required first item of every `.cddb`.
fn parse_database_decl(p: &mut Parser) {
    debug_assert!(p.at_keyword("database"));
    p.bump_trivia();
    p.start_node(SyntaxKind::DATABASE_DECL);
    p.bump(); // `database`

    if !p.eat(SyntaxKind::IDENT) {
        p.error("PB0002", "expected database name");
    }
    if !p.eat(SyntaxKind::SEMICOLON) {
        p.error("PB0003", "expected `;` after `database <Name>`");
    }

    p.finish_node();
}

/// Dispatch a single `.cddb` catalog item by its leading keyword.
/// Unknown items wrap in [`SyntaxKind::PARSE_ERROR`] and recover at the
/// next top-level `;`.
fn parse_cddb_item(p: &mut Parser) {
    if p.at_keyword("base") {
        parse_base_relvar_decl(p);
    } else if p.at_keyword("virtual") {
        parse_virtual_relvar_decl(p);
    } else {
        p.bump_trivia();
        if p.current() == SyntaxKind::EOF {
            return;
        }
        p.start_node(SyntaxKind::PARSE_ERROR);
        p.error("PB0004", "expected `base relvar` or `virtual relvar`");
        p.skip_to_top_level_anchor();
        p.finish_node();
    }
}

/// `base relvar <Name> <heading> [<key-clause>];` — persistent catalog
/// relvar.
fn parse_base_relvar_decl(p: &mut Parser) {
    debug_assert!(p.at_keyword("base"));
    p.bump_trivia();
    p.start_node(SyntaxKind::BASE_RELVAR_DECL);
    p.bump(); // `base`

    if !p.at_keyword("relvar") {
        p.error("PB0005", "expected `relvar` after `base`");
    } else {
        p.bump(); // `relvar`
    }

    if !p.eat(SyntaxKind::IDENT) {
        p.error("PB0006", "expected relvar name");
    }

    if p.at(SyntaxKind::L_BRACE) {
        p.parse_heading();
    } else {
        p.error("PB0007", "expected `{` to start relvar heading");
    }

    if p.at_keyword("key") {
        p.parse_key_clause();
    }

    if !p.eat(SyntaxKind::SEMICOLON) {
        p.error("PB0008", "expected `;` after `base relvar` declaration");
    }

    p.finish_node();
}

/// `virtual relvar <Name> = <RHS>;` — catalog view. v1 parses the
/// keyword + name + `=` and treats the RHS as an unknown body
/// recovered at the next top-level `;`. The actual relational
/// expression grammar lands with Phase 16.
fn parse_virtual_relvar_decl(p: &mut Parser) {
    debug_assert!(p.at_keyword("virtual"));
    p.bump_trivia();
    p.start_node(SyntaxKind::VIRTUAL_RELVAR_DECL);
    p.bump(); // `virtual`

    if !p.at_keyword("relvar") {
        p.error("PB0009", "expected `relvar` after `virtual`");
    } else {
        p.bump();
    }

    if !p.eat(SyntaxKind::IDENT) {
        p.error("PB0010", "expected relvar name");
    }

    if !p.eat(SyntaxKind::EQ) {
        p.error("PB0011", "expected `=` after virtual relvar name");
    }

    // RHS body — recover at the next top-level `;`. The `;` itself is
    // consumed by `skip_to_top_level_anchor`.
    p.skip_to_top_level_anchor();

    p.finish_node();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_kind::FileKind;
    use crate::parse;
    use crate::ParseOutput;
    use coddl_diagnostics::FileId;

    fn parse_str(src: &str) -> ParseOutput {
        parse(src, FileId(0), FileKind::Cddb)
    }

    #[test]
    fn empty_input_only_root() {
        let out = parse_str("");
        assert_eq!(out.tree.kind(), SyntaxKind::CDDB_ROOT);
        // Header is required; missing → PB0001.
        assert_eq!(out.diagnostics.len(), 0);
    }

    #[test]
    fn header_only() {
        let out = parse_str("database greetings;");
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(out.tree.text(), "database greetings;");
        let header = out.tree.first_child().unwrap();
        assert_eq!(header.kind(), SyntaxKind::DATABASE_DECL);
    }

    #[test]
    fn missing_header_diagnoses_pb0001() {
        let out = parse_str("base relvar X {} key { x };");
        assert!(out.diagnostics.iter().any(|d| d.code == "PB0001"));
    }

    #[test]
    fn base_relvar_minimum() {
        let out = parse_str("database d; base relvar X {} ;");
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![SyntaxKind::DATABASE_DECL, SyntaxKind::BASE_RELVAR_DECL]
        );
    }

    #[test]
    fn base_relvar_with_heading_and_key() {
        let src = "database greetings;\n\
                   \n\
                   base relvar Greetings {\n\
                       id: Integer,\n\
                       message: Text,\n\
                   }\n\
                   key { id };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(out.tree.text(), src);
        let relvar = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::BASE_RELVAR_DECL)
            .unwrap();
        let kinds: Vec<_> = relvar.children().map(|n| n.kind()).collect();
        assert!(kinds.contains(&SyntaxKind::HEADING));
        assert!(kinds.contains(&SyntaxKind::KEY_CLAUSE));
    }

    #[test]
    fn multiple_base_relvars() {
        let src = "database d;\n\
                   base relvar A { x: Integer } key { x };\n\
                   base relvar B { y: Text } key { y };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let relvars: Vec<_> = out
            .tree
            .children()
            .filter(|n| n.kind() == SyntaxKind::BASE_RELVAR_DECL)
            .collect();
        assert_eq!(relvars.len(), 2);
    }

    #[test]
    fn missing_base_name_diagnoses_pb0006() {
        let out = parse_str("database d; base relvar {};");
        assert!(out.diagnostics.iter().any(|d| d.code == "PB0006"));
    }

    #[test]
    fn missing_base_heading_diagnoses_pb0007() {
        let out = parse_str("database d; base relvar X;");
        assert!(out.diagnostics.iter().any(|d| d.code == "PB0007"));
    }

    #[test]
    fn virtual_relvar_parses_as_unknown_body() {
        // Even though the RHS isn't parsed structurally yet, the node
        // exists and recovery reaches the next item cleanly.
        let src = "database d;\n\
                   virtual relvar V = X where p;\n\
                   base relvar X { x: Integer };\n";
        let out = parse_str(src);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::DATABASE_DECL,
                SyntaxKind::VIRTUAL_RELVAR_DECL,
                SyntaxKind::BASE_RELVAR_DECL,
            ]
        );
    }

    #[test]
    fn unknown_item_recovers_and_keeps_parsing() {
        let src = "database d;\n\
                   garbage { stuff };\n\
                   base relvar X { x: Integer };\n";
        let out = parse_str(src);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::DATABASE_DECL,
                SyntaxKind::PARSE_ERROR,
                SyntaxKind::BASE_RELVAR_DECL,
            ]
        );
        assert!(out.diagnostics.iter().any(|d| d.code == "PB0004"));
    }

    #[test]
    fn round_trips_source_bytes() {
        let src = "// header comment\n\
                   database greetings;\n\
                   \n\
                   base relvar Greetings {\n\
                       id: Integer,\n\
                       message: Text,\n\
                   }\n\
                   key { id };\n";
        let out = parse_str(src);
        assert_eq!(out.tree.text(), src);
    }
}

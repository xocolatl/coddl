//! Recursive-descent parser for the `.cdmap` dialect — external →
//! conceptual adapter declarations.
//!
//! Shape:
//!
//! ```text
//! <cdmap-root>   ::= <cdmap-header> <cdmap-entry>* EOF
//! <cdmap-header> ::= 'map' IDENT 'to' IDENT ';'
//! <cdmap-entry>  ::= IDENT '=' IDENT ';'
//! ```
//!
//! v1 supports only identity mappings (`AppName = CatalogName;`).
//! `project { … }` and `rename { … }` chains are reserved for Phase 16;
//! their SyntaxKind variants are allocated but no production parses
//! into them today.

use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;

/// Parse a `.cdmap` document: a `map <program> to <database>;` header
/// followed by zero or more identity entries.
pub(crate) fn parse_cdmap_root(p: &mut Parser) {
    p.start_node(SyntaxKind::CDMAP_ROOT);
    p.bump_trivia();

    if p.at_keyword("map") {
        parse_cdmap_header(p);
    } else if p.current() != SyntaxKind::EOF {
        p.error("PM0001", "expected `map <program> to <database>;` header");
    }

    while p.current() != SyntaxKind::EOF {
        parse_cdmap_entry(p);
    }
    p.bump_trivia();
    p.finish_node();
}

/// `map <program> to <database>;` — required first item.
fn parse_cdmap_header(p: &mut Parser) {
    debug_assert!(p.at_keyword("map"));
    p.bump_trivia();
    p.start_node(SyntaxKind::CDMAP_HEADER);
    p.bump(); // `map`

    if !p.eat(SyntaxKind::IDENT) {
        p.error("PM0002", "expected program name");
    }
    if !p.at_keyword("to") {
        p.error("PM0003", "expected `to` between program and database name");
    } else {
        p.bump(); // `to`
    }
    if !p.eat(SyntaxKind::IDENT) {
        p.error("PM0004", "expected database name");
    }
    if !p.eat(SyntaxKind::SEMICOLON) {
        p.error("PM0005", "expected `;` after `map` header");
    }

    p.finish_node();
}

/// `<AppName> = <CatalogName>;` — identity mapping. The LHS is the
/// app's `public relvar` name; the RHS is the catalog (`base` or
/// `virtual`) relvar name. Project / rename chains land in Phase 16.
fn parse_cdmap_entry(p: &mut Parser) {
    p.bump_trivia();
    if p.current() == SyntaxKind::EOF {
        return;
    }
    if !p.at(SyntaxKind::IDENT) {
        p.start_node(SyntaxKind::PARSE_ERROR);
        p.error("PM0006", "expected map entry (`<name> = <catalog-name>;`)");
        p.skip_to_top_level_anchor();
        p.finish_node();
        return;
    }

    p.start_node(SyntaxKind::CDMAP_ENTRY);
    p.bump(); // app name (IDENT)

    if !p.eat(SyntaxKind::EQ) {
        p.error("PM0007", "expected `=` after map LHS name");
    }
    if !p.eat(SyntaxKind::IDENT) {
        p.error("PM0008", "expected catalog name on map RHS");
    }
    if !p.eat(SyntaxKind::SEMICOLON) {
        p.error("PM0009", "expected `;` after map entry");
    }

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
        parse(src, FileId(0), FileKind::Cdmap)
    }

    #[test]
    fn empty_input_only_root() {
        let out = parse_str("");
        assert_eq!(out.tree.kind(), SyntaxKind::CDMAP_ROOT);
        assert_eq!(out.diagnostics.len(), 0);
    }

    #[test]
    fn header_only() {
        let out = parse_str("map myapp to mydb;");
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(out.tree.text(), "map myapp to mydb;");
        let header = out.tree.first_child().unwrap();
        assert_eq!(header.kind(), SyntaxKind::CDMAP_HEADER);
    }

    #[test]
    fn missing_header_diagnoses_pm0001() {
        let out = parse_str("Greetings = Greetings;");
        assert!(out.diagnostics.iter().any(|d| d.code == "PM0001"));
    }

    #[test]
    fn header_missing_to_diagnoses_pm0003() {
        let out = parse_str("map myapp mydb;");
        assert!(out.diagnostics.iter().any(|d| d.code == "PM0003"));
    }

    #[test]
    fn single_identity_entry() {
        let out = parse_str("map myapp to mydb;\nGreetings = Greetings;\n");
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![SyntaxKind::CDMAP_HEADER, SyntaxKind::CDMAP_ENTRY]
        );
    }

    #[test]
    fn multiple_identity_entries() {
        let src = "map myapp to mydb;\n\
                   A = A;\n\
                   B = B;\n\
                   C = C;\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let entries: Vec<_> = out
            .tree
            .children()
            .filter(|n| n.kind() == SyntaxKind::CDMAP_ENTRY)
            .collect();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn malformed_entry_recovers() {
        let src = "map a to b;\n\
                   garbage stuff;\n\
                   Greetings = Greetings;\n";
        let out = parse_str(src);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        // First entry tried to parse with name `garbage`, then hit
        // `stuff` where it expected `=`. The diagnostic is PM0007.
        assert!(kinds.contains(&SyntaxKind::CDMAP_ENTRY));
        assert!(out.diagnostics.iter().any(|d| d.code == "PM0007"));
    }

    #[test]
    fn entry_missing_semicolon_diagnoses_pm0009() {
        let out = parse_str("map a to b;\nGreetings = Greetings");
        assert!(out.diagnostics.iter().any(|d| d.code == "PM0009"));
    }

    #[test]
    fn round_trips_source_bytes() {
        let src = "// adapter\n\
                   map hello_world_db to greetings;\n\
                   \n\
                   Greetings = Greetings;\n";
        let out = parse_str(src);
        assert_eq!(out.tree.text(), src);
    }
}

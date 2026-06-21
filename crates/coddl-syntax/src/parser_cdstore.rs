//! Recursive-descent parser for the `.cdstore` dialect — conceptual →
//! physical binding declarations.
//!
//! Shape:
//!
//! ```text
//! <cdstore-root>    ::= <cdstore-header> <cdstore-item>* EOF
//! <cdstore-header>  ::= 'store' 'for' IDENT ';'
//! <cdstore-item>    ::= <backend-decl> | <relvar-binding>
//!
//! <backend-decl>    ::= 'backend' IDENT '{' (<cdstore-field> (',' <cdstore-field>)* ','?)? '}' ';'
//! <relvar-binding>  ::= 'relvar' IDENT ':' 'table' STRING_LIT '{' <columns-block> ','? '}' ';'
//! <columns-block>   ::= 'columns' ':' '{' (<cdstore-field> (',' <cdstore-field>)* ','?)? '}'
//! <cdstore-field>   ::= IDENT [ ':' <cdstore-value> ]   -- value optional only in <columns-block>
//! <cdstore-value>   ::= STRING_LIT | IDENT | <env-call>
//! <env-call>        ::= 'env' '(' STRING_LIT (',' 'default' ':' STRING_LIT)? ')'
//! ```
//!
//! The value grammar is intentionally narrow — `.cdstore` is
//! declarative configuration, not a programming surface. Tuple
//! literals, expressions, and computed values are not supported.

use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;

/// Parse a `.cdstore` document: a `store for <db>;` header followed by
/// zero or more items (one `backend` declaration and zero-or-more
/// `relvar` bindings).
pub(crate) fn parse_cdstore_root(p: &mut Parser) {
    p.start_node(SyntaxKind::CDSTORE_ROOT);
    p.bump_trivia();

    if p.at_keyword("store") {
        parse_cdstore_header(p);
    } else if p.current() != SyntaxKind::EOF {
        p.error("PS0001", "expected `store for <database>;` header");
    }

    while p.current() != SyntaxKind::EOF {
        parse_cdstore_item(p);
    }
    p.bump_trivia();
    p.finish_node();
}

/// `store for <database>;` — required first item.
fn parse_cdstore_header(p: &mut Parser) {
    debug_assert!(p.at_keyword("store"));
    p.bump_trivia();
    p.start_node(SyntaxKind::CDSTORE_HEADER);
    p.bump(); // `store`

    if !p.at_keyword("for") {
        p.error("PS0002", "expected `for` after `store`");
    } else {
        p.bump();
    }
    if !p.eat(SyntaxKind::IDENT) {
        p.error("PS0003", "expected database name");
    }
    if !p.eat(SyntaxKind::SEMICOLON) {
        p.error("PS0004", "expected `;` after `store for <database>`");
    }

    p.finish_node();
}

/// Dispatch a single `.cdstore` item by its leading keyword.
fn parse_cdstore_item(p: &mut Parser) {
    if p.at_keyword("backend") {
        parse_backend_decl(p);
    } else if p.at_keyword("relvar") {
        parse_relvar_binding(p);
    } else {
        p.bump_trivia();
        if p.current() == SyntaxKind::EOF {
            return;
        }
        p.start_node(SyntaxKind::PARSE_ERROR);
        p.error("PS0005", "expected `backend` or `relvar` declaration");
        p.skip_to_top_level_anchor();
        p.finish_node();
    }
}

/// `backend <kind> { <field>, … };`
fn parse_backend_decl(p: &mut Parser) {
    debug_assert!(p.at_keyword("backend"));
    p.bump_trivia();
    p.start_node(SyntaxKind::BACKEND_DECL);
    p.bump(); // `backend`

    if !p.eat(SyntaxKind::IDENT) {
        p.error("PS0006", "expected backend kind name");
    }

    if !p.eat(SyntaxKind::L_BRACE) {
        p.error("PS0007", "expected `{` to start backend body");
        if !p.eat(SyntaxKind::SEMICOLON) {
            p.error("PS0008", "expected `;` after backend declaration");
        }
        p.finish_node();
        return;
    }

    parse_field_list(p, false); // backend fields carry values — no shorthand

    if !p.eat(SyntaxKind::R_BRACE) {
        p.error("PS0009", "expected `}` to close backend body");
    }
    if !p.eat(SyntaxKind::SEMICOLON) {
        p.error("PS0008", "expected `;` after backend declaration");
    }

    p.finish_node();
}

/// `relvar <Name>: table "<sql>" { <columns-block> };`
fn parse_relvar_binding(p: &mut Parser) {
    debug_assert!(p.at_keyword("relvar"));
    p.bump_trivia();
    p.start_node(SyntaxKind::RELVAR_BINDING);
    p.bump(); // `relvar`

    if !p.eat(SyntaxKind::IDENT) {
        p.error("PS0010", "expected relvar name");
    }
    if !p.eat(SyntaxKind::COLON) {
        p.error("PS0011", "expected `:` after relvar name");
    }
    if !p.at_keyword("table") {
        p.error("PS0012", "expected `table` after `:`");
    } else {
        p.bump();
    }
    if !p.eat(SyntaxKind::STRING_LIT) {
        p.error("PS0013", "expected table-name string literal");
    }

    if !p.eat(SyntaxKind::L_BRACE) {
        p.error("PS0014", "expected `{` to start relvar binding body");
        if !p.eat(SyntaxKind::SEMICOLON) {
            p.error("PS0015", "expected `;` after relvar binding");
        }
        p.finish_node();
        return;
    }

    // Body of the relvar binding. v1 expects exactly one `columns: { … }`
    // block (with optional trailing comma); the loop tolerates extra
    // unknown fields for forward-compatibility (Phase 16+ may add more).
    parse_relvar_binding_body(p);

    if !p.eat(SyntaxKind::R_BRACE) {
        p.error("PS0016", "expected `}` to close relvar binding body");
    }
    if !p.eat(SyntaxKind::SEMICOLON) {
        p.error("PS0015", "expected `;` after relvar binding");
    }

    p.finish_node();
}

/// Parse the body of a relvar binding. Today only `columns: { … }` is
/// recognized; other named fields are tolerated as `CDSTORE_FIELD` with
/// the narrow value grammar so the relvar binding still closes cleanly.
fn parse_relvar_binding_body(p: &mut Parser) {
    loop {
        p.bump_trivia();
        if p.at(SyntaxKind::R_BRACE) || p.current() == SyntaxKind::EOF {
            return;
        }

        if p.at_keyword("columns") {
            parse_columns_block(p);
        } else if p.at(SyntaxKind::IDENT) {
            parse_cdstore_field(p, false); // operational field — value required
        } else {
            p.error("PS0017", "expected relvar binding field");
            return;
        }

        if !p.eat(SyntaxKind::COMMA) {
            return;
        }
    }
}

/// `columns: { <field>, … }`
fn parse_columns_block(p: &mut Parser) {
    debug_assert!(p.at_keyword("columns"));
    p.bump_trivia();
    p.start_node(SyntaxKind::COLUMNS_BLOCK);
    p.bump(); // `columns`

    if !p.eat(SyntaxKind::COLON) {
        p.error("PS0018", "expected `:` after `columns`");
    }
    if !p.eat(SyntaxKind::L_BRACE) {
        p.error("PS0019", "expected `{` to start columns block");
        p.finish_node();
        return;
    }
    parse_field_list(p, true); // columns allow the `<name>` ≡ `<name>: "<name>"` shorthand
    if !p.eat(SyntaxKind::R_BRACE) {
        p.error("PS0020", "expected `}` to close columns block");
    }

    p.finish_node();
}

/// Parse a comma-separated list of fields. Empty and trailing-comma forms are
/// accepted. `allow_shorthand` is forwarded to each field: `true` in a
/// `columns: { … }` block (where `<name>` is sugar for `<name>: "<name>"`),
/// `false` in a `backend { … }` body (where a lone name is the PS0022 error).
fn parse_field_list(p: &mut Parser, allow_shorthand: bool) {
    p.bump_trivia();
    if p.at(SyntaxKind::R_BRACE) || p.current() == SyntaxKind::EOF {
        return;
    }

    loop {
        parse_cdstore_field(p, allow_shorthand);
        if !p.eat(SyntaxKind::COMMA) {
            break;
        }
        // Trailing comma ok.
        if p.at(SyntaxKind::R_BRACE) {
            break;
        }
    }
}

/// `<name>: <value>`, or — when `allow_shorthand` (in a `columns: { … }`
/// block) — the lone-name shorthand `<name>` (no `:` value), which means
/// `<name>: "<name>"`: the column name equals the attribute name. A field with
/// no `:` and no shorthand is the missing-colon error PS0022. Shorthand is
/// disabled in a `backend { … }` body, where every operational field carries a
/// meaningful value.
fn parse_cdstore_field(p: &mut Parser, allow_shorthand: bool) {
    p.bump_trivia();
    p.start_node(SyntaxKind::CDSTORE_FIELD);

    if !p.eat(SyntaxKind::IDENT) {
        p.error("PS0021", "expected field name");
    }
    if p.at(SyntaxKind::COLON) {
        p.bump(); // `:`
        parse_cdstore_value(p);
    } else if !allow_shorthand {
        p.error("PS0022", "expected `:` after field name");
    }
    // else: columns shorthand `<name>` — no value node; the consumer fills in
    // the column name from the attribute name.

    p.finish_node();
}

/// `STRING_LIT | IDENT | env(...)`. An IDENT followed by `(` is treated
/// as a function-style call; only `env(...)` is recognized for v1.
fn parse_cdstore_value(p: &mut Parser) {
    p.bump_trivia();
    match p.current() {
        SyntaxKind::STRING_LIT => {
            p.bump();
        }
        SyntaxKind::IDENT => {
            // Lookahead: env(...) is the only call form recognized.
            if p.at_keyword("env") {
                parse_env_call(p);
            } else {
                p.bump();
            }
        }
        _ => {
            p.error(
                "PS0023",
                "expected string literal, identifier, or `env(...)`",
            );
        }
    }
}

/// `env("VAR" [, default: "fallback"])` — late-bind an operational
/// field from the environment at runtime startup. Parsed as a
/// `CALL_EXPR` reusing the existing call-shape SyntaxKind so the AST
/// view can extract the arg list uniformly.
fn parse_env_call(p: &mut Parser) {
    debug_assert!(p.at_keyword("env"));
    let cp = p.checkpoint();
    p.bump(); // `env`

    if !p.eat(SyntaxKind::L_PAREN) {
        p.error("PS0024", "expected `(` after `env`");
        p.start_node_at(cp, SyntaxKind::CALL_EXPR);
        p.finish_node();
        return;
    }

    p.start_node_at(cp, SyntaxKind::CALL_EXPR);

    if !p.eat(SyntaxKind::STRING_LIT) {
        p.error("PS0025", "expected env-var name string literal");
    }

    if p.eat(SyntaxKind::COMMA) {
        // Optional `default: "fallback"` keyword arg.
        if !p.at_keyword("default") {
            p.error("PS0026", "expected `default` after `,` in `env(...)`");
        } else {
            p.bump();
        }
        if !p.eat(SyntaxKind::COLON) {
            p.error("PS0027", "expected `:` after `default`");
        }
        if !p.eat(SyntaxKind::STRING_LIT) {
            p.error("PS0028", "expected default-value string literal");
        }
    }

    if !p.eat(SyntaxKind::R_PAREN) {
        p.error("PS0029", "expected `)` to close `env(...)`");
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
        parse(src, FileId(0), FileKind::Cdstore)
    }

    #[test]
    fn empty_input_only_root() {
        let out = parse_str("");
        assert_eq!(out.tree.kind(), SyntaxKind::CDSTORE_ROOT);
        assert_eq!(out.diagnostics.len(), 0);
    }

    #[test]
    fn header_only() {
        let out = parse_str("store for mydb;");
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let header = out.tree.first_child().unwrap();
        assert_eq!(header.kind(), SyntaxKind::CDSTORE_HEADER);
    }

    #[test]
    fn header_missing_for_diagnoses_ps0002() {
        let out = parse_str("store mydb;");
        assert!(out.diagnostics.iter().any(|d| d.code == "PS0002"));
    }

    #[test]
    fn backend_with_string_value() {
        let src = "store for mydb;\n\
                   backend sqlite { file: \"db.sqlite\" };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let backend = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::BACKEND_DECL)
            .unwrap();
        let fields: Vec<_> = backend
            .children()
            .filter(|n| n.kind() == SyntaxKind::CDSTORE_FIELD)
            .collect();
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn backend_with_ident_value() {
        let src = "store for d;\nbackend postgres { mode: pooled };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn backend_with_env_value() {
        let src = "store for d;\nbackend sqlite { file: env(\"CODDL_DB\") };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        // The env(...) call is recorded as a CALL_EXPR inside the field.
        let backend = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::BACKEND_DECL)
            .unwrap();
        let field = backend
            .children()
            .find(|n| n.kind() == SyntaxKind::CDSTORE_FIELD)
            .unwrap();
        assert!(field.children().any(|n| n.kind() == SyntaxKind::CALL_EXPR));
    }

    #[test]
    fn backend_with_env_value_and_default() {
        let src = "store for d;\n\
                   backend sqlite { file: env(\"CODDL_DB\", default: \"x.sqlite\") };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn relvar_binding_with_columns_block() {
        let src = "store for d;\n\
                   relvar Greetings: table \"greetings\" {\n\
                       columns: {\n\
                           id: \"id\",\n\
                           message: \"message\",\n\
                       },\n\
                   };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let binding = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::RELVAR_BINDING)
            .unwrap();
        let columns = binding
            .children()
            .find(|n| n.kind() == SyntaxKind::COLUMNS_BLOCK)
            .expect("COLUMNS_BLOCK present");
        let fields: Vec<_> = columns
            .children()
            .filter(|n| n.kind() == SyntaxKind::CDSTORE_FIELD)
            .collect();
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn columns_shorthand_parses() {
        // `columns: { id, message }` — the bare-name shorthand (≡ `id: "id"`).
        let src = "store for d;\n\
                   relvar Greetings: table \"greetings\" {\n\
                       columns: { id, message }\n\
                   };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        let columns = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::COLUMNS_BLOCK)
            .unwrap();
        let fields: Vec<_> = columns
            .children()
            .filter(|n| n.kind() == SyntaxKind::CDSTORE_FIELD)
            .collect();
        assert_eq!(fields.len(), 2);
        // A shorthand field has no COLON token.
        assert!(!fields[0]
            .children_with_tokens()
            .any(|el| el.kind() == SyntaxKind::COLON));
    }

    #[test]
    fn columns_mix_shorthand_and_explicit() {
        // The two forms coexist in one block: `id` (shorthand) and
        // `body: "message"` (explicit, renamed column).
        let src = "store for d;\n\
                   relvar Greetings: table \"greetings\" {\n\
                       columns: { id, body: \"message\" }\n\
                   };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn backend_lone_name_still_diagnoses_ps0022() {
        // Shorthand is columns-only — a value-less backend field is still the
        // missing-colon error.
        let out = parse_str("store for d;\nbackend sqlite { file };\n");
        assert!(out.diagnostics.iter().any(|d| d.code == "PS0022"));
    }

    #[test]
    fn complete_example_round_trips() {
        let src = "store for greetings;\n\
                   \n\
                   backend sqlite {\n\
                       file: \"greetings.sqlite\",\n\
                   };\n\
                   \n\
                   relvar Greetings: table \"greetings\" {\n\
                       columns: {\n\
                           id:      \"id\",\n\
                           message: \"message\",\n\
                       },\n\
                   };\n";
        let out = parse_str(src);
        assert_eq!(
            out.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(out.tree.text(), src);
    }

    #[test]
    fn unknown_item_recovers() {
        let src = "store for d;\ngarbage stuff;\nbackend sqlite { file: \"x\" };\n";
        let out = parse_str(src);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::CDSTORE_HEADER,
                SyntaxKind::PARSE_ERROR,
                SyntaxKind::BACKEND_DECL,
            ]
        );
        assert!(out.diagnostics.iter().any(|d| d.code == "PS0005"));
    }

    #[test]
    fn relvar_binding_missing_table_keyword_diagnoses_ps0012() {
        let out = parse_str("store for d;\nrelvar X: \"x\" { columns: {} };\n");
        assert!(out.diagnostics.iter().any(|d| d.code == "PS0012"));
    }
}

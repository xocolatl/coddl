//! Recursive-descent parser for Coddl source.
//!
//! The parser is driven by a small [`Parser`] state machine over the
//! lexer's flat token stream. It produces a lossless concrete syntax
//! tree via [`CstBuilder`]: every byte of source ends up somewhere in
//! the tree, including comments and whitespace.
//!
//! ## Trivia
//!
//! Whitespace and comments are emitted into the tree the moment the
//! cursor passes over them. Concretely, every `bump()` first flushes
//! any pending trivia at the cursor and then emits the next non-trivia
//! token. Decision methods like `current()`, `at()`, and `at_keyword()`
//! peek past trivia without consuming it, so productions can branch
//! based on the next meaningful token while trivia attaches to whichever
//! parent node owns the next non-trivia token.
//!
//! ## Error recovery
//!
//! When the parser sees a top-level token sequence it can't make sense
//! of, it opens a [`SyntaxKind::PARSE_ERROR`] node, records a
//! diagnostic, and consumes tokens until it finds a top-level recovery
//! anchor (a `;` at bracket-depth zero, or end of input). The rest of
//! the file still parses; downstream passes get a well-formed tree
//! with the bad region clearly marked.

use coddl_diagnostics::{Diagnostic, FileId, Span};

use crate::cst::CstBuilder;
use crate::lexer::lex;
use crate::syntax_kind::SyntaxKind;
use crate::token::{Token, TokenKind};
use crate::ParseOutput;

/// Tokenize and parse a source buffer.
pub fn parse(source: &str, file: FileId) -> ParseOutput {
    let lex_out = lex(source, file);
    let mut p = Parser {
        source,
        file,
        tokens: lex_out.tokens,
        pos: 0,
        builder: CstBuilder::new(source),
        diagnostics: lex_out.diagnostics,
    };
    p.parse_root();
    p.finish()
}

struct Parser<'a> {
    source: &'a str,
    file: FileId,
    tokens: Vec<Token>,
    /// Raw index into `tokens`. Trivia is at `pos` until the next
    /// non-trivia is reached; `bump_trivia` flushes those into the
    /// tree before any meaningful token is emitted.
    pos: usize,
    builder: CstBuilder<'a>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    // ── Cursor primitives ────────────────────────────────────────────

    /// Peek the kind of the next non-trivia token, or [`SyntaxKind::EOF`].
    fn current(&self) -> SyntaxKind {
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind.is_trivia() {
            i += 1;
        }
        if i < self.tokens.len() {
            SyntaxKind::from(self.tokens[i].kind)
        } else {
            SyntaxKind::EOF
        }
    }

    /// Source span of the next non-trivia token (or a zero-length span
    /// at end of input).
    fn current_span(&self) -> Span {
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind.is_trivia() {
            i += 1;
        }
        if i < self.tokens.len() {
            self.tokens[i].span
        } else {
            let end = self.source.len() as u32;
            Span::new(self.file, end, end)
        }
    }

    /// True iff the next non-trivia token has the given kind.
    fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    /// True iff the next non-trivia token is an identifier whose lexeme
    /// is `lexeme`. Used for contextual keyword recognition (Coddl has
    /// no reserved words; every keyword is contextual).
    fn at_keyword(&self, lexeme: &str) -> bool {
        if !self.at(SyntaxKind::IDENT) {
            return false;
        }
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind.is_trivia() {
            i += 1;
        }
        let span = self.tokens[i].span;
        &self.source[span.start as usize..span.end as usize] == lexeme
    }

    /// Emit every trivia token at the cursor into the current node.
    fn bump_trivia(&mut self) {
        while self.pos < self.tokens.len() && self.tokens[self.pos].kind.is_trivia() {
            let tok = self.tokens[self.pos];
            let range = tok.span.start as usize..tok.span.end as usize;
            self.builder.token(SyntaxKind::from(tok.kind), range);
            self.pos += 1;
        }
    }

    /// Emit any pending trivia and then the next non-trivia token. The
    /// synthetic EOF token is recognized and not emitted.
    fn bump(&mut self) {
        self.bump_trivia();
        if self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos];
            if tok.kind != TokenKind::Eof {
                let range = tok.span.start as usize..tok.span.end as usize;
                self.builder.token(SyntaxKind::from(tok.kind), range);
            }
            self.pos += 1;
        }
    }

    /// Bump if the cursor is at `kind`. Returns true on consumption.
    fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn start_node(&mut self, kind: SyntaxKind) {
        self.builder.start_node(kind);
    }

    fn checkpoint(&self) -> crate::cst::Checkpoint {
        self.builder.checkpoint()
    }

    fn start_node_at(&mut self, cp: crate::cst::Checkpoint, kind: SyntaxKind) {
        self.builder.start_node_at(cp, kind);
    }

    fn finish_node(&mut self) {
        self.builder.finish_node();
    }

    fn error(&mut self, code: &'static str, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::error(self.current_span(), code, message));
    }

    fn finish(self) -> ParseOutput {
        ParseOutput {
            tree: self.builder.finish(),
            diagnostics: self.diagnostics,
        }
    }

    // ── Recovery ─────────────────────────────────────────────────────

    /// Consume tokens until the cursor reaches a top-level recovery
    /// anchor: a `;` at bracket depth zero, or end of input. Used when
    /// an item production can't proceed.
    fn skip_to_top_level_anchor(&mut self) {
        let mut brace: i32 = 0;
        let mut bracket: i32 = 0;
        let mut paren: i32 = 0;
        loop {
            match self.current() {
                SyntaxKind::EOF => return,
                SyntaxKind::L_BRACE => {
                    brace += 1;
                    self.bump();
                }
                SyntaxKind::R_BRACE => {
                    brace = brace.saturating_sub(1);
                    self.bump();
                }
                SyntaxKind::L_BRACKET => {
                    bracket += 1;
                    self.bump();
                }
                SyntaxKind::R_BRACKET => {
                    bracket = bracket.saturating_sub(1);
                    self.bump();
                }
                SyntaxKind::L_PAREN => {
                    paren += 1;
                    self.bump();
                }
                SyntaxKind::R_PAREN => {
                    paren = paren.saturating_sub(1);
                    self.bump();
                }
                SyntaxKind::SEMICOLON if brace == 0 && bracket == 0 && paren == 0 => {
                    self.bump();
                    return;
                }
                _ => self.bump(),
            }
        }
    }

    // ── Productions ──────────────────────────────────────────────────

    /// Entry point. Wraps every top-level item in a [`SyntaxKind::ROOT`]
    /// node and flushes any trivia at the head or tail of the file.
    fn parse_root(&mut self) {
        self.start_node(SyntaxKind::ROOT);
        self.bump_trivia();
        while self.current() != SyntaxKind::EOF {
            self.parse_item();
        }
        self.bump_trivia();
        self.finish_node();
    }

    /// Dispatch a single top-level item by its leading keyword.
    fn parse_item(&mut self) {
        if self.at_keyword("program") {
            self.parse_program_decl();
        } else if self.at_keyword("oper") {
            self.parse_oper_decl();
        } else {
            self.parse_unknown_item();
        }
    }

    /// `program <name>;`. The trailing semicolon is required; missing
    /// pieces produce a diagnostic but the node still closes cleanly.
    fn parse_program_decl(&mut self) {
        debug_assert!(self.at_keyword("program"));
        self.start_node(SyntaxKind::PROGRAM_DECL);
        self.bump(); // "program"

        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0002", "expected program name");
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0003", "expected `;` after program declaration");
        }

        self.finish_node();
    }

    /// `oper <name> <heading> <body>;`. The return-type clause (`: Type`
    /// or `-> Type`) is intentionally not parsed yet — the syntax for it
    /// is open. Until it settles, an operator with a return type will
    /// trigger the "expected `[`" or "expected `;`" diagnostic on the
    /// stray punctuation.
    fn parse_oper_decl(&mut self) {
        debug_assert!(self.at_keyword("oper"));
        self.start_node(SyntaxKind::OPER_DECL);
        self.bump(); // "oper"

        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0004", "expected operator name");
        }

        if self.at(SyntaxKind::L_BRACE) {
            self.parse_heading();
        } else {
            self.error("P0005", "expected `{` to start parameter heading");
        }

        if self.at(SyntaxKind::L_BRACKET) {
            self.parse_block();
        } else {
            self.error("P0006", "expected `[` to start operator body");
        }

        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0007", "expected `;` after operator declaration");
        }

        self.finish_node();
    }

    /// `{ <param>, … }` — the structural type used both as an operator's
    /// parameter list and as a `Tuple H` heading. Empty `{}` is valid;
    /// trailing comma is accepted.
    fn parse_heading(&mut self) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.start_node(SyntaxKind::HEADING);
        self.bump(); // {

        if self.eat(SyntaxKind::R_BRACE) {
            self.finish_node();
            return;
        }

        loop {
            self.parse_param();

            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            // Trailing comma is OK — `{ x: T, }` is the same as `{ x: T }`.
            if self.at(SyntaxKind::R_BRACE) {
                break;
            }
        }

        if !self.eat(SyntaxKind::R_BRACE) {
            self.error("P0008", "expected `}` to close parameter heading");
        }

        self.finish_node();
    }

    /// `<name>: <type>`. Both the name and the colon are individually
    /// recoverable so a typo in one doesn't sink the whole parameter.
    fn parse_param(&mut self) {
        self.start_node(SyntaxKind::PARAM);

        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0009", "expected parameter name");
        }
        if !self.eat(SyntaxKind::COLON) {
            self.error("P0010", "expected `:` after parameter name");
        }
        self.parse_type_ref();

        self.finish_node();
    }

    /// A type expression. Today only a single identifier is recognized;
    /// `Tuple H`, `Relation H`, `Sequence T`, and qualified names land
    /// alongside the rest of expression parsing.
    fn parse_type_ref(&mut self) {
        self.start_node(SyntaxKind::TYPE_REF);
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0011", "expected type name");
        }
        self.finish_node();
    }

    /// `[ <stmt>; <stmt>; … ]` body. Each statement is parsed
    /// individually; nested `[…]` inside a statement's expression is
    /// handled by that expression's own recursion, not by depth
    /// counting here.
    fn parse_block(&mut self) {
        debug_assert!(self.at(SyntaxKind::L_BRACKET));
        self.start_node(SyntaxKind::BLOCK);
        self.bump(); // [

        while !self.at(SyntaxKind::R_BRACKET) && !self.at(SyntaxKind::EOF) {
            self.parse_stmt();
        }

        if !self.eat(SyntaxKind::R_BRACKET) {
            self.error("P0012", "unclosed operator body");
        }
        self.finish_node();
    }

    /// One statement. Today only the expression-statement form
    /// (`<expr>;`) is recognized; `let` / `mut` / `return` / etc.
    /// arrive when their semantics are settled.
    fn parse_stmt(&mut self) {
        // Defensive: never enter at a block-closing or terminal token.
        if matches!(self.current(), SyntaxKind::R_BRACKET | SyntaxKind::EOF) {
            return;
        }

        let cp = self.checkpoint();
        let before = self.pos;
        self.parse_expr();

        if self.pos == before {
            // No progress — bump one token to avoid infinite loop and
            // let the BLOCK loop try again past it.
            self.bump();
            return;
        }

        self.start_node_at(cp, SyntaxKind::EXPR_STMT);
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after expression");
        }
        self.finish_node();
    }

    /// An expression. Parses a primary, then chains postfix forms
    /// (today only the brace-delimited call form `<expr>{ … }`). Field
    /// access (`.x`) and indexing (`[i]`) join the loop once their
    /// semantic decisions are in.
    fn parse_expr(&mut self) {
        let cp = self.checkpoint();
        if !self.parse_primary_expr() {
            return;
        }

        while self.at(SyntaxKind::L_BRACE) {
            self.start_node_at(cp, SyntaxKind::CALL_EXPR);
            self.parse_arg_list();
            self.finish_node();
        }
    }

    /// A primary expression — the atomic forms an expression can start
    /// with. Returns `true` if anything was consumed.
    fn parse_primary_expr(&mut self) -> bool {
        match self.current() {
            SyntaxKind::IDENT => {
                self.start_node(SyntaxKind::NAME_REF);
                self.bump();
                self.finish_node();
                true
            }
            SyntaxKind::STRING_LIT
            | SyntaxKind::CHAR_LIT
            | SyntaxKind::INTEGER_LIT
            | SyntaxKind::RATIONAL_LIT
            | SyntaxKind::APPROXIMATE_LIT => {
                self.start_node(SyntaxKind::LITERAL);
                self.bump();
                self.finish_node();
                true
            }
            _ => {
                self.error("P0014", "expected expression");
                false
            }
        }
    }

    /// `{ <named_arg>, … }` — the call-site argument list. Empty and
    /// trailing-comma forms are both accepted.
    fn parse_arg_list(&mut self) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.start_node(SyntaxKind::ARG_LIST);
        self.bump(); // {

        if self.eat(SyntaxKind::R_BRACE) {
            self.finish_node();
            return;
        }

        loop {
            self.parse_named_arg();
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            if self.at(SyntaxKind::R_BRACE) {
                break;
            }
        }

        if !self.eat(SyntaxKind::R_BRACE) {
            self.error("P0015", "expected `}` to close argument list");
        }
        self.finish_node();
    }

    /// `<name>: <expr>` inside an argument list. The shorthand bare
    /// `<name>` (= `<name>: <name>`) is deferred.
    fn parse_named_arg(&mut self) {
        self.start_node(SyntaxKind::NAMED_ARG);
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0016", "expected argument name");
        }
        if !self.eat(SyntaxKind::COLON) {
            self.error("P0017", "expected `:` after argument name");
        }
        self.parse_expr();
        self.finish_node();
    }

    /// Wrap an unrecognized top-level item in a `PARSE_ERROR` node and
    /// recover at the next top-level anchor.
    fn parse_unknown_item(&mut self) {
        self.start_node(SyntaxKind::PARSE_ERROR);
        self.error("P0001", "expected a top-level declaration");
        self.skip_to_top_level_anchor();
        self.finish_node();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(src: &str) -> ParseOutput {
        parse(src, FileId(0))
    }

    fn kinds(out: &ParseOutput) -> Vec<SyntaxKind> {
        out.tree
            .children_with_tokens()
            .map(|el| el.kind())
            .collect()
    }

    #[test]
    fn empty_input_yields_just_root() {
        let out = parse_str("");
        assert_eq!(out.tree.kind(), SyntaxKind::ROOT);
        assert!(out.tree.children_with_tokens().next().is_none());
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn whitespace_only_attaches_to_root() {
        let out = parse_str("   \n  ");
        assert_eq!(out.tree.text(), "   \n  ");
        assert_eq!(kinds(&out), vec![SyntaxKind::WHITESPACE]);
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn comments_attach_to_root() {
        let src = "// a comment\n/* and a block */\n";
        let out = parse_str(src);
        assert_eq!(out.tree.text(), src);
        assert_eq!(
            kinds(&out),
            vec![
                SyntaxKind::LINE_COMMENT,
                SyntaxKind::WHITESPACE,
                SyntaxKind::BLOCK_COMMENT,
                SyntaxKind::WHITESPACE,
            ]
        );
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn minimal_program_decl() {
        let out = parse_str("program foo;");
        assert!(out.diagnostics.is_empty());
        assert_eq!(out.tree.text(), "program foo;");
        assert_eq!(kinds(&out), vec![SyntaxKind::PROGRAM_DECL]);

        let decl = out.tree.first_child().unwrap();
        let token_kinds: Vec<_> = decl.children_with_tokens().map(|el| el.kind()).collect();
        assert_eq!(
            token_kinds,
            vec![
                SyntaxKind::IDENT,
                SyntaxKind::WHITESPACE,
                SyntaxKind::IDENT,
                SyntaxKind::SEMICOLON,
            ]
        );
    }

    #[test]
    fn program_decl_with_leading_and_trailing_trivia() {
        let src = "  // hi\n  program foo;  \n";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty());
        assert_eq!(out.tree.text(), src);

        // Leading trivia attaches to ROOT, the decl is in the middle,
        // trailing whitespace closes the file at ROOT level.
        let top: Vec<_> = out
            .tree
            .children_with_tokens()
            .map(|el| el.kind())
            .collect();
        assert_eq!(
            top,
            vec![
                SyntaxKind::WHITESPACE,
                SyntaxKind::LINE_COMMENT,
                SyntaxKind::WHITESPACE,
                SyntaxKind::PROGRAM_DECL,
                SyntaxKind::WHITESPACE,
            ]
        );
    }

    #[test]
    fn missing_program_name_diagnoses() {
        let out = parse_str("program ;");
        assert_eq!(out.tree.text(), "program ;");
        assert_eq!(kinds(&out), vec![SyntaxKind::PROGRAM_DECL]);
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0002"),
            "expected P0002 diagnostic, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn missing_semicolon_diagnoses() {
        let out = parse_str("program foo");
        assert_eq!(out.tree.text(), "program foo");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0003"));
    }

    #[test]
    fn unknown_top_level_item_becomes_parse_error_and_recovers() {
        // `oper main {} [];` isn't recognized yet — it should wrap in
        // PARSE_ERROR, recover at the top-level `;`, and then parse
        // the `program` decl that follows.
        let src = "oper main {} []; program foo;";
        let out = parse_str(src);
        assert_eq!(out.tree.text(), src);

        let top: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(top, vec![SyntaxKind::PARSE_ERROR, SyntaxKind::PROGRAM_DECL]);
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0001"),
            "expected P0001 diagnostic, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn recovery_respects_bracket_depth() {
        // The `;` inside `[ ... ]` must not be treated as a top-level
        // sync point — the recovery has to wait for the outer `;`.
        let src = "weird { a; b; } [ c; ]; program ok;";
        let out = parse_str(src);
        assert_eq!(out.tree.text(), src);

        let top: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(top, vec![SyntaxKind::PARSE_ERROR, SyntaxKind::PROGRAM_DECL]);
    }

    #[test]
    fn multiple_program_decls() {
        let out = parse_str("program a; program b;");
        assert!(out.diagnostics.is_empty());
        let top: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            top,
            vec![SyntaxKind::PROGRAM_DECL, SyntaxKind::PROGRAM_DECL]
        );
    }

    #[test]
    fn minimal_oper_decl() {
        let out = parse_str("oper main {} [];");
        assert!(
            out.diagnostics.is_empty(),
            "expected no diagnostics, got {:?}",
            out.diagnostics
        );
        assert_eq!(out.tree.text(), "oper main {} [];");

        let decl = out.tree.first_child().unwrap();
        assert_eq!(decl.kind(), SyntaxKind::OPER_DECL);

        let child_kinds: Vec<_> = decl
            .children_with_tokens()
            .map(|el| el.kind())
            .filter(|k| !k.is_trivia())
            .collect();
        assert_eq!(
            child_kinds,
            vec![
                SyntaxKind::IDENT, // "oper"
                SyntaxKind::IDENT, // "main"
                SyntaxKind::HEADING,
                SyntaxKind::BLOCK,
                SyntaxKind::SEMICOLON,
            ]
        );
    }

    #[test]
    fn oper_decl_with_single_param() {
        let out = parse_str("oper add { x: Integer } [];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);

        let decl = out.tree.first_child().unwrap();
        let heading = decl
            .children()
            .find(|n| n.kind() == SyntaxKind::HEADING)
            .unwrap();
        let params: Vec<_> = heading.children().collect();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].kind(), SyntaxKind::PARAM);
        assert_eq!(params[0].text(), "x: Integer");
    }

    #[test]
    fn oper_decl_with_multiple_params() {
        let out = parse_str("oper add { x: Integer, y: Integer } [];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let decl = out.tree.first_child().unwrap();
        let heading = decl
            .children()
            .find(|n| n.kind() == SyntaxKind::HEADING)
            .unwrap();
        let params: Vec<_> = heading.children().collect();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].text(), "x: Integer");
        assert_eq!(params[1].text(), "y: Integer");
    }

    #[test]
    fn oper_decl_with_trailing_comma_in_heading() {
        let out = parse_str("oper f { x: Integer, } [];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let heading = out
            .tree
            .first_child()
            .unwrap()
            .children()
            .find(|n| n.kind() == SyntaxKind::HEADING)
            .unwrap();
        assert_eq!(heading.children().count(), 1);
    }

    #[test]
    fn block_with_single_expr_stmt() {
        let out = parse_str("oper f {} [ x; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let block = out
            .tree
            .first_child()
            .unwrap()
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap();
        let stmts: Vec<_> = block.children().collect();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].kind(), SyntaxKind::EXPR_STMT);
        let name_ref = stmts[0].first_child().unwrap();
        assert_eq!(name_ref.kind(), SyntaxKind::NAME_REF);
        assert_eq!(name_ref.text(), "x");
    }

    #[test]
    fn literal_expr_stmt() {
        let out = parse_str("oper f {} [ \"hi\"; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let stmt = out
            .tree
            .first_child()
            .unwrap()
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap()
            .first_child()
            .unwrap();
        let lit = stmt.first_child().unwrap();
        assert_eq!(lit.kind(), SyntaxKind::LITERAL);
        assert_eq!(lit.text(), "\"hi\"");
    }

    #[test]
    fn call_with_no_args() {
        let out = parse_str("oper f {} [ foo{}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let call = out
            .tree
            .first_child()
            .unwrap()
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap()
            .first_child()
            .unwrap()
            .first_child()
            .unwrap();
        assert_eq!(call.kind(), SyntaxKind::CALL_EXPR);
        let kids: Vec<_> = call.children().map(|n| n.kind()).collect();
        assert_eq!(kids, vec![SyntaxKind::NAME_REF, SyntaxKind::ARG_LIST]);
        let arg_list = call
            .children()
            .find(|n| n.kind() == SyntaxKind::ARG_LIST)
            .unwrap();
        assert_eq!(arg_list.children().count(), 0);
    }

    #[test]
    fn call_with_named_args_and_trailing_comma() {
        let out = parse_str("oper f {} [ foo{x: 1, y: 2,}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let arg_list = out
            .tree
            .first_child()
            .unwrap()
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap()
            .first_child()
            .unwrap()
            .first_child()
            .unwrap()
            .children()
            .find(|n| n.kind() == SyntaxKind::ARG_LIST)
            .unwrap();
        let args: Vec<_> = arg_list.children().collect();
        assert_eq!(args.len(), 2);
        assert!(args.iter().all(|a| a.kind() == SyntaxKind::NAMED_ARG));
        assert_eq!(args[0].text(), "x: 1");
        assert_eq!(args[1].text(), "y: 2");
    }

    #[test]
    fn multiple_statements_in_block() {
        let out = parse_str("oper f {} [ a; b; c; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let stmts: Vec<_> = out
            .tree
            .first_child()
            .unwrap()
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap()
            .children()
            .collect();
        assert_eq!(stmts.len(), 3);
        assert!(stmts.iter().all(|s| s.kind() == SyntaxKind::EXPR_STMT));
    }

    #[test]
    fn hello_world_parses_clean_with_full_structure() {
        let src = "program hello_world;\n\
                   \n\
                   oper main {}\n\
                   [\n\
                       write_line{message: \"Hello, world!\"};\n\
                   ];\n";
        let out = parse_str(src);
        assert!(
            out.diagnostics.is_empty(),
            "expected hello-world to parse clean, got {:?}",
            out.diagnostics
        );
        assert_eq!(out.tree.text(), src);

        let top_kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            top_kinds,
            vec![SyntaxKind::PROGRAM_DECL, SyntaxKind::OPER_DECL]
        );

        let oper = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::OPER_DECL)
            .unwrap();
        let block = oper
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap();
        let expr_stmt = block.first_child().unwrap();
        assert_eq!(expr_stmt.kind(), SyntaxKind::EXPR_STMT);
        let call = expr_stmt.first_child().unwrap();
        assert_eq!(call.kind(), SyntaxKind::CALL_EXPR);

        let call_kids: Vec<_> = call.children().map(|n| n.kind()).collect();
        assert_eq!(call_kids, vec![SyntaxKind::NAME_REF, SyntaxKind::ARG_LIST]);

        let arg = call
            .children()
            .find(|n| n.kind() == SyntaxKind::ARG_LIST)
            .unwrap()
            .first_child()
            .unwrap();
        assert_eq!(arg.kind(), SyntaxKind::NAMED_ARG);
        assert_eq!(arg.first_child().unwrap().kind(), SyntaxKind::LITERAL);
    }

    #[test]
    fn missing_stmt_semicolon_diagnoses() {
        let out = parse_str("oper f {} [ x ];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0013"));
    }

    #[test]
    fn missing_expr_at_stmt_start_diagnoses() {
        let out = parse_str("oper f {} [ ; ];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0014"));
    }

    #[test]
    fn missing_arg_list_close_diagnoses() {
        let out = parse_str("oper f {} [ foo{x: 1; ];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0015"));
    }

    #[test]
    fn missing_arg_name_diagnoses() {
        let out = parse_str("oper f {} [ foo{: 1}; ];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0016"));
    }

    #[test]
    fn missing_arg_colon_diagnoses() {
        let out = parse_str("oper f {} [ foo{x 1}; ];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0017"));
    }

    #[test]
    fn missing_oper_name_diagnoses() {
        let out = parse_str("oper {} [];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0004"));
    }

    #[test]
    fn missing_oper_heading_diagnoses() {
        let out = parse_str("oper main [];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0005"));
    }

    #[test]
    fn missing_oper_body_diagnoses() {
        let out = parse_str("oper main {};");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0006"));
    }

    #[test]
    fn missing_oper_semicolon_diagnoses() {
        let out = parse_str("oper main {} []");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0007"));
    }

    #[test]
    fn missing_param_colon_diagnoses() {
        let out = parse_str("oper f { x Integer } [];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0010"));
    }

    #[test]
    fn missing_param_type_diagnoses() {
        let out = parse_str("oper f { x: } [];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0011"));
    }

    #[test]
    fn unclosed_body_diagnoses() {
        let out = parse_str("oper f {} [ stuff ");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0012"));
    }

    #[test]
    fn every_byte_round_trips() {
        // Pile of every category — leading comment, blank line, decl,
        // unknown item, trailing whitespace — and verify the tree
        // reproduces the source byte-for-byte.
        let src = "// header\n\n\
                   program first;\n\
                   garbage { x; y; };\n\
                   program second;\n";
        let out = parse_str(src);
        assert_eq!(out.tree.text(), src);
    }
}

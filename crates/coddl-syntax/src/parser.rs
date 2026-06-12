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
use crate::file_kind::FileKind;
use crate::lexer::{lex, LexOutput};
use crate::syntax_kind::SyntaxKind;
use crate::token::{Token, TokenKind};
use crate::ParseOutput;

/// Tokenize and parse a source buffer in the given dialect.
///
/// `FileKind` discriminates which root production runs:
///
/// - [`FileKind::Cd`]      → application source (`program`, `oper`, …)
/// - [`FileKind::Cddb`]    → database catalog (`database`, `base relvar`)
/// - [`FileKind::Cdmap`]   → external→conceptual adapter (`map`, identity)
/// - [`FileKind::Cdstore`] → conceptual→physical binding (`store for`, …)
///
/// The lexer is shared across all kinds (Coddl has no reserved words);
/// only the parser dispatch differs.
pub fn parse(source: &str, file: FileId, kind: FileKind) -> ParseOutput {
    let lex_out = lex(source, file);
    let mut p = Parser::new(source, file, lex_out);
    match kind {
        FileKind::Cd => p.parse_root(),
        FileKind::Cddb => crate::parser_cddb::parse_cddb_root(&mut p),
        FileKind::Cdmap => crate::parser_cdmap::parse_cdmap_root(&mut p),
        FileKind::Cdstore => crate::parser_cdstore::parse_cdstore_root(&mut p),
    }
    p.finish()
}

pub(crate) struct Parser<'a> {
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
    /// Build a parser over a freshly lexed token stream.
    pub(crate) fn new(source: &'a str, file: FileId, lex_out: LexOutput) -> Self {
        Self {
            source,
            file,
            tokens: lex_out.tokens,
            pos: 0,
            builder: CstBuilder::new(source),
            diagnostics: lex_out.diagnostics,
        }
    }

    // ── Cursor primitives ────────────────────────────────────────────

    /// Peek the kind of the next non-trivia token, or [`SyntaxKind::EOF`].
    pub(crate) fn current(&self) -> SyntaxKind {
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
    pub(crate) fn current_span(&self) -> Span {
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
    pub(crate) fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    /// True iff the next non-trivia token is an identifier whose lexeme
    /// is `lexeme`. Used for contextual keyword recognition (Coddl has
    /// no reserved words; every keyword is contextual).
    pub(crate) fn at_keyword(&self, lexeme: &str) -> bool {
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
    pub(crate) fn bump_trivia(&mut self) {
        while self.pos < self.tokens.len() && self.tokens[self.pos].kind.is_trivia() {
            let tok = self.tokens[self.pos];
            let range = tok.span.start as usize..tok.span.end as usize;
            self.builder.token(SyntaxKind::from(tok.kind), range);
            self.pos += 1;
        }
    }

    /// Emit any pending trivia and then the next non-trivia token. The
    /// synthetic EOF token is recognized and not emitted.
    pub(crate) fn bump(&mut self) {
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
    pub(crate) fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn start_node(&mut self, kind: SyntaxKind) {
        self.builder.start_node(kind);
    }

    pub(crate) fn checkpoint(&self) -> crate::cst::Checkpoint {
        self.builder.checkpoint()
    }

    pub(crate) fn start_node_at(&mut self, cp: crate::cst::Checkpoint, kind: SyntaxKind) {
        self.builder.start_node_at(cp, kind);
    }

    pub(crate) fn finish_node(&mut self) {
        self.builder.finish_node();
    }

    pub(crate) fn error(&mut self, code: &'static str, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::error(self.current_span(), code, message));
    }

    pub(crate) fn finish(self) -> ParseOutput {
        ParseOutput {
            tree: self.builder.finish(),
            diagnostics: self.diagnostics,
        }
    }

    // ── Recovery ─────────────────────────────────────────────────────

    /// Consume tokens until the cursor reaches a top-level recovery
    /// anchor: a `;` at bracket depth zero, or end of input. Used when
    /// an item production can't proceed.
    pub(crate) fn skip_to_top_level_anchor(&mut self) {
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

    /// Entry point for `.cd` source. Wraps every top-level item in a
    /// [`SyntaxKind::ROOT`] node and flushes any trivia at the head or
    /// tail of the file.
    pub(crate) fn parse_root(&mut self) {
        self.start_node(SyntaxKind::ROOT);
        self.bump_trivia();
        while self.current() != SyntaxKind::EOF {
            self.parse_item();
        }
        self.bump_trivia();
        self.finish_node();
    }

    /// Dispatch a single top-level item by its leading keyword.
    ///
    /// All four relvar kinds (public/private/base/virtual) parse here —
    /// `.cd` legitimately accepts public/private; base/virtual parse so
    /// the typechecker can emit T0014 (relvar kind not legal for this
    /// dialect) on the resulting tree rather than producing a generic
    /// P0001 parse error.
    fn parse_item(&mut self) {
        if self.at_keyword("program") {
            self.parse_program_decl();
        } else if self.at_keyword("database") {
            self.parse_database_binding();
        } else if self.at_keyword("public") {
            self.parse_public_relvar_decl();
        } else if self.at_keyword("private") {
            self.parse_private_relvar_decl();
        } else if self.at_keyword("base") {
            crate::parser_cddb::parse_base_relvar_decl(self);
        } else if self.at_keyword("virtual") {
            crate::parser_cddb::parse_virtual_relvar_decl(self);
        } else if self.at_keyword("oper") {
            self.parse_oper_decl();
        } else {
            self.parse_unknown_item();
        }
    }

    /// `database <Name>;` — binds this program to a catalog. The
    /// compiler discovers `<Name>.cddb` and `<Name>.cdstore` from the
    /// declared name. v1 expects at most one binding per program;
    /// duplicate bindings are tolerated at parse time and caught by
    /// downstream validation (Phase 16).
    fn parse_database_binding(&mut self) {
        debug_assert!(self.at_keyword("database"));
        self.start_node(SyntaxKind::DATABASE_BINDING);
        self.bump(); // `database`

        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0020", "expected database name");
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0021", "expected `;` after `database <Name>`");
        }

        self.finish_node();
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

        // Optional `-> <type-ref>` return clause between heading and
        // body. Absent → implicit `Tuple {}` (unit) return.
        if self.at(SyntaxKind::ARROW) {
            self.parse_return_clause();
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
    /// trailing comma is accepted. Shared with `.cddb` relvar
    /// declarations.
    pub(crate) fn parse_heading(&mut self) {
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
        self.bump_trivia();
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
    pub(crate) fn parse_type_ref(&mut self) {
        self.bump_trivia();
        self.start_node(SyntaxKind::TYPE_REF);
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0011", "expected type name");
        }
        self.finish_node();
    }

    /// `-> <type-ref>` — the optional return-type clause on an `oper`
    /// declaration. Absent → the operator implicitly returns
    /// `Tuple {}` (unit).
    fn parse_return_clause(&mut self) {
        debug_assert!(self.at(SyntaxKind::ARROW));
        self.start_node(SyntaxKind::RETURN_CLAUSE);
        self.bump(); // `->`
        self.parse_type_ref();
        self.finish_node();
    }

    /// `[ <stmt>; <stmt>; … <tail-expr>? ]` body. Statements are
    /// terminated by `;`; the final item, if it lacks a trailing `;`,
    /// is the block's tail expression and becomes the block's value.
    /// Nested `[…]` inside a statement's expression is handled by
    /// that expression's own recursion, not by depth counting here.
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

    /// `let <name> [: <type-ref>] = <expr>;` — a value binding visible
    /// to subsequent statements in the same block. Type annotation is
    /// optional; when absent, the binding's type is inferred from the
    /// RHS. No `mut`, no destructuring for now.
    fn parse_let_stmt(&mut self) {
        debug_assert!(self.at_keyword("let"));
        self.start_node(SyntaxKind::LET_STMT);
        self.bump(); // `let`

        self.bump_trivia();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0018", "expected binding name in `let`");
        }
        // Optional `: <type-ref>` annotation between the name and `=`.
        if self.eat(SyntaxKind::COLON) {
            self.parse_type_ref();
        }
        if !self.eat(SyntaxKind::EQ) {
            self.error("P0018", "expected `=` in `let`");
        }
        let before = self.pos;
        self.parse_expr();
        if self.pos == before {
            self.error("P0018", "expected expression in `let`");
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `let`");
        }
        self.finish_node();
    }

    /// One statement, *or* the block's trailing tail expression. The
    /// `let` form is recognized first; otherwise an expression is
    /// parsed and either wrapped in `EXPR_STMT` (terminated by `;`)
    /// or left as a bare child under `BLOCK` (the tail expression,
    /// immediately followed by `]`).
    fn parse_stmt(&mut self) {
        if matches!(self.current(), SyntaxKind::R_BRACKET | SyntaxKind::EOF) {
            return;
        }
        if self.at_keyword("let") {
            self.parse_let_stmt();
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

        // Tail expression: no trailing `;`, immediately followed by
        // the block's closing `]`. Leave the Expr bare under BLOCK.
        if self.at(SyntaxKind::R_BRACKET) {
            return;
        }

        // Expression statement: wrap in EXPR_STMT, expect `;`.
        self.start_node_at(cp, SyntaxKind::EXPR_STMT);
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after expression");
        }
        self.finish_node();
    }

    /// An expression. Parses a primary, then chains postfix forms:
    /// brace-delimited call (`<expr>{ … }`) and dot-prefixed field
    /// access (`<expr>.<name>`). The loop iterates so chained postfix
    /// (`f{}.x`, `t.a.b`, etc.) works uniformly.
    fn parse_expr(&mut self) {
        // Flush any leading trivia into the parent node before the
        // checkpoint — otherwise a retroactive `start_node_at(cp, …)`
        // for CALL_EXPR / FIELD_ACCESS would wrap the trivia inside the
        // expression.
        self.bump_trivia();
        let cp = self.checkpoint();
        if !self.parse_primary_expr() {
            return;
        }

        loop {
            match self.current() {
                SyntaxKind::L_BRACE => {
                    self.start_node_at(cp, SyntaxKind::CALL_EXPR);
                    self.parse_arg_list();
                    self.finish_node();
                }
                SyntaxKind::DOT => {
                    self.start_node_at(cp, SyntaxKind::FIELD_ACCESS);
                    self.bump(); // `.`
                    self.bump_trivia();
                    if !self.eat(SyntaxKind::IDENT) {
                        self.error("P0030", "expected field name after `.`");
                    }
                    self.finish_node();
                }
                _ => break,
            }
        }
    }

    /// A primary expression — the atomic forms an expression can start
    /// with. Returns `true` if anything was consumed.
    fn parse_primary_expr(&mut self) -> bool {
        if self.at_keyword("transaction") {
            self.parse_transaction_expr();
            return true;
        }
        match self.current() {
            SyntaxKind::IDENT => {
                self.bump_trivia();
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
                self.bump_trivia();
                self.start_node(SyntaxKind::LITERAL);
                self.bump();
                self.finish_node();
                true
            }
            SyntaxKind::L_BRACE => {
                self.parse_tuple_lit();
                true
            }
            _ => {
                self.error("P0014", "expected expression");
                false
            }
        }
    }

    /// `{ <named_arg>, … }` in expression position — a tuple literal.
    /// Empty `{}` is the unit value (`Tuple {}`); a non-empty form
    /// names each attribute. Same grammar as [`parse_arg_list`], but
    /// the wrapping node is `TUPLE_LIT` so the AST can distinguish a
    /// tuple value from a call-site argument list.
    fn parse_tuple_lit(&mut self) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.bump_trivia();
        self.start_node(SyntaxKind::TUPLE_LIT);
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
            self.error("P0029", "expected `}` to close tuple literal");
        }
        self.finish_node();
    }

    /// `transaction [ ... ]` — a block expression. The body parses
    /// as a normal block (statements + optional tail expression);
    /// `transaction` itself contributes no runtime semantics today.
    fn parse_transaction_expr(&mut self) {
        debug_assert!(self.at_keyword("transaction"));
        self.bump_trivia();
        self.start_node(SyntaxKind::TRANSACTION_EXPR);
        self.bump(); // `transaction`

        if !self.at(SyntaxKind::L_BRACKET) {
            self.error("P0019", "expected `[` after `transaction`");
            self.finish_node();
            return;
        }
        self.parse_block();
        self.finish_node();
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
        self.bump_trivia();
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

    /// `public relvar <Name> <heading> <key-clause>* ;` — an
    /// application-side relvar exposed to the catalog. The kind
    /// keyword is at the cursor when this is called.
    pub(crate) fn parse_public_relvar_decl(&mut self) {
        debug_assert!(self.at_keyword("public"));
        self.parse_relvar_with_heading(SyntaxKind::PUBLIC_RELVAR_DECL);
    }

    /// `private relvar <Name> <heading> <key-clause>* ;` — an
    /// application-side relvar internal to the program.
    pub(crate) fn parse_private_relvar_decl(&mut self) {
        debug_assert!(self.at_keyword("private"));
        self.parse_relvar_with_heading(SyntaxKind::PRIVATE_RELVAR_DECL);
    }

    /// Shared shape: `<KIND> relvar <Name> <heading> <key-clause>* ;`.
    /// Caller has already verified the cursor is at the kind keyword
    /// (`public` or `private`); this routine bumps it and parses the
    /// rest. Multi-key declarations (`key { a } key { b }`) parse —
    /// the typechecker validates one key for v1 (per Phase 15 plan).
    ///
    /// Diagnostics: P0025 (no `relvar`), P0026 (no name), P0027 (no
    /// `{` heading), P0028 (no `;`).
    fn parse_relvar_with_heading(&mut self, cst_kind: SyntaxKind) {
        self.bump_trivia();
        self.start_node(cst_kind);
        self.bump(); // kind keyword (`public` / `private`)

        if !self.at_keyword("relvar") {
            self.error("P0025", "expected `relvar` after relvar kind");
        } else {
            self.bump(); // `relvar`
        }

        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0026", "expected relvar name");
        }

        if self.at(SyntaxKind::L_BRACE) {
            self.parse_heading();
        } else {
            self.error("P0027", "expected `{` to start relvar heading");
        }

        while self.at_keyword("key") {
            self.parse_key_clause();
        }

        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0028", "expected `;` after relvar declaration");
        }

        self.finish_node();
    }

    /// `key { a, b, … }` — candidate-key clause on a relvar declaration.
    /// Shared between `.cddb` base relvars (today) and `.cd` public /
    /// private relvars (Phase 15). The leading `key` keyword has already
    /// been seen at the dispatch site.
    ///
    /// Diagnostics: P0022 (no `{`), P0023 (no attribute name), P0024
    /// (no `}`).
    pub(crate) fn parse_key_clause(&mut self) {
        debug_assert!(self.at_keyword("key"));
        self.bump_trivia();
        self.start_node(SyntaxKind::KEY_CLAUSE);
        self.bump(); // `key`

        if !self.eat(SyntaxKind::L_BRACE) {
            self.error("P0022", "expected `{` to start key clause");
            self.finish_node();
            return;
        }

        if self.eat(SyntaxKind::R_BRACE) {
            self.finish_node();
            return;
        }

        loop {
            self.bump_trivia();
            if !self.eat(SyntaxKind::IDENT) {
                self.error("P0023", "expected key attribute name");
                break;
            }
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            // Trailing comma ok.
            if self.at(SyntaxKind::R_BRACE) {
                break;
            }
        }

        if !self.eat(SyntaxKind::R_BRACE) {
            self.error("P0024", "expected `}` to close key clause");
        }

        self.finish_node();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(src: &str) -> ParseOutput {
        parse(src, FileId(0), FileKind::Cd)
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
    fn database_binding_parses_clean() {
        let out = parse_str("program p;\ndatabase greetings;\n");
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![SyntaxKind::PROGRAM_DECL, SyntaxKind::DATABASE_BINDING]
        );
    }

    #[test]
    fn database_binding_missing_name_diagnoses_p0020() {
        let out = parse_str("database ;");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0020"));
    }

    #[test]
    fn database_binding_missing_semicolon_diagnoses_p0021() {
        let out = parse_str("database greetings");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0021"));
    }

    #[test]
    fn public_relvar_parses_minimum() {
        let out = parse_str("public relvar X {};");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(kinds, vec![SyntaxKind::PUBLIC_RELVAR_DECL]);
    }

    #[test]
    fn private_relvar_parses_full_form() {
        let src = "private relvar X { a: Integer, b: Text } key { a };";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(kinds, vec![SyntaxKind::PRIVATE_RELVAR_DECL]);
    }

    #[test]
    fn public_relvar_supports_multi_key() {
        let src = "public relvar X { a: Integer, b: Integer } key { a } key { b };";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let relvar = out.tree.first_child().unwrap();
        let keys: Vec<_> = relvar
            .children()
            .filter(|n| n.kind() == SyntaxKind::KEY_CLAUSE)
            .collect();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn relvar_missing_relvar_keyword_diagnoses_p0025() {
        let out = parse_str("public X {};");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0025"));
    }

    #[test]
    fn relvar_missing_name_diagnoses_p0026() {
        let out = parse_str("public relvar {};");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0026"));
    }

    #[test]
    fn relvar_missing_heading_diagnoses_p0027() {
        let out = parse_str("public relvar X;");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0027"));
    }

    #[test]
    fn relvar_missing_semicolon_diagnoses_p0028() {
        let out = parse_str("public relvar X {}");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0028"));
    }

    #[test]
    fn base_relvar_parses_in_cd_dialect() {
        // `.cd` accepts `base relvar` so the typechecker can emit T0014
        // on the BASE_RELVAR_DECL node. Parser-side: zero diagnostics.
        let out = parse_str("base relvar X { a: Integer };");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(kinds, vec![SyntaxKind::BASE_RELVAR_DECL]);
    }

    #[test]
    fn unknown_top_level_item_becomes_parse_error_and_recovers() {
        // `relvar foo { x: T };` isn't recognized at the top level
        // yet — it should wrap in PARSE_ERROR, recover at the
        // top-level `;`, and then parse the `program` decl that
        // follows.
        let src = "relvar foo { x: T }; program foo;";
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
    fn expr_without_trailing_semicolon_is_tail_expression() {
        // Under tail-expression semantics, an Expr immediately before
        // `]` is the block's tail value — no `;` required, no P0013.
        let out = parse_str("oper f {} [ x ];");
        assert!(
            !out.diagnostics.iter().any(|d| d.code == "P0013"),
            "tail expression should not require `;`: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn middle_stmt_without_semicolon_still_diagnoses_p0013() {
        // An expression followed by another statement (not `]`) still
        // needs `;`. This is the original P0013 trigger condition.
        let out = parse_str("oper f {} [ x y; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
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
    fn let_stmt_parses_with_canonical_form() {
        let out = parse_str("oper f {} [ let x = 1; ];");
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        let oper = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::OPER_DECL)
            .unwrap();
        let body = oper
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap();
        let let_stmt = body
            .children()
            .find(|n| n.kind() == SyntaxKind::LET_STMT)
            .expect("LET_STMT child");
        // Inside the let: an IDENT (binding name) and a LITERAL (RHS).
        let kinds: Vec<_> = let_stmt
            .children_with_tokens()
            .filter_map(|e| {
                e.as_token()
                    .map(|t| t.kind())
                    .or_else(|| e.as_node().map(|n| n.kind()))
            })
            .collect();
        assert!(kinds.contains(&SyntaxKind::IDENT), "no IDENT in {kinds:?}");
        assert!(
            kinds.contains(&SyntaxKind::LITERAL),
            "no LITERAL in {kinds:?}"
        );
    }

    #[test]
    fn let_stmt_missing_equals_diagnoses_p0018() {
        let out = parse_str("oper f {} [ let x 1; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0018"),
            "expected P0018, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn let_stmt_missing_name_diagnoses_p0018() {
        let out = parse_str("oper f {} [ let = 1; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0018"),
            "expected P0018, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn transaction_expr_parses_with_tail() {
        let out = parse_str("oper f {} [ let x = transaction [ \"ok\" ]; ];");
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        // The let's value is a TRANSACTION_EXPR whose body's tail is a
        // LITERAL.
        let oper = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::OPER_DECL)
            .unwrap();
        let body = oper
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap();
        let let_stmt = body
            .children()
            .find(|n| n.kind() == SyntaxKind::LET_STMT)
            .unwrap();
        let txn = let_stmt
            .children()
            .find(|n| n.kind() == SyntaxKind::TRANSACTION_EXPR)
            .expect("TRANSACTION_EXPR child of LET_STMT");
        let txn_body = txn
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap();
        // Tail expression is a direct LITERAL child of the inner BLOCK.
        assert!(txn_body.children().any(|n| n.kind() == SyntaxKind::LITERAL));
    }

    #[test]
    fn transaction_missing_bracket_diagnoses_p0019() {
        let out = parse_str("oper f {} [ let x = transaction \"oops\"; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0019"),
            "expected P0019, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn block_with_only_tail_expression_has_no_expr_stmt() {
        let out = parse_str("oper f {} [ x ];");
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        let oper = out
            .tree
            .children()
            .find(|n| n.kind() == SyntaxKind::OPER_DECL)
            .unwrap();
        let body = oper
            .children()
            .find(|n| n.kind() == SyntaxKind::BLOCK)
            .unwrap();
        assert!(
            body.children().all(|n| n.kind() != SyntaxKind::EXPR_STMT),
            "expected no EXPR_STMT"
        );
        assert!(body.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
    }

    #[test]
    fn let_stmt_with_annotation_parses() {
        let out = parse_str("oper f {} [ let x: Integer = 1; ];");
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        // The let now has a TYPE_REF child between the name and the
        // RHS literal.
        let let_stmt = find_first(&out.tree, SyntaxKind::LET_STMT);
        let kinds: Vec<_> = let_stmt.children().map(|c| c.kind()).collect();
        assert!(
            kinds.contains(&SyntaxKind::TYPE_REF),
            "no TYPE_REF in {kinds:?}"
        );
        assert!(
            kinds.contains(&SyntaxKind::LITERAL),
            "no LITERAL in {kinds:?}"
        );
    }

    #[test]
    fn let_stmt_with_annotation_missing_type_diagnoses_p0011() {
        let out = parse_str("oper f {} [ let x: = 1; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0011"),
            "expected P0011, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn oper_decl_with_return_type_parses() {
        let out = parse_str("oper f {} -> Text [ \"hi\" ];");
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        let oper = find_first(&out.tree, SyntaxKind::OPER_DECL);
        let kinds: Vec<_> = oper.children().map(|c| c.kind()).collect();
        assert!(
            kinds.contains(&SyntaxKind::RETURN_CLAUSE),
            "no RETURN_CLAUSE in {kinds:?}"
        );
        let return_clause = oper
            .children()
            .find(|n| n.kind() == SyntaxKind::RETURN_CLAUSE)
            .unwrap();
        assert!(
            return_clause
                .children()
                .any(|n| n.kind() == SyntaxKind::TYPE_REF),
            "RETURN_CLAUSE missing TYPE_REF"
        );
    }

    #[test]
    fn oper_decl_return_type_missing_after_arrow_diagnoses_p0011() {
        let out = parse_str("oper f {} -> [ \"hi\" ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0011"),
            "expected P0011, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn empty_tuple_literal_parses() {
        let out = parse_str("oper f {} [ let t = {}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tuple = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .expect("TUPLE_LIT in tree");
        assert_eq!(tuple.children().count(), 0);
        assert_eq!(tuple.text(), "{}");
    }

    #[test]
    fn tuple_literal_with_named_fields_parses() {
        let out = parse_str("oper f {} [ let t = {a: 1, b: \"x\"}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tuple = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .expect("TUPLE_LIT in tree");
        let fields: Vec<_> = tuple.children().collect();
        assert_eq!(fields.len(), 2);
        assert!(fields.iter().all(|f| f.kind() == SyntaxKind::NAMED_ARG));
    }

    #[test]
    fn tuple_literal_trailing_comma_parses() {
        let out = parse_str("oper f {} [ let t = {a: 1, b: 2,}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tuple = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .expect("TUPLE_LIT in tree");
        assert_eq!(tuple.children().count(), 2);
    }

    #[test]
    fn unterminated_tuple_literal_diagnoses_p0029() {
        let out = parse_str("oper f {} [ let t = {a: 1 ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0029"),
            "expected P0029, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn field_access_parses() {
        let out = parse_str("oper f {} [ t.message; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let fa = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FIELD_ACCESS)
            .expect("FIELD_ACCESS in tree");
        // Base is a NAME_REF; the IDENT after `.` is the field token.
        let base = fa.first_child().unwrap();
        assert_eq!(base.kind(), SyntaxKind::NAME_REF);
        assert_eq!(base.text(), "t");
        assert_eq!(fa.text(), "t.message");
    }

    #[test]
    fn chained_field_access_nests() {
        let out = parse_str("oper f {} [ t.a.b; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        // Outer FIELD_ACCESS wraps the inner one.
        let outer = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FIELD_ACCESS)
            .expect("FIELD_ACCESS in tree");
        let inner = outer.first_child().unwrap();
        assert_eq!(inner.kind(), SyntaxKind::FIELD_ACCESS);
        assert_eq!(inner.text(), "t.a");
        assert_eq!(outer.text(), "t.a.b");
    }

    #[test]
    fn tuple_then_field_access_chain() {
        // `{a: 1}.a` — a tuple literal followed by a field access.
        let out = parse_str("oper f {} [ {a: 1}.a; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let fa = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FIELD_ACCESS)
            .expect("FIELD_ACCESS in tree");
        let base = fa.first_child().unwrap();
        assert_eq!(base.kind(), SyntaxKind::TUPLE_LIT);
    }

    #[test]
    fn field_access_missing_ident_diagnoses_p0030() {
        let out = parse_str("oper f {} [ t.; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0030"),
            "expected P0030, got {:?}",
            out.diagnostics
        );
    }

    fn find_first(root: &crate::cst::SyntaxNode, kind: SyntaxKind) -> crate::cst::SyntaxNode {
        fn descend(n: &crate::cst::SyntaxNode, kind: SyntaxKind) -> Option<crate::cst::SyntaxNode> {
            if n.kind() == kind {
                return Some(n.clone());
            }
            for c in n.children() {
                if let Some(found) = descend(&c, kind) {
                    return Some(found);
                }
            }
            None
        }
        descend(root, kind).expect("kind not found")
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

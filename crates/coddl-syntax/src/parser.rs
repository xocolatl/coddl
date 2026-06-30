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

    /// A type expression. Either a single named type (`Integer`,
    /// `Customer`) or the generator application `Sequence <type-ref>`,
    /// which nests an element `TYPE_REF` (e.g. `Sequence Integer`,
    /// `Sequence Sequence Text`). `Tuple H` / `Relation H` and qualified
    /// names land alongside the rest of expression parsing.
    pub(crate) fn parse_type_ref(&mut self) {
        self.bump_trivia();
        self.start_node(SyntaxKind::TYPE_REF);
        if self.at_keyword("Sequence") {
            // `Sequence <type-ref>`: the element type is a nested
            // TYPE_REF. A missing element type surfaces as P0011 from
            // the recursive call.
            self.bump(); // `Sequence`
            self.parse_type_ref();
        } else if !self.eat(SyntaxKind::IDENT) {
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

    /// `truncate <relvar> ;` — clear every tuple from a relvar. The operand
    /// is parsed permissively (`parse_expr`); the typechecker restricts it to
    /// a bare assignable relvar name (T0033) and the lowerer desugars it to
    /// `R := R minus R` (the surgical whole-table delete shape). `truncate` is
    /// a contextual keyword recognized only at statement-leading position (the
    /// `let` precedent) — it stays a usable identifier everywhere else.
    fn parse_truncate_stmt(&mut self) {
        debug_assert!(self.at_keyword("truncate"));
        self.start_node(SyntaxKind::TRUNCATE_STMT);
        self.bump(); // `truncate`
        let before = self.pos;
        self.parse_expr();
        if self.pos == before {
            self.error("P0014", "expected relvar name after `truncate`");
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `truncate`");
        }
        self.finish_node();
    }

    /// `delete <relvar> where <p> ;` — remove the matching tuples from a relvar.
    /// The operand is parsed permissively (`parse_expr`, which consumes the
    /// `where`); the typechecker restricts it to a `where`-restriction over a
    /// bare assignable relvar (T0033) with a *mandatory* predicate — a bare
    /// `delete R;` is T0052 (use `truncate`). It desugars to the relational
    /// assignment `R := R minus (R where p)` (the `DELETE … WHERE p` shape).
    /// `delete` is a contextual keyword recognized only at statement-leading
    /// position (the `let` precedent).
    fn parse_delete_stmt(&mut self) {
        debug_assert!(self.at_keyword("delete"));
        self.start_node(SyntaxKind::DELETE_STMT);
        self.bump(); // `delete`
        let before = self.pos;
        self.parse_expr();
        if self.pos == before {
            self.error("P0014", "expected relvar name after `delete`");
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `delete`");
        }
        self.finish_node();
    }

    /// `insert <relvar> ( <tuple-set> | <expr> ) ;` — add tuples to a relvar.
    /// After the target name, a `{` starts a brace **tuple-set**
    /// (`{ {…}, {…} }`, parsed as a keyword-less relation literal); anything
    /// else is a relation **expression** source (`insert R Priv`). Both forms
    /// expose a single relation `source`, so `insert R { … }` and `insert R e`
    /// desugar identically to `R := R union <source>` (the idempotent INSERT
    /// shape). `insert` is a contextual keyword (the `let` precedent).
    fn parse_insert_stmt(&mut self) {
        debug_assert!(self.at_keyword("insert"));
        self.start_node(SyntaxKind::INSERT_STMT);
        self.bump(); // `insert`

        // Target relvar name (a bare `NAME_REF`).
        if self.at(SyntaxKind::IDENT) {
            self.bump_trivia();
            self.start_node(SyntaxKind::NAME_REF);
            self.bump();
            self.finish_node();
        } else {
            self.error("P0014", "expected relvar name after `insert`");
        }

        // Source: `{` → brace tuple-set; otherwise a relation expression.
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_tuple_set();
        } else {
            let before = self.pos;
            self.parse_expr();
            if self.pos == before {
                self.error("P0014", "expected a relation or `{ … }` tuple-set to insert");
            }
        }

        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `insert`");
        }
        self.finish_node();
    }

    /// `{ <tuple-lit> , … }` — a brace tuple-set, the keyword-less spelling of a
    /// relation literal (the body is identical to `parse_relation_lit`'s, and it
    /// builds the same `RELATION_LIT` node so the checker/lowerer treat it as a
    /// relation source uniformly). Reuses the relation-literal tuple-body codes
    /// (P0032 / P0033). An empty `{}` yields a zero-tuple relation literal (the
    /// typechecker rejects it, T0018).
    fn parse_tuple_set(&mut self) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.bump_trivia();
        self.start_node(SyntaxKind::RELATION_LIT);
        self.bump(); // {

        if self.eat(SyntaxKind::R_BRACE) {
            self.finish_node();
            return;
        }

        loop {
            if self.at(SyntaxKind::L_BRACE) {
                self.parse_tuple_lit();
            } else {
                self.error("P0032", "expected `{` to start tuple in relation literal");
                break;
            }
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            // Trailing comma: `{ {a:1}, }` is the same as `{ {a:1} }`.
            if self.at(SyntaxKind::R_BRACE) {
                break;
            }
        }

        if !self.eat(SyntaxKind::R_BRACE) {
            self.error("P0033", "expected `}` to close relation literal");
        }
        self.finish_node();
    }

    /// `update <relvar> [ where <p> ] { c: e, … } ;` — overwrite named
    /// attributes of the matching tuples. The operand (`R` or `R where p`) is
    /// parsed with brace-call suppressed (`parse_expr_prec(_, false)`) so the
    /// trailing `{ … }` is the update clause, not a `CALL_EXPR` on the operand;
    /// a brace-call *inside* the predicate must be parenthesized. The clause is
    /// `parse_arg_list(false)` (colon required, no shorthand — same as
    /// `replace`). It desugars to `R := (R where ¬p) union ((R where p) «sub»)`
    /// (the `UPDATE … SET … WHERE p` shape), or a bare substitute for
    /// update-all. `update` is a contextual keyword (the `let` precedent).
    fn parse_update_stmt(&mut self) {
        debug_assert!(self.at_keyword("update"));
        self.start_node(SyntaxKind::UPDATE_STMT);
        self.bump(); // `update`

        // Operand: a bare relvar `R` or `R where p`, brace-call suppressed so it
        // stops at the clause `{`.
        let before = self.pos;
        self.parse_expr_prec(0, false);
        if self.pos == before {
            self.error("P0014", "expected relvar name after `update`");
        }

        // The `{ c: e, … }` clause.
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_arg_list(false);
        } else {
            self.error("P0054", "expected `{ … }` clause after the `update` target");
        }

        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `update`");
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
        if self.at_keyword("truncate") {
            self.parse_truncate_stmt();
            return;
        }
        if self.at_keyword("delete") {
            self.parse_delete_stmt();
            return;
        }
        if self.at_keyword("insert") {
            self.parse_insert_stmt();
            return;
        }
        if self.at_keyword("update") {
            self.parse_update_stmt();
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

        // Relational assignment: the parsed expression is the target and the
        // next token is `:=`. The LHS is parser-permissive (any expression);
        // the typechecker restricts it to a private-relvar name (T0033).
        // Retroactively wrap the target expression under ASSIGN_STMT.
        if self.at(SyntaxKind::ASSIGN) {
            self.start_node_at(cp, SyntaxKind::ASSIGN_STMT);
            self.bump(); // `:=`
            let before_rhs = self.pos;
            self.parse_expr();
            if self.pos == before_rhs {
                self.error("P0014", "expected expression after `:=`");
            }
            if !self.eat(SyntaxKind::SEMICOLON) {
                self.error("P0013", "expected `;` after assignment");
            }
            self.finish_node();
            return;
        }

        // Expression statement: wrap in EXPR_STMT, expect `;`.
        self.start_node_at(cp, SyntaxKind::EXPR_STMT);
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after expression");
        }
        self.finish_node();
    }

    /// An expression. Driven by a precedence-climbing Pratt walker:
    /// primary → postfix loop (call, field access) → infix loop
    /// (arithmetic, comparison, logical, `where`). Postfix binds tighter
    /// than every infix operator; among infix operators, the precedence
    /// ladder from lowest to highest is `where`(0) < `or`(1) < `and`(2)
    /// < comparison(3) < additive `+`/`-`/`||`(4) < multiplicative
    /// `*`/`/`(5). All infix operators are left-associative.
    fn parse_expr(&mut self) {
        self.parse_expr_prec(0, true);
    }

    /// Parse an expression, only consuming operators whose precedence
    /// is `>= min_prec`. The caller picks `min_prec` to control how
    /// far an operator's right operand is allowed to extend.
    ///
    /// `allow_brace_call` gates the postfix brace-call `name { args }`. It is
    /// `true` everywhere except a statement operand that is itself followed by a
    /// brace clause — `update R { c: e }` parses the operand with it `false` so
    /// the trailing `{ c: e }` stays the update clause rather than a call on
    /// `R`. It propagates down the infix chain (so `R where x = 5 { … }`
    /// suppresses the brace on `5`) but resets to `true` inside parentheses,
    /// which is the escape hatch for a brace-call in a suppressed predicate.
    fn parse_expr_prec(&mut self, min_prec: u8, allow_brace_call: bool) {
        // Flush any leading trivia into the parent node before the
        // checkpoint — otherwise a retroactive `start_node_at(cp, …)`
        // for CALL_EXPR / FIELD_ACCESS / BINARY_EXPR would wrap the
        // trivia inside the expression.
        self.bump_trivia();
        let cp = self.checkpoint();
        if !self.parse_primary_expr() {
            return;
        }

        // Postfix loop — these bind tighter than any infix op and
        // are independent of precedence.
        loop {
            match self.current() {
                SyntaxKind::L_BRACE if allow_brace_call => {
                    self.start_node_at(cp, SyntaxKind::CALL_EXPR);
                    self.parse_arg_list(true); // operator calls allow field-init shorthand
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

        // Pipeline loop. Each turn applies whichever relational/operator
        // form sits at the cursor at this precedence:
        //
        //   - `project { … }` — a postfix operator at pipeline precedence
        //     (the same altitude as `where`). It is gated to the top level
        //     (`min_prec == 0`) so it binds to the whole pipeline, never to
        //     a higher-precedence operand such as a `where` predicate.
        //     Left-associative, and interleaves with `where` in either
        //     order: `R where p project {a}` and `R project {a} where p`
        //     both nest left.
        //   - an infix operator — wrap the lhs in a BINARY_EXPR, bump the
        //     operator token (or keyword IDENT), and recurse for the rhs at
        //     `prec + 1` (left-associative). If its precedence is below
        //     `min_prec`, return and let the caller handle it.
        loop {
            if min_prec == 0 && self.at_keyword("project") {
                self.start_node_at(cp, SyntaxKind::PROJECT_EXPR);
                self.parse_project_suffix();
                self.finish_node();
                continue;
            }
            if min_prec == 0 && self.at_keyword("replace") {
                self.start_node_at(cp, SyntaxKind::REPLACE_EXPR);
                self.parse_replace_suffix();
                self.finish_node();
                continue;
            }
            if min_prec == 0 && self.at_keyword("tclose") {
                self.start_node_at(cp, SyntaxKind::TCLOSE_EXPR);
                self.parse_tclose_suffix();
                self.finish_node();
                continue;
            }
            if min_prec == 0 && self.at_keyword("extend") {
                self.start_node_at(cp, SyntaxKind::EXTEND_EXPR);
                self.parse_extend_suffix();
                self.finish_node();
                continue;
            }
            if min_prec == 0 && self.at_keyword("rename") {
                self.start_node_at(cp, SyntaxKind::RENAME_EXPR);
                self.parse_rename_suffix();
                self.finish_node();
                continue;
            }
            if min_prec == 0 && self.at_keyword("wrap") {
                self.start_node_at(cp, SyntaxKind::WRAP_EXPR);
                self.parse_wrap_suffix();
                self.finish_node();
                continue;
            }
            if min_prec == 0 && self.at_keyword("unwrap") {
                self.start_node_at(cp, SyntaxKind::UNWRAP_EXPR);
                self.parse_unwrap_suffix();
                self.finish_node();
                continue;
            }
            let Some(prec) = self.peek_infix_prec() else {
                break;
            };
            if prec < min_prec {
                break;
            }
            self.start_node_at(cp, SyntaxKind::BINARY_EXPR);
            self.bump_trivia();
            self.bump(); // operator token or keyword IDENT
            // Missing-rhs (e.g. `1 = ;`) surfaces as P0014 from the
            // inner `parse_primary_expr` — no dedicated code needed.
            self.parse_expr_prec(prec + 1, allow_brace_call);
            self.finish_node();
        }
    }

    /// Peek the next infix operator's precedence. Returns `None` if
    /// the cursor isn't on a recognized infix operator. Operators
    /// recognized by token kind: `*`, `/` (5); `+`, `-`, `||` (4);
    /// `=`, `<>`, `<`, `>`, `<=`, `>=` (all at prec 3). Operators
    /// recognized by contextual-keyword IDENT: `and` (2), `or` (1),
    /// `where` (0) (and the relational ops, also at 0).
    fn peek_infix_prec(&self) -> Option<u8> {
        match self.current() {
            // Multiplicative — binds tightest among infix ops.
            SyntaxKind::STAR | SyntaxKind::SLASH => Some(5),
            // Additive, plus text/character concatenation (`||`). Disjoint
            // operand types, so `||`'s rank among level-4 ops is immaterial.
            SyntaxKind::PLUS | SyntaxKind::MINUS | SyntaxKind::PIPE_PIPE => Some(4),
            SyntaxKind::EQ
            | SyntaxKind::NOT_EQ
            | SyntaxKind::LT
            | SyntaxKind::GT
            | SyntaxKind::LT_EQ
            | SyntaxKind::GT_EQ => Some(3),
            SyntaxKind::IDENT if self.at_keyword("and") => Some(2),
            SyntaxKind::IDENT if self.at_keyword("or") => Some(1),
            SyntaxKind::IDENT if self.at_keyword("where") => Some(0),
            SyntaxKind::IDENT if self.at_keyword("join") => Some(0),
            SyntaxKind::IDENT if self.at_keyword("times") => Some(0),
            SyntaxKind::IDENT if self.at_keyword("compose") => Some(0),
            SyntaxKind::IDENT if self.at_keyword("intersect") => Some(0),
            SyntaxKind::IDENT if self.at_keyword("union") => Some(0),
            SyntaxKind::IDENT if self.at_keyword("minus") => Some(0),
            _ => None,
        }
    }

    /// A primary expression — the atomic forms an expression can start
    /// with. Returns `true` if anything was consumed.
    fn parse_primary_expr(&mut self) -> bool {
        if self.at_keyword("transaction") {
            self.parse_transaction_expr();
            return true;
        }
        if self.at_keyword("Relation") {
            self.parse_relation_lit();
            return true;
        }
        if self.at_keyword("Sequence") {
            self.parse_sequence_lit();
            return true;
        }
        // `extract <expr>` — prefix-position unary form. Recognized
        // before the generic IDENT branch so the AST gets a
        // distinct `UNARY_EXPR` node. The operand parses at full
        // expression precedence so `extract R where p` reads as
        // `extract (R where p)` without parens.
        if self.at_keyword("extract") {
            self.parse_extract_expr();
            return true;
        }
        // `true` / `false` — contextual-keyword Boolean literal.
        // Recognized before the generic IDENT branch so the AST gets
        // a distinct `BOOL_LITERAL` node rather than a NAME_REF.
        if self.at_keyword("true") || self.at_keyword("false") {
            self.bump_trivia();
            self.start_node(SyntaxKind::BOOL_LITERAL);
            self.bump(); // IDENT
            self.finish_node();
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
            | SyntaxKind::FORMAT_STRING_LIT
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
            SyntaxKind::L_PAREN => {
                // Parenthesized expression — pure grouping. Wraps in
                // a PAREN_EXPR node; the AST view transparently
                // unwraps it so the typechecker/lowerer never see
                // the wrapper.
                self.bump_trivia();
                self.start_node(SyntaxKind::PAREN_EXPR);
                self.bump(); // `(`
                self.parse_expr_prec(0, true);
                if !self.eat(SyntaxKind::R_PAREN) {
                    self.error("P0035", "expected `)` to close parenthesized expression");
                }
                self.finish_node();
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
            self.parse_named_arg(true);
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

    /// `Relation { <tuple-lit>, <tuple-lit>, … }` in expression
    /// position. The body is a comma-separated list of tuple literals
    /// (each of which is itself `{ name: value, … }`). Empty
    /// `Relation {}` parses cleanly here; the typechecker emits T0018
    /// since there's no inference context for the heading.
    fn parse_relation_lit(&mut self) {
        debug_assert!(self.at_keyword("Relation"));
        self.bump_trivia();
        self.start_node(SyntaxKind::RELATION_LIT);
        self.bump(); // `Relation`

        if !self.at(SyntaxKind::L_BRACE) {
            self.error("P0031", "expected `{` after `Relation`");
            self.finish_node();
            return;
        }
        self.bump(); // {

        if self.eat(SyntaxKind::R_BRACE) {
            self.finish_node();
            return;
        }

        loop {
            if self.at(SyntaxKind::L_BRACE) {
                self.parse_tuple_lit();
            } else {
                self.error(
                    "P0032",
                    "expected `{` to start tuple in relation literal",
                );
                break;
            }
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            // Trailing comma: `Relation { {a:1}, }` is the same as
            // `Relation { {a:1} }`.
            if self.at(SyntaxKind::R_BRACE) {
                break;
            }
        }

        if !self.eat(SyntaxKind::R_BRACE) {
            self.error("P0033", "expected `}` to close relation literal");
        }
        self.finish_node();
    }

    /// `Sequence [ <expr>, <expr>, … ]` in expression position — a
    /// sequence literal, the ordered generator-prefixed counterpart to
    /// `Relation { … }`. Elements are arbitrary expressions; an empty
    /// `Sequence []` parses cleanly (the typechecker resolves its element
    /// type from a `let` annotation, else T0061). Trailing comma is
    /// accepted. This is *syntactically* a primary expression; the
    /// typechecker restricts it to `let`-binding values (T0063).
    fn parse_sequence_lit(&mut self) {
        debug_assert!(self.at_keyword("Sequence"));
        self.bump_trivia();
        self.start_node(SyntaxKind::SEQUENCE_LIT);
        self.bump(); // `Sequence`

        if !self.at(SyntaxKind::L_BRACKET) {
            self.error("P0055", "expected `[` after `Sequence`");
            self.finish_node();
            return;
        }
        self.bump(); // [

        if self.eat(SyntaxKind::R_BRACKET) {
            self.finish_node();
            return;
        }

        loop {
            let before = self.pos;
            self.parse_expr();
            // No progress (garbage element) — bail rather than spin;
            // recovery happens at the enclosing statement anchor.
            if self.pos == before {
                break;
            }
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            // Trailing comma: `Sequence [ 1, ]` is the same as `Sequence [ 1 ]`.
            if self.at(SyntaxKind::R_BRACKET) {
                break;
            }
        }

        if !self.eat(SyntaxKind::R_BRACKET) {
            self.error("P0056", "expected `]` to close sequence literal");
        }
        self.finish_node();
    }

    /// `extract <expr>` — the TTM RM Pre 10 primitive: collapse a
    /// single-row relation to a tuple. Parsed as a `UNARY_EXPR`
    /// containing one operand (typechecked to be a relation). The
    /// operand parses at full expression precedence so `extract R
    /// where p` reads as `extract (R where p)` — the canonical
    /// idiom.
    fn parse_extract_expr(&mut self) {
        debug_assert!(self.at_keyword("extract"));
        self.bump_trivia();
        self.start_node(SyntaxKind::UNARY_EXPR);
        self.bump(); // `extract`
        // Operand parses at the lowest precedence so `where`, `and`,
        // `or`, comparisons all bind inside. Missing operand surfaces
        // as P0014 from the inner `parse_primary_expr`.
        self.parse_expr_prec(0, true);
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
    fn parse_arg_list(&mut self, allow_shorthand: bool) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.start_node(SyntaxKind::ARG_LIST);
        self.bump(); // {

        if self.eat(SyntaxKind::R_BRACE) {
            self.finish_node();
            return;
        }

        loop {
            self.parse_named_arg(allow_shorthand);
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

    /// `<name>: <expr>` inside an argument list, or — when `allow_shorthand`
    /// — the field-init shorthand bare `<name>` (≡ `<name>: <name>`, the
    /// value is the same-named binding in scope). The shorthand wraps the
    /// just-read `IDENT` in a `NAME_REF` (retroactive `start_node_at`) so the
    /// AST's `value()` view yields a name-ref and every consumer sees the
    /// explicit form; no tokens are synthesized, so the CST stays
    /// byte-lossless. `allow_shorthand` is `false` in replace position, where
    /// the colon stays required (a shorthand `replace { x }` would bind the new
    /// attribute `x` to itself — the no-op identity `x -> x`).
    fn parse_named_arg(&mut self, allow_shorthand: bool) {
        self.bump_trivia();
        self.start_node(SyntaxKind::NAMED_ARG);
        let cp = self.checkpoint();
        let has_name = self.eat(SyntaxKind::IDENT);
        if !has_name {
            self.error("P0016", "expected argument name");
        }
        if self.at(SyntaxKind::COLON) {
            self.bump(); // `:`
            self.parse_expr();
        } else if allow_shorthand && has_name {
            // Field-init shorthand: wrap the name in a `NAME_REF` so it reads
            // as the value `<name>`.
            self.start_node_at(cp, SyntaxKind::NAME_REF);
            self.finish_node();
        } else {
            self.error("P0017", "expected `:` after argument name");
            self.parse_expr();
        }
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
        self.parse_ident_brace_list(
            ("P0022", "expected `{` to start key clause"),
            ("P0023", "expected key attribute name"),
            ("P0024", "expected `}` to close key clause"),
        );
        self.finish_node();
    }

    /// `project [ all but ] { a, b, … }` — relational projection suffix. The
    /// enclosing `PROJECT_EXPR` node (which wraps the relation operand) is
    /// opened by the caller in the pipeline loop, so this only consumes the
    /// `project` keyword, an optional `all but` prefix, and the attribute list.
    /// Plain `project { … }` keeps the named attributes; `project all but { … }`
    /// removes them and keeps the complement.
    ///
    /// Diagnostics: P0036 (no `{`), P0037 (no attribute name), P0038
    /// (no `}`), P0039 (`all` not followed by `but`).
    pub(crate) fn parse_project_suffix(&mut self) {
        debug_assert!(self.at_keyword("project"));
        self.bump_trivia();
        self.bump(); // `project`
        // Optional `all but` prefix — project *away* the named attributes
        // (keep the complement). `all`/`but` are contextual keywords: in this
        // position before the `{` they're the operator; inside the braces, or
        // anywhere else, they remain ordinary identifiers.
        if self.at_keyword("all") {
            self.bump(); // `all`
            if self.at_keyword("but") {
                self.bump(); // `but`
            } else {
                self.error("P0039", "expected `but` after `all` in project");
            }
        }
        self.parse_ident_brace_list(
            ("P0036", "expected `{` to start project list"),
            ("P0037", "expected project attribute name"),
            ("P0038", "expected `}` to close project list"),
        );
    }

    /// `replace { new: e, … }` — relational replace suffix. The enclosing
    /// `REPLACE_EXPR` node (wrapping the operand) is opened by the caller, so
    /// this consumes the `replace` keyword and the `{ new: e }` pair list. Each
    /// pair binds a new attribute name (left of the colon) to a value
    /// expression (right). The pairs reuse the `ARG_LIST` / `NAMED_ARG`
    /// production, so the value parses as a general `Expr`; the typechecker
    /// requires each value to compute (read ≥1 attribute) — a bare attribute
    /// reference is a pure relabel and belongs to `rename` (T0047).
    ///
    /// Diagnostics: P0040 (no `{`); the pair-list reuses the arg-list codes
    /// P0015 (no `}`), P0016 (no name), P0017 (no `:`).
    pub(crate) fn parse_replace_suffix(&mut self) {
        debug_assert!(self.at_keyword("replace"));
        self.bump_trivia();
        self.bump(); // `replace`
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_arg_list(false); // replace keeps the colon required (no shorthand)
        } else {
            self.error("P0040", "expected `{` to start replace list");
        }
    }

    /// `rename { new: old, … }` — relational rename suffix. The enclosing
    /// `RENAME_EXPR` node (wrapping the operand) is opened by the caller, so
    /// this consumes the `rename` keyword and the `{ new: old }` pair list. Each
    /// pair binds a new attribute name (left of the colon) to a source attribute
    /// (right). The pairs reuse the `ARG_LIST` / `NAMED_ARG` production, so the
    /// value parses as a general `Expr`; the typechecker requires each value to
    /// be a bare attribute reference — a computed value belongs to `replace`
    /// (T0030). The strict relabel-only partition of `replace`.
    ///
    /// Diagnostics: P0034 (no `{`); the pair-list reuses the arg-list codes
    /// P0015 (no `}`), P0016 (no name), P0017 (no `:`).
    pub(crate) fn parse_rename_suffix(&mut self) {
        debug_assert!(self.at_keyword("rename"));
        self.bump_trivia();
        self.bump(); // `rename`
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_arg_list(false); // rename keeps the colon required (no shorthand)
        } else {
            self.error("P0034", "expected `{` to start rename list");
        }
    }

    /// `wrap { t: { a, b }, … }` — relational wrap suffix. The enclosing
    /// `WRAP_EXPR` node (wrapping the operand) is opened by the caller, so this
    /// consumes the `wrap` keyword and the `{ new: { idents } }` pair list. Each
    /// pair is a `WRAP_PAIR` node: the new tuple-valued attribute name, a colon,
    /// then an unordered brace-list of existing attribute names (NOT an
    /// expression — wrap groups attributes, it does not compute).
    ///
    /// Diagnostics: P0044 (no outer `{`), P0050 (no outer `}`); per pair P0045
    /// (no new name), P0046 (no `:`); the inner brace-list emits P0047 (no `{`),
    /// P0048 (no attribute name), P0049 (no `}`).
    pub(crate) fn parse_wrap_suffix(&mut self) {
        debug_assert!(self.at_keyword("wrap"));
        self.bump_trivia();
        self.bump(); // `wrap`
        if !self.eat(SyntaxKind::L_BRACE) {
            self.error("P0044", "expected `{` to start wrap list");
            return;
        }
        if self.eat(SyntaxKind::R_BRACE) {
            return;
        }
        loop {
            self.bump_trivia();
            self.start_node(SyntaxKind::WRAP_PAIR);
            if !self.eat(SyntaxKind::IDENT) {
                self.error("P0045", "expected new attribute name in wrap");
            }
            if !self.eat(SyntaxKind::COLON) {
                self.error("P0046", "expected `:` after wrap attribute name");
            }
            self.parse_ident_brace_list(
                ("P0047", "expected `{` to start wrapped-attribute list"),
                ("P0048", "expected attribute name in wrapped-attribute list"),
                ("P0049", "expected `}` to close wrapped-attribute list"),
            );
            self.finish_node();
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
            // Trailing comma ok.
            if self.at(SyntaxKind::R_BRACE) {
                break;
            }
        }
        if !self.eat(SyntaxKind::R_BRACE) {
            self.error("P0050", "expected `}` to close wrap list");
        }
    }

    /// `unwrap { t, … }` — relational unwrap suffix. The enclosing
    /// `UNWRAP_EXPR` node (wrapping the operand) is opened by the caller, so
    /// this consumes the `unwrap` keyword and the unordered brace-list of
    /// tuple-valued attribute names to expand. Reuses `parse_ident_brace_list`
    /// (the same shape as `project`).
    ///
    /// Diagnostics: P0051 (no `{`), P0052 (no attribute name), P0053 (no `}`).
    pub(crate) fn parse_unwrap_suffix(&mut self) {
        debug_assert!(self.at_keyword("unwrap"));
        self.bump_trivia();
        self.bump(); // `unwrap`
        self.parse_ident_brace_list(
            ("P0051", "expected `{` to start unwrap list"),
            ("P0052", "expected attribute name in unwrap list"),
            ("P0053", "expected `}` to close unwrap list"),
        );
    }

    /// `extend { new: e, … }` — relational extend suffix. The enclosing
    /// `EXTEND_EXPR` node (wrapping the operand) is opened by the caller, so
    /// this consumes the `extend` keyword and the `{ new: e }` pair list. Each
    /// pair binds a new attribute name (left of the colon) to a computed value
    /// expression (right); `extend` adds it without removing anything.
    ///
    /// Mirrors `parse_replace_suffix`: the pairs reuse the `ARG_LIST` /
    /// `NAMED_ARG` production with field-init shorthand DISABLED (the colon is
    /// required — a shorthand `extend { x }` would be the no-op `x: x`).
    /// Diagnostics: P0043 (no `{`); the pair-list reuses P0015/P0016/P0017.
    pub(crate) fn parse_extend_suffix(&mut self) {
        debug_assert!(self.at_keyword("extend"));
        self.bump_trivia();
        self.bump(); // `extend`
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_arg_list(false); // colon required (no shorthand)
        } else {
            self.error("P0043", "expected `{` to start extend list");
        }
    }

    /// `tclose [ '{' <ident> { ',' <ident> } '}' ]` — relational transitive
    /// closure suffix. The enclosing `TCLOSE_EXPR` node (wrapping the operand)
    /// is opened by the caller in the pipeline loop, so this consumes the
    /// `tclose` keyword and the *optional* unordered two-attribute brace-list.
    /// The braces are sugar for `(R project { a, b }) tclose`; the bare form
    /// `R tclose` requires the operand to already be a binary relation.
    ///
    /// Unlike `key`/`project`, the brace-list is optional, so this does not
    /// reuse `parse_ident_brace_list` (which makes the braces mandatory): the
    /// bare form is not an error. When the braces *are* present, P0041 reports
    /// a missing attribute name and P0042 a missing close `}`.
    pub(crate) fn parse_tclose_suffix(&mut self) {
        debug_assert!(self.at_keyword("tclose"));
        self.bump_trivia();
        self.bump(); // `tclose`
        // Optional `{ a, b }` brace-list. Absent → bare form, no error.
        if !self.at(SyntaxKind::L_BRACE) {
            return;
        }
        self.bump(); // {
        if self.eat(SyntaxKind::R_BRACE) {
            return; // empty `{}` parses; the typechecker rejects non-binary (T0041)
        }
        loop {
            self.bump_trivia();
            if !self.eat(SyntaxKind::IDENT) {
                self.error("P0041", "expected attribute name in tclose list");
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
            self.error("P0042", "expected `}` to close tclose list");
        }
    }

    /// Parse a bare-identifier brace list `{ a, b, … }` (trailing comma
    /// permitted) into the current node. Shared by `key { … }` clauses and
    /// `project { … }` suffixes — structurally identical productions that
    /// differ only in which diagnostic codes/messages they report, passed
    /// as `(code, message)` pairs for the missing-`{`, missing-name, and
    /// missing-`}` cases.
    fn parse_ident_brace_list(
        &mut self,
        open: (&'static str, &'static str),
        ident: (&'static str, &'static str),
        close: (&'static str, &'static str),
    ) {
        if !self.eat(SyntaxKind::L_BRACE) {
            self.error(open.0, open.1);
            return;
        }

        if self.eat(SyntaxKind::R_BRACE) {
            return;
        }

        loop {
            self.bump_trivia();
            if !self.eat(SyntaxKind::IDENT) {
                self.error(ident.0, ident.1);
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
            self.error(close.0, close.1);
        }
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
    fn format_string_literal_expr_stmt() {
        let out = parse_str("oper f {} [ f\"hi {x}\"; ];");
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
        assert_eq!(lit.text(), "f\"hi {x}\"");
        assert_eq!(
            lit.first_token().unwrap().kind(),
            SyntaxKind::FORMAT_STRING_LIT
        );
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
    fn assign_stmt_parses() {
        let out = parse_str("oper main {} [ R := S; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), "oper main {} [ R := S; ];");
        let assign = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::ASSIGN_STMT)
            .expect("ASSIGN_STMT in tree");
        // Target then value, both NAME_REF nodes, in order.
        let names: Vec<_> = assign
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
    }

    #[test]
    fn assign_stmt_rhs_relation_literal_parses() {
        let out = parse_str("oper main {} [ R := Relation { {a: 1} }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let assign = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::ASSIGN_STMT)
            .expect("ASSIGN_STMT in tree");
        let child_kinds: Vec<_> = assign.children().map(|n| n.kind()).collect();
        assert!(
            child_kinds.contains(&SyntaxKind::NAME_REF),
            "target NAME_REF in {child_kinds:?}"
        );
        assert!(
            child_kinds.contains(&SyntaxKind::RELATION_LIT),
            "RHS RELATION_LIT in {child_kinds:?}"
        );
    }

    #[test]
    fn assign_stmt_missing_rhs_diagnoses_p0014() {
        let out = parse_str("oper main {} [ R := ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0014"),
            "expected P0014, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn assign_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper main {} [ R := S ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn truncate_stmt_parses() {
        let out = parse_str("oper main {} [ truncate R; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), "oper main {} [ truncate R; ];");
        let truncate = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TRUNCATE_STMT)
            .expect("TRUNCATE_STMT in tree");
        // The operand is a single NAME_REF child.
        let names: Vec<_> = truncate
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string()]);
    }

    #[test]
    fn truncate_stmt_missing_operand_diagnoses_p0014() {
        let out = parse_str("oper main {} [ truncate ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0014"),
            "expected P0014, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn truncate_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper main {} [ truncate R ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn truncate_remains_a_usable_identifier() {
        // `truncate` is a contextual keyword only at statement-leading position;
        // as an attribute name (or anywhere else) it stays an ordinary IDENT.
        let out = parse_str("oper main {} [ let _t = Relation { {truncate: 1} }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            out.tree
                .descendants()
                .all(|n| n.kind() != SyntaxKind::TRUNCATE_STMT),
            "`truncate` as an attribute name must not parse as TRUNCATE_STMT"
        );
    }

    #[test]
    fn delete_stmt_parses() {
        let out = parse_str("oper main {} [ delete R where a = 1; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), "oper main {} [ delete R where a = 1; ];");
        let delete = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::DELETE_STMT)
            .expect("DELETE_STMT in tree");
        // The operand is a single `where` BINARY_EXPR child.
        let kinds: Vec<_> = delete.children().map(|n| n.kind()).collect();
        assert!(
            kinds.contains(&SyntaxKind::BINARY_EXPR),
            "operand BINARY_EXPR in {kinds:?}"
        );
    }

    #[test]
    fn delete_stmt_missing_operand_diagnoses_p0014() {
        let out = parse_str("oper main {} [ delete ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0014"),
            "expected P0014, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn delete_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper main {} [ delete R where a = 1 ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn delete_remains_a_usable_identifier() {
        // `delete` is a contextual keyword only at statement-leading position.
        let out = parse_str("oper main {} [ let _t = Relation { {delete: 1} }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            out.tree
                .descendants()
                .all(|n| n.kind() != SyntaxKind::DELETE_STMT),
            "`delete` as an attribute name must not parse as DELETE_STMT"
        );
    }

    #[test]
    fn insert_stmt_tuple_set_parses() {
        let out = parse_str("oper main {} [ insert R { {a: 1}, {a: 2} }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), "oper main {} [ insert R { {a: 1}, {a: 2} }; ];");
        let insert = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::INSERT_STMT)
            .expect("INSERT_STMT in tree");
        // Target NAME_REF + the tuple-set as a (keyword-less) RELATION_LIT.
        let kinds: Vec<_> = insert.children().map(|n| n.kind()).collect();
        assert!(kinds.contains(&SyntaxKind::NAME_REF), "target NAME_REF in {kinds:?}");
        assert!(
            kinds.contains(&SyntaxKind::RELATION_LIT),
            "tuple-set RELATION_LIT in {kinds:?}"
        );
        // The tuple-set has two tuple children and no `Relation` keyword token.
        let rel = insert
            .children()
            .find(|n| n.kind() == SyntaxKind::RELATION_LIT)
            .unwrap();
        assert_eq!(
            rel.children().filter(|n| n.kind() == SyntaxKind::TUPLE_LIT).count(),
            2
        );
        assert!(!rel.text().to_string().contains("Relation"));
    }

    #[test]
    fn insert_stmt_relexpr_parses() {
        let out = parse_str("oper main {} [ insert R S; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let insert = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::INSERT_STMT)
            .expect("INSERT_STMT in tree");
        // Target then source, both NAME_REF nodes, in order.
        let names: Vec<_> = insert
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
    }

    #[test]
    fn insert_stmt_missing_source_diagnoses_p0014() {
        let out = parse_str("oper main {} [ insert R ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0014"),
            "expected P0014, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn insert_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper main {} [ insert R S ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn insert_remains_a_usable_identifier() {
        // `insert` is a contextual keyword only at statement-leading position.
        let out = parse_str("oper main {} [ let _t = Relation { {insert: 1} }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            out.tree
                .descendants()
                .all(|n| n.kind() != SyntaxKind::INSERT_STMT),
            "`insert` as an attribute name must not parse as INSERT_STMT"
        );
    }

    #[test]
    fn update_stmt_with_where_parses() {
        let out = parse_str("oper main {} [ update R where a = 1 { b: 2 }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), "oper main {} [ update R where a = 1 { b: 2 }; ];");
        let update = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UPDATE_STMT)
            .expect("UPDATE_STMT in tree");
        // Operand is the `where` BINARY_EXPR; clause is a separate ARG_LIST — the
        // brace was NOT swallowed into the predicate.
        let kinds: Vec<_> = update.children().map(|n| n.kind()).collect();
        assert!(kinds.contains(&SyntaxKind::BINARY_EXPR), "operand BINARY_EXPR in {kinds:?}");
        assert!(kinds.contains(&SyntaxKind::ARG_LIST), "clause ARG_LIST in {kinds:?}");
    }

    #[test]
    fn update_stmt_all_parses_operand_is_not_a_call() {
        // The key boundary: `update R { b: 2 }` must parse `R` as the operand
        // (a NAME_REF) and `{ b: 2 }` as the clause (ARG_LIST) — NOT as a
        // brace-call `R { b: 2 }` (CALL_EXPR).
        let out = parse_str("oper main {} [ update R { b: 2 }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let update = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UPDATE_STMT)
            .expect("UPDATE_STMT in tree");
        let kinds: Vec<_> = update.children().map(|n| n.kind()).collect();
        assert!(kinds.contains(&SyntaxKind::NAME_REF), "operand NAME_REF in {kinds:?}");
        assert!(kinds.contains(&SyntaxKind::ARG_LIST), "clause ARG_LIST in {kinds:?}");
        assert!(
            !kinds.contains(&SyntaxKind::CALL_EXPR),
            "operand must not be a brace-call: {kinds:?}"
        );
    }

    #[test]
    fn update_predicate_brace_call_needs_parens() {
        // A brace-call in the predicate is suppressed at the top level but
        // re-enabled inside parentheses (the escape hatch): the parenthesized
        // `(f { x: 1 })` parses as a CALL_EXPR, and `{ b: 3 }` stays the clause.
        let out = parse_str("oper main {} [ update R where (f { x: 1 }) = 2 { b: 3 }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let update = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UPDATE_STMT)
            .expect("UPDATE_STMT in tree");
        // The clause ARG_LIST is a direct child; the call lives inside the operand.
        assert!(
            update.children().any(|n| n.kind() == SyntaxKind::ARG_LIST),
            "clause ARG_LIST present"
        );
        assert!(
            out.tree.descendants().any(|n| n.kind() == SyntaxKind::CALL_EXPR),
            "parenthesized brace-call parses as CALL_EXPR"
        );
    }

    #[test]
    fn update_stmt_missing_clause_diagnoses_p0054() {
        let out = parse_str("oper main {} [ update R ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0054"),
            "expected P0054, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn update_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper main {} [ update R { b: 2 } ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn update_remains_a_usable_identifier() {
        // `update` is a contextual keyword only at statement-leading position.
        let out = parse_str("oper main {} [ let _t = Relation { {update: 1} }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            out.tree
                .descendants()
                .all(|n| n.kind() != SyntaxKind::UPDATE_STMT),
            "`update` as an attribute name must not parse as UPDATE_STMT"
        );
    }

    #[test]
    fn brace_call_still_parses_outside_suppressed_context() {
        // Regression: the default `allow_brace_call = true` path is unchanged —
        // `f { a: 1 }` in ordinary expression position is still a CALL_EXPR.
        let out = parse_str("oper main {} [ write_relation { rel: R }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            out.tree.descendants().any(|n| n.kind() == SyntaxKind::CALL_EXPR),
            "brace-call still parses as CALL_EXPR"
        );
    }

    #[test]
    fn join_infix_parses() {
        let out = parse_str("oper main {} [ R join S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("BINARY_EXPR for `R join S`");
        let names: Vec<_> = bin
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert!(bin.text().to_string().contains("join"));
    }

    #[test]
    fn times_infix_parses() {
        let out = parse_str("oper main {} [ R times S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("BINARY_EXPR for `R times S`");
        let names: Vec<_> = bin
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert!(bin.text().to_string().contains("times"));
    }

    #[test]
    fn compose_infix_parses() {
        let out = parse_str("oper main {} [ R compose S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("BINARY_EXPR for `R compose S`");
        let names: Vec<_> = bin
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert!(bin.text().to_string().contains("compose"));
    }

    #[test]
    fn intersect_infix_parses() {
        let out = parse_str("oper main {} [ R intersect S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("BINARY_EXPR for `R intersect S`");
        let names: Vec<_> = bin
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert!(bin.text().to_string().contains("intersect"));
    }

    #[test]
    fn union_infix_parses() {
        let out = parse_str("oper main {} [ R union S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("BINARY_EXPR for `R union S`");
        let names: Vec<_> = bin
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert!(bin.text().to_string().contains("union"));
    }

    #[test]
    fn minus_infix_parses() {
        let out = parse_str("oper main {} [ R minus S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("BINARY_EXPR for `R minus S`");
        let names: Vec<_> = bin
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert!(bin.text().to_string().contains("minus"));
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
    fn tuple_literal_field_init_shorthand_parses() {
        // `{a}` ≡ `{a: a}`: the NAMED_ARG has no colon and wraps the name in
        // a NAME_REF (the value view).
        let out = parse_str("oper f {} [ let a = 1; let t = {a}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tuple = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .expect("TUPLE_LIT in tree");
        let fields: Vec<_> = tuple.children().collect();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].kind(), SyntaxKind::NAMED_ARG);
        assert_eq!(fields[0].text(), "a");
        assert!(
            fields[0].children().any(|n| n.kind() == SyntaxKind::NAME_REF),
            "shorthand value should be a NAME_REF"
        );
    }

    #[test]
    fn arg_list_field_init_shorthand_parses() {
        let out = parse_str("oper f {} [ let message = \"hi\"; write_line {message}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let arg_list = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::ARG_LIST)
            .expect("ARG_LIST in tree");
        let args: Vec<_> = arg_list.children().collect();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].kind(), SyntaxKind::NAMED_ARG);
        assert_eq!(args[0].text(), "message");
        assert!(args[0].children().any(|n| n.kind() == SyntaxKind::NAME_REF));
    }

    #[test]
    fn field_init_shorthand_mixes_with_explicit() {
        // `{a, b: 2}` — a shorthand field followed by an explicit one.
        let out = parse_str("oper f {} [ let a = 1; let t = {a, b: 2}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let tuple = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .expect("TUPLE_LIT in tree");
        let fields: Vec<_> = tuple.children().collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].text(), "a"); // shorthand: no colon
        let has_colon = |n: &crate::cst::SyntaxNode| -> bool {
            n.children_with_tokens()
                .filter_map(|e| e.into_token())
                .any(|t| t.kind() == SyntaxKind::COLON)
        };
        assert!(!has_colon(&fields[0]), "shorthand field has no colon");
        assert!(has_colon(&fields[1]), "explicit field has a colon");
    }

    #[test]
    fn field_init_shorthand_round_trips() {
        // Losslessness: the shorthand source is reproduced byte-for-byte from
        // the CST — no synthesized colon/value tokens.
        let src = "oper f {} [ let message = \"hi\"; write_line { message }; ];";
        let out = parse_str(src);
        assert_eq!(out.tree.text(), src);
    }

    #[test]
    fn replace_requires_colon_diagnoses_p0017() {
        // Field-init shorthand is disabled in replace position; `replace { new }`
        // is missing the required `:`.
        let out = parse_str("oper f {} [ let s = r replace { new }; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0017"),
            "expected P0017, got {:?}",
            out.diagnostics
        );
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

    // ── Relation literals (Phase 19) ─────────────────────────────────

    #[test]
    fn empty_relation_literal_parses() {
        let out = parse_str("oper f {} [ let r = Relation {}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let rel = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RELATION_LIT)
            .expect("RELATION_LIT in tree");
        let tuples: Vec<_> = rel
            .children()
            .filter(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .collect();
        assert_eq!(tuples.len(), 0);
        assert_eq!(rel.text(), "Relation {}");
    }

    #[test]
    fn relation_literal_with_tuples_parses() {
        let out = parse_str("oper f {} [ let r = Relation { {a: 1}, {a: 2} }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let rel = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RELATION_LIT)
            .expect("RELATION_LIT in tree");
        let tuples: Vec<_> = rel
            .children()
            .filter(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .collect();
        assert_eq!(tuples.len(), 2);
    }

    #[test]
    fn relation_literal_trailing_comma_parses() {
        let out = parse_str("oper f {} [ let r = Relation { {a: 1}, {a: 2}, }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let rel = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RELATION_LIT)
            .expect("RELATION_LIT in tree");
        let tuples: Vec<_> = rel
            .children()
            .filter(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .collect();
        assert_eq!(tuples.len(), 2);
    }

    #[test]
    fn relation_literal_missing_lbrace_diagnoses_p0031() {
        let out = parse_str("oper f {} [ let r = Relation ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0031"),
            "expected P0031, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn relation_literal_non_tuple_element_diagnoses_p0032() {
        let out = parse_str("oper f {} [ let r = Relation { 42 }; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0032"),
            "expected P0032, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn unterminated_relation_literal_diagnoses_p0033() {
        let out = parse_str("oper f {} [ let r = Relation { {a: 1} ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0033"),
            "expected P0033, got {:?}",
            out.diagnostics
        );
    }

    // ── Sequence literals + `Sequence T` type-refs ──────────────────

    #[test]
    fn empty_sequence_literal_parses() {
        let out = parse_str("oper f {} [ let s = Sequence []; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let seq = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::SEQUENCE_LIT)
            .expect("SEQUENCE_LIT in tree");
        assert_eq!(seq.text(), "Sequence []");
    }

    #[test]
    fn sequence_literal_with_elements_parses() {
        let out = parse_str("oper f {} [ let s = Sequence [ \"a\", \"b\" ]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let seq = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::SEQUENCE_LIT)
            .expect("SEQUENCE_LIT in tree");
        let elems: Vec<_> = seq.children().filter_map(crate::ast::Expr::cast).collect();
        assert_eq!(elems.len(), 2);
    }

    #[test]
    fn sequence_literal_trailing_comma_parses() {
        let out = parse_str("oper f {} [ let s = Sequence [ 1, 2, ]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let seq = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::SEQUENCE_LIT)
            .expect("SEQUENCE_LIT in tree");
        let elems: Vec<_> = seq.children().filter_map(crate::ast::Expr::cast).collect();
        assert_eq!(elems.len(), 2);
    }

    #[test]
    fn sequence_literal_missing_lbracket_diagnoses_p0055() {
        let out = parse_str("oper f {} [ let s = Sequence ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0055"),
            "expected P0055, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn unterminated_sequence_literal_diagnoses_p0056() {
        let out = parse_str("oper f {} [ let s = Sequence [ 1 ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0056"),
            "expected P0056, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn sequence_type_ref_nests_inner_type_ref() {
        let out = parse_str("oper f {} [ let s: Sequence Integer = Sequence []; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        // The annotation is `TYPE_REF { Sequence, TYPE_REF { Integer } }`.
        let outer = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TYPE_REF)
            .expect("outer TYPE_REF in tree");
        let inner = outer
            .children()
            .find(|n| n.kind() == SyntaxKind::TYPE_REF)
            .expect("nested element TYPE_REF");
        assert_eq!(inner.text(), "Integer");
    }

    #[test]
    fn sequence_type_ref_missing_element_diagnoses_p0011() {
        let out = parse_str("oper f {} [ let s: Sequence = Sequence []; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0011"),
            "expected P0011, got {:?}",
            out.diagnostics
        );
    }

    // ── Infix operators + bool literals + `where` (Phase 20) ────────

    #[test]
    fn true_false_parse_as_bool_literals() {
        let out = parse_str("oper f {} [ let t = true; let g = false; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bools: Vec<_> = out
            .tree
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::BOOL_LITERAL)
            .collect();
        assert_eq!(bools.len(), 2);
        assert_eq!(bools[0].text(), "true");
        assert_eq!(bools[1].text(), "false");
    }

    #[test]
    fn binary_eq_wraps_two_operands_in_binary_expr() {
        let out = parse_str("oper f {} [ let b = 1 = 2; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("BINARY_EXPR in tree");
        assert_eq!(bin.text().to_string().trim(), "1 = 2");
    }

    #[test]
    fn and_binds_tighter_than_or_and_looser_than_comparison() {
        // `a = 1 and b = 2 or c = 3` →
        //   ((a = 1) and (b = 2)) or (c = 3)
        let out = parse_str("oper f {} [ let b = 1 = 1 and 2 = 2 or 3 = 3; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        // The outermost BINARY_EXPR should be `or`; check by walking
        // from the LET_STMT's value-side expression.
        let let_stmt = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LET_STMT)
            .unwrap();
        let outer_bin = let_stmt
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("LET_STMT's value is a BINARY_EXPR");
        // Outer op is `or` — the rhs is the lone `3 = 3`.
        let outer_text = outer_bin.text().to_string();
        assert!(outer_text.contains(" or "), "expected `or` at top: {outer_text}");
        // Inner-most lhs of `or` should be the `and` chain.
        let inner_bin = outer_bin
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("inner BINARY_EXPR (the `and` chain)");
        let inner_text = inner_bin.text().to_string();
        assert!(
            inner_text.contains(" and "),
            "expected `and` one level in: {inner_text}"
        );
    }

    #[test]
    fn where_is_lowest_precedence() {
        // `R where a = 1` parses as `WHERE(R, EQ(a, 1))` — the rhs of
        // `where` captures the full `a = 1` comparison without parens.
        let out = parse_str("oper f {} [ let s = R where a = 1; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let let_stmt = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LET_STMT)
            .unwrap();
        let outer = let_stmt
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("outer BINARY_EXPR");
        let outer_text = outer.text().to_string();
        assert!(outer_text.contains(" where "), "outer = {outer_text}");
        // rhs of `where` is the `a = 1` BINARY_EXPR.
        let inner = outer
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("inner BINARY_EXPR (the `a = 1` comparison)");
        assert_eq!(inner.text().to_string().trim(), "a = 1");
    }

    #[test]
    fn missing_rhs_after_operator_diagnoses_p0014() {
        // `1 = ;` — the rhs's `parse_primary_expr` sees `;`, emits
        // P0014 "expected expression". A dedicated missing-rhs code was
        // considered but deduped per the same logic as Phase 11's
        // P0011-vs-P0020/P0021 dedup.
        let out = parse_str("oper f {} [ let b = 1 = ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0014"),
            "expected P0014, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn comparison_operators_all_parse() {
        for op in &["=", "<>", "<", ">", "<=", ">="] {
            let src = format!("oper f {{}} [ let b = 1 {op} 2; ];");
            let out = parse_str(&src);
            assert!(
                out.diagnostics.is_empty(),
                "operator `{op}` failed: {:?}",
                out.diagnostics
            );
        }
    }

    // ── arithmetic & concatenation ───────────────────────────────────

    #[test]
    fn arithmetic_operators_all_parse() {
        for op in &["+", "-", "*", "/", "||"] {
            let src = format!("oper f {{}} [ let b = 1 {op} 2; ];");
            let out = parse_str(&src);
            assert!(
                out.diagnostics.is_empty(),
                "operator `{op}` failed: {:?}",
                out.diagnostics
            );
        }
    }

    #[test]
    fn arithmetic_binds_tighter_than_comparison() {
        // `a + b > c` parses as `(a + b) > c`: the outer BINARY_EXPR is `>`,
        // and its lhs is the `a + b` BINARY_EXPR.
        let out = parse_str("oper f {} [ let b = a + b > c; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let let_stmt = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LET_STMT)
            .unwrap();
        let outer = let_stmt
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("outer BINARY_EXPR");
        assert!(outer.text().to_string().contains(" > "), "outer is `>`");
        let inner = outer
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("inner BINARY_EXPR (the `a + b` sum)");
        assert_eq!(inner.text().to_string().trim(), "a + b");
    }

    #[test]
    fn multiplicative_binds_tighter_than_additive() {
        // `a + b * c` parses as `a + (b * c)`: outer `+`, its rhs is `b * c`.
        let out = parse_str("oper f {} [ let b = a + b * c; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let let_stmt = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LET_STMT)
            .unwrap();
        let outer = let_stmt
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("outer BINARY_EXPR");
        assert!(outer.text().to_string().contains(" + "), "outer is `+`");
        let inner = outer
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("inner BINARY_EXPR (the `b * c` product)");
        assert_eq!(inner.text().to_string().trim(), "b * c");
    }

    // ── extract (Phase 21) ───────────────────────────────────────────

    #[test]
    fn extract_parses_as_unary_expr() {
        let out = parse_str("oper f {} [ let t = extract R; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ue = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UNARY_EXPR)
            .expect("UNARY_EXPR in tree");
        // Operand is a NAME_REF for `R`.
        let operand = ue
            .children()
            .find(|n| n.kind() == SyntaxKind::NAME_REF)
            .expect("operand NAME_REF");
        assert_eq!(operand.text(), "R");
    }

    #[test]
    fn extract_binds_loosely_so_where_lives_inside() {
        // `extract R where a = 1` should parse as
        //   UNARY_EXPR(extract, BINARY_EXPR(R, where, BINARY_EXPR(a, =, 1)))
        let out = parse_str("oper f {} [ let t = extract R where a = 1; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ue = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UNARY_EXPR)
            .expect("UNARY_EXPR in tree");
        // The unary's operand is a BINARY_EXPR (the `where`).
        let inner = ue
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("operand BINARY_EXPR (the where)");
        let inner_text = inner.text().to_string();
        assert!(
            inner_text.contains(" where "),
            "expected `where` inside extract's operand: {inner_text}"
        );
    }

    #[test]
    fn extract_with_no_operand_diagnoses_p0014() {
        let out = parse_str("oper f {} [ let t = extract; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0014"),
            "expected P0014, got {:?}",
            out.diagnostics
        );
    }

    // ── project ──────────────────────────────────────────────────────

    #[test]
    fn project_parses_as_project_expr() {
        let out = parse_str("oper f {} [ let s = R project {a}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let pe = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::PROJECT_EXPR)
            .expect("PROJECT_EXPR in tree");
        // Operand is the NAME_REF for `R`; `a` is a bare attribute token.
        let operand = pe
            .children()
            .find(|n| n.kind() == SyntaxKind::NAME_REF)
            .expect("operand NAME_REF");
        assert_eq!(operand.text(), "R");
        assert!(pe.text().to_string().contains("project"));
    }

    #[test]
    fn project_binds_looser_than_where() {
        // `R where a = 1 project {b}` => PROJECT_EXPR(WHERE(R, a = 1), {b}).
        let out = parse_str("oper f {} [ let s = R where a = 1 project {b}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        // The outermost expression node is the projection.
        let pe = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::PROJECT_EXPR)
            .expect("PROJECT_EXPR in tree");
        // Its operand is the whole `where` comparison.
        let inner = pe
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("operand BINARY_EXPR (the where)");
        assert!(
            inner.text().to_string().contains(" where "),
            "expected `where` inside the projection's operand: {}",
            inner.text()
        );
    }

    #[test]
    fn project_then_where_nests_left() {
        // The reverse order also parses: `R project {a} where b = 1`
        // => WHERE(PROJECT(R, {a}), b = 1). Projection is the where's lhs.
        let out = parse_str("oper f {} [ let s = R project {a} where b = 1; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let let_stmt = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LET_STMT)
            .unwrap();
        let outer = let_stmt
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("outer BINARY_EXPR (the where)");
        assert!(outer.text().to_string().contains(" where "));
        assert!(
            outer
                .children()
                .any(|n| n.kind() == SyntaxKind::PROJECT_EXPR),
            "expected the where's lhs to be a PROJECT_EXPR: {}",
            outer.text()
        );
    }

    #[test]
    fn project_chains_left_associative() {
        // `R project {a} project {b}` => PROJECT(PROJECT(R, {a}), {b}).
        let out = parse_str("oper f {} [ let s = R project {a} project {b}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let outer = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::PROJECT_EXPR)
            .expect("outer PROJECT_EXPR");
        assert!(
            outer
                .children()
                .any(|n| n.kind() == SyntaxKind::PROJECT_EXPR),
            "expected a nested PROJECT_EXPR operand: {}",
            outer.text()
        );
    }

    #[test]
    fn project_empty_braces_parse_clean() {
        // Projecting onto the empty heading is syntactically valid.
        let out = parse_str("oper f {} [ let s = R project {}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::PROJECT_EXPR));
    }

    #[test]
    fn project_missing_open_brace_diagnoses_p0036() {
        let out = parse_str("oper f {} [ let s = R project a; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0036"),
            "expected P0036, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn project_non_ident_attr_diagnoses_p0037() {
        let out = parse_str("oper f {} [ let s = R project { 1 }; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0037"),
            "expected P0037, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn project_missing_close_brace_diagnoses_p0038() {
        let out = parse_str("oper f {} [ let s = R project { a ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0038"),
            "expected P0038, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn project_is_contextual_not_reserved() {
        // `project` remains a valid identifier elsewhere (no reserved
        // words): usable as a local name and as an attribute name.
        let out = parse_str("oper f {} [ let project = 1; let s = R where project = 2; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    #[test]
    fn project_all_but_parses() {
        let out = parse_str("oper f {} [ let s = R project all but {a}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let pe = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::PROJECT_EXPR)
            .expect("PROJECT_EXPR in tree");
        let text = pe.text().to_string();
        assert!(text.contains("all but"), "expected `all but` in {text}");
        // Operand is still the NAME_REF `R`.
        assert!(pe.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
    }

    #[test]
    fn project_all_without_but_diagnoses_p0039() {
        let out = parse_str("oper f {} [ let s = R project all {a}; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0039"),
            "expected P0039, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn project_all_and_but_usable_as_attr_names() {
        // Inside the braces, `all` and `but` are ordinary attribute names —
        // `project all but {all, but}` removes attributes named `all`/`but`.
        let out = parse_str("oper f {} [ let s = R project all but {all, but}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        // And the plain keep form with `all` as an attribute parses too.
        let keep = parse_str("oper f {} [ let s = R project {all}; ];");
        assert!(keep.diagnostics.is_empty(), "{:?}", keep.diagnostics);
    }

    // ── replace ──────────────────────────────────────────────────────

    #[test]
    fn replace_parses_as_replace_expr() {
        let out = parse_str("oper f {} [ let s = R replace {a: b, c: d}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let re = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::REPLACE_EXPR)
            .expect("REPLACE_EXPR in tree");
        // Operand is the NAME_REF `R`; the pairs are an ARG_LIST.
        assert!(re.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
        assert!(re.children().any(|n| n.kind() == SyntaxKind::ARG_LIST));
    }

    #[test]
    fn replace_binds_looser_than_where() {
        // `R where a = 1 replace {b: a}` => REPLACE(WHERE(R, a = 1), {b: a}).
        let out = parse_str("oper f {} [ let s = R where a = 1 replace {b: a}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let re = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::REPLACE_EXPR)
            .expect("REPLACE_EXPR in tree");
        let inner = re
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("operand BINARY_EXPR (the where)");
        assert!(inner.text().to_string().contains(" where "));
    }

    #[test]
    fn replace_missing_brace_diagnoses_p0040() {
        let out = parse_str("oper f {} [ let s = R replace a; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0040"),
            "expected P0040, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn replace_is_contextual_not_reserved() {
        // `replace` and `rename` are contextual keywords — valid identifiers
        // outside the postfix-suffix position.
        let out = parse_str(
            "oper f {} [ let replace = 1; let rename = 2; let s = R where replace = rename; ];",
        );
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    // ── rename ───────────────────────────────────────────────────────

    #[test]
    fn rename_parses_as_rename_expr() {
        let out = parse_str("oper f {} [ let s = R rename {a: b, c: d}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let re = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RENAME_EXPR)
            .expect("RENAME_EXPR in tree");
        // Operand is the NAME_REF `R`; the pairs are an ARG_LIST.
        assert!(re.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
        assert!(re.children().any(|n| n.kind() == SyntaxKind::ARG_LIST));
    }

    #[test]
    fn rename_binds_looser_than_where() {
        // `R where a = 1 rename {b: a}` => RENAME(WHERE(R, a = 1), {b: a}).
        let out = parse_str("oper f {} [ let s = R where a = 1 rename {b: a}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let re = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RENAME_EXPR)
            .expect("RENAME_EXPR in tree");
        let inner = re
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("operand BINARY_EXPR (the where)");
        assert!(inner.text().to_string().contains(" where "));
    }

    #[test]
    fn rename_interleaves_with_extend() {
        // `R extend {c: a * b} rename {ref: id}` nests left: RENAME(EXTEND(R)).
        let out = parse_str("oper f {} [ let s = R extend {c: a * b} rename {ref: id}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let re = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RENAME_EXPR)
            .expect("RENAME_EXPR at the top");
        assert!(re
            .children()
            .any(|n| n.kind() == SyntaxKind::EXTEND_EXPR));
    }

    #[test]
    fn rename_missing_brace_diagnoses_p0034() {
        let out = parse_str("oper f {} [ let s = R rename a; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0034"),
            "expected P0034, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn rename_is_contextual_not_reserved() {
        let out = parse_str("oper f {} [ let rename = 1; let s = R where rename = 2; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    // ── wrap / unwrap ─────────────────────────────────────────────────

    #[test]
    fn wrap_parses_as_wrap_expr_with_pairs() {
        let out = parse_str("oper f {} [ let s = R wrap {t: {a, b}, u: {c}}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let we = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::WRAP_EXPR)
            .expect("WRAP_EXPR in tree");
        assert!(we.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
        let pairs = we
            .children()
            .filter(|n| n.kind() == SyntaxKind::WRAP_PAIR)
            .count();
        assert_eq!(pairs, 2, "two WRAP_PAIR nodes");
    }

    #[test]
    fn unwrap_parses_as_unwrap_expr() {
        let out = parse_str("oper f {} [ let s = R unwrap {t, u}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ue = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UNWRAP_EXPR)
            .expect("UNWRAP_EXPR in tree");
        assert!(ue.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
    }

    #[test]
    fn wrap_binds_looser_than_where() {
        // `R where a = 1 wrap {t: {a}}` => WRAP(WHERE(R, a = 1), …).
        let out = parse_str("oper f {} [ let s = R where a = 1 wrap {t: {a}}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let we = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::WRAP_EXPR)
            .expect("WRAP_EXPR in tree");
        let inner = we
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("operand BINARY_EXPR (the where)");
        assert!(inner.text().to_string().contains(" where "));
    }

    #[test]
    fn unwrap_interleaves_with_wrap() {
        // `R wrap {t: {a, b}} unwrap {t}` nests left: UNWRAP(WRAP(R)).
        let out = parse_str("oper f {} [ let s = R wrap {t: {a, b}} unwrap {t}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ue = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UNWRAP_EXPR)
            .expect("UNWRAP_EXPR at the top");
        assert!(ue.children().any(|n| n.kind() == SyntaxKind::WRAP_EXPR));
    }

    #[test]
    fn wrap_missing_outer_brace_diagnoses_p0044() {
        let out = parse_str("oper f {} [ let s = R wrap a; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0044"),
            "expected P0044, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn wrap_missing_inner_brace_diagnoses_p0047() {
        let out = parse_str("oper f {} [ let s = R wrap {t: a}; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0047"),
            "expected P0047, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn unwrap_missing_brace_diagnoses_p0051() {
        let out = parse_str("oper f {} [ let s = R unwrap t; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0051"),
            "expected P0051, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn wrap_unwrap_are_contextual_not_reserved() {
        let out = parse_str("oper f {} [ let wrap = 1; let unwrap = 2; let s = R where wrap = unwrap; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    // ── extend ───────────────────────────────────────────────────────

    #[test]
    fn extend_parses_as_extend_expr() {
        let out = parse_str("oper f {} [ let s = R extend {total: a * b, c: d}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ee = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::EXTEND_EXPR)
            .expect("EXTEND_EXPR in tree");
        // Operand is the NAME_REF `R`; the pairs are an ARG_LIST.
        assert!(ee.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
        assert!(ee.children().any(|n| n.kind() == SyntaxKind::ARG_LIST));
    }

    #[test]
    fn extend_binds_looser_than_where() {
        // `R where a = 1 extend {b: a + 1}` => EXTEND(WHERE(R, a = 1), {b: a + 1}).
        let out = parse_str("oper f {} [ let s = R where a = 1 extend {b: a + 1}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ee = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::EXTEND_EXPR)
            .expect("EXTEND_EXPR in tree");
        let inner = ee
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("operand BINARY_EXPR (the where)");
        assert!(inner.text().to_string().contains(" where "));
    }

    #[test]
    fn extend_interleaves_with_replace() {
        // `R extend {c: a * b} replace {ref: id}` nests left: REPLACE(EXTEND(R)).
        let out = parse_str("oper f {} [ let s = R extend {c: a * b} replace {ref: id}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let re = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::REPLACE_EXPR)
            .expect("REPLACE_EXPR at the top");
        assert!(re
            .children()
            .any(|n| n.kind() == SyntaxKind::EXTEND_EXPR));
    }

    #[test]
    fn extend_missing_brace_diagnoses_p0043() {
        let out = parse_str("oper f {} [ let s = R extend a; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0043"),
            "expected P0043, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn extend_is_contextual_not_reserved() {
        let out = parse_str("oper f {} [ let extend = 1; let s = R where extend = 2; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    // ── tclose ───────────────────────────────────────────────────────

    #[test]
    fn tclose_bare_parses_as_tclose_expr() {
        let out = parse_str("oper f {} [ let s = R tclose; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let te = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TCLOSE_EXPR)
            .expect("TCLOSE_EXPR in tree");
        // Operand is the NAME_REF for `R`; no brace-list in the bare form.
        let operand = te
            .children()
            .find(|n| n.kind() == SyntaxKind::NAME_REF)
            .expect("operand NAME_REF");
        assert_eq!(operand.text(), "R");
        assert!(te.text().to_string().contains("tclose"));
    }

    #[test]
    fn tclose_braced_parses_with_two_attrs() {
        let out = parse_str("oper f {} [ let s = R tclose { a, b }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let te = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TCLOSE_EXPR)
            .expect("TCLOSE_EXPR in tree");
        let text = te.text().to_string();
        assert!(text.contains('a') && text.contains('b'), "{text}");
        assert!(te.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
    }

    #[test]
    fn tclose_binds_looser_than_where() {
        // `R where a = 1 tclose` => TCLOSE(WHERE(R, a = 1)).
        let out = parse_str("oper f {} [ let s = R where a = 1 tclose; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let te = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TCLOSE_EXPR)
            .expect("TCLOSE_EXPR in tree");
        let inner = te
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("operand BINARY_EXPR (the where)");
        assert!(
            inner.text().to_string().contains(" where "),
            "expected `where` inside the closure's operand: {}",
            inner.text()
        );
    }

    #[test]
    fn tclose_chains_left_associative() {
        // `R tclose tclose` => TCLOSE(TCLOSE(R)).
        let out = parse_str("oper f {} [ let s = R tclose tclose; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let outer = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TCLOSE_EXPR)
            .expect("outer TCLOSE_EXPR");
        assert!(
            outer
                .children()
                .any(|n| n.kind() == SyntaxKind::TCLOSE_EXPR),
            "expected a nested TCLOSE_EXPR operand: {}",
            outer.text()
        );
    }

    #[test]
    fn tclose_interleaves_with_project() {
        // `R project { a, b } tclose` => TCLOSE(PROJECT(R, {a, b})).
        let out = parse_str("oper f {} [ let s = R project { a, b } tclose; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let te = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TCLOSE_EXPR)
            .expect("TCLOSE_EXPR in tree");
        assert!(
            te.children().any(|n| n.kind() == SyntaxKind::PROJECT_EXPR),
            "expected a PROJECT_EXPR operand: {}",
            te.text()
        );
    }

    #[test]
    fn tclose_is_contextual_not_reserved() {
        // `tclose` remains a valid identifier elsewhere (no reserved words).
        let out = parse_str("oper f {} [ let tclose = 1; let s = R where tclose = 2; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    #[test]
    fn tclose_non_ident_attr_diagnoses_p0041() {
        let out = parse_str("oper f {} [ let s = R tclose { 1 }; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0041"),
            "expected P0041, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn tclose_missing_close_brace_diagnoses_p0042() {
        let out = parse_str("oper f {} [ let s = R tclose { a ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0042"),
            "expected P0042, got {:?}",
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
    fn malformed_call_arg_after_shorthand_diagnoses() {
        // In call position the colon is optional (field-init shorthand), so
        // `{x}` is valid and the stray `1` (no separating comma) is the error:
        // the `}` is expected. (The colon-required case now lives in replace —
        // see `replace_requires_colon_diagnoses_p0017`.)
        let out = parse_str("oper f {} [ foo{x 1}; ];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0015"));
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

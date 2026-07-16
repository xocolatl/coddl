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
        self.current_ident_text() == Some(lexeme)
    }

    /// The lexeme of the next non-trivia token when it is an identifier,
    /// else `None`. The table-lookup sibling of [`at_keyword`]: where a
    /// site resolves the current word against a `keywords` table (the
    /// infix-operator lookup) rather than testing one candidate lexeme.
    pub(crate) fn current_ident_text(&self) -> Option<&str> {
        if !self.at(SyntaxKind::IDENT) {
            return None;
        }
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind.is_trivia() {
            i += 1;
        }
        let span = self.tokens[i].span;
        Some(&self.source[span.start as usize..span.end as usize])
    }

    /// Peek the kind of the `n`-th non-trivia token from the cursor
    /// (`n == 0` is the current token, the same one [`current`] returns).
    /// Returns [`SyntaxKind::EOF`] past the end. Used for the small lookahead
    /// a sort-item needs to tell a direction keyword (`asc a`) from a bare
    /// attribute that happens to be spelled `asc` (`[asc]`).
    pub(crate) fn nth_kind(&self, n: usize) -> SyntaxKind {
        let mut i = self.pos;
        let mut seen = 0;
        while i < self.tokens.len() {
            if !self.tokens[i].kind.is_trivia() {
                if seen == n {
                    return SyntaxKind::from(self.tokens[i].kind);
                }
                seen += 1;
            }
            i += 1;
        }
        SyntaxKind::EOF
    }

    /// True iff the `n`-th non-trivia token from the cursor is an identifier
    /// whose lexeme is `lexeme` (`n == 0` is the current token). The lookahead
    /// analogue of [`at_keyword`], used to disambiguate `builtin relvar` from
    /// `builtin oper` at the item-dispatch site.
    pub(crate) fn nth_at_keyword(&self, n: usize, lexeme: &str) -> bool {
        let mut i = self.pos;
        let mut seen = 0;
        while i < self.tokens.len() {
            if !self.tokens[i].kind.is_trivia() {
                if seen == n {
                    if self.tokens[i].kind != TokenKind::Ident {
                        return false;
                    }
                    let span = self.tokens[i].span;
                    return &self.source[span.start as usize..span.end as usize] == lexeme;
                }
                seen += 1;
            }
            i += 1;
        }
        false
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

    /// Declaration-name reservation check: diagnose when the cursor sits on
    /// an IDENT whose text is a reserved word or word-operator glyph
    /// (`keywords::is_reserved`). Called immediately **before** a
    /// declaration site's `eat(IDENT)`, while the cursor is still on the
    /// name, so the diagnostic's span points at it. Soft, on the E0007
    /// model: the caller's eat proceeds, the name still binds, and parsing
    /// continues. Self-guarding — `current_ident_text()` is `None` off an
    /// IDENT, so a missing name stays the site's own diagnostic. The code
    /// is a parameter so the `.cddb` parser can emit its namespace's
    /// sibling (PB0012) for the decl sites it owns.
    pub(crate) fn check_reserved_decl_name(&mut self, code: &'static str) {
        if let Some(text) = self.current_ident_text() {
            if crate::keywords::is_reserved(text) {
                let message =
                    format!("`{text}` is a reserved word and cannot be used as an identifier");
                self.error(code, message);
            }
        }
    }

    /// [`Self::check_reserved_decl_name`] with the `.cd` parser's code.
    pub(crate) fn check_decl_name(&mut self) {
        self.check_reserved_decl_name("P0096");
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
        if self.at_keyword("program") || self.at_keyword("library") || self.at_keyword("module") {
            self.parse_file_header();
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
        } else if self.at_keyword("builtin") {
            // `builtin` qualifies either an `oper` (the prelude) or a `relvar`
            // (a stdlib runtime-backed relvar). Disambiguate on the next word.
            if self.nth_at_keyword(1, "relvar") {
                self.parse_builtin_relvar_decl();
            } else {
                self.parse_oper_decl();
            }
        } else if self.at_keyword("oper") {
            self.parse_oper_decl();
        } else if self.at_keyword("type") {
            self.parse_type_decl();
        } else if self.at_keyword("use") {
            self.parse_use_decl();
        } else if self.at_keyword("let") {
            // A module-position `let` is a **constant binding** — the same
            // production as the statement form (name, optional `: <type-ref>`
            // annotation, initializer); the typechecker applies the
            // module-scope rules (constant-expression initializer, mandatory
            // value, order-independence).
            self.parse_let_stmt();
        } else if self.at_keyword("var") {
            // Module-level mutable state is a relvar; parse the statement for
            // recovery, then reject the position.
            self.error(
                "P0086",
                "module-level mutable state is a relvar; use `let` for a \
                 constant binding",
            );
            self.parse_var_stmt();
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

        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0020", "expected database name");
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0021", "expected `;` after `database <Name>`");
        }

        self.finish_node();
    }

    /// `( program | library | module ) <name>;` — the file-kind header that
    /// every `.cd` compilation unit opens with. The leading keyword records
    /// the kind (`program` → executable, `library` → C-ABI artifact, `module`
    /// → an intermediate unit linked into a consumer); the name is the bare
    /// leaf identity. All three share the `PROGRAM_DECL` node — the kind is
    /// read back from the leading keyword token via `ProgramDecl::kind()`. The
    /// trailing semicolon is required; missing pieces produce a diagnostic but
    /// the node still closes cleanly. Whether a header is present, unique, and
    /// first is a compilation-unit rule enforced in the plan layer, not here.
    fn parse_file_header(&mut self) {
        debug_assert!(
            self.at_keyword("program") || self.at_keyword("library") || self.at_keyword("module")
        );
        self.start_node(SyntaxKind::PROGRAM_DECL);
        self.bump(); // "program" | "library" | "module"

        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error(
                "P0002",
                "expected file name after `program`/`library`/`module`",
            );
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0003", "expected `;` after file header");
        }

        self.finish_node();
    }

    /// `type <identifier> ( = <type-ref> | <heading> ) ;` — a type declaration
    /// in one of two forms, chosen by the token after the name:
    /// - `= <type-ref>` is a transparent **alias**, naming a structural type
    ///   (e.g. the prelude's `Request` / `Response`, docs/prelude.md).
    /// - `{ … }` is a **possrep-scalar** type — a distinct user-defined scalar
    ///   whose possrep components are the heading (single-possrep tier; the
    ///   component list reuses `parse_heading`). See docs/typecheck.md.
    ///
    /// Dispatched on the leading contextual `type` keyword, like the other item
    /// forms.
    fn parse_type_decl(&mut self) {
        debug_assert!(self.at_keyword("type"));
        self.start_node(SyntaxKind::TYPE_DECL);
        self.bump(); // "type"

        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0080", "expected type name after `type`");
        }
        if self.at(SyntaxKind::L_BRACE) {
            // Possrep-scalar form: the `{ … }` is the possrep component heading.
            self.parse_heading();
        } else if self.eat(SyntaxKind::EQ) {
            // Alias form.
            self.parse_type_ref();
        } else {
            self.error("P0081", "expected `{` or `=` after type name");
            self.parse_type_ref(); // recover: consume a trailing type-ref if any
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0082", "expected `;` after type declaration");
        }

        self.finish_node();
    }

    /// `use module <module-path> ;` — a module import. `module` is the only
    /// category today; `use database …` is reserved for a later item form
    /// (which is why the category word is spelled out rather than implied).
    /// Dispatched on the leading contextual `use` keyword. Missing `module`
    /// is P0083; the path/`;` diagnostics come from the helpers.
    fn parse_use_decl(&mut self) {
        debug_assert!(self.at_keyword("use"));
        self.start_node(SyntaxKind::USE_DECL);
        self.bump(); // "use"

        if self.at_keyword("module") {
            self.bump(); // "module"
        } else {
            self.error("P0083", "expected `module` after `use`");
        }

        self.parse_module_path();

        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0085", "expected `;` after `use module <path>`");
        }

        self.finish_node();
    }

    /// `<identifier> { '::' <identifier> }` — a `::`-separated module path
    /// (`coddl::core`). `::` appears only here; it is not accepted in
    /// expression or type position. A missing segment (leading or after a
    /// `::`) is P0084.
    fn parse_module_path(&mut self) {
        self.start_node(SyntaxKind::MODULE_PATH);
        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0084", "expected module name");
        }
        // Inside the loop, every path segment is name-checked, so a
        // reserved segment anywhere in the path diagnoses.
        while self.at(SyntaxKind::COLON_COLON) {
            self.bump(); // "::"
            self.check_decl_name();
            if !self.eat(SyntaxKind::IDENT) {
                self.error("P0084", "expected module name after `::`");
            }
        }
        self.finish_node();
    }

    /// `oper <name> <heading> <body>;`. The return-type clause (`: Type`
    /// or `-> Type`) is intentionally not parsed yet — the syntax for it
    /// is open. Until it settles, an operator with a return type will
    /// trigger the "expected `[`" or "expected `;`" diagnostic on the
    /// stray punctuation.
    fn parse_oper_decl(&mut self) {
        debug_assert!(self.at_keyword("oper") || self.at_keyword("builtin"));
        self.start_node(SyntaxKind::OPER_DECL);

        // Optional leading `builtin` qualifier: the operator is
        // compiler-provided (the prelude — see docs/prelude.md) and carries
        // no `[ … ]` body. Parsed here in item dispatch, mirroring the
        // leading `public` / `private` relvar qualifiers.
        let is_builtin = self.at_keyword("builtin");
        if is_builtin {
            self.bump(); // "builtin"
            if self.at_keyword("oper") {
                self.bump(); // "oper"
            } else {
                self.error("P0079", "expected `oper` or `relvar` after `builtin`");
            }
        } else {
            self.bump(); // "oper"
        }

        self.check_decl_name();
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

        if is_builtin {
            // A `builtin` operator has no body; the compiler provides the
            // implementation. A stray `[ … ]` here is an error.
            if self.at(SyntaxKind::L_BRACKET) {
                self.error("P0078", "builtin operator must not have a body");
                self.parse_block(); // consume for recovery
            }
        } else if self.at(SyntaxKind::L_BRACKET) {
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

        // Every heading funnels here (oper params, relvar attributes,
        // possrep components, Tuple/Relation generator headings — .cddb
        // relvar attributes included), so this one check covers them all.
        self.check_decl_name();
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
    /// `Customer`), the generator application `Sequence <type-ref>` — which
    /// nests an element `TYPE_REF` (e.g. `Sequence Integer`, `Sequence
    /// Sequence Text`) — or the heading generators `Tuple <heading>` /
    /// `Relation <heading>`, which nest a `HEADING` (e.g. `Relation { name:
    /// Text }`, `Tuple {}`). A bare `Tuple`/`Relation` with no `{` stays a leaf
    /// name (resolving to the unknown-type T0005, as before).
    pub(crate) fn parse_type_ref(&mut self) {
        self.bump_trivia();
        self.start_node(SyntaxKind::TYPE_REF);
        if self.at_keyword("Sequence") {
            // `Sequence <type-ref>`: the element type is a nested
            // TYPE_REF. A missing element type surfaces as P0011 from
            // the recursive call.
            self.bump(); // `Sequence`
            self.parse_type_ref();
        } else if self.at_keyword("Tuple") || self.at_keyword("Relation") {
            // `Tuple <heading>` / `Relation <heading>`: the heading is a nested
            // HEADING (reusing `parse_heading`, so attribute types nest). Only
            // when a `{` follows — a bare `Relation`/`Tuple` falls through as a
            // leaf name (the TYPE_REF holds just the keyword token → T0005).
            self.bump(); // `Tuple` / `Relation`
            if self.at(SyntaxKind::L_BRACE) {
                self.parse_heading();
            }
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

    /// `return [<expr>];` — an early return from the enclosing operator body.
    /// The value is optional: `return;` is legal only for a `Unit`-returning
    /// oper, and a present value is checked against the declared return type —
    /// both enforced in typecheck (T0018). `return` is a contextual keyword,
    /// recognized only here in statement position, so it never reserves the
    /// identifier `return` elsewhere (Coddl has no reserved words).
    fn parse_return_stmt(&mut self) {
        debug_assert!(self.at_keyword("return"));
        self.start_node(SyntaxKind::RETURN_STMT);
        self.bump(); // `return`

        // Optional value: any expression before the terminating `;`. A closing
        // `]` (the block's end) also stops the value scan so a malformed
        // `return` recovers at the block boundary rather than past it.
        if !self.at(SyntaxKind::SEMICOLON) && !self.at(SyntaxKind::R_BRACKET) {
            let before = self.pos;
            self.parse_expr();
            if self.pos == before {
                self.error("P0013", "expected `;` after `return`");
            }
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `return`");
        }
        self.finish_node();
    }

    /// `let <name> [: <type-ref>] = <expr>;` — an immutable value binding
    /// visible to subsequent statements in the same block. Type annotation
    /// is optional; when absent, the binding's type is inferred from the
    /// RHS. The mutable sibling is `var … := …` (`parse_var_stmt`). The
    /// initializer is optional at the parse level (`let x;` parses), but an
    /// uninitialized `let` is a type error (T0078) — an immutable binding must
    /// be initialized. No destructuring for now.
    fn parse_let_stmt(&mut self) {
        debug_assert!(self.at_keyword("let"));
        self.start_node(SyntaxKind::LET_STMT);
        self.bump(); // `let`

        self.bump_trivia();
        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0018", "expected binding name in `let`");
        }
        // Optional `: <type-ref>` annotation between the name and `=`.
        if self.eat(SyntaxKind::COLON) {
            self.parse_type_ref();
        }
        // The initializer is optional: a bare `let x;` parses (the typechecker
        // rejects it, T0078). When present, the operator is `=`; a `:=` is the
        // `var` operator by mistake — flag it, then consume it for recovery.
        if !self.at(SyntaxKind::SEMICOLON) {
            if self.at(SyntaxKind::ASSIGN) {
                self.error("P0067", "`let` bindings bind with `=`, not `:=`");
                self.bump(); // consume `:=` for recovery
            } else if !self.eat(SyntaxKind::EQ) {
                self.error("P0018", "expected `=` in `let`");
            }
            let before = self.pos;
            self.parse_expr();
            if self.pos == before {
                self.error("P0018", "expected expression in `let`");
            }
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `let`");
        }
        self.finish_node();
    }

    /// `var <name> [: <type-ref>] [:= <expr>];` — a mutable value binding.
    /// Like `let`, but reassignable via a bare `<name> := <expr>;`
    /// (`ASSIGN_STMT`); the binding operator is `:=`, matching the operator
    /// that reassigns it and the counted-`for` counter init. Both the type
    /// annotation and the initializer are optional: `var x;` declares an
    /// uninitialized mutable local whose type is inferred from its first
    /// assignment (definite-assignment then ensures it is assigned before it
    /// is read). No destructuring for now.
    fn parse_var_stmt(&mut self) {
        debug_assert!(self.at_keyword("var"));
        self.start_node(SyntaxKind::VAR_STMT);
        self.bump(); // `var`

        self.bump_trivia();
        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0018", "expected binding name in `var`");
        }
        // Optional `: <type-ref>` annotation between the name and `:=`.
        // (`:=` lexes as one `ASSIGN` token, so this never eats its `:`.)
        if self.eat(SyntaxKind::COLON) {
            self.parse_type_ref();
        }
        // The initializer is optional: a bare `var x;` declares without a value
        // (the future `load`/a later `x := …` fills it). When present, the
        // operator is `:=`; a plain `=` is the `let` operator by mistake — flag
        // it, then consume it for recovery.
        if !self.at(SyntaxKind::SEMICOLON) {
            if self.at(SyntaxKind::EQ) {
                self.error("P0068", "`var` bindings assign with `:=`, not `=`");
                self.bump(); // consume `=` for recovery
            } else if !self.eat(SyntaxKind::ASSIGN) {
                self.error("P0018", "expected `:=` in `var`");
            }
            let before = self.pos;
            self.parse_expr();
            if self.pos == before {
                self.error("P0018", "expected expression in `var`");
            }
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `var`");
        }
        self.finish_node();
    }

    /// Two forms, dispatched on the header separator after the loop variable:
    ///   counted — `for <ident> := <expr> to <expr> do <block> ;`
    ///   element — `for <ident> in <expr> do <block> ;`
    /// Counted has an **inclusive** upper bound (`i <= hi`); element iterates a
    /// `Sequence` (RM Pro 7 forbids tuple-at-a-time over a relation, so a
    /// relation operand is a type error). `in`/`to`/`do` are contextual
    /// keywords recognized only in this statement position (Coddl has no
    /// reserved words); each `<expr>` stops at the next keyword because none of
    /// them is an infix operator or a postfix trigger. The loop variable is
    /// loop-scoped and immutable (assigning it is T0072). A trailing `;` is
    /// required (P0013). Both forms build one `FOR_STMT`; the AST distinguishes
    /// them by the presence of the `:=` token.
    fn parse_for_stmt(&mut self) {
        debug_assert!(self.at_keyword("for"));
        self.start_node(SyntaxKind::FOR_STMT);
        self.bump(); // `for`

        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0062", "expected a loop variable name after `for`");
            self.finish_node();
            return;
        }
        if self.at_keyword("in") {
            // Element loop: `in <expr>` — the sequence to iterate.
            self.bump(); // `in`
            self.parse_expr();
        } else if self.eat(SyntaxKind::ASSIGN) {
            // Counted loop: `:= <expr> to <expr>` (inclusive).
            self.parse_expr(); // lower bound — stops at `to`
            if !self.at_keyword("to") {
                self.error("P0064", "expected `to` after the `for` lower bound");
                self.finish_node();
                return;
            }
            self.bump(); // `to`
            self.parse_expr(); // upper bound — stops at `do`
        } else {
            self.error(
                "P0063",
                "expected `:=` or `in` after the `for` loop variable",
            );
            self.finish_node();
            return;
        }
        // Shared tail: `do <block> ;`.
        if !self.at_keyword("do") {
            self.error("P0065", "expected `do` before the `for` loop body");
            self.finish_node();
            return;
        }
        self.bump(); // `do`
        if !self.at(SyntaxKind::L_BRACKET) {
            self.error("P0066", "expected `[` to start the `for` loop body");
            self.finish_node();
            return;
        }
        self.parse_block();
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after the `for` loop");
        }
        self.finish_node();
    }

    /// `while <expr> do <block> ;` — the pre-test loop. The condition is a full
    /// `<expr>` (it stops at `do`, which is neither an infix operator nor a
    /// postfix trigger); `while`/`do` are contextual keywords recognized only in
    /// this statement position. The condition is tested before each iteration —
    /// the loop is empty-safe. A trailing `;` is required (P0013). Builds
    /// `WHILE_STMT`.
    fn parse_while_stmt(&mut self) {
        debug_assert!(self.at_keyword("while"));
        self.start_node(SyntaxKind::WHILE_STMT);
        self.bump(); // `while`
        self.parse_expr(); // condition — stops at `do`
        if !self.at_keyword("do") {
            self.error("P0069", "expected `do` before the `while` loop body");
            self.finish_node();
            return;
        }
        self.bump(); // `do`
        if !self.at(SyntaxKind::L_BRACKET) {
            self.error("P0070", "expected `[` to start the `while` loop body");
            self.finish_node();
            return;
        }
        self.parse_block();
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after the `while` loop");
        }
        self.finish_node();
    }

    /// `do <block> while <expr> ;` — the post-test loop (C-style do…while). The
    /// body runs once before the condition is first tested. A statement-leading
    /// `do` is reserved exclusively for this form and *requires* a trailing
    /// `while <cond>` — a bare `do [ … ]` block statement is a parse error
    /// (P0072), otherwise `do [B] while c do [B2]` would be ambiguous against
    /// "run block, then a pre-test loop". A trailing `;` is required (P0013).
    /// Builds `DO_WHILE_STMT`.
    fn parse_do_while_stmt(&mut self) {
        debug_assert!(self.at_keyword("do"));
        self.start_node(SyntaxKind::DO_WHILE_STMT);
        self.bump(); // `do`
        if !self.at(SyntaxKind::L_BRACKET) {
            self.error("P0071", "expected `[` to start the `do` loop body");
            self.finish_node();
            return;
        }
        self.parse_block();
        if !self.at_keyword("while") {
            self.error("P0072", "expected `while` after the `do` loop body");
            self.finish_node();
            return;
        }
        self.bump(); // `while`
        self.parse_expr(); // condition
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after the `do` loop");
        }
        self.finish_node();
    }

    /// `load <target> from <source-expr> [ order [ <sort-item> { ',' <sort-item> } ] ] ;`
    /// — the sole relation→sequence iteration gate (RM Pro 7): force the source
    /// relation, impose an order, and materialize its tuples into the `Sequence`
    /// target. `load` / `from` / `order` are contextual keywords recognized only
    /// in this statement position (Coddl reserves no words); the source `Expr`
    /// stops at `order` because that word is neither an infix nor a postfix
    /// trigger. The `order` clause is an ordered bracket-list of `<sort-item>`s
    /// (the same production the window `rank` reuses) and is optional — the
    /// reverse `load <relvar> from <sequence>` form carries none. A trailing `;`
    /// is required (P0013). Builds `LOAD_STMT`.
    fn parse_load_stmt(&mut self) {
        debug_assert!(self.at_keyword("load"));
        self.start_node(SyntaxKind::LOAD_STMT);
        self.bump(); // `load`

        self.bump_trivia();
        self.check_decl_name();
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0073", "expected a target name after `load`");
        }
        if self.at_keyword("from") {
            self.bump(); // `from`
        } else {
            self.error("P0074", "expected `from` after the `load` target");
        }
        // The source relation expression — stops at `order` (a missing source
        // is P0014 "expected expression" from the expression parser).
        self.parse_expr();
        // Optional `order [ <sort-item> { ',' <sort-item> } ]`.
        if self.at_keyword("order") {
            self.bump(); // `order`
            if !self.eat(SyntaxKind::L_BRACKET) {
                self.error("P0075", "expected `[` to open the `load` order list");
            } else if self.at(SyntaxKind::R_BRACKET) {
                // An empty `order []` has no order keys.
                self.error("P0077", "expected an attribute name in the order key");
                self.bump(); // `]`
            } else {
                loop {
                    self.parse_sort_item();
                    if !self.eat(SyntaxKind::COMMA) {
                        break;
                    }
                    if self.at(SyntaxKind::R_BRACKET) {
                        break; // trailing comma ok
                    }
                }
                if !self.eat(SyntaxKind::R_BRACKET) {
                    self.error("P0076", "expected `]` to close the `load` order list");
                }
            }
        }
        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `load`");
        }
        self.finish_node();
    }

    /// `[asc|desc]? <attr>` — one entry in a `load` (or window `rank`) order
    /// list. The direction is an optional contextual keyword, recognized only
    /// when an attribute `IDENT` follows it; a bare attribute defaults to `asc`,
    /// so an attribute literally named `asc` / `desc` still parses as the order
    /// key (no reserved words). Builds `SORT_ITEM`.
    fn parse_sort_item(&mut self) {
        self.bump_trivia();
        self.start_node(SyntaxKind::SORT_ITEM);
        if (self.at_keyword(crate::keywords::ASC) || self.at_keyword(crate::keywords::DESC))
            && self.nth_kind(1) == SyntaxKind::IDENT
        {
            self.bump(); // direction keyword
        }
        if !self.eat(SyntaxKind::IDENT) {
            self.error("P0077", "expected an attribute name in the order key");
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
                self.error(
                    "P0014",
                    "expected a relation or `{ … }` tuple-set to insert",
                );
            }
        }

        if !self.eat(SyntaxKind::SEMICOLON) {
            self.error("P0013", "expected `;` after `insert`");
        }
        self.finish_node();
    }

    /// `{ <expr> , … }` — a brace tuple-set, the keyword-less spelling of a
    /// relation literal (shares the element-expression body with
    /// `parse_relation_lit` and builds the same `RELATION_LIT` node, so the
    /// checker/lowerer treat it as a relation source uniformly). Each element is
    /// a tuple-typed expression (P0033 if unterminated). An empty `{}` yields a
    /// zero-tuple relation literal (the typechecker rejects it, T0018).
    fn parse_tuple_set(&mut self) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.bump_trivia();
        self.start_node(SyntaxKind::RELATION_LIT);
        self.bump(); // {
        self.parse_relation_lit_body();
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

        // The `{ c: e, … }` clause — names *reference* existing attributes
        // (no reserved-name check; the decl sites already reject them).
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_arg_list(false, false);
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
        if self.at_keyword("var") {
            self.parse_var_stmt();
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
        if self.at_keyword("for") {
            self.parse_for_stmt();
            return;
        }
        if self.at_keyword("while") {
            self.parse_while_stmt();
            return;
        }
        if self.at_keyword("do") {
            self.parse_do_while_stmt();
            return;
        }
        if self.at_keyword("load") {
            self.parse_load_stmt();
            return;
        }
        if self.at_keyword("return") {
            self.parse_return_stmt();
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
                    // Argument names *reference* the callee's params (no
                    // reserved-name check; param decls already reject them).
                    self.parse_arg_list(true, false); // calls allow field-init shorthand
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
                // Postfix sequence index `s[i]` — 0-based, binds tighter than
                // the pipeline operators (like the call/field-access arms), so
                // `x[0][1]` nests left-associatively via the shared checkpoint.
                SyntaxKind::L_BRACKET => {
                    self.start_node_at(cp, SyntaxKind::INDEX_EXPR);
                    self.bump(); // `[`
                    let before = self.pos;
                    self.parse_expr();
                    if self.pos == before {
                        self.error("P0058", "expected index expression");
                    }
                    if !self.eat(SyntaxKind::R_BRACKET) {
                        self.error("P0057", "expected `]` to close index expression");
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
            if min_prec == 0 && self.at_keyword("group") {
                self.start_node_at(cp, SyntaxKind::GROUP_EXPR);
                self.parse_group_suffix();
                self.finish_node();
                continue;
            }
            if min_prec == 0 && self.at_keyword("ungroup") {
                self.start_node_at(cp, SyntaxKind::UNGROUP_EXPR);
                self.parse_ungroup_suffix();
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
            // The two-word `not matching` antijoin bumps both operator tokens;
            // every other infix op (incl. the `⋉`/`▷` glyphs) is a single token.
            let two_word = self.at_keyword("not") && self.nth_at_keyword(1, "matching");
            self.bump(); // operator token or keyword IDENT (`not` for `not matching`)
                         // Missing-rhs (e.g. `1 = ;`) surfaces as P0014 from the
                         // inner `parse_primary_expr` — no dedicated code needed.
            if two_word {
                self.bump_trivia();
                self.bump(); // the `matching` token of `not matching`
            }
            self.parse_expr_prec(prec + 1, allow_brace_call);
            self.finish_node();
        }
    }

    /// Peek the next infix operator's precedence. Returns `None` if
    /// the cursor isn't on a recognized infix operator. Operators
    /// recognized by token kind: `*`, `/` (5); `+`, `-`, `||` (4);
    /// `=`, `<>`, `<`, `>`, `<=`, `>=` (all at prec 3). Textual operators
    /// (keyword or glyph IDENT — a glyph lexes as an IDENT whose text is
    /// the glyph) resolve through the shared [`keywords::INFIX_OPS`]
    /// table, the same table the AST view's `op_token`/`op_kind` use, so
    /// the parser and the AST cannot disagree on the operator inventory.
    fn peek_infix_prec(&self) -> Option<u8> {
        match self.current() {
            // Multiplicative — binds tightest among infix ops (`div` rides
            // the keyword table at this level).
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
            // The two-word `not matching` antijoin is the one operator the
            // table can't see from a single token: recognized by a two-token
            // peek (`not` then `matching`), both bumped in the loop. Its
            // one-token glyph `▷` resolves through the table like the rest.
            SyntaxKind::IDENT if self.at_keyword("not") && self.nth_at_keyword(1, "matching") => {
                Some(0)
            }
            SyntaxKind::IDENT => crate::keywords::infix(self.current_ident_text()?).map(|e| e.prec),
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
        if self.at_keyword("if") {
            self.parse_if_expr();
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
        // `not <expr>` — Boolean prefix negation. Recognized before the
        // generic IDENT branch so the AST gets a distinct `UNARY_EXPR` node.
        // The `¬` glyph is a lexed IDENT whose text is the glyph, matched by
        // `at_keyword` directly (a raw source-text compare).
        if self.at_keyword("not") || self.at_keyword("¬") {
            self.parse_not_expr();
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
            // Literal fields *declare* the tuple's heading — reserved names
            // are rejected like any attribute declaration.
            self.parse_named_arg(true, true);
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

    /// `Relation { <expr>, <expr>, … }` in expression position. The body is a
    /// comma-separated list of element **expressions**, each of which must be
    /// tuple-typed (the typechecker enforces it, T0096). A tuple literal
    /// `{ name: value, … }` is an expression (it parses as `Expr::TupleLit` via
    /// `parse_primary_expr`), so `Relation { {a:1}, {a:2} }` still works; a
    /// tuple-valued name / call / field-access is any other expression, so
    /// `Relation { req }` works too. Trailing comma allowed. Empty `Relation {}`
    /// parses cleanly (typechecked to `relfalse` absent an annotation). Symmetric
    /// with `Sequence [ … ]` (`parse_sequence_lit`).
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

        self.parse_relation_lit_body();
        self.finish_node();
    }

    /// The comma-separated element-expression body shared by `Relation { … }`
    /// (`parse_relation_lit`) and the keyword-less `insert R { … }` tuple-set
    /// (`parse_tuple_set`). Assumes the opening `{` is already consumed; consumes
    /// through the closing `}`. Each element is parsed with `parse_expr` (the
    /// no-progress guard bails on a garbage element rather than spinning),
    /// mirroring `parse_sequence_lit`.
    fn parse_relation_lit_body(&mut self) {
        if self.eat(SyntaxKind::R_BRACE) {
            return;
        }
        loop {
            let before = self.pos;
            self.parse_expr();
            // No progress (garbage element) — bail rather than spin; recovery
            // happens at the enclosing statement anchor.
            if self.pos == before {
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

    /// `not <expr>` / `¬ <expr>` — Boolean prefix negation. Parsed as a
    /// `UNARY_EXPR` containing one operand (typechecked to be Boolean). The
    /// operand parses at precedence 3 (comparison level): since
    /// `parse_expr_prec` continues while `prec >= min_prec`, comparison and
    /// arithmetic (prec 3–5) bind inside the operand while `and` (2) / `or`
    /// (1) stay outside — so `not a and b` reads as `(not a) and b` and
    /// `not a = b` as `not (a = b)`, matching the `and < not < comparison`
    /// ladder. Nested `not not p` falls out naturally.
    fn parse_not_expr(&mut self) {
        debug_assert!(self.at_keyword("not") || self.at_keyword("¬"));
        self.bump_trivia();
        self.start_node(SyntaxKind::UNARY_EXPR);
        self.bump(); // `not` / `¬`
        self.parse_expr_prec(3, true);
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

    /// `if <cond> then [ <block> ]` with an optional `else [ <block> ]`.
    /// `then` delimits the condition, so it parses at full precedence — a
    /// trailing index run like `grid[r][c]` belongs to the condition, not
    /// the block (`[` is otherwise ambiguous between postfix index and the
    /// ordered block). Both arms are ordered statement blocks
    /// (`parse_block`); `else` is optional — a bare `if … then [ … ]` is the
    /// Unit-typed statement form.
    fn parse_if_expr(&mut self) {
        debug_assert!(self.at_keyword("if"));
        self.bump_trivia();
        self.start_node(SyntaxKind::IF_EXPR);
        self.bump(); // `if`

        // Condition — a full expression. `then` is neither an infix operator
        // (`peek_infix_prec`) nor a postfix trigger, so the condition stops
        // there naturally. A missing condition surfaces as P0014.
        self.parse_expr();

        if !self.at_keyword("then") {
            self.error("P0059", "expected `then` after the `if` condition");
            self.finish_node();
            return;
        }
        self.bump(); // `then`

        if !self.at(SyntaxKind::L_BRACKET) {
            self.error("P0060", "expected `[` to start the `if` block");
            self.finish_node();
            return;
        }
        self.parse_block();

        if self.at_keyword("else") {
            self.bump(); // `else`
            if !self.at(SyntaxKind::L_BRACKET) {
                self.error("P0061", "expected `[` after `else`");
                self.finish_node();
                return;
            }
            self.parse_block();
        }

        self.finish_node();
    }

    /// `{ <named_arg>, … }` — the call-site argument list. Empty and
    /// trailing-comma forms are both accepted. `check_decl` marks the
    /// callers whose names *declare* attributes (`extend`/`replace`/`rename`
    /// new names) rather than referencing existing ones (call arguments,
    /// `update` clauses) — see `parse_named_arg`.
    fn parse_arg_list(&mut self, allow_shorthand: bool, check_decl: bool) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.start_node(SyntaxKind::ARG_LIST);
        self.bump(); // {

        if self.eat(SyntaxKind::R_BRACE) {
            self.finish_node();
            return;
        }

        loop {
            self.parse_named_arg(allow_shorthand, check_decl);
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
    ///
    /// `check_decl` runs the reserved-name check (P0096) on the name: set by
    /// the callers whose names **declare** an attribute — tuple/relation
    /// literal fields and `extend`/`replace`/`rename` new names — and unset
    /// where the name **references** an existing parameter or attribute
    /// (operator call arguments, the `update` clause), which either resolves
    /// against an already-checked declaration or fails in the typechecker.
    fn parse_named_arg(&mut self, allow_shorthand: bool, check_decl: bool) {
        self.bump_trivia();
        self.start_node(SyntaxKind::NAMED_ARG);
        let cp = self.checkpoint();
        if check_decl {
            self.check_decl_name();
        }
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

    /// `builtin relvar <Name> <heading> { <key-clause> } ;` — a compiler-
    /// provided relvar whose backing the runtime supplies (the stdlib; see
    /// docs/prelude.md). Reuses the shared relvar tail with `builtin` in the
    /// kind-keyword slot. Dispatched from `parse_item` when `builtin` is
    /// followed by `relvar`.
    fn parse_builtin_relvar_decl(&mut self) {
        debug_assert!(self.at_keyword("builtin"));
        self.parse_relvar_with_heading(SyntaxKind::BUILTIN_RELVAR_DECL);
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

        self.check_decl_name();
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
        if self.at_keyword(crate::keywords::ALL) {
            self.bump(); // `all`
            if self.at_keyword(crate::keywords::BUT) {
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
            // New-attribute names: reserved names rejected (check_decl).
            self.parse_arg_list(false, true); // replace keeps the colon required (no shorthand)
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
            // New-attribute names: reserved names rejected (check_decl).
            self.parse_arg_list(false, true); // rename keeps the colon required (no shorthand)
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
            self.check_decl_name();
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

    /// `group { pq: { a, b }, … }` — relational group suffix (TTM GROUP). The
    /// enclosing `GROUP_EXPR` node (wrapping the operand) is opened by the
    /// caller, so this consumes the `group` keyword and the `{ new: { idents } }`
    /// pair list. Each pair is a `GROUP_PAIR` node: the new relation-valued
    /// attribute name, a colon, then an unordered brace-list of the existing
    /// attribute names to CONSUME into it (NOT an expression) — the attributes
    /// *not* named in any pair survive and partition the relation. Same
    /// production shape as `wrap`; the semantics differ (cardinality-changing
    /// nest vs. heading rewrite).
    ///
    /// Diagnostics: P0032 (no outer `{`), P0092 (no outer `}`); per pair P0087
    /// (no new name), P0088 (no `:`); the inner brace-list emits P0089 (no `{`),
    /// P0090 (no attribute name), P0091 (no `}`).
    pub(crate) fn parse_group_suffix(&mut self) {
        debug_assert!(self.at_keyword("group"));
        self.bump_trivia();
        self.bump(); // `group`
        if !self.eat(SyntaxKind::L_BRACE) {
            self.error("P0032", "expected `{` to start group list");
            return;
        }
        if self.eat(SyntaxKind::R_BRACE) {
            return;
        }
        loop {
            self.bump_trivia();
            self.start_node(SyntaxKind::GROUP_PAIR);
            self.check_decl_name();
            if !self.eat(SyntaxKind::IDENT) {
                self.error("P0087", "expected new attribute name in group");
            }
            if !self.eat(SyntaxKind::COLON) {
                self.error("P0088", "expected `:` after group attribute name");
            }
            self.parse_ident_brace_list(
                ("P0089", "expected `{` to start grouped-attribute list"),
                ("P0090", "expected attribute name in grouped-attribute list"),
                ("P0091", "expected `}` to close grouped-attribute list"),
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
            self.error("P0092", "expected `}` to close group list");
        }
    }

    /// `ungroup { pq, … }` — relational ungroup suffix (TTM UNGROUP). The
    /// enclosing `UNGROUP_EXPR` node (wrapping the operand) is opened by the
    /// caller, so this consumes the `ungroup` keyword and the unordered
    /// brace-list of relation-valued attribute names to unnest. Reuses
    /// `parse_ident_brace_list` (the same shape as `unwrap`).
    ///
    /// Diagnostics: P0093 (no `{`), P0094 (no attribute name), P0095 (no `}`).
    pub(crate) fn parse_ungroup_suffix(&mut self) {
        debug_assert!(self.at_keyword("ungroup"));
        self.bump_trivia();
        self.bump(); // `ungroup`
        self.parse_ident_brace_list(
            ("P0093", "expected `{` to start ungroup list"),
            ("P0094", "expected attribute name in ungroup list"),
            ("P0095", "expected `}` to close ungroup list"),
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
            // New-attribute names: reserved names rejected (check_decl).
            self.parse_arg_list(false, true); // colon required (no shorthand)
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
    fn not_and_glyph_parse_to_unary_expr() {
        // `not <expr>` and its `¬` glyph both build a UNARY_EXPR with no
        // diagnostics; the AST resolves either spelling to `UnaryOp::Not`.
        for src in [
            "program p; oper main {} [ let x = not true; ];",
            "program p; oper main {} [ let x = ¬ true; ];",
        ] {
            let out = parse_str(src);
            assert!(
                out.diagnostics.is_empty(),
                "src={src}: {:?}",
                out.diagnostics
            );
            let unary = out
                .tree
                .descendants()
                .find(|n| n.kind() == SyntaxKind::UNARY_EXPR)
                .and_then(<crate::ast::UnaryExpr as crate::ast::AstNode>::cast)
                .unwrap_or_else(|| panic!("src={src}: expected a UNARY_EXPR"));
            assert_eq!(unary.op_kind(), Some(crate::ast::UnaryOp::Not), "src={src}");
        }
    }

    #[test]
    fn every_infix_table_entry_parses_to_its_op() {
        // The parser↔AST↔table consistency proof: every `keywords::INFIX_OPS`
        // spelling (word and glyph) parses as a BINARY_EXPR whose `op_kind`
        // is the entry's operator. An entry recognized by the parser but
        // unresolvable by the AST (or vice versa) fails here — the drift
        // this table exists to prevent. The two-token `not matching` word
        // form (a space, never a single token) is exercised separately below.
        let spellings = crate::keywords::INFIX_OPS.iter().flat_map(|e| {
            let word = (!e.word.contains(' ')).then_some((e.word, e.op));
            let glyph = e.glyph.map(|g| (g, e.op));
            word.into_iter().chain(glyph)
        });
        for (spelling, op) in spellings.chain([("not matching", crate::ast::BinaryOp::NotMatching)])
        {
            let src = format!("program p; oper main {{}} [ let x = a {spelling} b; ];");
            let out = parse_str(&src);
            assert!(
                out.diagnostics.is_empty(),
                "src={src}: {:?}",
                out.diagnostics
            );
            let bin = out
                .tree
                .descendants()
                .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
                .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
                .unwrap_or_else(|| panic!("src={src}: expected a BINARY_EXPR"));
            assert_eq!(bin.op_kind(), Some(op), "src={src}");
        }
    }

    #[test]
    fn reserved_name_diagnosed_at_every_decl_site() {
        // P0096 fires wherever a reserved word (or word-operator glyph)
        // would *declare* a name — one case per declaration-site kind.
        let cases: &[&str] = &[
            // file header / database / type / module path (both segments)
            "program not;",
            "program p; database extract;",
            "program p; type false = Integer;",
            "program p; use module if::core;",
            "program p; use module coddl::if;",
            // oper name / param / relvar attribute (the heading funnel) /
            // relvar name
            "program p; oper extract {} [ ];",
            "program p; oper f { if: Integer } [ ];",
            "program p; public relvar R { not: Integer } key { not };",
            "program p; public relvar if { a: Integer } key { a };",
            // statement binders
            "program p; oper main {} [ let true = 1; ];",
            "program p; oper main {} [ var false := 1; ];",
            "program p; oper main {} [ for if := 0 to 2 do [ 1; ]; ];",
            "program p; oper main {} [ load if from r; ];",
            // a word-operator glyph is declarable-shaped and equally reserved
            "program p; oper main {} [ let ⋈ = 1; ];",
            // attribute creators: literal fields, extend/replace/rename new
            // names, wrap/group new names
            "program p; oper main {} [ let t = { if: 1 }; ];",
            "program p; oper main {} [ let x = r extend { true: 1 }; ];",
            "program p; oper main {} [ let x = r replace { false: a + 1 }; ];",
            "program p; oper main {} [ let x = r rename { extract: a }; ];",
            "program p; oper main {} [ let x = r wrap { if: { a } }; ];",
            "program p; oper main {} [ let x = r group { not: { a } }; ];",
        ];
        for src in cases {
            let out = parse_str(src);
            assert!(
                out.diagnostics.iter().any(|d| d.code == "P0096"),
                "src={src}: expected P0096, got {:?}",
                out.diagnostics
            );
        }
    }

    #[test]
    fn reserved_name_check_is_soft_and_parsing_continues() {
        // The E0007 model: the diagnostic is emitted, the name still binds
        // (its LET_STMT is built), and the rest of the block parses.
        let out = parse_str("program p; oper main {} [ let if = 2; let y = 3; ];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0096"));
        let lets = out
            .tree
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::LET_STMT)
            .count();
        assert_eq!(lets, 2, "both bindings still parse");
    }

    #[test]
    fn reserved_name_check_skips_reference_positions() {
        // Call-argument names reference the callee's params and `update`
        // clause names reference existing attributes — neither declares, so
        // neither is checked (the decl sites already reject reserved names;
        // an unresolvable reference is the typechecker's to report).
        for src in [
            "program p; oper main {} [ f { if: 1 }; ];",
            "program p; oper main {} [ update R { if: 1 }; ];",
            // Brace-list and dot references to attributes are likewise free.
            "program p; oper main {} [ let x = r project { if }; ];",
            "program p; oper main {} [ let x = t.if; ];",
        ] {
            let out = parse_str(src);
            assert!(
                !out.diagnostics.iter().any(|d| d.code == "P0096"),
                "src={src}: reference position must not fire P0096, got {:?}",
                out.diagnostics
            );
        }
    }

    #[test]
    fn relational_word_glyphs_parse_to_binary_ops() {
        // `⋈ ∪ ∩ ∖` are exact synonyms for `join`/`union`/`intersect`/`minus`:
        // each parses into a BINARY_EXPR the AST resolves to the same `BinaryOp`
        // as its ASCII spelling. (`times`/`compose` have no glyph.)
        use crate::ast::BinaryOp;
        for (glyph, want) in [
            ("⋈", BinaryOp::Join),
            ("∪", BinaryOp::Union),
            ("∩", BinaryOp::Intersect),
            ("∖", BinaryOp::Minus),
            ("⋉", BinaryOp::Matching),
            ("▷", BinaryOp::NotMatching),
        ] {
            let src = format!("program p; oper main {{}} [ let x = r {glyph} s; ];");
            let out = parse_str(&src);
            assert!(
                out.diagnostics.is_empty(),
                "glyph={glyph}: {:?}",
                out.diagnostics
            );
            let bin = out
                .tree
                .descendants()
                .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
                .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
                .unwrap_or_else(|| panic!("glyph={glyph}: expected a BINARY_EXPR"));
            assert_eq!(bin.op_kind(), Some(want), "glyph={glyph}");
        }
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
    fn library_and_module_headers_parse() {
        for src in ["library foo;", "module foo;"] {
            let out = parse_str(src);
            assert!(out.diagnostics.is_empty(), "{src}: {:?}", out.diagnostics);
            assert_eq!(out.tree.text(), src);
            assert_eq!(kinds(&out), vec![SyntaxKind::PROGRAM_DECL], "{src}");
        }
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
    fn use_module_parses_clean() {
        let out = parse_str("use module coddl::web;");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(kinds, vec![SyntaxKind::USE_DECL]);
        // Lossless round-trip (the `::` survives in the CST).
        assert_eq!(out.tree.text(), "use module coddl::web;");
    }

    #[test]
    fn use_module_path_segments_via_ast() {
        use crate::ast::{AstNode, Item, Root};
        let out = parse_str("use module a::b::c;");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let root = Root::cast(out.tree).unwrap();
        let Some(Item::UseDecl(u)) = root.items().next() else {
            panic!("expected a use decl");
        };
        assert_eq!(
            u.category().map(|t| t.text().to_string()).as_deref(),
            Some("module")
        );
        let segs: Vec<String> = u.segments().map(|t| t.text().to_string()).collect();
        assert_eq!(segs, vec!["a", "b", "c"]);
    }

    #[test]
    fn use_missing_module_keyword_diagnoses_p0083() {
        // `use coddl::core;` (forgot `module`) — one P0083; the path recovers.
        let out = parse_str("use coddl::core;");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0083"),
            "{:?}",
            out.diagnostics
        );
    }

    #[test]
    fn use_missing_path_diagnoses_p0084() {
        let out = parse_str("use module ;");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0084"),
            "{:?}",
            out.diagnostics
        );
    }

    #[test]
    fn use_trailing_coloncolon_diagnoses_p0084() {
        let out = parse_str("use module coddl::;");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0084"),
            "{:?}",
            out.diagnostics
        );
    }

    #[test]
    fn use_missing_semicolon_diagnoses_p0085() {
        let out = parse_str("use module coddl::core");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0085"),
            "{:?}",
            out.diagnostics
        );
    }

    #[test]
    fn builtin_relvar_parses_clean() {
        let out = parse_str("builtin relvar Environment { name: Text, value: Text } key { name };");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(kinds, vec![SyntaxKind::BUILTIN_RELVAR_DECL]);
    }

    #[test]
    fn builtin_oper_still_parses_as_oper() {
        // The `builtin` dispatch lookahead must not steal `builtin oper`.
        let out = parse_str("builtin oper foo {};");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let kinds: Vec<_> = out.tree.children().map(|n| n.kind()).collect();
        assert_eq!(kinds, vec![SyntaxKind::OPER_DECL]);
    }

    #[test]
    fn builtin_followed_by_neither_diagnoses_p0079() {
        let out = parse_str("builtin foo {};");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0079"),
            "{:?}",
            out.diagnostics
        );
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
    fn return_stmt_with_value_parses() {
        let out = parse_str("oper f {} -> Integer [ return 1; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ret = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RETURN_STMT)
            .expect("RETURN_STMT in tree");
        let kinds: Vec<_> = ret.children_with_tokens().map(|e| e.kind()).collect();
        assert!(
            kinds.contains(&SyntaxKind::LITERAL),
            "no returned value in {kinds:?}"
        );
    }

    #[test]
    fn return_stmt_bare_parses() {
        let out = parse_str("oper f {} [ return; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            out.tree
                .descendants()
                .any(|n| n.kind() == SyntaxKind::RETURN_STMT),
            "no RETURN_STMT for bare `return;`"
        );
    }

    #[test]
    fn return_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper f {} -> Integer [ return 1 ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
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
        assert_eq!(
            out.tree.text(),
            "oper main {} [ insert R { {a: 1}, {a: 2} }; ];"
        );
        let insert = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::INSERT_STMT)
            .expect("INSERT_STMT in tree");
        // Target NAME_REF + the tuple-set as a (keyword-less) RELATION_LIT.
        let kinds: Vec<_> = insert.children().map(|n| n.kind()).collect();
        assert!(
            kinds.contains(&SyntaxKind::NAME_REF),
            "target NAME_REF in {kinds:?}"
        );
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
            rel.children()
                .filter(|n| n.kind() == SyntaxKind::TUPLE_LIT)
                .count(),
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
        assert_eq!(
            out.tree.text(),
            "oper main {} [ update R where a = 1 { b: 2 }; ];"
        );
        let update = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UPDATE_STMT)
            .expect("UPDATE_STMT in tree");
        // Operand is the `where` BINARY_EXPR; clause is a separate ARG_LIST — the
        // brace was NOT swallowed into the predicate.
        let kinds: Vec<_> = update.children().map(|n| n.kind()).collect();
        assert!(
            kinds.contains(&SyntaxKind::BINARY_EXPR),
            "operand BINARY_EXPR in {kinds:?}"
        );
        assert!(
            kinds.contains(&SyntaxKind::ARG_LIST),
            "clause ARG_LIST in {kinds:?}"
        );
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
        assert!(
            kinds.contains(&SyntaxKind::NAME_REF),
            "operand NAME_REF in {kinds:?}"
        );
        assert!(
            kinds.contains(&SyntaxKind::ARG_LIST),
            "clause ARG_LIST in {kinds:?}"
        );
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
            out.tree
                .descendants()
                .any(|n| n.kind() == SyntaxKind::CALL_EXPR),
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
            out.tree
                .descendants()
                .any(|n| n.kind() == SyntaxKind::CALL_EXPR),
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
    fn matching_infix_parses() {
        use crate::ast::AstNode;
        let out = parse_str("oper main {} [ R matching S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
            .expect("BINARY_EXPR for `R matching S`");
        let names: Vec<_> = bin
            .syntax()
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert_eq!(bin.op_kind(), Some(crate::ast::BinaryOp::Matching));
    }

    #[test]
    fn not_matching_infix_parses() {
        use crate::ast::AstNode;
        // The two-word `not matching` bumps both operator tokens and resolves
        // to `NotMatching` (not `Matching`) via the sibling-`not` check.
        let out = parse_str("oper main {} [ R not matching S ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
            .expect("BINARY_EXPR for `R not matching S`");
        let names: Vec<_> = bin
            .syntax()
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(names, vec!["R".to_string(), "S".to_string()]);
        assert_eq!(bin.op_kind(), Some(crate::ast::BinaryOp::NotMatching));
        // Both `not` and `matching` are captured as operator tokens, so the
        // node text round-trips the two-word spelling.
        assert!(bin.syntax().text().to_string().contains("not matching"));
    }

    /// The root BINARY_EXPR of a parsed oper body, with its resolved op kind.
    fn root_binary(src: &str) -> (crate::ast::BinaryExpr, Option<crate::ast::BinaryOp>) {
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        // The outermost (first in preorder) BINARY_EXPR is the root of the
        // expression — precedence tests read its operator.
        let bin = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
            .expect("a BINARY_EXPR");
        let op = bin.op_kind();
        (bin, op)
    }

    #[test]
    fn when_infix_parses() {
        let (_, op) = root_binary("oper main {} [ R when c ];");
        assert_eq!(op, Some(crate::ast::BinaryOp::When));
    }

    #[test]
    fn otherwise_infix_parses() {
        let (_, op) = root_binary("oper main {} [ R otherwise D ];");
        assert_eq!(op, Some(crate::ast::BinaryOp::Otherwise));
    }

    #[test]
    fn when_chain_with_otherwise_nests_left() {
        use crate::ast::AstNode;
        // Same pipeline altitude, left-associative:
        // `A when c otherwise D` = `(A when c) otherwise D`.
        let (root, op) = root_binary("oper main {} [ A when c otherwise D ];");
        assert_eq!(op, Some(crate::ast::BinaryOp::Otherwise));
        let inner = root
            .syntax()
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
            .expect("inner BINARY_EXPR");
        assert_eq!(inner.op_kind(), Some(crate::ast::BinaryOp::When));
    }

    #[test]
    fn where_then_when_groups_the_restriction_first() {
        use crate::ast::AstNode;
        // `R where x = 1 when c` = `(R where x = 1) when c`: the `where`
        // predicate parses at prec 1, so the prec-0 `when` terminates it and
        // wraps the whole restriction.
        let (root, op) = root_binary("oper main {} [ R where x = 1 when c ];");
        assert_eq!(op, Some(crate::ast::BinaryOp::When));
        let inner = root
            .syntax()
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
            .expect("inner BINARY_EXPR");
        assert_eq!(inner.op_kind(), Some(crate::ast::BinaryOp::Where));
    }

    #[test]
    fn when_condition_binds_comparisons_without_parens() {
        use crate::ast::AstNode;
        // The rhs parses at prec 1, so `=` (prec 3) binds inside the
        // condition: `R when m = "GET"` = `R when (m = "GET")`.
        let (root, op) = root_binary("oper main {} [ R when m = \"GET\" ];");
        assert_eq!(op, Some(crate::ast::BinaryOp::When));
        let inner = root
            .syntax()
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .and_then(<crate::ast::BinaryExpr as crate::ast::AstNode>::cast)
            .expect("condition BINARY_EXPR");
        assert_eq!(inner.op_kind(), Some(crate::ast::BinaryOp::Eq));
    }

    #[test]
    fn when_and_otherwise_stay_ordinary_identifiers() {
        // No reserved words: both operators are contextual — in name
        // positions (`let` bindings, named args, attribute names) they are
        // plain identifiers.
        let out = parse_str(
            "oper main {} [ let when = 1; let otherwise = 2; \
             let r = Relation { { when: 1, otherwise: 2 } }; ];",
        );
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
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
            fields[0]
                .children()
                .any(|n| n.kind() == SyntaxKind::NAME_REF),
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

    // ── Postfix sequence index `s[i]` ────────────────────────────────

    #[test]
    fn index_expr_parses() {
        let out = parse_str("oper f {} [ s[1]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ie = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::INDEX_EXPR)
            .expect("INDEX_EXPR in tree");
        // Operand is a NAME_REF; the index `1` is a nested LITERAL.
        let operand = ie.first_child().unwrap();
        assert_eq!(operand.kind(), SyntaxKind::NAME_REF);
        assert_eq!(operand.text(), "s");
        assert_eq!(ie.text(), "s[1]");
    }

    #[test]
    fn chained_index_nests() {
        // `s[0][1]` — the outer INDEX_EXPR wraps the inner one.
        let out = parse_str("oper f {} [ s[0][1]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let outer = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::INDEX_EXPR)
            .expect("INDEX_EXPR in tree");
        let inner = outer.first_child().unwrap();
        assert_eq!(inner.kind(), SyntaxKind::INDEX_EXPR);
        assert_eq!(inner.text(), "s[0]");
        assert_eq!(outer.text(), "s[0][1]");
    }

    #[test]
    fn index_missing_close_bracket_diagnoses_p0057() {
        let out = parse_str("oper f {} [ s[1; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0057"),
            "expected P0057, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn index_missing_expr_diagnoses_p0058() {
        let out = parse_str("oper f {} [ s[]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0058"),
            "expected P0058, got {:?}",
            out.diagnostics
        );
    }

    // ── `var <name> := <expr>` and the let/var operator mismatch ─────

    #[test]
    fn var_stmt_parses_to_var_stmt() {
        let out = parse_str("oper f {} [ var x := 1; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(
            out.tree
                .descendants()
                .any(|n| n.kind() == SyntaxKind::VAR_STMT),
            "expected a VAR_STMT node"
        );
    }

    #[test]
    fn var_stmt_with_annotation_parses() {
        // The `:` annotation never eats the `:` of the `:=` operator (which
        // lexes as one ASSIGN token).
        let out = parse_str("oper f {} [ var x: Integer := 1; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::VAR_STMT));
    }

    #[test]
    fn let_with_walrus_diagnoses_p0067_and_recovers() {
        let out = parse_str("oper f {} [ let x := 1; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0067"),
            "expected P0067, got {:?}",
            out.diagnostics
        );
        // Recovery consumes the `:=` and still parses the binding as a LET_STMT.
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::LET_STMT));
    }

    #[test]
    fn var_with_eq_diagnoses_p0068_and_recovers() {
        let out = parse_str("oper f {} [ var x = 1; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0068"),
            "expected P0068, got {:?}",
            out.diagnostics
        );
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::VAR_STMT));
    }

    #[test]
    fn uninitialized_var_no_annotation_parses() {
        // `var x;` — no annotation, no initializer (type inferred later).
        let out = parse_str("oper f {} [ var x; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::VAR_STMT));
    }

    #[test]
    fn uninitialized_var_with_annotation_parses() {
        let out = parse_str("oper f {} [ var x: Integer; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::VAR_STMT));
    }

    #[test]
    fn uninitialized_let_parses_for_the_typechecker_to_reject() {
        // `let x;` parses (the typechecker rejects it, T0078) — a clean semantic
        // error beats a parse error.
        let out = parse_str("oper f {} [ let x: Integer; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::LET_STMT));
    }

    // ── `if <cond> then [ … ] else [ … ]` ────────────────────────────

    #[test]
    fn if_expr_with_else_parses() {
        let out = parse_str("oper f {} [ if b then [ 1 ] else [ 2 ]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ife = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::IF_EXPR)
            .expect("IF_EXPR in tree");
        // Condition is the first child node (a NAME_REF `b`); the two arms
        // are BLOCK children.
        assert_eq!(ife.first_child().unwrap().kind(), SyntaxKind::NAME_REF);
        let blocks = ife
            .children()
            .filter(|n| n.kind() == SyntaxKind::BLOCK)
            .count();
        assert_eq!(blocks, 2, "then + else blocks");
    }

    #[test]
    fn if_expr_no_else_parses() {
        let out = parse_str("oper f {} [ if b then [ 1 ]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ife = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::IF_EXPR)
            .expect("IF_EXPR in tree");
        let blocks = ife
            .children()
            .filter(|n| n.kind() == SyntaxKind::BLOCK)
            .count();
        assert_eq!(blocks, 1, "then block only");
    }

    #[test]
    fn if_expr_nested_in_else_arm() {
        // `else [ if … then [ … ] else [ … ] ]` — the deferred `else if`.
        let out = parse_str("oper f {} [ if a then [ 1 ] else [ if b then [ 2 ] else [ 3 ] ]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let count = out
            .tree
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::IF_EXPR)
            .count();
        assert_eq!(count, 2, "outer + nested IF_EXPR");
    }

    #[test]
    fn if_expr_index_run_condition_needs_no_parens() {
        // `then` delimits, so an index-run condition parses cleanly and the
        // `[ 1 ]` block is NOT swallowed as another index of `grid`.
        let out = parse_str("oper f {} [ if grid[a][b] then [ 1 ] else [ 2 ]; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ife = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::IF_EXPR)
            .expect("IF_EXPR in tree");
        let cond = ife.first_child().unwrap();
        assert_eq!(cond.kind(), SyntaxKind::INDEX_EXPR);
        assert_eq!(cond.text(), "grid[a][b]");
        let blocks = ife
            .children()
            .filter(|n| n.kind() == SyntaxKind::BLOCK)
            .count();
        assert_eq!(blocks, 2);
    }

    #[test]
    fn if_missing_then_diagnoses_p0059() {
        let out = parse_str("oper f {} [ if b [ 1 ] else [ 2 ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0059"),
            "expected P0059, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn if_missing_then_block_diagnoses_p0060() {
        let out = parse_str("oper f {} [ if b then 1; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0060"),
            "expected P0060, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn if_missing_else_block_diagnoses_p0061() {
        let out = parse_str("oper f {} [ if b then [ 1 ] else 2; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0061"),
            "expected P0061, got {:?}",
            out.diagnostics
        );
    }

    // ── Counted `for` loop ───────────────────────────────────────────

    #[test]
    fn for_stmt_counted_parses() {
        let src = "oper f {} [ for i := 0 to 2 do [ i; ]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let fs = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FOR_STMT)
            .expect("FOR_STMT in tree");
        // The direct IDENT token children are exactly the contextual keywords
        // and the counter: `to`/`do` survive as tokens (not swallowed into a
        // bound expression), and the counter is the second IDENT.
        let idents: Vec<_> = fs
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(idents, vec!["for", "i", "to", "do"]);
        let blocks = fs
            .children()
            .filter(|n| n.kind() == SyntaxKind::BLOCK)
            .count();
        assert_eq!(blocks, 1, "one body block");
    }

    #[test]
    fn for_stmt_upper_bound_is_full_expr() {
        // `n - 1` binds as the whole upper bound (a BINARY_EXPR), not stopping
        // at `n`; `do` still delimits the body.
        let src = "oper f {} [ for i := 0 to n - 1 do [ i; ]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let fs = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FOR_STMT)
            .unwrap();
        let bin = fs
            .children()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("upper bound is a BINARY_EXPR");
        assert_eq!(bin.text(), "n - 1");
    }

    #[test]
    fn for_stmt_missing_name_diagnoses_p0062() {
        let out = parse_str("oper f {} [ for := 0 to 2 do [ i; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0062"),
            "expected P0062, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn for_stmt_missing_assign_diagnoses_p0063() {
        let out = parse_str("oper f {} [ for i 0 to 2 do [ i; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0063"),
            "expected P0063, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn for_stmt_missing_to_diagnoses_p0064() {
        let out = parse_str("oper f {} [ for i := 0 2 do [ i; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0064"),
            "expected P0064, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn for_stmt_missing_do_diagnoses_p0065() {
        let out = parse_str("oper f {} [ for i := 0 to 2 [ i; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0065"),
            "expected P0065, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn for_stmt_missing_body_bracket_diagnoses_p0066() {
        let out = parse_str("oper f {} [ for i := 0 to 2 do i; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0066"),
            "expected P0066, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn for_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper f {} [ for i := 0 to 2 do [ i ] ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn for_in_stmt_parses() {
        let src = "oper f {} [ for name in xs do [ name; ]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let fs = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FOR_STMT)
            .expect("FOR_STMT in tree");
        // The element form has no `:=` — direct IDENT tokens are `for`,
        // `name`, `in`, `do`; the iterable is the sole (NameRef) Expr child.
        let idents: Vec<_> = fs
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(idents, vec!["for", "name", "in", "do"]);
        assert!(
            fs.children_with_tokens()
                .filter_map(|e| e.into_token())
                .all(|t| t.kind() != SyntaxKind::ASSIGN),
            "the element form carries no `:=` token"
        );
        let iterable = fs
            .children()
            .find(|n| n.kind() == SyntaxKind::NAME_REF)
            .expect("iterable NAME_REF");
        assert_eq!(iterable.text(), "xs");
    }

    #[test]
    fn for_in_missing_do_diagnoses_p0065() {
        let out = parse_str("oper f {} [ for name in xs [ name; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0065"),
            "expected P0065, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn for_missing_separator_diagnoses_p0063() {
        // Neither `:=` nor `in` after the loop variable.
        let out = parse_str("oper f {} [ for name xs do [ name; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0063"),
            "expected P0063, got {:?}",
            out.diagnostics
        );
    }

    // ── `while` / `do … while` loops ──────────────────────────────────

    #[test]
    fn while_stmt_parses() {
        let src = "oper f {} [ while j < 3 do [ j; ]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let ws = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::WHILE_STMT)
            .expect("WHILE_STMT in tree");
        // Direct IDENT token children are the contextual keywords `while`/`do`.
        let idents: Vec<_> = ws
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(idents, vec!["while", "do"]);
        // The condition is a BINARY_EXPR; the body is one BLOCK.
        assert!(ws.children().any(|n| n.kind() == SyntaxKind::BINARY_EXPR));
        assert_eq!(
            ws.children()
                .filter(|n| n.kind() == SyntaxKind::BLOCK)
                .count(),
            1
        );
    }

    #[test]
    fn while_stmt_condition_is_full_expr() {
        // `j < n and j > 0` binds as the whole condition; `do` delimits the body.
        let src = "oper f {} [ while j < n and j > 0 do [ j; ]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
    }

    #[test]
    fn while_stmt_missing_do_diagnoses_p0069() {
        let out = parse_str("oper f {} [ while j < 3 [ j; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0069"),
            "expected P0069, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn while_stmt_missing_body_bracket_diagnoses_p0070() {
        let out = parse_str("oper f {} [ while j < 3 do j; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0070"),
            "expected P0070, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn while_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper f {} [ while j < 3 do [ j ] ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn do_while_stmt_parses() {
        let src = "oper f {} [ do [ k; ] while k < 3; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let dw = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::DO_WHILE_STMT)
            .expect("DO_WHILE_STMT in tree");
        let idents: Vec<_> = dw
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(idents, vec!["do", "while"]);
        assert_eq!(
            dw.children()
                .filter(|n| n.kind() == SyntaxKind::BLOCK)
                .count(),
            1
        );
        assert!(dw.children().any(|n| n.kind() == SyntaxKind::BINARY_EXPR));
    }

    #[test]
    fn do_while_stmt_missing_body_bracket_diagnoses_p0071() {
        let out = parse_str("oper f {} [ do k; while k < 3; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0071"),
            "expected P0071, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn do_stmt_without_while_diagnoses_p0072() {
        // A bare `do [ … ];` with no trailing `while` is rejected — the settled
        // "statement-leading `do` is the post-test loop" rule.
        let out = parse_str("oper f {} [ do [ k; ]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0072"),
            "expected P0072, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn do_while_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper f {} [ do [ k; ] while k < 3 ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
            out.diagnostics
        );
    }

    // ── `load` statement ─────────────────────────────────────────────

    #[test]
    fn load_stmt_parses() {
        let src = "oper f {} [ load names from rnames order [asc name]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let ls = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LOAD_STMT)
            .expect("LOAD_STMT in tree");
        // Direct IDENT token children are the contextual keywords + the target;
        // the source relvar's IDENT is nested inside its NAME_REF child.
        let idents: Vec<_> = ls
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(idents, vec!["load", "names", "from", "order"]);
        // The source is the sole (NameRef) Expr child.
        let source = ls
            .children()
            .find(|n| n.kind() == SyntaxKind::NAME_REF)
            .expect("source NAME_REF");
        assert_eq!(source.text(), "rnames");
        // One order key.
        assert_eq!(
            ls.children()
                .filter(|n| n.kind() == SyntaxKind::SORT_ITEM)
                .count(),
            1
        );
    }

    #[test]
    fn load_stmt_without_order_parses() {
        // The reverse (sequence→relvar) / unordered form carries no `order`.
        let src = "oper f {} [ load r from xs; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let ls = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::LOAD_STMT)
            .expect("LOAD_STMT in tree");
        assert_eq!(
            ls.children()
                .filter(|n| n.kind() == SyntaxKind::SORT_ITEM)
                .count(),
            0
        );
    }

    #[test]
    fn load_stmt_multi_key_order_parses() {
        let src = "oper f {} [ load s from r order [asc a, desc b]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let items: Vec<_> = out
            .tree
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SORT_ITEM)
            .collect();
        assert_eq!(items.len(), 2);
        // Each item's IDENT tokens: `[direction?, attr]`. `desc a` → desc key;
        // a bare attr → ascending.
        let ids: Vec<Vec<String>> = items
            .iter()
            .map(|n| {
                n.children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .filter(|t| t.kind() == SyntaxKind::IDENT)
                    .map(|t| t.text().to_string())
                    .collect()
            })
            .collect();
        assert_eq!(ids[0], vec!["asc", "a"]);
        assert_eq!(ids[1], vec!["desc", "b"]);
    }

    #[test]
    fn load_stmt_bare_attr_defaults_to_asc() {
        // `order [name]` — no direction keyword; one SORT_ITEM, attr `name`.
        let src = "oper f {} [ load s from r order [name]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let item = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::SORT_ITEM)
            .expect("SORT_ITEM in tree");
        let ids: Vec<_> = item
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(ids, vec!["name"]);
    }

    #[test]
    fn load_stmt_attr_named_asc_still_parses() {
        // No reserved words: an attribute literally named `asc` is the order key
        // (a direction is recognized only when another attribute IDENT follows).
        let src = "oper f {} [ load s from r order [asc]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let item = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::SORT_ITEM)
            .expect("SORT_ITEM in tree");
        let ids: Vec<_> = item
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .map(|t| t.text().to_string())
            .collect();
        // A single IDENT `asc` — the attribute, not a direction.
        assert_eq!(ids, vec!["asc"]);
    }

    #[test]
    fn load_stmt_trailing_comma_in_order_parses() {
        let src = "oper f {} [ load s from r order [asc a, desc b,]; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            out.tree
                .descendants()
                .filter(|n| n.kind() == SyntaxKind::SORT_ITEM)
                .count(),
            2
        );
    }

    #[test]
    fn load_stmt_missing_target_diagnoses_p0073() {
        let out = parse_str("oper f {} [ load 1 from r; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0073"),
            "expected P0073, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn load_stmt_missing_from_diagnoses_p0074() {
        let out = parse_str("oper f {} [ load names rnames order [asc name]; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0074"),
            "expected P0074, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn load_stmt_missing_source_diagnoses_p0014() {
        // A missing source relation is the uniform "expected expression" (P0014).
        let out = parse_str("oper f {} [ load names from ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0014"),
            "expected P0014, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn load_stmt_missing_order_bracket_diagnoses_p0075() {
        let out = parse_str("oper f {} [ load names from r order asc name; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0075"),
            "expected P0075, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn load_stmt_unterminated_order_diagnoses_p0076() {
        let out = parse_str("oper f {} [ load names from r order [asc name ; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0076"),
            "expected P0076, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn load_stmt_empty_order_diagnoses_p0077() {
        let out = parse_str("oper f {} [ load names from r order []; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0077"),
            "expected P0077, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn load_stmt_missing_semicolon_diagnoses_p0013() {
        let out = parse_str("oper f {} [ load names from r order [asc name] ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0013"),
            "expected P0013, got {:?}",
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
    fn relation_literal_expression_element_parses() {
        // A tuple-valued expression (a bare name) is a valid element — it parses
        // cleanly as a `NAME_REF` child; the tuple-typed constraint is a
        // typecheck concern (T0096), not a parse one.
        let out = parse_str("oper f {} [ let r = Relation { req }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let rel = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RELATION_LIT)
            .expect("RELATION_LIT in tree");
        let names: Vec<_> = rel
            .children()
            .filter(|n| n.kind() == SyntaxKind::NAME_REF)
            .collect();
        assert_eq!(names.len(), 1);
        // Mixed literal + expression elements also parse.
        let mixed = parse_str("oper f {} [ let r = Relation { {a: 1}, req }; ];");
        assert!(mixed.diagnostics.is_empty(), "{:?}", mixed.diagnostics);
    }

    #[test]
    fn relation_literal_non_tuple_element_parses_defers_to_typecheck() {
        // `Relation { 42 }` now parses (42 is an expression element); rejecting a
        // non-tuple element is the typechecker's job (T0096), not the parser's —
        // P0032 is retired.
        let out = parse_str("oper f {} [ let r = Relation { 42 }; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert!(out
            .tree
            .descendants()
            .any(|n| n.kind() == SyntaxKind::RELATION_LIT));
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

    #[test]
    fn relation_type_ref_nests_a_heading() {
        let src = "oper f {} [ let r: Relation { name: Text } = Relation {}; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        // The annotation is `TYPE_REF { Relation, HEADING { PARAM … } }`.
        let tr = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TYPE_REF)
            .expect("TYPE_REF in tree");
        let heading = tr
            .children()
            .find(|n| n.kind() == SyntaxKind::HEADING)
            .expect("nested HEADING child");
        assert!(heading.children().any(|n| n.kind() == SyntaxKind::PARAM));
    }

    #[test]
    fn tuple_type_ref_nests_a_heading() {
        let src = "oper f {} [ let t: Tuple { a: Integer } = {a: 1}; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
        let tr = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TYPE_REF)
            .expect("TYPE_REF in tree");
        assert!(tr.children().any(|n| n.kind() == SyntaxKind::HEADING));
    }

    #[test]
    fn relation_type_ref_heading_types_nest() {
        // A relation-valued attribute type nests a further TYPE_REF (here a
        // `Sequence Text`) inside the heading's PARAM.
        let src = "oper f {} [ let r: Relation { xs: Sequence Text } = Relation {}; ];";
        let out = parse_str(src);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.tree.text(), src);
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
        assert!(
            outer_text.contains(" or "),
            "expected `or` at top: {outer_text}"
        );
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
        assert!(re.children().any(|n| n.kind() == SyntaxKind::EXTEND_EXPR));
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
        let out = parse_str(
            "oper f {} [ let wrap = 1; let unwrap = 2; let s = R where wrap = unwrap; ];",
        );
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    // ── group / ungroup ───────────────────────────────────────────────

    #[test]
    fn group_parses_as_group_expr_with_pairs() {
        let out = parse_str("oper f {} [ let s = R group {pq: {a, b}, r: {c}}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ge = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::GROUP_EXPR)
            .expect("GROUP_EXPR in tree");
        assert!(ge.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
        let pairs = ge
            .children()
            .filter(|n| n.kind() == SyntaxKind::GROUP_PAIR)
            .count();
        assert_eq!(pairs, 2, "two GROUP_PAIR nodes");
    }

    #[test]
    fn ungroup_parses_as_ungroup_expr() {
        let out = parse_str("oper f {} [ let s = R ungroup {pq, r}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ue = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UNGROUP_EXPR)
            .expect("UNGROUP_EXPR in tree");
        assert!(ue.children().any(|n| n.kind() == SyntaxKind::NAME_REF));
    }

    #[test]
    fn ungroup_interleaves_with_group() {
        // `R group {pq: {a, b}} ungroup {pq}` nests left: UNGROUP(GROUP(R)).
        let out = parse_str("oper f {} [ let s = R group {pq: {a, b}} ungroup {pq}; ];");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let ue = out
            .tree
            .descendants()
            .find(|n| n.kind() == SyntaxKind::UNGROUP_EXPR)
            .expect("UNGROUP_EXPR at the top");
        assert!(ue.children().any(|n| n.kind() == SyntaxKind::GROUP_EXPR));
    }

    #[test]
    fn group_missing_outer_brace_diagnoses_p0032() {
        let out = parse_str("oper f {} [ let s = R group a; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0032"),
            "expected P0032, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn group_missing_pair_name_diagnoses_p0087() {
        let out = parse_str("oper f {} [ let s = R group {: {a}}; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0087"),
            "expected P0087, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn group_missing_colon_diagnoses_p0088() {
        let out = parse_str("oper f {} [ let s = R group {pq {a}}; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0088"),
            "expected P0088, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn group_missing_inner_brace_diagnoses_p0089() {
        let out = parse_str("oper f {} [ let s = R group {pq: a}; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0089"),
            "expected P0089, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn ungroup_missing_brace_diagnoses_p0093() {
        let out = parse_str("oper f {} [ let s = R ungroup pq; ];");
        assert!(
            out.diagnostics.iter().any(|d| d.code == "P0093"),
            "expected P0093, got {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn group_ungroup_are_contextual_not_reserved() {
        let out = parse_str(
            "oper f {} [ let group = 1; let ungroup = 2; let s = R where group = ungroup; ];",
        );
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
        assert!(re.children().any(|n| n.kind() == SyntaxKind::EXTEND_EXPR));
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
    fn builtin_oper_decl_parses() {
        let out = parse_str("builtin oper to_text { self: Integer } -> Text;");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            out.tree.text(),
            "builtin oper to_text { self: Integer } -> Text;"
        );

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
                SyntaxKind::IDENT, // "builtin"
                SyntaxKind::IDENT, // "oper"
                SyntaxKind::IDENT, // "to_text"
                SyntaxKind::HEADING,
                SyntaxKind::RETURN_CLAUSE,
                SyntaxKind::SEMICOLON,
            ]
        );
    }

    #[test]
    fn builtin_oper_unit_return_parses() {
        let out = parse_str("builtin oper write_line { message: Text };");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    #[test]
    fn builtin_oper_with_body_diagnoses_p0078() {
        let out = parse_str("builtin oper f {} [];");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0078"));
    }

    #[test]
    fn builtin_without_oper_diagnoses_p0079() {
        let out = parse_str("builtin f {};");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0079"));
    }

    #[test]
    fn type_decl_parses() {
        let out = parse_str("type Request = Tuple { method: Text };");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let decl = out.tree.first_child().unwrap();
        assert_eq!(decl.kind(), SyntaxKind::TYPE_DECL);
    }

    #[test]
    fn type_decl_possrep_form_parses() {
        // The possrep-scalar form `type Name { component: Type }` parses clean,
        // with the component list as a *direct* HEADING child (not a TYPE_REF).
        let out = parse_str("type RawRequestPath { value: Text };");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let decl = out.tree.first_child().unwrap();
        assert_eq!(decl.kind(), SyntaxKind::TYPE_DECL);
        assert!(decl.children().any(|c| c.kind() == SyntaxKind::HEADING));
        assert!(!decl.children().any(|c| c.kind() == SyntaxKind::TYPE_REF));
    }

    #[test]
    fn type_decl_missing_name_diagnoses_p0080() {
        let out = parse_str("type = Integer;");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0080"));
    }

    #[test]
    fn type_decl_missing_eq_diagnoses_p0081() {
        let out = parse_str("type Foo Integer;");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0081"));
    }

    #[test]
    fn type_decl_missing_semicolon_diagnoses_p0082() {
        let out = parse_str("type Foo = Integer");
        assert!(out.diagnostics.iter().any(|d| d.code == "P0082"));
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

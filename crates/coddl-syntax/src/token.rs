//! Tokens — the lexer's output type.
//!
//! Both [`Token`] and [`TokenKind`] are plain data. `Token` is a fixed-size
//! record (`kind` + `span`); the lexeme itself is recoverable from the
//! source buffer by indexing with the span, so we don't carry a copy on
//! the token. `TokenKind` is a flat enum: every variant is unit, no
//! payloads. This makes both types directly expressible in Coddl once
//! sum types land — the Coddl shape would be `Tuple { kind: TokenKind,
//! span: Span }` and a `TokenKind` sum type with one nullary variant per
//! Rust variant here.

use coddl_diagnostics::Span;

/// One lexer-produced token. The lexeme (the actual source bytes) is
/// recoverable from `span`; we don't store a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub const fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// The lexical kind of a token. Flat enum — no payloads.
///
/// The lexer normalizes Unicode operator-glyph synonyms (`⋈`, `≤`, `⊆`,
/// etc.) to the same `TokenKind` as their ASCII counterparts (`Ident` for
/// `⋈`/`join`, `LtEq` for `≤`/`⊆`/`<=`, …). The CST keeps the original
/// byte range so the formatter can reproduce or normalize per
/// `format.edition`.
///
/// Trivia (`Whitespace`, `LineComment`, `BlockComment`) is emitted by the
/// lexer for the CST builder to attach. The parser sees the non-trivia
/// stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TokenKind {
    // ── identifiers & literals ───────────────────────────────────────────
    /// A UAX #31 identifier — alphabetic / digit / underscore characters.
    /// Reserved-keyword recognition happens contextually in the parser.
    Ident,
    /// `42`, `0xff`, `0b1010`, `0o17`, `0d99` — see §3 "Literals".
    IntegerLit,
    /// `42.0`, `3.14` — digits-dot-digits.
    RationalLit,
    /// `42e0`, `4.2e1`, `1e-9` — mantissa-with-exponent.
    ApproximateLit,
    /// `"hello"` — double-quoted text. Includes the quotes in the span.
    StringLit,
    /// `f"hello, {name}!"` — format-string literal. The `f` is fused to
    /// the opening quote (no space); only the adjacency `f"` triggers it,
    /// so `f` stays an ordinary identifier everywhere else. Lexically the
    /// same body as `StringLit` (the lexer doesn't validate placeholders);
    /// the `{…}` placeholder structure is interpreted later. Its type is
    /// `FormatText` — see `docs/typecheck.md`.
    FormatStringLit,
    /// `'a'`, `'\n'`, `'\u{1F600}'` — single-codepoint character literal.
    CharLit,

    // ── punctuation ──────────────────────────────────────────────────────
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `;`
    Semicolon,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `.`
    Dot,
    /// `:=`
    Assign,
    /// `->`  — operator return-type clause, future infix arrow.
    Arrow,

    // ── comparison ───────────────────────────────────────────────────────
    /// `=`
    Eq,
    /// `<>`  (also `≠`)
    NotEq,
    /// `<`  (also `⊂` in relational position)
    Lt,
    /// `>`  (also `⊃`)
    Gt,
    /// `<=`  (also `≤`, `⊆`)
    LtEq,
    /// `>=`  (also `≥`, `⊇`)
    GtEq,

    // ── arithmetic ───────────────────────────────────────────────────────
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `||`  (text/character concatenation)
    PipePipe,

    // ── trivia (kept for CST) ────────────────────────────────────────────
    /// Run of `XID_Continue` / Unicode `White_Space`. Discarded by the
    /// parser; attached to the CST for the formatter.
    Whitespace,
    /// `// …` to end of line.
    LineComment,
    /// `/* … */`, possibly nested. One token per outermost block —
    /// the lexer tracks nesting depth internally.
    BlockComment,

    // ── special ──────────────────────────────────────────────────────────
    /// End of input.
    Eof,
    /// Unrecognized input — the lexer emits this with a diagnostic and
    /// continues. Lets the parser keep recovering rather than bailing.
    Error,
}

impl TokenKind {
    /// True for `Whitespace`, `LineComment`, `BlockComment` — the kinds
    /// the parser skips by default but the CST preserves.
    pub const fn is_trivia(self) -> bool {
        matches!(
            self,
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
        )
    }
}

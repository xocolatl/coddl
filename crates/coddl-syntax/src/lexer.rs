//! Tokenizer for Coddl source.
//!
//! The lexer is a hand-rolled state machine over a UTF-8 byte cursor.
//! Two cases in particular shape that choice: nested block comments
//! (depth counting) and the three numeric literal shapes (integer,
//! rational, approximate — disambiguated by what follows the digits).
//! Both translate directly to a small `Lexer` struct with a single
//! cursor and a few helpers.
//!
//! ## Contract
//!
//! - Input: a `&str` source buffer and a `FileId`.
//! - Output: a [`LexOutput`] with the token sequence and any
//!   diagnostics. Both are vectors; no streams in the public API.
//! - The lexer **never panics** on any byte sequence. Unknown
//!   characters produce a `TokenKind::Error` token plus an error
//!   diagnostic, and the lexer continues.
//! - Spans are byte offsets, end-exclusive. The lexeme of a token is
//!   `source[token.span.start..token.span.end]`.
//! - Whitespace and comments are emitted as first-class tokens — the
//!   parser skips them, the syntax tree keeps them.

use coddl_diagnostics::{Diagnostic, FileId, Span};
use unicode_ident::{is_xid_continue, is_xid_start};

use crate::token::{Token, TokenKind};

/// Result of one lex pass: every token in order, plus every diagnostic.
#[derive(Debug, Default)]
pub struct LexOutput {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Tokenize `source` and return the result.
///
/// The returned token list ends with a single `TokenKind::Eof` whose span
/// is the zero-length range at `source.len()`. Trivia (whitespace,
/// comments) is included in order with the other tokens.
pub fn lex(source: &str, file: FileId) -> LexOutput {
    let mut lx = Lexer {
        source,
        file,
        pos: 0,
        tokens: Vec::new(),
        diagnostics: Vec::new(),
    };
    lx.run();
    LexOutput {
        tokens: lx.tokens,
        diagnostics: lx.diagnostics,
    }
}

struct Lexer<'a> {
    source: &'a str,
    file: FileId,
    /// Byte cursor into `source`. Always on a UTF-8 boundary.
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn run(&mut self) {
        while self.pos < self.source.len() {
            self.next_token();
        }
        let end = self.source.len() as u32;
        self.tokens.push(Token::new(
            TokenKind::Eof,
            Span::new(self.file, end, end),
        ));
    }

    // ── cursor helpers ───────────────────────────────────────────────────

    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn peek_n(&self, n: usize) -> Option<char> {
        self.source[self.pos..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn bump_while<F: Fn(char) -> bool>(&mut self, pred: F) {
        while let Some(c) = self.peek() {
            if pred(c) {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn span(&self, start: usize) -> Span {
        Span::new(self.file, start as u32, self.pos as u32)
    }

    fn emit(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token::new(kind, self.span(start)));
    }

    fn diag(&mut self, span: Span, code: &'static str, msg: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(span, code, msg));
    }

    // ── dispatch ─────────────────────────────────────────────────────────

    fn next_token(&mut self) {
        let start = self.pos;
        let c = match self.peek() {
            Some(c) => c,
            None => return,
        };

        match c {
            // whitespace
            c if c.is_whitespace() => self.lex_whitespace(start),

            // comment-or-slash
            '/' => match self.peek_n(1) {
                Some('/') => self.lex_line_comment(start),
                Some('*') => self.lex_block_comment(start),
                _ => self.lex_single(TokenKind::Slash, start),
            },

            // string and char literals
            '"' => self.lex_string(start),
            '\'' => self.lex_char(start),

            // numeric literals
            c if c.is_ascii_digit() => self.lex_number(start),

            // identifiers (`_` is XID_Continue but not XID_Start; we
            // accept it as a start char and apply the `__` rule below)
            c if is_xid_start(c) || c == '_' => self.lex_ident(start),

            // punctuation
            '{' => self.lex_single(TokenKind::LBrace, start),
            '}' => self.lex_single(TokenKind::RBrace, start),
            '[' => self.lex_single(TokenKind::LBracket, start),
            ']' => self.lex_single(TokenKind::RBracket, start),
            '(' => self.lex_single(TokenKind::LParen, start),
            ')' => self.lex_single(TokenKind::RParen, start),
            ';' => self.lex_single(TokenKind::Semicolon, start),
            ',' => self.lex_single(TokenKind::Comma, start),
            '.' => self.lex_single(TokenKind::Dot, start),
            '+' => self.lex_single(TokenKind::Plus, start),
            '-' => self.lex_single(TokenKind::Minus, start),
            '*' => self.lex_single(TokenKind::Star, start),
            '=' => self.lex_single(TokenKind::Eq, start),

            // single-or-multi-char operators
            ':' => self.lex_colon(start),
            '<' => self.lex_lt(start),
            '>' => self.lex_gt(start),

            // Unicode glyph synonyms (§3 "Unicode operator glyphs")
            '⋈' | '∪' | '∩' | '∖' => self.lex_word_glyph(start),
            '≤' | '⊆' => self.lex_glyph(TokenKind::LtEq, start),
            '≥' | '⊇' => self.lex_glyph(TokenKind::GtEq, start),
            '⊂' => self.lex_glyph(TokenKind::Lt, start),
            '⊃' => self.lex_glyph(TokenKind::Gt, start),
            '≠' => self.lex_glyph(TokenKind::NotEq, start),

            // anything else
            other => {
                self.bump();
                let span = self.span(start);
                self.diag(span, "E0001", format!("unexpected character {other:?}"));
                self.emit(TokenKind::Error, start);
            }
        }
    }

    fn lex_single(&mut self, kind: TokenKind, start: usize) {
        self.bump();
        self.emit(kind, start);
    }

    fn lex_glyph(&mut self, kind: TokenKind, start: usize) {
        self.bump();
        self.emit(kind, start);
    }

    // ── one-character-then-decide ────────────────────────────────────────

    fn lex_colon(&mut self, start: usize) {
        self.bump(); // ':'
        if self.peek() == Some('=') {
            self.bump();
            self.emit(TokenKind::Assign, start);
        } else {
            self.emit(TokenKind::Colon, start);
        }
    }

    fn lex_lt(&mut self, start: usize) {
        self.bump(); // '<'
        match self.peek() {
            Some('=') => {
                self.bump();
                self.emit(TokenKind::LtEq, start);
            }
            Some('>') => {
                self.bump();
                self.emit(TokenKind::NotEq, start);
            }
            _ => self.emit(TokenKind::Lt, start),
        }
    }

    fn lex_gt(&mut self, start: usize) {
        self.bump(); // '>'
        if self.peek() == Some('=') {
            self.bump();
            self.emit(TokenKind::GtEq, start);
        } else {
            self.emit(TokenKind::Gt, start);
        }
    }

    // ── trivia ───────────────────────────────────────────────────────────

    fn lex_whitespace(&mut self, start: usize) {
        self.bump_while(char::is_whitespace);
        self.emit(TokenKind::Whitespace, start);
    }

    fn lex_line_comment(&mut self, start: usize) {
        self.bump(); // '/'
        self.bump(); // '/'
        self.bump_while(|c| c != '\n');
        self.emit(TokenKind::LineComment, start);
    }

    /// `/* … */` with arbitrary nesting. Depth counter — see §3 "Comments".
    /// An unterminated comment runs to end of input and emits a diagnostic.
    fn lex_block_comment(&mut self, start: usize) {
        self.bump(); // '/'
        self.bump(); // '*'
        let mut depth: u32 = 1;
        while depth > 0 {
            match self.peek() {
                None => {
                    let span = self.span(start);
                    self.diag(span, "E0002", "unterminated /* block comment");
                    break;
                }
                Some('/') if self.peek_n(1) == Some('*') => {
                    self.bump();
                    self.bump();
                    depth += 1;
                }
                Some('*') if self.peek_n(1) == Some('/') => {
                    self.bump();
                    self.bump();
                    depth -= 1;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
        self.emit(TokenKind::BlockComment, start);
    }

    // ── string / character literals ──────────────────────────────────────

    fn lex_string(&mut self, start: usize) {
        self.bump(); // opening '"'
        loop {
            match self.peek() {
                None => {
                    let span = self.span(start);
                    self.diag(span, "E0003", "unterminated string literal");
                    self.emit(TokenKind::StringLit, start);
                    return;
                }
                Some('"') => {
                    self.bump(); // closing '"'
                    self.emit(TokenKind::StringLit, start);
                    return;
                }
                Some('\\') => {
                    // consume backslash + one following char unconditionally
                    // so that '\"' and '\\' don't end the string at the
                    // wrong place. The parser does the actual escape
                    // validation; here we just consume the bytes.
                    self.bump(); // '\\'
                    if self.bump().is_none() {
                        let span = self.span(start);
                        self.diag(span, "E0003", "unterminated string literal");
                        self.emit(TokenKind::StringLit, start);
                        return;
                    }
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn lex_char(&mut self, start: usize) {
        self.bump(); // opening '\''
        // Empty literal: '' — emit Error, parser will deal with it.
        if self.peek() == Some('\'') {
            self.bump();
            let span = self.span(start);
            self.diag(span, "E0004", "empty character literal");
            self.emit(TokenKind::CharLit, start);
            return;
        }
        // One codepoint, possibly an escape.
        match self.peek() {
            None => {
                let span = self.span(start);
                self.diag(span, "E0005", "unterminated character literal");
                self.emit(TokenKind::CharLit, start);
                return;
            }
            Some('\\') => {
                self.bump(); // '\\'
                if self.bump().is_none() {
                    let span = self.span(start);
                    self.diag(span, "E0005", "unterminated character literal");
                    self.emit(TokenKind::CharLit, start);
                    return;
                }
                // For \u{HHHHHH} we keep consuming until '}'; the parser
                // checks the contents.
                if self.tokens_ends_with_brace_escape() {
                    self.bump_while(|c| c != '}' && c != '\'');
                    if self.peek() == Some('}') {
                        self.bump();
                    }
                }
            }
            Some(_) => {
                self.bump();
            }
        }
        // Expect the closing quote.
        if self.peek() == Some('\'') {
            self.bump();
            self.emit(TokenKind::CharLit, start);
        } else {
            // Too many characters: consume until quote or newline to limit damage.
            let extra_start = self.pos;
            self.bump_while(|c| c != '\'' && c != '\n');
            let bad_span = Span::new(self.file, extra_start as u32, self.pos as u32);
            self.diag(bad_span, "E0006", "character literal must contain exactly one codepoint");
            if self.peek() == Some('\'') {
                self.bump();
            }
            self.emit(TokenKind::CharLit, start);
        }
    }

    /// True iff the byte just consumed before the cursor was `{` and the
    /// one before *that* was `u` after a backslash — i.e. we're inside a
    /// `\u{…}` escape and the next thing to scan is the hex run + `}`.
    /// Bit of a hack: the cleaner solution is to thread escape-mode
    /// through the loop, but for the v0 lexer we just look back.
    fn tokens_ends_with_brace_escape(&self) -> bool {
        let s = &self.source[..self.pos];
        s.ends_with("u{")
    }

    // ── numeric literals ─────────────────────────────────────────────────

    fn lex_number(&mut self, start: usize) {
        // Base prefixes — `0x`, `0b`, `0o`, `0d` (all case-insensitive).
        if self.peek() == Some('0') {
            match self.peek_n(1) {
                Some('x') | Some('X') => return self.lex_int_base(start, is_hex_digit),
                Some('b') | Some('B') => return self.lex_int_base(start, is_bin_digit),
                Some('o') | Some('O') => return self.lex_int_base(start, is_oct_digit),
                Some('d') | Some('D') => return self.lex_int_base(start, is_dec_digit),
                _ => {}
            }
        }
        // Decimal digit run.
        self.bump_while(|c| is_dec_digit(c) || c == '_');

        // Distinguish Integer / Rational / Approximate by what follows.
        match self.peek() {
            Some('.') if self.peek_n(1).map_or(false, is_dec_digit) => {
                // Rational: digits '.' digits …
                self.bump(); // '.'
                self.bump_while(|c| is_dec_digit(c) || c == '_');
                // Optional exponent → Approximate instead.
                if self.try_lex_exponent() {
                    self.emit(TokenKind::ApproximateLit, start);
                } else {
                    self.emit(TokenKind::RationalLit, start);
                }
            }
            Some('e') | Some('E') if self.exponent_follows() => {
                self.try_lex_exponent();
                self.emit(TokenKind::ApproximateLit, start);
            }
            _ => self.emit(TokenKind::IntegerLit, start),
        }
    }

    fn lex_int_base<F: Fn(char) -> bool>(&mut self, start: usize, digit: F) {
        self.bump(); // '0'
        self.bump(); // base letter
        self.bump_while(|c| digit(c) || c == '_');
        self.emit(TokenKind::IntegerLit, start);
    }

    /// Returns true and consumes the exponent (`[eE][+-]?digits`) if one
    /// is present, false otherwise. Caller must have already confirmed
    /// the shape via `exponent_follows`.
    fn try_lex_exponent(&mut self) -> bool {
        if !self.exponent_follows() {
            return false;
        }
        self.bump(); // 'e' / 'E'
        if matches!(self.peek(), Some('+') | Some('-')) {
            self.bump();
        }
        self.bump_while(|c| is_dec_digit(c) || c == '_');
        true
    }

    /// `[eE][+-]?digit` (lookahead-only, no consumption).
    fn exponent_follows(&self) -> bool {
        let Some(c0) = self.peek() else { return false };
        if c0 != 'e' && c0 != 'E' {
            return false;
        }
        let mut i = 1;
        if matches!(self.peek_n(i), Some('+') | Some('-')) {
            i += 1;
        }
        self.peek_n(i).map_or(false, is_dec_digit)
    }

    // ── identifiers and word-glyph operators ─────────────────────────────

    fn lex_ident(&mut self, start: usize) {
        // First char already known to be XID_Start or '_'.
        self.bump();
        self.bump_while(|c| is_xid_continue(c) || c == '_');

        // §3 "Identifier shape": leading `__` is reserved for compiler
        // internals — reject it from user source.
        let lexeme = &self.source[start..self.pos];
        if lexeme.starts_with("__") {
            let span = self.span(start);
            self.diag(
                span,
                "E0007",
                "identifiers may not start with `__` (reserved for compiler-internal names)",
            );
        }
        self.emit(TokenKind::Ident, start);
    }

    /// A single-codepoint Unicode word operator (`⋈ ∪ ∩ ∖`) emitted as an
    /// `Ident` token — the parser resolves it to its canonical word
    /// (`join`, `union`, `intersect`, `minus`) at the same recognition
    /// site as the ASCII spelling.
    fn lex_word_glyph(&mut self, start: usize) {
        self.bump();
        self.emit(TokenKind::Ident, start);
    }
}

// ── digit predicates (no Unicode-friendly `is_*` for binary/octal) ───────

fn is_dec_digit(c: char) -> bool {
    matches!(c, '0'..='9')
}
fn is_bin_digit(c: char) -> bool {
    matches!(c, '0' | '1')
}
fn is_oct_digit(c: char) -> bool {
    matches!(c, '0'..='7')
}
fn is_hex_digit(c: char) -> bool {
    matches!(c, '0'..='9' | 'a'..='f' | 'A'..='F')
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_diagnostics::FileId;

    fn lex_kinds(source: &str) -> Vec<TokenKind> {
        lex(source, FileId(0))
            .tokens
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !k.is_trivia()) // strip whitespace/comments for clarity
            .collect()
    }

    fn lex_all(source: &str) -> LexOutput {
        lex(source, FileId(0))
    }

    #[test]
    fn empty_input_is_just_eof() {
        assert_eq!(lex_kinds(""), vec![TokenKind::Eof]);
    }

    #[test]
    fn whitespace_only_is_one_whitespace_then_eof() {
        let out = lex_all("   \n\t  ");
        let kinds: Vec<_> = out.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, vec![TokenKind::Whitespace, TokenKind::Eof]);
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn line_comment_swallows_to_newline() {
        let out = lex_all("// hi there\n");
        let kinds: Vec<_> = out.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![TokenKind::LineComment, TokenKind::Whitespace, TokenKind::Eof]
        );
    }

    #[test]
    fn block_comment_nests() {
        let src = "/* outer /* inner */ still outer */";
        let kinds: Vec<_> = lex_all(src)
            .tokens
            .iter()
            .map(|t| t.kind)
            .collect();
        assert_eq!(kinds, vec![TokenKind::BlockComment, TokenKind::Eof]);
    }

    #[test]
    fn unterminated_block_comment_is_diagnosed() {
        let out = lex_all("/* never closed");
        assert!(out.diagnostics.iter().any(|d| d.code == "E0002"));
    }

    #[test]
    fn string_with_escapes() {
        let kinds = lex_kinds(r#""hello \"world\"!""#);
        assert_eq!(kinds, vec![TokenKind::StringLit, TokenKind::Eof]);
    }

    #[test]
    fn unterminated_string_diagnoses_and_emits_token() {
        let out = lex_all(r#""abc"#);
        assert_eq!(out.tokens[0].kind, TokenKind::StringLit);
        assert!(out.diagnostics.iter().any(|d| d.code == "E0003"));
    }

    #[test]
    fn char_literal_simple() {
        assert_eq!(lex_kinds("'a'"), vec![TokenKind::CharLit, TokenKind::Eof]);
    }

    #[test]
    fn char_literal_with_escape() {
        assert_eq!(
            lex_kinds(r"'\n'"),
            vec![TokenKind::CharLit, TokenKind::Eof]
        );
    }

    #[test]
    fn char_literal_unicode_escape() {
        assert_eq!(
            lex_kinds(r"'\u{1F600}'"),
            vec![TokenKind::CharLit, TokenKind::Eof]
        );
    }

    #[test]
    fn empty_char_literal_is_diagnosed() {
        let out = lex_all("''");
        assert!(out.diagnostics.iter().any(|d| d.code == "E0004"));
    }

    #[test]
    fn multi_codepoint_char_literal_is_diagnosed() {
        let out = lex_all("'ab'");
        assert!(out.diagnostics.iter().any(|d| d.code == "E0006"));
    }

    #[test]
    fn integer_literal_decimal() {
        assert_eq!(lex_kinds("42"), vec![TokenKind::IntegerLit, TokenKind::Eof]);
    }

    #[test]
    fn integer_literal_underscores() {
        assert_eq!(
            lex_kinds("1_000_000"),
            vec![TokenKind::IntegerLit, TokenKind::Eof]
        );
    }

    #[test]
    fn integer_literal_hex() {
        assert_eq!(
            lex_kinds("0xff_ff"),
            vec![TokenKind::IntegerLit, TokenKind::Eof]
        );
    }

    #[test]
    fn integer_literal_binary() {
        assert_eq!(
            lex_kinds("0b1010"),
            vec![TokenKind::IntegerLit, TokenKind::Eof]
        );
    }

    #[test]
    fn integer_literal_octal() {
        assert_eq!(
            lex_kinds("0o17"),
            vec![TokenKind::IntegerLit, TokenKind::Eof]
        );
    }

    #[test]
    fn rational_literal() {
        assert_eq!(
            lex_kinds("3.14"),
            vec![TokenKind::RationalLit, TokenKind::Eof]
        );
    }

    #[test]
    fn approximate_literal_with_dot() {
        assert_eq!(
            lex_kinds("4.2e1"),
            vec![TokenKind::ApproximateLit, TokenKind::Eof]
        );
    }

    #[test]
    fn approximate_literal_without_dot() {
        assert_eq!(
            lex_kinds("42e0"),
            vec![TokenKind::ApproximateLit, TokenKind::Eof]
        );
    }

    #[test]
    fn approximate_literal_negative_exponent() {
        assert_eq!(
            lex_kinds("1e-9"),
            vec![TokenKind::ApproximateLit, TokenKind::Eof]
        );
    }

    #[test]
    fn identifier_snake_case() {
        assert_eq!(
            lex_kinds("hello_world"),
            vec![TokenKind::Ident, TokenKind::Eof]
        );
    }

    #[test]
    fn identifier_with_leading_single_underscore_is_ok() {
        let out = lex_all("_unused");
        assert!(out.diagnostics.is_empty());
        assert_eq!(out.tokens[0].kind, TokenKind::Ident);
    }

    #[test]
    fn identifier_with_leading_double_underscore_is_diagnosed() {
        let out = lex_all("__internal");
        assert_eq!(out.tokens[0].kind, TokenKind::Ident);
        assert!(out.diagnostics.iter().any(|d| d.code == "E0007"));
    }

    #[test]
    fn punctuation_set() {
        let kinds = lex_kinds("{}[](),;.:");
        use TokenKind::*;
        assert_eq!(
            kinds,
            vec![
                LBrace, RBrace, LBracket, RBracket, LParen, RParen, Comma, Semicolon, Dot, Colon,
                Eof,
            ]
        );
    }

    #[test]
    fn assign_vs_colon() {
        assert_eq!(
            lex_kinds(":="),
            vec![TokenKind::Assign, TokenKind::Eof]
        );
        assert_eq!(lex_kinds(":"), vec![TokenKind::Colon, TokenKind::Eof]);
    }

    #[test]
    fn comparison_operators() {
        use TokenKind::*;
        assert_eq!(
            lex_kinds("= <> < > <= >="),
            vec![Eq, NotEq, Lt, Gt, LtEq, GtEq, Eof]
        );
    }

    #[test]
    fn arithmetic_operators() {
        use TokenKind::*;
        assert_eq!(lex_kinds("+ - * /"), vec![Plus, Minus, Star, Slash, Eof]);
    }

    #[test]
    fn unicode_glyph_synonyms_lex_to_canonical_tokens() {
        use TokenKind::*;
        // ⋈ ∪ ∩ ∖ emit Ident; ≤ ⊆ → LtEq; ⊂ → Lt; ≠ → NotEq
        let out = lex_all("⋈ ∪ ∩ ∖ ≤ ⊆ ≥ ⊇ ⊂ ⊃ ≠");
        let kinds: Vec<_> = out
            .tokens
            .iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| t.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![Ident, Ident, Ident, Ident, LtEq, LtEq, GtEq, GtEq, Lt, Gt, NotEq, Eof]
        );
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn hello_world_lexes_clean() {
        let src = "program hello_world;\n\
                   \n\
                   oper main {}\n\
                   [\n\
                       write_line{message: \"Hello, world!\"};\n\
                   ];\n";
        let out = lex_all(src);
        assert!(out.diagnostics.is_empty(), "expected no diagnostics, got {:?}", out.diagnostics);

        // Strip trivia for readability.
        use TokenKind::*;
        let kinds: Vec<_> = out
            .tokens
            .iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| t.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                Ident,   // program
                Ident,   // hello_world
                Semicolon,
                Ident,   // oper
                Ident,   // main
                LBrace,
                RBrace,
                LBracket,
                Ident,   // write_line
                LBrace,
                Ident,   // message
                Colon,
                StringLit,
                RBrace,
                Semicolon,
                RBracket,
                Semicolon,
                Eof,
            ]
        );
    }
}

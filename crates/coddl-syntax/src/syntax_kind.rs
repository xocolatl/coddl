//! The kind tag for every node and token in the Coddl syntax tree.
//!
//! `SyntaxKind` is a single flat `#[repr(u16)]` enum carrying both
//! token kinds (terminals) and node kinds (nonterminals). The lexer
//! produces only the token half; the parser combines tokens into the
//! node half. Keeping both in one enum is what `rowan`-style green
//! trees expect.

use crate::token::TokenKind;

/// Every distinct shape of node and token in a Coddl syntax tree.
///
/// The `#[repr(u16)]` is load-bearing: `rowan` stores kinds as raw
/// `u16`s, so the discriminant values must be stable and round-trip
/// through `kind_to_raw` / `kind_from_raw`. Add new variants at the
/// end of the enum (or at the end of each section) to keep prefixes
/// stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    // ── Tokens ───────────────────────────────────────────────────────
    // One variant per `TokenKind`. The `From<TokenKind>` impl below
    // maintains the 1:1 mapping.
    IDENT,
    INTEGER_LIT,
    RATIONAL_LIT,
    APPROXIMATE_LIT,
    STRING_LIT,
    CHAR_LIT,

    L_BRACE,
    R_BRACE,
    L_BRACKET,
    R_BRACKET,
    L_PAREN,
    R_PAREN,
    SEMICOLON,
    COMMA,
    COLON,
    DOT,
    ASSIGN,

    EQ,
    NOT_EQ,
    LT,
    GT,
    LT_EQ,
    GT_EQ,

    PLUS,
    MINUS,
    STAR,
    SLASH,

    WHITESPACE,
    LINE_COMMENT,
    BLOCK_COMMENT,

    EOF,
    LEX_ERROR,

    // ── Nodes ────────────────────────────────────────────────────────
    /// The whole file. Wraps every top-level item.
    ROOT,

    /// `program <name>;` declaration. Same shape covers `library` and
    /// `module` when those land.
    PROGRAM_DECL,
    /// `oper <name> <heading> [: <type>] <body>;`.
    OPER_DECL,
    /// `type <name> = …;` (sum type, type alias, possrep-scalar type).
    TYPE_DECL,
    /// `relvar <kind> <name> <heading> [ key { … } ];`.
    RELVAR_DECL,
    /// `constraint <name>: <expr>;`.
    CONSTRAINT_DECL,

    /// `{ name: type, … }` — the structural type used as both a
    /// `Tuple H` heading and an `oper` parameter list.
    HEADING,
    /// `name: type` pair inside a `HEADING`.
    PARAM,
    /// A type expression (a `NAME_REF`, or a type-generator application
    /// like `Sequence T`, or `Tuple H` / `Relation H`).
    TYPE_REF,

    /// `[ … ]` ordered statement sequence used as an operator body or
    /// other block body.
    BLOCK,

    // Statements.
    LET_STMT,
    MUT_STMT,
    ASSIGN_STMT,
    INSERT_STMT,
    DELETE_STMT,
    UPDATE_STMT,
    RETURN_STMT,
    EXPR_STMT,

    // Expressions.
    LITERAL,
    NAME_REF,
    PAREN_EXPR,
    CALL_EXPR,
    FIELD_ACCESS,
    INDEX_EXPR,
    UNARY_EXPR,
    BINARY_EXPR,
    IF_EXPR,
    MATCH_EXPR,
    MATCH_ARM,
    WHILE_EXPR,
    DO_EXPR,
    TRANSACTION_EXPR,

    TUPLE_LIT,
    RELATION_LIT,
    SEQUENCE_LIT,

    /// `{ name: value, name: value, … }` named-argument list at a call
    /// site. Distinguished from `HEADING` (types) and `TUPLE_LIT`
    /// (anonymous tuple values) by the surrounding production.
    ARG_LIST,
    NAMED_ARG,

    /// A range of source whose intended structure couldn't be
    /// recovered. The parser still wraps the tokens so the tree stays
    /// well-formed and downstream passes can keep going.
    PARSE_ERROR,
}

impl SyntaxKind {
    /// True for whitespace, line, and block comments — the kinds the
    /// parser skips by default but the syntax tree keeps.
    pub const fn is_trivia(self) -> bool {
        matches!(
            self,
            SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT
        )
    }

    /// True if this kind originates from the lexer (a terminal) rather
    /// than the parser (a nonterminal). Useful when traversing a green
    /// tree to ask "is this an interior node or a leaf?".
    pub const fn is_token(self) -> bool {
        (self as u16) <= (SyntaxKind::LEX_ERROR as u16)
    }
}

impl From<TokenKind> for SyntaxKind {
    fn from(tk: TokenKind) -> Self {
        match tk {
            TokenKind::Ident => SyntaxKind::IDENT,
            TokenKind::IntegerLit => SyntaxKind::INTEGER_LIT,
            TokenKind::RationalLit => SyntaxKind::RATIONAL_LIT,
            TokenKind::ApproximateLit => SyntaxKind::APPROXIMATE_LIT,
            TokenKind::StringLit => SyntaxKind::STRING_LIT,
            TokenKind::CharLit => SyntaxKind::CHAR_LIT,

            TokenKind::LBrace => SyntaxKind::L_BRACE,
            TokenKind::RBrace => SyntaxKind::R_BRACE,
            TokenKind::LBracket => SyntaxKind::L_BRACKET,
            TokenKind::RBracket => SyntaxKind::R_BRACKET,
            TokenKind::LParen => SyntaxKind::L_PAREN,
            TokenKind::RParen => SyntaxKind::R_PAREN,
            TokenKind::Semicolon => SyntaxKind::SEMICOLON,
            TokenKind::Comma => SyntaxKind::COMMA,
            TokenKind::Colon => SyntaxKind::COLON,
            TokenKind::Dot => SyntaxKind::DOT,
            TokenKind::Assign => SyntaxKind::ASSIGN,

            TokenKind::Eq => SyntaxKind::EQ,
            TokenKind::NotEq => SyntaxKind::NOT_EQ,
            TokenKind::Lt => SyntaxKind::LT,
            TokenKind::Gt => SyntaxKind::GT,
            TokenKind::LtEq => SyntaxKind::LT_EQ,
            TokenKind::GtEq => SyntaxKind::GT_EQ,

            TokenKind::Plus => SyntaxKind::PLUS,
            TokenKind::Minus => SyntaxKind::MINUS,
            TokenKind::Star => SyntaxKind::STAR,
            TokenKind::Slash => SyntaxKind::SLASH,

            TokenKind::Whitespace => SyntaxKind::WHITESPACE,
            TokenKind::LineComment => SyntaxKind::LINE_COMMENT,
            TokenKind::BlockComment => SyntaxKind::BLOCK_COMMENT,

            TokenKind::Eof => SyntaxKind::EOF,
            TokenKind::Error => SyntaxKind::LEX_ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_kind_round_trips_through_syntax_kind() {
        // For every TokenKind, the conversion produces a token-half
        // SyntaxKind, and that SyntaxKind reports itself as a token.
        let cases: &[TokenKind] = &[
            TokenKind::Ident,
            TokenKind::IntegerLit,
            TokenKind::RationalLit,
            TokenKind::ApproximateLit,
            TokenKind::StringLit,
            TokenKind::CharLit,
            TokenKind::LBrace,
            TokenKind::RBrace,
            TokenKind::LBracket,
            TokenKind::RBracket,
            TokenKind::LParen,
            TokenKind::RParen,
            TokenKind::Semicolon,
            TokenKind::Comma,
            TokenKind::Colon,
            TokenKind::Dot,
            TokenKind::Assign,
            TokenKind::Eq,
            TokenKind::NotEq,
            TokenKind::Lt,
            TokenKind::Gt,
            TokenKind::LtEq,
            TokenKind::GtEq,
            TokenKind::Plus,
            TokenKind::Minus,
            TokenKind::Star,
            TokenKind::Slash,
            TokenKind::Whitespace,
            TokenKind::LineComment,
            TokenKind::BlockComment,
            TokenKind::Eof,
            TokenKind::Error,
        ];
        for tk in cases {
            let sk = SyntaxKind::from(*tk);
            assert!(sk.is_token(), "{tk:?} → {sk:?} should be a token kind");
        }
    }

    #[test]
    fn node_kinds_report_themselves_as_non_tokens() {
        for sk in [
            SyntaxKind::ROOT,
            SyntaxKind::PROGRAM_DECL,
            SyntaxKind::OPER_DECL,
            SyntaxKind::HEADING,
            SyntaxKind::BLOCK,
            SyntaxKind::CALL_EXPR,
            SyntaxKind::PARSE_ERROR,
        ] {
            assert!(!sk.is_token(), "{sk:?} should be a node kind");
        }
    }

    #[test]
    fn trivia_predicate() {
        assert!(SyntaxKind::WHITESPACE.is_trivia());
        assert!(SyntaxKind::LINE_COMMENT.is_trivia());
        assert!(SyntaxKind::BLOCK_COMMENT.is_trivia());
        assert!(!SyntaxKind::IDENT.is_trivia());
        assert!(!SyntaxKind::ROOT.is_trivia());
    }
}

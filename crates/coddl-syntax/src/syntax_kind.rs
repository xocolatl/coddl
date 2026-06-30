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
    FORMAT_STRING_LIT,
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
    ARROW,

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
    PIPE_PIPE,

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
    /// `public relvar <Name> <heading> { <key-clause> };` —
    /// application-side relvar exposed to the catalog. `.cd` dialect.
    PUBLIC_RELVAR_DECL,
    /// `private relvar <Name> <heading> { <key-clause> };` —
    /// application-side relvar internal to the program. `.cd` dialect.
    PRIVATE_RELVAR_DECL,
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
    /// `-> <type-ref>` — the return-type clause on an `oper` decl.
    RETURN_CLAUSE,

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
    TRUNCATE_STMT,
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
    /// `true` / `false` — Boolean literal. Wraps the contextual-keyword
    /// IDENT token; the typechecker reads `.text()` to pick the value.
    BOOL_LITERAL,

    /// `{ name: value, name: value, … }` named-argument list at a call
    /// site. Distinguished from `HEADING` (types) and `TUPLE_LIT`
    /// (anonymous tuple values) by the surrounding production.
    ARG_LIST,
    NAMED_ARG,

    /// `key { a, b, … }` candidate-key clause on a relvar declaration.
    /// Shared between `.cd` application relvars and `.cddb` database
    /// relvars.
    KEY_CLAUSE,

    /// `database <Name>;` declaration in `.cd` source, binding the
    /// program to its database. Structurally identical to `.cddb`'s
    /// `DATABASE_DECL` but used in the inverse role — there it
    /// declares the catalog; here it declares which catalog the
    /// program consumes.
    DATABASE_BINDING,

    // ── `.cddb` dialect — database catalog ───────────────────────────
    /// Root of a parsed `.cddb` document.
    CDDB_ROOT,
    /// `database <Name>;` — required first item of every `.cddb`.
    DATABASE_DECL,
    /// `base relvar <Name> <heading> [<key-clause>];` — persistent
    /// catalog relvar.
    BASE_RELVAR_DECL,
    /// `virtual relvar <Name> = <relexpr>;` — catalog view. v1 parses
    /// keyword + name + `=` and treats the RHS as an unknown body.
    VIRTUAL_RELVAR_DECL,

    // ── `.cdmap` dialect — external → conceptual adapter ─────────────
    /// Root of a parsed `.cdmap` document.
    CDMAP_ROOT,
    /// `map <program> to <database>;` — required first item.
    CDMAP_HEADER,
    /// `<app-name> = <catalog-name>;` — identity mapping entry. Future:
    /// `... project { … } rename { … }` chains via the clauses below.
    CDMAP_ENTRY,
    /// `project { a, b, … }` clause on a map entry. Reserved for
    /// Phase 16; parsed as an unknown body today.
    CDMAP_PROJECT_CLAUSE,
    /// `rename { db_attr: app_attr, … }` clause on a map entry.
    /// Reserved for Phase 16; parsed as an unknown body today.
    CDMAP_RENAME_CLAUSE,

    // ── `.cdstore` dialect — conceptual → physical binding ───────────
    /// Root of a parsed `.cdstore` document.
    CDSTORE_ROOT,
    /// `store for <database>;` — required first item.
    CDSTORE_HEADER,
    /// `backend <kind> { <field>, … };` — exactly one per file.
    BACKEND_DECL,
    /// `relvar <Name>: table "<sql>" { columns: { … } };` — binds a
    /// base catalog relvar to a physical table and column set.
    RELVAR_BINDING,
    /// `<name>: <value>` field inside a backend block, columns block,
    /// or relvar-binding body. Value grammar is narrow: a string
    /// literal, an identifier, or an `env(...)` call.
    CDSTORE_FIELD,
    /// `columns: { <name>: "<col>", … }` block inside a relvar binding.
    COLUMNS_BLOCK,

    /// `<relExpr> project { a, b, … }` — relational projection. A postfix
    /// expression node wrapping its relation operand; the brace-list of
    /// bare attribute names follows the `project` keyword. An expression
    /// kind, placed at the end of the enum so existing discriminants stay
    /// stable (per the section-end convention above).
    PROJECT_EXPR,

    /// `<relExpr> replace { new: e, … }` — relational replace. A postfix
    /// expression node wrapping its relation operand; the `new: e` pairs
    /// (an `ARG_LIST` of `NAMED_ARG`) follow the `replace` keyword. Adds each
    /// `new` attribute and removes the operand attributes its value references.
    /// Each value must compute (read ≥1 attribute); a bare attribute reference
    /// is a pure relabel and belongs to `rename` (T0047). Placed at the end of
    /// the enum to keep existing discriminants stable.
    REPLACE_EXPR,

    /// `<relExpr> tclose [ { a, b } ]` — relational transitive closure. A
    /// postfix expression node wrapping its binary relation operand; the
    /// optional unordered brace-list of two attribute names (sugar for
    /// `project { a, b } tclose`) follows the `tclose` keyword. Placed at the
    /// end of the enum to keep existing discriminants stable.
    TCLOSE_EXPR,

    /// `<relExpr> extend { new: e, … }` — relational extend. A postfix
    /// expression node wrapping its relation operand; the `new: e` pairs (an
    /// `ARG_LIST` of `NAMED_ARG`) follow the `extend` keyword. Adds each `new`
    /// attribute bound to the computed value `e`, keeping every operand
    /// attribute. Placed at the end of the enum to keep existing discriminants
    /// stable.
    EXTEND_EXPR,

    /// `<relExpr> rename { new: old, … }` — relational rename (relabel). A
    /// postfix expression node wrapping its relation operand; the `new: old`
    /// pairs (an `ARG_LIST` of `NAMED_ARG`) follow the `rename` keyword. Each
    /// value must be a bare attribute reference (the source attribute); a
    /// computed value belongs to `replace` (T0030). The strict relabel-only
    /// partition of `replace`. Placed at the end of the enum to keep existing
    /// discriminants stable.
    RENAME_EXPR,

    /// `<relExpr> wrap { t: { a, b }, … }` — relational wrap (group attributes
    /// into a tuple-valued attribute). A postfix expression node wrapping its
    /// relation operand; each `WRAP_PAIR` follows the `wrap` keyword. Placed at
    /// the end of the enum to keep existing discriminants stable.
    WRAP_EXPR,

    /// One `new: { a, b }` pair inside a `WRAP_EXPR`: the new tuple-valued
    /// attribute name (an `IDENT`) and the unordered brace-list of existing
    /// attribute names to group into it (bare `IDENT` tokens after `{`).
    WRAP_PAIR,

    /// `<relExpr> unwrap { t, … }` — relational unwrap (expand a tuple-valued
    /// attribute back to its components). A postfix expression node wrapping its
    /// relation operand; the unordered brace-list of tuple-valued attribute
    /// names follows the `unwrap` keyword. Placed at the end of the enum to keep
    /// existing discriminants stable.
    UNWRAP_EXPR,

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
            TokenKind::FormatStringLit => SyntaxKind::FORMAT_STRING_LIT,
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
            TokenKind::Arrow => SyntaxKind::ARROW,

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
            TokenKind::PipePipe => SyntaxKind::PIPE_PIPE,

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
            TokenKind::FormatStringLit,
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
            TokenKind::PipePipe,
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
            SyntaxKind::KEY_CLAUSE,
            SyntaxKind::PUBLIC_RELVAR_DECL,
            SyntaxKind::PRIVATE_RELVAR_DECL,
            SyntaxKind::DATABASE_BINDING,
            SyntaxKind::CDDB_ROOT,
            SyntaxKind::DATABASE_DECL,
            SyntaxKind::BASE_RELVAR_DECL,
            SyntaxKind::VIRTUAL_RELVAR_DECL,
            SyntaxKind::CDMAP_ROOT,
            SyntaxKind::CDMAP_HEADER,
            SyntaxKind::CDMAP_ENTRY,
            SyntaxKind::CDMAP_PROJECT_CLAUSE,
            SyntaxKind::CDMAP_RENAME_CLAUSE,
            SyntaxKind::CDSTORE_ROOT,
            SyntaxKind::CDSTORE_HEADER,
            SyntaxKind::BACKEND_DECL,
            SyntaxKind::RELVAR_BINDING,
            SyntaxKind::CDSTORE_FIELD,
            SyntaxKind::COLUMNS_BLOCK,
            SyntaxKind::PROJECT_EXPR,
            SyntaxKind::REPLACE_EXPR,
            SyntaxKind::TCLOSE_EXPR,
            SyntaxKind::RENAME_EXPR,
            SyntaxKind::WRAP_EXPR,
            SyntaxKind::WRAP_PAIR,
            SyntaxKind::UNWRAP_EXPR,
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

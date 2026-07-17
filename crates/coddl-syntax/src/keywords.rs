//! Single source of truth for every contextually recognized identifier.
//!
//! Coddl's lexer has no keyword token type — every word lexes as `IDENT` —
//! and the parser recognizes specific identifiers in specific syntactic
//! positions. The full inventory lives here, in one place, so the sites
//! that must agree on it cannot drift: the parser's operator table
//! (`peek_infix_prec`) and the AST's operator allow-lists
//! (`BinaryExpr::op_token`/`op_kind`, `UnaryExpr`, `BoolLit`, `SortItem`,
//! `ProjectExpr`) consume the same consts, and `docs/grammar.md`
//! §"Reserved words" publishes this module as a table —
//! `tools/check-grammar.sh` (Check 3) diffs the two bidirectionally and
//! runs from the pre-commit hook. That check harvests every string
//! literal in this module outside the test block, so keep the invariant:
//! **every non-test string literal here is a keyword or glyph** — nothing
//! else (no message strings, no quoted prose in code). Check 3 also
//! cross-checks the VSCode TextMate grammar against this module, treating
//! everything above [`CDDB_WORDS`] as the `.cd` keyword set — so **the
//! dialect groups stay last in this file**.
//!
//! Two kinds of groups live here:
//!
//! - **Consumed** groups are mechanically matched in code ([`INFIX_OPS`],
//!   [`UNARY_OPS`], [`BOOL_WORDS`], [`EXPR_HEAD_NARROWED`],
//!   [`TYPE_GENERATORS`], the single-word consts) — changing an entry
//!   changes the parser (and, where noted, the AST or the typechecker)
//!   with it.
//! - **Inventory** groups ([`STMT_HEADS`], [`POSTFIX_SUFFIXES`],
//!   [`CLAUSE_WORDS`], [`ITEM_HEADS`], the dialect sets) each have exactly
//!   one hand-written match site in the parser today; they are published
//!   for the grammar.md keyword table.
//!
//! Symbolic operators (`= <> < > <= >= + - * / ||`) and the comparison
//! glyphs (`≤ ≥ ≠ ⊂ ⊃ ⊆ ⊇`) are lexer token kinds, not identifiers — they
//! have no entry here and no declaration-site exposure.

use crate::ast::{BinaryOp, UnaryOp};
use crate::syntax_kind::SyntaxKind;

/// The five reserved words — Tier 1 of the grammar.md taxonomy.
///
/// Each is claimed at an expression head with no delimiter to narrow on
/// (`true`/`false` are the bare word; `if`/`not`/`extract` are followed by
/// an arbitrary expression), so no lookahead can free them, and a binding
/// under one of these names would be unreachable or misparse at every bare
/// reference. Declaring one is therefore rejected at the declaration site
/// (P0096; PB0012 from the `.cddb` parser) via [`is_reserved`] — softly:
/// the diagnostic is emitted, the name still binds, parsing continues.
pub const RESERVED: &[&str] = &["true", "false", "if", "not", "extract"];

/// The word-operator glyphs, which lex as `IDENT` with the glyph text
/// (`lex_word_glyph`) and would therefore be declarable exactly like
/// [`RESERVED`] words — so [`is_reserved`] rejects them at declaration
/// sites the same way. Every glyph here is an operator spelling in
/// [`INFIX_OPS`] / [`UNARY_OPS`] (asserted by test).
pub const RESERVED_GLYPHS: &[&str] = &["¬", "⋈", "∪", "∩", "∖", "⋉", "▷"];

/// Whether `text` is a reserved word or a reserved word-operator glyph —
/// the declaration-name check's predicate (`Parser::check_decl_name`).
pub fn is_reserved(text: &str) -> bool {
    RESERVED.contains(&text) || RESERVED_GLYPHS.contains(&text)
}

/// One textual infix operator: its keyword, optional glyph synonym,
/// binding power, and AST operator. The parser's `peek_infix_prec` and the
/// AST's `BinaryExpr::op_token`/`op_kind` both resolve through this table.
pub struct InfixOp {
    pub word: &'static str,
    pub glyph: Option<&'static str>,
    /// Binding power on the shared ladder with the symbolic operators:
    /// `* / div` = 5, `+ - ||` = 4, comparisons = 3, `and` = 2, `or` = 1,
    /// pipeline (relational ops, `where`, `when`, `otherwise`) = 0.
    pub prec: u8,
    pub op: BinaryOp,
}

/// The textual infix operators. `not` is deliberately absent: alone it is
/// the unary Boolean negation ([`UNARY_OPS`]); the two-token antijoin
/// `not matching` is recognized by the parser's own two-token lookahead,
/// and only its one-token glyph `▷` resolves through this table (the entry
/// word contains a space, so it can never match a single token's text).
pub const INFIX_OPS: &[InfixOp] = &[
    InfixOp {
        word: "div",
        glyph: None,
        prec: 5,
        op: BinaryOp::IntDiv,
    },
    InfixOp {
        word: "and",
        glyph: None,
        prec: 2,
        op: BinaryOp::And,
    },
    InfixOp {
        word: "or",
        glyph: None,
        prec: 1,
        op: BinaryOp::Or,
    },
    InfixOp {
        word: "where",
        glyph: None,
        prec: 0,
        op: BinaryOp::Where,
    },
    InfixOp {
        word: "join",
        glyph: Some("⋈"),
        prec: 0,
        op: BinaryOp::Join,
    },
    InfixOp {
        word: "times",
        glyph: None,
        prec: 0,
        op: BinaryOp::Times,
    },
    InfixOp {
        word: "compose",
        glyph: None,
        prec: 0,
        op: BinaryOp::Compose,
    },
    InfixOp {
        word: "intersect",
        glyph: Some("∩"),
        prec: 0,
        op: BinaryOp::Intersect,
    },
    InfixOp {
        word: "union",
        glyph: Some("∪"),
        prec: 0,
        op: BinaryOp::Union,
    },
    InfixOp {
        word: "minus",
        glyph: Some("∖"),
        prec: 0,
        op: BinaryOp::Minus,
    },
    InfixOp {
        word: "matching",
        glyph: Some("⋉"),
        prec: 0,
        op: BinaryOp::Matching,
    },
    InfixOp {
        word: "not matching",
        glyph: Some("▷"),
        prec: 0,
        op: BinaryOp::NotMatching,
    },
    InfixOp {
        word: "when",
        glyph: None,
        prec: 0,
        op: BinaryOp::When,
    },
    InfixOp {
        word: "otherwise",
        glyph: None,
        prec: 0,
        op: BinaryOp::Otherwise,
    },
];

/// Resolve a single token's text to its infix operator, by keyword or
/// glyph. `None` for everything else — including `not` (unary / the first
/// token of `not matching`).
pub fn infix(text: &str) -> Option<&'static InfixOp> {
    INFIX_OPS
        .iter()
        .find(|e| e.word == text || e.glyph == Some(text))
}

/// The prefix operators: `(word, glyph, op)`.
pub const UNARY_OPS: &[(&str, Option<&str>, UnaryOp)] = &[
    ("extract", None, UnaryOp::Extract),
    ("not", Some("¬"), UnaryOp::Not),
];

/// Resolve a single token's text to its prefix operator, by keyword or glyph.
pub fn unary(text: &str) -> Option<UnaryOp> {
    UNARY_OPS
        .iter()
        .find(|(word, glyph, _)| *word == text || *glyph == Some(text))
        .map(|(_, _, op)| *op)
}

/// The Boolean literals.
pub const BOOL_WORDS: &[(&str, bool)] = &[("true", true), ("false", false)];

/// Resolve a single token's text to its Boolean literal value.
pub fn bool_word(text: &str) -> Option<bool> {
    BOOL_WORDS
        .iter()
        .find(|(word, _)| *word == text)
        .map(|(_, value)| *value)
}

/// Single words matched in both the parser and the AST view — named once so
/// the two sites cannot drift. `all`/`but` form the project-away suffix
/// (`R project all but { … }`); `asc`/`desc` are the order-key directions
/// (already lookahead-narrowed: recognized only when followed by another
/// IDENT, the in-tree narrowing precedent).
pub const ALL: &str = "all";
pub const BUT: &str = "but";
pub const ASC: &str = "asc";
pub const DESC: &str = "desc";

/// Expression-head words that are special **only together with their
/// delimiter** — Tier 2 of the taxonomy. `Parser::at_expr_head` claims a
/// word only when the next token is its delimiter from this table, so the
/// bare word falls through to an ordinary `NAME_REF` (a relvar named
/// `Sequence` or an attribute named `transaction` is fully usable).
pub const EXPR_HEAD_NARROWED: &[(&str, SyntaxKind)] = &[
    ("Relation", SyntaxKind::L_BRACE),
    ("Sequence", SyntaxKind::L_BRACKET),
    ("transaction", SyntaxKind::L_BRACKET),
];

/// Statement-head words (`parse_stmt` dispatch, via `Parser::at_stmt_head`).
/// Each is claimed only when the next token is not `:=`, so an assignment
/// to a same-named variable (`delete := 2;`) falls through to the
/// ASSIGN_STMT fallback and the bare word stays an ordinary identifier.
pub const STMT_HEADS: &[&str] = &[
    "let", "var", "truncate", "delete", "insert", "update", "for", "while", "do", "load", "return",
];

/// Postfix pipeline suffixes (relation-returning, written after their
/// operand). Recognized only at pipeline precedence in operator position —
/// no identifier can legally occur there, so the claim shadows nothing.
pub const POSTFIX_SUFFIXES: &[&str] = &[
    "project", "replace", "tclose", "extend", "rename", "wrap", "unwrap", "group", "ungroup",
];

/// Clause words, each recognized only after its introducing construct
/// (`if … then … else`, `for … in`/`to`, `load … from … order`, sort-item
/// `asc`/`desc`, relvar `key`, project `all but`). Vacuous claims.
pub const CLAUSE_WORDS: &[&str] = &[
    "then", "else", "in", "from", "to", "order", "asc", "desc", "key", "all", "but",
];

/// Item-head words (`parse_item` top-level dispatch). No expression can
/// start at item position, so these claims are vacuous. `relvar` and `key`
/// are decl-interior words (recognized inside a relvar declaration), not
/// item heads; `builtin` is already two-token narrowed
/// (`builtin relvar` vs `builtin oper`).
pub const ITEM_HEADS: &[&str] = &[
    "program", "library", "module", "database", "public", "private", "base", "virtual", "builtin",
    "oper", "type", "use", "let", "var",
];

/// Type-position generators (`parse_type_ref`). `Tuple` is claimed nowhere
/// else — it never claims expression space; `Relation`/`Sequence` also have
/// the expression-head claims listed in [`EXPR_HEAD_NARROWED`]. The
/// typechecker consumes this set as part of T0085's cannot-redefine list:
/// a `type` named after a generator would be unreachable, so declaring one
/// is rejected like a builtin.
pub const TYPE_GENERATORS: &[&str] = &["Tuple", "Relation", "Sequence"];

/// `.cddb` dialect keywords (`parser_cddb`).
pub const CDDB_WORDS: &[&str] = &[
    "database", "base", "virtual", "public", "private", "relvar", "key",
];

/// `.cdstore` dialect keywords (`parser_cdstore`).
pub const CDSTORE_WORDS: &[&str] = &[
    "store", "for", "backend", "relvar", "table", "columns", "env", "default",
];

/// `.cdmap` dialect keywords (`parser_cdmap`).
pub const CDMAP_WORDS: &[&str] = &["map", "to"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infix_words_and_glyphs_are_distinct() {
        // Every lookup key (word or glyph) resolves to exactly one entry.
        let mut keys: Vec<&str> = Vec::new();
        for e in INFIX_OPS {
            keys.push(e.word);
            if let Some(g) = e.glyph {
                keys.push(g);
            }
        }
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "duplicate infix key: {keys:?}");
    }

    #[test]
    fn reserved_glyphs_are_exactly_the_operator_glyphs() {
        // RESERVED_GLYPHS is the declarable-glyph inventory: every glyph
        // spelling of an infix/unary operator, nothing else.
        let mut op_glyphs: Vec<&str> = INFIX_OPS.iter().filter_map(|e| e.glyph).collect();
        op_glyphs.extend(UNARY_OPS.iter().filter_map(|(_, g, _)| *g));
        op_glyphs.sort_unstable();
        let mut reserved = RESERVED_GLYPHS.to_vec();
        reserved.sort_unstable();
        assert_eq!(op_glyphs, reserved);
    }

    #[test]
    fn reserved_words_are_the_unnarrowable_claims() {
        // The five: the Boolean literals plus the prefix operators plus
        // `if` — each claimed at an expression head with no delimiter.
        for (word, _) in BOOL_WORDS {
            assert!(RESERVED.contains(word), "{word} missing from RESERVED");
        }
        for (word, _, _) in UNARY_OPS {
            assert!(RESERVED.contains(word), "{word} missing from RESERVED");
        }
        assert!(RESERVED.contains(&"if"));
        assert_eq!(RESERVED.len(), 5);
    }

    #[test]
    fn not_is_not_an_infix_lookup_key() {
        // `not` alone is unary; the antijoin resolves only through its
        // glyph (the word entry contains a space, unmatched by any token).
        assert!(infix("not").is_none());
        assert_eq!(infix("▷").map(|e| e.op), Some(BinaryOp::NotMatching));
        assert!(unary("not").is_some());
    }
}

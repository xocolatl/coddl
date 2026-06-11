//! Concrete syntax tree types and builder.
//!
//! Backed by `rowan`'s green / red tree split: the green tree stores
//! the raw nodes and tokens with their offsets and is cheap to clone
//! (atomic refcount on the shared interior); the red tree is the
//! traversable view that knows its parent, sibling, and absolute
//! offset. The types here are thin newtypes over `rowan`'s — the
//! consumer-facing API stays small.

use rowan::GreenNodeBuilder;

use crate::syntax_kind::SyntaxKind;

/// The `rowan::Language` marker for Coddl. A zero-sized phantom type;
/// the only thing it does is tell `rowan` how to convert between
/// `rowan::SyntaxKind(u16)` and our [`SyntaxKind`] enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoddlLanguage {}

impl rowan::Language for CoddlLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        // The raw u16 came from `kind_to_raw` on the same `SyntaxKind`
        // discriminant, so transmuting is sound. The debug assertion
        // catches a class of bug (e.g. trees built with one
        // `SyntaxKind` version, read by another) early.
        debug_assert!(raw.0 <= SyntaxKind::PARSE_ERROR as u16);
        // SAFETY: SyntaxKind is `#[repr(u16)]` with a contiguous range
        // [0 .. PARSE_ERROR]. The debug_assert above enforces the
        // upper bound in development builds.
        unsafe { std::mem::transmute(raw.0) }
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind as u16)
    }
}

/// A non-leaf node in the syntax tree. Carries a [`SyntaxKind`], an
/// absolute byte range in the source, and children (more nodes and
/// tokens, in source order including trivia).
pub type SyntaxNode = rowan::SyntaxNode<CoddlLanguage>;

/// A leaf in the syntax tree. Stores its [`SyntaxKind`], its absolute
/// byte range, and the lexeme text.
pub type SyntaxToken = rowan::SyntaxToken<CoddlLanguage>;

/// Either a node or a token. The two variants are what
/// [`SyntaxNode::children_with_tokens`] yields.
pub type SyntaxElement = rowan::SyntaxElement<CoddlLanguage>;

/// A builder for syntax trees.
///
/// Wraps `rowan`'s [`GreenNodeBuilder`] with our [`SyntaxKind`]. The
/// parser drives it via three operations:
///
/// - [`CstBuilder::start_node`] opens a new interior node.
/// - [`CstBuilder::token`] adds a leaf.
/// - [`CstBuilder::finish_node`] closes the most recently opened node.
///
/// Calls must be balanced (one `finish_node` per `start_node`); the
/// builder asserts this in [`CstBuilder::finish`].
pub struct CstBuilder<'a> {
    inner: GreenNodeBuilder<'static>,
    /// The source buffer the builder is indexing into. Used when the
    /// parser hands token lexemes through by span rather than by string.
    source: &'a str,
}

impl<'a> CstBuilder<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            inner: GreenNodeBuilder::new(),
            source,
        }
    }

    /// Push a leaf token onto the current node. `range` is a byte
    /// range into the buffer passed at construction.
    pub fn token(&mut self, kind: SyntaxKind, range: std::ops::Range<usize>) {
        let text = &self.source[range];
        self.inner
            .token(<CoddlLanguage as rowan::Language>::kind_to_raw(kind), text);
    }

    /// Push a leaf token by its already-resolved lexeme. Equivalent to
    /// [`Self::token`] when the lexeme is computed elsewhere.
    pub fn token_str(&mut self, kind: SyntaxKind, text: &str) {
        self.inner
            .token(<CoddlLanguage as rowan::Language>::kind_to_raw(kind), text);
    }

    /// Start a new interior node at the current cursor.
    pub fn start_node(&mut self, kind: SyntaxKind) {
        self.inner
            .start_node(<CoddlLanguage as rowan::Language>::kind_to_raw(kind));
    }

    /// Close the most recently started interior node.
    pub fn finish_node(&mut self) {
        self.inner.finish_node();
    }

    /// Finalise the tree and return the [`SyntaxNode`] for its root.
    pub fn finish(self) -> SyntaxNode {
        SyntaxNode::new_root(self.inner.finish())
    }
}

/// Wrap a sequence of lexer tokens in a single `ROOT` node, producing
/// a valid (but structureless) syntax tree.
///
/// The resulting tree has no syntactic structure beyond its root —
/// every token is a direct child of `ROOT`. Useful as a smoke test for
/// the tree machinery and as a fallback for unparseable input.
pub fn lex_to_flat_cst(source: &str, file: coddl_diagnostics::FileId) -> crate::ParseOutput {
    let lex_out = crate::lex(source, file);
    let mut b = CstBuilder::new(source);
    b.start_node(SyntaxKind::ROOT);
    for tok in &lex_out.tokens {
        // Eof tokens carry a zero-length span; skip them in the tree.
        if tok.kind == crate::TokenKind::Eof {
            continue;
        }
        let range = tok.span.start as usize..tok.span.end as usize;
        b.token(SyntaxKind::from(tok.kind), range);
    }
    b.finish_node();
    crate::ParseOutput {
        tree: b.finish(),
        diagnostics: lex_out.diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_diagnostics::FileId;

    fn collect_kinds(node: &SyntaxNode) -> Vec<SyntaxKind> {
        node.children_with_tokens()
            .map(|el| el.kind())
            .collect()
    }

    #[test]
    fn builder_round_trips_a_minimal_tree() {
        // Build `oper main {}` by hand and verify the structure.
        let mut b = CstBuilder::new("oper main {}");
        b.start_node(SyntaxKind::ROOT);
        b.start_node(SyntaxKind::OPER_DECL);
        b.token(SyntaxKind::IDENT, 0..4); // "oper"
        b.token(SyntaxKind::WHITESPACE, 4..5);
        b.token(SyntaxKind::IDENT, 5..9); // "main"
        b.token(SyntaxKind::WHITESPACE, 9..10);
        b.start_node(SyntaxKind::HEADING);
        b.token(SyntaxKind::L_BRACE, 10..11);
        b.token(SyntaxKind::R_BRACE, 11..12);
        b.finish_node();
        b.finish_node();
        b.finish_node();
        let tree = b.finish();

        assert_eq!(tree.kind(), SyntaxKind::ROOT);
        assert_eq!(tree.text(), "oper main {}");

        let top: Vec<_> = tree.children().collect();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].kind(), SyntaxKind::OPER_DECL);

        let heading = top[0]
            .children()
            .find(|n| n.kind() == SyntaxKind::HEADING)
            .expect("heading present");
        assert_eq!(heading.text(), "{}");
    }

    #[test]
    fn lex_to_flat_cst_preserves_every_byte() {
        let src = "oper main {} [ write_line{message: \"hi\"}; ];";
        let out = lex_to_flat_cst(src, FileId(0));
        assert!(out.diagnostics.is_empty());
        assert_eq!(out.tree.kind(), SyntaxKind::ROOT);
        assert_eq!(out.tree.text(), src);
    }

    #[test]
    fn raw_kind_round_trip() {
        // Sanity-check the Language impl: every variant should survive
        // the round trip through rowan's raw u16 representation.
        for sk in [
            SyntaxKind::IDENT,
            SyntaxKind::INTEGER_LIT,
            SyntaxKind::L_BRACE,
            SyntaxKind::ROOT,
            SyntaxKind::OPER_DECL,
            SyntaxKind::PARSE_ERROR,
        ] {
            let raw = <CoddlLanguage as rowan::Language>::kind_to_raw(sk);
            let back = <CoddlLanguage as rowan::Language>::kind_from_raw(raw);
            assert_eq!(sk, back);
        }
    }
}

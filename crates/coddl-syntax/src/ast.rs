//! Typed AST view over the concrete syntax tree.
//!
//! Each AST node here is a thin newtype wrapping a [`SyntaxNode`]; the
//! [`AstNode`] trait mediates the cast from raw syntax to typed view.
//! Walking the AST is walking the CST through a typed lens — the tree
//! storage is the same; the types make access ergonomic and
//! type-checked.
//!
//! The wrapper layer is essentially zero-cost: an AST newtype is just a
//! `SyntaxNode`, and the cast is one tag comparison.

use crate::cst::{SyntaxNode, SyntaxToken};
use crate::syntax_kind::SyntaxKind;

/// Trait implemented by every typed AST node.
pub trait AstNode: Sized {
    fn can_cast(kind: SyntaxKind) -> bool;
    fn cast(syntax: SyntaxNode) -> Option<Self>;
    fn syntax(&self) -> &SyntaxNode;
}

// ── Internal helpers (used by the typed accessors below) ─────────────────

pub(crate) fn child<C: AstNode>(syntax: &SyntaxNode) -> Option<C> {
    syntax.children().find_map(C::cast)
}

pub(crate) fn children<C: AstNode>(syntax: &SyntaxNode) -> impl Iterator<Item = C> {
    syntax.children().filter_map(C::cast)
}

pub(crate) fn nth_token(syntax: &SyntaxNode, kind: SyntaxKind, n: usize) -> Option<SyntaxToken> {
    syntax
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|t| t.kind() == kind)
        .nth(n)
}

pub(crate) fn first_token_of_kind(syntax: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    nth_token(syntax, kind, 0)
}

// ── Boilerplate macro ────────────────────────────────────────────────────

/// Define a plain newtype around a single [`SyntaxKind`] node. Generates
/// the struct, the [`AstNode`] impl, and a public constructor.
///
/// Exported with `#[macro_export]` for crate-internal use by the
/// dialect AST modules (`ast_cddb`, `ast_cdmap`, `ast_cdstore`); the
/// macro is not part of the stable public API.
#[macro_export]
#[doc(hidden)]
macro_rules! ast_node {
    ($vis:vis $name:ident, $kind:ident) => {
        #[derive(Debug, Clone)]
        $vis struct $name {
            syntax: $crate::SyntaxNode,
        }
        impl $crate::ast::AstNode for $name {
            fn can_cast(kind: $crate::SyntaxKind) -> bool {
                kind == $crate::SyntaxKind::$kind
            }
            fn cast(syntax: $crate::SyntaxNode) -> Option<Self> {
                Self::can_cast(syntax.kind()).then_some(Self { syntax })
            }
            fn syntax(&self) -> &$crate::SyntaxNode {
                &self.syntax
            }
        }
    };
}

// ── Root + top-level items ───────────────────────────────────────────────

ast_node!(pub Root, ROOT);

impl Root {
    /// Iterate the top-level items in source order. Trivia and
    /// `PARSE_ERROR` placeholders are skipped.
    pub fn items(&self) -> impl Iterator<Item = Item> + '_ {
        self.syntax.children().filter_map(Item::cast)
    }
}

/// Top-level item variants.
///
/// In `.cd` source, `public` / `private` are the legal relvar kinds;
/// `base` / `virtual` also parse so the typechecker can emit T0014 on
/// the resulting tree. The AST view exposes every kind uniformly so a
/// pass walking `.cd` items doesn't need a separate parser-vs-checker
/// vocabulary.
#[derive(Debug, Clone)]
pub enum Item {
    ProgramDecl(ProgramDecl),
    DatabaseBinding(DatabaseBinding),
    PublicRelvarDecl(PublicRelvarDecl),
    PrivateRelvarDecl(PrivateRelvarDecl),
    BaseRelvarDecl(crate::ast_cddb::BaseRelvarDecl),
    VirtualRelvarDecl(crate::ast_cddb::VirtualRelvarDecl),
    OperDecl(OperDecl),
}

impl Item {
    pub fn cast(syntax: SyntaxNode) -> Option<Self> {
        Some(match syntax.kind() {
            SyntaxKind::PROGRAM_DECL => Item::ProgramDecl(ProgramDecl { syntax }),
            SyntaxKind::DATABASE_BINDING => Item::DatabaseBinding(DatabaseBinding { syntax }),
            SyntaxKind::PUBLIC_RELVAR_DECL => {
                Item::PublicRelvarDecl(PublicRelvarDecl::cast(syntax)?)
            }
            SyntaxKind::PRIVATE_RELVAR_DECL => {
                Item::PrivateRelvarDecl(PrivateRelvarDecl::cast(syntax)?)
            }
            SyntaxKind::BASE_RELVAR_DECL => {
                Item::BaseRelvarDecl(crate::ast_cddb::BaseRelvarDecl::cast(syntax)?)
            }
            SyntaxKind::VIRTUAL_RELVAR_DECL => {
                Item::VirtualRelvarDecl(crate::ast_cddb::VirtualRelvarDecl::cast(syntax)?)
            }
            SyntaxKind::OPER_DECL => Item::OperDecl(OperDecl { syntax }),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Item::ProgramDecl(d) => d.syntax(),
            Item::DatabaseBinding(d) => d.syntax(),
            Item::PublicRelvarDecl(d) => d.syntax(),
            Item::PrivateRelvarDecl(d) => d.syntax(),
            Item::BaseRelvarDecl(d) => d.syntax(),
            Item::VirtualRelvarDecl(d) => d.syntax(),
            Item::OperDecl(d) => d.syntax(),
        }
    }
}

// ── ProgramDecl ──────────────────────────────────────────────────────────

ast_node!(pub ProgramDecl, PROGRAM_DECL);

impl ProgramDecl {
    /// The declared program name. `program` itself is also an IDENT in
    /// the tree (contextual keyword), so the name is the *second* IDENT
    /// child.
    pub fn name(&self) -> Option<SyntaxToken> {
        nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }
}

// ── DatabaseBinding ──────────────────────────────────────────────────────

ast_node!(pub DatabaseBinding, DATABASE_BINDING);

impl DatabaseBinding {
    /// The declared database name. The `database` keyword occupies
    /// the first IDENT slot; the name is the second.
    pub fn name(&self) -> Option<SyntaxToken> {
        nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }
}

// ── PublicRelvarDecl / PrivateRelvarDecl ─────────────────────────────────

ast_node!(pub PublicRelvarDecl, PUBLIC_RELVAR_DECL);

impl PublicRelvarDecl {
    /// The declared relvar name. Keywords `public` and `relvar` occupy
    /// the first two IDENT slots; the name is the third.
    pub fn name(&self) -> Option<SyntaxToken> {
        nth_token(&self.syntax, SyntaxKind::IDENT, 2)
    }

    /// The relvar's heading (`{ a: T, b: U, … }`).
    pub fn heading(&self) -> Option<Heading> {
        child(&self.syntax)
    }

    /// All candidate-key clauses in source order. Multi-key parses;
    /// the typechecker validates the first one for v1.
    pub fn key_clauses(&self) -> impl Iterator<Item = KeyClause> + '_ {
        children(&self.syntax)
    }
}

ast_node!(pub PrivateRelvarDecl, PRIVATE_RELVAR_DECL);

impl PrivateRelvarDecl {
    /// The declared relvar name. Keywords `private` and `relvar`
    /// occupy the first two IDENT slots; the name is the third.
    pub fn name(&self) -> Option<SyntaxToken> {
        nth_token(&self.syntax, SyntaxKind::IDENT, 2)
    }

    /// The relvar's heading (`{ a: T, b: U, … }`).
    pub fn heading(&self) -> Option<Heading> {
        child(&self.syntax)
    }

    /// All candidate-key clauses in source order.
    pub fn key_clauses(&self) -> impl Iterator<Item = KeyClause> + '_ {
        children(&self.syntax)
    }
}

// ── OperDecl ─────────────────────────────────────────────────────────────

ast_node!(pub OperDecl, OPER_DECL);

impl OperDecl {
    /// The operator's name — the IDENT immediately after the contextual
    /// `oper` keyword.
    pub fn name(&self) -> Option<SyntaxToken> {
        nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }

    /// The parameter heading `{ … }`. `None` only if parsing recovered
    /// without finding one.
    pub fn heading(&self) -> Option<Heading> {
        child(&self.syntax)
    }

    /// The declared return type, if the operator carries an explicit
    /// `-> <type-ref>` clause. Absent → implicit `Tuple {}` return.
    pub fn return_type(&self) -> Option<TypeRef> {
        let clause: ReturnClause = child(&self.syntax)?;
        child(&clause.syntax)
    }

    /// The body `[ … ]`.
    pub fn body(&self) -> Option<Block> {
        child(&self.syntax)
    }
}

ast_node!(pub ReturnClause, RETURN_CLAUSE);

// ── Heading + Param ──────────────────────────────────────────────────────

ast_node!(pub Heading, HEADING);

impl Heading {
    /// All declared parameters in source order.
    pub fn params(&self) -> impl Iterator<Item = Param> + '_ {
        children(&self.syntax)
    }
}

ast_node!(pub Param, PARAM);

impl Param {
    /// The parameter's name (the first IDENT child).
    pub fn name(&self) -> Option<SyntaxToken> {
        first_token_of_kind(&self.syntax, SyntaxKind::IDENT)
    }

    /// The parameter's type expression.
    pub fn type_ref(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
}

// ── KeyClause ────────────────────────────────────────────────────────────

ast_node!(pub KeyClause, KEY_CLAUSE);

impl KeyClause {
    /// The candidate-key attribute names in source order. The leading
    /// `key` keyword token is skipped; only attribute IDENTs (the ones
    /// between the braces) are returned.
    pub fn attrs(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        // Tokens in source order: `key`, `{`, attr, `,`, attr, …, `}`.
        // Skip the first IDENT (the `key` keyword); the remaining IDENT
        // tokens inside the braces are the attribute names.
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|t| t.kind() == SyntaxKind::IDENT)
            .skip(1)
    }
}

// ── TypeRef ──────────────────────────────────────────────────────────────

ast_node!(pub TypeRef, TYPE_REF);

impl TypeRef {
    /// The head type-name token — the leftmost IDENT. For a leaf type-ref
    /// (`Integer`, `Customer`) this is the type name; for the generator
    /// application `Sequence T` it is `Sequence` (the element type is the
    /// nested [`TypeRef`], reached via [`TypeRef::element`]).
    pub fn name(&self) -> Option<SyntaxToken> {
        first_token_of_kind(&self.syntax, SyntaxKind::IDENT)
    }

    /// The element type of a generator application `Sequence <type-ref>`:
    /// the nested `TYPE_REF` child. `None` for a leaf type-ref.
    pub fn element(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
}

// ── Block + statements ───────────────────────────────────────────────────

ast_node!(pub Block, BLOCK);

impl Block {
    /// Statements in source order.
    pub fn statements(&self) -> impl Iterator<Item = Stmt> + '_ {
        self.syntax.children().filter_map(Stmt::cast)
    }

    /// The block's tail expression, if any. A tail expression is an
    /// `Expr` direct child of the BLOCK node (not wrapped in
    /// `EXPR_STMT` or `LET_STMT`) and supplies the block's value.
    pub fn tail_expr(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }
}

/// Statement variants. `let` is a binding; `Assign` is relational
/// assignment to a relvar (`R := <expr>;`); `Truncate` clears a relvar
/// (`truncate R;`, sugar for `R := R minus R`); `Delete` removes matching
/// tuples (`delete R where p;`, sugar for `R := R minus (R where p)`);
/// `Insert` adds tuples (`insert R <source>;`, sugar for `R := R union
/// <source>`); `Update` overwrites attributes (`update R where p { c: e };`,
/// sugar for `R := (R where ¬p) union ((R where p) «sub»)`); `ExprStmt`
/// evaluates an expression and discards the result. `mut` / `return` arrive
/// when their semantics are settled.
#[derive(Debug, Clone)]
pub enum Stmt {
    Let(LetStmt),
    Assign(AssignStmt),
    Truncate(TruncateStmt),
    Delete(DeleteStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    ExprStmt(ExprStmt),
}

impl Stmt {
    pub fn cast(syntax: SyntaxNode) -> Option<Self> {
        Some(match syntax.kind() {
            SyntaxKind::LET_STMT => Stmt::Let(LetStmt { syntax }),
            SyntaxKind::ASSIGN_STMT => Stmt::Assign(AssignStmt { syntax }),
            SyntaxKind::TRUNCATE_STMT => Stmt::Truncate(TruncateStmt { syntax }),
            SyntaxKind::DELETE_STMT => Stmt::Delete(DeleteStmt { syntax }),
            SyntaxKind::INSERT_STMT => Stmt::Insert(InsertStmt { syntax }),
            SyntaxKind::UPDATE_STMT => Stmt::Update(UpdateStmt { syntax }),
            SyntaxKind::EXPR_STMT => Stmt::ExprStmt(ExprStmt { syntax }),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Stmt::Let(s) => s.syntax(),
            Stmt::Assign(s) => s.syntax(),
            Stmt::Truncate(s) => s.syntax(),
            Stmt::Delete(s) => s.syntax(),
            Stmt::Insert(s) => s.syntax(),
            Stmt::Update(s) => s.syntax(),
            Stmt::ExprStmt(s) => s.syntax(),
        }
    }
}

ast_node!(pub LetStmt, LET_STMT);

impl LetStmt {
    /// The binding's name (the IDENT immediately after `let`).
    pub fn name(&self) -> Option<SyntaxToken> {
        // Skip the `let` IDENT (contextual keyword); the binding name
        // is the second IDENT child token.
        nth_token(&self.syntax, SyntaxKind::IDENT, 1)
    }

    /// The optional type annotation: the `TypeRef` child between the
    /// binding name and the `=`. Absent → type inferred from RHS.
    pub fn type_ref(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }

    /// The right-hand-side expression.
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }
}

ast_node!(pub ExprStmt, EXPR_STMT);

impl ExprStmt {
    /// The expression evaluated by this statement (its result is
    /// discarded unless this is the block's tail).
    pub fn expr(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }
}

ast_node!(pub AssignStmt, ASSIGN_STMT);

impl AssignStmt {
    /// The assignment target — the LHS, the first `Expr` child. The parser
    /// accepts any expression here; the typechecker requires a name
    /// reference bound to a private relvar (T0033).
    pub fn target(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The assigned value — the RHS, the second `Expr` child (after `:=`).
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().filter_map(Expr::cast).nth(1)
    }
}

ast_node!(pub TruncateStmt, TRUNCATE_STMT);

impl TruncateStmt {
    /// The relvar to clear — the sole `Expr` child after `truncate`. The
    /// parser accepts any expression; the typechecker requires a bare name
    /// reference bound to an assignable relvar (T0033).
    pub fn operand(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }
}

ast_node!(pub DeleteStmt, DELETE_STMT);

impl DeleteStmt {
    /// The operand — the sole `Expr` child after `delete`. The parser accepts
    /// any expression; the typechecker requires a `where`-restriction
    /// `R where p` over a bare assignable relvar (T0033), the predicate
    /// mandatory (T0052 otherwise — use `truncate`).
    pub fn operand(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }
}

ast_node!(pub InsertStmt, INSERT_STMT);

impl InsertStmt {
    /// The target relvar — the first `Expr` child (a bare `NAME_REF`). The
    /// typechecker requires a name bound to an assignable relvar (T0033).
    pub fn target(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The relation source — the second `Expr` child. For the brace tuple-set
    /// form it is a (keyword-less) `RELATION_LIT`; for the relexpr form it is
    /// the relation expression. Either way it must match the target's heading
    /// (T0034).
    pub fn source(&self) -> Option<Expr> {
        self.syntax.children().filter_map(Expr::cast).nth(1)
    }
}

ast_node!(pub UpdateStmt, UPDATE_STMT);

impl UpdateStmt {
    /// The operand — the sole `Expr` child after `update`: a bare relvar `R`
    /// (`NAME_REF`, update-all) or a restriction `R where p` (`BINARY_EXPR`).
    /// The typechecker requires the root to be a bare assignable relvar (T0033).
    /// (The `{ c: e }` clause lives in the `ARG_LIST` node, which isn't an
    /// `Expr`, so it never collides with this.)
    pub fn operand(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The `{ c: e }` clause — an `ARG_LIST` of `NAMED_ARG`s, the attributes to
    /// overwrite and their values.
    pub fn clause(&self) -> Option<ArgList> {
        child(&self.syntax)
    }

    /// The clause pairs in source order as `(target_name, value_expr)`. Unlike
    /// `replace`, a value may be a constant or a bare reference; each overwrites
    /// its (existing) named attribute (T0053 if it doesn't exist).
    pub fn pairs(&self) -> Vec<(Option<SyntaxToken>, Option<Expr>)> {
        self.clause()
            .map(|al| al.args().map(|na| (na.name(), na.value())).collect())
            .unwrap_or_default()
    }
}

// ── Expressions ──────────────────────────────────────────────────────────

/// Expression variants. The set will grow as the parser does; for now
/// the kinds recognized are name references, literals, brace-call
/// expressions, `transaction` block expressions, tuple literals,
/// relation literals, dot-prefixed field access, Boolean literals,
/// binary (infix) expressions, and unary (prefix) expressions.
#[derive(Debug, Clone)]
pub enum Expr {
    NameRef(NameRef),
    Literal(Literal),
    Call(CallExpr),
    Transaction(TransactionExpr),
    TupleLit(TupleLit),
    RelationLit(RelationLit),
    SequenceLit(SequenceLit),
    FieldAccess(FieldAccess),
    BoolLit(BoolLit),
    Binary(BinaryExpr),
    Unary(UnaryExpr),
    Project(ProjectExpr),
    Replace(ReplaceExpr),
    Extend(ExtendExpr),
    Tclose(TcloseExpr),
    Rename(RenameExpr),
    Wrap(WrapExpr),
    Unwrap(UnwrapExpr),
}

impl Expr {
    pub fn cast(syntax: SyntaxNode) -> Option<Self> {
        Some(match syntax.kind() {
            SyntaxKind::NAME_REF => Expr::NameRef(NameRef { syntax }),
            SyntaxKind::LITERAL => Expr::Literal(Literal { syntax }),
            SyntaxKind::CALL_EXPR => Expr::Call(CallExpr { syntax }),
            SyntaxKind::TRANSACTION_EXPR => Expr::Transaction(TransactionExpr { syntax }),
            SyntaxKind::TUPLE_LIT => Expr::TupleLit(TupleLit { syntax }),
            SyntaxKind::RELATION_LIT => Expr::RelationLit(RelationLit { syntax }),
            SyntaxKind::SEQUENCE_LIT => Expr::SequenceLit(SequenceLit { syntax }),
            SyntaxKind::FIELD_ACCESS => Expr::FieldAccess(FieldAccess { syntax }),
            SyntaxKind::BOOL_LITERAL => Expr::BoolLit(BoolLit { syntax }),
            SyntaxKind::BINARY_EXPR => Expr::Binary(BinaryExpr { syntax }),
            SyntaxKind::UNARY_EXPR => Expr::Unary(UnaryExpr { syntax }),
            SyntaxKind::PROJECT_EXPR => Expr::Project(ProjectExpr { syntax }),
            SyntaxKind::REPLACE_EXPR => Expr::Replace(ReplaceExpr { syntax }),
            SyntaxKind::EXTEND_EXPR => Expr::Extend(ExtendExpr { syntax }),
            SyntaxKind::TCLOSE_EXPR => Expr::Tclose(TcloseExpr { syntax }),
            SyntaxKind::RENAME_EXPR => Expr::Rename(RenameExpr { syntax }),
            SyntaxKind::WRAP_EXPR => Expr::Wrap(WrapExpr { syntax }),
            SyntaxKind::UNWRAP_EXPR => Expr::Unwrap(UnwrapExpr { syntax }),
            // Parenthesized expressions are transparent — recurse to
            // the inner `Expr` so the typechecker / lowerer never see
            // the wrapper. Used purely for precedence grouping.
            SyntaxKind::PAREN_EXPR => return syntax.children().find_map(Expr::cast),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Expr::NameRef(n) => n.syntax(),
            Expr::Literal(l) => l.syntax(),
            Expr::Call(c) => c.syntax(),
            Expr::Transaction(t) => t.syntax(),
            Expr::TupleLit(t) => t.syntax(),
            Expr::RelationLit(r) => r.syntax(),
            Expr::SequenceLit(s) => s.syntax(),
            Expr::FieldAccess(f) => f.syntax(),
            Expr::BoolLit(b) => b.syntax(),
            Expr::Binary(b) => b.syntax(),
            Expr::Unary(u) => u.syntax(),
            Expr::Project(p) => p.syntax(),
            Expr::Replace(r) => r.syntax(),
            Expr::Extend(e) => e.syntax(),
            Expr::Tclose(t) => t.syntax(),
            Expr::Rename(r) => r.syntax(),
            Expr::Wrap(w) => w.syntax(),
            Expr::Unwrap(u) => u.syntax(),
        }
    }
}

ast_node!(pub TransactionExpr, TRANSACTION_EXPR);

impl TransactionExpr {
    /// The block body. `None` only on parse-recovery edge cases.
    pub fn body(&self) -> Option<Block> {
        child(&self.syntax)
    }
}

ast_node!(pub NameRef, NAME_REF);

impl NameRef {
    /// The identifier token. Always present in a well-formed `NAME_REF`;
    /// `None` would indicate a parse-recovery edge case.
    pub fn ident(&self) -> Option<SyntaxToken> {
        first_token_of_kind(&self.syntax, SyntaxKind::IDENT)
    }
}

ast_node!(pub Literal, LITERAL);

impl Literal {
    /// The underlying literal token. Its `kind()` distinguishes
    /// `STRING_LIT` / `FORMAT_STRING_LIT` / `CHAR_LIT` / `INTEGER_LIT` /
    /// `RATIONAL_LIT` / `APPROXIMATE_LIT`.
    pub fn token(&self) -> Option<SyntaxToken> {
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| {
                matches!(
                    t.kind(),
                    SyntaxKind::STRING_LIT
                        | SyntaxKind::FORMAT_STRING_LIT
                        | SyntaxKind::CHAR_LIT
                        | SyntaxKind::INTEGER_LIT
                        | SyntaxKind::RATIONAL_LIT
                        | SyntaxKind::APPROXIMATE_LIT
                )
            })
    }
}

ast_node!(pub CallExpr, CALL_EXPR);

impl CallExpr {
    /// The expression in the callee position — `write_line` in
    /// `write_line { … }`. Today always a `NameRef` in practice, but
    /// the return type is `Expr` so chained postfix forms (e.g.
    /// `obj.method{ … }`) work naturally once they land.
    pub fn callee(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The brace-delimited argument list.
    pub fn args(&self) -> Option<ArgList> {
        child(&self.syntax)
    }
}

// ── Argument lists ───────────────────────────────────────────────────────

ast_node!(pub ArgList, ARG_LIST);

impl ArgList {
    /// All named arguments in source order.
    pub fn args(&self) -> impl Iterator<Item = NamedArg> + '_ {
        children(&self.syntax)
    }
}

ast_node!(pub NamedArg, NAMED_ARG);

impl NamedArg {
    /// The parameter name. For the explicit `name: value` form it's the
    /// leading bare `IDENT` before the colon. For the field-init shorthand
    /// `name` (≡ `name: name`) there is no bare leading `IDENT` — the name is
    /// wrapped in the value `NAME_REF` — so fall back to that name-ref's
    /// identifier. (`first_token_of_kind` only scans direct children, so the
    /// wrapped `IDENT` is invisible to it.)
    pub fn name(&self) -> Option<SyntaxToken> {
        first_token_of_kind(&self.syntax, SyntaxKind::IDENT).or_else(|| match self.value()? {
            Expr::NameRef(n) => n.ident(),
            _ => None,
        })
    }

    /// The value expression. `name: value` yields `value`; the shorthand
    /// `name` yields the `NAME_REF` wrapping the name (so consumers resolve
    /// and lower it exactly like the explicit `name: name`).
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }
}

// ── Tuple literals + field access ────────────────────────────────────────

ast_node!(pub TupleLit, TUPLE_LIT);

impl TupleLit {
    /// All named fields in source order. Each field shares the
    /// `NamedArg` view with call-site arguments — same `name: value`
    /// shape, different parent node.
    pub fn fields(&self) -> impl Iterator<Item = NamedArg> + '_ {
        children(&self.syntax)
    }
}

ast_node!(pub RelationLit, RELATION_LIT);

impl RelationLit {
    /// All tuples in source order. Each is a nested `TUPLE_LIT` child.
    /// An empty relation literal yields zero elements.
    pub fn tuples(&self) -> impl Iterator<Item = TupleLit> + '_ {
        children(&self.syntax)
    }
}

ast_node!(pub SequenceLit, SEQUENCE_LIT);

impl SequenceLit {
    /// All elements in source order. An empty `Sequence []` yields zero.
    pub fn elements(&self) -> impl Iterator<Item = Expr> + '_ {
        self.syntax.children().filter_map(Expr::cast)
    }
}

// ── Boolean literals + binary expressions (Phase 20) ─────────────────

// `true` / `false` Boolean literal. Wraps the contextual-keyword IDENT
// token. (Doc comment must be a `//` block here — `ast_node!` is a
// macro and rustdoc can't carry an outer doc comment through the
// macro invocation.)
ast_node!(pub BoolLit, BOOL_LITERAL);

impl BoolLit {
    /// The literal's value. `None` is a parse-recovery edge case.
    pub fn value(&self) -> Option<bool> {
        let tok = self
            .syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| t.kind() == SyntaxKind::IDENT)?;
        match tok.text() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }
}

/// Binary infix operator kinds — `=`, `<>`, `<`, `>`, `<=`, `>=`,
/// `and`, `or`, `where`, `join`, `times`, `compose`. `where`'s operands are
/// (relation, predicate) and `join`'s / `times`'s / `compose`'s are
/// (relation, relation); the rest are scalar. The typechecker dispatches on
/// this enum to apply the right operand-type rules. `Join` is the relational
/// natural join (Algebra-A AND), distinct from the scalar boolean `And`;
/// `Times` is the Cartesian product — the same AND, typed to require disjoint
/// headings; `Compose` is the natural join with the shared attributes removed
/// (AND then REMOVE), typed to require overlapping headings like `Join`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
    Where,
    Join,
    Times,
    Compose,
    Intersect,
    Union,
    Minus,
    /// Scalar arithmetic: `Integer × Integer → Integer` (integer division
    /// truncates toward zero). The symbolic `-` (token `MINUS`) is `Sub`,
    /// distinct from the relational `minus` keyword (`Minus`).
    Add,
    Sub,
    Mul,
    Div,
    /// Concatenation `||`: `(Text|Character) × (Text|Character) → Text`.
    Concat,
}

ast_node!(pub BinaryExpr, BINARY_EXPR);

impl BinaryExpr {
    /// Left operand (first `Expr` child in source order).
    pub fn lhs(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// Right operand (second `Expr` child).
    pub fn rhs(&self) -> Option<Expr> {
        self.syntax.children().filter_map(Expr::cast).nth(1)
    }

    /// The operator token between the operands. For symbolic ops
    /// (`=`, `<`, `>`, `<=`, `>=`, `<>`) the token's kind identifies
    /// the operator; for keyword ops (`and`, `or`, `where`) it's an
    /// IDENT whose text picks the variant.
    pub fn op_token(&self) -> Option<SyntaxToken> {
        // Walk the direct children-with-tokens; the operator is the
        // first token between two Expr nodes that's either a symbolic
        // operator kind or an `and`/`or`/`where` IDENT.
        for el in self.syntax.children_with_tokens() {
            if let Some(tok) = el.into_token() {
                match tok.kind() {
                    SyntaxKind::EQ
                    | SyntaxKind::NOT_EQ
                    | SyntaxKind::LT
                    | SyntaxKind::GT
                    | SyntaxKind::LT_EQ
                    | SyntaxKind::GT_EQ
                    | SyntaxKind::PLUS
                    | SyntaxKind::MINUS
                    | SyntaxKind::STAR
                    | SyntaxKind::SLASH
                    | SyntaxKind::PIPE_PIPE => return Some(tok),
                    SyntaxKind::IDENT
                        if matches!(
                            tok.text(),
                            "and" | "or" | "where" | "join" | "times" | "compose" | "intersect"
                                | "union" | "minus"
                        ) =>
                    {
                        return Some(tok);
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Resolve the operator token to a `BinaryOp` variant. `None` is a
    /// parse-recovery edge case (no operator token between operands).
    pub fn op_kind(&self) -> Option<BinaryOp> {
        let tok = self.op_token()?;
        Some(match tok.kind() {
            SyntaxKind::EQ => BinaryOp::Eq,
            SyntaxKind::NOT_EQ => BinaryOp::NotEq,
            SyntaxKind::LT => BinaryOp::Lt,
            SyntaxKind::GT => BinaryOp::Gt,
            SyntaxKind::LT_EQ => BinaryOp::LtEq,
            SyntaxKind::GT_EQ => BinaryOp::GtEq,
            SyntaxKind::PLUS => BinaryOp::Add,
            SyntaxKind::MINUS => BinaryOp::Sub,
            SyntaxKind::STAR => BinaryOp::Mul,
            SyntaxKind::SLASH => BinaryOp::Div,
            SyntaxKind::PIPE_PIPE => BinaryOp::Concat,
            SyntaxKind::IDENT => match tok.text() {
                "and" => BinaryOp::And,
                "or" => BinaryOp::Or,
                "where" => BinaryOp::Where,
                "join" => BinaryOp::Join,
                "times" => BinaryOp::Times,
                "compose" => BinaryOp::Compose,
                "intersect" => BinaryOp::Intersect,
                "union" => BinaryOp::Union,
                "minus" => BinaryOp::Minus,
                _ => return None,
            },
            _ => return None,
        })
    }
}

/// Unary prefix operator kinds. Phase 21 ships `Extract` only —
/// future unary ops (`not`, unary `-`) slot in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `extract <relexpr>` — TTM RM Pre 10 cardinality-checked
    /// relation-to-tuple primitive.
    Extract,
}

ast_node!(pub UnaryExpr, UNARY_EXPR);

impl UnaryExpr {
    /// The operand expression — the single `Expr` child.
    pub fn operand(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The operator's keyword token. Phase 21's only unary op is
    /// `extract`, recognized via its IDENT lexeme.
    pub fn op_token(&self) -> Option<SyntaxToken> {
        for el in self.syntax.children_with_tokens() {
            if let Some(tok) = el.into_token() {
                if tok.kind() == SyntaxKind::IDENT && matches!(tok.text(), "extract") {
                    return Some(tok);
                }
            }
        }
        None
    }

    /// Resolve the operator token to a `UnaryOp`. `None` is a
    /// parse-recovery edge case (no recognized prefix token).
    pub fn op_kind(&self) -> Option<UnaryOp> {
        let tok = self.op_token()?;
        Some(match tok.text() {
            "extract" => UnaryOp::Extract,
            _ => return None,
        })
    }
}

ast_node!(pub ProjectExpr, PROJECT_EXPR);

impl ProjectExpr {
    /// The relation operand being projected — the single `Expr` child
    /// (the attribute names are bare tokens, not child nodes).
    pub fn input(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// Whether this is the `project all but { … }` form, which removes the
    /// named attributes (keeping the complement) rather than keeping them.
    /// True iff an IDENT token before the `{` is the contextual keyword `all`.
    pub fn is_all_but(&self) -> bool {
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .take_while(|t| t.kind() != SyntaxKind::L_BRACE)
            .any(|t| t.kind() == SyntaxKind::IDENT && t.text() == "all")
    }

    /// The attribute names listed in the braces, in source order. These are
    /// the kept set for `project { … }`, or the removed set for
    /// `project all but { … }`.
    ///
    /// The operand is a child *node* (its IDENTs are nested inside it) and the
    /// `project` / `all` / `but` keyword tokens precede the `{`, so the brace
    /// names are exactly the IDENT *tokens* that appear after the `L_BRACE`.
    pub fn attrs(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        let mut after_brace = false;
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter_map(move |t| {
                if t.kind() == SyntaxKind::L_BRACE {
                    after_brace = true;
                    None
                } else if after_brace && t.kind() == SyntaxKind::IDENT {
                    Some(t)
                } else {
                    None
                }
            })
    }
}

ast_node!(pub ReplaceExpr, REPLACE_EXPR);

impl ReplaceExpr {
    /// The relation operand being replaced — the single `Expr` child (the
    /// `new: e` pairs live in the `ARG_LIST` node, which isn't an `Expr`).
    pub fn input(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The `{ new: e }` pair list — an `ARG_LIST` of `NAMED_ARG`s. The left of
    /// each colon is the new attribute name; the right is the value expression.
    pub fn arg_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }

    /// The pairs in source order as `(new_name, value_expr)`: the `NAMED_ARG`
    /// name (the new/target attribute) and its value expression. The typechecker
    /// requires every value to compute (read ≥1 attribute); a bare `NameRef` is
    /// a pure relabel and belongs to `rename` (T0047).
    pub fn pairs(&self) -> Vec<(Option<SyntaxToken>, Option<Expr>)> {
        self.arg_list()
            .map(|al| al.args().map(|na| (na.name(), na.value())).collect())
            .unwrap_or_default()
    }
}

ast_node!(pub RenameExpr, RENAME_EXPR);

impl RenameExpr {
    /// The relation operand being renamed — the single `Expr` child (the
    /// `new: old` pairs live in the `ARG_LIST` node, which isn't an `Expr`).
    pub fn input(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The `{ new: old }` pair list — an `ARG_LIST` of `NAMED_ARG`s. The left
    /// of each colon is the new attribute name; the right is the source
    /// attribute (a bare attribute reference).
    pub fn arg_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }

    /// The pairs in source order as `(new_name, value_expr)`: the `NAMED_ARG`
    /// name (the new/target attribute) and its value expression. The
    /// typechecker requires every value to be a bare `NameRef` (the source
    /// attribute) and emits T0030 otherwise.
    pub fn pairs(&self) -> Vec<(Option<SyntaxToken>, Option<Expr>)> {
        self.arg_list()
            .map(|al| al.args().map(|na| (na.name(), na.value())).collect())
            .unwrap_or_default()
    }

    /// The relabel view: `(old, new)` name tokens, where `old` is the value's
    /// bare-`NameRef` identifier (the source attribute) and `new` is the
    /// `NAMED_ARG` name (the target). `old` is `None` when the value isn't a
    /// bare attribute name — which the typechecker has already rejected (T0030),
    /// so by lowering time every pair is a clean relabel.
    pub fn renames(&self) -> Vec<(Option<SyntaxToken>, Option<SyntaxToken>)> {
        self.arg_list()
            .map(|al| {
                al.args()
                    .map(|na| {
                        let new = na.name();
                        let old = match na.value() {
                            Some(Expr::NameRef(n)) => n.ident(),
                            _ => None,
                        };
                        (old, new)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

ast_node!(pub WrapExpr, WRAP_EXPR);

impl WrapExpr {
    /// The relation operand being wrapped — the single `Expr` child (the
    /// `new: { … }` pairs are `WRAP_PAIR` nodes, not `Expr`s).
    pub fn input(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The `new: { a, b }` pairs in source order.
    pub fn pairs(&self) -> impl Iterator<Item = WrapPair> + '_ {
        children(&self.syntax)
    }
}

ast_node!(pub WrapPair, WRAP_PAIR);

impl WrapPair {
    /// The new tuple-valued attribute name — the IDENT token before the `{`.
    pub fn name(&self) -> Option<SyntaxToken> {
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .take_while(|t| t.kind() != SyntaxKind::L_BRACE)
            .find(|t| t.kind() == SyntaxKind::IDENT)
    }

    /// The existing attribute names to group into the tuple — the IDENT tokens
    /// after the `{` (same shape as `ProjectExpr::attrs`).
    pub fn wrapped(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        let mut after_brace = false;
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter_map(move |t| {
                if t.kind() == SyntaxKind::L_BRACE {
                    after_brace = true;
                    None
                } else if after_brace && t.kind() == SyntaxKind::IDENT {
                    Some(t)
                } else {
                    None
                }
            })
    }
}

ast_node!(pub UnwrapExpr, UNWRAP_EXPR);

impl UnwrapExpr {
    /// The relation operand being unwrapped — the single `Expr` child (the
    /// attribute names are bare tokens, not child nodes).
    pub fn input(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The tuple-valued attribute names to expand, in source order — the IDENT
    /// tokens after the `{` (same shape as `ProjectExpr::attrs`).
    pub fn attrs(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        let mut after_brace = false;
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter_map(move |t| {
                if t.kind() == SyntaxKind::L_BRACE {
                    after_brace = true;
                    None
                } else if after_brace && t.kind() == SyntaxKind::IDENT {
                    Some(t)
                } else {
                    None
                }
            })
    }
}

ast_node!(pub ExtendExpr, EXTEND_EXPR);

impl ExtendExpr {
    /// The relation operand being extended — the single `Expr` child (the
    /// `new: e` pairs live in the `ARG_LIST` node, which isn't an `Expr`).
    pub fn input(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The `{ new: e }` pair list — an `ARG_LIST` of `NAMED_ARG`s.
    pub fn arg_list(&self) -> Option<ArgList> {
        child(&self.syntax)
    }

    /// The pairs in source order as `(new_name, value_expr)`: the `NAMED_ARG`
    /// name (the new attribute) and its value expression. Unlike `replace`,
    /// the value is a general scalar expression (the computed attribute's
    /// value); `extend` adds it without removing anything.
    pub fn pairs(&self) -> Vec<(Option<SyntaxToken>, Option<Expr>)> {
        self.arg_list()
            .map(|al| al.args().map(|na| (na.name(), na.value())).collect())
            .unwrap_or_default()
    }
}

ast_node!(pub TcloseExpr, TCLOSE_EXPR);

impl TcloseExpr {
    /// The relation operand whose transitive closure is taken — the single
    /// `Expr` child (the optional `{ a, b }` attribute names are bare tokens,
    /// not child nodes).
    pub fn input(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The two attribute names listed in the optional braces, in source order
    /// (`R tclose { a, b }` ≡ `(R project { a, b }) tclose`). Absent braces
    /// yield zero names — the bare `R tclose` form, where the operand must
    /// already be binary.
    ///
    /// The operand is a child *node* (its IDENTs are nested inside it) and the
    /// `tclose` keyword token precedes the `{`, so the brace names are exactly
    /// the IDENT *tokens* that appear after the `L_BRACE`. (Same shape as
    /// `ProjectExpr::attrs`.)
    pub fn attrs(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        let mut after_brace = false;
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter_map(move |t| {
                if t.kind() == SyntaxKind::L_BRACE {
                    after_brace = true;
                    None
                } else if after_brace && t.kind() == SyntaxKind::IDENT {
                    Some(t)
                } else {
                    None
                }
            })
    }
}

ast_node!(pub FieldAccess, FIELD_ACCESS);

impl FieldAccess {
    /// The expression being projected from. `None` is a parse-recovery
    /// edge case (e.g. a `.` with no preceding primary).
    pub fn base(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }

    /// The post-`.` field-name token. `None` when the parser recovered
    /// past a missing identifier (P0030).
    pub fn field(&self) -> Option<SyntaxToken> {
        // The base expression contains its own IDENTs (e.g., NAME_REF);
        // the field's IDENT is the one that lives directly under the
        // FIELD_ACCESS node *after* the `.` token. Walk the direct
        // child elements and pick the IDENT that follows DOT.
        let mut seen_dot = false;
        for el in self.syntax.children_with_tokens() {
            if let Some(tok) = el.into_token() {
                match tok.kind() {
                    SyntaxKind::DOT => seen_dot = true,
                    SyntaxKind::IDENT if seen_dot => return Some(tok),
                    _ => {}
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_kind::FileKind;
    use crate::parse;
    use coddl_diagnostics::FileId;

    fn ast(src: &str) -> Root {
        let out = parse(src, FileId(0), FileKind::Cd);
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        Root::cast(out.tree).expect("root")
    }

    #[test]
    fn root_items_in_order() {
        let root = ast("program p; database d; oper f {} [];");
        let kinds: Vec<_> = root
            .items()
            .map(|i| match i {
                Item::ProgramDecl(_) => "program",
                Item::DatabaseBinding(_) => "database",
                Item::PublicRelvarDecl(_) => "public_relvar",
                Item::PrivateRelvarDecl(_) => "private_relvar",
                Item::BaseRelvarDecl(_) => "base_relvar",
                Item::VirtualRelvarDecl(_) => "virtual_relvar",
                Item::OperDecl(_) => "oper",
            })
            .collect();
        assert_eq!(kinds, vec!["program", "database", "oper"]);
    }

    #[test]
    fn public_relvar_walks_to_name_heading_keys() {
        let src = "public relvar Greetings { id: Integer, message: Text } key { id };";
        let root = ast(src);
        let Item::PublicRelvarDecl(decl) = root.items().next().unwrap() else {
            panic!("expected PublicRelvarDecl");
        };
        assert_eq!(decl.name().unwrap().text(), "Greetings");
        let heading = decl.heading().unwrap();
        assert_eq!(heading.params().count(), 2);
        let keys: Vec<_> = decl.key_clauses().collect();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].attrs().any(|t| t.text() == "id"));
    }

    #[test]
    fn private_relvar_with_multi_key() {
        let src = "private relvar P { a: Integer, b: Integer } key { a } key { b };";
        let root = ast(src);
        let Item::PrivateRelvarDecl(decl) = root.items().next().unwrap() else {
            panic!("expected PrivateRelvarDecl");
        };
        assert_eq!(decl.name().unwrap().text(), "P");
        let keys: Vec<_> = decl.key_clauses().collect();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn base_relvar_in_cd_dialect_appears_as_base_item() {
        // `.cd` parses base/virtual so the typechecker can emit T0014;
        // here we verify the AST surface routes the resulting node to
        // `Item::BaseRelvarDecl`.
        let src = "base relvar Greetings { id: Integer } key { id };";
        let root = ast(src);
        assert!(matches!(
            root.items().next().unwrap(),
            Item::BaseRelvarDecl(_)
        ));
    }

    #[test]
    fn database_binding_name_resolves() {
        let root = ast("program p; database greetings;");
        let mut items = root.items();
        let _ = items.next(); // skip program
        let Item::DatabaseBinding(binding) = items.next().unwrap() else {
            panic!("expected DatabaseBinding");
        };
        assert_eq!(binding.name().unwrap().text(), "greetings");
    }

    #[test]
    fn program_decl_name() {
        let root = ast("program hello_world;");
        let Item::ProgramDecl(p) = root.items().next().unwrap() else {
            panic!("expected ProgramDecl");
        };
        assert_eq!(p.name().unwrap().text(), "hello_world");
    }

    #[test]
    fn oper_decl_with_params() {
        let root = ast("oper add { x: Integer, y: Integer } [];");
        let Item::OperDecl(o) = root.items().next().unwrap() else {
            panic!("expected OperDecl");
        };
        assert_eq!(o.name().unwrap().text(), "add");

        let heading = o.heading().expect("heading");
        let params: Vec<_> = heading.params().collect();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name().unwrap().text(), "x");
        assert_eq!(
            params[0].type_ref().unwrap().name().unwrap().text(),
            "Integer"
        );
        assert_eq!(params[1].name().unwrap().text(), "y");
        assert_eq!(
            params[1].type_ref().unwrap().name().unwrap().text(),
            "Integer"
        );

        let body = o.body().expect("body");
        assert_eq!(body.statements().count(), 0);
    }

    #[test]
    fn empty_heading_and_empty_body() {
        let root = ast("oper main {} [];");
        let Item::OperDecl(o) = root.items().next().unwrap() else {
            panic!();
        };
        assert_eq!(o.heading().unwrap().params().count(), 0);
        assert_eq!(o.body().unwrap().statements().count(), 0);
    }

    #[test]
    fn multi_stmt_block() {
        let root = ast("oper main {} [ a; b; c; ];");
        let Item::OperDecl(o) = root.items().next().unwrap() else {
            panic!();
        };
        let stmts: Vec<_> = o.body().unwrap().statements().collect();
        assert_eq!(stmts.len(), 3);
        for s in &stmts {
            let Stmt::ExprStmt(e) = s else {
                panic!("expected ExprStmt, got {s:?}")
            };
            match e.expr().unwrap() {
                Expr::NameRef(_) => {}
                other => panic!("expected NameRef, got {other:?}"),
            }
        }
    }

    #[test]
    fn hello_world_full_traversal() {
        let src = "program hello_world;\n\
                   \n\
                   oper main {}\n\
                   [\n\
                       write_line{message: \"Hello, world!\"};\n\
                   ];\n";
        let root = ast(src);

        let items: Vec<_> = root.items().collect();
        assert_eq!(items.len(), 2);

        // 1. program decl
        let Item::ProgramDecl(p) = &items[0] else {
            panic!()
        };
        assert_eq!(p.name().unwrap().text(), "hello_world");

        // 2. oper decl
        let Item::OperDecl(o) = &items[1] else {
            panic!()
        };
        assert_eq!(o.name().unwrap().text(), "main");
        assert_eq!(o.heading().unwrap().params().count(), 0);

        // 3. body has one EXPR_STMT
        let stmts: Vec<_> = o.body().unwrap().statements().collect();
        assert_eq!(stmts.len(), 1);
        let Stmt::ExprStmt(expr_stmt) = &stmts[0] else {
            panic!("expected ExprStmt")
        };

        // 4. statement is a call expression
        let Expr::Call(call) = expr_stmt.expr().unwrap() else {
            panic!("expected call");
        };

        // 5. callee is `write_line` (a NameRef)
        let Expr::NameRef(callee) = call.callee().unwrap() else {
            panic!("expected NameRef callee");
        };
        assert_eq!(callee.ident().unwrap().text(), "write_line");

        // 6. argument list has one named arg: `message: "Hello, world!"`
        let args: Vec<_> = call.args().unwrap().args().collect();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].name().unwrap().text(), "message");

        // 7. the value is a string literal
        let Expr::Literal(lit) = args[0].value().unwrap() else {
            panic!("expected literal");
        };
        let tok = lit.token().unwrap();
        assert_eq!(tok.kind(), SyntaxKind::STRING_LIT);
        assert_eq!(tok.text(), "\"Hello, world!\"");
    }

    #[test]
    fn tuple_lit_fields_iterate() {
        let root = ast("oper f {} [ let t = {a: 1, b: \"x\"}; ];");
        // Drill: oper → block → let_stmt → tuple_lit
        let tup_node = root
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .unwrap();
        let tup = TupleLit::cast(tup_node).unwrap();
        let fields: Vec<NamedArg> = tup.fields().collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name().unwrap().text(), "a");
        assert_eq!(fields[1].name().unwrap().text(), "b");
    }

    #[test]
    fn empty_tuple_lit_has_no_fields() {
        let root = ast("oper f {} [ let t = {}; ];");
        let tup_node = root
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::TUPLE_LIT)
            .unwrap();
        let tup = TupleLit::cast(tup_node).unwrap();
        assert_eq!(tup.fields().count(), 0);
    }

    #[test]
    fn field_access_resolves_base_and_field() {
        let root = ast("oper f {} [ t.message; ];");
        let fa_node = root
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FIELD_ACCESS)
            .unwrap();
        let fa = FieldAccess::cast(fa_node).unwrap();
        let Expr::NameRef(base) = fa.base().unwrap() else {
            panic!("expected NameRef base");
        };
        assert_eq!(base.ident().unwrap().text(), "t");
        assert_eq!(fa.field().unwrap().text(), "message");
    }

    #[test]
    fn chained_field_access_base_is_inner_field_access() {
        let root = ast("oper f {} [ t.a.b; ];");
        let outer_node = root
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::FIELD_ACCESS)
            .unwrap();
        let outer = FieldAccess::cast(outer_node).unwrap();
        assert_eq!(outer.field().unwrap().text(), "b");
        let Expr::FieldAccess(inner) = outer.base().unwrap() else {
            panic!("expected nested FieldAccess");
        };
        assert_eq!(inner.field().unwrap().text(), "a");
    }

    #[test]
    fn relation_lit_tuples_iterate() {
        let root = ast("oper f {} [ let r = Relation { {a: 1}, {a: 2} }; ];");
        let rel_node = root
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RELATION_LIT)
            .unwrap();
        let rel = RelationLit::cast(rel_node).unwrap();
        let tuples: Vec<TupleLit> = rel.tuples().collect();
        assert_eq!(tuples.len(), 2);
        assert_eq!(tuples[0].fields().count(), 1);
        assert_eq!(tuples[0].fields().next().unwrap().name().unwrap().text(), "a");
    }

    #[test]
    fn empty_relation_lit_has_no_tuples() {
        let root = ast("oper f {} [ let r = Relation {}; ];");
        let rel_node = root
            .syntax()
            .descendants()
            .find(|n| n.kind() == SyntaxKind::RELATION_LIT)
            .unwrap();
        let rel = RelationLit::cast(rel_node).unwrap();
        assert_eq!(rel.tuples().count(), 0);
    }

    // ── BoolLit + BinaryExpr (Phase 20) ──────────────────────────────

    #[test]
    fn bool_literals_resolve_to_true_false() {
        let root = ast("oper f {} [ let t = true; let g = false; ];");
        let lits: Vec<BoolLit> = root
            .syntax()
            .descendants()
            .filter_map(BoolLit::cast)
            .collect();
        assert_eq!(lits.len(), 2);
        assert_eq!(lits[0].value(), Some(true));
        assert_eq!(lits[1].value(), Some(false));
    }

    #[test]
    fn binary_expr_op_kinds_round_trip() {
        let cases = [
            ("1 = 2", BinaryOp::Eq),
            ("1 <> 2", BinaryOp::NotEq),
            ("1 < 2", BinaryOp::Lt),
            ("1 > 2", BinaryOp::Gt),
            ("1 <= 2", BinaryOp::LtEq),
            ("1 >= 2", BinaryOp::GtEq),
            ("true and false", BinaryOp::And),
            ("true or false", BinaryOp::Or),
            ("1 + 2", BinaryOp::Add),
            ("1 - 2", BinaryOp::Sub),
            ("1 * 2", BinaryOp::Mul),
            ("1 / 2", BinaryOp::Div),
            ("1 || 2", BinaryOp::Concat),
        ];
        for (rhs, expected) in cases {
            let src = format!("oper f {{}} [ let b = {rhs}; ];");
            let root = ast(&src);
            let bin = root
                .syntax()
                .descendants()
                .find_map(BinaryExpr::cast)
                .unwrap_or_else(|| panic!("no BinaryExpr for `{rhs}`"));
            assert_eq!(bin.op_kind(), Some(expected), "for `{rhs}`");
        }
    }

    #[test]
    fn where_binary_expr_has_relation_lhs_and_predicate_rhs() {
        // Use a NameRef `R` on the lhs to keep the test focused on
        // the BinaryExpr shape. (The typechecker will reject `R` as
        // unresolved, but the parse tree is what we're checking.)
        let root = ast("oper f {} [ let s = R where a = 1; ];");
        let outer = root
            .syntax()
            .descendants()
            .find_map(BinaryExpr::cast)
            .expect("outer BinaryExpr");
        assert_eq!(outer.op_kind(), Some(BinaryOp::Where));
        match outer.lhs() {
            Some(Expr::NameRef(_)) => {}
            other => panic!("expected NameRef lhs, got {other:?}"),
        }
        match outer.rhs() {
            Some(Expr::Binary(inner)) => {
                assert_eq!(inner.op_kind(), Some(BinaryOp::Eq));
            }
            other => panic!("expected Binary rhs, got {other:?}"),
        }
    }

    #[test]
    fn extract_unary_expr_carries_extract_op_kind() {
        let root = ast("oper f {} [ let t = extract R; ];");
        let ue = root
            .syntax()
            .descendants()
            .find_map(UnaryExpr::cast)
            .expect("UnaryExpr in tree");
        assert_eq!(ue.op_kind(), Some(UnaryOp::Extract));
        match ue.operand() {
            Some(Expr::NameRef(n)) => {
                assert_eq!(n.ident().unwrap().text(), "R");
            }
            other => panic!("expected NameRef operand, got {other:?}"),
        }
    }
}

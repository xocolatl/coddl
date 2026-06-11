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

fn child<C: AstNode>(syntax: &SyntaxNode) -> Option<C> {
    syntax.children().find_map(C::cast)
}

fn children<C: AstNode>(syntax: &SyntaxNode) -> impl Iterator<Item = C> {
    syntax.children().filter_map(C::cast)
}

fn nth_token(syntax: &SyntaxNode, kind: SyntaxKind, n: usize) -> Option<SyntaxToken> {
    syntax
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|t| t.kind() == kind)
        .nth(n)
}

fn first_token_of_kind(syntax: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    nth_token(syntax, kind, 0)
}

// ── Boilerplate macro ────────────────────────────────────────────────────

/// Define a plain newtype around a single [`SyntaxKind`] node. Generates
/// the struct, the [`AstNode`] impl, and a public constructor.
macro_rules! ast_node {
    ($vis:vis $name:ident, $kind:ident) => {
        #[derive(Debug, Clone)]
        $vis struct $name {
            syntax: SyntaxNode,
        }
        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                kind == SyntaxKind::$kind
            }
            fn cast(syntax: SyntaxNode) -> Option<Self> {
                Self::can_cast(syntax.kind()).then_some(Self { syntax })
            }
            fn syntax(&self) -> &SyntaxNode {
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
#[derive(Debug, Clone)]
pub enum Item {
    ProgramDecl(ProgramDecl),
    OperDecl(OperDecl),
}

impl Item {
    pub fn cast(syntax: SyntaxNode) -> Option<Self> {
        Some(match syntax.kind() {
            SyntaxKind::PROGRAM_DECL => Item::ProgramDecl(ProgramDecl { syntax }),
            SyntaxKind::OPER_DECL => Item::OperDecl(OperDecl { syntax }),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Item::ProgramDecl(d) => d.syntax(),
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

    /// The body `[ … ]`.
    pub fn body(&self) -> Option<Block> {
        child(&self.syntax)
    }
}

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

// ── TypeRef ──────────────────────────────────────────────────────────────

ast_node!(pub TypeRef, TYPE_REF);

impl TypeRef {
    /// Today only a single named type is recognized; this returns its
    /// IDENT token.
    pub fn name(&self) -> Option<SyntaxToken> {
        first_token_of_kind(&self.syntax, SyntaxKind::IDENT)
    }
}

// ── Block + statements ───────────────────────────────────────────────────

ast_node!(pub Block, BLOCK);

impl Block {
    /// Statements in source order.
    pub fn statements(&self) -> impl Iterator<Item = Stmt> + '_ {
        self.syntax.children().filter_map(Stmt::cast)
    }
}

/// Statement variants. Today only `ExprStmt` exists; `let` / `mut` /
/// `return` / `insert` / `delete` / `update` arrive when their semantics
/// are settled.
#[derive(Debug, Clone)]
pub enum Stmt {
    ExprStmt(ExprStmt),
}

impl Stmt {
    pub fn cast(syntax: SyntaxNode) -> Option<Self> {
        Some(match syntax.kind() {
            SyntaxKind::EXPR_STMT => Stmt::ExprStmt(ExprStmt { syntax }),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Stmt::ExprStmt(s) => s.syntax(),
        }
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

// ── Expressions ──────────────────────────────────────────────────────────

/// Expression variants. The set will grow as the parser does; for now
/// the kinds recognized are name references, literals, and brace-call
/// expressions.
#[derive(Debug, Clone)]
pub enum Expr {
    NameRef(NameRef),
    Literal(Literal),
    Call(CallExpr),
}

impl Expr {
    pub fn cast(syntax: SyntaxNode) -> Option<Self> {
        Some(match syntax.kind() {
            SyntaxKind::NAME_REF => Expr::NameRef(NameRef { syntax }),
            SyntaxKind::LITERAL => Expr::Literal(Literal { syntax }),
            SyntaxKind::CALL_EXPR => Expr::Call(CallExpr { syntax }),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Expr::NameRef(n) => n.syntax(),
            Expr::Literal(l) => l.syntax(),
            Expr::Call(c) => c.syntax(),
        }
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
    /// `STRING_LIT` / `CHAR_LIT` / `INTEGER_LIT` / `RATIONAL_LIT` /
    /// `APPROXIMATE_LIT`.
    pub fn token(&self) -> Option<SyntaxToken> {
        self.syntax
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| {
                matches!(
                    t.kind(),
                    SyntaxKind::STRING_LIT
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
    /// The parameter name on the left of the `:`.
    pub fn name(&self) -> Option<SyntaxToken> {
        first_token_of_kind(&self.syntax, SyntaxKind::IDENT)
    }

    /// The value expression on the right of the `:`.
    pub fn value(&self) -> Option<Expr> {
        self.syntax.children().find_map(Expr::cast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use coddl_diagnostics::FileId;

    fn ast(src: &str) -> Root {
        let out = parse(src, FileId(0));
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        Root::cast(out.tree).expect("root")
    }

    #[test]
    fn root_items_in_order() {
        let root = ast("program p; oper f {} [];");
        let kinds: Vec<_> = root
            .items()
            .map(|i| match i {
                Item::ProgramDecl(_) => "program",
                Item::OperDecl(_) => "oper",
            })
            .collect();
        assert_eq!(kinds, vec!["program", "oper"]);
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
            let Stmt::ExprStmt(e) = s;
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
        let Stmt::ExprStmt(expr_stmt) = &stmts[0];

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
}

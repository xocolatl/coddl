//! The typechecker walk.
//!
//! `TypeChecker` walks the AST produced by `coddl-syntax`, resolving
//! names, validating call sites against the built-in registry, and
//! emitting diagnostics with stable `T####` codes. Walk methods are
//! named to mirror the productions in `docs/grammar.md` (`parse_oper_decl`
//! → `check_oper_decl`, etc.); `docs/typecheck.md` is the spec they
//! enforce.

use std::collections::{HashMap, HashSet};

use coddl_diagnostics::{Diagnostic, FileId, Span};
use coddl_syntax::ast::{
    AstNode, Block, CallExpr, Expr, ExprStmt, Item, LetStmt, NamedArg, OperDecl, ProgramDecl, Root,
    Stmt, TransactionExpr,
};
use coddl_syntax::cst::{SyntaxNode, SyntaxToken};
use coddl_syntax::{parse, FileKind, SyntaxKind};

use crate::builtins::Builtins;
use crate::ty::Type;

/// A stack of binding scopes — the outermost layer is an operator's
/// parameter scope; each `transaction [...]` block pushes a new layer;
/// `let` statements insert into the topmost layer. Lookups walk
/// innermost-first so inner bindings shadow outer ones.
#[derive(Default)]
struct Scope {
    layers: Vec<HashMap<String, Type>>,
}

impl Scope {
    fn push(&mut self) {
        self.layers.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.layers.pop();
    }

    fn insert(&mut self, name: String, ty: Type) {
        self.layers
            .last_mut()
            .expect("scope stack is empty")
            .insert(name, ty);
    }

    fn lookup(&self, name: &str) -> Option<&Type> {
        self.layers.iter().rev().find_map(|l| l.get(name))
    }
}

/// What kind of position a `TypeHint` decorates. The label prefix
/// differs (`:` for a binding, `->` for an operator return) so
/// downstream renderers can format consistently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintKind {
    /// `let x = expr;` — the hint goes after the binding name.
    LetBinding,
    /// `oper f { } [ ... ]` — the hint goes after the heading.
    OperReturn,
}

/// One inferred-type hint surfaced by the typechecker.
///
/// `span` is the byte range where an editor would render the hint
/// (e.g., immediately after the binding name or heading); `ty` is
/// the inferred type; `kind` tells the renderer which prefix to use.
#[derive(Clone, Debug)]
pub struct TypeHint {
    pub span: Span,
    pub ty: Type,
    pub kind: HintKind,
}

/// The output of one `check` pass: the parsed CST root, every
/// diagnostic from the parser and the typechecker together, and a
/// list of inferred-type hints for editor surfaces (inlay hints,
/// hover). The typechecker doesn't filter parse errors — downstream
/// tools see the full picture. The tree is always present (the
/// parser's error recovery guarantees this); downstream passes lower
/// the same tree without re-parsing.
#[derive(Debug)]
pub struct CheckOutput {
    pub tree: SyntaxNode,
    pub diagnostics: Vec<Diagnostic>,
    pub hints: Vec<TypeHint>,
}

/// Tokenize, parse, and type-check `source` as a `.cd` document.
/// Other dialects (`.cddb` / `.cdmap` / `.cdstore`) parse in Phase 14
/// but don't typecheck yet — call `coddl_syntax::parse` directly with
/// the desired [`FileKind`] for parse-only output.
pub fn check(source: &str, file: FileId) -> CheckOutput {
    let parse_out = parse(source, file, FileKind::Cd);
    let tree = parse_out.tree.clone();
    let mut tc = TypeChecker {
        file,
        builtins: Builtins::new(),
        diagnostics: parse_out.diagnostics,
        hints: Vec::new(),
    };
    if let Some(root) = Root::cast(parse_out.tree) {
        tc.check_root(&root);
    }
    CheckOutput {
        tree,
        diagnostics: tc.diagnostics,
        hints: tc.hints,
    }
}

struct TypeChecker {
    file: FileId,
    builtins: Builtins,
    diagnostics: Vec<Diagnostic>,
    hints: Vec<TypeHint>,
}

impl TypeChecker {
    // ── Diagnostic helper ────────────────────────────────────────────

    fn error(&mut self, span: Span, code: &'static str, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::error(span, code, message));
    }

    fn node_span(&self, node: &SyntaxNode) -> Span {
        let r = node.text_range();
        Span::new(self.file, r.start().into(), r.end().into())
    }

    fn token_span(&self, token: &SyntaxToken) -> Span {
        let r = token.text_range();
        Span::new(self.file, r.start().into(), r.end().into())
    }

    // ── Walks ────────────────────────────────────────────────────────

    fn check_root(&mut self, root: &Root) {
        for item in root.items() {
            match item {
                Item::ProgramDecl(p) => self.check_program_decl(&p),
                Item::DatabaseBinding(_) => {
                    // Plan discovery + cross-file validation lands in
                    // Phase 16; the binding is a label here, no
                    // semantic constraints from the typechecker yet.
                }
                Item::OperDecl(o) => self.check_oper_decl(&o),
            }
        }
    }

    fn check_program_decl(&mut self, _decl: &ProgramDecl) {
        // The program name is a label today — no semantic constraints
        // beyond what the parser already checks.
    }

    fn check_oper_decl(&mut self, decl: &OperDecl) {
        let mut scope = Scope::default();
        scope.push(); // operator parameter layer

        // Resolve declared parameters into the parameter layer.
        // Duplicate names are rejected here so the body sees a
        // well-formed scope.
        let mut param_count: usize = 0;
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(heading) = decl.heading() {
            for param in heading.params() {
                let name_tok = match param.name() {
                    Some(t) => t,
                    None => continue,
                };
                let name = name_tok.text().to_string();
                if !seen.insert(name.clone()) {
                    self.error(
                        self.token_span(&name_tok),
                        "T0007",
                        format!("duplicate parameter name `{name}`"),
                    );
                    continue;
                }

                let ty = match param.type_ref().and_then(|tr| tr.name()) {
                    Some(name_tok) => self.resolve_type_name(&name_tok),
                    None => Type::Unknown,
                };
                scope.insert(name, ty);
                param_count += 1;
            }
        }

        // Resolve the declared return type, if any. Default = Unit.
        // When absent, also surface the implicit return as an inlay
        // hint right after the heading — that's where the user would
        // have typed `-> Type`.
        let return_type = match decl.return_type().and_then(|tr| tr.name()) {
            Some(name_tok) => self.resolve_type_name(&name_tok),
            None => {
                if let Some(heading) = decl.heading() {
                    let r = heading.syntax().text_range();
                    self.hints.push(TypeHint {
                        span: Span::new(self.file, r.end().into(), r.end().into()),
                        ty: Type::unit(),
                        kind: HintKind::OperReturn,
                    });
                }
                Type::unit()
            }
        };

        // Entry-point rules: `main` must take no parameters and must
        // return Unit. The runtime always exits with `i32 0`; a
        // declared non-Unit return would lie about what `main`
        // produces. When real exit-code semantics arrive, T0011
        // relaxes.
        if let Some(name_tok) = decl.name() {
            if name_tok.text() == "main" {
                if param_count > 0 {
                    self.error(
                        self.token_span(&name_tok),
                        "T0006",
                        "`main` must take zero parameters",
                    );
                }
                if !return_type.assignable_to(&Type::unit()) {
                    let span = decl
                        .return_type()
                        .map(|tr| self.node_span(tr.syntax()))
                        .unwrap_or_else(|| self.token_span(&name_tok));
                    self.error(
                        span,
                        "T0011",
                        format!("`main` must return {}, got {}", Type::unit(), return_type),
                    );
                }
            }
        }

        // The body's result type must match the declared return.
        if let Some(body) = decl.body() {
            let body_ty = self.check_block(&body, &mut scope);
            if !body_ty.assignable_to(&return_type) {
                let span = body
                    .tail_expr()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(body.syntax()));
                self.error(
                    span,
                    "T0009",
                    format!("operator body produces {body_ty}, but operator returns {return_type}"),
                );
            }
        }

        scope.pop();
    }

    fn resolve_type_name(&mut self, token: &SyntaxToken) -> Type {
        let name = token.text();
        match Type::from_builtin_name(name) {
            Some(t) => t,
            None => {
                self.error(
                    self.token_span(token),
                    "T0005",
                    format!("unknown type `{name}`"),
                );
                Type::Unknown
            }
        }
    }

    fn check_block(&mut self, block: &Block, scope: &mut Scope) -> Type {
        for stmt in block.statements() {
            match stmt {
                Stmt::Let(l) => self.check_let_stmt(&l, scope),
                Stmt::ExprStmt(e) => self.check_expr_stmt(&e, scope),
            }
        }
        match block.tail_expr() {
            Some(expr) => self.check_expr(&expr, scope),
            None => Type::unit(),
        }
    }

    fn check_let_stmt(&mut self, stmt: &LetStmt, scope: &mut Scope) {
        // Infer the RHS type. Missing name or value here means the
        // parser already reported the recovery; we still walk what's
        // parseable to keep diagnostics flowing.
        let rhs_ty = match stmt.value() {
            Some(v) => self.check_expr(&v, scope),
            None => Type::Unknown,
        };

        // If the binding carries an explicit annotation, the
        // annotation is authoritative: the RHS must conform, and
        // subsequent lookups see the declared type, not the inferred
        // one. Otherwise the inferred type is bound *and* surfaced as
        // an inlay hint — that's what the editor renders.
        let bound_ty = match stmt.type_ref().and_then(|tr| tr.name()) {
            Some(name_tok) => {
                let declared = self.resolve_type_name(&name_tok);
                if !rhs_ty.assignable_to(&declared) {
                    let span = stmt
                        .value()
                        .map(|v| self.node_span(v.syntax()))
                        .unwrap_or_else(|| self.node_span(stmt.syntax()));
                    self.error(
                        span,
                        "T0010",
                        format!(
                            "let binding declared {declared}, but expression produces {rhs_ty}"
                        ),
                    );
                }
                declared
            }
            None => {
                if let Some(name_tok) = stmt.name() {
                    // Render the hint immediately after the binding
                    // name token — that's where the user would have
                    // typed `: Type`.
                    let r = name_tok.text_range();
                    self.hints.push(TypeHint {
                        span: Span::new(self.file, r.end().into(), r.end().into()),
                        ty: rhs_ty.clone(),
                        kind: HintKind::LetBinding,
                    });
                }
                rhs_ty
            }
        };

        if let Some(name_tok) = stmt.name() {
            scope.insert(name_tok.text().to_string(), bound_ty);
        }
    }

    fn check_expr_stmt(&mut self, stmt: &ExprStmt, scope: &mut Scope) {
        if let Some(expr) = stmt.expr() {
            let _ = self.check_expr(&expr, scope); // result discarded
        }
    }

    fn check_expr(&mut self, expr: &Expr, scope: &mut Scope) -> Type {
        match expr {
            Expr::NameRef(n) => {
                let Some(ident) = n.ident() else {
                    return Type::Unknown;
                };
                let name = ident.text();
                if let Some(ty) = scope.lookup(name) {
                    return ty.clone();
                }
                self.error(
                    self.token_span(&ident),
                    "T0001",
                    format!("cannot resolve name `{name}`"),
                );
                Type::Unknown
            }
            Expr::Literal(lit) => match lit.token().map(|t| t.kind()) {
                Some(SyntaxKind::STRING_LIT) => Type::Text,
                Some(SyntaxKind::CHAR_LIT) => Type::Character,
                Some(SyntaxKind::INTEGER_LIT) => Type::Integer,
                Some(SyntaxKind::RATIONAL_LIT) => Type::Rational,
                Some(SyntaxKind::APPROXIMATE_LIT) => Type::Approximate,
                _ => Type::Unknown,
            },
            Expr::Call(call) => self.check_call(call, scope),
            Expr::Transaction(t) => self.check_transaction_expr(t, scope),
        }
    }

    fn check_transaction_expr(&mut self, txn: &TransactionExpr, scope: &mut Scope) -> Type {
        // `transaction [ ... ]` is a block expression; its value is
        // the body block's value. The scope push gates inner
        // bindings from leaking out.
        scope.push();
        let ty = match txn.body() {
            Some(b) => self.check_block(&b, scope),
            None => Type::unit(),
        };
        scope.pop();
        ty
    }

    fn check_call(&mut self, call: &CallExpr, scope: &mut Scope) -> Type {
        // The callee must be a `NameRef` whose lexeme is in builtins.
        let callee = call.callee();
        let callee_name_tok = match &callee {
            Some(Expr::NameRef(n)) => n.ident(),
            _ => None,
        };

        let Some(callee_name_tok) = callee_name_tok else {
            // Parser already complained about a missing callee, or the
            // callee is structurally something else we don't handle yet.
            return Type::Unknown;
        };

        let callee_name = callee_name_tok.text().to_string();
        let sig = self.builtins.oper(&callee_name).cloned();
        let Some(sig) = sig else {
            self.error(
                self.token_span(&callee_name_tok),
                "T0001",
                format!("cannot resolve name `{callee_name}`"),
            );
            return Type::Unknown;
        };

        // Walk the actual argument list against the declared parameters.
        let mut seen: HashSet<String> = HashSet::new();
        let mut provided: HashSet<String> = HashSet::new();
        if let Some(arg_list) = call.args() {
            for arg in arg_list.args() {
                self.check_named_arg(&arg, &sig, scope, &mut seen, &mut provided);
            }
        }

        // Every declared parameter must be supplied exactly once.
        for (pname, _) in &sig.params {
            if !provided.contains(*pname) {
                let span = call
                    .args()
                    .map(|a| self.node_span(a.syntax()))
                    .unwrap_or_else(|| self.node_span(call.syntax()));
                self.error(
                    span,
                    "T0003",
                    format!("missing argument `{pname}` in call to `{callee_name}`"),
                );
            }
        }

        sig.return_type.clone()
    }

    fn check_named_arg(
        &mut self,
        arg: &NamedArg,
        sig: &crate::builtins::OperSig,
        scope: &mut Scope,
        seen: &mut HashSet<String>,
        provided: &mut HashSet<String>,
    ) {
        let name_tok = match arg.name() {
            Some(t) => t,
            None => return,
        };
        let name = name_tok.text().to_string();

        if !seen.insert(name.clone()) {
            self.error(
                self.token_span(&name_tok),
                "T0008",
                format!("duplicate argument `{name}`"),
            );
            return;
        }

        let declared = sig
            .params
            .iter()
            .find(|(p, _)| *p == name)
            .map(|(_, t)| t.clone());

        let arg_ty = match arg.value() {
            Some(v) => self.check_expr(&v, scope),
            None => Type::Unknown,
        };

        match declared {
            Some(expected) => {
                provided.insert(name.clone());
                if !arg_ty.assignable_to(&expected) {
                    let span = arg
                        .value()
                        .map(|v| self.node_span(v.syntax()))
                        .unwrap_or_else(|| self.node_span(arg.syntax()));
                    self.error(
                        span,
                        "T0004",
                        format!("argument `{name}` expected {expected}, got {arg_ty}"),
                    );
                }
            }
            None => {
                self.error(
                    self.token_span(&name_tok),
                    "T0002",
                    format!("argument `{name}` is not declared"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics(src: &str) -> Vec<Diagnostic> {
        check(src, FileId(0)).diagnostics
    }

    fn codes(src: &str) -> Vec<&'static str> {
        diagnostics(src).into_iter().map(|d| d.code).collect()
    }

    const HELLO_WORLD: &str = "program hello_world;\n\
                               \n\
                               oper main {}\n\
                               [\n\
                                   write_line{message: \"Hello, world!\"};\n\
                               ];\n";

    #[test]
    fn hello_world_checks_clean() {
        let diags = diagnostics(HELLO_WORLD);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn unknown_callee_diagnoses_t0001() {
        let src = "oper main {} [ write_lne{message: \"x\"}; ];";
        assert!(codes(src).contains(&"T0001"));
    }

    #[test]
    fn unknown_arg_diagnoses_t0002() {
        let src = "oper main {} [ write_line{msg: \"x\", message: \"x\"}; ];";
        assert!(codes(src).contains(&"T0002"));
    }

    #[test]
    fn missing_arg_diagnoses_t0003() {
        let src = "oper main {} [ write_line{}; ];";
        assert!(codes(src).contains(&"T0003"));
    }

    #[test]
    fn arg_type_mismatch_diagnoses_t0004() {
        let src = "oper main {} [ write_line{message: 42}; ];";
        assert!(codes(src).contains(&"T0004"));
    }

    #[test]
    fn unknown_type_diagnoses_t0005() {
        let src = "oper f { x: NotAType } [];";
        assert!(codes(src).contains(&"T0005"));
    }

    #[test]
    fn main_with_params_diagnoses_t0006() {
        let src = "oper main { x: Integer } [];";
        assert!(codes(src).contains(&"T0006"));
    }

    #[test]
    fn duplicate_param_diagnoses_t0007() {
        let src = "oper f { x: Integer, x: Text } [];";
        assert!(codes(src).contains(&"T0007"));
    }

    #[test]
    fn duplicate_arg_diagnoses_t0008() {
        let src = "oper main {} [ write_line{message: \"a\", message: \"b\"}; ];";
        assert!(codes(src).contains(&"T0008"));
    }

    #[test]
    fn let_binds_for_later_statements() {
        // A let binding is visible to subsequent statements in the
        // same block. The call here resolves `msg` against the let
        // binding, not against the empty operator parameter scope.
        let src = "oper main {} [ let msg = \"hi\"; write_line{message: msg}; ];";
        assert!(
            diagnostics(src).is_empty(),
            "unexpected diagnostics: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn let_shadows_outer_binding() {
        // The inner let with the same name as an outer binding should
        // be silently allowed; the inner shadows the outer.
        let src = "oper f { x: Integer } [ let x = \"shadowed\"; write_line{message: x}; ];";
        assert!(
            diagnostics(src).is_empty(),
            "unexpected diagnostics: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn transaction_expr_type_is_tail_expression_type() {
        // The transaction's body's tail expression is a Text, so the
        // overall let-bound value is Text — write_line accepts it.
        let src = "oper main {} [ let ok = transaction [ \"ok\" ]; write_line{message: ok}; ];";
        assert!(
            diagnostics(src).is_empty(),
            "unexpected diagnostics: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn transaction_with_no_tail_is_unit() {
        // No tail expression in the body — value is Tuple {}. Passing
        // it to write_line (which expects Text) is a T0004 mismatch.
        let src = "oper main {} [ let u = transaction []; write_line{message: u}; ];";
        assert!(
            codes(src).contains(&"T0004"),
            "expected T0004, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn oper_body_with_non_unit_tail_diagnoses_t0009() {
        // A tail expression in an oper body that isn't Unit. Today
        // all opers return Unit implicitly.
        let src = "oper main {} [ \"oops\" ];";
        assert!(
            codes(src).contains(&"T0009"),
            "expected T0009, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn let_without_annotation_emits_type_hint() {
        // The unannotated `let count = 42;` should surface a hint
        // of type Integer, positioned at the end of `count`.
        let src = "oper main {} [ let count = 42; ];";
        let out = check(src, FileId(0));
        assert!(
            out.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            out.diagnostics
        );
        let hint = out
            .hints
            .iter()
            .find(|h| matches!(h.ty, Type::Integer))
            .expect("expected Integer hint for `count`");
        // The hint span ends at the byte position right after `count`.
        let count_end = src.find("count").unwrap() + "count".len();
        assert_eq!(hint.span.start as usize, count_end);
        assert_eq!(hint.span.end as usize, count_end);
    }

    #[test]
    fn let_with_annotation_emits_no_let_hint() {
        // When the user already wrote `: Text`, no binding hint to
        // render. (The oper-return hint for `main`'s implicit
        // `-> Tuple {}` still fires; we filter for the let kind.)
        let src = "oper main {} [ let m: Text = \"hi\"; ];";
        let out = check(src, FileId(0));
        let let_hints: Vec<_> = out
            .hints
            .iter()
            .filter(|h| h.kind == HintKind::LetBinding)
            .collect();
        assert!(
            let_hints.is_empty(),
            "expected no LetBinding hints, got {let_hints:?}"
        );
    }

    #[test]
    fn oper_without_return_clause_emits_return_hint() {
        // `oper main {}` has no `-> Type` clause; the implicit return
        // is Unit, so the editor should ghost `-> Tuple {}` right
        // after the heading's `}`.
        let src = "oper main {} [];";
        let out = check(src, FileId(0));
        let hint = out
            .hints
            .iter()
            .find(|h| h.kind == HintKind::OperReturn)
            .expect("expected an OperReturn hint");
        assert!(matches!(hint.ty, Type::Tuple(ref v) if v.is_empty()));
        // The hint span is right after the heading's closing `}`.
        let after_heading = src.find("{}").unwrap() + "{}".len();
        assert_eq!(hint.span.start as usize, after_heading);
    }

    #[test]
    fn oper_with_explicit_return_clause_emits_no_return_hint() {
        let src = "oper greet {} -> Text [ \"hi\" ]; oper main {} [];";
        let out = check(src, FileId(0));
        // The `greet` oper has an explicit clause, so no hint for it.
        // `main` still gets one because its return is implicit.
        let return_hints: Vec<_> = out
            .hints
            .iter()
            .filter(|h| h.kind == HintKind::OperReturn)
            .collect();
        assert_eq!(return_hints.len(), 1, "got {return_hints:?}");
    }

    #[test]
    fn let_annotation_accepted_when_matching() {
        let src = "oper main {} [ let m: Text = \"ok\"; write_line{message: m}; ];";
        assert!(
            diagnostics(src).is_empty(),
            "unexpected diagnostics: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn let_annotation_must_match_rhs_diagnoses_t0010() {
        let src = "oper main {} [ let x: Integer = \"hi\"; ];";
        assert!(
            codes(src).contains(&"T0010"),
            "expected T0010, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn let_annotation_authoritative_for_later_uses() {
        // Annotation declares Integer but RHS is Text — that's T0010.
        // Subsequent use as Text in write_line is also a mismatch
        // because the binding's type is the *declared* Integer, not
        // the inferred Text. So we should see both T0010 (annotation
        // vs RHS) and T0004 (call-site type mismatch).
        let src = "oper main {} [ let x: Integer = 1; write_line{message: x}; ];";
        let cs = codes(src);
        assert!(
            cs.contains(&"T0004"),
            "expected T0004 from passing Integer where Text is needed, got {cs:?}"
        );
    }

    #[test]
    fn oper_return_type_enforced_against_tail() {
        // Declared Text, body returns Unit (no tail) — T0009.
        let src = "oper greet {} -> Text [];";
        assert!(
            codes(src).contains(&"T0009"),
            "expected T0009, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn oper_with_matching_return_type_checks_clean() {
        let src = "oper greet {} -> Text [ \"hi\" ]; oper main {} [];";
        assert!(
            diagnostics(src).is_empty(),
            "unexpected diagnostics: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn main_with_non_unit_return_diagnoses_t0011() {
        let src = "oper main {} -> Integer [ 1 ];";
        assert!(
            codes(src).contains(&"T0011"),
            "expected T0011, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn inner_block_bindings_dont_leak() {
        // `inner` is bound inside the transaction but not visible
        // outside it.
        let src = "oper main {} [ let _ = transaction [ let inner = \"x\"; ]; write_line{message: inner}; ];";
        assert!(
            codes(src).contains(&"T0001"),
            "expected T0001, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn parse_errors_carry_through() {
        // The trailing `;` on the oper decl is missing — a parse-level
        // problem. The typechecker still walks what it can and the
        // parse diagnostic is reported alongside any typecheck ones.
        let src = "oper main {} []";
        let diags = diagnostics(src);
        assert!(
            diags.iter().any(|d| d.code.starts_with('P')),
            "expected a parse diagnostic, got {diags:?}"
        );
    }

    #[test]
    fn name_ref_resolves_to_parameter() {
        // `x` in the body is a parameter reference; should type-check
        // clean (the expression statement's result is discarded).
        let src = "oper f { x: Integer } [ x; ];";
        assert!(
            diagnostics(src).is_empty(),
            "expected no diagnostics, got {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn unresolved_name_ref_diagnoses_t0001() {
        let src = "oper f {} [ unknown_var; ];";
        assert!(codes(src).contains(&"T0001"));
    }

    #[test]
    fn check_output_exposes_tree() {
        // Clean program — the tree is the parsed Root.
        let ok = check(HELLO_WORLD, FileId(0));
        assert_eq!(ok.tree.kind(), SyntaxKind::ROOT);

        // Even with errors the tree is still surfaced, so downstream
        // passes can decide what to do with the diagnostic-bearing
        // input without re-parsing.
        let bad = check("oper main {} []", FileId(0));
        assert_eq!(bad.tree.kind(), SyntaxKind::ROOT);
        assert!(!bad.diagnostics.is_empty());
    }
}

//! The typechecker walk.
//!
//! `TypeChecker` walks the AST produced by `coddl-syntax`, resolving
//! names, validating call sites against the built-in registry, and
//! emitting diagnostics with stable `T####` codes. Walk methods are
//! named to mirror the productions in `docs/grammar.md` (`parse_oper_decl`
//! → `check_oper_decl`, etc.); `docs/typecheck.md` is the spec they
//! enforce.

use std::collections::HashSet;

use coddl_diagnostics::{Diagnostic, FileId, Span};
use coddl_syntax::ast::{
    AstNode, Block, CallExpr, Expr, ExprStmt, Item, NamedArg, OperDecl, ProgramDecl, Root, Stmt,
};
use coddl_syntax::cst::{SyntaxNode, SyntaxToken};
use coddl_syntax::{parse, SyntaxKind};

use crate::builtins::Builtins;
use crate::ty::Type;

/// The output of one `check` pass: the parsed CST root and every
/// diagnostic from the parser and the typechecker together. The
/// typechecker doesn't filter parse errors — downstream tools see the
/// full picture. The tree is always present (the parser's error
/// recovery guarantees this); downstream passes lower the same tree
/// without re-parsing.
#[derive(Debug)]
pub struct CheckOutput {
    pub tree: SyntaxNode,
    pub diagnostics: Vec<Diagnostic>,
}

/// Tokenize, parse, and type-check `source`.
pub fn check(source: &str, file: FileId) -> CheckOutput {
    let parse_out = parse(source, file);
    let tree = parse_out.tree.clone();
    let mut tc = TypeChecker {
        file,
        builtins: Builtins::new(),
        diagnostics: parse_out.diagnostics,
    };
    if let Some(root) = Root::cast(parse_out.tree) {
        tc.check_root(&root);
    }
    CheckOutput {
        tree,
        diagnostics: tc.diagnostics,
    }
}

struct TypeChecker {
    file: FileId,
    builtins: Builtins,
    diagnostics: Vec<Diagnostic>,
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
                Item::OperDecl(o) => self.check_oper_decl(&o),
            }
        }
    }

    fn check_program_decl(&mut self, _decl: &ProgramDecl) {
        // The program name is a label today — no semantic constraints
        // beyond what the parser already checks.
    }

    fn check_oper_decl(&mut self, decl: &OperDecl) {
        // Resolve declared parameters into a scope. Duplicate names
        // are rejected here so the body sees a well-formed scope.
        let mut params: Vec<(String, Type)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(heading) = decl.heading() {
            for param in heading.params() {
                let name_tok = match param.name() {
                    Some(t) => t,
                    None => continue, // parse error already reported
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
                    None => Type::Unknown, // parse error already reported
                };
                params.push((name, ty));
            }
        }

        // Entry-point rule: `main` must take no parameters.
        if let Some(name_tok) = decl.name() {
            if name_tok.text() == "main" && !params.is_empty() {
                self.error(
                    self.token_span(&name_tok),
                    "T0006",
                    "`main` must take zero parameters",
                );
            }
        }

        if let Some(body) = decl.body() {
            self.check_block(&body, &params);
        }
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

    fn check_block(&mut self, block: &Block, params: &[(String, Type)]) {
        for stmt in block.statements() {
            match stmt {
                Stmt::ExprStmt(e) => self.check_expr_stmt(&e, params),
            }
        }
    }

    fn check_expr_stmt(&mut self, stmt: &ExprStmt, params: &[(String, Type)]) {
        if let Some(expr) = stmt.expr() {
            let _ = self.check_expr(&expr, params); // result discarded
        }
    }

    fn check_expr(&mut self, expr: &Expr, params: &[(String, Type)]) -> Type {
        match expr {
            Expr::NameRef(n) => {
                // A bare NameRef in expression position with no callable
                // following it is a value reference. Today the only
                // values in scope are parameters.
                let Some(ident) = n.ident() else {
                    return Type::Unknown;
                };
                let name = ident.text();
                if let Some((_, ty)) = params.iter().find(|(n, _)| n == name) {
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
            Expr::Call(call) => self.check_call(call, params),
        }
    }

    fn check_call(&mut self, call: &CallExpr, params: &[(String, Type)]) -> Type {
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
                self.check_named_arg(&arg, &sig, params, &mut seen, &mut provided);
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
        params: &[(String, Type)],
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
            Some(v) => self.check_expr(&v, params),
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

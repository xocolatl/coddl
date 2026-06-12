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
    AstNode, Block, CallExpr, Expr, ExprStmt, FieldAccess, Heading as AstHeading, Item, KeyClause,
    LetStmt, NamedArg, OperDecl, PrivateRelvarDecl, ProgramDecl, PublicRelvarDecl, Root, Stmt,
    TransactionExpr, TupleLit,
};
use coddl_syntax::ast_cddb::{BaseRelvarDecl, CddbItem, CddbRoot, VirtualRelvarDecl};
use coddl_syntax::cst::{SyntaxNode, SyntaxToken};
use coddl_syntax::{parse, FileKind, SyntaxKind};

use crate::builtins::Builtins;
use crate::relvars::{RelvarInfo, RelvarKind, RelvarTable};
use crate::ty::{Heading, Type};

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
/// diagnostic from the parser and the typechecker together, the
/// inferred-type hints for editor surfaces (inlay hints, hover), and
/// the relvar table populated from this file's declarations. The
/// typechecker doesn't filter parse errors — downstream tools see the
/// full picture. The tree is always present (the parser's error
/// recovery guarantees this); downstream passes lower the same tree
/// without re-parsing.
#[derive(Debug)]
pub struct CheckOutput {
    pub tree: SyntaxNode,
    pub diagnostics: Vec<Diagnostic>,
    pub hints: Vec<TypeHint>,
    /// All relvars declared in this file. For `.cd`: public + private
    /// (and any base/virtual the user mistakenly placed in `.cd`,
    /// which T0014 flags). For `.cddb`: base + virtual (similarly).
    /// Empty for `.cdmap` / `.cdstore` — those don't declare relvars.
    pub relvars: RelvarTable,
}

/// Tokenize, parse, and type-check `source` in the supplied dialect.
///
/// For `.cd` and `.cddb`, the typechecker walks declarations and emits
/// every applicable diagnostic. For `.cdmap` and `.cdstore`, the
/// function is parse-only — the result carries the tree and parser
/// diagnostics; the relvar table is empty.
pub fn check(source: &str, file: FileId, file_kind: FileKind) -> CheckOutput {
    let parse_out = parse(source, file, file_kind);
    let tree = parse_out.tree.clone();
    let mut tc = TypeChecker {
        file,
        file_kind,
        builtins: Builtins::new(),
        diagnostics: parse_out.diagnostics,
        hints: Vec::new(),
        relvars: RelvarTable::new(),
    };
    match file_kind {
        FileKind::Cd => {
            if let Some(root) = Root::cast(parse_out.tree) {
                tc.check_root(&root);
            }
        }
        FileKind::Cddb => {
            if let Some(root) = CddbRoot::cast(parse_out.tree) {
                tc.check_cddb_root(&root);
            }
        }
        FileKind::Cdmap | FileKind::Cdstore => {
            // Parse-only today; semantic validation lands with Phase 16
            // (the plan layer) and Phase 21 (storage materialization).
        }
    }
    CheckOutput {
        tree,
        diagnostics: tc.diagnostics,
        hints: tc.hints,
        relvars: tc.relvars,
    }
}

struct TypeChecker {
    file: FileId,
    file_kind: FileKind,
    builtins: Builtins,
    diagnostics: Vec<Diagnostic>,
    hints: Vec<TypeHint>,
    relvars: RelvarTable,
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
        // Pre-pass: collect every relvar declaration into the table.
        // This runs before any operator body is walked so that future
        // phases (Phase 18+) can resolve relvar references in
        // expressions against a complete table.
        for item in root.items() {
            match item {
                Item::PublicRelvarDecl(d) => self.check_public_relvar_decl(&d),
                Item::PrivateRelvarDecl(d) => self.check_private_relvar_decl(&d),
                Item::BaseRelvarDecl(d) => self.check_base_relvar_decl(&d),
                Item::VirtualRelvarDecl(d) => self.check_virtual_relvar_decl(&d),
                _ => {}
            }
        }
        // Main pass: walk operator bodies + label-only items.
        for item in root.items() {
            match item {
                Item::ProgramDecl(p) => self.check_program_decl(&p),
                Item::DatabaseBinding(_) => {
                    // Plan discovery + cross-file validation lands in
                    // Phase 16; the binding is a label here, no
                    // semantic constraints from the typechecker yet.
                }
                Item::OperDecl(o) => self.check_oper_decl(&o),
                Item::PublicRelvarDecl(_)
                | Item::PrivateRelvarDecl(_)
                | Item::BaseRelvarDecl(_)
                | Item::VirtualRelvarDecl(_) => {
                    // Relvar items walked in the pre-pass above.
                }
            }
        }
    }

    /// `.cddb` root walk. There are no operator bodies in `.cddb`, so
    /// this is a one-pass collection of every relvar declaration into
    /// the table. T0014 fires here if `public` / `private` appears
    /// (those are `.cd`-only kinds).
    fn check_cddb_root(&mut self, root: &CddbRoot) {
        for item in root.items() {
            match item {
                CddbItem::BaseRelvar(d) => self.check_base_relvar_decl(&d),
                CddbItem::VirtualRelvar(d) => self.check_virtual_relvar_decl(&d),
            }
        }
        // Walk the raw tree for any PUBLIC/PRIVATE_RELVAR_DECL nodes
        // that the `.cddb` parser produced — these mean the user typed
        // a `.cd` kind keyword in a `.cddb` file. Insert them into the
        // table too, so a later T0012 still fires on duplicates with
        // the same name, but flag them with T0014.
        for node in root.syntax().children() {
            match node.kind() {
                SyntaxKind::PUBLIC_RELVAR_DECL => {
                    if let Some(d) = PublicRelvarDecl::cast(node) {
                        self.check_public_relvar_decl(&d);
                    }
                }
                SyntaxKind::PRIVATE_RELVAR_DECL => {
                    if let Some(d) = PrivateRelvarDecl::cast(node) {
                        self.check_private_relvar_decl(&d);
                    }
                }
                _ => {}
            }
        }
    }

    fn check_program_decl(&mut self, _decl: &ProgramDecl) {
        // The program name is a label today — no semantic constraints
        // beyond what the parser already checks.
    }

    // ── Relvar declarations ──────────────────────────────────────────

    fn check_public_relvar_decl(&mut self, decl: &PublicRelvarDecl) {
        self.collect_relvar(
            RelvarKind::Public,
            decl.name(),
            decl.heading(),
            decl.key_clauses().collect(),
            decl.syntax(),
        );
    }

    fn check_private_relvar_decl(&mut self, decl: &PrivateRelvarDecl) {
        self.collect_relvar(
            RelvarKind::Private,
            decl.name(),
            decl.heading(),
            decl.key_clauses().collect(),
            decl.syntax(),
        );
    }

    fn check_base_relvar_decl(&mut self, decl: &BaseRelvarDecl) {
        self.collect_relvar(
            RelvarKind::Base,
            decl.name(),
            decl.heading(),
            decl.key_clauses().collect(),
            decl.syntax(),
        );
    }

    fn check_virtual_relvar_decl(&mut self, decl: &VirtualRelvarDecl) {
        // Virtual relvars carry no syntactic heading — their type is
        // the type of their RHS expression, which doesn't typecheck
        // until the relational algebra lands (Phase 19+). For now we
        // still emit a T0014 if the kind is illegal for this dialect,
        // and register a record with an empty heading so a duplicate
        // name still flags T0012.
        let name_tok = decl.name();
        if !self.is_kind_legal_for_dialect(RelvarKind::Virtual) {
            if let Some(t) = &name_tok {
                self.emit_t0014(t, RelvarKind::Virtual);
            }
        }
        let Some(name_tok) = name_tok else {
            return;
        };
        let name = name_tok.text().to_string();
        let info = RelvarInfo {
            kind: RelvarKind::Virtual,
            heading: Heading::empty(),
            keys: Vec::new(),
            span: self.token_span(&name_tok),
        };
        if let Err(prior) = self.relvars.try_insert(name.clone(), info) {
            self.emit_t0012(&name_tok, &name, prior);
        }
    }

    /// Shared collection routine for the three heading-bearing relvar
    /// kinds (public, private, base). Resolves the heading, validates
    /// each key clause against it, validates dialect legality, and
    /// inserts into the table.
    fn collect_relvar(
        &mut self,
        kind: RelvarKind,
        name_tok: Option<SyntaxToken>,
        heading_ast: Option<AstHeading>,
        keys: Vec<KeyClause>,
        _node: &SyntaxNode,
    ) {
        if !self.is_kind_legal_for_dialect(kind) {
            if let Some(t) = &name_tok {
                self.emit_t0014(t, kind);
            }
        }
        let Some(name_tok) = name_tok else {
            return;
        };
        let name = name_tok.text().to_string();

        // Resolve the heading. Duplicate attribute names within the
        // heading reuse T0007 (the same per-attribute uniqueness
        // diagnostic that applies to `oper` headings).
        let heading = match heading_ast {
            Some(h) => self.resolve_heading(&h),
            None => Heading::empty(),
        };

        // Walk every key clause; v1 typechecks every key's attributes
        // (they're cheap to validate), even though downstream only
        // uses the first key for indexing decisions.
        let key_lists: Vec<Vec<String>> = keys
            .iter()
            .map(|k| self.validate_key_clause(k, &heading))
            .collect();

        let info = RelvarInfo {
            kind,
            heading,
            keys: key_lists,
            span: self.token_span(&name_tok),
        };
        if let Err(prior) = self.relvars.try_insert(name.clone(), info) {
            self.emit_t0012(&name_tok, &name, prior);
        }
    }

    /// True iff the given relvar kind is allowed in the current file's
    /// dialect. `public`/`private` belong in `.cd`; `base`/`virtual`
    /// belong in `.cddb`.
    fn is_kind_legal_for_dialect(&self, kind: RelvarKind) -> bool {
        match (self.file_kind, kind) {
            (FileKind::Cd, RelvarKind::Public | RelvarKind::Private) => true,
            (FileKind::Cddb, RelvarKind::Base | RelvarKind::Virtual) => true,
            _ => false,
        }
    }

    fn emit_t0012(&mut self, name_tok: &SyntaxToken, name: &str, _prior_span: Span) {
        self.error(
            self.token_span(name_tok),
            "T0012",
            format!("duplicate relvar `{name}`"),
        );
    }

    fn emit_t0014(&mut self, name_tok: &SyntaxToken, kind: RelvarKind) {
        let dialect = match self.file_kind {
            FileKind::Cd => ".cd",
            FileKind::Cddb => ".cddb",
            FileKind::Cdmap => ".cdmap",
            FileKind::Cdstore => ".cdstore",
        };
        self.error(
            self.token_span(name_tok),
            "T0014",
            format!(
                "`{kw}` relvar is not legal in {dialect}",
                kw = kind.keyword(),
            ),
        );
    }

    /// Resolve a syntactic heading into a canonical [`Heading`]. Each
    /// `param.type_ref()` resolves through `resolve_type_name` (T0005
    /// on unknown names); duplicate attribute names emit T0007 — the
    /// same diagnostic used by `oper` headings, since the rule is the
    /// same: an attribute name appears at most once per heading.
    fn resolve_heading(&mut self, heading: &AstHeading) -> Heading {
        let mut fields: Vec<(String, Type)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for param in heading.params() {
            let Some(name_tok) = param.name() else {
                continue;
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
                Some(t) => self.resolve_type_name(&t),
                None => Type::Unknown,
            };
            fields.push((name, ty));
        }
        Heading::new(fields)
    }

    /// Verify every attribute named in `key { ... }` actually appears
    /// in `heading`. Emits T0013 against each offender. Returns the
    /// list of attribute names in source order — even ones that
    /// didn't validate, so downstream "candidate key" lookups see
    /// exactly what the user wrote.
    fn validate_key_clause(&mut self, key: &KeyClause, heading: &Heading) -> Vec<String> {
        let mut attrs: Vec<String> = Vec::new();
        for tok in key.attrs() {
            let name = tok.text().to_string();
            if heading.lookup(&name).is_none() {
                self.error(
                    self.token_span(&tok),
                    "T0013",
                    format!("key attribute `{name}` is not in the heading"),
                );
            }
            attrs.push(name);
        }
        attrs
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
            Expr::TupleLit(t) => self.check_tuple_lit(t, scope),
            Expr::FieldAccess(f) => self.check_field_access(f, scope),
        }
    }

    /// Walk a `{ name: expr, … }` literal. Each field's expression is
    /// typechecked independently; duplicates emit T0015 and the second
    /// occurrence is skipped. The result is `Tuple H` where `H` is the
    /// canonical (name-sorted) heading built from the surviving fields.
    fn check_tuple_lit(&mut self, tup: &TupleLit, scope: &mut Scope) -> Type {
        let mut seen: HashSet<String> = HashSet::new();
        let mut fields: Vec<(String, Type)> = Vec::new();
        for field in tup.fields() {
            let name_tok = match field.name() {
                Some(t) => t,
                None => continue,
            };
            let name = name_tok.text().to_string();
            let ty = match field.value() {
                Some(v) => self.check_expr(&v, scope),
                None => Type::Unknown,
            };
            if !seen.insert(name.clone()) {
                self.error(
                    self.token_span(&name_tok),
                    "T0015",
                    format!("duplicate field `{name}` in tuple literal"),
                );
                continue;
            }
            fields.push((name, ty));
        }
        Type::Tuple(Heading::new(fields))
    }

    /// Walk `<expr>.<field>`. The base must be of type `Tuple H`; the
    /// field name must exist in `H`. T0016 fires when the base isn't a
    /// tuple; T0017 when the field isn't in the heading.
    fn check_field_access(&mut self, fa: &FieldAccess, scope: &mut Scope) -> Type {
        let base_ty = match fa.base() {
            Some(b) => self.check_expr(&b, scope),
            None => return Type::Unknown,
        };
        let field_tok = match fa.field() {
            Some(t) => t,
            // Parser already emitted P0030; nothing more to add here.
            None => return Type::Unknown,
        };
        let field_name = field_tok.text();
        match base_ty {
            Type::Unknown => Type::Unknown,
            Type::Tuple(ref heading) => match heading.lookup(field_name) {
                Some(ty) => ty.clone(),
                None => {
                    self.error(
                        self.token_span(&field_tok),
                        "T0017",
                        format!("unknown field `{field_name}` in tuple {heading}"),
                    );
                    Type::Unknown
                }
            },
            other => {
                let span = fa
                    .base()
                    .map(|b| self.node_span(b.syntax()))
                    .unwrap_or_else(|| self.node_span(fa.syntax()));
                self.error(
                    span,
                    "T0016",
                    format!("field access requires a tuple value, but base has type {other}"),
                );
                Type::Unknown
            }
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
        check(src, FileId(0), FileKind::Cd).diagnostics
    }

    fn codes(src: &str) -> Vec<&'static str> {
        diagnostics(src).into_iter().map(|d| d.code).collect()
    }

    fn diagnostics_cddb(src: &str) -> Vec<Diagnostic> {
        check(src, FileId(0), FileKind::Cddb).diagnostics
    }

    fn codes_cddb(src: &str) -> Vec<&'static str> {
        diagnostics_cddb(src).into_iter().map(|d| d.code).collect()
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
        let out = check(src, FileId(0), FileKind::Cd);
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
        let out = check(src, FileId(0), FileKind::Cd);
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
        let out = check(src, FileId(0), FileKind::Cd);
        let hint = out
            .hints
            .iter()
            .find(|h| h.kind == HintKind::OperReturn)
            .expect("expected an OperReturn hint");
        assert!(matches!(hint.ty, Type::Tuple(ref h) if h.is_empty()));
        // The hint span is right after the heading's closing `}`.
        let after_heading = src.find("{}").unwrap() + "{}".len();
        assert_eq!(hint.span.start as usize, after_heading);
    }

    #[test]
    fn oper_with_explicit_return_clause_emits_no_return_hint() {
        let src = "oper greet {} -> Text [ \"hi\" ]; oper main {} [];";
        let out = check(src, FileId(0), FileKind::Cd);
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

    // ── Relvar declaration tests ─────────────────────────────────────

    #[test]
    fn public_relvar_typechecks_cleanly() {
        let src = "public relvar Greetings { id: Integer, message: Text } key { id };";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(
            out.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            out.diagnostics
        );
        let info = out.relvars.get("Greetings").expect("relvar registered");
        assert_eq!(info.kind, RelvarKind::Public);
        assert_eq!(info.heading.len(), 2);
        // Canonical (sorted) order: id < message.
        assert_eq!(info.heading.attrs()[0].0, "id");
        assert_eq!(info.heading.attrs()[1].0, "message");
        assert_eq!(info.keys, vec![vec!["id".to_string()]]);
    }

    #[test]
    fn private_relvar_typechecks_cleanly() {
        let src = "private relvar Local { a: Integer } key { a };";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(
            out.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            out.diagnostics
        );
        let info = out.relvars.get("Local").expect("relvar registered");
        assert_eq!(info.kind, RelvarKind::Private);
    }

    #[test]
    fn base_relvar_in_cddb_typechecks_cleanly() {
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n";
        let out = check(src, FileId(0), FileKind::Cddb);
        assert!(
            out.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            out.diagnostics
        );
        let info = out.relvars.get("X").expect("relvar registered");
        assert_eq!(info.kind, RelvarKind::Base);
    }

    #[test]
    fn virtual_relvar_registers_with_empty_heading() {
        let src = "database d;\nvirtual relvar V = X where p;\nbase relvar X { a: Integer };\n";
        let out = check(src, FileId(0), FileKind::Cddb);
        let info = out.relvars.get("V").expect("virtual registered");
        assert_eq!(info.kind, RelvarKind::Virtual);
        assert!(info.heading.is_empty());
    }

    #[test]
    fn duplicate_relvar_diagnoses_t0012() {
        let src = "public relvar X { a: Integer } key { a };\n\
                   public relvar X { b: Text } key { b };";
        let cs = codes(src);
        assert!(cs.contains(&"T0012"), "expected T0012, got {cs:?}");
    }

    #[test]
    fn duplicate_relvar_across_dialects_in_cd_diagnoses_t0012() {
        // `base relvar X` in `.cd` is illegal (T0014), but the table
        // still registers X so a duplicate `public relvar X` flags T0012
        // alongside the dialect error.
        let src = "base relvar X { a: Integer };\n\
                   public relvar X { a: Integer } key { a };";
        let cs = codes(src);
        assert!(cs.contains(&"T0012"), "expected T0012, got {cs:?}");
        assert!(cs.contains(&"T0014"), "expected T0014, got {cs:?}");
    }

    #[test]
    fn key_attr_not_in_heading_diagnoses_t0013() {
        let src = "public relvar X { id: Integer } key { missing };";
        let cs = codes(src);
        assert!(cs.contains(&"T0013"), "expected T0013, got {cs:?}");
    }

    #[test]
    fn base_kind_in_cd_diagnoses_t0014() {
        let src = "base relvar X { a: Integer } key { a };";
        let cs = codes(src);
        assert!(cs.contains(&"T0014"), "expected T0014, got {cs:?}");
    }

    #[test]
    fn virtual_kind_in_cd_diagnoses_t0014() {
        let src = "virtual relvar V = X;";
        let cs = codes(src);
        assert!(cs.contains(&"T0014"), "expected T0014, got {cs:?}");
    }

    #[test]
    fn public_kind_in_cddb_diagnoses_t0014() {
        let src = "database d;\npublic relvar X { a: Integer } key { a };";
        let cs = codes_cddb(src);
        assert!(cs.contains(&"T0014"), "expected T0014, got {cs:?}");
    }

    #[test]
    fn private_kind_in_cddb_diagnoses_t0014() {
        let src = "database d;\nprivate relvar X { a: Integer } key { a };";
        let cs = codes_cddb(src);
        assert!(cs.contains(&"T0014"), "expected T0014, got {cs:?}");
    }

    #[test]
    fn relvar_heading_canonicalizes_attribute_order() {
        let src = "public relvar X { z: Integer, a: Text } key { a };";
        let out = check(src, FileId(0), FileKind::Cd);
        let info = out.relvars.get("X").unwrap();
        // Sorted by name: a < z.
        assert_eq!(info.heading.attrs()[0].0, "a");
        assert_eq!(info.heading.attrs()[1].0, "z");
    }

    #[test]
    fn multi_key_relvar_typechecks() {
        let src = "public relvar SP { sid: Integer, pid: Integer, qty: Integer } \
                   key { sid, pid } key { qty };";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(
            out.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            out.diagnostics
        );
        let info = out.relvars.get("SP").unwrap();
        assert_eq!(info.keys.len(), 2);
        assert_eq!(info.keys[0], vec!["sid".to_string(), "pid".to_string()]);
        assert_eq!(info.keys[1], vec!["qty".to_string()]);
    }

    #[test]
    fn relvar_with_unknown_attribute_type_diagnoses_t0005() {
        let src = "public relvar X { id: NotAType } key { id };";
        let cs = codes(src);
        assert!(cs.contains(&"T0005"), "expected T0005, got {cs:?}");
    }

    #[test]
    fn check_output_exposes_tree() {
        // Clean program — the tree is the parsed Root.
        let ok = check(HELLO_WORLD, FileId(0), FileKind::Cd);
        assert_eq!(ok.tree.kind(), SyntaxKind::ROOT);

        // Even with errors the tree is still surfaced, so downstream
        // passes can decide what to do with the diagnostic-bearing
        // input without re-parsing.
        let bad = check("oper main {} []", FileId(0), FileKind::Cd);
        assert_eq!(bad.tree.kind(), SyntaxKind::ROOT);
        assert!(!bad.diagnostics.is_empty());
    }

    // ── Tuple literals + field access (Phase 18) ─────────────────────

    #[test]
    fn tuple_let_with_field_access_checks_clean() {
        // A let-bound tuple flowing into a call argument through field
        // access. The tuple's `message` field is Text, matching
        // write_line's expected parameter type.
        let src = "oper main {} [ \
                   let t = {message: \"hi\"}; \
                   write_line{message: t.message}; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn duplicate_field_in_tuple_lit_diagnoses_t0015() {
        let src = "oper main {} [ let t = {a: 1, a: 2}; ];";
        assert!(codes(src).contains(&"T0015"));
    }

    #[test]
    fn field_access_on_non_tuple_diagnoses_t0016() {
        let src = "oper main {} [ let n = 1; let _x = n.field; ];";
        assert!(codes(src).contains(&"T0016"));
    }

    #[test]
    fn unknown_field_diagnoses_t0017() {
        let src = "oper main {} [ let t = {a: 1}; let _x = t.b; ];";
        assert!(codes(src).contains(&"T0017"));
    }

    #[test]
    fn empty_tuple_lit_types_as_unit() {
        // The implicit-unit case made explicit: `{}` is Tuple {}.
        // No diagnostics, and the let binding's hint is Tuple {}.
        let src = "oper main {} [ let _t = {}; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn field_access_returns_attribute_type() {
        // `t.a` is Integer; passing it as a Text-typed arg fires
        // T0004, proving the field's type flows correctly.
        let src = "oper main {} [ \
                   let t = {a: 1}; \
                   write_line{message: t.a}; \
                   ];";
        assert!(codes(src).contains(&"T0004"));
    }
}

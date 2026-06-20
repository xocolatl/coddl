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
    AssignStmt, AstNode, BinaryExpr, BinaryOp, Block, CallExpr, Expr, ExprStmt, FieldAccess,
    Heading as AstHeading, Item, KeyClause, LetStmt, NamedArg, OperDecl, PrivateRelvarDecl,
    ProgramDecl, ProjectExpr, PublicRelvarDecl, RelationLit, RenameExpr, Root, Stmt, TransactionExpr,
    TupleLit, UnaryExpr, UnaryOp,
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
/// Where a scope binding came from, so the unused-binding check (T0032)
/// fires only on user `let`s — never on injected names (public relvars,
/// `where`-predicate heading attributes) or, for now, parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingOrigin {
    Let,
    Param,
    Relvar,
    WhereAttr,
}

/// One binding in a scope layer: its `Type` for lookup, plus the metadata the
/// unused-binding check needs — the name-token span (for the squiggle), the
/// origin, and whether any `NameRef` ever resolved to it.
struct Binding {
    ty: Type,
    name: String,
    span: Span,
    origin: BindingOrigin,
    used: bool,
}

#[derive(Default)]
struct Scope {
    /// Per layer: name -> index of the *active* binding in `records[layer]`.
    /// Re-binding a name overwrites the index (shadowing) but leaves the
    /// shadowed `Binding` in `records`, so it can still be reported unused.
    layers: Vec<HashMap<String, usize>>,
    /// Per layer: every binding inserted, in insertion order (append-only),
    /// parallel to `layers`.
    records: Vec<Vec<Binding>>,
}

impl Scope {
    fn push(&mut self) {
        self.layers.push(HashMap::new());
        self.records.push(Vec::new());
    }

    /// Pop the topmost layer, returning its bindings so the caller can report
    /// any that went unused.
    fn pop(&mut self) -> Vec<Binding> {
        self.layers.pop();
        self.records.pop().unwrap_or_default()
    }

    fn insert(&mut self, name: String, ty: Type, span: Span, origin: BindingOrigin) {
        let records = self.records.last_mut().expect("scope stack is empty");
        let idx = records.len();
        records.push(Binding {
            ty,
            name: name.clone(),
            span,
            origin,
            used: false,
        });
        self.layers
            .last_mut()
            .expect("scope stack is empty")
            .insert(name, idx);
    }

    fn lookup(&self, name: &str) -> Option<&Type> {
        for layer in (0..self.layers.len()).rev() {
            if let Some(&idx) = self.layers[layer].get(name) {
                return Some(&self.records[layer][idx].ty);
            }
        }
        None
    }

    /// Mark the active binding for `name` used (innermost layer first). A
    /// shadowed binding keeps `used = false` and is still reported unused.
    fn mark_used(&mut self, name: &str) {
        for layer in (0..self.layers.len()).rev() {
            if let Some(&idx) = self.layers[layer].get(name) {
                self.records[layer][idx].used = true;
                return;
            }
        }
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
        transaction_depth: 0,
        public_relvars: HashSet::new(),
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
    /// How many `transaction [...]` blocks the current walk is nested
    /// inside. Bumped by `check_transaction_expr` around its body. Used
    /// to gate T0025 (public-relvar reference outside any transaction)
    /// and T0026 (side-effecting call inside one).
    transaction_depth: usize,
    /// Names that resolve to public relvars in this file, populated at
    /// `check_oper_decl` entry from the `RelvarTable`. A `NameRef` whose
    /// lexeme is in this set produces a `Type::Relation(H)` and — if
    /// `transaction_depth == 0` — fires T0025.
    public_relvars: HashSet<String>,
}

impl TypeChecker {
    // ── Diagnostic helper ────────────────────────────────────────────

    fn error(&mut self, span: Span, code: &'static str, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::error(span, code, message));
    }

    fn warn(&mut self, span: Span, code: &'static str, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::warning(span, code, message));
    }

    /// Emit T0032 for every user `let` binding in a popped scope layer that no
    /// `NameRef` ever resolved to. A leading `_` (including bare `_`) opts out
    /// — the "unused-OK" convention. Injected names (relvars, `where`
    /// attributes) and parameters are excluded by origin.
    fn warn_unused(&mut self, layer: Vec<Binding>) {
        for b in layer {
            if b.used || !matches!(b.origin, BindingOrigin::Let | BindingOrigin::Param) {
                continue;
            }
            // A leading `_` opts out. `self` is the UFCS receiver — a parameter
            // literally named `self` is what makes an `oper` callable as
            // `x.method { ... }`, so renaming it to `_self` would break that
            // call syntax; it never warns even when the body ignores it.
            if b.name.starts_with('_') || b.name == "self" {
                continue;
            }
            let what = if matches!(b.origin, BindingOrigin::Param) {
                "parameter"
            } else {
                "binding"
            };
            self.warn(
                b.span,
                "T0032",
                format!("unused {what} `{}`; prefix with `_` if intentional", b.name),
            );
        }
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

        // Seed the scope with every public *and* private relvar in this
        // file. A bare `Greetings` / `Employees` in expression position then
        // resolves to `Type::Relation(H)` via the standard scope-lookup path.
        // The parallel `public_relvars` set records which names are *public*
        // relvars so NameRef can apply the T0025 transaction-scope rule
        // (RM Pre 14 / OO Pre 4: every public-relvar access lives inside a
        // `transaction [...]`). Private relvars are in-memory — no transaction.
        self.public_relvars.clear();
        for (name, info) in self.relvars.iter() {
            if matches!(info.kind, RelvarKind::Public | RelvarKind::Private) {
                let ty = Type::Relation(info.heading.clone());
                scope.insert(name.to_string(), ty, Span::default(), BindingOrigin::Relvar);
                if matches!(info.kind, RelvarKind::Public) {
                    self.public_relvars.insert(name.to_string());
                }
            }
        }

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
                scope.insert(name, ty, self.token_span(&name_tok), BindingOrigin::Param);
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

        let unused = scope.pop();
        self.warn_unused(unused);
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
                Stmt::Assign(a) => self.check_assignment_stmt(&a, scope),
                Stmt::ExprStmt(e) => self.check_expr_stmt(&e, scope),
            }
        }
        match block.tail_expr() {
            Some(expr) => self.check_expr(&expr, scope),
            None => Type::unit(),
        }
    }

    /// Check a relational assignment `R := <expr>;`. The target must be a
    /// bare name bound to a *private* relvar (public relvars are read-only in
    /// v1); the RHS must be a relation whose heading matches the relvar's.
    fn check_assignment_stmt(&mut self, stmt: &AssignStmt, scope: &mut Scope) {
        // Check the RHS first so its own diagnostics surface regardless of
        // the target's validity.
        let rhs_ty = match stmt.value() {
            Some(v) => self.check_expr(&v, scope),
            None => return, // parser recovery already emitted a diagnostic
        };

        // The target must be a bare name reference …
        let Some(Expr::NameRef(target)) = stmt.target() else {
            let span = stmt
                .target()
                .map(|t| self.node_span(t.syntax()))
                .unwrap_or_else(|| self.node_span(stmt.syntax()));
            self.error(span, "T0033", "assignment target must be a private relvar name");
            return;
        };
        let Some(ident) = target.ident() else { return };
        let name = ident.text();

        // … bound to a private relvar.
        let lookup = self
            .relvars
            .get(name)
            .map(|i| (matches!(i.kind, RelvarKind::Private), i.heading.clone()));
        let Some((is_private, heading)) = lookup else {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot assign to `{name}`: not an assignable (private) relvar"),
            );
            return;
        };
        if !is_private {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot assign to `{name}`: only private relvars are assignable (public relvars are read-only in v1)"),
            );
            return;
        }
        scope.mark_used(name);

        // The RHS heading must match the relvar's.
        let target_ty = Type::Relation(heading);
        if !rhs_ty.assignable_to(&target_ty) {
            let span = stmt
                .value()
                .map(|v| self.node_span(v.syntax()))
                .unwrap_or_else(|| self.token_span(&ident));
            self.error(
                span,
                "T0034",
                format!("cannot assign {rhs_ty} to relvar `{name}` (heading mismatch)"),
            );
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
            scope.insert(
                name_tok.text().to_string(),
                bound_ty,
                self.token_span(&name_tok),
                BindingOrigin::Let,
            );
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
                if let Some(ty) = scope.lookup(name).cloned() {
                    // Record the reference so the unused-binding check (T0032)
                    // doesn't flag this binding. This is the sole name-
                    // resolution site, so it captures every source use —
                    // including ones the lowerer later folds/pushes away.
                    scope.mark_used(name);
                    // A public-relvar reference is allowed only inside
                    // a `transaction [...]` block — that's where the
                    // runtime can safely begin/commit (or replay on
                    // serialization failure). RM Pre 14 + OO Pre 4 in
                    // combination: D forbids autocommit; the typechecker
                    // makes the wrap explicit at every access site.
                    if self.public_relvars.contains(name) && self.transaction_depth == 0 {
                        self.error(
                            self.token_span(&ident),
                            "T0025",
                            format!(
                                "public relvar `{name}` referenced outside any `transaction [...]` block"
                            ),
                        );
                    }
                    return ty;
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
            Expr::RelationLit(r) => self.check_relation_lit(r, scope),
            Expr::FieldAccess(f) => self.check_field_access(f, scope),
            Expr::BoolLit(_) => Type::Boolean,
            Expr::Binary(b) => self.check_binary_expr(b, scope),
            Expr::Unary(u) => self.check_unary_expr(u, scope),
            Expr::Project(p) => self.check_project_expr(p, scope),
            Expr::Rename(r) => self.check_rename_expr(r, scope),
        }
    }

    /// Walk `R project { a, … }` / `R project all but { a, … }` — relational
    /// projection. The operand must be `Relation H` (T0023 otherwise, shared
    /// with `where`). Each listed attribute must exist in `H` (T0027) and
    /// appear at most once (T0028). The result is `Relation H'` where `H'` is
    /// `H` narrowed to the listed attributes — or, for `all but`, to their
    /// complement — canonically re-sorted by `Heading::new`, so the order the
    /// names are written is irrelevant.
    fn check_project_expr(&mut self, pe: &ProjectExpr, scope: &mut Scope) -> Type {
        let input_ty = match pe.input() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &input_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = pe
                    .input()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(pe.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`project` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Validate each listed name (must exist → T0027, unique → T0028).
        // The surviving valid names form the project list; unknown/duplicate
        // names are reported and dropped (best-effort recovery).
        let mut seen: HashSet<String> = HashSet::new();
        let mut listed: HashSet<String> = HashSet::new();
        for tok in pe.attrs() {
            let name = tok.text();
            if !seen.insert(name.to_string()) {
                self.error(
                    self.token_span(&tok),
                    "T0028",
                    format!("duplicate attribute `{name}` in project list"),
                );
                continue;
            }
            match heading.lookup(name) {
                Some(_) => {
                    listed.insert(name.to_string());
                }
                None => self.error(
                    self.token_span(&tok),
                    "T0027",
                    format!("unknown attribute `{name}` in project of {heading}"),
                ),
            }
        }
        // `project { … }` keeps the listed attributes; `project all but { … }`
        // keeps the complement. `contains == all_but` is exactly the dropped
        // set, so `!= all_but` is the kept set.
        let all_but = pe.is_all_but();
        let result: Vec<(String, Type)> = heading
            .attrs()
            .iter()
            .filter(|(name, _)| listed.contains(name) != all_but)
            .cloned()
            .collect();
        Type::Relation(Heading::new(result))
    }

    /// Walk `R rename { old: new, … }` — relational rename. The operand must
    /// be `Relation H` (T0023). Each target must be a bare attribute name
    /// (T0030); each source must exist in `H` (T0029); the rename must stay a
    /// bijection — no source repeats and no target collides with a surviving
    /// attribute (T0031). The result heading is `H` with names remapped,
    /// canonically re-sorted.
    fn check_rename_expr(&mut self, re: &RenameExpr, scope: &mut Scope) -> Type {
        let input_ty = match re.input() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &input_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = re
                    .input()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(re.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`rename` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Validate each pair, collecting the valid `(old, new)` renames.
        let mut renames: Vec<(String, String)> = Vec::new();
        let mut seen_src: HashSet<String> = HashSet::new();
        for (old_tok, new_tok) in re.renames() {
            let Some(old_tok) = old_tok else { continue }; // parse recovery
            let old = old_tok.text();
            let Some(new_tok) = new_tok else {
                self.error(
                    self.token_span(&old_tok),
                    "T0030",
                    format!("rename target for `{old}` must be a bare attribute name"),
                );
                continue;
            };
            if heading.lookup(old).is_none() {
                self.error(
                    self.token_span(&old_tok),
                    "T0029",
                    format!("unknown attribute `{old}` in rename of {heading}"),
                );
                continue;
            }
            if !seen_src.insert(old.to_string()) {
                self.error(
                    self.token_span(&old_tok),
                    "T0031",
                    format!("attribute `{old}` is renamed more than once"),
                );
                continue;
            }
            renames.push((old.to_string(), new_tok.text().to_string()));
        }
        // Remap names; a target colliding with another attribute (the rename
        // isn't a bijection) is T0031.
        let mut result: Vec<(String, Type)> = Vec::new();
        let mut result_names: HashSet<String> = HashSet::new();
        for (name, ty) in heading.attrs() {
            let new_name = renames
                .iter()
                .find(|(old, _)| old == name)
                .map(|(_, new)| new.clone())
                .unwrap_or_else(|| name.clone());
            if !result_names.insert(new_name.clone()) {
                self.error(
                    self.node_span(re.syntax()),
                    "T0031",
                    format!("rename produces a duplicate attribute `{new_name}`"),
                );
            }
            result.push((new_name, ty.clone()));
        }
        Type::Relation(Heading::new(result))
    }

    /// Walk a unary prefix expression. Dispatches on `UnaryOp`.
    /// Phase 21's only operator is `Extract`: operand must be
    /// `Relation H`; result is `Tuple H`. T0024 on mismatch.
    fn check_unary_expr(&mut self, ue: &UnaryExpr, scope: &mut Scope) -> Type {
        let op = match ue.op_kind() {
            Some(op) => op,
            None => return Type::Unknown,
        };
        match op {
            UnaryOp::Extract => {
                let operand_ty = match ue.operand() {
                    Some(e) => self.check_expr(&e, scope),
                    None => return Type::Unknown,
                };
                match operand_ty {
                    Type::Relation(h) => Type::Tuple(h),
                    Type::Unknown => Type::Unknown,
                    other => {
                        let span = ue
                            .operand()
                            .map(|e| self.node_span(e.syntax()))
                            .unwrap_or_else(|| self.node_span(ue.syntax()));
                        self.error(
                            span,
                            "T0024",
                            format!("`extract` expects a Relation, got {other}"),
                        );
                        Type::Unknown
                    }
                }
            }
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

    /// Walk a `Relation { <tuple-lit>, <tuple-lit>, … }` literal. The
    /// first tuple establishes the heading; subsequent tuples must
    /// have the same `(name, type)` set. An empty `Relation {}` emits
    /// T0018 (no inference context for the heading). A heading
    /// mismatch emits T0019 on the offending tuple; the typechecker
    /// keeps the first tuple's heading so downstream checks see a
    /// stable type.
    fn check_relation_lit(&mut self, rel: &RelationLit, scope: &mut Scope) -> Type {
        let tuples: Vec<TupleLit> = rel.tuples().collect();
        if tuples.is_empty() {
            self.error(
                self.node_span(rel.syntax()),
                "T0018",
                "empty relation literal requires at least one tuple",
            );
            return Type::Unknown;
        }
        let first_heading = match self.check_tuple_lit(&tuples[0], scope) {
            Type::Tuple(h) => h,
            // The tuple typecheck only ever returns Tuple or Unknown
            // (on internal recovery); fall through with Unknown so we
            // don't cascade.
            _ => return Type::Unknown,
        };
        for tuple in &tuples[1..] {
            let h = match self.check_tuple_lit(tuple, scope) {
                Type::Tuple(h) => h,
                _ => continue,
            };
            if !h.assignable_to(&first_heading) {
                self.error(
                    self.node_span(tuple.syntax()),
                    "T0019",
                    format!(
                        "tuple heading {h} differs from relation's first tuple {first_heading}"
                    ),
                );
            }
        }
        Type::Relation(first_heading)
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

    /// Walk a binary infix expression. Dispatches on the parsed
    /// `BinaryOp`. Comparison and logical ops are scalar with Boolean
    /// result. The `Where` op is relational: lhs must be `Relation H`,
    /// rhs is a predicate typechecked in a scope augmented with the
    /// heading's attributes, and the result is `Relation H`.
    fn check_binary_expr(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        let op = match bin.op_kind() {
            Some(op) => op,
            None => return Type::Unknown,
        };
        match op {
            BinaryOp::Where => self.check_where_binary(bin, scope),
            BinaryOp::Join => self.check_join_binary(bin, scope),
            BinaryOp::Times => self.check_times_binary(bin, scope),
            BinaryOp::Compose => self.check_compose_binary(bin, scope),
            BinaryOp::And | BinaryOp::Or => self.check_logical_op(bin, op, scope),
            BinaryOp::Eq | BinaryOp::NotEq => self.check_equality_op(bin, op, scope),
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                self.check_ordering_op(bin, op, scope)
            }
        }
    }

    /// `R where pred` — restriction. Lhs must be relational; rhs
    /// typechecks with the operand's heading attributes injected
    /// into a fresh scope layer.
    fn check_where_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &lhs_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = bin
                    .lhs()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(bin.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`where` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Inject heading attributes as bindings in a fresh scope
        // layer. `Scope::lookup` walks innermost-first so heading
        // attributes shadow any outer locals with the same name.
        scope.push();
        for (name, ty) in heading.attrs() {
            scope.insert(name.clone(), ty.clone(), Span::default(), BindingOrigin::WhereAttr);
        }
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        scope.pop();
        // Predicate must be Boolean (or Unknown for error recovery).
        if !matches!(rhs_ty, Type::Boolean | Type::Unknown) {
            let span = bin
                .rhs()
                .map(|e| self.node_span(e.syntax()))
                .unwrap_or_else(|| self.node_span(bin.syntax()));
            self.error(
                span,
                "T0020",
                format!("`where` predicate must be Boolean, got {rhs_ty}"),
            );
        }
        Type::Relation(heading)
    }

    /// Type-check one relational operand of a binary op: returns its heading,
    /// or `None` (after emitting T0023) if it isn't a `Relation`.
    fn relation_operand(
        &mut self,
        operand: Option<Expr>,
        op_name: &str,
        scope: &mut Scope,
    ) -> Option<Heading> {
        let e = operand?;
        match self.check_expr(&e, scope) {
            Type::Relation(h) => Some(h),
            Type::Unknown => None,
            other => {
                self.error(
                    self.node_span(e.syntax()),
                    "T0023",
                    format!("`{op_name}` expects a Relation, got {other}"),
                );
                None
            }
        }
    }

    /// The natural-join heading check shared by `join` and `compose`: both
    /// require overlapping headings (≥1 shared attribute, with matching types on
    /// the shared ones). Returns the union heading, or `None` after emitting the
    /// diagnostic — disjoint headings → T0035 (suggest `times`), a shared-
    /// attribute type clash → T0036. `op_name` is interpolated so each operator
    /// reports under its own lexeme.
    fn natural_join_heading(
        &mut self,
        bin: &BinaryExpr,
        lhs_h: &Heading,
        rhs_h: &Heading,
        op_name: &str,
    ) -> Option<Heading> {
        if lhs_h.is_disjoint_from(rhs_h) {
            self.error(
                self.node_span(bin.syntax()),
                "T0035",
                format!("`{op_name}` operands share no attribute — did you mean `times`?"),
            );
            return None;
        }
        match lhs_h.union(rhs_h) {
            Ok(h) => Some(h),
            Err(name) => {
                self.error(
                    self.node_span(bin.syntax()),
                    "T0036",
                    format!(
                        "`{op_name}` shared attribute `{name}` has different types on each side"
                    ),
                );
                None
            }
        }
    }

    /// `R join S` — natural join (Algebra-A AND). Both operands must be
    /// relations that share ≥1 attribute (with matching types on the shared
    /// attributes); the result heading is the union. Disjoint headings →
    /// T0035 (suggest `times`); a shared-attribute type clash → T0036.
    fn check_join_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "join", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "join", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        match self.natural_join_heading(bin, &lhs_h, &rhs_h, "join") {
            Some(h) => Type::Relation(h),
            None => Type::Unknown,
        }
    }

    /// `R times S` — Cartesian product (Algebra-A AND of disjoint operands).
    /// Both operands must be relations whose headings are disjoint (share no
    /// attribute); the result heading is the union. Overlapping headings →
    /// T0037 (suggest `join`). Because the operands are proven disjoint, the
    /// union can never conflict on a shared attribute (join's T0036 case is
    /// unreachable here), so the `union` result is unwrapped.
    fn check_times_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "times", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "times", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        if !lhs_h.is_disjoint_from(&rhs_h) {
            self.error(
                self.node_span(bin.syntax()),
                "T0037",
                "`times` operands share an attribute — did you mean `join`?".to_string(),
            );
            return Type::Unknown;
        }
        Type::Relation(
            lhs_h
                .union(&rhs_h)
                .expect("disjoint headings cannot conflict on a shared attribute"),
        )
    }

    /// `R compose S` — natural join then REMOVE the shared attributes (Algebra-A
    /// AND then REMOVE). Like `join`, both operands must be relations sharing ≥1
    /// attribute (disjoint → T0035 suggest `times`; type clash → T0036); the
    /// result heading is the union with the shared attributes dropped.
    fn check_compose_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "compose", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "compose", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        let Some(union_h) = self.natural_join_heading(bin, &lhs_h, &rhs_h, "compose") else {
            return Type::Unknown;
        };
        // Drop the shared attributes: the result keeps only attributes that
        // appear in exactly one operand.
        let shared = lhs_h.shared_names(&rhs_h);
        let kept: Vec<(String, Type)> = union_h
            .attrs()
            .iter()
            .filter(|(name, _)| !shared.contains(name))
            .cloned()
            .collect();
        Type::Relation(Heading::new(kept))
    }

    /// `lhs and rhs` / `lhs or rhs` — both operands must be Boolean,
    /// result is Boolean.
    fn check_logical_op(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        for (operand_ty, side) in [(&lhs_ty, "left"), (&rhs_ty, "right")] {
            if !matches!(operand_ty, Type::Boolean | Type::Unknown) {
                let opname = op_display(op);
                let target = if side == "left" { bin.lhs() } else { bin.rhs() };
                let span = target
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(bin.syntax()));
                self.error(
                    span,
                    "T0021",
                    format!("`{opname}` expects Boolean on the {side}, got {operand_ty}"),
                );
            }
        }
        Type::Boolean
    }

    /// `lhs = rhs` / `lhs <> rhs` — operands must share a scalar type
    /// (Integer, Text, or Boolean for v1). Result is Boolean.
    fn check_equality_op(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let supported =
            |t: &Type| matches!(t, Type::Integer | Type::Text | Type::Boolean | Type::Unknown);
        if !supported(&lhs_ty) || !supported(&rhs_ty) || !lhs_ty.assignable_to(&rhs_ty) {
            let opname = op_display(op);
            self.error(
                self.node_span(bin.syntax()),
                "T0021",
                format!(
                    "`{opname}` operands must share a scalar type (Integer, Text, or Boolean); got {lhs_ty} vs {rhs_ty}"
                ),
            );
        }
        Type::Boolean
    }

    /// `lhs < rhs` / `lhs > rhs` / `lhs <= rhs` / `lhs >= rhs` —
    /// operands must both be Integer (Phase 20). Result is Boolean.
    fn check_ordering_op(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let supported = |t: &Type| matches!(t, Type::Integer | Type::Unknown);
        if !supported(&lhs_ty) || !supported(&rhs_ty) {
            let opname = op_display(op);
            self.error(
                self.node_span(bin.syntax()),
                "T0021",
                format!("`{opname}` requires Integer operands; got {lhs_ty} vs {rhs_ty}"),
            );
        }
        Type::Boolean
    }

    fn check_transaction_expr(&mut self, txn: &TransactionExpr, scope: &mut Scope) -> Type {
        // `transaction [ ... ]` is a block expression; its value is
        // the body block's value. The scope push gates inner
        // bindings from leaking out. The depth bump lets NameRef and
        // check_call enforce T0025 / T0026 — transactions must be
        // replayable, so public-relvar access is allowed and side
        // effects are forbidden inside them.
        scope.push();
        self.transaction_depth += 1;
        let ty = match txn.body() {
            Some(b) => self.check_block(&b, scope),
            None => Type::unit(),
        };
        self.transaction_depth -= 1;
        let unused = scope.pop();
        self.warn_unused(unused);
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

        // Transactions must be pure so the runtime can replay them on
        // serialization conflict. Side-effecting builtins (write_line,
        // write_relation) are blocked here; the surface rule extends to
        // user-defined opers once they carry a derived purity flag.
        if self.transaction_depth > 0
            && matches!(sig.purity, crate::builtins::Purity::SideEffecting)
        {
            self.error(
                self.token_span(&callee_name_tok),
                "T0026",
                format!(
                    "side-effecting operator `{callee_name}` called inside `transaction [...]` (transactions must be pure)"
                ),
            );
        }

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
            .map(|(_, k)| k.clone());

        let arg_ty = match arg.value() {
            Some(v) => self.check_expr(&v, scope),
            None => Type::Unknown,
        };

        match declared {
            Some(crate::builtins::ParamKind::Concrete(expected)) => {
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
            Some(crate::builtins::ParamKind::AnyRelation) => {
                provided.insert(name.clone());
                // Accept any `Relation H` regardless of heading.
                // `Type::Unknown` (error recovery) also slips through
                // so we don't pile errors on top of upstream failures.
                if !matches!(arg_ty, Type::Relation(_) | Type::Unknown) {
                    let span = arg
                        .value()
                        .map(|v| self.node_span(v.syntax()))
                        .unwrap_or_else(|| self.node_span(arg.syntax()));
                    self.error(
                        span,
                        "T0004",
                        format!("argument `{name}` expected a Relation, got {arg_ty}"),
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

/// Human-readable form of a binary operator, used in T0020/T0021
/// diagnostic messages. Surfaces the same lexeme the user typed.
fn op_display(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::LtEq => "<=",
        BinaryOp::GtEq => ">=",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Where => "where",
        BinaryOp::Join => "join",
        BinaryOp::Times => "times",
        BinaryOp::Compose => "compose",
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
    fn private_relvar_assignment_checks_clean() {
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ R := Relation { {a: 1} }; write_relation { rel: R }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn private_relvar_resolves_in_scope_no_t0001() {
        // Before M1a a bare private-relvar name was T0001; now it resolves.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ write_relation { rel: R }; ];";
        assert!(!codes(src).contains(&"T0001"), "{:?}", codes(src));
    }

    #[test]
    fn assignment_heading_mismatch_diagnoses_t0034() {
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ R := Relation { {b: 1} }; ];";
        assert!(codes(src).contains(&"T0034"), "{:?}", codes(src));
    }

    #[test]
    fn assignment_to_public_relvar_diagnoses_t0033() {
        // Public relvars are read-only in v1.
        let src = "program p; public relvar R { a: Integer } key { a }; \
                   oper main {} [ R := Relation { {a: 1} }; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn assignment_to_undeclared_name_diagnoses_t0033() {
        let src = "program p; oper main {} [ Nope := Relation { {a: 1} }; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn join_with_shared_attribute_checks_clean() {
        // R { a, b } join S { a, c } shares `a` (same type) -> ok, result { a, b, c }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R join S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn join_with_disjoint_headings_diagnoses_t0035() {
        // No shared attribute -> the user wants `times`.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { b: Integer } key { b }; \
                   oper main {} [ write_relation { rel: R join S }; ];";
        assert!(codes(src).contains(&"T0035"), "{:?}", codes(src));
    }

    #[test]
    fn join_with_shared_attribute_type_mismatch_diagnoses_t0036() {
        // Shared name `a` but Integer on one side, Text on the other.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { a: Text } key { a }; \
                   oper main {} [ write_relation { rel: R join S }; ];";
        assert!(codes(src).contains(&"T0036"), "{:?}", codes(src));
    }

    #[test]
    fn times_with_disjoint_headings_checks_clean() {
        // R { a } times S { b } — disjoint -> ok, result { a, b }.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { b: Integer } key { b }; \
                   oper main {} [ write_relation { rel: R times S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn times_with_shared_attribute_diagnoses_t0037() {
        // Shared attribute `a` -> not disjoint -> the user wants `join`.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R times S }; ];";
        assert!(codes(src).contains(&"T0037"), "{:?}", codes(src));
    }

    #[test]
    fn compose_with_shared_attribute_removes_it_checks_clean() {
        // R { a, b } compose S { a, c } shares `a` -> join on `a`, remove `a`,
        // result { b, c }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R compose S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn compose_with_disjoint_headings_diagnoses_t0035() {
        // Like `join`, `compose` requires overlap; disjoint -> suggest `times`.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { b: Integer } key { b }; \
                   oper main {} [ write_relation { rel: R compose S }; ];";
        assert!(codes(src).contains(&"T0035"), "{:?}", codes(src));
    }

    #[test]
    fn compose_with_shared_type_mismatch_diagnoses_t0036() {
        // Shared name `a` but Integer on one side, Text on the other.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { a: Text } key { a }; \
                   oper main {} [ write_relation { rel: R compose S }; ];";
        assert!(codes(src).contains(&"T0036"), "{:?}", codes(src));
    }

    #[test]
    fn join_feeding_where_project_checks_clean() {
        // A `join` feeds the already-implemented `where` and `project`: the join's
        // union heading is injected into the predicate scope (`dept_name` resolves),
        // then the result narrows to { dept_name, emp_name }. No new machinery.
        let src = "program p; \
                   private relvar Employees { emp_id: Integer, emp_name: Text, dept_id: Integer } key { emp_id }; \
                   private relvar Departments { dept_id: Integer, dept_name: Text } key { dept_id }; \
                   oper main {} [ write_relation { rel: (Employees join Departments) where dept_name = \"Engineering\" project { emp_name, dept_name } }; ];";
        let diags = diagnostics(src);
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
        // The inner `let x` shadows the outer binding (here the parameter
        // `x`) and the later reference resolves to the inner. `let _ = x`
        // reads the parameter so it isn't itself flagged unused.
        let src =
            "oper f { x: Integer } [ let _ = x; let x = \"shadowed\"; write_line{message: x}; ];";
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
        let src = "oper main {} [ let _count = 42; ];";
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
            .expect("expected Integer hint for `_count`");
        // The hint span ends at the byte position right after `_count`.
        let count_end = src.find("_count").unwrap() + "_count".len();
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

    // ── Relation literals (Phase 19) ─────────────────────────────────

    #[test]
    fn relation_lit_with_uniform_tuples_checks_clean() {
        let src = "oper main {} [ \
                   let _r = Relation { {a: 1}, {a: 2}, {a: 3} }; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn empty_relation_lit_diagnoses_t0018() {
        let src = "oper main {} [ let r = Relation {}; ];";
        assert!(codes(src).contains(&"T0018"));
    }

    #[test]
    fn relation_lit_heading_mismatch_diagnoses_t0019() {
        // First tuple has only `a`; second has `a` + `b`.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: 2, b: \"x\"} }; \
                   ];";
        assert!(codes(src).contains(&"T0019"));
    }

    #[test]
    fn relation_lit_attr_type_mismatch_diagnoses_t0019() {
        // Same attribute name but different type — heading mismatch.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: \"x\"} }; \
                   ];";
        assert!(codes(src).contains(&"T0019"));
    }

    // ── Binary infix + where (Phase 20) ──────────────────────────────

    #[test]
    fn comparison_returns_boolean() {
        // Use the result as a Boolean argument to `write_relation`'s
        // polymorphic param — no, that wants a Relation. Easier:
        // assign to a let and the typechecker's hint surfaces the
        // inferred type as Boolean; for now just confirm no diags.
        let src = "oper main {} [ let _b = 1 = 2; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn where_on_relation_filter_checks_clean() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let _s = r where a = 2; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn where_non_boolean_predicate_diagnoses_t0020() {
        // Predicate is `1` (Integer) — not Boolean. T0020 fires.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r where 1; \
                   ];";
        assert!(codes(src).contains(&"T0020"));
    }

    #[test]
    fn scalar_op_type_mismatch_diagnoses_t0021() {
        // `1 = \"x\"` mixes Integer with Text — T0021 fires.
        let src = "oper main {} [ let b = 1 = \"x\"; ];";
        assert!(codes(src).contains(&"T0021"));
    }

    #[test]
    fn text_equality_typechecks() {
        // `=` and `<>` accept matching Text operands (result Boolean); no T0021.
        let src = "oper main {} [ let _a = \"x\" = \"y\"; let _b = \"x\" <> \"y\"; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn field_init_shorthand_resolves_the_binding() {
        // `write_line { message }` ≡ `{ message: message }`: with `message`
        // bound in scope it checks clean and matches the `message` parameter.
        let src = "oper main {} [ let message = \"hi\"; write_line { message }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn field_init_shorthand_unbound_diagnoses_t0001() {
        // Shorthand `{ message }` with no `message` in scope is an unresolved
        // name, exactly like `{ message: message }` would be.
        let src = "oper main {} [ write_line { message }; ];";
        assert!(codes(src).contains(&"T0001"));
    }

    #[test]
    fn tuple_field_init_shorthand_builds_heading() {
        // `{a}` ≡ `{a: a}` — the tuple gets attribute `a` of a's type, so
        // `t.a` resolves.
        let src = "oper main {} [ let a = 1; let t = {a}; let _n = t.a; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn where_non_relation_lhs_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 where true; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    #[test]
    fn where_heading_attrs_shadow_outer_locals() {
        // Outer `a` is Text; the predicate's `a` must resolve to the
        // heading's Integer attribute, so the comparison `a = 2`
        // (Integer = Integer) typechecks cleanly. The outer `a` is used by
        // `write_line` so it isn't itself flagged unused — its presence is
        // what makes this a genuine shadowing test.
        let src = "oper main {} [ \
                   let a = \"outer\"; \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let _s = r where a = 2; \
                   write_line { message: a }; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn and_or_on_non_boolean_diagnoses_t0021() {
        let src = "oper main {} [ let b = 1 and 2; ];";
        assert!(codes(src).contains(&"T0021"));
    }

    // ── project ──────────────────────────────────────────────────────

    #[test]
    fn project_on_relation_checks_clean() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let _s = r project {a}; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn project_unknown_attr_diagnoses_t0027() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r project {nope}; \
                   ];";
        assert!(codes(src).contains(&"T0027"));
    }

    #[test]
    fn project_duplicate_attr_diagnoses_t0028() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r project {a, a}; \
                   ];";
        assert!(codes(src).contains(&"T0028"));
    }

    #[test]
    fn project_non_relation_lhs_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 project {a}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    #[test]
    fn project_narrows_heading_drops_unprojected_attr() {
        // After `project {a}` the heading is `{a}`; `extract` gives
        // `Tuple {a}`, so accessing the projected-away `b` is an unknown
        // field (T0017) — proof the heading was actually narrowed.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let t = extract (r project {a}); \
                   let x = t.b; \
                   ];";
        assert!(codes(src).contains(&"T0017"));
    }

    #[test]
    fn project_keeps_projected_attr_accessible() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let t = extract (r project {a}); \
                   let _x = t.a; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn project_all_but_keeps_the_complement() {
        // `all but {a}` over {a, b} keeps {b}: `t.b` is accessible…
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2} }; \
                  let t = extract (r project all but {a}); \
                  let _x = t.b; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
        // …and the removed `a` is gone (T0017 on access).
        let gone = "oper main {} [ \
                    let r = Relation { {a: 1, b: 2} }; \
                    let t = extract (r project all but {a}); \
                    let x = t.a; \
                    ];";
        assert!(codes(gone).contains(&"T0017"));
    }

    #[test]
    fn project_all_but_unknown_attr_diagnoses_t0027() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r project all but {nope}; \
                   ];";
        assert!(codes(src).contains(&"T0027"));
    }

    #[test]
    fn project_all_but_everything_yields_empty_heading() {
        // Removing every attribute leaves the empty heading; even the
        // formerly-present `a` is then an unknown field (T0017).
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let t = extract (r project all but {a, b}); \
                   let x = t.a; \
                   ];";
        assert!(codes(src).contains(&"T0017"));
    }

    #[test]
    fn project_all_but_nothing_keeps_all() {
        // `all but {}` removes nothing — both attributes remain accessible.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let t = extract (r project all but {}); \
                   let _x = t.a; \
                   let _y = t.b; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    // ── rename ────────────────────────────────────────────────────────

    #[test]
    fn rename_remaps_the_heading() {
        // {a, b} rename {a: x}: `x` is accessible, `a` is gone (T0017).
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2} }; \
                  let t = extract (r rename {a: x}); \
                  let _v = t.x; let _w = t.b; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
        let gone = "oper main {} [ \
                    let r = Relation { {a: 1, b: 2} }; \
                    let t = extract (r rename {a: x}); \
                    let v = t.a; \
                    ];";
        assert!(codes(gone).contains(&"T0017"));
    }

    #[test]
    fn rename_unknown_source_diagnoses_t0029() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r rename {nope: x}; \
                   ];";
        assert!(codes(src).contains(&"T0029"));
    }

    #[test]
    fn rename_target_not_a_name_diagnoses_t0030() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r rename {a: 42}; \
                   ];";
        assert!(codes(src).contains(&"T0030"));
    }

    #[test]
    fn rename_target_collision_diagnoses_t0031() {
        // a → b, but b already exists → not a bijection.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r rename {a: b}; \
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn rename_duplicate_source_diagnoses_t0031() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r rename {a: x, a: y}; \
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn rename_swap_is_a_valid_bijection() {
        // {a, b} rename {a: b, b: a} swaps names — no collision.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let _s = r rename {a: b, b: a}; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn rename_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 rename {a: b}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    // ── extract (Phase 21) ───────────────────────────────────────────

    #[test]
    fn extract_on_relation_checks_clean() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let _t = extract (r where a = 2); \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn extract_field_access_threads_into_call() {
        // `extract (r where a = 2).b` should typecheck if the tuple
        // has a `b` attribute. Use it in a write_line call to exercise
        // the whole pipeline.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: \"hi\"}, {a: 2, b: \"ho\"} }; \
                   let t = extract (r where a = 2); \
                   write_line { message: t.b }; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    // ── unused-binding warning (T0032) ───────────────────────────────

    #[test]
    fn unused_let_binding_warns_t0032() {
        let src = "oper main {} [ let x = 1; ];";
        assert!(codes(src).contains(&"T0032"));
    }

    #[test]
    fn unused_binding_is_a_warning_not_an_error() {
        let src = "oper main {} [ let x = 1; ];";
        let d = diagnostics(src);
        let t0032 = d.iter().find(|d| d.code == "T0032").expect("expected T0032");
        assert_eq!(t0032.severity, coddl_diagnostics::Severity::Warning);
    }

    #[test]
    fn underscore_prefixed_binding_is_exempt() {
        let src = "oper main {} [ let _x = 1; ];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn bare_underscore_binding_is_exempt() {
        let src = "oper main {} [ let _ = 1; ];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn used_binding_does_not_warn() {
        let src = "oper main {} [ let x = \"hi\"; write_line { message: x }; ];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn shadowed_then_unused_binding_warns_once() {
        // `x = "a"` is shadowed and never read → warns; the active `x = "b"`
        // is used → no warning. Exactly one T0032.
        let src = "oper main {} [ let x = \"a\"; let x = \"b\"; write_line { message: x }; ];";
        let n = codes(src).iter().filter(|c| **c == "T0032").count();
        assert_eq!(n, 1, "only the shadowed `x` should warn: {:?}", diagnostics(src));
    }

    #[test]
    fn where_predicate_attrs_do_not_warn() {
        // The heading attr `a` injected into the predicate scope must not be
        // flagged unused (WhereAttr origin); only user `let`s warn. `_s` is
        // exempt, `r` is used, so the program is diagnostic-free.
        let src =
            "oper main {} [ let r = Relation { {a: 1}, {a: 2} }; let _s = r where a = 2; ];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn binding_used_only_inside_a_pushed_expression_is_used() {
        // `r` is referenced only inside `r where a = 2` (an expression the
        // lowerer may fold/push away) — usage is a source-level fact, so `r`
        // is not flagged. `_s` is exempt.
        let src =
            "oper main {} [ let r = Relation { {a: 1}, {a: 2} }; let _s = r where a = 2; ];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn unused_parameter_warns_t0032() {
        let src = "oper f { x: Integer } [];";
        assert!(codes(src).contains(&"T0032"));
    }

    #[test]
    fn underscore_prefixed_parameter_is_exempt() {
        let src = "oper f { _x: Integer } [];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn used_parameter_does_not_warn() {
        let src = "oper f { x: Text } [ write_line { message: x }; ];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn self_parameter_is_exempt_even_when_unused() {
        // `self` is the UFCS receiver; renaming it to `_self` would break
        // `x.method { ... }` call syntax, so it never warns even when unused.
        let src = "oper describe { self: Text } [ write_line { message: \"a thing\" }; ];";
        assert!(!codes(src).contains(&"T0032"));
    }

    #[test]
    fn extract_on_non_relation_diagnoses_t0024() {
        let src = "oper main {} [ let t = extract 42; ];";
        assert!(codes(src).contains(&"T0024"));
    }

    // ── public relvars + transaction scope (Phase 22) ────────────────

    const HELLO_DB_PRELUDE: &str = "program p; \
                                    database greetings; \
                                    public relvar Greetings { id: Integer, message: Text } \
                                    key { id }; ";

    #[test]
    fn public_relvar_inside_transaction_checks_clean() {
        let src = format!(
            "{}oper main {{}} [ \
             let g = transaction [ extract (Greetings where id = 1) ]; \
             write_line {{ message: g.message }}; \
             ];",
            HELLO_DB_PRELUDE
        );
        let diags = diagnostics(&src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn public_relvar_outside_transaction_diagnoses_t0025() {
        let src = format!(
            "{}oper main {{}} [ \
             let g = extract (Greetings where id = 1); \
             write_line {{ message: g.message }}; \
             ];",
            HELLO_DB_PRELUDE
        );
        assert!(
            codes(&src).contains(&"T0025"),
            "expected T0025, got {:?}",
            codes(&src)
        );
    }

    #[test]
    fn private_relvar_outside_transaction_does_not_fire_t0025() {
        // private relvars are local program state per RM Pre 14;
        // they aren't database-backed and so don't need a transaction.
        let src = "program p; \
                   private relvar Local { id: Integer } key { id }; \
                   oper main {} [];";
        assert!(
            !codes(src).contains(&"T0025"),
            "T0025 should not fire on private relvars: {:?}",
            codes(src)
        );
    }

    #[test]
    fn write_line_inside_transaction_diagnoses_t0026() {
        // `write_line` is SideEffecting; calling it inside a
        // transaction breaks replay safety.
        let src = "oper main {} [ \
                   transaction [ write_line { message: \"x\" } ]; \
                   ];";
        assert!(
            codes(src).contains(&"T0026"),
            "expected T0026, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn write_line_outside_transaction_no_t0026() {
        let src = "oper main {} [ write_line { message: \"x\" }; ];";
        assert!(
            !codes(src).contains(&"T0026"),
            "T0026 should not fire outside transactions: {:?}",
            codes(src)
        );
    }

    #[test]
    fn pure_ops_inside_transaction_no_t0026() {
        // Pure ops (extract / where / relation lit / scalar ops) are
        // all fine inside a transaction.
        let src = format!(
            "{}oper main {{}} [ \
             let g = transaction [ extract (Greetings where id = 1) ]; \
             ];",
            HELLO_DB_PRELUDE
        );
        assert!(
            !codes(&src).contains(&"T0026"),
            "T0026 should not fire on pure ops: {:?}",
            codes(&src)
        );
    }
}

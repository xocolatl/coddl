//! The typechecker walk.
//!
//! `TypeChecker` walks the AST produced by `coddl-syntax`, resolving
//! names, validating call sites against the built-in registry, and
//! emitting diagnostics with stable `T####` codes. Walk methods are
//! named to mirror the productions in `docs/grammar.md` (`parse_oper_decl`
//! → `check_oper_decl`, etc.); `docs/typecheck.md` is the spec they
//! enforce.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use coddl_diagnostics::{Diagnostic, FileId, Span};
use coddl_syntax::ast::{
    AssignStmt, AstNode, BinaryExpr, BinaryOp, Block, CallExpr, DeleteStmt, DoWhileStmt, Expr,
    ExprStmt,
    InsertStmt,
    ExtendExpr, FieldAccess, ForStmt, Heading as AstHeading, IfExpr, IndexExpr, Item, KeyClause,
    LetStmt, LoadStmt,
    NamedArg, OperDecl,
    PrivateRelvarDecl, ProgramDecl, ProjectExpr, PublicRelvarDecl, RelationLit, RenameExpr,
    ReplaceExpr, Root, SequenceLit, Stmt, TcloseExpr, TransactionExpr, TruncateStmt, TupleLit,
    TypeDecl, TypeRef, UnaryExpr, UnaryOp, UnwrapExpr, UpdateStmt, VarStmt, WhileStmt, WrapExpr,
};
use coddl_stdlib::ModulePath;
use coddl_syntax::ast_cddb::{BaseRelvarDecl, CddbItem, CddbRoot, VirtualRelvarDecl};
use coddl_syntax::cst::{SyntaxNode, SyntaxToken};
use coddl_syntax::{parse, parse_format_template, FileKind, SyntaxKind, TemplateChunk};

use crate::builtins::{Builtins, OperSig, ParamKind, Purity};
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
    /// A mutable `var x := …` binding. Reassignable via `x := …` (T0074
    /// otherwise, for `Let`/`Param`); every occurrence is reported to editors
    /// as mutable (the LSP's `mutable` semantic-token modifier). Warns unused
    /// like `Let`.
    Var,
    Param,
    Relvar,
    WhereAttr,
    /// The counter of a counted `for i := lo to hi` loop. Loop-scoped and
    /// immutable (assigning it is T0072); excluded from the unused-binding
    /// warning (a counted loop may legitimately ignore its counter).
    ForCounter,
}

/// One binding in a scope layer: its `Type` for lookup, plus the metadata the
/// binding lints + definite-assignment need — the name-token span (for the
/// squiggle), the origin, whether any `NameRef` ever resolved to it (`used`),
/// whether it was ever the target of a reassignment (`reassigned`; a `var` that
/// never is should be a `let`, T0077), and whether it is definitely assigned at
/// the current program point (`initialized`; reading an uninitialized `var` is
/// T0079). For an unannotated `var x;`, `ty` starts `Unknown` and is inferred
/// from the first assignment.
struct Binding {
    ty: Type,
    name: String,
    span: Span,
    origin: BindingOrigin,
    used: bool,
    reassigned: bool,
    initialized: bool,
    /// The parsed template for a `let x = f"…"` binding, so each later
    /// `format { template: x, … }` reuses the same chunks. `None` for every
    /// other binding. See the type-level note above.
    format_template: Option<Rc<Vec<TemplateChunk>>>,
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
            reassigned: false,
            // Assigned by default; an uninitialized `var x;` clears this via
            // `mark_uninitialized` right after insertion.
            initialized: true,
            format_template: None,
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

    /// The origin of the active binding for `name` (innermost layer first),
    /// or `None` if unbound. Used to give an assignment to a loop counter a
    /// dedicated diagnostic (T0072).
    fn origin(&self, name: &str) -> Option<BindingOrigin> {
        for layer in (0..self.layers.len()).rev() {
            if let Some(&idx) = self.layers[layer].get(name) {
                return Some(self.records[layer][idx].origin);
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

    /// Attach a parsed `f"…"` template to the active binding for `name` — the
    /// `let x = f"…"` case, so a later `format { template: x, … }` reuses it.
    fn attach_format_template(&mut self, name: &str, chunks: Option<Rc<Vec<TemplateChunk>>>) {
        if let Some((l, i)) = self.locate(name) {
            self.records[l][i].format_template = chunks;
        }
    }

    /// The parsed template of the active binding for `name`, if it is a
    /// `let`-bound `f"…"` template (else `None`). The `Rc` clone is cheap.
    fn format_template(&self, name: &str) -> Option<Rc<Vec<TemplateChunk>>> {
        self.locate(name)
            .and_then(|(l, i)| self.records[l][i].format_template.clone())
    }

    /// Mark the active binding for `name` as reassigned (innermost layer
    /// first) — a `var` that is never reassigned should be a `let` (T0077).
    fn mark_reassigned(&mut self, name: &str) {
        for layer in (0..self.layers.len()).rev() {
            if let Some(&idx) = self.layers[layer].get(name) {
                self.records[layer][idx].reassigned = true;
                return;
            }
        }
    }

    /// Find the active binding for `name` as a stable `(layer, idx)` handle
    /// (innermost layer first). `(layer, idx)` survives arm scopes being
    /// pushed/popped, so it identifies a binding across the definite-assignment
    /// snapshot dance below (where `name` alone would be ambiguous under
    /// shadowing).
    fn locate(&self, name: &str) -> Option<(usize, usize)> {
        for layer in (0..self.layers.len()).rev() {
            if let Some(&idx) = self.layers[layer].get(name) {
                return Some((layer, idx));
            }
        }
        None
    }

    /// Clear the active binding's `initialized` flag — for a freshly declared
    /// uninitialized `var x;` (definite-assignment starts it un-assigned).
    fn mark_uninitialized(&mut self, name: &str) {
        if let Some((l, i)) = self.locate(name) {
            self.records[l][i].initialized = false;
        }
    }

    /// Mark the active binding for `name` definitely assigned (its first/any
    /// assignment reached this program point unconditionally).
    fn mark_initialized(&mut self, name: &str) {
        if let Some((l, i)) = self.locate(name) {
            self.records[l][i].initialized = true;
        }
    }

    /// Whether the active binding for `name` is definitely assigned. An unbound
    /// or unknown name reports `true` so unrelated resolution failures (T0001)
    /// don't also trip the read-before-assignment check (T0079).
    fn is_initialized(&self, name: &str) -> bool {
        match self.locate(name) {
            Some((l, i)) => self.records[l][i].initialized,
            None => true,
        }
    }

    /// Set the active binding's type — used to infer an unannotated `var x;`
    /// from its first assignment.
    fn set_type(&mut self, name: &str, ty: Type) {
        if let Some((l, i)) = self.locate(name) {
            self.records[l][i].ty = ty;
        }
    }

    /// The name-token span of the active binding for `name` — the anchor for a
    /// deferred inlay hint on an unannotated `var x;` once its type is inferred.
    fn binding_span(&self, name: &str) -> Option<Span> {
        self.locate(name).map(|(l, i)| self.records[l][i].span)
    }

    /// Every currently-uninitialized binding, as `(layer, idx)` handles — the
    /// definite-assignment state to intersect across `if` arms / restore after
    /// a conditional or loop body.
    fn uninit_snapshot(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (l, layer) in self.records.iter().enumerate() {
            for (i, b) in layer.iter().enumerate() {
                if !b.initialized {
                    out.push((l, i));
                }
            }
        }
        out
    }

    /// Reset the given bindings to uninitialized — undoing any assignments a
    /// conditional/loop body made (its effects don't persist to the join).
    fn restore_uninit(&mut self, snap: &[(usize, usize)]) {
        for &(l, i) in snap {
            self.records[l][i].initialized = false;
        }
    }

    /// The snapshot entries that are now initialized — the vars a walked arm
    /// definitely assigned. Intersecting `then`/`else` results gives the vars
    /// assigned on *both* paths (definitely assigned after the `if`).
    fn newly_initialized(&self, snap: &[(usize, usize)]) -> Vec<(usize, usize)> {
        snap.iter()
            .copied()
            .filter(|&(l, i)| self.records[l][i].initialized)
            .collect()
    }

    /// Mark a `(layer, idx)` binding definitely assigned (the post-`if` commit).
    fn set_initialized_at(&mut self, l: usize, i: usize) {
        self.records[l][i].initialized = true;
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
    /// Byte ranges of every occurrence — declaration, read, and write — of a
    /// mutable `var` binding, collected as a side product of name resolution.
    /// The LSP emits one `variable`+`mutable` semantic token per span (the
    /// rust-analyzer-style mutability marking); no symbol table or tree walk
    /// is needed downstream.
    pub mutable_spans: Vec<Span>,
    /// All relvars declared in this file. For `.cd`: public + private
    /// (and any base/virtual the user mistakenly placed in `.cd`,
    /// which T0014 flags). For `.cddb`: base + virtual (similarly).
    /// Empty for `.cdmap` / `.cdstore` — those don't declare relvars.
    pub relvars: RelvarTable,
    /// Every resolved type alias in scope — user `type Name = …;` declarations
    /// and the type aliases of active (`use module`) stdlib modules (e.g.
    /// `coddl::web`'s `Request`/`Response`). Each maps to its fully-resolved
    /// `Type`. The ProcIR lowerer absorbs this so operator signatures naming an
    /// alias resolve (the static `resolve_type_ref_quiet` knows only inline
    /// types and builtins).
    pub type_aliases: HashMap<String, Type>,
    /// Every user-defined single-possrep scalar type in scope (`type Name {
    /// component: T };`) → its possrep component. The ProcIR lowerer absorbs
    /// this to erase a `Type::Scalar(name)` to its component's representation
    /// and to lower the selector / accessor as identity. See `docs/typecheck.md`.
    pub nominal_scalars: HashMap<String, PossrepScalar>,
}

/// A user-defined single-possrep scalar's possrep: its one component's name and
/// type. `RawRequestPath { value: Text }` → `{ component: "value", ty: Text }`.
/// Single-component only for now (a multi-component possrep is rejected with
/// T0091; multi-*possrep* is a later tier).
#[derive(Debug, Clone)]
pub struct PossrepScalar {
    pub component: String,
    pub ty: Type,
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
        mutable_spans: Vec::new(),
        relvars: RelvarTable::new(),
        transaction_depth: 0,
        public_relvars: HashSet::new(),
        user_opers: HashMap::new(),
        type_aliases: HashMap::new(),
        nominal_scalars: HashMap::new(),
        active_modules: HashSet::new(),
        stdlib_oper_owner: HashMap::new(),
        stdlib_type_owner: HashMap::new(),
        stdlib_relvar_owner: HashMap::new(),
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
        mutable_spans: tc.mutable_spans,
        relvars: tc.relvars,
        type_aliases: tc.type_aliases,
        nominal_scalars: tc.nominal_scalars,
    }
}

/// Quiet (no-diagnostic) resolution of a `TypeRef` to a `Type`. The static
/// counterpart of [`TypeChecker::resolve_type_ref`]: `Sequence T` recurses,
/// `Relation { H }` / `Tuple { H }` build the heading via [`heading_quiet`], and
/// an unknown leaf becomes `Unknown` silently. Used by the operator-signature
/// pre-pass (where a loud resolve would double-report T0005) and by the ProcIR
/// lowerer (to resolve a `let`/`var` annotation's heading without a checker).
pub fn resolve_type_ref_quiet(tr: &TypeRef) -> Type {
    let Some(name_tok) = tr.name() else {
        return Type::Unknown;
    };
    if name_tok.text() == "Sequence" {
        let elem = tr
            .element()
            .map(|e| resolve_type_ref_quiet(&e))
            .unwrap_or(Type::Unknown);
        return Type::Sequence(Box::new(elem));
    }
    if name_tok.text() == "Relation" {
        if let Some(h) = tr.heading() {
            return Type::Relation(heading_quiet(&h));
        }
    }
    if name_tok.text() == "Tuple" {
        if let Some(h) = tr.heading() {
            return Type::Tuple(heading_quiet(&h));
        }
    }
    Type::from_builtin_name(name_tok.text()).unwrap_or(Type::Unknown)
}

/// Quiet (no-diagnostic) heading builder — the static sibling of
/// [`TypeChecker::resolve_heading`]. Skips duplicate detection (the loud
/// body-walking pass re-reports T0007); a missing attribute type is `Unknown`.
fn heading_quiet(heading: &AstHeading) -> Heading {
    let mut fields: Vec<(String, Type)> = Vec::new();
    for param in heading.params() {
        let Some(name_tok) = param.name() else {
            continue;
        };
        let ty = param
            .type_ref()
            .map(|tr| resolve_type_ref_quiet(&tr))
            .unwrap_or(Type::Unknown);
        fields.push((name_tok.text().to_string(), ty));
    }
    Heading::new(fields)
}

/// Context-aware [`resolve_type_ref_quiet`] for resolving a module's alias RHS:
/// an otherwise-unknown leaf that names one of the module's own **type aliases**
/// (e.g. `RawRequest`'s `headers: OrderedNameValues`) or **possrep scalars**
/// (e.g. `path: RawRequestPath`) resolves to the alias's type / `Type::Scalar`
/// rather than `Unknown` — a plain quiet resolve would leave the field
/// `Unknown`, which then survives into lowering. The caller resolves the
/// module's declarations in source order, so referenced aliases/scalars are
/// already registered.
fn resolve_type_ref_quiet_with_scalars(
    tr: &TypeRef,
    scalars: &HashMap<String, PossrepScalar>,
    aliases: &HashMap<String, Type>,
) -> Type {
    let Some(name_tok) = tr.name() else {
        return Type::Unknown;
    };
    if name_tok.text() == "Sequence" {
        let elem = tr
            .element()
            .map(|e| resolve_type_ref_quiet_with_scalars(&e, scalars, aliases))
            .unwrap_or(Type::Unknown);
        return Type::Sequence(Box::new(elem));
    }
    if name_tok.text() == "Relation" {
        if let Some(h) = tr.heading() {
            return Type::Relation(heading_quiet_with_scalars(&h, scalars, aliases));
        }
    }
    if name_tok.text() == "Tuple" {
        if let Some(h) = tr.heading() {
            return Type::Tuple(heading_quiet_with_scalars(&h, scalars, aliases));
        }
    }
    if let Some(t) = Type::from_builtin_name(name_tok.text()) {
        return t;
    }
    if let Some(t) = aliases.get(name_tok.text()) {
        return t.clone();
    }
    if scalars.contains_key(name_tok.text()) {
        return Type::Scalar(name_tok.text().to_string());
    }
    Type::Unknown
}

/// Context-aware [`heading_quiet`] — resolves each attribute type through
/// [`resolve_type_ref_quiet_with_scalars`].
fn heading_quiet_with_scalars(
    heading: &AstHeading,
    scalars: &HashMap<String, PossrepScalar>,
    aliases: &HashMap<String, Type>,
) -> Heading {
    let mut fields: Vec<(String, Type)> = Vec::new();
    for param in heading.params() {
        let Some(name_tok) = param.name() else {
            continue;
        };
        let ty = param
            .type_ref()
            .map(|tr| resolve_type_ref_quiet_with_scalars(&tr, scalars, aliases))
            .unwrap_or(Type::Unknown);
        fields.push((name_tok.text().to_string(), ty));
    }
    Heading::new(fields)
}

struct TypeChecker {
    file: FileId,
    file_kind: FileKind,
    builtins: Builtins,
    diagnostics: Vec<Diagnostic>,
    hints: Vec<TypeHint>,
    /// Occurrence spans of mutable `var` bindings; see [`CheckOutput`].
    mutable_spans: Vec<Span>,
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
    /// Signatures of every user-defined `oper` in this file, collected in a
    /// pre-pass (sibling of `relvars`) before any body is walked, so a call
    /// resolves regardless of declaration order (forward references). A call
    /// whose callee is in this table is checked through the same monomorphic
    /// path as a single-signature builtin. Names are unique across builtins ∪
    /// user ops — a collision is rejected at registration with T0060.
    user_opers: HashMap<String, crate::builtins::OperSig>,
    /// User-defined type aliases (`type Name = <type-ref>;`), collected in a
    /// pre-pass so a later type reference resolves regardless of declaration
    /// order. Consulted by `resolve_type_name` after the built-in type names.
    /// The *loud* resolution path (`resolve_type_ref`) reads this; the quiet
    /// free `resolve_type_ref_quiet` (user-oper pre-pass, ProcIR lowerer) does
    /// not yet, so an alias used as a user-oper param type resolves quietly to
    /// `Unknown` until that path is threaded through. Once a file `use`s an
    /// opt-in stdlib module, that module's aliases are inserted here too.
    type_aliases: HashMap<String, Type>,
    /// User-defined single-possrep scalar types (`type Name { c: T };`) → their
    /// possrep component. Registered in the type-decl pre-pass; consulted by
    /// `resolve_type_name` (→ `Type::Scalar`), the possrep accessor
    /// (`check_field_access`), and the synthesized selector (`check_call`).
    /// Mirrored into `CheckOutput` for the ProcIR lowerer.
    nominal_scalars: HashMap<String, PossrepScalar>,
    /// The opt-in stdlib modules this file has brought into scope with
    /// `use module <path>;`. `coddl::core` is always in scope and is not
    /// required here. Populated by [`Self::resolve_modules`] before any body is
    /// walked; consulted when deciding whether an opt-in module's operators /
    /// types are visible.
    active_modules: HashSet<ModulePath>,
    /// Every opt-in (non-`core`) stdlib operator name → the module that owns it.
    /// Built from the embedded stdlib regardless of what this file imports, and
    /// consulted **only** to upgrade an unresolved-name error (T0001) into the
    /// actionable "add `use module …`" hint (T0087). It never puts a name in
    /// scope — that is [`Self::active_modules`]'s job — so an un-imported stdlib
    /// name stays a free identifier the user may define themselves.
    stdlib_oper_owner: HashMap<String, ModulePath>,
    /// Every opt-in (non-`core`) stdlib type name → its owning module. The type
    /// analogue of [`Self::stdlib_oper_owner`]; upgrades T0005 → T0088.
    stdlib_type_owner: HashMap<String, ModulePath>,
    /// Every opt-in (non-`core`) stdlib `builtin relvar` name → its owning
    /// module. The relvar analogue of [`Self::stdlib_oper_owner`]; upgrades an
    /// unresolved `NameRef` → T0090.
    stdlib_relvar_owner: HashMap<String, ModulePath>,
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

    /// Binding lints for a popped scope layer: T0032 for a `let`/`var`/param
    /// no `NameRef` ever resolved to, and T0077 for a `var` that is read but
    /// never reassigned (it should be a `let`). A leading `_` (including bare
    /// `_`) opts out of both — the "unused-OK" convention. Injected names
    /// (relvars, `where` attributes) are excluded by origin.
    fn warn_unused(&mut self, layer: Vec<Binding>) {
        for b in layer {
            // A leading `_` opts out of every binding lint. `self` is the UFCS
            // receiver — a parameter literally named `self` is what makes an
            // `oper` callable as `x.method { ... }`, so renaming it to `_self`
            // would break that call syntax; it never warns even when ignored.
            if b.name.starts_with('_') || b.name == "self" {
                continue;
            }
            // Never read → unused binding (T0032), for `let`/`var`/parameters.
            if !b.used
                && matches!(
                    b.origin,
                    BindingOrigin::Let | BindingOrigin::Var | BindingOrigin::Param
                )
            {
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
                continue;
            }
            // Read but never reassigned → a `var` that could be a `let`
            // (the analog of Rust's `unused_mut`).
            if b.origin == BindingOrigin::Var && b.used && !b.reassigned {
                self.warn(
                    b.span,
                    "T0077",
                    format!("`{}` is declared `var` but never reassigned; use `let`", b.name),
                );
            }
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
        // Pre-pass: resolve `use module …` imports FIRST. This registers an
        // imported stdlib module's builtin relvars / type aliases / operators,
        // so a same-named user declaration in the pre-passes below collides
        // (T0012 / T0086 / T0060) and bodies see the imported names.
        self.resolve_modules(root);
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
                // `builtin relvar` is inert in a checked file — see the main-pass
                // arm below. The real stdlib relvars register via `resolve_modules`.
                _ => {}
            }
        }
        // Pre-pass: register user-defined type declarations — aliases
        // (`type Name = …;`) and possrep scalars (`type Name { c: T };`) — so a
        // later type reference resolves regardless of declaration order. Runs
        // before the operator pre-pass so operator param/return types can name
        // one, and so a scalar's synthesized selector is visible to it.
        for item in root.items() {
            if let Item::TypeDecl(d) = item {
                self.register_type_decl(&d);
            }
        }
        // Pre-pass: collect every user-defined operator's signature so a
        // call site resolves regardless of declaration order (forward
        // references). Bodies are still walked in the main pass below.
        for item in root.items() {
            if let Item::OperDecl(o) = item {
                self.register_user_oper(&o);
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
                Item::TypeDecl(_) => {
                    // Type aliases are validated in the pre-pass above.
                }
                Item::UseDecl(_) => {
                    // Imports are resolved in the `resolve_modules` pre-pass
                    // above (opt-in scoping); nothing to do in the main walk.
                }
                Item::PublicRelvarDecl(_)
                | Item::PrivateRelvarDecl(_)
                | Item::BaseRelvarDecl(_)
                | Item::VirtualRelvarDecl(_) => {
                    // Relvar items walked in the pre-pass above.
                }
                Item::BuiltinRelvarDecl(_) => {
                    // A `builtin relvar` is only meaningful inside a stdlib
                    // module, where it registers via `resolve_modules` when the
                    // module is imported. In an ordinary checked file it is
                    // **inert** — exactly like a user `builtin oper` — so that a
                    // stdlib module's own source (e.g. `coddl::env`'s `env.cd`)
                    // opened in the editor typechecks clean, and a stray user
                    // `builtin relvar` simply fails to resolve at its use site
                    // rather than tripping a decl-site error the LSP can't scope.
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
            let ty = match param.type_ref() {
                Some(tr) => self.resolve_type_ref(&tr),
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

    /// Resolve `use module …` imports — opt-in module scoping. Two steps, in
    /// order:
    ///   1. Each `use module <path>;` names an embedded stdlib module. An
    ///      unknown path is **T0089**; `coddl::core` is implicit, so importing
    ///      it is a harmless no-op. The rest populate [`Self::active_modules`].
    ///   2. Every non-`core` stdlib module is scanned to build the hint catalogs
    ///      ([`Self::stdlib_oper_owner`] / [`Self::stdlib_type_owner`]), and the
    ///      *active* ones have their operators (into `builtins`) and type aliases
    ///      (into `type_aliases`) registered — lazily, so an un-imported module's
    ///      names never enter this file's namespace.
    fn resolve_modules(&mut self, root: &Root) {
        let core = ModulePath::parse("coddl::core");

        // (1) Collect the imports.
        for item in root.items() {
            let Item::UseDecl(u) = item else { continue };
            let segs: Vec<String> = u.segments().map(|t| t.text().to_string()).collect();
            if segs.is_empty() {
                continue; // malformed path — the parser already reported it
            }
            let path = ModulePath::new(segs);
            // The reserved `coddl` root is the embedded stdlib; anything else is
            // a userspace module the plan layer resolves (it has the file path
            // and does I/O). The checker defers userspace imports to it —
            // neither erroring nor bringing names into scope here — so a valid
            // `use module foo;` does not trip T0089. Registering an imported
            // module's signatures is a later step.
            let is_coddl_root = path.segments().first().map(String::as_str) == Some("coddl");
            if !is_coddl_root {
                continue;
            }
            if coddl_stdlib::resolve(&path).is_none() {
                self.error(
                    self.node_span(u.syntax()),
                    "T0089",
                    format!("unknown module `{path}` — no such module under `coddl::`"),
                );
                continue;
            }
            self.active_modules.insert(path);
        }

        // (2) Build the hint catalogs, and load the active modules' contents.
        for module in coddl_stdlib::stdlib_modules() {
            if module.path == core {
                continue; // core is always loaded (Builtins::new) and always in scope
            }
            let active = self.active_modules.contains(&module.path);
            if active {
                // Register this module's `builtin oper` signatures.
                self.builtins.load_module(module.source());
            }
            let out = parse(module.source(), FileId(0), FileKind::Cd);
            let Some(mroot) = Root::cast(out.tree) else { continue };
            for item in mroot.items() {
                match item {
                    Item::OperDecl(o) if o.is_builtin() => {
                        if let Some(n) = o.name() {
                            self.stdlib_oper_owner
                                .insert(n.text().to_string(), module.path.clone());
                        }
                    }
                    Item::TypeDecl(d) => {
                        if let Some(n) = d.name() {
                            let name = n.text().to_string();
                            self.stdlib_type_owner
                                .insert(name.clone(), module.path.clone());
                            if active {
                                if let Some(heading) = d.possrep_heading() {
                                    // Possrep scalar (single-component tier):
                                    // register the nominal type so an imported
                                    // `RawRequestPath` resolves to `Type::Scalar`,
                                    // not an `Unknown` alias.
                                    let comps: Vec<_> = heading.params().collect();
                                    if comps.len() == 1 {
                                        if let Some(cn) = comps[0].name() {
                                            let cty = comps[0]
                                                .type_ref()
                                                .map(|tr| resolve_type_ref_quiet(&tr))
                                                .unwrap_or(Type::Unknown);
                                            self.nominal_scalars.insert(
                                                name,
                                                PossrepScalar {
                                                    component: cn.text().to_string(),
                                                    ty: cty,
                                                },
                                            );
                                        }
                                    }
                                } else {
                                    // Context-aware: a field naming one of this
                                    // module's own aliases or possrep scalars
                                    // (registered above, in source order) resolves
                                    // to that type / `Type::Scalar` rather than
                                    // `Unknown`.
                                    let ty = d
                                        .aliased_type()
                                        .map(|tr| {
                                            resolve_type_ref_quiet_with_scalars(
                                                &tr,
                                                &self.nominal_scalars,
                                                &self.type_aliases,
                                            )
                                        })
                                        .unwrap_or(Type::Unknown);
                                    self.type_aliases.insert(name, ty);
                                }
                            }
                        }
                    }
                    Item::BuiltinRelvarDecl(d) => {
                        if let Some(n) = d.name() {
                            let name = n.text().to_string();
                            self.stdlib_relvar_owner
                                .insert(name.clone(), module.path.clone());
                            if active {
                                // A stdlib module's source is curated + valid, so
                                // resolving its heading/keys here emits nothing.
                                let heading = match d.heading() {
                                    Some(h) => self.resolve_heading(&h),
                                    None => Heading::empty(),
                                };
                                let keys: Vec<Vec<String>> = d
                                    .key_clauses()
                                    .map(|k| self.validate_key_clause(&k, &heading))
                                    .collect();
                                let info = RelvarInfo {
                                    kind: RelvarKind::Builtin,
                                    heading,
                                    keys,
                                    span: self.token_span(&n),
                                };
                                let _ = self.relvars.try_insert(name, info);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Collect one user `oper`'s signature into `user_opers`. Param and
    /// return types resolve *quietly* here (via `Type::from_builtin_name`):
    /// the body-walking `check_oper_decl` re-resolves the same tokens through
    /// `resolve_type_name`, which is where any T0005 (unknown type) is
    /// emitted — resolving loudly in both passes would double-report it.
    /// Every user param is `ParamKind::Concrete`; user ops default to
    /// `SideEffecting` purity — the sound default for the transaction-purity
    /// gate (T0026) until body-derived purity lands. A name that already
    /// names a builtin or an earlier user op is rejected with T0060 and the
    /// first definition wins.
    /// Register a `type Name = <type-ref>;` alias. Rejects shadowing a
    /// built-in type name (T0085) and a duplicate declaration (T0086); a bad
    /// component of the aliased type surfaces T0005 once, here. The aliased
    /// type resolves loudly, so it may name an alias registered earlier in
    /// source order.
    /// Register a `type` declaration in its pre-pass. Two forms (chosen by the
    /// parser, surfaced as `possrep_heading()` vs `aliased_type()`):
    /// - `type Name { component: T };` — a distinct nominal **possrep scalar**
    ///   (single-component tier), recorded in `nominal_scalars`.
    /// - `type Name = <type-ref>;` — a transparent **alias**, recorded in
    ///   `type_aliases`.
    fn register_type_decl(&mut self, decl: &TypeDecl) {
        let Some(name_tok) = decl.name() else { return };
        let name = name_tok.text().to_string();

        if Type::from_builtin_name(&name).is_some() {
            self.error(
                self.token_span(&name_tok),
                "T0085",
                format!("cannot redefine built-in type `{name}`"),
            );
            return;
        }
        if self.type_aliases.contains_key(&name) || self.nominal_scalars.contains_key(&name) {
            self.error(
                self.token_span(&name_tok),
                "T0086",
                format!("type `{name}` is already defined"),
            );
            return;
        }

        // Possrep-scalar form: a distinct nominal type. Single-component only for
        // now (a multi-component possrep would erase to a tuple — deferred).
        if let Some(heading) = decl.possrep_heading() {
            let comps: Vec<_> = heading.params().collect();
            if comps.len() != 1 {
                self.error(
                    self.token_span(&name_tok),
                    "T0091",
                    format!(
                        "possrep of `{name}` must have exactly one component \
                         (multi-component possreps are not yet supported)"
                    ),
                );
                return;
            }
            let Some(cname_tok) = comps[0].name() else {
                return;
            };
            let cty = comps[0]
                .type_ref()
                .map(|tr| self.resolve_type_ref(&tr))
                .unwrap_or(Type::Unknown);
            self.nominal_scalars.insert(
                name,
                PossrepScalar {
                    component: cname_tok.text().to_string(),
                    ty: cty,
                },
            );
            return;
        }

        // Alias form.
        let ty = match decl.aliased_type() {
            Some(tr) => self.resolve_type_ref(&tr),
            None => Type::Unknown,
        };
        self.type_aliases.insert(name, ty);
    }

    fn register_user_oper(&mut self, decl: &OperDecl) {
        // `builtin` declarations are compiler-provided signatures (the
        // prelude — see docs/prelude.md), not user definitions. They are
        // inert to user-oper registration until the prelude loader lands.
        if decl.is_builtin() {
            return;
        }
        let Some(name_tok) = decl.name() else { return };
        let name = name_tok.text().to_string();

        // A user `oper` can't reuse a possrep scalar's name — that name is the
        // scalar's synthesized selector (registered in the earlier type-decl
        // pre-pass).
        if self.nominal_scalars.contains_key(&name) {
            self.error(
                self.token_span(&name_tok),
                "T0060",
                format!("`{name}` is already defined as a possrep-scalar type"),
            );
            return;
        }

        // Build the heading first: operators are identified by name *and*
        // heading, so the collision check below needs the parameter shape.
        let mut params: Vec<(Cow<'static, str>, ParamKind)> = Vec::new();
        if let Some(heading) = decl.heading() {
            for param in heading.params() {
                let Some(pname_tok) = param.name() else { continue };
                let pty = param
                    .type_ref()
                    .map(|tr| resolve_type_ref_quiet(&tr))
                    .unwrap_or(Type::Unknown);
                params.push((
                    Cow::Owned(pname_tok.text().to_string()),
                    ParamKind::Concrete(pty),
                ));
            }
        }

        // `format` is a compile-time intrinsic, not an overloadable operator
        // (no runtime symbol, cross-argument check) — it cannot be redefined.
        if name == "format" {
            self.error(
                self.token_span(&name_tok),
                "T0060",
                format!("`{name}` is a built-in intrinsic and cannot be overloaded"),
            );
            return;
        }

        // A user `oper` may *extend* a built-in name with a distinct heading
        // (e.g. `to_text { self: Sequence Text }` alongside the built-in
        // `to_text { self: Text }`). Reject only a true duplicate: a heading
        // that exactly matches an existing built-in overload of this name, or
        // a second user overload of this name (only one user overload per name
        // is supported until linkage-name mangling lands).
        if self
            .builtins
            .candidates(&name)
            .iter()
            .any(|sig| Self::same_heading(&sig.params, &params))
        {
            self.error(
                self.token_span(&name_tok),
                "T0060",
                format!("operator `{name}` with this heading is already defined by a built-in"),
            );
            return;
        }
        if self.user_opers.contains_key(&name) {
            self.error(
                self.token_span(&name_tok),
                "T0060",
                format!(
                    "operator `{name}` already has a user-defined overload (only one user overload per operator name is supported for now)"
                ),
            );
            return;
        }

        let return_type = match decl.return_type() {
            Some(tr) => resolve_type_ref_quiet(&tr),
            None => Type::unit(),
        };

        self.user_opers.insert(
            name,
            OperSig {
                params,
                return_type,
                purity: Purity::SideEffecting,
            },
        );
    }

    /// Exact heading equality — same set of `(param name, ParamKind)` pairs,
    /// order-independent (headings are unordered). Used to decide whether a
    /// user `oper` is a true redefinition of an existing overload (vs. a
    /// distinct-heading extension). Param names are unique within a heading,
    /// so a same-length all-match comparison is exact.
    fn same_heading(
        a: &[(Cow<'static, str>, ParamKind)],
        b: &[(Cow<'static, str>, ParamKind)],
    ) -> bool {
        a.len() == b.len()
            && a.iter()
                .all(|(an, ak)| b.iter().any(|(bn, bk)| an == bn && ak == bk))
    }

    fn check_oper_decl(&mut self, decl: &OperDecl) {
        // A `builtin` declaration carries no body to check — the compiler
        // provides the implementation (see register_user_oper).
        if decl.is_builtin() {
            return;
        }
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
            if matches!(
                info.kind,
                RelvarKind::Public | RelvarKind::Private | RelvarKind::Builtin
            ) {
                let ty = Type::Relation(info.heading.clone());
                scope.insert(name.to_string(), ty, Span::default(), BindingOrigin::Relvar);
                // Only `public` relvars carry the transaction gate (T0025); a
                // `builtin` relvar is FFI-backed, not a persistent DB relvar.
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

                let ty = match param.type_ref() {
                    Some(tr) => self.resolve_type_ref(&tr),
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
        let return_type = match decl.return_type() {
            Some(tr) => self.resolve_type_ref(&tr),
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

        // The body's result type must match the declared return. The tail is
        // checked *against* the return type (bidirectional), so an empty
        // relation / nested tuple in the returned value infers from it.
        if let Some(body) = decl.body() {
            let body_ty = self.check_block_expected(&body, &mut scope, &return_type);
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
        if let Some(t) = Type::from_builtin_name(name) {
            return t;
        }
        // A user-defined `type Name = …;` alias, or an imported opt-in stdlib
        // alias (both registered in the pre-pass).
        if let Some(t) = self.type_aliases.get(name) {
            return t.clone();
        }
        // A user-defined possrep scalar (`type Name { c: T };`) — a distinct
        // nominal type. Its component lives in `nominal_scalars`; here we only
        // need the name to form the nominal `Type::Scalar`.
        if self.nominal_scalars.contains_key(name) {
            return Type::Scalar(name.to_string());
        }
        // Not in scope. If it's an opt-in stdlib type, point at the import
        // rather than reporting a plain unknown-type.
        if let Some(module) = self.stdlib_type_owner.get(name).cloned() {
            self.error(
                self.token_span(token),
                "T0088",
                format!("type `{name}` requires `use module {module};`"),
            );
            return Type::Unknown;
        }
        self.error(
            self.token_span(token),
            "T0005",
            format!("unknown type `{name}`"),
        );
        Type::Unknown
    }

    /// Resolve a (possibly generator-applied) `TypeRef` to a `Type`.
    /// `Sequence T` recurses into the element type-ref; any other head is
    /// a leaf name resolved through [`Self::resolve_type_name`] (which
    /// emits T0005 on an unknown name). A `Sequence` with no element
    /// type-ref (parser already emitted P0011) yields `Sequence Unknown`
    /// rather than cascading.
    fn resolve_type_ref(&mut self, tr: &TypeRef) -> Type {
        let Some(name_tok) = tr.name() else {
            return Type::Unknown;
        };
        if name_tok.text() == "Sequence" {
            let elem = match tr.element() {
                Some(e) => self.resolve_type_ref(&e),
                None => Type::Unknown,
            };
            return Type::Sequence(Box::new(elem));
        }
        // The heading generators `Relation { H }` / `Tuple { H }` take a nested
        // HEADING; a bare `Relation`/`Tuple` (no heading) falls through to the
        // leaf path → T0005, as before.
        if name_tok.text() == "Relation" {
            if let Some(h) = tr.heading() {
                return Type::Relation(self.resolve_heading(&h));
            }
        }
        if name_tok.text() == "Tuple" {
            if let Some(h) = tr.heading() {
                return Type::Tuple(self.resolve_heading(&h));
            }
        }
        self.resolve_type_name(&name_tok)
    }


    fn check_stmt(&mut self, stmt: &Stmt, scope: &mut Scope) {
        match stmt {
            Stmt::Let(l) => self.check_let_stmt(l, scope),
            Stmt::Var(v) => self.check_var_stmt(v, scope),
            Stmt::Assign(a) => self.check_assignment_stmt(a, scope),
            Stmt::Truncate(t) => self.check_truncate_stmt(t, scope),
            Stmt::Delete(d) => self.check_delete_stmt(d, scope),
            Stmt::Insert(i) => self.check_insert_stmt(i, scope),
            Stmt::Update(u) => self.check_update_stmt(u, scope),
            Stmt::ExprStmt(e) => self.check_expr_stmt(e, scope),
            Stmt::For(f) => self.check_for_stmt(f, scope),
            Stmt::While(w) => self.check_while_stmt(w, scope),
            Stmt::DoWhile(d) => self.check_do_while_stmt(d, scope),
            Stmt::Load(l) => self.check_load_stmt(l, scope),
        }
    }

    fn check_block(&mut self, block: &Block, scope: &mut Scope) -> Type {
        for stmt in block.statements() {
            self.check_stmt(&stmt, scope);
        }
        match block.tail_expr() {
            Some(expr) => self.check_expr(&expr, scope),
            None => Type::unit(),
        }
    }

    /// Check a `for` loop — counted (`for i := lo to hi`) or element
    /// (`for name in seq`). The loop variable is bound loop-scoped and
    /// immutable (assigning it is T0072, in `check_assignment_stmt`), and is
    /// exempt from the unused-binding warning. The counted `to` is inclusive
    /// (`lo > hi` runs zero times, no diagnostic); `for … in` iterates a
    /// `Sequence` element-wise.
    fn check_for_stmt(&mut self, stmt: &ForStmt, scope: &mut Scope) {
        // The header operands are full expressions in the *enclosing* scope
        // (they can't reference the loop variable). Check them before binding
        // it, so the variable never leaks into a header and header diagnostics
        // surface even if the body is empty. The loop variable's type follows
        // the form.
        let var_ty = if stmt.is_for_in() {
            // Element form: the operand must be a `Sequence T`; the variable
            // takes the element type `T`. A `Relation` (or scalar) operand is
            // the RM Pro 7 boundary — point at `load … order`.
            match stmt.iterable() {
                Some(expr) => match self.check_expr(&expr, scope) {
                    Type::Sequence(elem) => *elem,
                    // A bare sequence literal already produced T0063, and parse
                    // recovery yields Unknown — no second diagnostic either way.
                    Type::Unknown => Type::Unknown,
                    other => {
                        self.error(
                            self.node_span(expr.syntax()),
                            "T0073",
                            format!(
                                "`for … in` requires a Sequence, but the operand has type \
                                 {other}; materialize a relation into an ordered Sequence with \
                                 `load … order`"
                            ),
                        );
                        Type::Unknown
                    }
                },
                None => Type::Unknown,
            }
        } else {
            // Counted form: both bounds must be Integer; the counter is Integer.
            for (bound, which) in [(stmt.lower_bound(), "lower"), (stmt.upper_bound(), "upper")] {
                if let Some(expr) = bound {
                    let ty = self.check_expr(&expr, scope);
                    if !matches!(ty, Type::Integer | Type::Unknown) {
                        self.error(
                            self.node_span(expr.syntax()),
                            "T0071",
                            format!("`for` loop {which} bound must be Integer, but has type {ty}"),
                        );
                    }
                }
            }
            Type::Integer
        };

        // The loop variable is loop-scoped: push a layer, bind it with the
        // `ForCounter` origin (immutable, exempt from the unused warning), check
        // the body, then pop and report any unused *body* bindings.
        scope.push();
        if let Some(name_tok) = stmt.var_name() {
            scope.insert(
                name_tok.text().to_string(),
                var_ty,
                self.token_span(&name_tok),
                BindingOrigin::ForCounter,
            );
        }
        // Definite assignment: the body may run zero times, so an outer `var`
        // it assigns is not definitely assigned after the loop — snapshot the
        // uninitialized bindings and roll them back once the body is checked.
        let da_snap = scope.uninit_snapshot();
        if let Some(body) = stmt.body() {
            self.check_block(&body, scope);
        }
        scope.restore_uninit(&da_snap);
        let unused = scope.pop();
        self.warn_unused(unused);
    }

    /// Check a `while <cond> do [ … ]` pre-test loop. The condition is `Boolean`
    /// (T0080) and checked in the enclosing scope. The body may run zero times,
    /// so — like `for` — a `var` it assigns is not definitely assigned after the
    /// loop: snapshot the uninitialized bindings and roll them back once the body
    /// is checked. There is no loop variable (the driver is the user's own outer
    /// `var`).
    fn check_while_stmt(&mut self, stmt: &WhileStmt, scope: &mut Scope) {
        self.check_loop_condition(stmt.condition(), stmt.syntax(), "while", scope);
        let da_snap = scope.uninit_snapshot();
        scope.push();
        if let Some(body) = stmt.body() {
            self.check_block(&body, scope);
        }
        scope.restore_uninit(&da_snap);
        let unused = scope.pop();
        self.warn_unused(unused);
    }

    /// Check a `do [ … ] while <cond>` post-test loop. The body runs at least
    /// once, so its *unconditional* assignments to an outer `var` are definitely
    /// assigned afterward (and when the trailing condition reads them) — no
    /// snapshot/rollback (contrast `while`/`for`). The body is checked first, in
    /// its own scope; the condition (`Boolean`, T0080) is checked afterward in
    /// the enclosing scope — body-locals are scoped to the `[ … ]` and never
    /// visible to the condition.
    fn check_do_while_stmt(&mut self, stmt: &DoWhileStmt, scope: &mut Scope) {
        scope.push();
        if let Some(body) = stmt.body() {
            self.check_block(&body, scope);
        }
        let unused = scope.pop();
        self.warn_unused(unused);
        self.check_loop_condition(stmt.condition(), stmt.syntax(), "do … while", scope);
    }

    /// Shared loop-condition check: the condition must be `Boolean` (T0080). A
    /// missing condition is parse recovery (no second diagnostic). `keyword`
    /// names the loop form in the message.
    fn check_loop_condition(
        &mut self,
        cond: Option<Expr>,
        stmt_node: &SyntaxNode,
        keyword: &str,
        scope: &mut Scope,
    ) {
        let cond_ty = match &cond {
            Some(c) => self.check_expr(c, scope),
            None => return,
        };
        if !matches!(cond_ty, Type::Boolean | Type::Unknown) {
            let span = cond
                .as_ref()
                .map(|c| self.node_span(c.syntax()))
                .unwrap_or_else(|| self.node_span(stmt_node));
            self.error(
                span,
                "T0080",
                format!("`{keyword}` condition must be Boolean, but has type {cond_ty}"),
            );
        }
    }

    /// Check `load <target> from <relExpr> [ order [ <sort-item>… ] ];` — the
    /// relation→`Sequence` iteration gate (RM Pro 7). The source must be a
    /// `Relation H` (T0081); the materialized target is `Sequence Tuple H`, one
    /// ordered tuple element per source tuple. Each `order` key must name an
    /// attribute of `H` (T0027) and be scalar — a relation- or tuple-valued key
    /// carries `=`/`<>` only (RM Pro 1), so it has no sort order (T0082). The
    /// target is a pre-declared `var` (there is no expression form of `load`, so
    /// the deferred-init `var names;` is the only legal target): an unannotated
    /// one is inferred here and an annotated one is checked (T0075), then marked
    /// definitely assigned — the same path a first `x := …` assignment takes.
    fn check_load_stmt(&mut self, stmt: &LoadStmt, scope: &mut Scope) {
        // Check the source first so its own diagnostics surface regardless of
        // the target's validity (mirrors `check_assignment_stmt`).
        let source_ty = match stmt.source() {
            Some(e) => self.check_expr(&e, scope),
            None => return, // parser recovery already emitted a diagnostic
        };

        // A `Sequence` source is the reverse form: seal the sequence's element
        // tuples back into a relvar as a set (RM Pro 1, 3). The source type — not
        // the target — carries the direction, so dispatch on it; the forward form
        // (`Relation` source → ordered `Sequence`) continues below.
        if let Type::Sequence(elem) = &source_ty {
            let elem = elem.as_ref().clone();
            self.check_load_reverse(stmt, elem, scope);
            return;
        }

        // The `Sequence` holds one tuple element per source tuple, so its element
        // type is `Tuple H`. A non-relation source is T0081; `Unknown` stays
        // `Unknown` so a single failure doesn't poison the target's binding.
        let inferred = match &source_ty {
            Type::Relation(heading) => {
                for item in stmt.sort_items() {
                    let Some(tok) = item.attr() else { continue };
                    let key = tok.text();
                    match heading.lookup(key) {
                        None => self.error(
                            self.token_span(&tok),
                            "T0027",
                            format!("unknown attribute `{key}` in `order` of {heading}"),
                        ),
                        // Only scalars have an order; a relation- or tuple-valued
                        // attribute carries `=`/`<>` only (RM Pro 1).
                        Some(Type::Relation(_) | Type::Tuple(_)) => self.error(
                            self.token_span(&tok),
                            "T0082",
                            format!("cannot order by `{key}`: only scalar attributes have an order"),
                        ),
                        Some(_) => {}
                    }
                }
                Type::Sequence(Box::new(Type::Tuple(heading.clone())))
            }
            Type::Unknown => Type::Unknown,
            other => {
                let span = stmt
                    .source()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(stmt.syntax()));
                self.error(
                    span,
                    "T0081",
                    format!("`load` source must be a Relation, but has type {other}"),
                );
                Type::Unknown
            }
        };

        // Bind the target — the deferred-init `var` (annotated or not).
        let Some(ident) = stmt.target() else { return };
        let name = ident.text();
        match scope.origin(name) {
            Some(BindingOrigin::ForCounter) => self.error(
                self.token_span(&ident),
                "T0072",
                format!("`{name}` is a loop counter and cannot be a `load` target"),
            ),
            Some(BindingOrigin::Var) => {
                // A write is a mutable occurrence (LSP marking) and counts as the
                // `var`'s reassignment, so a deferred-init `var` filled only by
                // `load` isn't flagged as a `let` (T0077) — the same bookkeeping
                // a first `x := …` does.
                self.mutable_spans.push(self.token_span(&ident));
                scope.mark_reassigned(name);
                match scope.lookup(name).cloned() {
                    // An unannotated `var names;` has an `Unknown` type until the
                    // `load` infers it; surface the result as an inlay hint at the
                    // declaration (like `let x = …`).
                    Some(Type::Unknown) => {
                        scope.set_type(name, inferred.clone());
                        if !matches!(inferred, Type::Unknown) {
                            if let Some(decl_span) = scope.binding_span(name) {
                                self.hints.push(TypeHint {
                                    span: Span::new(self.file, decl_span.end, decl_span.end),
                                    ty: inferred.clone(),
                                    kind: HintKind::LetBinding,
                                });
                            }
                        }
                    }
                    // An annotated target must accept the loaded sequence type.
                    Some(decl_ty) => {
                        if !inferred.assignable_to(&decl_ty) {
                            self.error(
                                self.token_span(&ident),
                                "T0075",
                                format!("cannot load {inferred} into `{name}`, declared {decl_ty}"),
                            );
                        }
                    }
                    None => {}
                }
                // Definitely assigned from here (the deferred `var`'s init).
                scope.mark_initialized(name);
            }
            Some(BindingOrigin::Let) => self.error(
                self.token_span(&ident),
                "T0074",
                format!("`{name}` is an immutable `let` binding and cannot be a `load` target; declare it with `var`"),
            ),
            Some(BindingOrigin::Param) => self.error(
                self.token_span(&ident),
                "T0074",
                format!("`{name}` is a parameter and cannot be a `load` target"),
            ),
            _ => self.error(
                self.token_span(&ident),
                "T0001",
                format!("`{name}` is not a declared `var`; `load` needs a `var` target"),
            ),
        }
    }

    /// Check the reverse `load <relvar> from <sequence>` form: seal the source
    /// sequence's element tuples back into a relvar as a set. The target must be a
    /// **private** (in-memory) relvar whose heading matches the sequence's element
    /// tuple; an `order` clause is rejected (a relation is unordered, T0083), and a
    /// public relvar reverse — a SQL DML replace — is not yet wired (T0084). `elem`
    /// is the sequence's element type.
    fn check_load_reverse(&mut self, stmt: &LoadStmt, elem: Type, scope: &mut Scope) {
        // A relation has no tuple order — an `order` clause on the reverse form is
        // meaningless. Flag the first sort item.
        if let Some(item) = stmt.sort_items().next() {
            self.error(
                self.node_span(item.syntax()),
                "T0083",
                "`order` is not allowed when loading a sequence into a relvar (a relation is unordered)",
            );
        }

        let Some(ident) = stmt.target() else { return };
        let name = ident.text();

        // A local binding (`var`/`let`/parameter/loop counter) named `name` shadows
        // any relvar and is not a valid reverse target — the target is a relvar.
        if let Some(origin) = scope.origin(name) {
            if !matches!(origin, BindingOrigin::Relvar) {
                self.error(
                    self.token_span(&ident),
                    "T0033",
                    format!("cannot load a sequence into `{name}`: not an assignable relvar"),
                );
                return;
            }
        }

        // Resolve the target relvar. A private (in-memory) relvar is the legal
        // reverse target; a public relvar reverse (a SQL replace) is deferred.
        let Some(info) = self.relvars.get(name) else {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot load a sequence into `{name}`: not an assignable relvar"),
            );
            return;
        };
        if matches!(info.kind, RelvarKind::Public) {
            self.error(
                self.token_span(&ident),
                "T0084",
                format!(
                    "loading a sequence into public relvar `{name}` is not yet supported; \
                     use a private relvar"
                ),
            );
            return;
        }
        if !matches!(info.kind, RelvarKind::Private) {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot load a sequence into `{name}`: not an assignable relvar"),
            );
            return;
        }
        let target_ty = Type::Relation(info.heading.clone());
        scope.mark_used(name);

        // The sealed relation is `Relation H` when the element is `Tuple H`; it must
        // be assignable to the relvar's declared type. A scalar (non-tuple) element
        // sequence has no relation form and mismatches the same way (T0075). An
        // `Unknown` element (a source error) is not re-flagged.
        if matches!(elem, Type::Unknown) {
            return;
        }
        let sealed = match &elem {
            Type::Tuple(h) => Type::Relation(h.clone()),
            _ => Type::Sequence(Box::new(elem.clone())),
        };
        if !sealed.assignable_to(&target_ty) {
            let source_ty = Type::Sequence(Box::new(elem));
            self.error(
                self.token_span(&ident),
                "T0075",
                format!("cannot load {source_ty} into `{name}`, declared {target_ty}"),
            );
        }
    }

    /// Check a relational assignment `R := <expr>;`. The target must be a bare
    /// name bound to a public or private relvar; the RHS must be a relation
    /// whose heading matches the relvar's. A **private** target stores into an
    /// in-memory slot; a **public** target is a write to its SQL-backed table —
    /// the RHS shape is recognized and emitted as surgical DML at lowering,
    /// which is where a non-writable view (T0050) or an unsupported RHS shape
    /// (T0086) is caught. A public-relvar reference forces a transaction
    /// (T0025) via the (self-referencing) RHS, checked below.
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
            self.error(span, "T0033", "assignment target must be a relvar name");
            return;
        };
        let Some(ident) = target.ident() else { return };
        let name = ident.text();

        // A loop counter is immutable — assigning it is its own error, before
        // the relvar lookup (the counter is a scope local, never a relvar).
        if matches!(scope.origin(name), Some(BindingOrigin::ForCounter)) {
            self.error(
                self.token_span(&ident),
                "T0072",
                format!("`{name}` is a loop counter and cannot be reassigned"),
            );
            return;
        }

        // A local-binding target: a mutable `var` is reassignable; an
        // immutable `let` or a parameter is not. This precedes the relvar
        // lookup — a local binding shadows any same-named relvar for
        // assignment, and its diagnostics are clearer than "not a relvar".
        // (`Relvar`/`WhereAttr` origins fall through to the relvar path.)
        match scope.origin(name) {
            Some(BindingOrigin::Var) => {
                // A write is a mutable occurrence (LSP marking) but not a
                // read — an only-written `var` still warns unused (T0032).
                self.mutable_spans.push(self.token_span(&ident));
                // Record the reassignment so a never-reassigned `var` can be
                // flagged as a `let` at scope exit (T0077).
                scope.mark_reassigned(name);
                // An unannotated `var x;` has an `Unknown` type until its first
                // assignment infers it; later assignments must match (T0075).
                match scope.lookup(name).cloned() {
                    Some(Type::Unknown) => {
                        scope.set_type(name, rhs_ty.clone());
                        // Surface the inferred type as an inlay hint at the
                        // declaration, so an unannotated `var x;` shows `: T`
                        // once the first assignment fixes it (like `let x = …`).
                        if let Some(decl_span) = scope.binding_span(name) {
                            self.hints.push(TypeHint {
                                span: Span::new(self.file, decl_span.end, decl_span.end),
                                ty: rhs_ty.clone(),
                                kind: HintKind::LetBinding,
                            });
                        }
                    }
                    Some(decl_ty) => {
                        if !rhs_ty.assignable_to(&decl_ty) {
                            let span = stmt
                                .value()
                                .map(|v| self.node_span(v.syntax()))
                                .unwrap_or_else(|| self.token_span(&ident));
                            self.error(
                                span,
                                "T0075",
                                format!("cannot assign {rhs_ty} to `{name}`, declared {decl_ty}"),
                            );
                        }
                    }
                    None => {}
                }
                // The var is now definitely assigned at this program point
                // (the initialization of a deferred `var x;`, or a reassignment).
                scope.mark_initialized(name);
                return;
            }
            Some(origin @ (BindingOrigin::Let | BindingOrigin::Param)) => {
                let what = if origin == BindingOrigin::Param {
                    "a parameter"
                } else {
                    "an immutable `let` binding"
                };
                let hint = if origin == BindingOrigin::Param {
                    ""
                } else {
                    "; declare it with `var` to allow reassignment"
                };
                self.error(
                    self.token_span(&ident),
                    "T0074",
                    format!("`{name}` is {what} and cannot be reassigned{hint}"),
                );
                return;
            }
            _ => {}
        }

        // … bound to an assignable relvar (public or private).
        let lookup = self.relvars.get(name).and_then(|i| {
            matches!(i.kind, RelvarKind::Public | RelvarKind::Private)
                .then(|| i.heading.clone())
        });
        let Some(heading) = lookup else {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot assign to `{name}`: not an assignable relvar"),
            );
            return;
        };
        scope.mark_used(name);

        // `R := R` is dead code — it never does anything (it's elided at
        // lowering). Warn so the redundancy is reported rather than vanishing.
        if let Some(Expr::NameRef(v)) = stmt.value() {
            if v.ident().is_some_and(|t| t.text() == name) {
                self.warn(
                    self.node_span(v.syntax()),
                    "T0051",
                    format!("assignment of `{name}` to itself has no effect"),
                );
            }
        }

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

    /// Check `truncate R;` — clear every tuple from a relvar. It desugars to
    /// `R := R minus R`, so the operand must be a bare name bound to an
    /// assignable relvar (public or private); a restricted or compound operand
    /// is a different operation (`R where p` → delete) and is rejected (T0033).
    /// A **public** relvar is a write to its SQL table, so it requires a
    /// transaction (T0025), exactly as the desugared self-reference would.
    fn check_truncate_stmt(&mut self, stmt: &TruncateStmt, scope: &mut Scope) {
        // The operand must be a bare name reference — truncate clears the whole
        // relvar; a `where`-restriction or any compound expression isn't a relvar.
        let Some(Expr::NameRef(target)) = stmt.operand() else {
            let span = stmt
                .operand()
                .map(|o| self.node_span(o.syntax()))
                .unwrap_or_else(|| self.node_span(stmt.syntax()));
            self.error(span, "T0033", "truncate operand must be a relvar name");
            return;
        };
        let Some(ident) = target.ident() else { return };
        let name = ident.text();

        // … bound to an assignable relvar (public or private).
        let assignable = self
            .relvars
            .get(name)
            .is_some_and(|i| matches!(i.kind, RelvarKind::Public | RelvarKind::Private));
        if !assignable {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot truncate `{name}`: not an assignable relvar"),
            );
            return;
        }
        scope.mark_used(name);

        // A public relvar is written only inside a `transaction [...]` block
        // (T0025), the same rule the desugared `R := R minus R` self-reference
        // would enforce.
        if self.public_relvars.contains(name) && self.transaction_depth == 0 {
            self.error(
                self.token_span(&ident),
                "T0025",
                format!("public relvar `{name}` referenced outside any `transaction [...]` block"),
            );
        }
    }

    /// Check `delete R where p;` — remove the matching tuples. It desugars to
    /// `R := R minus (R where p)`, so the operand must be a `where`-restriction
    /// over a bare assignable relvar. A bare `delete R;` would clear the whole
    /// relvar — that's `truncate`, so it's rejected (T0052). The predicate is
    /// validated (Boolean + heading scope) and the transaction requirement
    /// (T0025) enforced by checking the `where`-operand once the structure is
    /// known good.
    fn check_delete_stmt(&mut self, stmt: &DeleteStmt, scope: &mut Scope) {
        let Some(operand) = stmt.operand() else { return };

        // The operand must be a `where`-restriction `R where p`.
        let where_bin = match &operand {
            Expr::Binary(bin) if matches!(bin.op_kind(), Some(BinaryOp::Where)) => bin,
            Expr::NameRef(_) => {
                self.error(
                    self.node_span(operand.syntax()),
                    "T0052",
                    "`delete` requires a `where` clause; use `truncate` to clear the whole relvar",
                );
                return;
            }
            _ => {
                self.error(
                    self.node_span(operand.syntax()),
                    "T0033",
                    "delete operand must be a relvar restricted by `where`",
                );
                return;
            }
        };

        // The restricted relation (the `where` lhs) must be a bare relvar name …
        let Some(Expr::NameRef(target)) = where_bin.lhs() else {
            let span = where_bin
                .lhs()
                .map(|l| self.node_span(l.syntax()))
                .unwrap_or_else(|| self.node_span(operand.syntax()));
            self.error(span, "T0033", "delete target must be a relvar name");
            return;
        };
        let Some(ident) = target.ident() else { return };
        let name = ident.text();

        // … bound to an assignable relvar (public or private).
        let assignable = self
            .relvars
            .get(name)
            .is_some_and(|i| {
                matches!(
                    i.kind,
                    RelvarKind::Public | RelvarKind::Private | RelvarKind::Builtin
                )
            });
        if !assignable {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot delete from `{name}`: not an assignable relvar"),
            );
            return;
        }

        // Structure is valid: typecheck the `where`-operand to validate the
        // predicate (Boolean, attribute scope via the heading injection) and
        // force a transaction for a public relvar (T0025) — exactly as the
        // desugared `R := R minus (R where p)` self-reference would.
        let _ = self.check_expr(&operand, scope);
    }

    /// Check `insert R <source>;` — add tuples. It desugars to `R := R union
    /// <source>`, so the source must be a relation whose heading matches the
    /// target relvar's (T0034), and the target a bare assignable relvar (T0033).
    /// A public relvar requires a transaction (T0025). The `source` is a single
    /// relation expression regardless of surface form (the tuple-set is a
    /// keyword-less relation literal), so one `check_expr` validates both.
    fn check_insert_stmt(&mut self, stmt: &InsertStmt, scope: &mut Scope) {
        // Check the source first so its own diagnostics surface regardless of
        // the target's validity (mirrors `check_assignment_stmt`).
        let source_ty = match stmt.source() {
            Some(s) => self.check_expr(&s, scope),
            None => return, // parser recovery already emitted a diagnostic
        };

        // The target must be a bare name reference …
        let Some(Expr::NameRef(target)) = stmt.target() else {
            let span = stmt
                .target()
                .map(|t| self.node_span(t.syntax()))
                .unwrap_or_else(|| self.node_span(stmt.syntax()));
            self.error(span, "T0033", "insert target must be a relvar name");
            return;
        };
        let Some(ident) = target.ident() else { return };
        let name = ident.text();

        // … bound to an assignable relvar (public or private).
        let lookup = self.relvars.get(name).and_then(|i| {
            matches!(
                i.kind,
                RelvarKind::Public | RelvarKind::Private | RelvarKind::Builtin
            )
            .then(|| i.heading.clone())
        });
        let Some(heading) = lookup else {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot insert into `{name}`: not an assignable relvar"),
            );
            return;
        };
        scope.mark_used(name);

        // A public relvar is written only inside a `transaction [...]` block
        // (T0025) — the desugared `R union source` references `R`.
        if self.public_relvars.contains(name) && self.transaction_depth == 0 {
            self.error(
                self.token_span(&ident),
                "T0025",
                format!("public relvar `{name}` referenced outside any `transaction [...]` block"),
            );
        }

        // The source heading must match the relvar's (union requires identical
        // headings; assigning the union back keeps the relvar's heading).
        let target_ty = Type::Relation(heading);
        if !source_ty.assignable_to(&target_ty) {
            let span = stmt
                .source()
                .map(|s| self.node_span(s.syntax()))
                .unwrap_or_else(|| self.token_span(&ident));
            self.error(
                span,
                "T0034",
                format!("cannot insert {source_ty} into relvar `{name}` (heading mismatch)"),
            );
        }
    }

    /// Check `update R where p { c: e };` — overwrite named attributes of the
    /// matching tuples. It desugars to `R := (R where ¬p) union ((R where p)
    /// «sub»)`, so the operand must be relvar-rooted (a bare relvar, or
    /// `R where p`) over a bare assignable relvar (T0033). Unlike `replace`, the
    /// `{ c: e }` values may be constants or bare references (T0042/T0047 are
    /// *not* applied); but each target must be an **existing** attribute (T0053)
    /// whose type the value matches (T0034), and no target is named twice
    /// (T0031). A public relvar requires a transaction (T0025), the predicate
    /// must be Boolean (T0020) — both via the operand's own `check_expr`.
    fn check_update_stmt(&mut self, stmt: &UpdateStmt, scope: &mut Scope) {
        let Some(operand) = stmt.operand() else { return };

        // The operand must be relvar-rooted: a bare relvar `R` (update-all) or
        // a restriction `R where p`. Extract the root relvar name.
        let root = match &operand {
            Expr::NameRef(n) => Some(n.clone()),
            Expr::Binary(b) if matches!(b.op_kind(), Some(BinaryOp::Where)) => match b.lhs() {
                Some(Expr::NameRef(n)) => Some(n),
                _ => None,
            },
            _ => None,
        };
        let Some(target) = root else {
            self.error(
                self.node_span(operand.syntax()),
                "T0033",
                "update operand must be a relvar, optionally restricted by `where`",
            );
            return;
        };
        let Some(ident) = target.ident() else { return };
        let name = ident.text();

        // … bound to an assignable relvar (public or private).
        let lookup = self.relvars.get(name).and_then(|i| {
            matches!(
                i.kind,
                RelvarKind::Public | RelvarKind::Private | RelvarKind::Builtin
            )
            .then(|| i.heading.clone())
        });
        let Some(heading) = lookup else {
            self.error(
                self.token_span(&ident),
                "T0033",
                format!("cannot update `{name}`: not an assignable relvar"),
            );
            return;
        };

        // Typecheck the operand — validates the predicate (Boolean T0020, heading
        // scope-injected) and forces a transaction for a public relvar (T0025),
        // exactly as the desugared `R where p` self-reference would.
        let _ = self.check_expr(&operand, scope);

        // The `{ c: e }` clause: inject the relvar's attributes so each value
        // resolves against the heading first (the same scope rule as `replace`).
        scope.push();
        for (n, ty) in heading.attrs() {
            scope.insert(n.clone(), ty.clone(), Span::default(), BindingOrigin::WhereAttr);
        }
        let mut seen: HashSet<String> = HashSet::new();
        for (name_tok, value) in stmt.pairs() {
            let Some(name_tok) = name_tok else { continue }; // parse recovery
            let attr = name_tok.text();
            let Some(value) = value else { continue };
            // The target attribute must already exist — `update` overwrites it
            // (adding a new attribute is `extend`; relabelling is `rename`).
            let Some(target_ty) = heading.lookup(attr).cloned() else {
                self.error(
                    self.token_span(&name_tok),
                    "T0053",
                    format!("update target attribute `{attr}` does not exist in {heading}"),
                );
                continue;
            };
            if !seen.insert(attr.to_string()) {
                self.error(
                    self.token_span(&name_tok),
                    "T0031",
                    format!("update assigns attribute `{attr}` more than once"),
                );
                continue;
            }
            // The value must match the target's type (a mismatch would make the
            // desugared union/assignment a heading mismatch).
            let vty = self.check_expr(&value, scope);
            if !vty.assignable_to(&target_ty) {
                self.error(
                    self.node_span(value.syntax()),
                    "T0034",
                    format!(
                        "cannot update attribute `{attr}`: value type {vty} does not match {target_ty}"
                    ),
                );
            }
        }
        scope.pop();
    }

    fn check_let_stmt(&mut self, stmt: &LetStmt, scope: &mut Scope) {
        self.check_binding(
            stmt.name(),
            stmt.type_ref(),
            stmt.value(),
            stmt.syntax(),
            BindingOrigin::Let,
            "let",
            scope,
        );
    }

    fn check_var_stmt(&mut self, stmt: &VarStmt, scope: &mut Scope) {
        self.check_binding(
            stmt.name(),
            stmt.type_ref(),
            stmt.value(),
            stmt.syntax(),
            BindingOrigin::Var,
            "var",
            scope,
        );
    }

    /// Shared body of `let`/`var` binding checks. `origin` distinguishes the
    /// immutable `let` from the mutable `var`; `kw` labels the two in
    /// diagnostics. For a `var`, the declaration's name span is recorded as a
    /// mutable occurrence (LSP mutability marking).
    #[allow(clippy::too_many_arguments)]
    fn check_binding(
        &mut self,
        name: Option<SyntaxToken>,
        type_ref: Option<TypeRef>,
        value: Option<Expr>,
        stmt_syntax: &SyntaxNode,
        origin: BindingOrigin,
        kw: &str,
        scope: &mut Scope,
    ) {
        // Resolve the optional annotation first: it's authoritative, and
        // for a `Sequence [ … ]` RHS its element type is the inference
        // context an empty literal falls back on.
        let declared = type_ref.map(|tr| self.resolve_type_ref(&tr));

        // Uninitialized declaration: `let/var x [: T];` with no `:= …`.
        if value.is_none() {
            if origin == BindingOrigin::Let {
                // An immutable binding that is never assigned is meaningless —
                // a `let` can't be reassigned later either (that's T0074).
                self.error(
                    self.node_span(stmt_syntax),
                    "T0078",
                    "an immutable `let` binding must be initialized; use `var` for a later-assigned local",
                );
            }
            // An unannotated `var x;` starts with an unknown type — inferred
            // from its first assignment; an annotation fixes it up front.
            let bound_ty = declared.unwrap_or(Type::Unknown);
            if let Some(name_tok) = &name {
                if origin == BindingOrigin::Var {
                    self.mutable_spans.push(self.token_span(name_tok));
                }
                let n = name_tok.text().to_string();
                scope.insert(n.clone(), bound_ty, self.token_span(name_tok), origin);
                // A valid uninitialized `var` is not yet assigned (definite-
                // assignment, T0079, tracks it). The `let` errored above; leave
                // it "initialized" so it doesn't also trip read-before-assign.
                if origin == BindingOrigin::Var {
                    scope.mark_uninitialized(&n);
                }
            }
            return;
        }

        // `let x = f"…"` binds a reusable format template. Intercept it before
        // the RHS reaches `check_expr` (which rejects any `f"…"` outside
        // `format`'s template with T0055). The template is parsed once here and
        // rides on the binding, so each later `format { template: x, … }` use
        // validates its own `args` against the same chunks. Only a direct
        // literal on an unannotated `let` qualifies — the provenance stays a
        // compile-time literal, never a runtime `Text` (the FormatText firewall).
        if origin == BindingOrigin::Let && declared.is_none() {
            if let Some(Expr::Literal(lit)) = &value {
                if lit.token().map(|t| t.kind()) == Some(SyntaxKind::FORMAT_STRING_LIT) {
                    if let (Some(tok), Some(name_tok)) = (lit.token(), &name) {
                        let chunks = self.parse_template_tok(&tok).map(Rc::new);
                        let n = name_tok.text().to_string();
                        scope.insert(
                            n.clone(),
                            Type::FormatText,
                            self.token_span(name_tok),
                            origin,
                        );
                        scope.attach_format_template(&n, chunks);
                    }
                    return;
                }
            }
        }

        // Infer the RHS type. A sequence literal is checked specially so
        // it can take its element type from `declared` when empty and so
        // it is *permitted* here — `check_expr` rejects sequence literals
        // in every other position (T0063, the binding-value-only rule).
        // Missing name or value means the parser already reported the
        // recovery; we still walk what's parseable to keep diagnostics
        // flowing.
        let rhs_ty = match &value {
            Some(Expr::SequenceLit(s)) => {
                let expected_elem = match &declared {
                    Some(Type::Sequence(e)) => Some((**e).clone()),
                    _ => None,
                };
                self.check_sequence_lit(s, scope, expected_elem)
            }
            Some(Expr::RelationLit(r)) => {
                // An empty `Relation {}` takes its heading from a `Relation { H }`
                // annotation (a headed empty relation); with no annotation it is
                // relfalse. A non-empty literal ignores the expected heading and
                // infers from its tuples (the annotation conformance is checked
                // below, T0010).
                let expected_heading = match &declared {
                    Some(Type::Relation(h)) => Some(h.clone()),
                    _ => None,
                };
                self.check_relation_lit(r, scope, expected_heading)
            }
            // A tuple literal bound with a `Tuple` annotation propagates the
            // annotation's field types (so an empty relation field infers).
            Some(Expr::TupleLit(t)) if matches!(&declared, Some(Type::Tuple(_))) => {
                let Some(Type::Tuple(h)) = &declared else {
                    unreachable!("guarded by the match arm")
                };
                self.check_tuple_lit_expected(t, scope, &h.clone())
            }
            Some(v) => self.check_expr(v, scope),
            None => Type::Unknown,
        };

        // If the binding carries an explicit annotation, the
        // annotation is authoritative: the RHS must conform, and
        // subsequent lookups see the declared type, not the inferred
        // one. Otherwise the inferred type is bound *and* surfaced as
        // an inlay hint — that's what the editor renders.
        let bound_ty = match declared {
            Some(declared) => {
                if !rhs_ty.assignable_to(&declared) {
                    let span = value
                        .as_ref()
                        .map(|v| self.node_span(v.syntax()))
                        .unwrap_or_else(|| self.node_span(stmt_syntax));
                    self.error(
                        span,
                        "T0010",
                        format!(
                            "{kw} binding declared {declared}, but expression produces {rhs_ty}"
                        ),
                    );
                }
                declared
            }
            None => {
                if let Some(name_tok) = &name {
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

        if let Some(name_tok) = &name {
            // A `var` declaration is itself a mutable occurrence — mark it so
            // the editor underlines the binding site, not just its uses.
            if origin == BindingOrigin::Var {
                self.mutable_spans.push(self.token_span(name_tok));
            }
            scope.insert(
                name_tok.text().to_string(),
                bound_ty,
                self.token_span(name_tok),
                origin,
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
                    // A read of a mutable `var` is a mutable occurrence — the
                    // editor marks every use, not just the declaration.
                    if scope.origin(name) == Some(BindingOrigin::Var) {
                        self.mutable_spans.push(self.token_span(&ident));
                    }
                    // Definite assignment: reading a `var` declared without a
                    // value before it has been assigned is an error.
                    if !scope.is_initialized(name) {
                        self.error(
                            self.token_span(&ident),
                            "T0079",
                            format!("`{name}` may be read before it is assigned"),
                        );
                    }
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
                    // A `let`-bound `f"…"` template used anywhere but `format`'s
                    // `template` argument — that legitimate use is intercepted in
                    // `check_format_call` before reaching the generic walk. Same
                    // firewall as a stray `f"…"` literal: recover as `Unknown` so
                    // a redundant type-mismatch doesn't pile on.
                    if matches!(ty, Type::FormatText) {
                        self.error(
                            self.token_span(&ident),
                            "T0055",
                            "an f\"…\" format string is only allowed as the `template` argument of `format`",
                        );
                        return Type::Unknown;
                    }
                    return ty;
                }
                // Not in scope. If it's an opt-in stdlib builtin relvar, point
                // at the import rather than reporting a plain unresolved name.
                if let Some(module) = self.stdlib_relvar_owner.get(name).cloned() {
                    self.error(
                        self.token_span(&ident),
                        "T0090",
                        format!("builtin relvar `{name}` requires `use module {module};`"),
                    );
                    return Type::Unknown;
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
                Some(SyntaxKind::FORMAT_STRING_LIT) => {
                    // The legitimate template is intercepted in
                    // `check_format_call` before it reaches the generic
                    // walk, so any `f"…"` arriving here is misplaced. The
                    // type is still `FormatText` (its only producer is this
                    // literal) — the firewall is that it is unusable
                    // anywhere but `format`'s `template`.
                    if let Some(tok) = lit.token() {
                        self.error(
                            self.token_span(&tok),
                            "T0055",
                            "an f\"…\" format string is only allowed as the `template` argument of `format`",
                        );
                    }
                    Type::FormatText
                }
                _ => Type::Unknown,
            },
            Expr::Call(call) => self.check_call(call, scope),
            Expr::Transaction(t) => self.check_transaction_expr(t, scope),
            Expr::TupleLit(t) => self.check_tuple_lit(t, scope),
            Expr::RelationLit(r) => self.check_relation_lit(r, scope, None),
            Expr::SequenceLit(s) => {
                // A sequence literal is valid only as a `let` binding's
                // value, where `check_let_stmt` intercepts it before this
                // generic walk. Reaching here means it appeared elsewhere
                // (a call argument, nested in an expression, …) — reject.
                self.error(
                    self.node_span(s.syntax()),
                    "T0063",
                    "a sequence literal is only allowed as a `let` binding value",
                );
                Type::Unknown
            }
            Expr::FieldAccess(f) => self.check_field_access(f, scope),
            Expr::BoolLit(_) => Type::Boolean,
            Expr::Binary(b) => self.check_binary_expr(b, scope),
            Expr::Unary(u) => self.check_unary_expr(u, scope),
            Expr::Project(p) => self.check_project_expr(p, scope),
            Expr::Replace(r) => self.check_replace_expr(r, scope),
            Expr::Extend(e) => self.check_extend_expr(e, scope),
            Expr::Tclose(t) => self.check_tclose_expr(t, scope),
            Expr::Rename(r) => self.check_rename_expr(r, scope),
            Expr::Wrap(w) => self.check_wrap_expr(w, scope),
            Expr::Unwrap(u) => self.check_unwrap_expr(u, scope),
            Expr::Index(i) => self.check_index_expr(i, scope),
            Expr::If(i) => self.check_if_expr(i, scope),
        }
    }

    /// Check one `if` arm — an ordered block, scoped like a `transaction`
    /// body so its locals don't leak (but without the transaction-depth bump;
    /// an `if` is not a transaction boundary). An absent block (parse
    /// recovery) is Unit.
    fn check_if_arm(&mut self, block: Option<Block>, scope: &mut Scope) -> Type {
        scope.push();
        let ty = match block {
            Some(b) => self.check_block(&b, scope),
            None => Type::unit(),
        };
        let unused = scope.pop();
        self.warn_unused(unused);
        ty
    }

    /// Walk `if <cond> then [ … ] else [ … ]`. The condition must be `Boolean`
    /// (T0067). With `else`, both arms must share a type (T0068) and that is
    /// the result. Without `else`, the then-arm must be Unit (T0069) and the
    /// result is Unit — the statement form.
    fn check_if_expr(&mut self, ife: &IfExpr, scope: &mut Scope) -> Type {
        let cond_ty = match ife.condition() {
            Some(c) => self.check_expr(&c, scope),
            None => Type::Unknown,
        };
        if !matches!(cond_ty, Type::Boolean | Type::Unknown) {
            let span = ife
                .condition()
                .map(|c| self.node_span(c.syntax()))
                .unwrap_or_else(|| self.node_span(ife.syntax()));
            self.error(
                span,
                "T0067",
                format!("`if` condition must be Boolean, but has type {cond_ty}"),
            );
        }

        // Definite assignment: an arm's assignments to an outer `var` are only
        // conditional. Snapshot the uninitialized bindings, walk each arm from
        // the same pre-`if` state, and (with an `else`) commit as assigned only
        // the vars assigned on *both* paths.
        let da_snap = scope.uninit_snapshot();
        let then_ty = self.check_if_arm(ife.then_body(), scope);
        let then_init = scope.newly_initialized(&da_snap);
        scope.restore_uninit(&da_snap);

        match ife.else_body() {
            Some(else_block) => {
                let else_ty = self.check_if_arm(Some(else_block), scope);
                let else_init = scope.newly_initialized(&da_snap);
                scope.restore_uninit(&da_snap);
                // Assigned on both arms ⇒ definitely assigned after the `if`.
                for handle in &then_init {
                    if else_init.contains(handle) {
                        scope.set_initialized_at(handle.0, handle.1);
                    }
                }
                // Only flag a genuine mismatch — if an arm already errored
                // (Unknown), stay quiet and propagate the concrete side.
                match (&then_ty, &else_ty) {
                    (Type::Unknown, _) => else_ty,
                    (_, Type::Unknown) => then_ty,
                    _ if then_ty == else_ty => then_ty,
                    _ => {
                        self.error(
                            self.node_span(ife.syntax()),
                            "T0068",
                            format!(
                                "`if` arms have mismatched types — then {then_ty}, else {else_ty}"
                            ),
                        );
                        Type::Unknown
                    }
                }
            }
            None => {
                // No `else`: the then-arm may not run, so nothing it assigned is
                // definite afterward (already rolled back by `restore_uninit`).
                if then_ty != Type::unit() && then_ty != Type::Unknown {
                    let span = ife
                        .then_body()
                        .map(|b| self.node_span(b.syntax()))
                        .unwrap_or_else(|| self.node_span(ife.syntax()));
                    self.error(
                        span,
                        "T0069",
                        format!(
                            "an `if` without `else` must have a Unit then-arm, but it has type {then_ty}"
                        ),
                    );
                }
                Type::unit()
            }
        }
    }

    /// Walk `s[i]` — postfix sequence indexing (0-based). The operand must be a
    /// `Sequence T` (T0065 otherwise) and the index must be `Integer` (T0066
    /// otherwise); the result is the element type `T`. Out-of-bounds is a
    /// runtime error, not a type error.
    fn check_index_expr(&mut self, ie: &IndexExpr, scope: &mut Scope) -> Type {
        let seq_ty = match ie.sequence() {
            Some(s) => self.check_expr(&s, scope),
            None => return Type::Unknown,
        };
        // Check the index in all cases so its own errors (unresolved names,
        // etc.) surface even when the operand is already bad.
        let idx_ty = match ie.index() {
            Some(i) => self.check_expr(&i, scope),
            None => return Type::Unknown,
        };
        if !matches!(idx_ty, Type::Integer | Type::Unknown) {
            let span = ie
                .index()
                .map(|i| self.node_span(i.syntax()))
                .unwrap_or_else(|| self.node_span(ie.syntax()));
            self.error(
                span,
                "T0066",
                format!("sequence index must be Integer, but has type {idx_ty}"),
            );
        }
        match seq_ty {
            Type::Unknown => Type::Unknown,
            Type::Sequence(elem) => *elem,
            other => {
                let span = ie
                    .sequence()
                    .map(|s| self.node_span(s.syntax()))
                    .unwrap_or_else(|| self.node_span(ie.syntax()));
                self.error(
                    span,
                    "T0065",
                    format!("indexing requires a Sequence value, but operand has type {other}"),
                );
                Type::Unknown
            }
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

    /// Walk `R wrap { t: { a, b }, … }` — group attributes into tuple-valued
    /// attributes. The operand must be `Relation H` (T0023). Each wrapped attr
    /// must exist in `H` (T0027) and be wrapped at most once across all pairs
    /// (T0028). Each new name must be fresh vs. surviving attributes and other
    /// new names (T0031). Result heading = the attributes not wrapped, plus each
    /// `new : Tuple(<components with their H types>)`.
    fn check_wrap_expr(&mut self, we: &WrapExpr, scope: &mut Scope) -> Type {
        let input_ty = match we.input() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &input_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = we
                    .input()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(we.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`wrap` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Collect each pair's new name + its wrapped components (as a TVA), and
        // the set of all wrapped (consumed) attributes.
        let mut wrapped: HashSet<String> = HashSet::new();
        let mut added: Vec<(String, Type)> = Vec::new();
        for pair in we.pairs() {
            let Some(new_tok) = pair.name() else { continue };
            let new = new_tok.text();
            let mut components: Vec<(String, Type)> = Vec::new();
            for tok in pair.wrapped() {
                let name = tok.text();
                let Some(ty) = heading.lookup(name).cloned() else {
                    self.error(
                        self.token_span(&tok),
                        "T0027",
                        format!("unknown attribute `{name}` in wrap of {heading}"),
                    );
                    continue;
                };
                if !wrapped.insert(name.to_string()) {
                    self.error(
                        self.token_span(&tok),
                        "T0028",
                        format!("attribute `{name}` is wrapped more than once"),
                    );
                    continue;
                }
                components.push((name.to_string(), ty));
            }
            added.push((new.to_string(), Type::Tuple(Heading::new(components))));
        }
        // Result = surviving (non-wrapped) attributes + the new TVAs; a new name
        // colliding with a survivor or another new name is T0031.
        let mut result: Vec<(String, Type)> = Vec::new();
        let mut result_names: HashSet<String> = HashSet::new();
        for (name, ty) in heading.attrs() {
            if wrapped.contains(name) {
                continue;
            }
            result_names.insert(name.clone());
            result.push((name.clone(), ty.clone()));
        }
        for (name, ty) in added {
            if !result_names.insert(name.clone()) {
                self.error(
                    self.node_span(we.syntax()),
                    "T0031",
                    format!("wrap produces a duplicate attribute `{name}`"),
                );
            }
            result.push((name, ty));
        }
        Type::Relation(Heading::new(result))
    }

    /// Walk `R unwrap { t, … }` — expand tuple-valued attributes back to their
    /// components, lifted to top level. The operand must be `Relation H`
    /// (T0023). Each named attr must exist (T0027), be listed once (T0028), and
    /// be `Type::Tuple(_)` (T0048). Result heading = the attributes not unwrapped,
    /// plus each unwrapped tuple's components; a lifted component colliding with
    /// a survivor or another lifted component is T0031.
    fn check_unwrap_expr(&mut self, ue: &UnwrapExpr, scope: &mut Scope) -> Type {
        let input_ty = match ue.input() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &input_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = ue
                    .input()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(ue.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`unwrap` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Each listed name: exists (T0027), unique in the list (T0028), and is a
        // tuple-valued attribute (T0048). Collect the unwrapped set + the lifted
        // components.
        let mut unwrapped: HashSet<String> = HashSet::new();
        let mut lifted: Vec<(String, Type)> = Vec::new();
        for tok in ue.attrs() {
            let name = tok.text();
            let Some(ty) = heading.lookup(name).cloned() else {
                self.error(
                    self.token_span(&tok),
                    "T0027",
                    format!("unknown attribute `{name}` in unwrap of {heading}"),
                );
                continue;
            };
            if !unwrapped.insert(name.to_string()) {
                self.error(
                    self.token_span(&tok),
                    "T0028",
                    format!("duplicate attribute `{name}` in unwrap list"),
                );
                continue;
            }
            match ty {
                Type::Tuple(sub) => {
                    for (cn, ct) in sub.attrs() {
                        lifted.push((cn.clone(), ct.clone()));
                    }
                }
                other => self.error(
                    self.token_span(&tok),
                    "T0048",
                    format!("`unwrap` target `{name}` is not a tuple-valued attribute (got {other})"),
                ),
            }
        }
        // Result = surviving (non-unwrapped) attributes + the lifted components;
        // a collision (component vs survivor or vs another component) is T0031.
        let mut result: Vec<(String, Type)> = Vec::new();
        let mut result_names: HashSet<String> = HashSet::new();
        for (name, ty) in heading.attrs() {
            if unwrapped.contains(name) {
                continue;
            }
            result_names.insert(name.clone());
            result.push((name.clone(), ty.clone()));
        }
        for (name, ty) in lifted {
            if !result_names.insert(name.clone()) {
                self.error(
                    self.node_span(ue.syntax()),
                    "T0031",
                    format!("unwrap produces a duplicate attribute `{name}`"),
                );
            }
            result.push((name, ty));
        }
        Type::Relation(Heading::new(result))
    }

    /// Walk `R replace { new: e, … }` — relational replace: add each `new`
    /// attribute bound to the computed value `e` and remove the operand
    /// attributes `e` references. The operand must be `Relation H` (T0023).
    /// `replace` requires every value to compute; dispatch on each value:
    /// - a bare `NameRef` → a pure relabel, not a computation: that's `rename`
    ///   (T0047).
    /// - a constant (or a general expression that reads no operand attribute):
    ///   it removes nothing → use `extend` (T0042).
    /// - any other (general) expression `e`: typechecked in a scope with `H`'s
    ///   attributes injected (the same rule as `where`/`extend`); its type is
    ///   restricted to Integer or Text (T0046); it adds `new` and removes the
    ///   operand attributes `e` references (the compute-and-consume case,
    ///   desugared through `extend` + `project` + `rename` at lowering).
    ///
    /// The result heading is `(H minus the removed attributes) plus each added
    /// `(new, type)``, canonically re-sorted. A new name colliding with a
    /// surviving attribute or another target is T0031.
    fn check_replace_expr(&mut self, re: &ReplaceExpr, scope: &mut Scope) -> Type {
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
                    format!("`replace` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Inject the operand's attributes so each general value expression
        // resolves against the heading first (the same scope rule as `where`).
        scope.push();
        for (name, ty) in heading.attrs() {
            scope.insert(name.clone(), ty.clone(), Span::default(), BindingOrigin::WhereAttr);
        }
        // Classify each pair into a removed-attribute set and an added
        // `(name, type)`. Every value computes: it adds `new` and removes the
        // operand attributes it reads (a bare-ref relabel is rejected → rename).
        let mut removed: HashSet<String> = HashSet::new();
        let mut added: Vec<(String, Type)> = Vec::new();
        for (new_tok, value) in re.pairs() {
            let Some(new_tok) = new_tok else { continue }; // parse recovery
            let new = new_tok.text();
            let Some(value) = value else { continue };
            let value_span = self.node_span(value.syntax());
            match value {
                // A bare attribute reference only relabels — it computes
                // nothing — so it belongs to `rename`, not `replace`.
                Expr::NameRef(_) => {
                    self.error(
                        value_span,
                        "T0047",
                        format!(
                            "`replace` value for `{new}` is a bare attribute reference, so it only \
                             relabels — use `rename` to relabel an attribute"
                        ),
                    );
                }
                // A constant references no attribute, so it removes nothing —
                // that's `extend`, not `replace`.
                Expr::Literal(_) | Expr::BoolLit(_) => {
                    self.error(
                        value_span,
                        "T0042",
                        format!(
                            "`replace` value for `{new}` references no attribute, so it removes \
                             nothing — use `extend` to add an attribute without removing"
                        ),
                    );
                }
                // General expression → add `new`, remove the attributes it reads.
                other => {
                    let vty = self.check_expr(&other, scope);
                    // The operand attributes the value references (the removed set).
                    let mut refs: HashSet<String> = HashSet::new();
                    attr_refs(&other, &mut refs);
                    refs.retain(|r| heading.lookup(r).is_some());
                    if refs.is_empty() {
                        self.error(
                            value_span,
                            "T0042",
                            format!(
                                "`replace` value for `{new}` references no operand attribute, so it \
                                 removes nothing — use `extend` to add without removing"
                            ),
                        );
                        continue;
                    }
                    if !matches!(vty, Type::Integer | Type::Text | Type::Unknown) {
                        self.error(
                            value_span,
                            "T0046",
                            format!("`replace` value for `{new}` must be Integer or Text, got {vty}"),
                        );
                        continue;
                    }
                    removed.extend(refs);
                    added.push((new.to_string(), vty));
                }
            }
        }
        scope.pop();
        // Result = surviving operand attributes (not removed) plus the added
        // ones; a new name colliding with a survivor or another target is T0031.
        let mut result: Vec<(String, Type)> = Vec::new();
        let mut result_names: HashSet<String> = HashSet::new();
        for (name, ty) in heading.attrs() {
            if removed.contains(name) {
                continue;
            }
            result_names.insert(name.clone());
            result.push((name.clone(), ty.clone()));
        }
        for (name, ty) in added {
            if !result_names.insert(name.clone()) {
                self.error(
                    self.node_span(re.syntax()),
                    "T0031",
                    format!("replace produces a duplicate attribute `{name}`"),
                );
            }
            result.push((name, ty));
        }
        Type::Relation(Heading::new(result))
    }

    /// Walk `R rename { new: old, … }` — relational rename (relabel): replace
    /// each `old` attribute name with `new`, type- and cardinality-preserving.
    /// The strict relabel-only partition of `replace`. The operand must be
    /// `Relation H` (T0023). Each value must be a bare `NameRef` `old` that
    /// exists in `H` (T0029), with no source relabeled twice and no target
    /// collision (T0031); a computed value belongs to `replace` (T0030). The
    /// result heading is `H` with each `old` renamed to `new`, canonically
    /// re-sorted.
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
        // Each pair relabels `old` → `new`: remove `old`, add `new` with the
        // same type. A non-bare-ref value computes → that's `replace` (T0030).
        let mut removed: HashSet<String> = HashSet::new();
        let mut added: Vec<(String, Type)> = Vec::new();
        let mut seen_src: HashSet<String> = HashSet::new();
        for (new_tok, value) in re.pairs() {
            let Some(new_tok) = new_tok else { continue }; // parse recovery
            let new = new_tok.text();
            let Some(value) = value else { continue };
            match value {
                Expr::NameRef(n) => {
                    let Some(old_tok) = n.ident() else { continue };
                    let old = old_tok.text();
                    let Some(ty) = heading.lookup(old).cloned() else {
                        self.error(
                            self.token_span(&old_tok),
                            "T0029",
                            format!("unknown attribute `{old}` in rename of {heading}"),
                        );
                        continue;
                    };
                    if !seen_src.insert(old.to_string()) {
                        self.error(
                            self.token_span(&old_tok),
                            "T0031",
                            format!("attribute `{old}` is renamed more than once"),
                        );
                        continue;
                    }
                    removed.insert(old.to_string());
                    added.push((new.to_string(), ty));
                }
                // Anything other than a bare attribute reference is a
                // computation, not a relabel — that's `replace`, not `rename`.
                other => {
                    self.error(
                        self.node_span(other.syntax()),
                        "T0030",
                        format!(
                            "`rename` value for `{new}` must be a bare attribute reference — \
                             use `replace` for computed values"
                        ),
                    );
                }
            }
        }
        // Result = surviving operand attributes (not relabeled) plus the renamed
        // ones; a new name colliding with a survivor or another target is T0031.
        let mut result: Vec<(String, Type)> = Vec::new();
        let mut result_names: HashSet<String> = HashSet::new();
        for (name, ty) in heading.attrs() {
            if removed.contains(name) {
                continue;
            }
            result_names.insert(name.clone());
            result.push((name.clone(), ty.clone()));
        }
        for (name, ty) in added {
            if !result_names.insert(name.clone()) {
                self.error(
                    self.node_span(re.syntax()),
                    "T0031",
                    format!("rename produces a duplicate attribute `{name}`"),
                );
            }
            result.push((name, ty));
        }
        Type::Relation(Heading::new(result))
    }

    /// Walk `R extend { c: e, … }` — relational extend: add each new attribute
    /// `c` bound to the computed value `e`, keeping every operand attribute.
    /// The operand must be `Relation H` (T0023, shared with `where`/`replace`).
    /// Each value `e` is a general scalar expression typechecked in a scope
    /// with `H`'s attributes injected (the same machinery `where` uses), so it
    /// may reference the operand's attributes. The new name `c` must not
    /// collide with an existing attribute or another `extend` target (T0045).
    /// The result heading is `H` plus each `(c, type_of e)`, canonically
    /// re-sorted.
    fn check_extend_expr(&mut self, ee: &ExtendExpr, scope: &mut Scope) -> Type {
        let input_ty = match ee.input() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &input_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = ee
                    .input()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(ee.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`extend` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Inject the operand's attributes so each value expression resolves
        // against the heading first (the same scope rule as `where`).
        scope.push();
        for (name, ty) in heading.attrs() {
            scope.insert(name.clone(), ty.clone(), Span::default(), BindingOrigin::WhereAttr);
        }
        // Result heading: existing attributes plus each computed one. `seen` is
        // seeded with `H`'s names so a new name colliding with an existing
        // attribute OR a duplicate `extend` target both fire T0045 (without it
        // `Heading::new` would silently dedup and drop a column).
        let mut result: Vec<(String, Type)> = heading.attrs().to_vec();
        let mut seen: HashSet<String> = heading.attrs().iter().map(|(n, _)| n.clone()).collect();
        for (name_tok, value) in ee.pairs() {
            let Some(name_tok) = name_tok else { continue };
            let name = name_tok.text();
            let value_span = value.as_ref().map(|v| self.node_span(v.syntax()));
            let vty = match value {
                Some(v) => self.check_expr(&v, scope),
                None => Type::Unknown,
            };
            if !seen.insert(name.to_string()) {
                self.error(
                    self.token_span(&name_tok),
                    "T0045",
                    format!("`extend` attribute `{name}` already exists in {heading}"),
                );
                continue;
            }
            // v1: only Integer and Text are representable as relation cells (the
            // arithmetic→Integer / concatenation→Text scalars), so an extend
            // value's type is restricted to those — both for the SQL push and
            // the in-process path. Boolean/Character and non-scalar values await
            // wider cell support.
            if !matches!(vty, Type::Integer | Type::Text | Type::Unknown) {
                let span = value_span.unwrap_or_else(|| self.token_span(&name_tok));
                self.error(
                    span,
                    "T0046",
                    format!("`extend` value for `{name}` must be Integer or Text, got {vty}"),
                );
                continue;
            }
            result.push((name.to_string(), vty));
        }
        scope.pop();
        Type::Relation(Heading::new(result))
    }

    /// Walk `R tclose` / `R tclose { a, b }` — relational transitive closure.
    /// The operand must be `Relation H` (T0023, shared with `where`/`project`).
    /// When a brace-list is given it picks two columns first (sugar for
    /// `(R project { a, b }) tclose`): each listed name must exist in `H`
    /// (T0027) and appear at most once (T0028), and the *effective* heading is
    /// `H` narrowed to those names. The effective heading must then be a binary
    /// relation of two **identically-typed** attributes (else T0041) — the
    /// precondition for closing a graph. Direction-agnostic: the result heading
    /// is exactly that effective heading (no from/to; `TC(reverse G) =
    /// reverse(TC G)`).
    fn check_tclose_expr(&mut self, te: &TcloseExpr, scope: &mut Scope) -> Type {
        let input_ty = match te.input() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &input_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = te
                    .input()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(te.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`tclose` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Effective heading: with a brace-list, project onto the listed names
        // (each must exist → T0027, unique → T0028); without, the operand
        // heading itself. The final binary/same-type check (T0041) catches a
        // list that isn't exactly two valid attributes.
        let listed: Vec<SyntaxToken> = te.attrs().collect();
        let effective = if listed.is_empty() {
            heading.clone()
        } else {
            let mut seen: HashSet<String> = HashSet::new();
            let mut picked: HashSet<String> = HashSet::new();
            for tok in &listed {
                let name = tok.text();
                if !seen.insert(name.to_string()) {
                    self.error(
                        self.token_span(tok),
                        "T0028",
                        format!("duplicate attribute `{name}` in tclose list"),
                    );
                    continue;
                }
                match heading.lookup(name) {
                    Some(_) => {
                        picked.insert(name.to_string());
                    }
                    None => self.error(
                        self.token_span(tok),
                        "T0027",
                        format!("unknown attribute `{name}` in tclose of {heading}"),
                    ),
                }
            }
            let kept: Vec<(String, Type)> = heading
                .attrs()
                .iter()
                .filter(|(name, _)| picked.contains(name))
                .cloned()
                .collect();
            Heading::new(kept)
        };
        // Require exactly two attributes of identical type — a binary graph
        // relation. (`Heading::attrs()` is canonically sorted; comparing the
        // two types directly is order-independent.)
        let attrs = effective.attrs();
        if attrs.len() != 2 || attrs[0].1 != attrs[1].1 {
            self.error(
                self.node_span(te.syntax()),
                "T0041",
                "`tclose` operand must be a relation of exactly two attributes \
                 of the same type"
                    .to_string(),
            );
            return Type::Unknown;
        }
        Type::Relation(effective)
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

    /// Check `expr` against an `expected` type, propagating the expected type
    /// into the positions where inference needs it (bidirectional checking): an
    /// empty `Relation {}` / `Sequence []` literal takes its heading/element
    /// from the expectation, and a tuple literal propagates per-field. Every
    /// other shape falls back to bottom-up [`Self::check_expr`]. Used at the
    /// return position and annotated bindings so e.g. a `Response` tuple can
    /// write `{ …, headers: Relation {} }` and the empty relation infers its
    /// `{name, value}` heading.
    fn check_expr_expected(&mut self, expr: &Expr, scope: &mut Scope, expected: &Type) -> Type {
        match (expr, expected) {
            (Expr::RelationLit(r), Type::Relation(h)) => {
                self.check_relation_lit(r, scope, Some(h.clone()))
            }
            (Expr::SequenceLit(s), Type::Sequence(elem)) => {
                self.check_sequence_lit(s, scope, Some((**elem).clone()))
            }
            (Expr::TupleLit(t), Type::Tuple(h)) => self.check_tuple_lit_expected(t, scope, h),
            _ => self.check_expr(expr, scope),
        }
    }

    /// Like [`Self::check_tuple_lit`] but checks each field value against the
    /// expected field type (looked up by name in `expected`), so nested empty
    /// relations / tuples infer from the expectation. A field absent from
    /// `expected` (an extra field) is checked bottom-up; the surplus surfaces as
    /// a heading mismatch (`assignable_to`) at the call/return site.
    fn check_tuple_lit_expected(
        &mut self,
        tup: &TupleLit,
        scope: &mut Scope,
        expected: &Heading,
    ) -> Type {
        let mut seen: HashSet<String> = HashSet::new();
        let mut fields: Vec<(String, Type)> = Vec::new();
        for field in tup.fields() {
            let name_tok = match field.name() {
                Some(t) => t,
                None => continue,
            };
            let name = name_tok.text().to_string();
            let ty = match field.value() {
                Some(v) => match expected.lookup(&name) {
                    Some(exp) => self.check_expr_expected(&v, scope, &exp.clone()),
                    None => self.check_expr(&v, scope),
                },
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

    /// Like [`Self::check_block`] but the tail expression is checked against
    /// `expected` (bidirectional). Statements are unaffected.
    fn check_block_expected(&mut self, block: &Block, scope: &mut Scope, expected: &Type) -> Type {
        for stmt in block.statements() {
            self.check_stmt(&stmt, scope);
        }
        match block.tail_expr() {
            Some(expr) => self.check_expr_expected(&expr, scope, expected),
            None => Type::unit(),
        }
    }

    /// Walk a `Relation { <tuple-lit>, <tuple-lit>, … }` literal. The
    /// first tuple establishes the heading; subsequent tuples must
    /// have the same `(name, type)` set. An empty `Relation {}` is the
    /// nullary empty relation `relfalse` (empty heading, zero tuples —
    /// the zero of the join semiring); its sibling `reltrue` is
    /// `Relation { {} }` (one empty tuple). A heading mismatch emits
    /// T0019 on the offending tuple; the typechecker keeps the first
    /// tuple's heading so downstream checks see a stable type.
    fn check_relation_lit(
        &mut self,
        rel: &RelationLit,
        scope: &mut Scope,
        expected: Option<Heading>,
    ) -> Type {
        let tuples: Vec<TupleLit> = rel.tuples().collect();
        if tuples.is_empty() {
            // Empty `Relation {}`: take the heading from the expected type when
            // there is one (a `let`/`var` annotation → a *headed* empty
            // relation), else default to `relfalse` — the nullary empty relation
            // (∅ heading). Unlike an empty `Sequence []` (T0061), no annotation
            // is *required*: relfalse is a sensible unconstrained default.
            return Type::Relation(expected.unwrap_or_else(Heading::empty));
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

    /// Walk a `Sequence [ e, … ]` literal. The element type is inferred
    /// from the first element; every later element must be assignable to
    /// it (T0062 otherwise). An empty literal has no element to infer
    /// from, so it takes `expected` — the `let` annotation's element type
    /// — when present, else emits T0061. The result is `Sequence T`.
    ///
    /// Only [`Self::check_let_stmt`] calls this; a sequence literal in any
    /// other position is rejected by `check_expr` (T0063, the
    /// let-value-only rule), so `expected` is exactly the binding's
    /// declared element type.
    fn check_sequence_lit(
        &mut self,
        seq: &SequenceLit,
        scope: &mut Scope,
        expected: Option<Type>,
    ) -> Type {
        let elems: Vec<Expr> = seq.elements().collect();
        let Some((first, rest)) = elems.split_first() else {
            // Empty `Sequence []`: fall back to the annotation's element
            // type, or demand one.
            return match expected {
                Some(e) => Type::Sequence(Box::new(e)),
                None => {
                    self.error(
                        self.node_span(seq.syntax()),
                        "T0061",
                        "empty sequence literal needs a type annotation, \
                         e.g. `let s: Sequence Integer = Sequence []`",
                    );
                    Type::Sequence(Box::new(Type::Unknown))
                }
            };
        };
        let elem_ty = self.check_expr(first, scope);
        for e in rest {
            let t = self.check_expr(e, scope);
            if !t.assignable_to(&elem_ty) {
                self.error(
                    self.node_span(e.syntax()),
                    "T0062",
                    format!(
                        "sequence element type {t} differs from the first element's {elem_ty}"
                    ),
                );
            }
        }
        Type::Sequence(Box::new(elem_ty))
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
            // A possrep accessor `x.component` on a nominal scalar reads its
            // possrep component (RM Pre 5). Single-possrep, so the one component
            // must match by name.
            Type::Scalar(ref sname) => match self.nominal_scalars.get(sname).cloned() {
                Some(ps) if ps.component == field_name => ps.ty,
                _ => {
                    self.error(
                        self.token_span(&field_tok),
                        "T0017",
                        format!("unknown possrep component `{field_name}` of type `{sname}`"),
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
            BinaryOp::Intersect => self.check_intersect_binary(bin, scope),
            BinaryOp::Union => self.check_union_binary(bin, scope),
            BinaryOp::Minus => self.check_minus_binary(bin, scope),
            BinaryOp::And | BinaryOp::Or => self.check_logical_op(bin, op, scope),
            BinaryOp::Eq | BinaryOp::NotEq => self.check_equality_op(bin, op, scope),
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                self.check_ordering_op(bin, op, scope)
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::IntDiv => {
                self.check_arith_op(bin, op, scope)
            }
            BinaryOp::Concat => self.check_concat_op(bin, scope),
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

    /// The identical-headings check shared by `union`, `intersect`, and
    /// `minus`: all three require the two operands to have the *same* heading
    /// (Coddl has no nulls, so there is no heading-agnostic set operation).
    /// Returns the shared heading, or `None` after emitting T0038 naming the
    /// attribute(s) that differ. `Heading` is canonical-sorted, so equality is
    /// order-independent. `op_name` is interpolated so each operator reports
    /// under its own lexeme.
    fn identical_headings(
        &mut self,
        bin: &BinaryExpr,
        lhs_h: &Heading,
        rhs_h: &Heading,
        op_name: &str,
    ) -> Option<Heading> {
        if lhs_h == rhs_h {
            return Some(lhs_h.clone());
        }
        // Name the attributes present (by name and type) on one side but not
        // the other, so the diagnostic points at the actual mismatch.
        let mut differing: Vec<String> = Vec::new();
        for (name, ty) in lhs_h.attrs() {
            if !rhs_h.attrs().iter().any(|(n, t)| n == name && t == ty) {
                differing.push(name.clone());
            }
        }
        for (name, ty) in rhs_h.attrs() {
            if !lhs_h.attrs().iter().any(|(n, t)| n == name && t == ty)
                && !differing.contains(name)
            {
                differing.push(name.clone());
            }
        }
        differing.sort();
        differing.dedup();
        self.error(
            self.node_span(bin.syntax()),
            "T0038",
            format!(
                "`{op_name}` operands must have identical headings — they differ on `{}`",
                differing.join("`, `")
            ),
        );
        None
    }

    /// `R join S` — natural join (Algebra-A AND). Both operands must be
    /// relations that share ≥1 attribute (with matching types on the shared
    /// attributes), but **not** identical headings; the result heading is the
    /// union. This makes the AND-family heading relationship a total, mutually
    /// exclusive partition: disjoint → `times` (T0035), identical → `intersect`
    /// (T0039), partial overlap → `join`. A shared-attribute type clash → T0036.
    fn check_join_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "join", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "join", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        // Identical headings: the join would be a set intersection (a join on
        // every attribute). Require `intersect`, mirroring how disjoint headings
        // require `times`. Checked before `natural_join_heading` because
        // identical non-empty headings are not disjoint and would otherwise pass.
        if lhs_h == rhs_h {
            self.error(
                self.node_span(bin.syntax()),
                "T0039",
                "`join` operands have identical headings — did you mean `intersect`?".to_string(),
            );
            return Type::Unknown;
        }
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
    /// AND then REMOVE). `compose` is meaningful only when **both** derived sets
    /// are non-empty: the shared attributes `A ∩ B` (the join/remove key) and the
    /// symmetric difference `A △ B` (the result heading). Empty `A ∩ B` (disjoint)
    /// → T0035 (suggest `times`, nothing to join on). Empty `A △ B` (identical
    /// headings) → T0040 (every attribute removed, result always nullary; suggest
    /// `intersect`). A shared-attribute type clash → T0036. So `compose`'s legal
    /// domain is partial overlap — same as `join`; a proper subset/superset like
    /// `{a,b,c} compose {b,c}` (→ `{a}`) is fine.
    fn check_compose_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "compose", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "compose", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        // Identical headings: every attribute is shared, so the REMOVE drops them
        // all and the result is always the nullary relation regardless of data.
        // Reject and suggest `intersect` (the likely intent — keep the matching
        // tuples). Checked before `natural_join_heading` (identical non-empty
        // headings are not disjoint and would otherwise pass).
        if lhs_h == rhs_h {
            self.error(
                self.node_span(bin.syntax()),
                "T0040",
                "`compose` operands have identical headings — every attribute \
                 would be removed (the result is always nullary); did you mean \
                 `intersect`?"
                    .to_string(),
            );
            return Type::Unknown;
        }
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

    /// `R intersect S` — set intersection (Algebra-A AND on identical headings:
    /// a join on *every* attribute). Both operands must be relations with the
    /// **same** heading; mismatched headings → T0038. The result heading is that
    /// shared heading.
    fn check_intersect_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "intersect", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "intersect", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        match self.identical_headings(bin, &lhs_h, &rhs_h, "intersect") {
            Some(h) => Type::Relation(h),
            None => Type::Unknown,
        }
    }

    /// `R union S` — set union (Algebra-A OR restricted to matching headings;
    /// Coddl has no nulls, so no heading-agnostic union). Both operands must be
    /// relations with the **same** heading; mismatched headings → T0038. The
    /// result heading is that shared heading.
    fn check_union_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "union", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "union", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        match self.identical_headings(bin, &lhs_h, &rhs_h, "union") {
            Some(h) => Type::Relation(h),
            None => Type::Unknown,
        }
    }

    /// `R minus S` — set difference (Algebra-A AND-NOT restricted to matching
    /// headings). Both operands must be relations with the **same** heading;
    /// mismatched headings → T0038. The result heading is that shared heading
    /// (the result is the subset of `lhs` not in `rhs`).
    fn check_minus_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "minus", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "minus", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        match self.identical_headings(bin, &lhs_h, &rhs_h, "minus") {
            Some(h) => Type::Relation(h),
            None => Type::Unknown,
        }
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
    /// (Integer, Text, Character, Approximate, Rational, or Boolean for v1).
    /// Result is Boolean.
    fn check_equality_op(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let supported = |t: &Type| {
            matches!(
                t,
                Type::Integer
                    | Type::Text
                    | Type::Character
                    | Type::Approximate
                    | Type::Rational
                    | Type::Boolean
                    | Type::Unknown
            )
        };
        if !supported(&lhs_ty) || !supported(&rhs_ty) || !lhs_ty.assignable_to(&rhs_ty) {
            let opname = op_display(op);
            self.error(
                self.node_span(bin.syntax()),
                "T0021",
                format!(
                    "`{opname}` operands must share a scalar type (Integer, Text, Character, Approximate, Rational, or Boolean); got {lhs_ty} vs {rhs_ty}"
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
        // Ordering is defined on Integer and Rational scalars (no mixing).
        // Rational compares via the runtime's cross-multiply comparator; both
        // must be the same scalar type.
        let supported = |t: &Type| matches!(t, Type::Integer | Type::Rational | Type::Unknown);
        let same_kind = matches!(
            (&lhs_ty, &rhs_ty),
            (Type::Integer, Type::Integer)
                | (Type::Rational, Type::Rational)
                | (Type::Unknown, _)
                | (_, Type::Unknown)
        );
        if !supported(&lhs_ty) || !supported(&rhs_ty) || !same_kind {
            let opname = op_display(op);
            self.error(
                self.node_span(bin.syntax()),
                "T0021",
                format!("`{opname}` requires two Integer or two Rational operands; got {lhs_ty} vs {rhs_ty}"),
            );
        }
        Type::Boolean
    }

    /// `lhs + rhs` / `lhs - rhs` / `lhs * rhs` / `lhs / rhs` — scalar
    /// arithmetic. `div` (truncating integer division, toward zero) is
    /// Integer-only → Integer. `+ - * /` accept either two Integers or two
    /// Rationals (no implicit mixing); `/` on Integers is **exact** and yields
    /// Rational, and every op on Rationals yields Rational. T0043 otherwise.
    fn check_arith_op(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let is_int = |t: &Type| matches!(t, Type::Integer | Type::Unknown);
        let is_rat = |t: &Type| matches!(t, Type::Rational | Type::Unknown);

        // `div` — truncating integer division, Integer only.
        if matches!(op, BinaryOp::IntDiv) {
            if !is_int(&lhs_ty) || !is_int(&rhs_ty) {
                self.error(
                    self.node_span(bin.syntax()),
                    "T0043",
                    format!("`div` requires Integer operands; got {lhs_ty} vs {rhs_ty}"),
                );
            }
            return Type::Integer;
        }

        // `+ - * /`: both Integer, or both Rational (no mixing).
        let both_rat = is_rat(&lhs_ty)
            && is_rat(&rhs_ty)
            && (matches!(lhs_ty, Type::Rational) || matches!(rhs_ty, Type::Rational));
        if both_rat {
            return Type::Rational;
        }
        if is_int(&lhs_ty) && is_int(&rhs_ty) {
            // Integer operands: `/` is exact → Rational, `+ - *` stay Integer.
            return if matches!(op, BinaryOp::Div) {
                Type::Rational
            } else {
                Type::Integer
            };
        }
        // Mixed or unsupported operand types.
        let opname = op_display(op);
        self.error(
            self.node_span(bin.syntax()),
            "T0043",
            format!("`{opname}` requires two Integer or two Rational operands; got {lhs_ty} vs {rhs_ty}"),
        );
        if matches!(op, BinaryOp::Div) {
            Type::Rational
        } else {
            Type::Integer
        }
    }

    /// `lhs || rhs` — text/character concatenation. Each operand must be Text
    /// or Character (any mix); the result is always Text (two Characters can't
    /// be one Character). T0044 on an operand that is neither Text nor
    /// Character.
    fn check_concat_op(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let supported = |t: &Type| matches!(t, Type::Text | Type::Character | Type::Unknown);
        if !supported(&lhs_ty) || !supported(&rhs_ty) {
            self.error(
                self.node_span(bin.syntax()),
                "T0044",
                format!("`||` requires Text or Character operands; got {lhs_ty} vs {rhs_ty}"),
            );
        }
        Type::Text
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
        // Resolve the callee to a method name plus, for the UFCS method-call
        // form `receiver.method { … }`, the receiver's type — injected below
        // as a synthetic `self` argument (`x.m { … }` ≡ `m { self: x, … }`).
        // A bare `NameRef` callee is the ordinary prefix call (no receiver).
        let callee = call.callee();
        let (callee_name_tok, self_ty): (SyntaxToken, Option<Type>) = match &callee {
            Some(Expr::NameRef(n)) => match n.ident() {
                Some(t) => (t, None),
                None => return Type::Unknown,
            },
            Some(Expr::FieldAccess(fa)) => {
                // The receiver is type-checked exactly once, here. (A braced
                // call over a field access is a method call; a bare field
                // access with no braces is a possrep/tuple field, handled by
                // `check_field_access`.)
                let recv_ty = match fa.base() {
                    Some(b) => self.check_expr(&b, scope),
                    None => return Type::Unknown,
                };
                match fa.field() {
                    Some(t) => (t, Some(recv_ty)),
                    None => return Type::Unknown,
                }
            }
            // Parser already complained about a missing callee, or the callee
            // is structurally something we don't handle as a call.
            _ => return Type::Unknown,
        };

        let callee_name = callee_name_tok.text().to_string();

        // `format` is a compile-time intrinsic, not an ordinary builtin: it
        // needs a cross-argument check (placeholders ↔ args heading) and
        // has no runtime symbol, so it is handled entirely here and is not
        // in the registry. It is not method-callable (`x.format {}` falls
        // through to normal resolution and fails to resolve).
        if callee_name == "format" && self_ty.is_none() {
            return self.check_format_call(call, scope);
        }

        // The `write_line { template: FormatText, args: Tuple H }` overload —
        // the same heading shape as `format`, but it writes the interpolated
        // Text instead of returning it. Like `format` it is frontend-hardcoded
        // (no generics, absent from the registry, not user-declarable), and it
        // routes to `check_format_call` so the template is validated inline —
        // never through `check_expr`, so the `FormatText` firewall is untouched.
        // Discriminated by a `template` argument; the `message: Text` overload
        // never carries one, so the two forms are disjoint.
        if callee_name == "write_line" && self_ty.is_none() && call_has_named_arg(call, "template") {
            // Preserve the side-effecting/transaction rule (T0026) the plain
            // overload gets from the registry.
            if let Some(sig) = self.builtins.candidates("write_line").first().cloned() {
                self.check_call_purity(&callee_name, &callee_name_tok, &sig);
            }
            // Validate template + args exactly as `format`; the Text result is
            // discarded — this overload yields unit.
            self.check_format_call(call, scope);
            return Type::unit();
        }

        // Operators are identified by name + heading: a user `oper` may extend
        // a built-in name with a distinct heading, so resolve across the merged
        // candidate set — every built-in overload of this name plus the (at most
        // one) user overload. A single candidate takes the monomorphic path; two
        // or more go through overload resolution.
        let mut candidates = self.builtins.candidates(&callee_name).to_vec();
        if let Some(user_sig) = self.user_opers.get(&callee_name).cloned() {
            candidates.push(user_sig);
        }
        // A possrep scalar's synthesized selector: `Name { component: e } -> Name`
        // (RM Pre 4 — a selector per possrep). Derived from the possrep; a user
        // oper can't reuse a scalar's name (rejected at registration, T0060), so
        // at most one of these applies.
        if let Some(ps) = self.nominal_scalars.get(&callee_name).cloned() {
            candidates.push(crate::builtins::OperSig {
                params: vec![(
                    std::borrow::Cow::Owned(ps.component),
                    crate::builtins::ParamKind::Concrete(ps.ty),
                )],
                return_type: Type::Scalar(callee_name.clone()),
                purity: crate::builtins::Purity::Pure,
            });
        }

        // A method call `x.m { … }` requires `m` to declare a `self` parameter
        // (UFCS binds the receiver to it). Reject up front with a clear
        // diagnostic rather than cascading through arg resolution.
        if self_ty.is_some()
            && !candidates.is_empty()
            && !candidates
                .iter()
                .any(|s| s.params.iter().any(|(p, _)| p.as_ref() == "self"))
        {
            self.error(
                self.token_span(&callee_name_tok),
                "T0070",
                format!("`{callee_name}` is not callable as a method — it has no `self` parameter"),
            );
            return Type::Unknown;
        }

        match candidates.len() {
            0 => {
                // If the name is an opt-in stdlib operator, point at the import
                // rather than reporting a plain unresolved name.
                if let Some(module) = self.stdlib_oper_owner.get(&callee_name).cloned() {
                    self.error(
                        self.token_span(&callee_name_tok),
                        "T0087",
                        format!("operator `{callee_name}` requires `use module {module};`"),
                    );
                } else {
                    self.error(
                        self.token_span(&callee_name_tok),
                        "T0001",
                        format!("cannot resolve name `{callee_name}`"),
                    );
                }
                Type::Unknown
            }
            // Fast path for the common single-signature case — behavior is
            // identical to before overloading landed.
            1 => self.check_monomorphic_call(
                call,
                &callee_name,
                &callee_name_tok,
                candidates.into_iter().next().unwrap(),
                self_ty,
                scope,
            ),
            _ => self.check_overloaded_call(
                call,
                &callee_name,
                &callee_name_tok,
                &candidates,
                self_ty,
                scope,
            ),
        }
    }

    /// The single-signature call path: purity check, name-matched argument
    /// validation, missing-argument check, result type.
    fn check_monomorphic_call(
        &mut self,
        call: &CallExpr,
        callee_name: &str,
        callee_name_tok: &SyntaxToken,
        sig: crate::builtins::OperSig,
        self_ty: Option<Type>,
        scope: &mut Scope,
    ) -> Type {
        self.check_call_purity(callee_name, callee_name_tok, &sig);

        // Walk the actual argument list against the declared parameters.
        let mut seen: HashSet<String> = HashSet::new();
        let mut provided: HashSet<String> = HashSet::new();

        // UFCS: the receiver supplies `self`. Mark it seen (so an explicit
        // `self:` in the braces trips T0008) and provided, and validate the
        // receiver against the `self` parameter's kind. (A candidate lacking
        // `self` was already rejected in `check_call` with T0070.)
        if let Some(recv_ty) = &self_ty {
            seen.insert("self".to_string());
            if let Some((_, kind)) = sig.params.iter().find(|(p, _)| p.as_ref() == "self") {
                provided.insert("self".to_string());
                if !matches!(recv_ty, Type::Unknown) && !param_kind_accepts(kind, recv_ty) {
                    self.error(
                        self.token_span(callee_name_tok),
                        "T0004",
                        format!(
                            "receiver of `{callee_name}` has type {recv_ty}, which its `self` parameter does not accept"
                        ),
                    );
                }
            }
        }

        if let Some(arg_list) = call.args() {
            for arg in arg_list.args() {
                self.check_named_arg(&arg, &sig, scope, &mut seen, &mut provided);
            }
        }

        // Every declared parameter must be supplied exactly once.
        for (pname, _) in &sig.params {
            if !provided.contains(pname.as_ref()) {
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

    /// Resolve an overloaded builtin (e.g. `to_text`) by the static types
    /// of its arguments. Each candidate signature is monomorphic, so this
    /// is pure static dispatch (RM Pre 8 preserved); the surface name is
    /// just a shared spelling. Argument types are evaluated once here to
    /// avoid double-checking expressions.
    fn check_overloaded_call(
        &mut self,
        call: &CallExpr,
        callee_name: &str,
        callee_name_tok: &SyntaxToken,
        sigs: &[crate::builtins::OperSig],
        self_ty: Option<Type>,
        scope: &mut Scope,
    ) -> Type {
        let mut seen: HashSet<String> = HashSet::new();
        // (arg name, evaluated type)
        let mut args: Vec<(String, Type)> = Vec::new();
        let mut any_unknown = false;
        // UFCS: the receiver is a synthetic `self` argument (already checked in
        // `check_call`). It joins overload resolution like any other arg — its
        // type picks the overload (e.g. a `Sequence` receiver selects
        // `cardinality`'s `AnySequence` over `AnyRelation`). Seeding `seen`
        // makes an explicit `self:` in the braces a duplicate (T0008).
        if let Some(recv_ty) = self_ty {
            seen.insert("self".to_string());
            if matches!(recv_ty, Type::Unknown) {
                any_unknown = true;
            }
            args.push(("self".to_string(), recv_ty));
        }
        if let Some(arg_list) = call.args() {
            for arg in arg_list.args() {
                let Some(name_tok) = arg.name() else { continue };
                let aname = name_tok.text().to_string();
                if !seen.insert(aname.clone()) {
                    self.error(
                        self.token_span(&name_tok),
                        "T0008",
                        format!("duplicate argument `{aname}`"),
                    );
                    continue;
                }
                let aty = match arg.value() {
                    Some(v) => self.check_expr(&v, scope),
                    None => Type::Unknown,
                };
                if matches!(aty, Type::Unknown) {
                    any_unknown = true;
                }
                args.push((aname, aty));
            }
        }

        let provided: HashSet<&str> = args.iter().map(|(n, _)| n.as_str()).collect();
        let applicable: Vec<&crate::builtins::OperSig> = sigs
            .iter()
            .filter(|sig| {
                let pnames: HashSet<&str> = sig.params.iter().map(|(p, _)| p.as_ref()).collect();
                pnames == provided
                    && sig.params.iter().all(|(p, kind)| {
                        args.iter()
                            .find(|(n, _)| n.as_str() == p.as_ref())
                            .map(|(_, t)| param_kind_accepts(kind, t))
                            .unwrap_or(false)
                    })
            })
            .collect();

        match applicable.as_slice() {
            [sig] => {
                self.check_call_purity(callee_name, callee_name_tok, sig);
                sig.return_type.clone()
            }
            // Unknown argument types (error recovery) make multiple — or
            // zero — candidates "match" spuriously; stay quiet so we don't
            // pile on the upstream error.
            _ if any_unknown => applicable
                .first()
                .map(|s| s.return_type.clone())
                .unwrap_or(Type::Unknown),
            [] => {
                let got = args
                    .iter()
                    .map(|(n, t)| format!("{n}: {t}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let span = call
                    .args()
                    .map(|a| self.node_span(a.syntax()))
                    .unwrap_or_else(|| self.node_span(call.syntax()));
                self.error(
                    span,
                    "T0054",
                    format!("no matching overload of `{callee_name}` for argument types {{ {got} }}"),
                );
                Type::Unknown
            }
            // Genuine ambiguity can't arise from the current builtins (each
            // overload has a distinct concrete `self` type). Fall back to
            // the first applicable signature's result without inventing a
            // diagnostic code for an unreachable case.
            _ => applicable[0].return_type.clone(),
        }
    }

    /// Transactions must be pure so the runtime can replay them on
    /// serialization conflict. Side-effecting builtins (write_line,
    /// write_relation, read_line) are blocked inside one; the surface rule
    /// extends to user-defined opers once they carry a derived purity flag.
    fn check_call_purity(
        &mut self,
        callee_name: &str,
        callee_name_tok: &SyntaxToken,
        sig: &crate::builtins::OperSig,
    ) {
        if self.transaction_depth > 0
            && matches!(sig.purity, crate::builtins::Purity::SideEffecting)
        {
            self.error(
                self.token_span(callee_name_tok),
                "T0026",
                format!(
                    "side-effecting operator `{callee_name}` called inside `transaction [...]` (transactions must be pure)"
                ),
            );
        }
    }

    /// True iff some `to_text` overload accepts a `self` argument of type
    /// `ty`. `format` desugars each `{x}` placeholder to `to_text { self: x }`,
    /// so a placeholder whose `args` attribute isn't `to_text`-able (a
    /// `Sequence`, `Tuple`, or `Relation`) must be rejected at check time —
    /// otherwise it reaches the lowerer's `to_text` fold, which has no such
    /// overload and would panic.
    fn to_text_accepts(&self, ty: &Type) -> bool {
        // Built-in `to_text` overloads plus the (at most one) user-defined one —
        // interpolation dispatches across the merged set, so a user
        // `to_text { self: T }` makes `{x : T}` renderable.
        self.builtins
            .candidates("to_text")
            .iter()
            .chain(self.user_opers.get("to_text"))
            .any(|sig| {
                sig.params
                    .iter()
                    .find(|(p, _)| p.as_ref() == "self")
                    .map(|(_, kind)| param_kind_accepts(kind, ty))
                    .unwrap_or(false)
            })
    }

    /// Parse an `f"…"` token into template chunks, reporting T0057 at the
    /// malformed-placeholder sub-span. Returns `None` when the template is
    /// malformed (no usable chunks). Shared by the `let x = f"…"` binding site
    /// and the inline-literal `format` template argument.
    fn parse_template_tok(&mut self, tok: &SyntaxToken) -> Option<Vec<TemplateChunk>> {
        let tok_span = self.token_span(tok);
        match parse_format_template(tok.text()) {
            Ok(chunks) => Some(chunks),
            Err(errors) => {
                for e in errors {
                    let span = Span::new(
                        tok_span.file,
                        tok_span.start + e.range.start as u32,
                        tok_span.start + e.range.end as u32,
                    );
                    self.error(span, "T0057", e.kind.message());
                }
                None
            }
        }
    }

    /// Type-check the `format { template: …, args: { … } }` intrinsic.
    ///
    /// `template` must be an `f"…"` literal *or* a `let`-bound `f"…"` template
    /// (T0056 otherwise) — neither is routed through `check_expr`, both so the
    /// literal-only requirement is enforced and so the stray-`f"…"` firewall
    /// (T0055) doesn't fire on a legitimate site. `args` is heading-
    /// polymorphic and optional (absent ⇒ empty heading). Every placeholder
    /// must name an `args` attribute (T0058); attributes no placeholder uses
    /// warn (T0059); a malformed template is T0057. The result is always `Text`
    /// (the lowerer desugars it to a `to_text`/`||` chain), returned even
    /// on error so callers recover.
    fn check_format_call(&mut self, call: &CallExpr, scope: &mut Scope) -> Type {
        // Where the template came from: an inline `f"…"` literal (parsed here,
        // with T0057 at its sub-spans), or a `let`-bound template (chunks parsed
        // once at the binding site; placeholder errors anchor at the argument).
        enum TemplateSrc {
            Missing,
            Literal(SyntaxToken),
            Bound {
                chunks: Rc<Vec<TemplateChunk>>,
                span: Span,
            },
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut template_src = TemplateSrc::Missing;
        let mut have_template = false;
        // `Some(h)` once args types to a `Tuple`; left `None` if args is
        // absent *or* ill-typed — disambiguated by `args_present`.
        let mut args_heading: Option<Heading> = None;
        let mut args_present = false;

        if let Some(arg_list) = call.args() {
            for arg in arg_list.args() {
                let Some(name_tok) = arg.name() else { continue };
                let name = name_tok.text().to_string();
                if !seen.insert(name.clone()) {
                    self.error(
                        self.token_span(&name_tok),
                        "T0008",
                        format!("duplicate argument `{name}`"),
                    );
                    continue;
                }
                match name.as_str() {
                    "template" => {
                        have_template = true;
                        match arg.value() {
                            Some(Expr::Literal(lit))
                                if lit.token().map(|t| t.kind())
                                    == Some(SyntaxKind::FORMAT_STRING_LIT) =>
                            {
                                if let Some(tok) = lit.token() {
                                    template_src = TemplateSrc::Literal(tok);
                                }
                            }
                            // A `let`-bound `f"…"` template, reused here: the
                            // chunks were parsed at the binding site and ride on
                            // the binding. A name that resolves but isn't a
                            // template binding is T0056; an unresolved one is the
                            // usual T0001.
                            Some(Expr::NameRef(n)) => {
                                if let Some(ident) = n.ident() {
                                    let nm = ident.text();
                                    if let Some(chunks) = scope.format_template(nm) {
                                        scope.mark_used(nm);
                                        template_src = TemplateSrc::Bound {
                                            chunks,
                                            span: self.node_span(arg.syntax()),
                                        };
                                    } else if scope.lookup(nm).is_some() {
                                        // Resolves, but isn't a template binding —
                                        // still counts as a use (so it isn't also
                                        // flagged unused, T0032).
                                        scope.mark_used(nm);
                                        self.error(
                                            self.node_span(arg.syntax()),
                                            "T0056",
                                            "`format` template must be an f\"…\" literal or a `let` bound to one",
                                        );
                                    } else {
                                        self.error(
                                            self.token_span(&ident),
                                            "T0001",
                                            format!("cannot resolve name `{nm}`"),
                                        );
                                    }
                                }
                            }
                            other => {
                                let span = other
                                    .as_ref()
                                    .map(|v| self.node_span(v.syntax()))
                                    .unwrap_or_else(|| self.node_span(arg.syntax()));
                                self.error(
                                    span,
                                    "T0056",
                                    "`format` template must be an f\"…\" literal or a `let` bound to one",
                                );
                            }
                        }
                    }
                    "args" => {
                        args_present = true;
                        let ty = match arg.value() {
                            Some(v) => self.check_expr(&v, scope),
                            None => Type::Unknown,
                        };
                        match ty {
                            Type::Tuple(h) => args_heading = Some(h),
                            Type::Unknown => {} // recovery; heading stays None
                            other => {
                                let span = arg
                                    .value()
                                    .map(|v| self.node_span(v.syntax()))
                                    .unwrap_or_else(|| self.node_span(arg.syntax()));
                                self.error(
                                    span,
                                    "T0004",
                                    format!("argument `args` expected a Tuple, got {other}"),
                                );
                            }
                        }
                    }
                    _ => {
                        self.error(
                            self.token_span(&name_tok),
                            "T0002",
                            format!("argument `{name}` is not declared"),
                        );
                    }
                }
            }
        }

        if !have_template {
            let span = call
                .args()
                .map(|a| self.node_span(a.syntax()))
                .unwrap_or_else(|| self.node_span(call.syntax()));
            self.error(
                span,
                "T0003",
                "missing argument `template` in call to `format`",
            );
            return Type::Text;
        }

        // Resolve the heading to check placeholders against: absent args ⇒
        // empty (placeholders all fail T0058); present-but-ill-typed ⇒ None
        // (skip placeholder/heading checks, but still validate structure).
        let heading: Option<Heading> = match (args_present, args_heading) {
            (false, _) => Some(Heading::empty()),
            (true, Some(h)) => Some(h),
            (true, None) => None,
        };

        // Resolve the template to chunks plus a span for placeholder diagnostics.
        // An inline literal parses here (T0057 at its sub-spans); a `let`-bound
        // template reuses chunks parsed at the binding site and anchors any
        // placeholder error at the argument. Either may yield no chunks (T0056/
        // T0057 already reported), in which case the loop below is a no-op.
        let (chunks, place_span, warn_span): (
            Vec<TemplateChunk>,
            Box<dyn Fn(std::ops::Range<usize>) -> Span>,
            Span,
        ) = match template_src {
            TemplateSrc::Missing => return Type::Text,
            TemplateSrc::Literal(tok) => {
                let tok_span = self.token_span(&tok);
                let chunks = self.parse_template_tok(&tok).unwrap_or_default();
                (
                    chunks,
                    Box::new(move |r: std::ops::Range<usize>| {
                        Span::new(
                            tok_span.file,
                            tok_span.start + r.start as u32,
                            tok_span.start + r.end as u32,
                        )
                    }),
                    tok_span,
                )
            }
            TemplateSrc::Bound { chunks, span } => {
                ((*chunks).clone(), Box::new(move |_r| span), span)
            }
        };

        let mut used: HashSet<String> = HashSet::new();
        for chunk in &chunks {
            if let TemplateChunk::Placeholder { name, range } = chunk {
                used.insert(name.clone());
                if let Some(h) = &heading {
                    match h.lookup(name) {
                        None => {
                            self.error(
                                place_span(range.clone()),
                                "T0058",
                                format!(
                                    "format template references `{{{name}}}` but `args` has no attribute `{name}`"
                                ),
                            );
                        }
                        // `{name}` desugars to `to_text { self: <attr> }`;
                        // a non-`to_text`-able attribute (Sequence / Tuple /
                        // Relation) fails here exactly as a direct `to_text`
                        // call would (T0054), instead of panicking in the
                        // lowerer.
                        Some(attr_ty) => {
                            if !matches!(attr_ty, Type::Unknown) && !self.to_text_accepts(attr_ty) {
                                self.error(
                                    place_span(range.clone()),
                                    "T0054",
                                    format!(
                                        "format placeholder `{{{name}}}` has type {attr_ty}, which has no `to_text` overload"
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }
        // Every args attribute should be referenced by the template.
        if let Some(h) = &heading {
            for (attr, _) in h.attrs() {
                if !used.contains(attr) {
                    self.warn(
                        warn_span,
                        "T0059",
                        format!("`args` attribute `{attr}` is never used by the format template"),
                    );
                }
            }
        }

        Type::Text
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
            .find(|(p, _)| p.as_ref() == name.as_str())
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
            Some(crate::builtins::ParamKind::AnySequence) => {
                provided.insert(name.clone());
                // Accept any `Sequence T` regardless of element type
                // (mirrors `AnyRelation`); `Unknown` slips through for
                // recovery.
                if !matches!(arg_ty, Type::Sequence(_) | Type::Unknown) {
                    let span = arg
                        .value()
                        .map(|v| self.node_span(v.syntax()))
                        .unwrap_or_else(|| self.node_span(arg.syntax()));
                    self.error(
                        span,
                        "T0004",
                        format!("argument `{name}` expected a Sequence, got {arg_ty}"),
                    );
                }
            }
            Some(crate::builtins::ParamKind::AnyTuple) => {
                provided.insert(name.clone());
                // Accept any `Tuple H` regardless of heading (mirrors
                // `AnyRelation`); `Unknown` slips through for recovery.
                if !matches!(arg_ty, Type::Tuple(_) | Type::Unknown) {
                    let span = arg
                        .value()
                        .map(|v| self.node_span(v.syntax()))
                        .unwrap_or_else(|| self.node_span(arg.syntax()));
                    self.error(
                        span,
                        "T0004",
                        format!("argument `{name}` expected a Tuple, got {arg_ty}"),
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

/// Does an argument of static type `ty` satisfy a parameter of kind `kind`?
/// `Unknown` (error recovery) is accepted everywhere so one upstream
/// failure doesn't poison overload resolution. Shared by the overloaded
/// call path.
/// True iff `call` supplies an argument literally named `name`. Used to route
/// `write_line { template, args }` to the format-writing overload.
fn call_has_named_arg(call: &CallExpr, name: &str) -> bool {
    call.args()
        .map(|list| {
            list.args()
                .any(|a| a.name().map(|t| t.text().to_string()).as_deref() == Some(name))
        })
        .unwrap_or(false)
}

fn param_kind_accepts(kind: &crate::builtins::ParamKind, ty: &Type) -> bool {
    match kind {
        crate::builtins::ParamKind::Concrete(expected) => ty.assignable_to(expected),
        crate::builtins::ParamKind::AnyRelation => matches!(ty, Type::Relation(_) | Type::Unknown),
        crate::builtins::ParamKind::AnySequence => matches!(ty, Type::Sequence(_) | Type::Unknown),
        crate::builtins::ParamKind::AnyTuple => matches!(ty, Type::Tuple(_) | Type::Unknown),
    }
}

/// Collect the attribute names a scalar expression references into `into` — the
/// "removed set" of a general-expression `replace`. Walks `NameRef` (a leaf
/// attribute ref), `Binary` (both operands), and `Unary` (its operand); other
/// shapes contribute nothing. Names not in the operand heading are filtered by
/// the caller.
fn attr_refs(expr: &Expr, into: &mut HashSet<String>) {
    match expr {
        Expr::NameRef(n) => {
            if let Some(tok) = n.ident() {
                into.insert(tok.text().to_string());
            }
        }
        Expr::Binary(b) => {
            if let Some(lhs) = b.lhs() {
                attr_refs(&lhs, into);
            }
            if let Some(rhs) = b.rhs() {
                attr_refs(&rhs, into);
            }
        }
        Expr::Unary(u) => {
            if let Some(operand) = u.operand() {
                attr_refs(&operand, into);
            }
        }
        _ => {}
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
        BinaryOp::Intersect => "intersect",
        BinaryOp::Union => "union",
        BinaryOp::Minus => "minus",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::IntDiv => "div",
        BinaryOp::Concat => "||",
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
    fn user_oper_call_resolves_clean() {
        let src = "program p; \
                   oper greet {} -> Text [ \"hi\" ]; \
                   oper main {} [ let g = greet {}; write_line { message: g }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn user_oper_forward_reference_resolves_no_t0001() {
        // `main` calls `greet` declared *after* it — the pre-pass registers
        // every signature before any body is walked.
        let src = "program p; \
                   oper main {} [ let g = greet {}; write_line { message: g }; ]; \
                   oper greet {} -> Text [ \"hi\" ];";
        assert!(!codes(src).contains(&"T0001"), "{:?}", codes(src));
    }

    #[test]
    fn user_oper_missing_arg_diagnoses_t0003() {
        let src = "program p; \
                   oper echo { x: Text } -> Text [ x ]; \
                   oper main {} [ let g = echo {}; write_line { message: g }; ];";
        assert!(codes(src).contains(&"T0003"), "{:?}", codes(src));
    }

    #[test]
    fn user_oper_wrong_arg_type_diagnoses_t0004() {
        let src = "program p; \
                   oper echo { x: Text } -> Text [ x ]; \
                   oper main {} [ let g = echo { x: 42 }; write_line { message: g }; ];";
        assert!(codes(src).contains(&"T0004"), "{:?}", codes(src));
    }

    #[test]
    fn duplicate_user_oper_diagnoses_t0060() {
        let src = "program p; \
                   oper foo {} -> Text [ \"a\" ]; \
                   oper foo {} -> Text [ \"b\" ]; \
                   oper main {} [ write_line { message: foo {} }; ];";
        assert!(codes(src).contains(&"T0060"), "{:?}", codes(src));
    }

    #[test]
    fn user_oper_redefining_builtin_heading_diagnoses_t0060() {
        // Operators are identified by name + heading: a user `oper` whose
        // heading exactly matches a built-in overload is a redefinition (T0060).
        let src = "program p; \
                   oper write_line { message: Text } [ {} ]; \
                   oper main {} [ write_line { message: \"x\" }; ];";
        assert!(codes(src).contains(&"T0060"), "{:?}", codes(src));
    }

    #[test]
    fn user_oper_extending_builtin_with_distinct_heading_is_allowed() {
        // A distinct heading is a *new* overload, not a redefinition — so a user
        // `to_text { self: Sequence Text }` registers alongside the built-in
        // `to_text` overloads and a call resolves to it (no T0060, no T0054).
        let src = "program p; \
                   oper to_text { self: Sequence Text } -> Text [ \"x\" ]; \
                   oper main {} [ let names = Sequence [ \"a\" ]; \
                   let _t = to_text { self: names }; ];";
        let c = codes(src);
        assert!(!c.contains(&"T0060"), "unexpected T0060: {c:?}");
        assert!(!c.contains(&"T0054"), "unexpected T0054: {c:?}");
        assert!(!c.contains(&"T0001"), "unexpected T0001: {c:?}");
    }

    #[test]
    fn second_user_overload_of_a_name_diagnoses_t0060() {
        // Only one user overload per name is supported for now (pending linkage
        // mangling), even with distinct headings.
        let src = "program p; \
                   oper to_text { self: Sequence Text } -> Text [ \"x\" ]; \
                   oper to_text { self: Sequence Integer } -> Text [ \"y\" ]; \
                   oper main {} [];";
        assert!(codes(src).contains(&"T0060"), "{:?}", codes(src));
    }

    #[test]
    fn user_oper_cannot_redefine_format_intrinsic_t0060() {
        let src = "program p; \
                   oper format { x: Text } -> Text [ \"x\" ]; \
                   oper main {} [];";
        assert!(codes(src).contains(&"T0060"), "{:?}", codes(src));
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
    fn assignment_to_public_relvar_is_an_allowed_target() {
        // A public relvar is a write target now (surgical DML at lowering); a
        // recognized self-referencing shape inside a transaction typechecks
        // clean. (Whether the RHS shape is *emittable* is a lowering concern.)
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ transaction [ R := R minus (R where a = 1); ] ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn assignment_to_public_relvar_outside_transaction_diagnoses_t0025() {
        // The self-referencing RHS references the public relvar, so the
        // transaction-scope rule still fires outside any `transaction [...]`.
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ R := R minus (R where a = 1); ];";
        assert!(codes(src).contains(&"T0025"), "{:?}", codes(src));
    }

    #[test]
    fn assignment_to_public_relvar_heading_mismatch_diagnoses_t0034() {
        // Heading-match still applies to a public target.
        let src = "program p; database greetings; \
                   public relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ transaction [ R := Relation { {a: 1} }; ] ];";
        assert!(codes(src).contains(&"T0034"), "{:?}", codes(src));
    }

    #[test]
    fn assignment_to_undeclared_name_diagnoses_t0033() {
        let src = "program p; oper main {} [ Nope := Relation { {a: 1} }; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn self_assignment_warns_t0051() {
        // `R := R` does nothing — warn (it's elided at lowering).
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ R := R; ];";
        assert!(codes(src).contains(&"T0051"), "{:?}", codes(src));
    }

    #[test]
    fn non_identity_assignment_does_not_warn_t0051() {
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { a: Integer } key { a }; \
                   oper main {} [ R := S; ];";
        assert!(!codes(src).contains(&"T0051"), "{:?}", codes(src));
    }

    #[test]
    fn truncate_private_relvar_checks_clean() {
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ truncate R; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn truncate_public_relvar_in_transaction_checks_clean() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ transaction [ truncate R; ] ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn truncate_public_relvar_outside_transaction_diagnoses_t0025() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ truncate R; ];";
        assert!(codes(src).contains(&"T0025"), "{:?}", codes(src));
    }

    #[test]
    fn truncate_undeclared_name_diagnoses_t0033() {
        let src = "program p; oper main {} [ truncate Nope; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn truncate_non_relvar_operand_diagnoses_t0033() {
        // A restricted operand (`R where p`) isn't a bare relvar — that's a
        // delete, not a truncate.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ truncate R where a = 1; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn delete_private_relvar_checks_clean() {
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ delete R where a = 1; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn delete_public_relvar_in_transaction_checks_clean() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ transaction [ delete R where a = 1; ] ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn delete_public_relvar_outside_transaction_diagnoses_t0025() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ delete R where a = 1; ];";
        assert!(codes(src).contains(&"T0025"), "{:?}", codes(src));
    }

    #[test]
    fn delete_without_where_diagnoses_t0052() {
        // A bare `delete R;` would clear the whole relvar — that's `truncate`.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ delete R; ];";
        assert!(codes(src).contains(&"T0052"), "{:?}", codes(src));
    }

    #[test]
    fn delete_undeclared_name_diagnoses_t0033() {
        let src = "program p; oper main {} [ delete Nope where a = 1; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn delete_where_over_non_relvar_diagnoses_t0033() {
        // The `where` lhs is a relation literal, not a bare relvar name.
        let src = "program p; oper main {} \
                   [ delete Relation { {a: 1} } where a = 1; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn delete_predicate_is_typechecked_t0020() {
        // The predicate is validated — a non-Boolean predicate fires T0020,
        // confirming the `where`-operand is checked.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ delete R where a; ];";
        assert!(codes(src).contains(&"T0020"), "{:?}", codes(src));
    }

    #[test]
    fn insert_tuple_set_private_checks_clean() {
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ insert R { {a: 1}, {a: 2} }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn insert_relexpr_private_checks_clean() {
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { a: Integer } key { a }; \
                   oper main {} [ insert R S; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn insert_public_in_transaction_checks_clean() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ transaction [ insert R { {a: 1} }; ] ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn insert_public_outside_transaction_diagnoses_t0025() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer } key { a }; \
                   oper main {} [ insert R { {a: 1} }; ];";
        assert!(codes(src).contains(&"T0025"), "{:?}", codes(src));
    }

    #[test]
    fn insert_undeclared_target_diagnoses_t0033() {
        let src = "program p; oper main {} [ insert Nope { {a: 1} }; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn insert_heading_mismatch_diagnoses_t0034() {
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ insert R { {b: 1} }; ];";
        assert!(codes(src).contains(&"T0034"), "{:?}", codes(src));
    }

    #[test]
    fn insert_empty_tuple_set_diagnoses_t0034() {
        // An empty `{}` is `relfalse` (the nullary empty relation). Inserting it
        // into a headed relvar is a heading mismatch (∅ vs `{a}`) — T0034, the
        // same as any source whose heading differs from the relvar's.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ insert R {}; ];";
        assert!(codes(src).contains(&"T0034"), "{:?}", codes(src));
    }

    #[test]
    fn update_private_relvar_checks_clean() {
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R where a = 1 { b: \"x\" }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn update_all_checks_clean() {
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R { b: \"x\" }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn update_public_in_transaction_checks_clean() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ transaction [ update R where a = 1 { b: \"x\" }; ] ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn update_public_outside_transaction_diagnoses_t0025() {
        let src = "program p; database greetings; \
                   public relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R where a = 1 { b: \"x\" }; ];";
        assert!(codes(src).contains(&"T0025"), "{:?}", codes(src));
    }

    #[test]
    fn update_nonexistent_target_diagnoses_t0053() {
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R { nope: 1 }; ];";
        assert!(codes(src).contains(&"T0053"), "{:?}", codes(src));
    }

    #[test]
    fn update_type_mismatch_diagnoses_t0034() {
        // `a` is Integer; a Text value doesn't match.
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R { a: \"text\" }; ];";
        assert!(codes(src).contains(&"T0034"), "{:?}", codes(src));
    }

    #[test]
    fn update_allows_constant_value_no_t0042() {
        // Unlike `replace`, a constant value is fine (overwrite with a literal).
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R { b: \"const\" }; ];";
        assert!(!codes(src).contains(&"T0042"), "{:?}", codes(src));
    }

    #[test]
    fn update_allows_bare_reference_value_no_t0047() {
        // Unlike `replace`, a bare attribute reference is fine (copy a value).
        let src = "program p; private relvar R { a: Integer, b: Integer } key { a }; \
                   oper main {} [ update R { a: b }; ];";
        assert!(!codes(src).contains(&"T0047"), "{:?}", codes(src));
    }

    #[test]
    fn update_predicate_must_be_boolean_t0020() {
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R where a { b: \"x\" }; ];";
        assert!(codes(src).contains(&"T0020"), "{:?}", codes(src));
    }

    #[test]
    fn update_undeclared_target_diagnoses_t0033() {
        let src = "program p; oper main {} [ update Nope { a: 1 }; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn update_duplicate_target_diagnoses_t0031() {
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R { b: \"x\", b: \"y\" }; ];";
        assert!(codes(src).contains(&"T0031"), "{:?}", codes(src));
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
    fn join_with_identical_headings_diagnoses_t0039() {
        // Identical headings -> the join is a set intersection -> the user wants
        // `intersect` (mirrors disjoint -> `times`).
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R join S }; ];";
        assert!(codes(src).contains(&"T0039"), "{:?}", codes(src));
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
    fn compose_with_identical_headings_diagnoses_t0040() {
        // Every attribute is shared -> REMOVE drops them all -> always nullary ->
        // the user wants `intersect`.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R compose S }; ];";
        assert!(codes(src).contains(&"T0040"), "{:?}", codes(src));
    }

    #[test]
    fn compose_with_subset_heading_checks_clean() {
        // R { a, b, c } compose S { b, c } — shares { b, c }, not identical:
        // join on { b, c }, remove them, keep { a }. A proper partial overlap.
        let src = "program p; \
                   private relvar R { a: Integer, b: Integer, c: Integer } key { a }; \
                   private relvar S { b: Integer, c: Integer } key { b, c }; \
                   oper main {} [ write_relation { rel: R compose S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn intersect_with_identical_headings_checks_clean() {
        // R { a, b } intersect S { a, b } — identical headings -> ok, result { a, b }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R intersect S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn intersect_with_differing_headings_diagnoses_t0038() {
        // S has `c` where R has `b` -> headings not identical -> T0038.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R intersect S }; ];";
        assert!(codes(src).contains(&"T0038"), "{:?}", codes(src));
    }

    #[test]
    fn union_with_identical_headings_checks_clean() {
        // R { a, b } union S { a, b } — identical headings -> ok, result { a, b }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R union S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn union_with_differing_headings_diagnoses_t0038() {
        // S has `c` where R has `b` -> headings not identical -> T0038.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R union S }; ];";
        assert!(codes(src).contains(&"T0038"), "{:?}", codes(src));
    }

    #[test]
    fn prelude_checks_clean() {
        // The `coddl::core` prelude (embedded in coddl-stdlib) — the
        // `builtin oper` signatures — must fully parse and typecheck with zero
        // diagnostics.
        let core = coddl_stdlib::resolve(&coddl_stdlib::ModulePath::parse("coddl::core"))
            .expect("coddl::core is always embedded");
        let diags = diagnostics(core.source());
        assert!(diags.is_empty(), "coddl::core has diagnostics: {diags:?}");
    }

    // ── Module system — opt-in `use module …` scoping ────────────────────

    #[test]
    fn core_operators_visible_without_imports() {
        // `coddl::core` is always in scope — no `use module` needed.
        let src = "program p; oper main {} [ write_line { message: to_text { self: 1 } }; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn unknown_module_diagnoses_t0089() {
        let src = "program p; use module coddl::bogus; oper main {} [];";
        assert!(codes(src).contains(&"T0089"), "{:?}", codes(src));
    }

    #[test]
    fn importing_core_is_a_noop() {
        let src = "program p; use module coddl::core; oper main {} [];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn web_type_without_import_diagnoses_t0088() {
        // `RawRequest` belongs to opt-in `coddl::web`; unimported → T0088, not the
        // generic unknown-type T0005.
        let src = "program p; oper handle { req: RawRequest } [];";
        let cs = codes(src);
        assert!(cs.contains(&"T0088"), "{:?}", cs);
        assert!(!cs.contains(&"T0005"), "should be T0088, not T0005: {:?}", cs);
    }

    #[test]
    fn web_type_with_import_resolves_clean() {
        // Importing `coddl::web` brings `RawRequest` into scope; its fields resolve.
        let src = "program p; use module coddl::web; \
                   oper handle { req: RawRequest } -> Text [ req.body ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn unimported_module_name_is_a_free_identifier() {
        // No reserved words: without importing `coddl::web`, a user may define
        // their own `Request` type (the opt-in web name) freely.
        let src = "program p; \
                   type Request = Integer; \
                   oper f { x: Request } -> Request [ x ]; \
                   oper main {} [];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn importing_web_makes_request_collide_with_user_type_t0086() {
        // Once `coddl::web` is imported, `RawRequest` is defined — a same-named
        // user `type` is a genuine duplicate (T0086).
        let src = "program p; use module coddl::web; type RawRequest = Integer;";
        assert!(codes(src).contains(&"T0086"), "{:?}", codes(src));
    }

    #[test]
    fn builtin_relvar_without_import_diagnoses_t0090() {
        // `Environment` belongs to opt-in `coddl::env`; unimported → T0090, not
        // the generic unresolved-name T0001.
        let src = "program p; oper main {} [ write_relation { rel: Environment }; ];";
        let cs = codes(src);
        assert!(cs.contains(&"T0090"), "{:?}", cs);
        assert!(!cs.contains(&"T0001"), "should be T0090, not T0001: {:?}", cs);
    }

    #[test]
    fn builtin_relvar_with_import_resolves_clean() {
        // Importing `coddl::env` brings `Environment` (a relation) into scope.
        let src = "program p; use module coddl::env; \
                   oper main {} [ write_relation { rel: Environment }; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn builtin_relvar_in_a_checked_file_is_inert() {
        // A stdlib module's own source (`env.cd`, a `builtin relvar` decl) opened
        // in the editor must typecheck clean — the `builtin relvar` is inert
        // here, like a user `builtin oper`. Regression: it used to trip a bogus
        // decl-site error that surfaced in the LSP when env.cd was opened.
        let env = coddl_stdlib::resolve(&coddl_stdlib::ModulePath::parse("coddl::env"))
            .expect("coddl::env is embedded");
        assert!(
            diagnostics(env.source()).is_empty(),
            "coddl::env source should check clean: {:?}",
            diagnostics(env.source())
        );
        // A stray user `builtin relvar` is likewise not a decl-site error; it is
        // inert (unregistered), so a *reference* to it is what fails to resolve.
        let src = "program p; builtin relvar Foo { a: Integer } key { a };";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn user_may_name_a_relvar_environment_without_import() {
        // No reserved words: without importing `coddl::env`, `Environment` is a
        // free name a user may claim for their own relvar.
        let src = "program p; \
                   private relvar Environment { name: Text } key { name }; \
                   oper main {} [];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn importing_env_makes_environment_collide_with_user_relvar_t0012() {
        // With `coddl::env` imported, `Environment` is defined — a same-named
        // user relvar is a genuine duplicate (T0012).
        let src = "program p; use module coddl::env; \
                   private relvar Environment { name: Text } key { name };";
        assert!(codes(src).contains(&"T0012"), "{:?}", codes(src));
    }

    #[test]
    fn env_builtin_relvar_is_writable_via_dml() {
        // insert / update / delete on `Environment` are allowed (RelvarKind::Builtin
        // is a writable target); no T0033. No transaction gate either (it's not a
        // public relvar).
        let src = "program p; use module coddl::env; oper main {} [ \
                   insert Environment { { name: \"X\", value: \"y\" } }; \
                   update Environment where name = \"X\" { value: \"z\" }; \
                   delete Environment where name = \"X\"; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn env_builtin_relvar_general_assign_is_rejected_t0033() {
        // The general `R := …` surgical form on a builtin relvar is deferred —
        // use insert/update/delete instead.
        let src = "program p; use module coddl::env; \
                   oper main {} [ Environment := Environment; ];";
        assert!(codes(src).contains(&"T0033"), "{:?}", codes(src));
    }

    #[test]
    fn type_alias_resolves_when_used() {
        // `type Foo = Integer;` — a param typed `Foo` resolves to Integer, so
        // the body's use of it typechecks with no unknown-type error.
        let src = "program p; \
                   type Foo = Integer; \
                   oper f { x: Foo } -> Integer [ x ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn unknown_type_still_errors() {
        let src = "program p; oper f { x: Bar } [];";
        assert!(codes(src).contains(&"T0005"), "{:?}", codes(src));
    }

    #[test]
    fn type_alias_cannot_shadow_builtin() {
        let src = "program p; type Integer = Text;";
        assert!(codes(src).contains(&"T0085"), "{:?}", codes(src));
    }

    #[test]
    fn duplicate_type_alias_errors() {
        let src = "program p; type Foo = Integer; type Foo = Text;";
        assert!(codes(src).contains(&"T0086"), "{:?}", codes(src));
    }

    #[test]
    fn possrep_scalar_selector_and_accessor_check_clean() {
        // A single-possrep scalar: construct via the synthesized selector
        // `Meters { value: e }`, read the component back via the possrep
        // accessor `m.value`. Both typecheck with no diagnostics.
        let src = "program p; \
                   type Meters { value: Integer }; \
                   oper f { m: Meters } -> Integer [ m.value ]; \
                   oper g {} -> Meters [ Meters { value: 42 } ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn possrep_scalar_is_disjoint_from_component_t0009() {
        // A `Meters` value is not a `Text` (RM Pre 1 — distinct scalar types are
        // disjoint), so a body producing a `Meters` can't satisfy a declared
        // `Text` return. (Checked via the loud body-vs-return path; user-oper
        // *parameter* types still resolve quietly, a pre-existing alias gap.)
        let src = "program p; \
                   type Meters { value: Integer }; \
                   oper g {} -> Text [ Meters { value: 42 } ];";
        assert!(codes(src).contains(&"T0009"), "{:?}", codes(src));
    }

    #[test]
    fn multi_component_possrep_is_unsupported_t0091() {
        // Single-component only for now; a two-component possrep is rejected.
        let src = "program p; type Point { x: Integer, y: Integer };";
        assert!(codes(src).contains(&"T0091"), "{:?}", codes(src));
    }

    #[test]
    fn oper_cannot_shadow_possrep_scalar_selector_t0060() {
        // The scalar's name is its selector; a user `oper` can't reuse it.
        let src = "program p; type Meters { value: Integer }; oper Meters { x: Integer } [];";
        assert!(codes(src).contains(&"T0060"), "{:?}", codes(src));
    }

    #[test]
    fn tuple_type_alias_checks_clean() {
        // The prelude's Request shape: a Tuple alias with a nested Relation.
        let src = "program p; \
                   type Request = Tuple { method: Text, \
                   headers: Relation { name: Text, value: Text }, body: Text };";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn minus_with_identical_headings_checks_clean() {
        // R { a, b } minus S { a, b } — identical headings -> ok, result { a, b }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R minus S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn minus_with_differing_headings_diagnoses_t0038() {
        // S has `c` where R has `b` -> headings not identical -> T0038.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R minus S }; ];";
        assert!(codes(src).contains(&"T0038"), "{:?}", codes(src));
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

    // ── Sequence literals (let-value-only) ──────────────────────────

    #[test]
    fn sequence_let_infers_element_type() {
        let src = "oper main {} [ let _s = Sequence [ 1, 2, 3 ]; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let hint = out
            .hints
            .iter()
            .find(|h| h.kind == HintKind::LetBinding)
            .expect("expected a LetBinding hint");
        assert!(
            matches!(&hint.ty, Type::Sequence(e) if **e == Type::Integer),
            "expected `Sequence Integer`, got {}",
            hint.ty
        );
    }

    #[test]
    fn sequence_let_element_mismatch_emits_t0062() {
        let src = "oper main {} [ let _s = Sequence [ 1, \"x\" ]; ];";
        assert!(codes(src).contains(&"T0062"), "got {:?}", codes(src));
    }

    #[test]
    fn empty_sequence_without_annotation_emits_t0061() {
        let src = "oper main {} [ let _s = Sequence []; ];";
        assert!(codes(src).contains(&"T0061"), "got {:?}", codes(src));
    }

    #[test]
    fn empty_sequence_with_annotation_checks_clean() {
        let src = "oper main {} [ let _s: Sequence Integer = Sequence []; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    #[test]
    fn sequence_annotation_vs_rhs_mismatch_emits_t0010() {
        // RHS infers `Sequence Text`; annotation says `Sequence Integer`.
        let src = "oper main {} [ let _s: Sequence Integer = Sequence [ \"x\" ]; ];";
        assert!(codes(src).contains(&"T0010"), "got {:?}", codes(src));
    }

    #[test]
    fn sequence_literal_outside_let_emits_t0063() {
        // A sequence literal as a bare expression (not a `let` value) is
        // rejected by the let-value-only rule.
        let src = "oper main {} [ Sequence [ 1 ]; ];";
        assert!(codes(src).contains(&"T0063"), "got {:?}", codes(src));
    }

    #[test]
    fn sequence_index_yields_element_type() {
        // `s[i]` on a `Sequence Text` has type `Text` (the element type).
        let src = "oper main {} [ let _s = Sequence [ \"a\", \"b\" ]; let _e = _s[1]; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        // Two LetBinding hints: `_s : Sequence Text` and `_e : Text`.
        assert!(
            out.hints
                .iter()
                .any(|h| h.kind == HintKind::LetBinding && h.ty == Type::Text),
            "expected a `Text` hint for the index result, got {:?}",
            out.hints.iter().map(|h| &h.ty).collect::<Vec<_>>()
        );
    }

    #[test]
    fn indexing_non_sequence_emits_t0065() {
        // The operand is an `Integer`, not a `Sequence`.
        let src = "oper main {} [ let _s = 5; let _e = _s[0]; ];";
        assert!(codes(src).contains(&"T0065"), "got {:?}", codes(src));
    }

    #[test]
    fn non_integer_index_emits_t0066() {
        // The index is a `Text`, not an `Integer`.
        let src = "oper main {} [ let _s = Sequence [ \"a\" ]; let _e = _s[\"k\"]; ];";
        assert!(codes(src).contains(&"T0066"), "got {:?}", codes(src));
    }

    // ── `if <cond> then [ … ] else [ … ]` ────────────────────────────

    #[test]
    fn if_with_else_typechecks_clean() {
        // The `if` is the tail expression; both arms are Integer, matching
        // the declared return.
        let src = "oper f {} -> Integer [ if true then [ 1 ] else [ 2 ] ]; oper main {} [];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn if_non_boolean_condition_emits_t0067() {
        let src = "oper main {} [ let _b = if 5 then [ 1 ] else [ 2 ]; ];";
        assert!(codes(src).contains(&"T0067"), "got {:?}", codes(src));
    }

    #[test]
    fn if_arm_type_mismatch_emits_t0068() {
        // then is Integer, else is Text.
        let src = "oper main {} [ let _b = if true then [ 1 ] else [ \"x\" ]; ];";
        assert!(codes(src).contains(&"T0068"), "got {:?}", codes(src));
    }

    #[test]
    fn if_without_else_non_unit_then_emits_t0069() {
        // No else, but the then-arm is Integer (not Unit).
        let src = "oper main {} [ if true then [ 1 ]; ];";
        assert!(codes(src).contains(&"T0069"), "got {:?}", codes(src));
    }

    #[test]
    fn if_without_else_unit_then_clean() {
        // No else, then-arm value is Unit (`{}`) — the statement form.
        let src = "oper main {} [ if true then [ {} ]; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    // ── UFCS method calls (`x.m {}` ≡ `m { self: x }`) ───────────────

    #[test]
    fn ufcs_cardinality_on_sequence_resolves() {
        // `xs.cardinality {}` picks the `AnySequence` overload → Integer.
        let src = "oper main {} [ let xs = Sequence [ \"a\", \"b\" ]; let _n = xs.cardinality {}; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn ufcs_user_oper_method_resolves() {
        // `"hi".greet {}` ≡ `greet { self: "hi" }`.
        let src = "oper greet { self: Text } -> Text [ self ]; \
                   oper main {} [ let _g = \"hi\".greet {}; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn ufcs_multi_arg_binds_self_from_receiver() {
        // `self` comes from the receiver, `other` from the braces.
        let src = "oper same { self: Integer, other: Integer } -> Boolean [ self = other ]; \
                   oper main {} [ let _b = 5.same { other: 3 }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn ufcs_method_without_self_param_emits_t0070() {
        let src = "oper noself {} -> Integer [ 1 ]; \
                   oper main {} [ let _n = 5.noself {}; ];";
        assert!(codes(src).contains(&"T0070"), "got {:?}", codes(src));
    }

    #[test]
    fn ufcs_receiver_type_mismatch_emits_t0054() {
        // `cardinality`'s overloads accept Relation/Sequence, not Integer.
        let src = "oper main {} [ let _n = 5.cardinality {}; ];";
        assert!(codes(src).contains(&"T0054"), "got {:?}", codes(src));
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
    fn empty_relation_lit_is_relfalse() {
        // `Relation {}` is the nullary empty relation `relfalse` — a valid value,
        // no longer rejected. Its sibling `reltrue` is `Relation { {} }`.
        let src = "oper main {} [ let _r = Relation {}; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn headed_empty_relation_takes_annotation_heading() {
        // With a `Relation { H }` annotation, an empty `Relation {}` is the empty
        // relation *of that heading* — it conforms (no T0010), not relfalse.
        let src = "oper main {} [ let _r: Relation { name: Text } = Relation {}; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn tuple_annotation_typechecks() {
        let src = "oper main {} [ let _t: Tuple { a: Integer } = {a: 1}; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn headed_empty_relation_annotation_mismatch_diagnoses_t0010() {
        // A *non-empty* literal ignores the expected heading and infers from its
        // tuples; a heading that differs from the annotation is a conformance
        // error (T0010).
        let src = "oper main {} [ let _r: Relation { a: Integer } = Relation { {a: 1, b: 2} }; ];";
        assert!(codes(src).contains(&"T0010"), "{:?}", diagnostics(src));
    }

    #[test]
    fn relation_typed_oper_param_is_accepted() {
        // A `Relation { H }` operator parameter is now supported (a relation
        // crosses the ABI as a single pointer) — no T0018.
        let src = "oper f { r: Relation { a: Integer } } [ let _x = 1; ];";
        assert!(!codes(src).contains(&"T0018"), "{:?}", diagnostics(src));
    }

    #[test]
    fn tuple_typed_oper_param_is_accepted() {
        // A `Tuple { H }` operator parameter is supported (flattens per-attribute).
        let src = "oper f { t: Tuple { a: Integer } } [ let _x = t.a; ];";
        assert!(!codes(src).contains(&"T0018"), "{:?}", diagnostics(src));
    }

    #[test]
    fn relation_typed_oper_return_is_accepted() {
        // A `Relation { H }` return is supported (payload pointer + escape retain).
        let src = "oper f {} -> Relation { a: Integer } [ Relation { {a: 1} } ];";
        assert!(!codes(src).contains(&"T0018"), "{:?}", diagnostics(src));
    }

    #[test]
    fn whole_tuple_return_is_accepted() {
        // A whole-`Tuple` return now type-checks (lowering boxes it) — T0018
        // retired. Both a small and a wide tuple return are fine here.
        let src = "oper f {} -> Tuple { a: Integer } [ {a: 1} ];";
        assert!(!codes(src).contains(&"T0018"), "{:?}", diagnostics(src));
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
    fn character_equality_typechecks() {
        // `=` and `<>` accept matching Character operands (result Boolean); no T0021.
        let src = "oper main {} [ let _a = 'x' = 'y'; let _b = 'x' <> 'y'; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn character_integer_mismatch_diagnoses_t0021() {
        // `'x' = 1` mixes Character with Integer — T0021 fires.
        let src = "oper main {} [ let _b = 'x' = 1; ];";
        assert!(codes(src).contains(&"T0021"));
    }

    #[test]
    fn approximate_equality_typechecks() {
        // `=` and `<>` accept matching Approximate operands (result Boolean); no T0021.
        let src = "oper main {} [ let _a = 1.5e0 = 2.5e0; let _b = 1.5e0 <> 2.5e0; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn approximate_integer_mismatch_diagnoses_t0021() {
        // `1.5e0 = 1` mixes Approximate with Integer — T0021 fires.
        let src = "oper main {} [ let _b = 1.5e0 = 1; ];";
        assert!(codes(src).contains(&"T0021"));
    }

    #[test]
    fn rational_equality_typechecks() {
        // `=` and `<>` accept matching Rational operands (result Boolean); no T0021.
        let src = "oper main {} [ let _a = 3.4 = 1.5; let _b = 3.4 <> 1.5; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn rational_integer_mismatch_diagnoses_t0021() {
        // `3.4 = 1` mixes Rational with Integer — T0021 fires.
        let src = "oper main {} [ let _b = 3.4 = 1; ];";
        assert!(codes(src).contains(&"T0021"));
    }

    #[test]
    fn rational_ordering_typechecks() {
        // `< > <= >=` accept matching Rational operands (result Boolean); no T0021.
        let src = "oper main {} [ let _a = 1.5 < 3.4; let _b = 3.4 >= 1.5; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn ordering_mixed_integer_rational_diagnoses_t0021() {
        // `1 < 3.4` mixes Integer with Rational — ordering forbids the mix.
        let src = "oper main {} [ let _b = 1 < 3.4; ];";
        assert!(codes(src).contains(&"T0021"));
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

    // ── arithmetic & concatenation ───────────────────────────────────

    #[test]
    fn integer_arithmetic_typechecks_clean() {
        // `+ - *` and `div` (truncating) on Integer operands are Integer-typed;
        // exact `/` yields a Rational. All diagnostic-free.
        let src = "oper main {} [ \
                   let _a = 1 + 2; \
                   let _b = 5 - 3; \
                   let _c = 4 * 6; \
                   let _d = 5 div 2; \
                   let _e = 5 / 2; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn exact_division_is_rational_int_div_is_integer() {
        // `/` on Integers is exact → Rational; `div` is truncating → Integer.
        // `1/2 = 0.5` (both Rational) and `7 div 2 = 3` (both Integer) check clean.
        let src = "oper main {} [ let _a = 1 / 2 = 0.5; let _b = 7 div 2 = 3; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn rational_conversions_bridge_the_types() {
        // `to_approximate: Rational → Approximate`; `to_rational: Integer → Rational`.
        // No implicit mixing, so `to_rational` is how an Integer joins a rational
        // sum: `to_rational { self: 1 } + 1/2` is Rational + Rational.
        let src = "oper main {} [ \
                   let _a = to_approximate { self: 1/2 } = 0.5e0; \
                   let _r = to_rational { self: 1 } + 1/2; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn mixed_integer_rational_arithmetic_diagnoses_t0043() {
        // No implicit coercion: `1 + 1/2` mixes Integer and Rational — T0043.
        let src = "oper main {} [ let _b = 1 + 1/2; ];";
        assert!(codes(src).contains(&"T0043"));
    }

    #[test]
    fn arithmetic_on_non_integer_diagnoses_t0043() {
        let src = "oper main {} [ let b = 1 + \"x\"; ];";
        assert!(codes(src).contains(&"T0043"));
    }

    #[test]
    fn concat_on_text_and_character_typechecks_clean() {
        // Text||Text, Text||Character, Character||Character all yield Text.
        let src = "oper main {} [ \
                   let _a = \"a\" || \"b\"; \
                   let _b = \"a\" || 'b'; \
                   let _c = 'a' || 'b'; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn concat_on_integer_diagnoses_t0044() {
        let src = "oper main {} [ let b = 1 || 2; ];";
        assert!(codes(src).contains(&"T0044"));
    }

    #[test]
    fn arithmetic_in_where_predicate_typechecks_clean() {
        // The heading attributes `a`/`b` are Integer; `a + b > 2` is a
        // Boolean predicate over them — runs in-process.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let _s = r where a + b > 2; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
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

    // ── replace ───────────────────────────────────────────────────────

    #[test]
    fn replace_bare_ref_diagnoses_t0047() {
        // A bare attribute reference only relabels — that's `rename`, not
        // `replace`.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r replace {x: a}; \
                   ];";
        assert!(codes(src).contains(&"T0047"));
    }

    #[test]
    fn replace_constant_value_diagnoses_t0042() {
        // A constant value references no attribute → removes nothing → use extend.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r replace {flag: true}; \
                   ];";
        assert!(codes(src).contains(&"T0042"));
    }

    #[test]
    fn replace_boolean_value_diagnoses_t0046() {
        // A general value's type is restricted to Integer/Text; a comparison
        // (Boolean) is rejected — same rule as `extend`.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r replace {t: a = b}; \
                   ];";
        assert!(codes(src).contains(&"T0046"));
    }

    #[test]
    fn replace_general_collapse_adds_and_consumes() {
        // `replace { c: a + b }` adds `c` and removes `a`, `b`. `extract(...).c`
        // resolves (c added); accessing the consumed `a` is T0017 (proving it
        // was removed).
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} } replace {c: a + b}; \
                   let t = extract r; \
                   let _c = t.c; \
                   ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
        let gone = "oper main {} [ \
                    let r = Relation { {a: 1, b: 2} } replace {c: a + b}; \
                    let t = extract r; \
                    let _a = t.a; \
                    ];";
        assert!(codes(gone).contains(&"T0017"));
    }

    #[test]
    fn replace_in_place_keeps_attribute() {
        // `replace { a: a + 1 }` updates `a` in place — `a` survives.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} } replace {a: a + 1}; \
                   let t = extract r; \
                   let _a = t.a; \
                   let _b = t.b; \
                   ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn replace_general_reading_no_attribute_diagnoses_t0042() {
        // A general value that references no operand attribute removes nothing.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r replace {c: 1 + 1}; \
                   ];";
        assert!(codes(src).contains(&"T0042"));
    }

    #[test]
    fn replace_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 replace {c: a + b}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    // ── rename ────────────────────────────────────────────────────────

    #[test]
    fn rename_remaps_the_heading() {
        // {a, b} rename {x: a}: `x` is accessible, `a` is gone (T0017).
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2} }; \
                  let t = extract (r rename {x: a}); \
                  let _v = t.x; let _w = t.b; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
        let gone = "oper main {} [ \
                    let r = Relation { {a: 1, b: 2} }; \
                    let t = extract (r rename {x: a}); \
                    let v = t.a; \
                    ];";
        assert!(codes(gone).contains(&"T0017"));
    }

    #[test]
    fn rename_unknown_source_diagnoses_t0029() {
        // The value (source) `nope` doesn't exist in the heading.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r rename {x: nope}; \
                   ];";
        assert!(codes(src).contains(&"T0029"));
    }

    #[test]
    fn rename_computed_value_diagnoses_t0030() {
        // A computed value isn't a relabel — that's `replace`, not `rename`.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r rename {x: a + 1}; \
                   ];";
        assert!(codes(src).contains(&"T0030"));
    }

    #[test]
    fn rename_target_collision_diagnoses_t0031() {
        // b ← a, but b already exists → not a bijection.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r rename {b: a}; \
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn rename_duplicate_source_diagnoses_t0031() {
        // `a` is the source for both `x` and `y` → renamed more than once.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r rename {x: a, y: a}; \
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn rename_swap_is_a_valid_bijection() {
        // {a, b} rename {b: a, a: b} swaps names — no collision.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let _s = r rename {b: a, a: b}; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn rename_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 rename {a: b}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    // ── wrap / unwrap ───────────────────────────────────────────────────

    #[test]
    fn wrap_groups_attrs_into_a_tuple_valued_attribute() {
        // {a, b, c} wrap {t: {a, b}}: `t` is accessible (a tuple), `a`/`b` are
        // gone (consumed), `c` survives.
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2, c: 3} }; \
                  let s = r wrap {t: {a, b}}; \
                  let u = extract s; let _t = u.t; let _c = u.c; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
        let gone = "oper main {} [ \
                    let r = Relation { {a: 1, b: 2, c: 3} }; \
                    let s = r wrap {t: {a, b}}; \
                    let u = extract s; let _a = u.a; \
                    ];";
        assert!(codes(gone).contains(&"T0017"));
    }

    #[test]
    fn wrap_unknown_attr_diagnoses_t0027() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r wrap {t: {nope}}; \
                   ];";
        assert!(codes(src).contains(&"T0027"));
    }

    #[test]
    fn wrap_same_attr_twice_diagnoses_t0028() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r wrap {t: {a}, u: {a}}; \
                   ];";
        assert!(codes(src).contains(&"T0028"));
    }

    #[test]
    fn wrap_new_name_collides_diagnoses_t0031() {
        // new name `c` collides with a surviving attribute `c`.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2, c: 3} }; \
                   let s = r wrap {c: {a, b}}; \
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn unwrap_expands_a_tuple_valued_attribute() {
        // wrap then unwrap round-trips: after unwrap, `a`/`b` are back, `t` gone.
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2, c: 3} }; \
                  let s = r wrap {t: {a, b}} unwrap {t}; \
                  let u = extract s; let _a = u.a; let _b = u.b; let _c = u.c; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
    }

    #[test]
    fn unwrap_non_tuple_diagnoses_t0048() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r unwrap {a}; \
                   ];";
        assert!(codes(src).contains(&"T0048"));
    }

    #[test]
    fn unwrap_component_collision_diagnoses_t0031() {
        // {a, t: Tuple{a, b}} unwrap {t}: lifting `a` collides with the surviving
        // top-level `a`.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, t: {a: 9, b: 8}} }; \
                   let s = r unwrap {t}; \
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn wrap_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 wrap {t: {a}}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    #[test]
    fn unwrap_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 unwrap {t}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    // ── extend ────────────────────────────────────────────────────────

    #[test]
    fn extend_adds_computed_integer_attribute() {
        // `c: a + b` adds an Integer attribute `c`; `extract(...).c` resolves,
        // proving `c` is in the result heading (and a/b survive).
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} } extend {c: a + b}; \
                   let t = extract r; \
                   let _n = t.c; \
                   let _a = t.a; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn extend_concat_adds_text_attribute() {
        let src = "oper main {} [ \
                   let r = Relation { {x: \"a\", y: \"b\"} } extend {z: x || y}; \
                   let t = extract r; \
                   let _z = t.z; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn extend_collision_with_existing_attribute_diagnoses_t0045() {
        let src = "oper main {} [ let s = Relation { {a: 1, b: 2} } extend {a: b}; ];";
        assert!(codes(src).contains(&"T0045"));
    }

    #[test]
    fn extend_duplicate_target_diagnoses_t0045() {
        let src = "oper main {} [ let s = Relation { {a: 1} } extend {c: a, c: a}; ];";
        assert!(codes(src).contains(&"T0045"));
    }

    #[test]
    fn extend_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 extend {c: a}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    #[test]
    fn extend_value_unknown_attr_diagnoses_t0001() {
        let src = "oper main {} [ let s = Relation { {a: 1} } extend {c: nope + 1}; ];";
        assert!(codes(src).contains(&"T0001"));
    }

    #[test]
    fn extend_boolean_value_diagnoses_t0046() {
        // Only Integer/Text are representable as relation cells in v1; a
        // Boolean-valued extend (a comparison) is rejected.
        let src = "oper main {} [ let s = Relation { {a: 1, b: 2} } extend {c: a = b}; ];";
        assert!(codes(src).contains(&"T0046"));
    }

    #[test]
    fn extend_integer_and_text_values_clean() {
        let src = "oper main {} [ \
                   let _i = Relation { {a: 1} } extend {c: a * 2}; \
                   let _t = Relation { {x: \"a\"} } extend {y: x || \"!\"}; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    // ── tclose ────────────────────────────────────────────────────────

    #[test]
    fn tclose_on_binary_same_typed_relation_checks_clean() {
        // {from, to} are both Integer — a binary same-typed graph relation.
        let src = "oper main {} [ \
                   let g = Relation { {from: 1, to: 2}, {to: 3, from: 2} }; \
                   let _c = g tclose; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn tclose_braces_pick_two_columns_checks_clean() {
        // A ternary relation narrowed to two same-typed attributes by the
        // brace-list — sugar for `(g project { major, minor }) tclose`.
        let src = "oper main {} [ \
                   let g = Relation { {major: 1, minor: 2, qty: 5} }; \
                   let _c = g tclose { major, minor }; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn tclose_non_binary_relation_diagnoses_t0041() {
        // Three attributes, no brace-list → not a binary relation.
        let src = "oper main {} [ \
                   let g = Relation { {a: 1, b: 2, c: 3} }; \
                   let _c = g tclose; \
                   ];";
        assert!(codes(src).contains(&"T0041"));
    }

    #[test]
    fn tclose_different_attr_types_diagnoses_t0041() {
        // Binary but the two attributes differ in type (Integer vs Text).
        let src = "oper main {} [ \
                   let g = Relation { {from: 1, to: \"x\"} }; \
                   let _c = g tclose; \
                   ];";
        assert!(codes(src).contains(&"T0041"));
    }

    #[test]
    fn tclose_unknown_attr_in_braces_diagnoses_t0027() {
        let src = "oper main {} [ \
                   let g = Relation { {from: 1, to: 2} }; \
                   let _c = g tclose { from, nope }; \
                   ];";
        assert!(codes(src).contains(&"T0027"));
    }

    #[test]
    fn tclose_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 tclose; ];";
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

    // ── Counted `for` loop ───────────────────────────────────────────

    #[test]
    fn for_counted_typechecks_clean() {
        // The counter is Integer and in scope in the body; the bounds are
        // Integer. No errors.
        let src = "oper main {} [ for i := 0 to 2 do [ let _x = i + 1; ]; ];";
        let d = diagnostics(src);
        assert!(
            d.iter().all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn for_counter_unused_does_not_warn_t0032() {
        // A counted loop may legitimately ignore its counter — the ForCounter
        // origin is exempt from the unused-binding warning.
        let src = "oper main {} [ for i := 0 to 2 do [ write_line { message: \"hi\" }; ]; ];";
        assert!(!codes(src).contains(&"T0032"), "{:?}", diagnostics(src));
    }

    #[test]
    fn for_non_integer_bound_diagnoses_t0071() {
        let src = "oper main {} [ for i := \"x\" to 2 do [ let _x = i; ]; ];";
        assert!(codes(src).contains(&"T0071"), "{:?}", diagnostics(src));
    }

    #[test]
    fn for_counter_is_immutable_t0072() {
        let src = "oper main {} [ for i := 0 to 2 do [ i := 5; ]; ];";
        let c = codes(src);
        assert!(
            c.contains(&"T0072"),
            "expected T0072, got {:?}",
            diagnostics(src)
        );
        // The dedicated code fires — not the generic non-relvar assignment T0033.
        assert!(
            !c.contains(&"T0033"),
            "should not fall through to T0033: {:?}",
            diagnostics(src)
        );
    }

    // ── `while` / `do … while` loops ─────────────────────────────────

    #[test]
    fn while_loop_typechecks() {
        let src = "oper main {} [ var j := 0; while j < 3 do [ j := j + 1; ]; let _y = j; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn while_non_boolean_condition_diagnoses_t0080() {
        let src = "oper main {} [ var j := 0; while j + 1 do [ j := j + 1; ]; ];";
        assert!(codes(src).contains(&"T0080"), "{:?}", diagnostics(src));
    }

    #[test]
    fn do_while_loop_typechecks() {
        let src = "oper main {} [ var k := 0; do [ k := k + 1; ] while k < 3; let _y = k; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn do_while_non_boolean_condition_diagnoses_t0080() {
        let src = "oper main {} [ var k := 0; do [ k := k + 1; ] while k; ];";
        assert!(codes(src).contains(&"T0080"), "{:?}", diagnostics(src));
    }

    #[test]
    fn do_while_body_runs_so_var_is_definitely_assigned() {
        // The post-test body runs at least once, so an outer `var x;`
        // unconditionally assigned in the body is definitely assigned when the
        // trailing condition reads it and afterward — no read-before-assign
        // (T0079).
        let src = "oper main {} [ var x; do [ x := 5; ] while x < 10; let _y = x; ];";
        assert!(!codes(src).contains(&"T0079"), "{:?}", diagnostics(src));
    }

    #[test]
    fn while_body_may_skip_so_var_stays_unassigned() {
        // The pre-test body may run zero times, so a `var x;` it assigns is NOT
        // definitely assigned after the loop — reading it afterward is T0079.
        // The condition reads only `g` (initialized), isolating the after-loop read.
        let src =
            "oper main {} [ var x; var g := 0; while g < 0 do [ x := 5; g := g + 1; ]; let _y = x; ];";
        assert!(codes(src).contains(&"T0079"), "{:?}", diagnostics(src));
    }

    // ── mutable `var` bindings + reassignment ────────────────────────

    #[test]
    fn var_reassignment_is_allowed() {
        let src = "oper main {} [ var x := 1; x := 2; let _y = x; ];";
        let c = codes(src);
        assert!(!c.contains(&"T0074"), "reassigning a `var` is legal: {:?}", diagnostics(src));
        assert!(!c.contains(&"T0075"), "same-type value is fine: {:?}", diagnostics(src));
    }

    #[test]
    fn reassigning_let_binding_is_t0074() {
        let src = "oper main {} [ let x = 1; x := 2; ];";
        let c = codes(src);
        assert!(c.contains(&"T0074"), "expected T0074, got {:?}", diagnostics(src));
        // The dedicated code fires — not the generic non-relvar assignment T0033.
        assert!(!c.contains(&"T0033"), "should not fall through to T0033: {:?}", diagnostics(src));
    }

    #[test]
    fn var_reassignment_type_mismatch_is_t0075() {
        let src = "oper main {} [ var x := 1; x := \"s\"; ];";
        assert!(codes(src).contains(&"T0075"), "expected T0075, got {:?}", diagnostics(src));
    }

    #[test]
    fn unused_var_binding_warns_t0032() {
        // A `var` warns unused like a `let`; a write does not count as a use.
        let src = "oper main {} [ var x := 1; x := 2; ];";
        assert!(codes(src).contains(&"T0032"), "{:?}", diagnostics(src));
    }

    #[test]
    fn underscore_prefixed_var_is_exempt() {
        let src = "oper main {} [ var _x := 1; _x := 2; ];";
        assert!(!codes(src).contains(&"T0032"), "{:?}", diagnostics(src));
    }

    #[test]
    fn var_occurrences_recorded_as_mutable_spans() {
        // decl (`var x`) + one read (RHS `x`) + one write (target `x`) = 3.
        let src = "oper main {} [ var x := 1; x := x; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        assert_eq!(out.mutable_spans.len(), 3, "decl + read + write; got {:?}", out.mutable_spans);
    }

    #[test]
    fn let_binding_has_no_mutable_spans() {
        let src = "oper main {} [ let x = 1; let _y = x; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(out.mutable_spans.is_empty(), "an immutable `let` is not mutable: {:?}", out.mutable_spans);
    }

    #[test]
    fn var_read_but_never_reassigned_suggests_let_t0077() {
        // The analog of Rust's `unused_mut`: read, never written → use `let`.
        let src = "oper main {} [ var x := 1; let _y = x; ];";
        let d = diagnostics(src);
        let t = d.iter().find(|d| d.code == "T0077").expect("expected T0077");
        assert_eq!(t.severity, coddl_diagnostics::Severity::Warning);
    }

    #[test]
    fn reassigned_var_does_not_suggest_let() {
        let src = "oper main {} [ var x := 1; x := 2; let _y = x; ];";
        assert!(!codes(src).contains(&"T0077"), "a genuinely mutable var: {:?}", diagnostics(src));
    }

    #[test]
    fn underscore_var_never_reassigned_is_exempt_from_t0077() {
        let src = "oper main {} [ var _x := 1; let _y = _x; ];";
        assert!(!codes(src).contains(&"T0077"), "{:?}", diagnostics(src));
    }

    // ── uninitialized `var` + definite assignment ────────────────────

    #[test]
    fn uninitialized_let_is_t0078() {
        let src = "oper main {} [ let x: Integer; ];";
        assert!(codes(src).contains(&"T0078"), "expected T0078, got {:?}", diagnostics(src));
    }

    #[test]
    fn uninitialized_var_assigned_then_read_is_clean() {
        // `var x;` with no annotation: the type is inferred from `x := 1`, and
        // the read follows the assignment — no T0078/T0079.
        let src = "oper main {} [ var x; x := 1; let _y = x; ];";
        let d = diagnostics(src);
        assert!(
            d.iter().all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn read_before_assignment_is_t0079() {
        let src = "oper main {} [ var x; let _y = x; ];";
        assert!(codes(src).contains(&"T0079"), "expected T0079, got {:?}", diagnostics(src));
    }

    #[test]
    fn both_arms_assign_then_read_is_clean() {
        // Full definite-assignment: assigned on every branch ⇒ assigned after.
        let src = "oper main {} [ var x; if true then [ x := 1; ] else [ x := 2; ]; let _y = x; ];";
        let d = diagnostics(src);
        assert!(
            d.iter().all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn assign_in_only_one_arm_then_read_is_t0079() {
        // No `else` ⇒ the then-arm may not run ⇒ not definitely assigned after.
        let src = "oper main {} [ var x; if true then [ x := 1; ]; let _y = x; ];";
        assert!(codes(src).contains(&"T0079"), "expected T0079, got {:?}", diagnostics(src));
    }

    #[test]
    fn assign_in_loop_body_then_read_after_is_t0079() {
        // A loop may run zero times ⇒ its body's assignments aren't definite.
        let src = "oper main {} [ var x; for i := 1 to 3 do [ x := i; ]; let _y = x; ];";
        assert!(codes(src).contains(&"T0079"), "expected T0079, got {:?}", diagnostics(src));
    }

    #[test]
    fn inferred_type_fixed_by_first_assignment_is_t0075() {
        // `x`'s type is inferred `Integer` from `x := 1`; the `Text` write fails.
        let src = "oper main {} [ var x; x := 1; x := \"s\"; ];";
        assert!(codes(src).contains(&"T0075"), "expected T0075, got {:?}", diagnostics(src));
    }

    #[test]
    fn uninitialized_var_shows_inferred_type_hint_at_declaration() {
        // `var x;` gets its `: Integer` inlay hint at the declaration once the
        // first assignment infers the type — anchored right after the decl `x`,
        // not the assignment.
        let src = "oper main {} [ var x; x := 42; let _y = x; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        let decl_x_end = (src.find("var x").unwrap() + "var x".len()) as u32;
        let hint = out
            .hints
            .iter()
            .find(|h| h.kind == HintKind::LetBinding && h.span.start == decl_x_end)
            .expect("expected an inferred-type hint anchored at `var x`");
        assert!(matches!(hint.ty, Type::Integer), "hint type was {:?}", hint.ty);
    }

    #[test]
    fn load_infers_sequence_of_tuples() {
        // `load names from rnames order [asc name]` fixes the unannotated
        // `var names;` to `Sequence Tuple { name: Text }`, surfaced as a hint,
        // and reports no diagnostics.
        let src = "oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names; \
                   load names from rnames order [asc name]; \
                   let _c = names; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let want = Type::Sequence(Box::new(Type::Tuple(Heading::new(vec![(
            "name".to_string(),
            Type::Text,
        )]))));
        assert!(
            out.hints
                .iter()
                .any(|h| h.kind == HintKind::LetBinding && h.ty == want),
            "expected a `Sequence Tuple {{ name: Text }}` hint, got {:?}",
            out.hints.iter().map(|h| &h.ty).collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_unknown_order_key_is_t0027() {
        // `nope` is not an attribute of the source heading.
        let src = "oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names; \
                   load names from rnames order [asc nope]; ];";
        assert!(codes(src).contains(&"T0027"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_non_relation_source_is_t0081() {
        // The source is an `Integer`, not a `Relation`.
        let src = "oper main {} [ var names; load names from 5; ];";
        assert!(codes(src).contains(&"T0081"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_tuple_valued_order_key_is_t0082() {
        // `t` is a tuple-valued attribute — tuples carry `=`/`<>` only (RM Pro 1),
        // so they have no sort order.
        let src = "oper main {} [ \
                   let r = Relation { { t: { a: 1 } } }; \
                   var xs; \
                   load xs from r order [asc t]; ];";
        assert!(codes(src).contains(&"T0082"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_agrees_with_matching_annotation() {
        // An annotated target whose type matches the inferred sequence is
        // error-free (an unused-`var` warning is fine — not an error).
        let src = "oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names: Sequence Tuple { name: Text }; \
                   load names from rnames order [asc name]; ];";
        let d = diagnostics(src);
        assert!(
            d.iter().all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn load_conflicting_annotation_is_t0075() {
        // The target is annotated `Sequence Integer`; the load produces a
        // `Sequence Tuple {…}`, which doesn't match.
        let src = "oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names: Sequence Integer; \
                   load names from rnames order [asc name]; ];";
        assert!(codes(src).contains(&"T0075"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn reading_load_target_before_load_is_t0079() {
        // `names` is a deferred-init `var names;`; reading it before the `load`
        // that assigns it trips definite assignment.
        let src = "oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names; \
                   let _peek = names; \
                   load names from rnames order [asc name]; ];";
        assert!(codes(src).contains(&"T0079"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_reverse_seals_sequence_into_private_relvar() {
        // A `Sequence Tuple { name: Text }` (from a forward load) seals back into a
        // matching private relvar — no `order`, no errors.
        let src = "program p; \
                   private relvar Names { name: Text } key { name }; \
                   oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names; \
                   load names from rnames order [asc name]; \
                   load Names from names; ];";
        let d = diagnostics(src);
        assert!(
            d.iter().all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn load_reverse_with_order_clause_is_t0083() {
        // A relation is unordered — an `order` clause on the reverse form is a
        // mistake.
        let src = "program p; \
                   private relvar Names { name: Text } key { name }; \
                   oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names; \
                   load names from rnames order [asc name]; \
                   load Names from names order [asc name]; ];";
        assert!(codes(src).contains(&"T0083"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_reverse_heading_mismatch_is_t0075() {
        // The sequence is `Sequence Tuple { name: Text }`; the relvar heading is
        // `{ age: Integer }` — not assignable.
        let src = "program p; \
                   private relvar Ages { age: Integer } key { age }; \
                   oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names; \
                   load names from rnames order [asc name]; \
                   load Ages from names; ];";
        assert!(codes(src).contains(&"T0075"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_reverse_scalar_sequence_is_t0075() {
        // A `Sequence Integer` has no relation form — sealing it into a relvar is a
        // type mismatch.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   oper main {} [ \
                   let s = Sequence [1, 2, 3]; \
                   load R from s; ];";
        assert!(codes(src).contains(&"T0075"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_reverse_into_var_target_is_t0033() {
        // A plain `var` is not a relvar — the reverse form needs a relvar target.
        let src = "oper main {} [ \
                   let s = Sequence [1, 2, 3]; \
                   var x; \
                   load x from s; ];";
        assert!(codes(src).contains(&"T0033"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn load_reverse_into_public_relvar_is_t0084() {
        // Reverse into a public (SQL-backed) relvar is not yet wired.
        let src = "program p; database d; \
                   public relvar R { name: Text } key { name }; \
                   oper main {} [ \
                   let rnames = Relation { { name: \"Alice\" } }; \
                   var names; \
                   load names from rnames order [asc name]; \
                   load R from names; ];";
        assert!(codes(src).contains(&"T0084"), "got {:?}", diagnostics(src));
    }

    #[test]
    fn for_counter_is_scoped_to_body() {
        // `i` is visible only inside the loop body; referencing it afterward is
        // an unresolved name (T0001).
        let src = "oper main {} [ for i := 0 to 2 do [ let _x = i; ]; let _y = i; ];";
        assert!(codes(src).contains(&"T0001"), "{:?}", diagnostics(src));
    }

    #[test]
    fn for_in_over_sequence_typechecks_clean() {
        // The element binds to the sequence's element type (`Text` here) and is
        // in scope in the body.
        let src = "oper main {} [ let names = Sequence [\"a\", \"b\"]; \
                   for name in names do [ write_line { message: name }; ]; ];";
        let d = diagnostics(src);
        assert!(
            d.iter().all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn for_in_over_relation_diagnoses_t0073() {
        // A relation can't be iterated tuple-at-a-time (RM Pro 7); T0073 points
        // at `load … order`.
        let src = "oper main {} [ let r = Relation { {a: 1} }; \
                   for t in r do [ let _x = t; ]; ];";
        assert!(codes(src).contains(&"T0073"), "{:?}", diagnostics(src));
    }

    #[test]
    fn for_in_element_is_immutable_t0072() {
        // The element variable is loop-scoped and immutable, like the counted
        // counter.
        let src = "oper main {} [ let names = Sequence [\"a\", \"b\"]; \
                   for name in names do [ name := \"x\"; ]; ];";
        assert!(codes(src).contains(&"T0072"), "{:?}", diagnostics(src));
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

    // ── string interpolation: `format` + `f"…"` + `to_text` ──────────────

    const FORMAT_HELLO: &str = "program p;\n\
        oper main {} [\n\
            let name_in = read_line { prompt: \"n: \" };\n\
            let message = format { template: f\"Hello, {name}!\", args: { name: name_in } };\n\
            write_line { message };\n\
        ];\n";

    #[test]
    fn format_interpolation_checks_clean() {
        let diags = diagnostics(FORMAT_HELLO);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn format_returns_text() {
        // `message` flows into write_line's Text `message` param with no
        // T0004 — proving format's result type is Text.
        assert!(!codes(FORMAT_HELLO).contains(&"T0004"), "{:?}", codes(FORMAT_HELLO));
    }

    #[test]
    fn fstring_outside_format_template_is_t0055() {
        // The firewall: an f"…" literal anywhere but format's template.
        let src = "program p; oper main {} [ write_line { message: f\"hi\" }; ];";
        assert!(codes(src).contains(&"T0055"), "{:?}", codes(src));
        // A `let`-bound template can't slip into a Text slot either — same
        // firewall, now via the name reference rather than the literal.
        let src2 = "program p; oper main {} [ let t = f\"hi\"; write_line { message: t }; ];";
        assert!(codes(src2).contains(&"T0055"), "{:?}", codes(src2));
    }

    #[test]
    fn fstring_bound_to_let_and_reused_checks_clean() {
        // A template written once and reused in two `format` calls, each with
        // its own `args`. The `{name}` hole need not resolve at the binding
        // site — it is validated per call, not at the `let`.
        let src = "program p; oper main {} [ \
            let t = f\"Hi, {name}!\"; \
            let a = format { template: t, args: { name: \"A\" } }; \
            let b = format { template: t, args: { name: \"B\" } }; \
            write_line { message: a }; write_line { message: b }; \
        ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
    }

    #[test]
    fn let_bound_template_placeholder_mismatch_is_t0058() {
        // Placeholder vs `args` is checked at the call, not the binding.
        let src = "program p; oper main {} [ \
            let t = f\"Hi, {name}!\"; \
            let m = format { template: t, args: { wrong: \"A\" } }; \
            write_line { m }; \
        ];";
        assert!(codes(src).contains(&"T0058"), "{:?}", codes(src));
    }

    #[test]
    fn non_fstring_let_in_template_position_is_t0056() {
        // A `let` bound to a plain Text is not a template — provenance holds.
        let src = "program p; oper main {} [ \
            let t = \"hi {x}\"; \
            let m = format { template: t, args: { x: 1 } }; \
            write_line { m }; \
        ];";
        assert!(codes(src).contains(&"T0056"), "{:?}", codes(src));
    }

    #[test]
    fn format_template_must_be_fstring_literal_t0057() {
        // A plain string in template position is the classic mistake.
        let src = "program p; oper main {} [ let m = format { template: \"hi {x}\", args: { x: 1 } }; write_line { m }; ];";
        assert!(codes(src).contains(&"T0056"), "{:?}", codes(src));
    }

    #[test]
    fn format_placeholder_without_attribute_is_t0059() {
        let src = "program p; oper main {} [ let m = format { template: f\"hi {missing}\", args: { present: 1 } }; write_line { m }; ];";
        let c = codes(src);
        assert!(c.contains(&"T0058"), "{:?}", c);
    }

    #[test]
    fn format_unused_args_attribute_warns_t0060() {
        let src = "program p; oper main {} [ let m = format { template: f\"hi\", args: { unused: 1 } }; write_line { m }; ];";
        let c = codes(src);
        assert!(c.contains(&"T0059"), "{:?}", c);
    }

    #[test]
    fn format_malformed_template_is_t0058() {
        let src = "program p; oper main {} [ let m = format { template: f\"hi {}\", args: {} }; write_line { m }; ];";
        assert!(codes(src).contains(&"T0057"), "{:?}", codes(src));
    }

    #[test]
    fn to_text_overload_resolves_for_text_and_character() {
        // Both overloads are reachable; neither is a no-match (T0054).
        let src = "program p; oper main {} [ let a = to_text { self: \"x\" }; let b = to_text { self: 'y' }; write_line { a }; write_line { b }; ];";
        let c = codes(src);
        assert!(!c.contains(&"T0054"), "{:?}", c);
        assert!(!c.contains(&"T0001"), "{:?}", c);
    }

    #[test]
    fn to_text_no_matching_overload_is_t0054() {
        // No `to_text { self: Relation }` overload exists.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ let t = to_text { self: R }; write_line { t }; ];";
        let c = codes(src);
        assert!(c.contains(&"T0054"), "{:?}", c);
    }

    #[test]
    fn format_placeholder_non_to_text_tuple_is_t0054() {
        // `{t}` desugars to `to_text { self: t }`; a Tuple has no overload,
        // so this is rejected at check time rather than panicking in lowering.
        let src = "program p; oper main {} [ let t = { a: 1 }; \
                   let m = format { template: f\"{t}\", args: { t } }; \
                   write_line { m }; ];";
        assert!(codes(src).contains(&"T0054"), "{:?}", codes(src));
    }

    #[test]
    fn format_placeholder_sequence_is_t0054() {
        // A `Sequence` param interpolated into a template has no `to_text`
        // overload — caught at typecheck, so lowering (and T0064) never runs.
        let src = "program p; oper main {} [ let s = Sequence [ 1, 2 ]; \
                   let m = format { template: f\"{s}\", args: { s } }; \
                   write_line { m }; ];";
        assert!(codes(src).contains(&"T0054"), "{:?}", codes(src));
    }

    #[test]
    fn format_placeholder_scalar_types_have_no_t0054() {
        // Text / Integer / Boolean placeholders all have a `to_text` overload;
        // the happy path must not regress.
        let src = "program p; oper main {} [ \
                   let m = format { template: f\"{a}{b}{c}\", \
                   args: { a: \"x\", b: 1, c: true } }; \
                   write_line { m }; ];";
        assert!(!codes(src).contains(&"T0054"), "{:?}", codes(src));
    }

    // The `write_line { template: FormatText, args: Tuple H }` overload — the
    // format-writing form. It shares `format`'s validation, so the diagnostics
    // below are exactly the ones `format` raises for the same mistake.

    #[test]
    fn write_line_format_overload_checks_clean() {
        // Inline template and a let-bound template both type-check to unit.
        let src = "program p; oper main {} [ \
                   write_line { template: f\"Hello, {name}!\", args: { name: \"X\" } }; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
        let bound = "program p; oper main {} [ \
                     let t = f\"Hello, {name}!\"; \
                     write_line { template: t, args: { name: \"X\" } }; ];";
        assert!(diagnostics(bound).is_empty(), "{:?}", diagnostics(bound));
    }

    #[test]
    fn write_line_format_overload_runtime_text_template_is_t0056() {
        // A plain (runtime) Text in template position is rejected exactly as in
        // a `format` call — the firewall holds through the write_line overload.
        let src = "program p; oper main {} [ \
                   write_line { template: \"hi {x}\", args: { x: 1 } }; ];";
        assert!(codes(src).contains(&"T0056"), "{:?}", codes(src));
    }

    #[test]
    fn write_line_format_overload_non_tuple_args_is_t0004() {
        let src = "program p; oper main {} [ \
                   write_line { template: f\"hi\", args: 5 }; ];";
        assert!(codes(src).contains(&"T0004"), "{:?}", codes(src));
    }

    #[test]
    fn write_line_format_overload_unknown_placeholder_is_t0058() {
        let src = "program p; oper main {} [ \
                   write_line { template: f\"hi {missing}\", args: { present: 1 } }; ];";
        assert!(codes(src).contains(&"T0058"), "{:?}", codes(src));
    }

    #[test]
    fn write_line_format_overload_stray_message_arg_is_t0002() {
        // `message` alongside `template` routes to the format path, where any
        // arg other than template/args is undeclared.
        let src = "program p; oper main {} [ \
                   write_line { template: f\"hi\", message: \"x\" }; ];";
        assert!(codes(src).contains(&"T0002"), "{:?}", codes(src));
    }

    #[test]
    fn write_line_format_overload_in_transaction_is_t0026() {
        // The overload keeps write_line's side-effecting purity: illegal inside
        // a `transaction [...]`, same as the `message` form.
        let src = "program p; oper main {} [ \
                   transaction [ write_line { template: f\"hi\", args: {} }; ]; ];";
        assert!(codes(src).contains(&"T0026"), "{:?}", codes(src));
    }

    #[test]
    fn write_line_message_overload_still_resolves() {
        // The plain form is untouched — no `template` arg means the normal
        // registry path, no T0001/T0002/T0003.
        let src = "program p; oper main {} [ write_line { message: \"hi\" }; ];";
        let c = codes(src);
        assert!(!c.contains(&"T0001"), "{:?}", c);
        assert!(!c.contains(&"T0002"), "{:?}", c);
        assert!(!c.contains(&"T0003"), "{:?}", c);
    }
}

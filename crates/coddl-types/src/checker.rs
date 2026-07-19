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
use coddl_stdlib::ModulePath;
use coddl_syntax::ast::{
    AssignStmt, AstNode, BinaryExpr, BinaryOp, Block, CallExpr, DeleteStmt, DoWhileStmt, Expr,
    ExprStmt, ExtendExpr, FieldAccess, ForStmt, GroupExpr, Heading as AstHeading, IfExpr,
    IndexExpr, InsertStmt, Item, KeyClause, LetStmt, LoadStmt, NameRef, NamedArg, OperDecl,
    PrivateRelvarDecl, ProgramDecl, ProjectExpr, PublicRelvarDecl, RelationLit, RenameExpr,
    ReplaceExpr, ReturnStmt, Root, SequenceLit, Stmt, TcloseExpr, TransactionExpr, TruncateStmt,
    TupleLit, TypeDecl, TypeRef, UnaryExpr, UnaryOp, UngroupExpr, UnwrapExpr, UpdateStmt, VarStmt,
    WhileStmt, WrapExpr,
};
use coddl_syntax::ast_cddb::{BaseRelvarDecl, CddbItem, CddbRoot, RelvarInit, VirtualRelvarDecl};
use coddl_syntax::ast_cdstore::CdstoreRoot;
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
    /// All relvars in scope for this file. For `.cd`: public + private
    /// (and any base/virtual the user mistakenly placed in `.cd`,
    /// which T0014 flags). For `.cddb`: base + virtual (similarly). For
    /// `.cdstore`: the `coddl::storage` builtin relvars, brought in implicitly
    /// (a `.cdstore` is DML over that meta-catalog). Empty for `.cdmap`.
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
    check_inner(source, file, file_kind, HashMap::new(), HashMap::new()).0
}

/// Type-check one unit with `imported_opers` / `imported_lets` seeded from its
/// module imports. Returns the [`CheckOutput`] and the unit's own exports —
/// its top-level `oper` signatures and module-level `let` types — which feed
/// [`check_program`]'s export catalog.
fn check_inner(
    source: &str,
    file: FileId,
    file_kind: FileKind,
    imported_opers: HashMap<String, Vec<(ModulePath, crate::builtins::OperSig)>>,
    imported_lets: HashMap<String, Vec<(ModulePath, Type)>>,
) -> (
    CheckOutput,
    HashMap<String, crate::builtins::OperSig>,
    HashMap<String, Type>,
) {
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
        imported_opers,
        module_lets: HashMap::new(),
        imported_lets,
        stdlib_lets: HashMap::new(),
        type_aliases: HashMap::new(),
        nominal_scalars: HashMap::new(),
        active_modules: HashSet::new(),
        stdlib_oper_owner: HashMap::new(),
        stdlib_type_owner: HashMap::new(),
        stdlib_relvar_owner: HashMap::new(),
        current_return_type: None,
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
        FileKind::Cdstore => {
            if let Some(root) = CdstoreRoot::cast(parse_out.tree) {
                tc.check_cdstore_root(&root);
            }
        }
        FileKind::Cdmap => {
            // Parse-only today; semantic validation lands with Phase 16
            // (the plan layer).
        }
    }
    let exports = tc.user_opers.clone();
    let let_exports = tc.module_lets.clone();
    (
        CheckOutput {
            tree,
            diagnostics: tc.diagnostics,
            hints: tc.hints,
            mutable_spans: tc.mutable_spans,
            relvars: tc.relvars,
            type_aliases: tc.type_aliases,
            nominal_scalars: tc.nominal_scalars,
        },
        exports,
        let_exports,
    )
}

/// One compilation unit for [`check_program`].
pub struct CheckUnit<'a> {
    /// The unit's module path; `None` for the entry `program`/`library`.
    pub module: Option<ModulePath>,
    /// The unit's `.cd` source.
    pub source: &'a str,
    /// The unit's file id — every diagnostic from this unit carries it, so the
    /// driver's source map can render errors against the right file.
    pub file: FileId,
}

/// The result of [`check_program`].
pub struct ProgramCheckOutput {
    /// The entry unit's [`CheckOutput`] (types, relvars, tree). `None` only when
    /// no entry unit (`module: None`) was supplied.
    pub entry: Option<CheckOutput>,
    /// Every diagnostic across all units, each tagged with its unit's `FileId`.
    pub diagnostics: Vec<Diagnostic>,
}

/// Type-check a program as a set of units — the entry `program`/`library` plus
/// the userspace `module`s it transitively imports, **dependency-first** (every
/// module precedes each unit that imports it). Each unit is checked with its
/// direct imports' exported operators in scope (opt-in; a unit's own definitions
/// and builtins shadow imports). Diagnostics from all units are merged.
///
/// `coddl_types` never sees `coddl_plan` (reverse arrow), so the caller (the plan
/// layer) supplies the resolved units; the export catalog is built here from each
/// module's own top-level operators.
pub fn check_program(units: &[CheckUnit]) -> ProgramCheckOutput {
    let mut catalog: HashMap<ModulePath, HashMap<String, crate::builtins::OperSig>> =
        HashMap::new();
    let mut let_catalog: HashMap<ModulePath, HashMap<String, Type>> = HashMap::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut entry: Option<CheckOutput> = None;

    for unit in units {
        // Seed the imported-operator and imported-let scopes from the catalog
        // for this unit's direct `use module` imports. Stdlib (`coddl::*`)
        // paths aren't in the catalog — they're handled by `resolve_modules`
        // inside the unit check.
        let mut imported: HashMap<String, Vec<(ModulePath, crate::builtins::OperSig)>> =
            HashMap::new();
        let mut imported_lets: HashMap<String, Vec<(ModulePath, Type)>> = HashMap::new();
        for path in use_module_paths(unit.source) {
            if let Some(exports) = catalog.get(&path) {
                for (name, sig) in exports {
                    imported
                        .entry(name.clone())
                        .or_default()
                        .push((path.clone(), sig.clone()));
                }
            }
            if let Some(lets) = let_catalog.get(&path) {
                for (name, ty) in lets {
                    imported_lets
                        .entry(name.clone())
                        .or_default()
                        .push((path.clone(), ty.clone()));
                }
            }
        }
        let (out, exports, let_exports) = check_inner(
            unit.source,
            unit.file,
            FileKind::Cd,
            imported,
            imported_lets,
        );
        diagnostics.extend(out.diagnostics.iter().cloned());
        match &unit.module {
            Some(m) => {
                catalog.insert(m.clone(), exports);
                let_catalog.insert(m.clone(), let_exports);
            }
            None => entry = Some(out),
        }
    }

    ProgramCheckOutput { entry, diagnostics }
}

/// The `use module <path>;` paths declared in a `.cd` source, in order. Malformed
/// (empty) paths are skipped — the parser already reported them.
fn use_module_paths(source: &str) -> Vec<ModulePath> {
    let out = parse(source, FileId(0), FileKind::Cd);
    let Some(root) = Root::cast(out.tree) else {
        return Vec::new();
    };
    root.items()
        .filter_map(|item| {
            let Item::UseDecl(u) = item else {
                return None;
            };
            let segs: Vec<String> = u.segments().map(|t| t.text().to_string()).collect();
            (!segs.is_empty()).then(|| ModulePath::new(segs))
        })
        .collect()
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
    /// Operators imported from userspace modules via `use module <leaf>;`, keyed
    /// by name → the `(owning module, signature)` pairs that export it. Held
    /// **separately** from `user_opers` so a unit's own definition (and builtins)
    /// *shadow* a same-named import — this table is consulted only when nothing
    /// local matches — and so two imported modules exporting the same name
    /// coexist until that name is actually called (then **T0092**). Empty for a
    /// single-unit [`check`]; seeded by [`check_program`] from the export catalog
    /// for a unit's direct imports.
    imported_opers: HashMap<String, Vec<(ModulePath, crate::builtins::OperSig)>>,
    /// Module-level `let` bindings (constants), name → bound type. Collected
    /// and checked in a pre-pass (in initializer-dependency order — module
    /// lets are order-independent like every other item; a reference cycle is
    /// T0097), so a use resolves regardless of declaration order. Consulted
    /// by name resolution after the local scope (an oper-local shadows a
    /// module let) and mirrored into the export catalog by
    /// [`check_program`].
    module_lets: HashMap<String, Type>,
    /// Module lets imported via `use module <leaf>;`, keyed by name → the
    /// `(owning module, type)` pairs that export it. Held separately from
    /// `module_lets` for the same reason `imported_opers` is: local names
    /// shadow imports, and two modules exporting the same name coexist until
    /// the name is actually used (then **T0092**).
    imported_lets: HashMap<String, Vec<(ModulePath, Type)>>,
    /// Module-level `let`s of the always-in-scope stdlib (`coddl::core` —
    /// `reltrue`/`relfalse`), name → annotated type. Stdlib lets are
    /// annotated by convention (the annotation is their signature, like a
    /// `builtin oper`'s heading). Consulted **last** among the let tables
    /// (locals → own module lets → imports → these), so any user binding
    /// shadows core's — the no-reserved-words discipline; the module-let
    /// duplicate check never consults this table.
    stdlib_lets: HashMap<String, Type>,
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
    /// The declared return type of the operator whose body is currently being
    /// walked, set by [`Self::check_oper_decl`] around the body. A `return`
    /// statement checks its value against this (T0018); `None` outside any
    /// operator body (no statement position exists there, so it stays a
    /// defensive guard).
    current_return_type: Option<Type>,
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
                    format!(
                        "`{}` is declared `var` but never reassigned; use `let`",
                        b.name
                    ),
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
        // Pre-pass: the always-in-scope stdlib's module-level `let`s
        // (`coddl::core` — reltrue/relfalse). Registered before the user
        // prepass but consulted *after* every user table, so user bindings
        // shadow core's.
        self.absorb_stdlib_lets();
        // Pre-pass: module-level `let` bindings (constants), checked in
        // initializer-dependency order so they are order-independent like
        // every other item (the sibling of the operator forward-reference
        // rule above).
        self.check_module_lets(root);
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
                Item::LetBinding(_) => {
                    // Module lets are collected and checked in the pre-pass
                    // above (dependency order, not source order).
                }
            }
        }
    }

    /// Register the always-in-scope stdlib's module-level `let`s
    /// (`coddl::core`'s `reltrue`/`relfalse`) into `stdlib_lets`. Stdlib
    /// lets are **annotated by convention** — the annotation is their
    /// signature, exactly as a `builtin oper` declaration carries its full
    /// heading — so the type comes from `resolve_type_ref_quiet` with no
    /// diagnostics against the embedded source (core is ours; a missing
    /// annotation there is a compiler bug, debug-asserted). The lowerer
    /// independently lowers the real initializers.
    fn absorb_stdlib_lets(&mut self) {
        let core = coddl_stdlib::resolve(&ModulePath::parse("coddl::core"))
            .expect("coddl::core is always embedded in coddl-stdlib");
        let out = parse(core.source(), FileId(0), FileKind::Cd);
        let Some(root) = Root::cast(out.tree) else {
            return;
        };
        for item in root.items() {
            let Item::LetBinding(b) = item else { continue };
            let Some(name_tok) = b.name() else { continue };
            let ty = b
                .type_ref()
                .map(|tr| resolve_type_ref_quiet(&tr))
                .unwrap_or(Type::Unknown);
            debug_assert!(
                !matches!(ty, Type::Unknown),
                "stdlib module-level `let {}` must carry a type annotation",
                name_tok.text(),
            );
            self.stdlib_lets.insert(name_tok.text().to_string(), ty);
        }
    }

    /// Collect and check every module-level `let` binding. Bindings are
    /// **order-independent** like every other module item: a syntactic walk
    /// over each initializer builds the in-module reference graph, and
    /// checking runs in topological order so a binding's dependencies are
    /// typed (and, later, folded/materialized) before it — purity is what
    /// makes dependency order the only observable order. A reference cycle
    /// is T0097.
    fn check_module_lets(&mut self, root: &Root) {
        let bindings: Vec<LetStmt> = root
            .items()
            .filter_map(|item| match item {
                Item::LetBinding(b) => Some(b),
                _ => None,
            })
            .collect();
        if bindings.is_empty() {
            return;
        }
        // Names first, so the reference graph and duplicate checks see the
        // whole set regardless of order.
        let mut index: HashMap<String, usize> = HashMap::new();
        for (i, b) in bindings.iter().enumerate() {
            let Some(name_tok) = b.name() else { continue };
            let name = name_tok.text().to_string();
            // One namespace per module: a module let can't reuse another
            // module-level name (another let, an oper, a possrep scalar, or
            // a relvar) — same discipline as oper registration (T0060).
            let collides = index.contains_key(&name)
                || self.user_opers.contains_key(&name)
                || self.nominal_scalars.contains_key(&name)
                || self.relvars.get(&name).is_some();
            if collides {
                self.error(
                    self.token_span(&name_tok),
                    "T0060",
                    format!("`{name}` is already defined at module level"),
                );
                continue;
            }
            index.insert(name, i);
        }
        // Reference edges: binding i depends on binding j when i's
        // initializer names j. Purity forbids every binding form inside an
        // initializer, so a bare syntactic walk over NAME_REFs is exact.
        let edges: Vec<Vec<usize>> = bindings
            .iter()
            .map(|b| {
                let Some(value) = b.value() else {
                    return Vec::new();
                };
                let mut deps: Vec<usize> = value
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|el| el.into_node())
                    .filter(|n| n.kind() == SyntaxKind::NAME_REF)
                    .filter_map(|n| NameRef::cast(n)?.ident())
                    .filter_map(|tok| index.get(tok.text()).copied())
                    .collect();
                deps.sort_unstable();
                deps.dedup();
                deps
            })
            .collect();
        // Depth-first topological sort; a back edge is a reference cycle.
        #[derive(Clone, Copy, PartialEq)]
        enum State {
            Unvisited,
            InProgress,
            Done,
        }
        fn visit(
            i: usize,
            edges: &[Vec<usize>],
            state: &mut [State],
            order: &mut Vec<usize>,
        ) -> Result<(), usize> {
            match state[i] {
                State::Done => return Ok(()),
                State::InProgress => return Err(i),
                State::Unvisited => {}
            }
            state[i] = State::InProgress;
            for &dep in &edges[i] {
                visit(dep, edges, state, order)?;
            }
            state[i] = State::Done;
            order.push(i);
            Ok(())
        }
        let mut state = vec![State::Unvisited; bindings.len()];
        let mut order: Vec<usize> = Vec::new();
        for i in 0..bindings.len() {
            if let Err(at) = visit(i, &edges, &mut state, &mut order) {
                let name = bindings[at]
                    .name()
                    .map(|t| t.text().to_string())
                    .unwrap_or_else(|| "?".to_string());
                self.error(
                    self.node_span(bindings[at].syntax()),
                    "T0097",
                    format!("module-level `let` bindings form a reference cycle through `{name}`"),
                );
                // Recover: mark the whole strongly-connected tangle done so
                // one cycle reports once, not once per member.
                for s in state.iter_mut() {
                    if *s == State::InProgress {
                        *s = State::Done;
                    }
                }
            }
        }
        for i in order {
            self.check_module_let(&bindings[i]);
        }
    }

    /// Check one module-level `let`: mandatory constant-expression
    /// initializer (T0098), then the shared binding discipline
    /// (`check_binding_rhs` — annotation as the expected type for empty
    /// constructor literals, T0010 conformance, inlay hint), landing in
    /// `module_lets` instead of a scope.
    fn check_module_let(&mut self, binding: &LetStmt) {
        let name = binding.name();
        let declared = binding.type_ref().map(|tr| self.resolve_type_ref(&tr));
        let value = binding.value();
        let Some(value_expr) = &value else {
            self.error(
                self.node_span(binding.syntax()),
                "T0098",
                "a module-level `let` requires an initializer — it is a constant binding",
            );
            return;
        };
        self.check_module_let_purity(value_expr);
        let mut scope = Scope::default();
        let bound_ty =
            self.check_binding_rhs(declared, &name, &value, binding.syntax(), "let", &mut scope);
        if let Some(name_tok) = name {
            // Registered even after a purity error (with the checked type),
            // so downstream uses resolve and don't cascade T0001s.
            self.module_lets
                .insert(name_tok.text().to_string(), bound_ty);
        }
    }

    /// The module-let purity walk: an initializer must be a **constant
    /// expression** — literals, tuple/relation/sequence literals, and
    /// built-in operators over them, with names restricted to other module
    /// lets. Everything effectful or not-yet-foldable is rejected here with
    /// T0098 (calls stay out until purity derivation lands; relvar reads are
    /// never constant). The walk recurses syntactically; name *resolution*
    /// still happens in `check_expr`, so an unknown name is a plain T0001,
    /// not a purity error.
    fn check_module_let_purity(&mut self, expr: &Expr) {
        let reject = |kind: &str| -> Option<String> {
            Some(format!(
                "module-level `let` initializer must be a constant expression — {kind} is not allowed here"
            ))
        };
        let message = match expr {
            Expr::Call(_) => reject("a call (purity derivation is not built yet)"),
            Expr::Transaction(_) => reject("a `transaction` block"),
            Expr::FieldAccess(_) => reject("a field access"),
            Expr::If(_) => reject("an `if` expression"),
            Expr::Index(_) => reject("indexing"),
            Expr::NameRef(n) => {
                let named_relvar = n
                    .ident()
                    .map(|t| self.relvars.get(t.text()).is_some())
                    .unwrap_or(false);
                if named_relvar {
                    reject("a relvar read")
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(message) = message {
            self.error(self.node_span(expr.syntax()), "T0098", message);
            return;
        }
        // Recurse into child expressions (operator operands, literal
        // elements, parenthesized bodies).
        for child in expr.syntax().children() {
            if let Some(e) = Expr::cast(child) {
                self.check_module_let_purity(&e);
            }
        }
    }

    /// `.cdstore` root walk. A `.cdstore` is DML into `coddl::storage` — a bare
    /// sequence of statements over that meta-catalog's builtin relvars. The
    /// module is **implicit** (no `use module` line), so we seed it as active and
    /// register its relvars, then check each statement against them with a fresh
    /// (empty) scope — a `.cdstore` has no operator params or locals, only the
    /// storage relvars. The DML checkers already accept `RelvarKind::Builtin`
    /// targets (T0033/T0034); the T0025 transaction guard fires only for
    /// `Public`, so builtin writes need no `transaction [...]`.
    fn check_cdstore_root(&mut self, root: &CdstoreRoot) {
        self.active_modules
            .insert(ModulePath::parse("coddl::storage"));
        self.load_active_modules();

        let mut scope = Scope::default();
        scope.push();
        // Seed the scope with the storage relvars so a bare `ConnDefault` in
        // expression position (e.g. the RHS of `ConnDefault := ConnDefault union
        // …`) resolves to `Type::Relation(H)` — the same seeding
        // `check_oper_decl` does for a `.cd` body. Every storage relvar is
        // `Builtin`, so none carries the T0025 transaction gate.
        for (name, info) in self.relvars.iter() {
            if matches!(info.kind, RelvarKind::Builtin) {
                let ty = Type::Relation(info.heading.clone());
                scope.insert(name.to_string(), ty, Span::default(), BindingOrigin::Relvar);
            }
        }

        for stmt in root.stmts() {
            self.check_stmt(&stmt, &mut scope);
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
                // Base-relvar INIT values (`S := Relation { … };`) are
                // validated in a second pass below, once every base/virtual
                // relvar is in `self.relvars` — so an INIT may reference a
                // relvar declared anywhere in the file.
                CddbItem::RelvarInit(_) => {}
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
        // Second pass — base-relvar INIT values. Deferred until here so an
        // INIT may name a relvar declared anywhere in the file, and so a
        // duplicate INIT for one relvar is caught via `initialized`.
        let mut initialized: HashSet<String> = HashSet::new();
        for item in root.items() {
            if let CddbItem::RelvarInit(init) = item {
                self.check_relvar_init(&init, &mut initialized);
            }
        }
    }

    /// Validate a base-relvar INIT value (`S := <expr>;`) — the TTM initial
    /// value applied at `coddl provision`. The LHS must resolve to a `base
    /// relvar` in this catalog; the RHS must be a **constant** relation
    /// expression (no relvar reads, no `transaction`, no side-effecting
    /// operator calls) whose heading matches the relvar's. Cell values are
    /// ordinary constant expressions typed by [`Self::check_expr`], so
    /// arithmetic (`2 * 3 + 1`), unary minus, and pure built-in calls all
    /// work; evaluating them to rows is provision's job, not the checker's.
    fn check_relvar_init(&mut self, init: &RelvarInit, initialized: &mut HashSet<String>) {
        let Some(name_tok) = init.name() else {
            return;
        };
        let name = name_tok.text().to_string();

        // 1. Resolve the LHS to a base relvar of this catalog. Copy the kind
        // and heading out so the immutable `self.relvars` borrow ends before
        // any `self.error(...)` below.
        let declared = match self.relvars.get(&name).map(|i| (i.kind, i.heading.clone())) {
            None => {
                self.error(
                    self.token_span(&name_tok),
                    "T0102",
                    format!("no base relvar `{name}` is declared in this catalog to initialize"),
                );
                return;
            }
            Some((kind, _)) if kind != RelvarKind::Base => {
                self.error(
                    self.token_span(&name_tok),
                    "T0103",
                    format!("`{name}` is a virtual relvar — only base relvars have an INIT value"),
                );
                return;
            }
            Some((_, heading)) => heading,
        };

        // 2. One INIT per relvar.
        if !initialized.insert(name.clone()) {
            self.error(
                self.token_span(&name_tok),
                "T0104",
                format!("duplicate INIT value for base relvar `{name}`"),
            );
            return;
        }

        // 3. The RHS. A missing one already drew PB0013 from the parser.
        let Some(rhs) = init.rhs() else {
            return;
        };

        // 4. Type it against the declared relation type (bidirectional, so an
        // empty `Relation {}` INIT takes the declared heading). This reuses
        // the relation-/tuple-literal typers, so malformed tuples (T0096),
        // heading-inconsistent tuples (T0019), duplicate fields (T0015) and
        // per-cell type errors (arithmetic, unknown names → T0001) all surface
        // here for free.
        let mut scope = Scope::default();
        let ty = self.check_expr_expected(&rhs, &mut scope, &Type::Relation(declared.clone()));

        // 5. The RHS must be constant (evaluable at provision time with no
        // database and no effects).
        self.check_init_value_constness(&rhs);

        // 6. Heading conformance against the declared relvar.
        match ty {
            Type::Relation(actual) => {
                self.check_init_heading(&name, &actual, &declared, rhs.syntax())
            }
            // Recovery: a cell already reported; don't pile on.
            Type::Unknown => {}
            other => {
                self.error(
                    self.node_span(rhs.syntax()),
                    "T0105",
                    format!("INIT value for `{name}` must be a relation, but is {other}"),
                );
            }
        }
    }

    /// Compare an INIT relation's derived heading against the relvar's
    /// declared heading: any missing or unexpected attribute is T0106 (one
    /// diagnostic naming them), and each attribute present on both sides whose
    /// value type isn't assignable to the declared type is T0108. An `Integer`
    /// value is accepted where a `Rational` is declared (an INIT `12` seeds a
    /// `Rational` column).
    fn check_init_heading(
        &mut self,
        name: &str,
        actual: &Heading,
        declared: &Heading,
        span_node: &SyntaxNode,
    ) {
        let missing: Vec<&str> = declared
            .attrs()
            .iter()
            .filter(|(dn, _)| actual.lookup(dn).is_none())
            .map(|(dn, _)| dn.as_str())
            .collect();
        let unexpected: Vec<&str> = actual
            .attrs()
            .iter()
            .filter(|(an, _)| declared.lookup(an).is_none())
            .map(|(an, _)| an.as_str())
            .collect();
        if !missing.is_empty() || !unexpected.is_empty() {
            let mut parts: Vec<String> = Vec::new();
            if !missing.is_empty() {
                parts.push(format!("missing {}", quote_join(&missing)));
            }
            if !unexpected.is_empty() {
                parts.push(format!("unexpected {}", quote_join(&unexpected)));
            }
            self.error(
                self.node_span(span_node),
                "T0106",
                format!(
                    "INIT value for `{name}` does not match the relvar heading ({})",
                    parts.join("; ")
                ),
            );
        }
        for (an, at) in actual.attrs() {
            if let Some(dt) = declared.lookup(an) {
                if !init_type_assignable(at, dt) {
                    self.error(
                        self.node_span(span_node),
                        "T0108",
                        format!(
                            "INIT value for `{name}`: attribute `{an}` has type {at}, but the relvar declares {dt}"
                        ),
                    );
                }
            }
        }
    }

    /// The INIT constant-expression walk — the analogue of
    /// [`Self::check_module_let_purity`], but with the `.cddb` policy: a
    /// **pure** operator call is allowed (a `.cddb` has no `oper`s or imports,
    /// so every callee is a built-in whose purity is known), and so are `if` /
    /// field-access / indexing over constants. What is rejected (T0107) is
    /// what a provision-time evaluation cannot do without a database or would
    /// make non-constant: reading a relvar, a `transaction`, or a call to a
    /// side-effecting operator. An unknown bare name is left to
    /// [`Self::check_expr`]'s T0001, not double-reported here. (The two walks
    /// are points on one "constant expression" spectrum; unify them once
    /// user-operator purity derivation lands.)
    fn check_init_value_constness(&mut self, expr: &Expr) {
        match expr {
            Expr::Transaction(_) => {
                self.error(
                    self.node_span(expr.syntax()),
                    "T0107",
                    "an INIT value must be a constant expression — a `transaction` is not constant",
                );
                return;
            }
            Expr::Call(c) => {
                if let Some(callee) = call_callee_name(c) {
                    let side_effecting = self
                        .builtins
                        .candidates(&callee)
                        .iter()
                        .any(|s| matches!(s.purity, Purity::SideEffecting));
                    if side_effecting {
                        self.error(
                            self.node_span(expr.syntax()),
                            "T0107",
                            format!(
                                "an INIT value must be a constant expression — it calls the side-effecting operator `{callee}`"
                            ),
                        );
                        return;
                    }
                }
                // A pure (or as-yet-unresolved) call: recurse into its
                // arguments below.
            }
            Expr::NameRef(n) => {
                if let Some(ident) = n.ident() {
                    if self.relvars.get(ident.text()).is_some() {
                        self.error(
                            self.node_span(expr.syntax()),
                            "T0107",
                            format!(
                                "an INIT value must be a constant expression — it reads relvar `{}`",
                                ident.text()
                            ),
                        );
                        return;
                    }
                }
            }
            _ => {}
        }
        for child in expr.syntax().children() {
            if let Some(e) = Expr::cast(child) {
                self.check_init_value_constness(&e);
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
            module: None,
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
        let heading = match &heading_ast {
            Some(h) => self.resolve_heading(h),
            None => Heading::empty(),
        };

        // A storage-backed relvar (public in `.cd`, base in `.cddb`) cannot
        // yet persist a relation- or tuple-valued attribute — an SQL base
        // table has no composite or nested-set column (T0101). The designed
        // endpoint is vertical decomposition in the `.cdstore` mapping layer
        // (an RVA maps to a child table keyed by the parent key; see
        // docs/storage.md "Nested attributes"); until that lands, reject at
        // check time rather than aborting in the runtime marshaller. Private
        // relvars are in-process state and take any heading.
        if matches!(kind, RelvarKind::Public | RelvarKind::Base) {
            if let Some(h) = &heading_ast {
                for param in h.params() {
                    let Some(ptok) = param.name() else { continue };
                    let shape = match heading.lookup(ptok.text()) {
                        Some(Type::Relation(_)) => "a relation-valued",
                        Some(Type::Tuple(_)) => "a tuple-valued",
                        _ => continue,
                    };
                    self.error(
                        self.token_span(&ptok),
                        "T0101",
                        format!(
                            "attribute `{}` of {} relvar `{name}` is {shape} attribute, \
                             which cannot be stored yet — decompose it into a side relvar \
                             keyed by `{name}`'s key",
                            ptok.text(),
                            kind.keyword(),
                        ),
                    );
                }
            }
        }

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
            module: None,
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

        // (2) Register the active modules' contents (operators, aliases,
        //     builtin relvars) and build the hint catalogs.
        self.load_active_modules();
    }

    /// Step (2) of module resolution: scan every embedded stdlib module to build
    /// the hint catalogs ([`Self::stdlib_oper_owner`] / [`Self::stdlib_type_owner`]
    /// / [`Self::stdlib_relvar_owner`]), and for the **active** ones (those in
    /// [`Self::active_modules`]) register their `builtin oper`s, type aliases /
    /// possrep scalars, and `builtin relvar`s into scope. Split out from
    /// [`Self::resolve_modules`] so the `.cdstore` check path can reuse it after
    /// seeding `active_modules` with the implicit `coddl::storage` (a `.cdstore`
    /// has no `use module` decls to collect in step (1)).
    fn load_active_modules(&mut self) {
        let core = ModulePath::parse("coddl::core");
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
            let Some(mroot) = Root::cast(out.tree) else {
                continue;
            };
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
                                    module: Some(module.path.clone()),
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
    /// built-in type or type-generator name (T0085) and a duplicate
    /// declaration (T0086); a bad component of the aliased type surfaces
    /// T0005 once, here. The aliased type resolves loudly, so it may name an
    /// alias registered earlier in source order.
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
        // The generators are not resolvable as bare type names
        // (`from_builtin_name` is "resolves to a scalar type"), but a `type`
        // named after one would be unreachable — `parse_type_ref` intercepts
        // the word as the generator — so it is rejected the same way.
        if coddl_syntax::keywords::TYPE_GENERATORS.contains(&name.as_str()) {
            self.error(
                self.token_span(&name_tok),
                "T0085",
                format!("cannot redefine built-in type generator `{name}`"),
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
                let Some(pname_tok) = param.name() else {
                    continue;
                };
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
        // relation / nested tuple in the returned value infers from it. The
        // return type is also stashed so any early `return` inside the body
        // checks its value against the same target (T0018).
        if let Some(body) = decl.body() {
            let prev_return = self.current_return_type.replace(return_type.clone());
            let body_ty = self.check_block_expected(&body, &mut scope, &return_type);
            self.current_return_type = prev_return;
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

    /// Check one statement and return its type — `Type::unit()` for the
    /// value-less statements, the expression's type for a bare `ExprStmt`
    /// (so a statement-position `if/else` both of whose arms `return`
    /// surfaces as `Never`), and `Type::Never` for `return` (it diverges).
    /// `check_block` uses this to decide whether the block falls through.
    fn check_stmt(&mut self, stmt: &Stmt, scope: &mut Scope) -> Type {
        match stmt {
            Stmt::Let(l) => {
                self.check_let_stmt(l, scope);
                Type::unit()
            }
            Stmt::Var(v) => {
                self.check_var_stmt(v, scope);
                Type::unit()
            }
            Stmt::Assign(a) => {
                self.check_assignment_stmt(a, scope);
                Type::unit()
            }
            Stmt::Truncate(t) => {
                self.check_truncate_stmt(t, scope);
                Type::unit()
            }
            Stmt::Delete(d) => {
                self.check_delete_stmt(d, scope);
                Type::unit()
            }
            Stmt::Insert(i) => {
                self.check_insert_stmt(i, scope);
                Type::unit()
            }
            Stmt::Update(u) => {
                self.check_update_stmt(u, scope);
                Type::unit()
            }
            Stmt::ExprStmt(e) => self.check_expr_stmt(e, scope),
            Stmt::For(f) => {
                self.check_for_stmt(f, scope);
                Type::unit()
            }
            Stmt::While(w) => {
                self.check_while_stmt(w, scope);
                Type::unit()
            }
            Stmt::DoWhile(d) => {
                self.check_do_while_stmt(d, scope);
                Type::unit()
            }
            Stmt::Load(l) => {
                self.check_load_stmt(l, scope);
                Type::unit()
            }
            Stmt::Return(r) => {
                self.check_return_stmt(r, scope);
                Type::Never
            }
        }
    }

    /// Check `return [<expr>];`. The value (or its absence, `Unit`) is checked
    /// against the enclosing operator's declared return type (T0018), stashed
    /// in [`Self::current_return_type`]. A `return` lexically inside a
    /// `transaction [...]` is rejected (T0093): its early exit would skip the
    /// transaction's commit, so it is a hard error until real BEGIN/COMMIT
    /// lands (mirrors the L8 "hard error over silent-wrong" discipline).
    fn check_return_stmt(&mut self, stmt: &ReturnStmt, scope: &mut Scope) {
        // The declared return type of the operator we're inside. Absent only
        // in a malformed tree (a statement outside any oper body); check the
        // value defensively against Unknown so we never cascade.
        let expected = self.current_return_type.clone().unwrap_or(Type::Unknown);

        let value_ty = match stmt.value() {
            Some(expr) => self.check_expr_expected(&expr, scope, &expected),
            // A bare `return;` yields the unit value.
            None => Type::unit(),
        };

        if !value_ty.assignable_to(&expected) {
            let span = stmt
                .value()
                .map(|e| self.node_span(e.syntax()))
                .unwrap_or_else(|| self.node_span(stmt.syntax()));
            self.error(
                span,
                "T0018",
                format!("`return` produces {value_ty}, but the operator returns {expected}"),
            );
        }

        if self.transaction_depth > 0 {
            self.error(
                self.node_span(stmt.syntax()),
                "T0093",
                "`return` from within a `transaction` is not yet supported",
            );
        }
    }

    fn check_block(&mut self, block: &Block, scope: &mut Scope) -> Type {
        let mut diverges = false;
        for stmt in block.statements() {
            // A statement whose own type is `Never` diverges — a bare
            // `return`, or a statement-position `if/else` both of whose arms
            // return. Everything after it is dead code (still walked for
            // diagnostics), and the block can't fall through.
            if self.check_stmt(&stmt, scope) == Type::Never {
                diverges = true;
            }
        }
        // The tail is still walked for diagnostics even when a preceding
        // divergent statement made it dead code.
        let tail_ty = match block.tail_expr() {
            Some(expr) => self.check_expr(&expr, scope),
            None => Type::unit(),
        };
        // A block that diverges (a statement or the tail leaves via `return`)
        // is `Never`: it never falls through to a value, so it unifies with
        // any sibling and satisfies any declared return type.
        if diverges || tail_ty == Type::Never {
            Type::Never
        } else {
            tail_ty
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
                            format!(
                                "cannot order by `{key}`: only scalar attributes have an order"
                            ),
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

        // … bound to an assignable relvar (public, private, or builtin).
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

        // … bound to an assignable relvar (public, private, or builtin).
        let assignable = self.relvars.get(name).is_some_and(|i| {
            matches!(
                i.kind,
                RelvarKind::Public | RelvarKind::Private | RelvarKind::Builtin
            )
        });
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
        let Some(operand) = stmt.operand() else {
            return;
        };

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
        let assignable = self.relvars.get(name).is_some_and(|i| {
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
        let Some(operand) = stmt.operand() else {
            return;
        };

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
            scope.insert(
                n.clone(),
                ty.clone(),
                Span::default(),
                BindingOrigin::WhereAttr,
            );
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

        let bound_ty = self.check_binding_rhs(declared, &name, &value, stmt_syntax, kw, scope);

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

    /// Infer a binding's RHS type against an optional declared annotation and
    /// enforce conformance — the shared core of statement bindings
    /// (`check_binding`) and module-level `let`s (`check_module_let`).
    ///
    /// A sequence literal is checked specially so it can take its element
    /// type from `declared` when empty and so it is *permitted* here —
    /// `check_expr` rejects sequence literals in every other position
    /// (T0063, the binding-value-only rule). An empty `Relation {}` takes
    /// its heading from a `Relation { H }` annotation (a headed empty
    /// relation); with no annotation it is relfalse. A tuple literal bound
    /// with a `Tuple` annotation propagates the annotation's field types (so
    /// an empty relation field infers). When the annotation is present it is
    /// authoritative: the RHS must conform (T0010) and lookups see the
    /// declared type; otherwise the inferred type binds *and* surfaces as an
    /// inlay hint after the name token — that's what the editor renders.
    fn check_binding_rhs(
        &mut self,
        declared: Option<Type>,
        name: &Option<SyntaxToken>,
        value: &Option<Expr>,
        stmt_syntax: &SyntaxNode,
        kw: &str,
        scope: &mut Scope,
    ) -> Type {
        let rhs_ty = match value {
            Some(Expr::SequenceLit(s)) => {
                let expected_elem = match &declared {
                    Some(Type::Sequence(e)) => Some((**e).clone()),
                    _ => None,
                };
                self.check_sequence_lit(s, scope, expected_elem)
            }
            Some(Expr::RelationLit(r)) => {
                let expected_heading = match &declared {
                    Some(Type::Relation(h)) => Some(h.clone()),
                    _ => None,
                };
                self.check_relation_lit(r, scope, expected_heading)
            }
            Some(Expr::TupleLit(t)) if matches!(&declared, Some(Type::Tuple(_))) => {
                let Some(Type::Tuple(h)) = &declared else {
                    unreachable!("guarded by the match arm")
                };
                self.check_tuple_lit_expected(t, scope, &h.clone())
            }
            Some(v) => self.check_expr(v, scope),
            None => Type::Unknown,
        };

        match declared {
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
                if let Some(name_tok) = name {
                    let r = name_tok.text_range();
                    self.hints.push(TypeHint {
                        span: Span::new(self.file, r.end().into(), r.end().into()),
                        ty: rhs_ty.clone(),
                        kind: HintKind::LetBinding,
                    });
                }
                rhs_ty
            }
        }
    }

    /// An expression in statement position. Its value is discarded *unless*
    /// this is the block's tail — but its type still flows back to
    /// `check_stmt`, so a divergent expression (a statement-position
    /// `if/else` both of whose arms `return`) can make the block diverge.
    fn check_expr_stmt(&mut self, stmt: &ExprStmt, scope: &mut Scope) -> Type {
        match stmt.expr() {
            Some(expr) => self.check_expr(&expr, scope),
            None => Type::unit(),
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
                // Not in the local scope: a module-level `let` (constant
                // binding) resolves next — locals shadow it, it shadows
                // imports.
                if let Some(ty) = self.module_lets.get(name) {
                    return ty.clone();
                }
                // Then module lets imported via `use module`. Two imports
                // exporting the same name coexist until it is used — the
                // same ambiguity rule imported opers follow (T0092).
                if let Some(candidates) = self.imported_lets.get(name) {
                    if candidates.len() > 1 {
                        let owners: Vec<String> =
                            candidates.iter().map(|(m, _)| m.to_string()).collect();
                        self.error(
                            self.token_span(&ident),
                            "T0092",
                            format!(
                                "`{name}` is exported by more than one imported module ({})",
                                owners.join(", ")
                            ),
                        );
                        return Type::Unknown;
                    }
                    if let Some((_, ty)) = candidates.first() {
                        return ty.clone();
                    }
                }
                // The always-in-scope stdlib's module lets (coddl::core —
                // reltrue/relfalse), shadowed by everything above.
                if let Some(ty) = self.stdlib_lets.get(name) {
                    return ty.clone();
                }
                // If it's an opt-in stdlib builtin relvar, point at the
                // import rather than reporting a plain unresolved name.
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
            Expr::Group(g) => self.check_group_expr(g, scope),
            Expr::Ungroup(u) => self.check_ungroup_expr(u, scope),
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
                    // A diverging arm (one ending in `return`) yields no value,
                    // so the `if` takes the other arm's type; both diverging is
                    // `Never`. This is what lets a `{ status, body }` route sit
                    // opposite an `else [ return not_found{} ]`.
                    (Type::Never, _) => else_ty,
                    (_, Type::Never) => then_ty,
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
                // A Unit then-arm is the statement form; a `Never` then-arm is a
                // guard clause (`if bad then [ return … ]`) — both are fine, and
                // the whole expression's value is Unit (the false fall-through).
                if then_ty != Type::unit() && then_ty != Type::Unknown && then_ty != Type::Never {
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
                    format!(
                        "`unwrap` target `{name}` is not a tuple-valued attribute (got {other})"
                    ),
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

    /// Walk `R group { pq: { a, b }, … }` — TTM GROUP: consume attributes into
    /// relation-valued attributes; the attributes named in NO pair survive and
    /// partition the relation (one result tuple per distinct survivor
    /// combination). The operand must be `Relation H` (T0023). Each consumed
    /// attr must exist (T0027) and be consumed once across all pairs (T0028).
    /// Multi-pair `group` is simultaneous — a single partition by the common
    /// survivors, each pair nesting its own components (`{…}` is unordered, so
    /// Tutorial D's sequential-commalist semantics is out; chain `group {…}
    /// group {…}` for that). Result heading = survivors + each `(new,
    /// Relation(components))`; a collision is T0031.
    fn check_group_expr(&mut self, ge: &GroupExpr, scope: &mut Scope) -> Type {
        let input_ty = match ge.input() {
            Some(e) => self.check_expr(&e, scope),
            None => return Type::Unknown,
        };
        let heading = match &input_ty {
            Type::Relation(h) => h.clone(),
            Type::Unknown => return Type::Unknown,
            other => {
                let span = ge
                    .input()
                    .map(|e| self.node_span(e.syntax()))
                    .unwrap_or_else(|| self.node_span(ge.syntax()));
                self.error(
                    span,
                    "T0023",
                    format!("`group` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Collect each pair's new name + its consumed components (as an RVA),
        // and the set of all consumed attributes.
        let mut consumed: HashSet<String> = HashSet::new();
        let mut added: Vec<(String, Type)> = Vec::new();
        for pair in ge.pairs() {
            let Some(new_tok) = pair.name() else { continue };
            let new = new_tok.text();
            let mut components: Vec<(String, Type)> = Vec::new();
            for tok in pair.grouped() {
                let name = tok.text();
                let Some(ty) = heading.lookup(name).cloned() else {
                    self.error(
                        self.token_span(&tok),
                        "T0027",
                        format!("unknown attribute `{name}` in group of {heading}"),
                    );
                    continue;
                };
                if !consumed.insert(name.to_string()) {
                    self.error(
                        self.token_span(&tok),
                        "T0028",
                        format!("attribute `{name}` is grouped more than once"),
                    );
                    continue;
                }
                components.push((name.to_string(), ty));
            }
            added.push((new.to_string(), Type::Relation(Heading::new(components))));
        }
        // Result = surviving (non-consumed) attributes + the new RVAs; a new
        // name colliding with a survivor or another new name is T0031.
        let mut result: Vec<(String, Type)> = Vec::new();
        let mut result_names: HashSet<String> = HashSet::new();
        for (name, ty) in heading.attrs() {
            if consumed.contains(name) {
                continue;
            }
            result_names.insert(name.clone());
            result.push((name.clone(), ty.clone()));
        }
        for (name, ty) in added {
            if !result_names.insert(name.clone()) {
                self.error(
                    self.node_span(ge.syntax()),
                    "T0031",
                    format!("group produces a duplicate attribute `{name}`"),
                );
            }
            result.push((name, ty));
        }
        Type::Relation(Heading::new(result))
    }

    /// Walk `R ungroup { pq, … }` — TTM UNGROUP: unnest relation-valued
    /// attributes back to top level, one result tuple per combination of an
    /// outer tuple and one tuple from each named RVA (an empty RVA contributes
    /// nothing). The operand must be `Relation H` (T0023). Each named attr must
    /// exist (T0027), be listed once (T0028), and be `Type::Relation(_)`
    /// (T0100 — the RVA analogue of unwrap's T0048). Result heading = the
    /// attributes not ungrouped, plus each ungrouped relation's attributes; a
    /// lifted attribute colliding with a survivor or another lifted attribute
    /// is T0031 (rename before ungrouping).
    fn check_ungroup_expr(&mut self, ue: &UngroupExpr, scope: &mut Scope) -> Type {
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
                    format!("`ungroup` expects a Relation on the left, got {other}"),
                );
                return Type::Unknown;
            }
        };
        // Each listed name: exists (T0027), unique in the list (T0028), and is
        // a relation-valued attribute (T0100). Collect the ungrouped set + the
        // lifted attributes.
        let mut ungrouped: HashSet<String> = HashSet::new();
        let mut lifted: Vec<(String, Type)> = Vec::new();
        for tok in ue.attrs() {
            let name = tok.text();
            let Some(ty) = heading.lookup(name).cloned() else {
                self.error(
                    self.token_span(&tok),
                    "T0027",
                    format!("unknown attribute `{name}` in ungroup of {heading}"),
                );
                continue;
            };
            if !ungrouped.insert(name.to_string()) {
                self.error(
                    self.token_span(&tok),
                    "T0028",
                    format!("duplicate attribute `{name}` in ungroup list"),
                );
                continue;
            }
            match ty {
                Type::Relation(sub) => {
                    for (cn, ct) in sub.attrs() {
                        lifted.push((cn.clone(), ct.clone()));
                    }
                }
                other => self.error(
                    self.token_span(&tok),
                    "T0100",
                    format!(
                        "`ungroup` target `{name}` is not a relation-valued attribute (got {other})"
                    ),
                ),
            }
        }
        // Result = surviving (non-ungrouped) attributes + the lifted
        // attributes; a collision (lifted vs survivor or vs another lifted) is
        // T0031.
        let mut result: Vec<(String, Type)> = Vec::new();
        let mut result_names: HashSet<String> = HashSet::new();
        for (name, ty) in heading.attrs() {
            if ungrouped.contains(name) {
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
                    format!("ungroup produces a duplicate attribute `{name}`"),
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
            scope.insert(
                name.clone(),
                ty.clone(),
                Span::default(),
                BindingOrigin::WhereAttr,
            );
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
                            format!(
                                "`replace` value for `{new}` must be Integer or Text, got {vty}"
                            ),
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
            scope.insert(
                name.clone(),
                ty.clone(),
                Span::default(),
                BindingOrigin::WhereAttr,
            );
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
    /// `Extract`: operand must be `Relation H`; result is `Tuple H`
    /// (T0024 on mismatch). `Not`: operand must be `Boolean`; result
    /// is `Boolean` (T0021 on mismatch — shared with `and`/`or`).
    /// `Pos`/`Neg` (unary `+`/`-`): operand must be `Integer` or `Rational`;
    /// result is the operand type (T0109 on mismatch — Approximate is not yet
    /// supported).
    fn check_unary_expr(&mut self, ue: &UnaryExpr, scope: &mut Scope) -> Type {
        let op = match ue.op_kind() {
            Some(op) => op,
            None => return Type::Unknown,
        };
        match op {
            UnaryOp::Not => {
                let operand_ty = match ue.operand() {
                    Some(e) => self.check_expr(&e, scope),
                    None => return Type::Boolean,
                };
                if !matches!(operand_ty, Type::Boolean | Type::Unknown) {
                    let span = ue
                        .operand()
                        .map(|e| self.node_span(e.syntax()))
                        .unwrap_or_else(|| self.node_span(ue.syntax()));
                    self.error(
                        span,
                        "T0021",
                        format!("`not` expects Boolean, got {operand_ty}"),
                    );
                }
                // `not` is always Boolean-valued; return `Boolean` even on a
                // bad operand (like `check_logical_op`) so the error doesn't
                // cascade into the enclosing `if`/`and`/`or`.
                Type::Boolean
            }
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
            UnaryOp::Pos | UnaryOp::Neg => {
                let operand_ty = match ue.operand() {
                    Some(e) => self.check_expr(&e, scope),
                    None => return Type::Unknown,
                };
                let opname = if matches!(op, UnaryOp::Neg) { "-" } else { "+" };
                match operand_ty {
                    // Result is the operand's type; `+` is identity, `-`
                    // negates. Rational stays canonical (den > 0) — the
                    // lowerer desugars `-x` to `0 - x`, so the runtime's
                    // rational subtract does the reduction.
                    Type::Integer => Type::Integer,
                    Type::Rational => Type::Rational,
                    Type::Unknown => Type::Unknown,
                    other => {
                        let span = ue
                            .operand()
                            .map(|e| self.node_span(e.syntax()))
                            .unwrap_or_else(|| self.node_span(ue.syntax()));
                        self.error(
                            span,
                            "T0109",
                            format!(
                                "unary `{opname}` requires an Integer or Rational operand (Approximate is not yet supported), got {other}"
                            ),
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
        let mut diverges = false;
        for stmt in block.statements() {
            // See [`Self::check_block`]: any statement typed `Never` diverges.
            if self.check_stmt(&stmt, scope) == Type::Never {
                diverges = true;
            }
        }
        let tail_ty = match block.tail_expr() {
            Some(expr) => self.check_expr_expected(&expr, scope, expected),
            None => Type::unit(),
        };
        // See [`Self::check_block`]: a block that diverges (a statement or the
        // tail leaves via `return`) has bottom type `Never`.
        if diverges || tail_ty == Type::Never {
            Type::Never
        } else {
            tail_ty
        }
    }

    /// Walk a `Relation { <expr>, <expr>, … }` literal. Each element is an
    /// arbitrary expression that must be **tuple-typed** — a tuple literal
    /// `{ a: 1 }`, or a tuple-valued name/call/field-access (`Relation { req }`);
    /// a non-tuple element is T0096. The first tuple-typed element establishes
    /// the heading; the rest must have the same `(name, type)` set (T0019 on the
    /// offending element). An empty `Relation {}` is the nullary empty relation
    /// `relfalse` (empty heading, zero tuples — the zero of the join semiring);
    /// its sibling `reltrue` is `Relation { {} }` (one empty tuple).
    fn check_relation_lit(
        &mut self,
        rel: &RelationLit,
        scope: &mut Scope,
        expected: Option<Heading>,
    ) -> Type {
        let elements: Vec<Expr> = rel.elements().collect();
        if elements.is_empty() {
            // Empty `Relation {}`: take the heading from the expected type when
            // there is one (a `let`/`var` annotation → a *headed* empty
            // relation), else default to `relfalse` — the nullary empty relation
            // (∅ heading). Unlike an empty `Sequence []` (T0061), no annotation
            // is *required*: relfalse is a sensible unconstrained default.
            return Type::Relation(expected.unwrap_or_else(Heading::empty));
        }
        // Each element is an arbitrary expression that must be tuple-typed (a
        // tuple literal, or a tuple-valued name/call/…). Check every element so
        // each surfaces its own diagnostic; the first tuple-typed element fixes
        // the heading, the rest must be assignable to it (T0019). A non-tuple
        // element is T0096.
        let mut first_heading: Option<Heading> = None;
        for e in &elements {
            let h = match self.check_expr(e, scope) {
                Type::Tuple(h) => h,
                // Recovery: skip, don't cascade.
                Type::Unknown => continue,
                other => {
                    self.error(
                        self.node_span(e.syntax()),
                        "T0096",
                        format!("relation literal element must be a tuple, got {other}"),
                    );
                    continue;
                }
            };
            match &first_heading {
                None => first_heading = Some(h),
                Some(first) if !h.assignable_to(first) => {
                    self.error(
                        self.node_span(e.syntax()),
                        "T0019",
                        format!(
                            "tuple heading {h} differs from the relation's first tuple {first}"
                        ),
                    );
                }
                Some(_) => {}
            }
        }
        match first_heading {
            Some(h) => Type::Relation(h),
            None => Type::Unknown,
        }
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
                    format!("sequence element type {t} differs from the first element's {elem_ty}"),
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
            BinaryOp::Matching | BinaryOp::NotMatching => {
                self.check_matching_binary(bin, op, scope)
            }
            BinaryOp::When => self.check_when_binary(bin, scope),
            BinaryOp::Otherwise => self.check_otherwise_binary(bin, scope),
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
            scope.insert(
                name.clone(),
                ty.clone(),
                Span::default(),
                BindingOrigin::WhereAttr,
            );
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

    /// `R when c` — gate: `Relation H × Boolean → Relation H`. The left
    /// operand survives when the condition holds and is replaced by the empty
    /// relation with the same heading when it doesn't (IR-level `R times ⟨c⟩`,
    /// the condition lifted to reltrue/relfalse). The condition typechecks in
    /// the **enclosing** scope — deliberately no heading injection: `where`
    /// filters per-tuple with the heading in scope, `when` gates the whole
    /// relation from outside it. Not a coercion (TTM ch. 3, p. 74 — coercion
    /// is *implicit* conversion of a wrongly-typed operand): the operand is
    /// required-Boolean and is-Boolean, an operation "with operands that are
    /// explicitly defined to be of different types" (the LOAD pattern,
    /// ch. 5, p. 123). A non-Boolean condition is T0099; a relation-typed
    /// condition gets the `times` suggestion, and a condition naming an
    /// attribute of the left operand gets the `where` suggestion.
    fn check_when_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        let lhs_h = self.relation_operand(bin.lhs(), "when", scope);
        // No scope.push(): the condition sees exactly what the enclosing
        // expression sees.
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        if !matches!(rhs_ty, Type::Boolean | Type::Unknown) {
            let span = bin
                .rhs()
                .map(|e| self.node_span(e.syntax()))
                .unwrap_or_else(|| self.node_span(bin.syntax()));
            let msg = match &rhs_ty {
                Type::Relation(_) => format!(
                    "`when` condition must be Boolean, got {rhs_ty} — to gate by a \
                     relation, use `times`"
                ),
                _ => format!("`when` condition must be Boolean, got {rhs_ty}"),
            };
            self.error(span, "T0099", msg);
        }
        // A condition that names an attribute of the left operand is the
        // where/when confusion: attributes are deliberately not in scope
        // here. Point at `where` instead of leaving a bare unknown-name.
        if let (Some(h), Some(rhs)) = (&lhs_h, bin.rhs()) {
            let mut names = HashSet::new();
            attr_refs(&rhs, &mut names);
            for name in names {
                let resolves_outside = scope.lookup(&name).is_some()
                    || self.module_lets.contains_key(&name)
                    || self.imported_lets.contains_key(&name)
                    || self.stdlib_lets.contains_key(&name);
                if !resolves_outside && h.attrs().iter().any(|(n, _)| n == &name) {
                    self.error(
                        self.node_span(rhs.syntax()),
                        "T0099",
                        format!(
                            "`{name}` is an attribute of the left operand — attributes \
                             are not in scope in a `when` condition; use `where` to \
                             filter per-tuple"
                        ),
                    );
                }
            }
        }
        match lhs_h {
            Some(h) => Type::Relation(h),
            None => Type::Unknown,
        }
    }

    /// `R otherwise D` — relational COALESCE: the left operand if it is
    /// nonempty, else the right (IR-level `R union (D times (reltrue minus
    /// (R project {})))`; the arms are exclusive by construction). Both
    /// operands must be relations with the **same** heading, like `union`;
    /// mismatched headings → T0038. Result: that shared heading.
    fn check_otherwise_binary(&mut self, bin: &BinaryExpr, scope: &mut Scope) -> Type {
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), "otherwise", scope);
        let rhs_h = self.relation_operand(bin.rhs(), "otherwise", scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        match self.identical_headings(bin, &lhs_h, &rhs_h, "otherwise") {
            Some(h) => Type::Relation(h),
            None => Type::Unknown,
        }
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
            if !lhs_h.attrs().iter().any(|(n, t)| n == name && t == ty) && !differing.contains(name)
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

    /// `R matching S` (semijoin) / `R not matching S` (antijoin) — filter the
    /// **left** operand by whether each of its tuples has (matching) / lacks (not
    /// matching) a match in `S` on the shared attributes. Both are typed like
    /// `join`/`compose` — partial overlap required — because the shared
    /// attributes are the key matched on. The result heading is always the left
    /// operand's (a subset of its tuples). The two degenerate cases are rejected
    /// with the set operator they collapse to / their non-meaning:
    /// identical headings → a join on every attribute = `intersect` (matching) /
    /// `minus` (not matching), so T0094 suggests it; disjoint headings → no key to
    /// match on (the semijoin degenerates to an existence guard on the left
    /// operand), so T0095 rejects it. A shared-attribute type clash → T0036 (the
    /// same join-key check `join`/`compose` use).
    fn check_matching_binary(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let (op_name, set_op) = match op {
            BinaryOp::Matching => ("matching", "intersect"),
            BinaryOp::NotMatching => ("not matching", "minus"),
            _ => unreachable!("check_matching_binary called on {op:?}"),
        };
        // Check both operands first so each surfaces its own diagnostics.
        let lhs_h = self.relation_operand(bin.lhs(), op_name, scope);
        let rhs_h = self.relation_operand(bin.rhs(), op_name, scope);
        let (Some(lhs_h), Some(rhs_h)) = (lhs_h, rhs_h) else {
            return Type::Unknown;
        };
        // Identical headings: the semijoin matches on every attribute, which is a
        // set intersection (matching) / difference (not matching). Suggest that
        // set operator. Checked before the disjoint case so two nullary operands
        // (identical *and* share-nothing) report the more useful suggestion.
        if lhs_h == rhs_h {
            self.error(
                self.node_span(bin.syntax()),
                "T0094",
                format!("`{op_name}` operands have identical headings — did you mean `{set_op}`?"),
            );
            return Type::Unknown;
        }
        // Disjoint headings: no shared attribute means no key to match on; the
        // semijoin degenerates to an existence guard on the left operand. Reject —
        // the operands must share the attributes the match is computed over.
        if lhs_h.is_disjoint_from(&rhs_h) {
            self.error(
                self.node_span(bin.syntax()),
                "T0095",
                format!(
                    "`{op_name}` operands share no attribute — a semijoin has no key to match on"
                ),
            );
            return Type::Unknown;
        }
        // Partial overlap: the shared attributes must agree in type (the same
        // join-key check as `join`/`compose`); the result keeps the left heading.
        match lhs_h.union(&rhs_h) {
            Ok(_) => Type::Relation(lhs_h),
            Err(name) => {
                self.error(
                    self.node_span(bin.syntax()),
                    "T0036",
                    format!(
                        "`{op_name}` shared attribute `{name}` has different types on each side"
                    ),
                );
                Type::Unknown
            }
        }
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

    /// `lhs = rhs` / `lhs <> rhs` — polymorphic: on scalars, operands must
    /// share a scalar type (Integer, Text, Character, Approximate, Rational,
    /// or Boolean for v1); on relations, observational set equality
    /// (RM Pre 8) over identical headings (T0038 otherwise, like
    /// `union`/`minus`). Result is Boolean.
    fn check_equality_op(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        // The relation overload: heading + tuple set, however the operands
        // were built or fetched — never a representation compare.
        if let (Type::Relation(lhs_h), Type::Relation(rhs_h)) = (&lhs_ty, &rhs_ty) {
            let (lhs_h, rhs_h) = (lhs_h.clone(), rhs_h.clone());
            self.identical_headings(bin, &lhs_h, &rhs_h, op_display(op));
            return Type::Boolean;
        }
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
                    "`{opname}` operands must share a scalar type (Integer, Text, Character, Approximate, Rational, or Boolean) or both be relations with identical headings; got {lhs_ty} vs {rhs_ty}"
                ),
            );
        }
        Type::Boolean
    }

    /// `lhs < rhs` / `lhs > rhs` / `lhs <= rhs` / `lhs >= rhs` —
    /// polymorphic: scalar ordering on two Integers or two Rationals (no
    /// mixing), **subset / superset** on two relations with identical
    /// headings (`<`/`>` are the strict forms; T0038 on a heading mismatch,
    /// like `union`/`minus`). Result is Boolean.
    fn check_ordering_op(&mut self, bin: &BinaryExpr, op: BinaryOp, scope: &mut Scope) -> Type {
        let lhs_ty = match bin.lhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        let rhs_ty = match bin.rhs() {
            Some(e) => self.check_expr(&e, scope),
            None => Type::Unknown,
        };
        // The relation overload: `<=` is the subset test — there is no
        // separate `subset` keyword (docs/grammar.md "Symbolic").
        if let (Type::Relation(lhs_h), Type::Relation(rhs_h)) = (&lhs_ty, &rhs_ty) {
            let (lhs_h, rhs_h) = (lhs_h.clone(), rhs_h.clone());
            self.identical_headings(bin, &lhs_h, &rhs_h, op_display(op));
            return Type::Boolean;
        }
        // Scalar ordering is defined on Integer and Rational (no mixing).
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
                format!("`{opname}` requires two Integer or two Rational operands, or two relations with identical headings (subset/superset); got {lhs_ty} vs {rhs_ty}"),
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
        if callee_name == "write_line" && self_ty.is_none() && call_has_named_arg(call, "template")
        {
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
        // Nothing local (builtin / own `oper` / possrep selector) claims this
        // name — consult the userspace imports. A unit's own definitions shadow
        // imports, so this runs only when `candidates` is empty. Exactly one
        // exporting module resolves; two or more is ambiguous (T0092).
        if candidates.is_empty() {
            let imports = self
                .imported_opers
                .get(&callee_name)
                .cloned()
                .unwrap_or_default();
            match imports.len() {
                0 => {}
                1 => candidates.push(imports.into_iter().next().expect("len == 1").1),
                _ => {
                    let mods = imports
                        .iter()
                        .map(|(m, _)| format!("`{m}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.error(
                        self.token_span(&callee_name_tok),
                        "T0092",
                        format!(
                            "call to `{callee_name}` is ambiguous — it is exported by more than \
                             one imported module ({mods}); define a local `oper {callee_name}` to \
                             disambiguate"
                        ),
                    );
                    return Type::Unknown;
                }
            }
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
                    format!(
                        "no matching overload of `{callee_name}` for argument types {{ {got} }}"
                    ),
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

/// The operator name a call selects — the bare-`NameRef` callee of a prefix
/// call `m { … }`, or the method token of a UFCS call `x.m { … }`. Used to
/// look a call's purity up in the builtins registry.
fn call_callee_name(call: &CallExpr) -> Option<String> {
    match call.callee()? {
        Expr::NameRef(n) => n.ident().map(|t| t.text().to_string()),
        Expr::FieldAccess(fa) => fa.field().map(|t| t.text().to_string()),
        _ => None,
    }
}

/// Whether an INIT cell value of type `actual` may seed a column declared
/// `declared`: ordinary assignability, plus the one widening a constant seed
/// needs — an `Integer` value into a `Rational` column (an INIT `12` for a
/// `Rational` attribute).
fn init_type_assignable(actual: &Type, declared: &Type) -> bool {
    actual.assignable_to(declared) || matches!((actual, declared), (Type::Integer, Type::Rational))
}

/// Render a name list as `` `a`, `b` `` for a heading-mismatch diagnostic.
fn quote_join(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
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
/// attribute ref), `Binary` (both operands), `Unary` (its operand), and `Call`
/// (each argument value — `replace { html: page_html{ title, body } }` reads
/// `title` and `body`); other shapes contribute nothing. Names not in the
/// operand heading are filtered by the caller. Kept in step with the lowerer's
/// `ast_attr_refs`, so the removed set the checker reports is the one the
/// desugar removes.
fn attr_refs(expr: &Expr, into: &mut HashSet<String>) {
    match expr {
        Expr::NameRef(n) => {
            if let Some(tok) = n.ident() {
                into.insert(tok.text().to_string());
            }
        }
        Expr::Call(c) => {
            if let Some(list) = c.args() {
                for arg in list.args() {
                    if let Some(v) = arg.value() {
                        attr_refs(&v, into);
                    }
                }
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
        BinaryOp::Matching => "matching",
        BinaryOp::NotMatching => "not matching",
        BinaryOp::When => "when",
        BinaryOp::Otherwise => "otherwise",
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

    fn diagnostics_cdstore(src: &str) -> Vec<Diagnostic> {
        check(src, FileId(0), FileKind::Cdstore).diagnostics
    }

    fn codes_cdstore(src: &str) -> Vec<&'static str> {
        diagnostics_cdstore(src)
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn cdstore_dml_against_storage_relvars_checks_clean() {
        // The greetings.cdstore shape: `coddl::storage` is implicit, so the two
        // inserts and the `:=` resolve `Backends`/`ConnEnv`/`ConnDefault` and
        // typecheck with no `use module` line and no diagnostics.
        let src = "insert Backends Relation { { database: \"greetings\", backend: \"sqlite\" }, };\n\
                   insert ConnEnv Relation { { database: \"greetings\", backend: \"sqlite\", field: \"file\", env_var: \"HELLO_WORLD_SQLITE_PATH\" }, };\n\
                   ConnDefault := ConnDefault union Relation { { database: \"greetings\", backend: \"sqlite\", field: \"file\", value: \"greetings.sqlite\" }, };\n";
        let diags = diagnostics_cdstore(src);
        assert!(diags.is_empty(), "expected clean, got: {diags:?}");
    }

    #[test]
    fn cdstore_insert_heading_mismatch_is_t0034() {
        // `Backends` is `{ database, backend }`; an extra attribute mismatches
        // the target heading.
        let src = "insert Backends Relation { { database: \"g\", backend: \"sqlite\", bogus: \"x\" }, };\n";
        assert!(
            codes_cdstore(src).contains(&"T0034"),
            "{:?}",
            codes_cdstore(src)
        );
    }

    #[test]
    fn cdstore_write_to_unknown_relvar_is_t0033() {
        let src = "insert Nope Relation { { a: \"x\" }, };\n";
        assert!(
            codes_cdstore(src).contains(&"T0033"),
            "{:?}",
            codes_cdstore(src)
        );
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
    fn not_on_boolean_checks_clean() {
        // `not` (and its `¬` glyph) on a Boolean is `Boolean` — no diagnostics.
        for src in [
            "program p; oper main {} [ let b = not true; if b then [ write_line { message: \"x\" }; ]; ];",
            "program p; oper main {} [ let b = ¬ true; if b then [ write_line { message: \"x\" }; ]; ];",
        ] {
            let diags = diagnostics(src);
            assert!(diags.is_empty(), "src={src}: {diags:?}");
        }
    }

    #[test]
    fn not_on_non_boolean_is_t0021() {
        // A non-Boolean operand is T0021, the same code `and`/`or` use.
        let src = "program p; oper main {} [ let b = not 3; if b then [ write_line { message: \"x\" }; ]; ];";
        assert!(codes(src).contains(&"T0021"), "{:?}", codes(src));
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
    fn matching_with_shared_attribute_checks_clean() {
        // R { a, b } matching S { a, c } shares `a` -> semijoin on `a`, result
        // keeps R's heading { a, b }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R matching S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn matching_with_subset_heading_checks_clean() {
        // R { a, b } matching S { a } — S's heading is a subset; partial overlap,
        // result keeps R's heading { a, b }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer } key { a }; \
                   oper main {} [ write_relation { rel: R matching S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn matching_result_keeps_left_heading() {
        // The semijoin result is a subset of the left operand, so its heading is
        // R's — not the join's union. `(R matching S) union R` type-checks clean
        // only if `R matching S` has R's heading (else union -> T0038).
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: (R matching S) union R }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn matching_with_identical_headings_diagnoses_t0094() {
        // Semijoin on every attribute is a set intersection -> suggest `intersect`.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R matching S }; ];";
        assert!(codes(src).contains(&"T0094"), "{:?}", codes(src));
    }

    #[test]
    fn matching_with_disjoint_headings_diagnoses_t0095() {
        // No shared attribute -> no key to match on -> reject.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { b: Integer } key { b }; \
                   oper main {} [ write_relation { rel: R matching S }; ];";
        assert!(codes(src).contains(&"T0095"), "{:?}", codes(src));
    }

    #[test]
    fn matching_with_shared_type_mismatch_diagnoses_t0036() {
        // Shared name `a` but Integer on one side, Text on the other.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Text, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R matching S }; ];";
        assert!(codes(src).contains(&"T0036"), "{:?}", codes(src));
    }

    #[test]
    fn when_gate_checks_clean_and_keeps_left_heading() {
        // `R when c` — c is an enclosing-scope Boolean; the result keeps R's
        // heading (`(R when c) union R` typechecks clean only then).
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ let c = true; \
                   write_relation { rel: (R when c) union R }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn when_condition_may_be_any_boolean_expression() {
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   oper main {} [ let m = \"GET\"; \
                   write_relation { rel: R when m = \"GET\" and 1 < 2 }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn when_non_boolean_condition_diagnoses_t0099() {
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   oper main {} [ write_relation { rel: R when 42 }; ];";
        assert!(codes(src).contains(&"T0099"), "{:?}", codes(src));
    }

    #[test]
    fn when_relation_condition_suggests_times() {
        // Gating by a relation is what `times` already does — T0099 with the
        // `times` pointer.
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { b: Integer } key { b }; \
                   oper main {} [ write_relation { rel: R when S }; ];";
        let diags = diagnostics(src);
        let t99 = diags.iter().find(|d| d.code == "T0099").expect("T0099");
        assert!(t99.message.contains("times"), "{}", t99.message);
    }

    #[test]
    fn when_condition_gets_no_heading_injection() {
        // `where` injects R's attributes into the predicate scope; `when`
        // deliberately does not — an attribute name in the condition is
        // unresolved, and the hint points at `where`.
        let src = "program p; \
                   private relvar R { a: Integer, flag: Boolean } key { a }; \
                   oper main {} [ write_relation { rel: R when flag }; ];";
        let diags = diagnostics(src);
        let hint = diags.iter().find(|d| d.code == "T0099").expect("T0099");
        assert!(hint.message.contains("where"), "{}", hint.message);
    }

    #[test]
    fn when_condition_sees_enclosing_locals_over_nothing() {
        // The condition resolves in the enclosing scope even when the name
        // collides with an attribute of the left operand — the local wins
        // (there is no injection to compete with).
        let src = "program p; \
                   private relvar R { a: Integer, flag: Boolean } key { a }; \
                   oper main {} [ let flag = true; \
                   write_relation { rel: R when flag }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn otherwise_checks_clean_with_identical_headings() {
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar D { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R otherwise D }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn otherwise_heading_mismatch_diagnoses_t0038() {
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar D { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R otherwise D }; ];";
        assert!(codes(src).contains(&"T0038"), "{:?}", codes(src));
    }

    #[test]
    fn otherwise_non_relation_operand_diagnoses_t0023() {
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   oper main {} [ write_relation { rel: R otherwise 42 }; ];";
        assert!(codes(src).contains(&"T0023"), "{:?}", codes(src));
    }

    #[test]
    fn not_matching_with_shared_attribute_checks_clean() {
        // R { a, b } not matching S { a, c } shares `a` -> antijoin, result keeps
        // R's heading { a, b }.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, c: Text } key { a }; \
                   oper main {} [ write_relation { rel: R not matching S }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn not_matching_with_identical_headings_diagnoses_t0094() {
        // Antijoin on every attribute is a set difference -> suggest `minus`.
        let src = "program p; \
                   private relvar R { a: Integer, b: Text } key { a }; \
                   private relvar S { a: Integer, b: Text } key { a }; \
                   oper main {} [ write_relation { rel: R not matching S }; ];";
        assert!(codes(src).contains(&"T0094"), "{:?}", codes(src));
    }

    #[test]
    fn not_matching_with_disjoint_headings_diagnoses_t0095() {
        let src = "program p; \
                   private relvar R { a: Integer } key { a }; \
                   private relvar S { b: Integer } key { b }; \
                   oper main {} [ write_relation { rel: R not matching S }; ];";
        assert!(codes(src).contains(&"T0095"), "{:?}", codes(src));
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
        assert!(
            !cs.contains(&"T0005"),
            "should be T0088, not T0005: {:?}",
            cs
        );
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
        // Module vocabulary is not reserved: without importing `coddl::web`, a user may define
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
        assert!(
            !cs.contains(&"T0001"),
            "should be T0090, not T0001: {:?}",
            cs
        );
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
        // Module vocabulary is not reserved: without importing `coddl::env`, `Environment` is a
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
    fn env_builtin_relvar_general_assign_and_truncate_are_allowed() {
        // `R := V` and `truncate R` on a builtin relvar are now allowed, uniform
        // with insert/update/delete: the builtin write path reconciles the whole
        // value. A genuine reassignment (not `R := R`, which warns T0051) and a
        // truncate draw no error and no T0033.
        let src = "program p; use module coddl::env; oper main {} [ \
                   Environment := Environment where name = \"X\"; \
                   truncate Environment; ];";
        assert!(diagnostics(src).is_empty(), "{:?}", diagnostics(src));
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
    fn type_decl_cannot_shadow_a_generator() {
        // A `type` named after a generator would register and then be
        // unreachable (`parse_type_ref` intercepts the word as the
        // generator), so both declaration forms reject it like a builtin.
        for gen in coddl_syntax::keywords::TYPE_GENERATORS {
            let possrep = format!("program p; type {gen} {{ c: Integer }};");
            assert!(
                codes(&possrep).contains(&"T0085"),
                "`type {gen} {{ … }}`: {:?}",
                codes(&possrep)
            );
            let alias = format!("program p; type {gen} = Integer;");
            assert!(
                codes(&alias).contains(&"T0085"),
                "`type {gen} = …`: {:?}",
                codes(&alias)
            );
        }
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

    // ── `return` (early exit) ────────────────────────────────────────

    #[test]
    fn return_value_type_mismatch_emits_t0018() {
        // `return "x"` in an `Integer`-returning oper.
        let src = "oper f {} -> Integer [ return \"x\"; ]; oper main {} [];";
        assert!(codes(src).contains(&"T0018"), "got {:?}", codes(src));
    }

    #[test]
    fn return_matching_declared_type_clean() {
        // A guard `return` plus a matching tail; both are Integer.
        let src = "oper f { n: Integer } -> Integer [ if n = 0 then [ return 7; ]; 9 ]; \
                   oper main {} [];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn if_arm_return_unifies_no_t0068() {
        // The `else` arm diverges (`return`), so it unifies with the `Integer`
        // then-arm rather than tripping T0068 — the wiki 404-route shape.
        let src = "oper f {} -> Integer [ \
                   let x = if true then [ 1 ] else [ return 2; ]; x ]; \
                   oper main {} [];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
        assert!(!codes(src).contains(&"T0068"), "unexpected T0068");
    }

    #[test]
    fn guard_return_no_else_no_t0069() {
        // A guard clause `if bad then [ return … ]` with no `else` — the
        // `Never` then-arm must not trip T0069.
        let src = "oper f { n: Integer } -> Integer [ if n = 0 then [ return 1; ]; 2 ]; \
                   oper main {} [];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
        assert!(!codes(src).contains(&"T0069"), "unexpected T0069");
    }

    #[test]
    fn terminal_if_else_both_return_is_never_no_t0009() {
        // A body that exits every path via `return`, ending in a
        // statement-position `if/else` both of whose arms return (no tail
        // expression), diverges — the fall-through is unreachable, so there is
        // no implicit `Tuple {}` and no T0009. The wiki `handle` shape, minimized.
        let src = "oper f { n: Integer } -> Text [ \
                   if n = 0 then [ return \"a\"; ] else [ return \"b\"; ]; ]; \
                   oper main {} [];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
        assert!(
            !codes(src).contains(&"T0009"),
            "unexpected T0009: {:?}",
            codes(src)
        );
    }

    #[test]
    fn then_only_guard_without_tail_still_t0009() {
        // A *then-only* guard can fall through (the false path continues), so a
        // body that is just a guard with no tail really can reach the implicit
        // `Tuple {}` — T0009 must still fire against the declared `Text` return.
        // Confirms the divergence rule wasn't over-broadened to then-only ifs.
        let src = "oper f { n: Integer } -> Text [ if n = 0 then [ return \"a\"; ]; ]; \
                   oper main {} [];";
        assert!(
            codes(src).contains(&"T0009"),
            "expected T0009, got {:?}",
            codes(src)
        );
    }

    #[test]
    fn return_inside_transaction_emits_t0093() {
        // An early `return` from within a `transaction [...]` would skip the
        // commit — rejected for now.
        let src = "oper f {} -> Integer [ transaction [ return 1; ]; 2 ]; oper main {} [];";
        assert!(codes(src).contains(&"T0093"), "got {:?}", codes(src));
    }

    // ── UFCS method calls (`x.m {}` ≡ `m { self: x }`) ───────────────

    #[test]
    fn ufcs_cardinality_on_sequence_resolves() {
        // `xs.cardinality {}` picks the `AnySequence` overload → Integer.
        let src =
            "oper main {} [ let xs = Sequence [ \"a\", \"b\" ]; let _n = xs.cardinality {}; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn ufcs_is_empty_on_relation_resolves() {
        // `r.is_empty {}` on a Relation resolves to Boolean (usable as a
        // condition), like `cardinality` but over `Relation H` only.
        let src = "oper main {} [ \
                   let r = Relation { { a: 1 } }; \
                   if r.is_empty {} then [ write_line { message: \"e\" }; ]; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn is_empty_on_non_relation_is_error() {
        // `is_empty` is registered over `Relation H` only — an Integer receiver
        // has no matching overload, so the call is a type error.
        let src = "oper main {} [ let n = 3; let _b = n.is_empty {}; ];";
        assert!(
            !diagnostics(src).is_empty(),
            "expected a diagnostic for is_empty on an Integer, got clean"
        );
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

    // ── `.cddb` base-relvar INIT values (`S := Relation { … };`) ──────

    #[test]
    fn init_matching_heading_typechecks_cleanly() {
        let src = "database d;\n\
                   base relvar X { a: Integer, b: Text } key { a };\n\
                   X := Relation { { a: 1, b: \"x\" }, { a: 2, b: \"y\" } };\n";
        assert!(
            diagnostics_cddb(src).is_empty(),
            "unexpected: {:?}",
            diagnostics_cddb(src)
        );
    }

    #[test]
    fn init_rational_and_composite_key_clean() {
        // The suppliers-and-parts shape: a Rational column and a composite key.
        let src = "database d;\n\
                   base relvar P { pno: Text, weight: Rational } key { pno };\n\
                   P := Relation { { pno: \"P1\", weight: 12.0 } };\n\
                   base relvar SP { sno: Text, pno: Text, qty: Integer } key { sno, pno };\n\
                   SP := Relation { { sno: \"S1\", pno: \"P1\", qty: 300 } };\n";
        assert!(
            diagnostics_cddb(src).is_empty(),
            "unexpected: {:?}",
            diagnostics_cddb(src)
        );
    }

    #[test]
    fn init_empty_relation_seeds_clean() {
        let src = "database d;\nbase relvar X { a: Integer } key { a };\nX := Relation {};\n";
        assert!(
            diagnostics_cddb(src).is_empty(),
            "unexpected: {:?}",
            diagnostics_cddb(src)
        );
    }

    #[test]
    fn init_cell_may_be_a_constant_expression() {
        // A cell is any constant expression, not just a literal.
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n\
                   X := Relation { { a: 2 * 3 + 1 } };\n";
        assert!(
            diagnostics_cddb(src).is_empty(),
            "unexpected: {:?}",
            diagnostics_cddb(src)
        );
    }

    #[test]
    fn init_integer_seeds_rational_column_clean() {
        // Integer→Rational widening for a constant seed (`12` into a Rational).
        let src = "database d;\nbase relvar P { pno: Text, weight: Rational } key { pno };\n\
                   P := Relation { { pno: \"P1\", weight: 12 } };\n";
        assert!(
            diagnostics_cddb(src).is_empty(),
            "unexpected: {:?}",
            diagnostics_cddb(src)
        );
    }

    #[test]
    fn init_unknown_relvar_diagnoses_t0102() {
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n\
                   Y := Relation { { a: 1 } };\n";
        assert!(codes_cddb(src).contains(&"T0102"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_on_virtual_relvar_diagnoses_t0103() {
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n\
                   virtual relvar V = X;\n\
                   V := Relation { { a: 1 } };\n";
        assert!(codes_cddb(src).contains(&"T0103"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn duplicate_init_diagnoses_t0104() {
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n\
                   X := Relation { { a: 1 } };\n\
                   X := Relation { { a: 2 } };\n";
        assert!(codes_cddb(src).contains(&"T0104"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_non_relation_rhs_diagnoses_t0105() {
        let src = "database d;\nbase relvar X { a: Integer } key { a };\nX := 42;\n";
        assert!(codes_cddb(src).contains(&"T0105"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_missing_attribute_diagnoses_t0106() {
        let src = "database d;\nbase relvar X { a: Integer, b: Text } key { a };\n\
                   X := Relation { { a: 1 } };\n";
        assert!(codes_cddb(src).contains(&"T0106"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_extra_attribute_diagnoses_t0106() {
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n\
                   X := Relation { { a: 1, c: 2 } };\n";
        assert!(codes_cddb(src).contains(&"T0106"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_type_mismatch_diagnoses_t0108() {
        let src = "database d;\nbase relvar X { a: Text } key { a };\n\
                   X := Relation { { a: 5 } };\n";
        assert!(codes_cddb(src).contains(&"T0108"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_relvar_read_diagnoses_t0107() {
        let src = "database d;\n\
                   base relvar X { a: Integer } key { a };\n\
                   base relvar Y { a: Integer } key { a };\n\
                   X := Y;\n";
        assert!(codes_cddb(src).contains(&"T0107"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_side_effecting_call_diagnoses_t0107() {
        // `write_relation` is a side-effecting built-in — not a constant value.
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n\
                   X := write_relation { rel: Relation {} };\n";
        assert!(codes_cddb(src).contains(&"T0107"), "{:?}", codes_cddb(src));
    }

    #[test]
    fn init_unknown_name_is_t0001_not_t0107() {
        // A bare name that resolves to nothing is a plain unresolved-name
        // error, not a constant-ness error (matches the module-let discipline).
        let src = "database d;\nbase relvar X { a: Integer } key { a };\n\
                   X := Relation { { a: foo } };\n";
        let cs = codes_cddb(src);
        assert!(cs.contains(&"T0001"), "{cs:?}");
        assert!(!cs.contains(&"T0107"), "{cs:?}");
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
    fn relation_lit_from_tuple_variable_checks_clean() {
        // A tuple-valued expression (a local) is a valid element — `Relation { t }`
        // is the singleton relation containing `t`. This is the motivating case.
        let src = "oper main {} [ \
                   let t = {a: 1, b: \"x\"}; \
                   let _r = Relation { t }; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn relation_lit_mixes_literal_and_expression_elements() {
        // A tuple literal and a tuple variable with the same heading coexist.
        let src = "oper main {} [ \
                   let t = {a: 2}; \
                   let _r = Relation { {a: 1}, t }; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn relation_lit_non_tuple_element_diagnoses_t0096() {
        // A non-tuple element (an Integer) is rejected — a relation is a set of
        // tuples. This moved from the parser (retired P0032) to typecheck.
        let src = "oper main {} [ let _r = Relation { 42 }; ];";
        assert!(codes(src).contains(&"T0096"), "{:?}", diagnostics(src));
    }

    #[test]
    fn relation_lit_element_heading_mismatch_diagnoses_t0019() {
        // Two elements with different headings — the second differs from the
        // first (whether both are literals or one is a variable).
        let lit = "oper main {} [ let _r = Relation { {a: 1}, {b: 2} }; ];";
        assert!(codes(lit).contains(&"T0019"), "{:?}", diagnostics(lit));
        let var = "oper main {} [ \
                   let t = {b: 2}; \
                   let _r = Relation { {a: 1}, t }; \
                   ];";
        assert!(codes(var).contains(&"T0019"), "{:?}", diagnostics(var));
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
    fn relation_equality_and_subset_typecheck() {
        // The relation overloads: `=`/`<>` are observational set equality
        // (RM Pre 8), `<= >= < >` the subset family — all Boolean-valued,
        // identical headings required. No T0021, no T0038.
        let src = "oper main {} [ \
             let r = Relation { { a: 1 }, { a: 2 } }; \
             let s = Relation { { a: 2 } }; \
             let _eq = r = s; \
             let _ne = r <> s; \
             let _le = s <= r; \
             let _ge = r >= s; \
             let _lt = s < r; \
             let _gt = r > s; \
         ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn relation_comparison_heading_mismatch_diagnoses_t0038() {
        // Different headings can't compare — the same rule (and code) as
        // `union`/`minus`, naming the differing attribute.
        let src = "oper main {} [ \
             let r = Relation { { a: 1 } }; \
             let s = Relation { { b: 1 } }; \
             let _eq = r = s; \
             let _le = r <= s; \
         ];";
        let cs = codes(src);
        assert_eq!(
            cs.iter().filter(|c| **c == "T0038").count(),
            2,
            "both comparisons diagnose T0038: {cs:?}"
        );
        assert!(!cs.contains(&"T0021"), "{cs:?}");
    }

    #[test]
    fn relation_scalar_mix_diagnoses_t0021() {
        // A relation against a scalar is neither overload.
        let src = "oper main {} [ \
             let r = Relation { { a: 1 } }; \
             let _b = r = 1; \
             let _c = r <= 1; \
         ];";
        let cs = codes(src);
        assert_eq!(
            cs.iter().filter(|c| **c == "T0021").count(),
            2,
            "both mixes diagnose T0021: {cs:?}"
        );
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
    fn unary_sign_on_integer_and_rational_typecheck_clean() {
        let src = "oper main {} [ \
                   let _a = -5; \
                   let _b = +5; \
                   let _c = -(1/2); \
                   let _d = -2.0; \
                   let _e = - -3; \
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn unary_sign_result_type_matches_operand() {
        // `-5` is Integer, `-2.0` is Rational — each compares to a same-type
        // literal without a mismatch.
        let src = "oper main {} [ let _a = -5 = 3; let _b = -2.0 = 0.5; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn unary_sign_on_approximate_diagnoses_t0109() {
        let src = "oper main {} [ let _a = -2e0; ];";
        assert!(codes(src).contains(&"T0109"), "{:?}", codes(src));
    }

    #[test]
    fn unary_sign_on_boolean_diagnoses_t0109() {
        let src = "oper main {} [ let _a = -true; ];";
        assert!(codes(src).contains(&"T0109"), "{:?}", codes(src));
    }

    #[test]
    fn unary_sign_on_text_diagnoses_t0109() {
        let src = "oper main {} [ let _a = -\"x\"; ];";
        assert!(codes(src).contains(&"T0109"), "{:?}", codes(src));
    }

    #[test]
    fn cddb_init_allows_negative_rational_seed() {
        // Unary `-` closes the provision negative-Rational-weight seed gap:
        // `weight: -12.0` is now a valid constant INIT value.
        let src = "database d;\n\
                   base relvar P { pno: Text, weight: Rational } key { pno };\n\
                   P := Relation { { pno: \"P1\", weight: -12.0 } };\n";
        assert!(
            diagnostics_cddb(src).is_empty(),
            "unexpected: {:?}",
            diagnostics_cddb(src)
        );
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

    // ── group / ungroup ─────────────────────────────────────────────────

    #[test]
    fn group_consumes_attrs_into_a_relation_valued_attribute() {
        // {a, b, c} group {pq: {a, b}}: `pq` is accessible (a relation), `a`/`b`
        // are gone (consumed), `c` survives and partitions.
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2, c: 3} }; \
                  let s = r group {pq: {a, b}}; \
                  let u = extract s; let _pq = u.pq; let _c = u.c; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
        let gone = "oper main {} [ \
                    let r = Relation { {a: 1, b: 2, c: 3} }; \
                    let s = r group {pq: {a, b}}; \
                    let u = extract s; let _a = u.a; \
                    ];";
        assert!(codes(gone).contains(&"T0017"));
    }

    #[test]
    fn group_of_every_attribute_types_as_single_rva() {
        // Consuming the whole heading leaves only the RVA — legal (the result
        // has 0 or 1 tuples).
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2} }; \
                  let s = r group {all: {a, b}}; \
                  let u = extract s; let _all = u.all; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
    }

    #[test]
    fn group_unknown_attr_diagnoses_t0027() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let s = r group {pq: {nope}}; \
                   ];";
        assert!(codes(src).contains(&"T0027"));
    }

    #[test]
    fn group_same_attr_twice_diagnoses_t0028() {
        // Across pairs too — multi-pair group is simultaneous, so an attribute
        // is consumed at most once.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r group {pq: {a}, r2: {a}}; \
                   ];";
        assert!(codes(src).contains(&"T0028"));
    }

    #[test]
    fn group_new_name_collides_diagnoses_t0031() {
        // new name `c` collides with the surviving attribute `c`.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2, c: 3} }; \
                   let s = r group {c: {a, b}}; \
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn ungroup_inverts_group_typing() {
        // group then ungroup round-trips: `a`/`b` are back, `pq` gone.
        let ok = "oper main {} [ \
                  let r = Relation { {a: 1, b: 2, c: 3} }; \
                  let s = r group {pq: {a, b}} ungroup {pq}; \
                  let u = extract s; let _a = u.a; let _b = u.b; let _c = u.c; \
                  ];";
        assert!(diagnostics(ok).is_empty(), "{:?}", diagnostics(ok));
    }

    #[test]
    fn ungroup_non_rva_diagnoses_t0100() {
        // A scalar target and a tuple-valued target are both T0100 — only a
        // relation-valued attribute unnests.
        let scalar = "oper main {} [ \
                      let r = Relation { {a: 1, b: 2} }; \
                      let s = r ungroup {a}; \
                      ];";
        assert!(codes(scalar).contains(&"T0100"));
        let tuple = "oper main {} [ \
                     let r = Relation { {a: 1, t: {x: 1, y: 2}} }; \
                     let s = r ungroup {t}; \
                     ];";
        assert!(codes(tuple).contains(&"T0100"));
    }

    #[test]
    fn ungroup_lifted_collision_diagnoses_t0031() {
        // {a, pq: Relation{a}} ungroup {pq}: lifting `a` collides with the
        // surviving top-level `a` — rename before ungrouping.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1, b: 2} }; \
                   let s = r group {pq: {b}} rename {b: a} ungroup {pq};\
                   ];";
        assert!(codes(src).contains(&"T0031"));
    }

    #[test]
    fn group_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 group {pq: {a}}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    #[test]
    fn ungroup_non_relation_diagnoses_t0023() {
        let src = "oper main {} [ let s = 1 ungroup {pq}; ];";
        assert!(codes(src).contains(&"T0023"));
    }

    #[test]
    fn storage_backed_rva_or_tva_attribute_diagnoses_t0101() {
        // A public relvar cannot persist a relation- or tuple-valued attribute
        // (no SQL column form yet; the designed endpoint is decomposition in
        // the `.cdstore` layer). A private relvar is in-process state and may
        // hold either.
        let rva = "database d;\n\
                   public relvar Bad { a: Integer, b: Relation { x: Text } } key { a };";
        assert!(codes(rva).contains(&"T0101"));
        let tva = "database d;\n\
                   public relvar Bad { a: Integer, t: Tuple { x: Text } } key { a };";
        assert!(codes(tva).contains(&"T0101"));
        let private_ok = "private relvar Fine { a: Integer, b: Relation { x: Text } } key { a };";
        assert!(
            !codes(private_ok).contains(&"T0101"),
            "{:?}",
            diagnostics(private_ok)
        );
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
        let t0032 = d
            .iter()
            .find(|d| d.code == "T0032")
            .expect("expected T0032");
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
        assert_eq!(
            n,
            1,
            "only the shadowed `x` should warn: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn where_predicate_attrs_do_not_warn() {
        // The heading attr `a` injected into the predicate scope must not be
        // flagged unused (WhereAttr origin); only user `let`s warn. `_s` is
        // exempt, `r` is used, so the program is diagnostic-free.
        let src = "oper main {} [ let r = Relation { {a: 1}, {a: 2} }; let _s = r where a = 2; ];";
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
            d.iter()
                .all(|x| x.severity != coddl_diagnostics::Severity::Error),
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
        assert!(
            !c.contains(&"T0074"),
            "reassigning a `var` is legal: {:?}",
            diagnostics(src)
        );
        assert!(
            !c.contains(&"T0075"),
            "same-type value is fine: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn reassigning_let_binding_is_t0074() {
        let src = "oper main {} [ let x = 1; x := 2; ];";
        let c = codes(src);
        assert!(
            c.contains(&"T0074"),
            "expected T0074, got {:?}",
            diagnostics(src)
        );
        // The dedicated code fires — not the generic non-relvar assignment T0033.
        assert!(
            !c.contains(&"T0033"),
            "should not fall through to T0033: {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn var_reassignment_type_mismatch_is_t0075() {
        let src = "oper main {} [ var x := 1; x := \"s\"; ];";
        assert!(
            codes(src).contains(&"T0075"),
            "expected T0075, got {:?}",
            diagnostics(src)
        );
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
        assert_eq!(
            out.mutable_spans.len(),
            3,
            "decl + read + write; got {:?}",
            out.mutable_spans
        );
    }

    #[test]
    fn let_binding_has_no_mutable_spans() {
        let src = "oper main {} [ let x = 1; let _y = x; ];";
        let out = check(src, FileId(0), FileKind::Cd);
        assert!(
            out.mutable_spans.is_empty(),
            "an immutable `let` is not mutable: {:?}",
            out.mutable_spans
        );
    }

    #[test]
    fn var_read_but_never_reassigned_suggests_let_t0077() {
        // The analog of Rust's `unused_mut`: read, never written → use `let`.
        let src = "oper main {} [ var x := 1; let _y = x; ];";
        let d = diagnostics(src);
        let t = d
            .iter()
            .find(|d| d.code == "T0077")
            .expect("expected T0077");
        assert_eq!(t.severity, coddl_diagnostics::Severity::Warning);
    }

    #[test]
    fn reassigned_var_does_not_suggest_let() {
        let src = "oper main {} [ var x := 1; x := 2; let _y = x; ];";
        assert!(
            !codes(src).contains(&"T0077"),
            "a genuinely mutable var: {:?}",
            diagnostics(src)
        );
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
        assert!(
            codes(src).contains(&"T0078"),
            "expected T0078, got {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn uninitialized_var_assigned_then_read_is_clean() {
        // `var x;` with no annotation: the type is inferred from `x := 1`, and
        // the read follows the assignment — no T0078/T0079.
        let src = "oper main {} [ var x; x := 1; let _y = x; ];";
        let d = diagnostics(src);
        assert!(
            d.iter()
                .all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn read_before_assignment_is_t0079() {
        let src = "oper main {} [ var x; let _y = x; ];";
        assert!(
            codes(src).contains(&"T0079"),
            "expected T0079, got {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn both_arms_assign_then_read_is_clean() {
        // Full definite-assignment: assigned on every branch ⇒ assigned after.
        let src = "oper main {} [ var x; if true then [ x := 1; ] else [ x := 2; ]; let _y = x; ];";
        let d = diagnostics(src);
        assert!(
            d.iter()
                .all(|x| x.severity != coddl_diagnostics::Severity::Error),
            "expected no errors, got {d:?}"
        );
    }

    #[test]
    fn assign_in_only_one_arm_then_read_is_t0079() {
        // No `else` ⇒ the then-arm may not run ⇒ not definitely assigned after.
        let src = "oper main {} [ var x; if true then [ x := 1; ]; let _y = x; ];";
        assert!(
            codes(src).contains(&"T0079"),
            "expected T0079, got {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn assign_in_loop_body_then_read_after_is_t0079() {
        // A loop may run zero times ⇒ its body's assignments aren't definite.
        let src = "oper main {} [ var x; for i := 1 to 3 do [ x := i; ]; let _y = x; ];";
        assert!(
            codes(src).contains(&"T0079"),
            "expected T0079, got {:?}",
            diagnostics(src)
        );
    }

    #[test]
    fn inferred_type_fixed_by_first_assignment_is_t0075() {
        // `x`'s type is inferred `Integer` from `x := 1`; the `Text` write fails.
        let src = "oper main {} [ var x; x := 1; x := \"s\"; ];";
        assert!(
            codes(src).contains(&"T0075"),
            "expected T0075, got {:?}",
            diagnostics(src)
        );
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
        assert!(
            matches!(hint.ty, Type::Integer),
            "hint type was {:?}",
            hint.ty
        );
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
            d.iter()
                .all(|x| x.severity != coddl_diagnostics::Severity::Error),
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
            d.iter()
                .all(|x| x.severity != coddl_diagnostics::Severity::Error),
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
            d.iter()
                .all(|x| x.severity != coddl_diagnostics::Severity::Error),
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
        let src = "oper main {} [ let r = Relation { {a: 1}, {a: 2} }; let _s = r where a = 2; ];";
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
        assert!(
            !codes(FORMAT_HELLO).contains(&"T0004"),
            "{:?}",
            codes(FORMAT_HELLO)
        );
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

    // ── Module-level `let` (constant bindings) ────────────────────────────

    #[test]
    fn module_let_binds_and_resolves_in_bodies() {
        // A module-position `let` is a constant binding: annotated or
        // inferred, visible in every oper body.
        let src = "program p;\n\
                   let limit = 2 + 1;\n\
                   let greeting: Text = \"hi\";\n\
                   oper main {} [\n\
                       write_line { message: greeting };\n\
                       let x = limit + 1;\n\
                       write_line { message: format { template: f\"{x}\", args: { x: x } } };\n\
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn module_let_forward_reference_resolves_no_t0001() {
        // Order-independent like opers (the module-let sibling of
        // `user_oper_forward_reference_resolves_no_t0001`): `a` references
        // `b`, declared later.
        let src = "program p; let a = b + 1; let b = 2; oper main {} [ let _x = a; ];";
        let c = codes(src);
        assert!(!c.contains(&"T0001"), "{c:?}");
        assert!(!c.contains(&"T0097"), "{c:?}");
    }

    #[test]
    fn module_let_cycle_diagnoses_t0097() {
        let src = "program p; let a = b; let b = a; oper main {} [];";
        let c = codes(src);
        assert!(c.contains(&"T0097"), "{c:?}");
    }

    #[test]
    fn module_let_requires_constant_expression_t0098() {
        // A call is not a constant expression (purity derivation isn't
        // built yet) …
        let src = "program p; let x = to_text { self: 1 }; oper main {} [];";
        assert!(codes(src).contains(&"T0098"), "{:?}", codes(src));
        // … and a transaction (a relvar read) never is.
        let src2 = "program p;\n\
                    database d;\n\
                    public relvar R { a: Integer } key { a };\n\
                    let y = transaction [ R ];\n\
                    oper main {} [];";
        assert!(codes(src2).contains(&"T0098"), "{:?}", codes(src2));
    }

    #[test]
    fn module_let_missing_initializer_t0098() {
        let src = "program p; let x: Integer; oper main {} [];";
        assert!(codes(src).contains(&"T0098"), "{:?}", codes(src));
    }

    #[test]
    fn module_let_annotation_mismatch_t0010() {
        let src = "program p; let x: Integer = \"hi\"; oper main {} [];";
        assert!(codes(src).contains(&"T0010"), "{:?}", codes(src));
    }

    #[test]
    fn module_let_annotated_empty_relation_checks() {
        // The annotation supplies the heading an empty `Relation {}` can't
        // infer — `check_binding`'s discipline at module scope.
        let src = "program p;\n\
                   let none: Relation { a: Integer } = Relation {};\n\
                   oper main {} [ let _r = none; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn module_let_duplicate_name_t0060() {
        // One namespace per module: another let, or an oper, collides.
        let src = "program p; let x = 1; let x = 2; oper main {} [];";
        assert!(codes(src).contains(&"T0060"), "{:?}", codes(src));
        let src2 = "program p; oper f {} []; let f = 1; oper main {} [];";
        assert!(codes(src2).contains(&"T0060"), "{:?}", codes(src2));
    }

    #[test]
    fn oper_local_shadows_module_let() {
        // Scope lookup runs first, so a body-local `let x` shadows the
        // module binding — consistent with no-reserved-words.
        let src = "program p;\n\
                   let x = 1;\n\
                   oper main {} [ let x = \"hi\"; write_line { message: x }; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn reltrue_relfalse_resolve_bare_from_core() {
        // coddl::core is always in scope, so its module-level `let`s need no
        // import: reltrue/relfalse type as Relation {} and feed the nullary
        // algebra (times gating, minus, comparisons).
        let src = "program p;\n\
                   oper main {} [\n\
                       let r = Relation { { a: 1 } };\n\
                       let gate = reltrue minus (r project {});\n\
                       let _kept = r times gate;\n\
                       let _t = relfalse < reltrue;\n\
                   ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn user_binding_shadows_core_let() {
        // Library vocabulary is shadowable: a user module let (or local) named `reltrue`
        // shadows core's — no T0060, and the user's type wins.
        let src = "program p;\n\
                   let reltrue = 1;\n\
                   oper main {} [ let _x = reltrue + 1; ];";
        let diags = diagnostics(src);
        assert!(diags.is_empty(), "{diags:?}");
        let src2 = "program p;\n\
                    oper main {} [ let reltrue = \"hi\"; write_line { message: reltrue }; ];";
        let diags2 = diagnostics(src2);
        assert!(diags2.is_empty(), "{diags2:?}");
    }

    // ── Multi-unit checking (userspace module imports) ───────────────────

    fn program_codes(units: &[CheckUnit]) -> Vec<&'static str> {
        check_program(units)
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn imported_oper_resolves_when_imported() {
        let greet = "module greet;\noper hello {} [ write_line { message: \"hi\" }; ];\n";
        let app = "program app;\nuse module greet;\noper main {} [ hello {}; ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("greet")),
                source: greet,
                file: FileId(1),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        let c = program_codes(&units);
        assert!(
            !c.contains(&"T0001"),
            "hello must resolve via import: {c:?}"
        );
        assert!(!c.contains(&"T0092"), "{c:?}");
    }

    #[test]
    fn unimported_module_oper_is_unresolved_t0001() {
        // `greet` is in the program, but `app` never imports it, so its `hello`
        // is out of scope — opt-in, like the stdlib module precedent.
        let greet = "module greet;\noper hello {} [ ];\n";
        let app = "program app;\noper main {} [ hello {}; ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("greet")),
                source: greet,
                file: FileId(1),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        assert!(program_codes(&units).contains(&"T0001"));
    }

    #[test]
    fn own_oper_shadows_same_named_import() {
        // A local `hello` alongside an imported `greet::hello` is not a
        // redefinition (they live in separate tables) and the call binds locally.
        let greet = "module greet;\noper hello {} [ ];\n";
        let app =
            "program app;\nuse module greet;\noper hello {} [ ];\noper main {} [ hello {}; ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("greet")),
                source: greet,
                file: FileId(1),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        let c = program_codes(&units);
        assert!(
            !c.contains(&"T0060"),
            "own+imported same name is not a redef: {c:?}"
        );
        assert!(!c.contains(&"T0092"), "{c:?}");
        assert!(!c.contains(&"T0001"), "{c:?}");
    }

    #[test]
    fn unused_ambiguous_import_is_clean() {
        // Two modules exporting the same name coexist until it is actually used.
        let foo = "module foo;\noper hello {} [ ];\n";
        let bar = "module bar;\noper hello {} [ ];\n";
        let app = "program app;\nuse module foo;\nuse module bar;\noper main {} [ ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("foo")),
                source: foo,
                file: FileId(1),
            },
            CheckUnit {
                module: Some(ModulePath::parse("bar")),
                source: bar,
                file: FileId(2),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        assert!(!program_codes(&units).contains(&"T0092"));
    }

    #[test]
    fn ambiguous_import_on_use_diagnoses_t0092() {
        let foo = "module foo;\noper hello {} [ ];\n";
        let bar = "module bar;\noper hello {} [ ];\n";
        let app = "program app;\nuse module foo;\nuse module bar;\noper main {} [ hello {}; ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("foo")),
                source: foo,
                file: FileId(1),
            },
            CheckUnit {
                module: Some(ModulePath::parse("bar")),
                source: bar,
                file: FileId(2),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        assert!(program_codes(&units).contains(&"T0092"));
    }

    #[test]
    fn imported_module_let_resolves() {
        let config = "module config;\nlet limit = 40 + 2;\n";
        let app = "program app;\nuse module config;\noper main {} [ let _x = limit + 1; ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("config")),
                source: config,
                file: FileId(1),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        let c = program_codes(&units);
        assert!(
            !c.contains(&"T0001"),
            "limit must resolve via import: {c:?}"
        );
    }

    #[test]
    fn ambiguous_imported_module_let_diagnoses_t0092() {
        let foo = "module foo;\nlet limit = 1;\n";
        let bar = "module bar;\nlet limit = 2;\n";
        let app =
            "program app;\nuse module foo;\nuse module bar;\noper main {} [ let _x = limit; ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("foo")),
                source: foo,
                file: FileId(1),
            },
            CheckUnit {
                module: Some(ModulePath::parse("bar")),
                source: bar,
                file: FileId(2),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        assert!(program_codes(&units).contains(&"T0092"));
    }

    #[test]
    fn module_body_error_surfaces_against_its_file() {
        // Each module body is type-checked; an unresolved call inside `greet`
        // reports against `greet`'s FileId, not the entry's.
        let greet = "module greet;\noper hello {} [ nonexistent {}; ];\n";
        let app = "program app;\nuse module greet;\noper main {} [ hello {}; ];\n";
        let units = [
            CheckUnit {
                module: Some(ModulePath::parse("greet")),
                source: greet,
                file: FileId(7),
            },
            CheckUnit {
                module: None,
                source: app,
                file: FileId(0),
            },
        ];
        let diags = check_program(&units).diagnostics;
        let unresolved: Vec<_> = diags.iter().filter(|d| d.code == "T0001").collect();
        assert!(!unresolved.is_empty(), "module body must be checked");
        assert!(
            unresolved.iter().all(|d| d.span.file == FileId(7)),
            "error must carry greet's FileId: {unresolved:?}"
        );
    }
}

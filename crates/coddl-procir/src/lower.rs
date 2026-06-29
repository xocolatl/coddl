//! AST → ProcIR lowering pass.
//!
//! `lower(source, file)` runs the lexer, parser, typechecker, and (if
//! the result is diagnostic-free) walks the AST into a `Module`. The
//! walk mirrors the typechecker's structure: every `check_<x>` in
//! `coddl-types::checker` has a sibling `lower_<x>` here.
//!
//! Lowering is defined to be infallible on a clean typecheck. Cases
//! that aren't reachable on diagnostic-free input (an unresolved
//! callee, a malformed call) hit `unreachable!()`; tests cover the
//! reachable ones. The `L####` namespace is reserved for the future.

use std::collections::{HashMap, HashSet};

use coddl_diagnostics::{Diagnostic, FileId, Severity, Span};
use coddl_plan::{Plan, WritePolicy};
use coddl_syntax::ast::{
    AssignStmt, AstNode, BinaryExpr, BinaryOp, Block, BoolLit, CallExpr, DeleteStmt, Expr, ExprStmt,
    InsertStmt,
    ExtendExpr, FieldAccess, Item,
    LetStmt, Literal, NameRef, NamedArg, OperDecl, ProgramDecl, ProjectExpr, RelationLit, RenameExpr,
    ReplaceExpr, Root, Stmt, TcloseExpr, TransactionExpr, TruncateStmt, TupleLit, UnaryExpr, UnaryOp,
    UnwrapExpr, UpdateStmt, WrapExpr,
};
use coddl_syntax::SyntaxKind;
use coddl_types::{check, Heading, RelvarKind, RelvarTable, Type};

use coddl_relir::{
    CmpOp, Literal as RelLiteral, Predicate, RelExpr, ScalarBinOp, ScalarExpr, StorageOrigin,
};
use coddl_sqlemit::{
    emit_assignment, emit_insert_template, emit_replace_insert, emit_truncate, Dialect, SqlQuery,
    Value,
};

use crate::ir::{
    BasicBlock, BlockId, Const, Function, HeadingId, Inst, Module, PlanEntry, ProcType,
    PublicRelvarBinding, ScalarOp, Terminator, ValueId,
};

/// Surface name → C-ABI linkage name for each runtime extern. The
/// table is short by design; every entry corresponds to a built-in
/// operator the typechecker already knows.
const BUILTIN_EXTERNS: &[BuiltinExtern] = &[
    BuiltinExtern {
        surface: "write_line",
        linkage: "coddl_write_line",
        params: &[("message", ProcType::Text)],
        return_type: ProcType::Unit,
    },
    // `read_line { prompt: Text } -> Text`. Returns a Text by value; at the
    // C ABI the length crosses back through a trailing len-out pointer (the
    // backends synthesize it — see their `lower_call`), since the runtime
    // can't return a fat pointer.
    BuiltinExtern {
        surface: "read_line",
        linkage: "coddl_read_line",
        params: &[("prompt", ProcType::Text)],
        return_type: ProcType::Text,
    },
];

struct BuiltinExtern {
    surface: &'static str,
    linkage: &'static str,
    params: &'static [(&'static str, ProcType)],
    return_type: ProcType,
}

/// The output of one `lower` pass. `module` is `None` iff any error
/// diagnostic was emitted upstream — the lowering pass refuses to
/// shape an IR for a program that didn't typecheck cleanly.
#[derive(Debug)]
pub struct LowerOutput {
    pub module: Option<Module>,
    pub diagnostics: Vec<Diagnostic>,
    /// Each relvar-rooted relational subtree the cut pushed to SQL, paired
    /// with the SQL it lowered to — populated only by [`explain_with_plan`]
    /// (the `coddl explain` subcommand); always empty on the compile path so
    /// normal lowering pays nothing.
    pub relir: Vec<ExplainEntry>,
}

/// One pushed relational expression captured for `coddl explain`: the
/// as-lowered RelIR tree and the SQL it became. Only successful pushdowns are
/// captured — a successful push is a clean RelExpr root (the cut returns
/// immediately, so no sub-expression is captured twice).
#[derive(Debug, Clone)]
pub struct ExplainEntry {
    pub expr: RelExpr,
    pub sql: String,
}

/// Tokenize, parse, type-check, and lower `source` to ProcIR.
/// Lowering is `.cd`-only — `.cddb`, `.cdmap`, and `.cdstore` describe
/// storage shape that the typechecker and the (Phase 16) plan layer
/// consume; they have no procedural lowering.
pub fn lower(source: &str, file: FileId) -> LowerOutput {
    lower_with_plan(source, file, None)
}

/// Plan-aware lowering. The optional [`Plan`] carries the resolved
/// public relvars (with table names + column orderings + the canonical
/// SQLite path baked at compile time); the lowerer turns each entry
/// into one `Module::public_relvars` slot, emits `RelvarSlotInit` /
/// `RelvarSlotRelease` in `main`'s prologue/epilogue, and resolves
/// bare-name references against the relvar set. When `plan` is `None`,
/// behavior matches the legacy `lower()` path: no relvar slots, no
/// SQLite, no transaction externs.
pub fn lower_with_plan(source: &str, file: FileId, plan: Option<&Plan>) -> LowerOutput {
    lower_impl(source, file, plan, false)
}

/// Plan-aware lowering that also captures each pushed relational subtree's
/// RelIR tree + emitted SQL into [`LowerOutput::relir`], for the `coddl
/// explain` subcommand. Otherwise identical to [`lower_with_plan`].
pub fn explain_with_plan(source: &str, file: FileId, plan: Option<&Plan>) -> LowerOutput {
    lower_impl(source, file, plan, true)
}

fn lower_impl(
    source: &str,
    file: FileId,
    plan: Option<&Plan>,
    collect_relir: bool,
) -> LowerOutput {
    let check_out = check(source, file, coddl_syntax::FileKind::Cd);
    let has_errors = check_out
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error);
    if has_errors {
        return LowerOutput {
            module: None,
            diagnostics: check_out.diagnostics,
            relir: Vec::new(),
        };
    }
    let root = Root::cast(check_out.tree).expect("parser always returns a Root");
    let mut lowerer = Lowerer::new(file);
    lowerer.collect_relir = collect_relir;
    if let Some(plan) = plan {
        lowerer.absorb_plan(plan);
    }
    lowerer.absorb_private_relvars(&check_out.relvars);
    let module = lowerer.lower_root(&root);
    let relir = std::mem::take(&mut lowerer.relir);
    // Merge in any diagnostics the lowerer itself emitted (e.g.
    // T0022 for captures in `where` predicates). If the lowerer
    // emitted error-severity diagnostics, the IR is unsafe to
    // codegen — return no module.
    let mut diagnostics = check_out.diagnostics;
    diagnostics.extend(lowerer.diagnostics);
    let lower_errored = diagnostics.iter().any(|d| d.severity == Severity::Error);
    LowerOutput {
        module: if lower_errored { None } else { Some(module) },
        diagnostics,
        relir,
    }
}

struct Lowerer {
    program_name: String,
    functions: Vec<Function>,
    seen_externs: HashSet<&'static str>,
    /// Per-module interner: maps each unique `Heading` to a
    /// `HeadingId`. Phase 19 backends emit one static descriptor per
    /// entry; `ProcType::Relation(HeadingId)` and `Inst::RelationLit`
    /// reference these by id. Order is push-only (id == index).
    headings: Vec<Heading>,
    heading_ids: HashMap<Heading, HeadingId>,
    /// Source file for diagnostic spans the lowerer itself emits
    /// (e.g. T0022 captures).
    file: FileId,
    /// Lowering-time diagnostics. Merged into `LowerOutput::diagnostics`
    /// at the end of `lower()`.
    diagnostics: Vec<Diagnostic>,
    /// When set, capture each pushed relational subtree's RelIR + SQL into
    /// `relir` (for `coddl explain`). Off on the compile path, so normal
    /// lowering never clones a `RelExpr`.
    collect_relir: bool,
    /// Pushed relational subtrees captured for `coddl explain`, in lowering
    /// order. Empty unless `collect_relir` is set. Drained onto
    /// `LowerOutput::relir`.
    relir: Vec<ExplainEntry>,
    /// Counter for synthesized predicate function names
    /// (`__coddl_where_<n>`). Per-module; never reset.
    next_where: u32,
    /// Counter for synthesized `extend` helper names
    /// (`__coddl_extend_<n>`). Per-module; never reset.
    next_extend: u32,
    // Per-function state, reset on each `lower_oper_decl`.
    next_value: u32,
    next_block: u32,
    insts: Vec<Inst>,
    /// Stack of binding scopes. The outermost layer is the function's
    /// parameter scope; each `transaction [...]` block pushes a new
    /// layer; `let` statements insert into the topmost layer. Each
    /// entry stores the binding's `ValueId` and its `ProcType` so
    /// later walks (tuple construction, field access) know the static
    /// shape of a `NameRef` lookup result.
    locals: Vec<HashMap<String, (ValueId, ProcType)>>,
    /// Relation `let`-bindings whose RHS is a pushable relvar-rooted
    /// relational expression are recorded here as deferred `RelExpr`
    /// *aliases* instead of being materialized, so `build_rel_expr` sees
    /// through them (`let gg = Greetings; gg where id = 1` folds into one
    /// pushed query, and an unused `gg` emits nothing). Parallel to `locals`
    /// (same push/pop discipline); an alias carries no `ValueId`, so it never
    /// appears in `locals` and is invisible to scope-release.
    relexpr_aliases: Vec<HashMap<String, RelExpr>>,
    /// Type of every SSA value defined in the current function. Built
    /// up as each `Inst` is emitted; consulted by lowerings that need
    /// the static type of a base expression (notably field access on
    /// a let-bound tuple, where the heading lives in the source
    /// `ValueId`'s `ProcType::Tuple`).
    value_types: HashMap<ValueId, ProcType>,
    /// When lowering a `where`-predicate body, this holds a snapshot
    /// of the enclosing function's `locals` so the NameRef walk can
    /// detect captures (Phase 20 deferral, T0022). `None` outside any
    /// predicate body.
    outer_locals_for_capture: Option<Vec<HashMap<String, (ValueId, ProcType)>>>,
    /// Plan-derived public-relvar metadata, keyed by surface name. Each
    /// entry carries the heading id (interned at plan-absorption time)
    /// plus the table / columns / db info the backend needs at slot-
    /// init emission. Empty when the program declares no public
    /// relvars (or no plan was supplied).
    public_relvars: HashMap<String, PublicRelvarBinding>,
    /// Write policy per public relvar, keyed by surface name. A lowering-time
    /// authorization concern only (reject an assignment to a view), kept out of
    /// the IR's `PublicRelvarBinding` so the IR stays plan-independent. Base
    /// relvars are `ReadWrite`; relvars mapped to a catalog view are `ReadOnly`.
    public_relvar_write_policy: HashMap<String, WritePolicy>,
    /// Source-declaration order of public-relvar names. The lowerer
    /// emits `RelvarSlotInit` / `RelvarSlotRelease` in this order so
    /// the slot-global emission matches across both backends and
    /// per-run.
    public_relvar_order: Vec<String>,
    /// Database name from the `database <name>;` binding. Mirrors
    /// `Module::db_name`.
    db_name: Option<String>,
    /// Canonical absolute SQLite path baked at compile time. Mirrors
    /// `Module::db_path_default`.
    db_path_default: Option<String>,
    /// SQL dialect to bake pushed queries for, derived from the plan's
    /// backend. `Some` only when the backend is one the cut can push to
    /// (SQLite today); `None` disables pushdown so every relvar read takes
    /// the legacy in-process materialize path.
    dialect: Option<Dialect>,
    /// Baked query plans, in assignment order. Drained onto `Module::plans`.
    plans: Vec<PlanEntry>,
    /// Maps the storage layer's text-stable plan id (`coddl_sqlemit::PlanId`,
    /// a 64-bit text hash) to the dense per-module `u32` id, so an identical
    /// query baked twice registers (and executes against) one plan.
    plan_ids: HashMap<u64, u32>,
    /// Next dense plan id to hand out.
    next_plan_id: u32,
    /// Public relvars referenced via the legacy `RelvarRead` path (i.e. not
    /// pushed to SQL). Slot init/release in `main` is emitted only for these;
    /// fully-pushed (or unreferenced) relvars get no startup materialization.
    legacy_used_relvars: HashSet<String>,
    /// In-memory `private` relvars: surface name → interned heading id.
    /// Absorbed from the typechecker's relvar table; they have no SQL source,
    /// so their slots start empty and are filled by assignment.
    private_relvars: HashMap<String, HeadingId>,
    /// Private-relvar names in a stable (name-sorted) order, so slot
    /// init/release emits identically across backends and runs.
    private_relvar_order: Vec<String>,
    /// Private relvars actually read or assigned; only these get a slot
    /// init/release in `main`.
    used_private_relvars: HashSet<String>,
    /// SSA values that are *owned* heap `Text` payloads — produced by `||`
    /// (`Concat`/`CharToText`), `read_line`, or a retained `Text` alias. Only
    /// these are auto-released (at scope exit, or as consumed temporaries):
    /// a `Text` loaded out of a relation/tuple cell (`AttrLoad`/`TupleField`/
    /// `Extract`) is *borrowed* and must never be released here. Immortal
    /// literals are safe to release but aren't tracked (release no-ops on
    /// them). Function-global like `value_types` — must survive `pop_local_scope`
    /// so a transaction-escaping owned `Text` stays owned.
    owned_texts: HashSet<ValueId>,
    /// For each `TupleLit` (by dst `ValueId`): the `Text` cell values consumed
    /// directly into it — direct `Text` field values plus, recursively, the
    /// temps of *fresh nested `TupleLit`* fields (not `NameRef`-aliased tuples).
    /// `lower_relation_lit` drains the top-level tuples' entries and runs
    /// `release_text_temp` on each, balancing the relation's retain-on-store of
    /// an owned `Text` temp consumed into a cell. A standalone tuple's entry is
    /// never drained (its temps flow out via `.field` and are released there).
    tuple_cell_text_temps: HashMap<ValueId, Vec<ValueId>>,
    /// Relation temporaries whose release is deferred to **function** scope
    /// exit. `extract` copies a record's cells into a tuple as *borrowed*
    /// `(ptr,len)` values, then the source relation would normally be released
    /// at once — but the relation drop walker now frees its `Text` cells, which
    /// the borrowed fields still point at. Deferring the source's release to the
    /// function epilogue keeps those cells alive past every use (including after
    /// a `transaction [...]` the extract sat inside), with no leak (it *is*
    /// released, just last). Drained in `lower_oper_decl`.
    deferred_relation_releases: Vec<ValueId>,
}

impl Lowerer {
    fn new(file: FileId) -> Self {
        Self {
            program_name: String::new(),
            functions: Vec::new(),
            seen_externs: HashSet::new(),
            headings: Vec::new(),
            heading_ids: HashMap::new(),
            file,
            diagnostics: Vec::new(),
            collect_relir: false,
            relir: Vec::new(),
            next_where: 0,
            next_extend: 0,
            next_value: 0,
            next_block: 0,
            insts: Vec::new(),
            locals: vec![HashMap::new()],
            relexpr_aliases: vec![HashMap::new()],
            value_types: HashMap::new(),
            outer_locals_for_capture: None,
            public_relvars: HashMap::new(),
            public_relvar_write_policy: HashMap::new(),
            public_relvar_order: Vec::new(),
            db_name: None,
            db_path_default: None,
            dialect: None,
            plans: Vec::new(),
            plan_ids: HashMap::new(),
            next_plan_id: 0,
            legacy_used_relvars: HashSet::new(),
            private_relvars: HashMap::new(),
            private_relvar_order: Vec::new(),
            used_private_relvars: HashSet::new(),
            owned_texts: HashSet::new(),
            tuple_cell_text_temps: HashMap::new(),
            deferred_relation_releases: Vec::new(),
        }
    }

    /// Walk the plan's `resolved` list, intern each relvar's heading,
    /// and build the per-relvar `PublicRelvarBinding` the IR carries on
    /// `Module::public_relvars`. Also stash `db_name` /
    /// `db_path_default` so the codegen layer can emit the
    /// env-var-resolved path lookup at slot init.
    fn absorb_plan(&mut self, plan: &Plan) {
        self.db_name = plan.database_name.clone();
        self.db_path_default = plan.db_file_default.clone();
        // Only backends the cut can emit SQL for enable pushdown; others
        // leave `dialect` `None` and fall through to the legacy path.
        self.dialect = match plan.backend_kind {
            coddl_plan::BackendKind::Sqlite => Some(Dialect::SQLite),
            coddl_plan::BackendKind::Other(_) | coddl_plan::BackendKind::Unknown => None,
        };
        for r in &plan.resolved {
            let heading_id = self.intern_heading(&r.heading);
            let binding = PublicRelvarBinding {
                name: r.app_name.clone(),
                heading_id,
                table_name: r.table_name.clone(),
                columns: r.columns.clone(),
                keys: r.keys.clone(),
            };
            self.public_relvar_order.push(r.app_name.clone());
            self.public_relvars.insert(r.app_name.clone(), binding);
            self.public_relvar_write_policy
                .insert(r.app_name.clone(), r.write_policy);
        }
    }

    /// Absorb `private` relvars from the typechecker's relvar table: intern
    /// each heading and record it for in-memory slot storage. They have no
    /// plan entry (no SQL source). Name-sorted for deterministic emission.
    fn absorb_private_relvars(&mut self, relvars: &RelvarTable) {
        let mut privs: Vec<_> = relvars
            .iter()
            .filter(|(_, info)| matches!(info.kind, RelvarKind::Private))
            .collect();
        privs.sort_by(|a, b| a.0.cmp(b.0));
        for (name, info) in privs {
            let heading_id = self.intern_heading(&info.heading);
            self.private_relvar_order.push(name.to_string());
            self.private_relvars.insert(name.to_string(), heading_id);
        }
    }

    /// Compute a `Span` for an AST node — used when the lowerer emits
    /// a diagnostic against a specific subtree (e.g. T0022 against
    /// the offending `NameRef`).
    fn node_span(&self, node: &coddl_syntax::cst::SyntaxNode) -> Span {
        let r = node.text_range();
        Span::new(self.file, r.start().into(), r.end().into())
    }

    /// Intern a heading: return its existing `HeadingId` or push a new
    /// one. Stable order; backends iterate `Module::headings` in this
    /// order when emitting descriptors.
    fn intern_heading(&mut self, h: &Heading) -> HeadingId {
        if let Some(id) = self.heading_ids.get(h) {
            return *id;
        }
        let id = HeadingId(self.headings.len() as u32);
        self.headings.push(h.clone());
        self.heading_ids.insert(h.clone(), id);
        id
    }

    fn push_local_scope(&mut self) {
        self.locals.push(HashMap::new());
        self.relexpr_aliases.push(HashMap::new());
    }

    fn pop_local_scope(&mut self) {
        self.locals.pop();
        self.relexpr_aliases.pop();
    }

    fn bind_local(&mut self, name: String, v: ValueId, ty: ProcType) {
        self.locals
            .last_mut()
            .expect("scope stack empty")
            .insert(name, (v, ty));
    }

    fn lookup_local(&self, name: &str) -> Option<(ValueId, ProcType)> {
        self.locals
            .iter()
            .rev()
            .find_map(|l| l.get(name).cloned())
    }

    /// Record a relation `let`-binding as a deferred `RelExpr` alias (see
    /// `relexpr_aliases`). The binding emits no instruction; `build_rel_expr`
    /// substitutes the stored expression wherever the name is used.
    fn bind_alias(&mut self, name: String, rel: RelExpr) {
        self.relexpr_aliases
            .last_mut()
            .expect("scope stack empty")
            .insert(name, rel);
    }

    /// Resolve a name to its deferred `RelExpr` alias, innermost scope first.
    fn lookup_alias(&self, name: &str) -> Option<&RelExpr> {
        self.relexpr_aliases.iter().rev().find_map(|l| l.get(name))
    }

    fn fresh_value(&mut self) -> ValueId {
        let v = ValueId(self.next_value);
        self.next_value += 1;
        v
    }

    fn fresh_block(&mut self) -> BlockId {
        let b = BlockId(self.next_block);
        self.next_block += 1;
        b
    }

    fn reset_function_state(&mut self) {
        self.next_value = 0;
        self.next_block = 0;
        self.insts.clear();
        self.locals.clear();
        self.locals.push(HashMap::new());
        self.relexpr_aliases.clear();
        self.relexpr_aliases.push(HashMap::new());
        self.value_types.clear();
        self.owned_texts.clear();
        self.tuple_cell_text_temps.clear();
        self.deferred_relation_releases.clear();
    }

    /// Look up an SSA value's static type. Diagnostic-free input always
    /// has a recorded type for every consumed value; an unbound id is
    /// an internal lowerer bug.
    fn value_type(&self, v: ValueId) -> ProcType {
        self.value_types
            .get(&v)
            .cloned()
            .unwrap_or_else(|| unreachable!("unbound ValueId {v}"))
    }

    /// Bind a freshly-defined SSA value to its `ProcType`. Every
    /// instruction-emission helper goes through this so `value_types`
    /// stays in sync without per-call-site duplication.
    fn record_type(&mut self, v: ValueId, ty: ProcType) {
        self.value_types.insert(v, ty);
    }

    /// True iff `ty` describes an always-heap-managed value that needs RC
    /// retain/release regardless of provenance. Relations always allocate;
    /// `Text` is provenance-dependent (owned vs borrowed) and handled
    /// separately via `owned_texts` — see [`Self::needs_scope_release`].
    fn is_heap_managed(ty: &ProcType) -> bool {
        matches!(ty, ProcType::Relation(_))
    }

    /// Whether a *scope-bound local* `v` of type `ty` must be released at
    /// scope exit: any relation, or an **owned** heap `Text` (a borrowed
    /// `Text` loaded from a cell is excluded — releasing it would be a
    /// premature free).
    fn needs_scope_release(&self, v: ValueId, ty: &ProcType) -> bool {
        Self::is_heap_managed(ty)
            || (matches!(ty, ProcType::Text) && self.owned_texts.contains(&v))
    }

    /// Mark `v` as an owned heap `Text` (a `||` result, `read_line` result,
    /// or retained alias). No-op for non-`Text`, but callers gate on type.
    fn mark_text_owned(&mut self, v: ValueId) {
        self.owned_texts.insert(v);
    }

    /// Emit a `Release` for each deferred `extract`-source relation into the
    /// current instruction stream (the function/helper epilogue). The list is
    /// drained, so it's safe to call once per function or helper body.
    fn drain_deferred_relation_releases(&mut self) {
        for src in std::mem::take(&mut self.deferred_relation_releases) {
            self.insts.push(Inst::Release { src });
        }
    }

    /// Release an owned heap `Text` *temporary* — one consumed by a borrowing
    /// operator (`Concat`/`coddl_text_eq`/a builtin call) that no local owns.
    /// A let-bound owned `Text` is left for scope-exit release; a borrowed
    /// `Text` (literal or cell-loaded) is never in `owned_texts`. No-op for
    /// any non-owned or non-`Text` value, so callers can invoke it blanketly.
    fn release_text_temp(&mut self, v: ValueId) {
        if !self.owned_texts.contains(&v) {
            return;
        }
        let owned_by_local = self
            .locals
            .iter()
            .any(|layer| layer.values().any(|(vid, _)| *vid == v));
        if !owned_by_local {
            self.insts.push(Inst::Release { src: v });
        }
    }

    /// Emit `Inst::Release` for every heap-managed binding in the
    /// topmost local scope, in unspecified (HashMap) order. Called
    /// before popping a scope (transaction exit) and at function
    /// epilogue, before any terminator or runtime-shutdown call.
    fn release_top_scope_heap_locals(&mut self) {
        let candidates: Vec<(ValueId, ProcType)> = self
            .locals
            .last()
            .expect("scope stack empty")
            .values()
            .cloned()
            .collect();
        for (v, ty) in candidates {
            if self.needs_scope_release(v, &ty) {
                self.insts.push(Inst::Release { src: v });
            }
        }
    }

    fn lookup_extern(&self, surface: &str) -> Option<&'static BuiltinExtern> {
        BUILTIN_EXTERNS.iter().find(|e| e.surface == surface)
    }

    fn ensure_extern(&mut self, ext: &'static BuiltinExtern) {
        if !self.seen_externs.insert(ext.surface) {
            return;
        }
        self.functions.push(Function {
            name: ext.surface.to_string(),
            linkage_name: ext.linkage.to_string(),
            params: ext
                .params
                .iter()
                .map(|(n, t)| ((*n).to_string(), t.clone()))
                .collect(),
            return_type: ext.return_type.clone(),
            blocks: Vec::new(),
        });
    }

    /// Register a runtime entry-point extern whose name *is* its
    /// linkage symbol (`coddl_runtime_init`, `coddl_runtime_shutdown`).
    /// Used by `main`'s init/shutdown wrapping; the synthetic extern
    /// participates in the same `seen_externs` deduplication as the
    /// builtin-mapped externs.
    fn ensure_runtime_extern(&mut self, linkage: &'static str) {
        if !self.seen_externs.insert(linkage) {
            return;
        }
        self.functions.push(Function {
            name: linkage.to_string(),
            linkage_name: linkage.to_string(),
            params: Vec::new(),
            return_type: ProcType::Integer,
            blocks: Vec::new(),
        });
    }

    // ── Walks ────────────────────────────────────────────────────────

    fn lower_root(&mut self, root: &Root) -> Module {
        for item in root.items() {
            match item {
                Item::ProgramDecl(p) => self.lower_program_decl(&p),
                Item::DatabaseBinding(_) => {
                    // The binding is a parse-time label today; runtime
                    // wiring lands with Phase 21's SQLite materialization.
                }
                Item::PublicRelvarDecl(_)
                | Item::PrivateRelvarDecl(_)
                | Item::BaseRelvarDecl(_)
                | Item::VirtualRelvarDecl(_) => {
                    // Relvar declarations don't lower yet — they
                    // describe storage shape that the typechecker
                    // collects into the relvar table (Phase 15).
                    // Storage init lands in Phase 21.
                }
                Item::OperDecl(o) => {
                    let func = self.lower_oper_decl(&o);
                    self.functions.push(func);
                }
            }
        }
        // Now that every function is lowered (so `plans` and
        // `legacy_used_relvars` are final), patch `main`'s prologue:
        // register the database + pushed plans, and emit slot init/release
        // only for relvars still read in-process.
        self.finalize_main_prologue();
        let public_relvars: Vec<PublicRelvarBinding> = self
            .public_relvar_order
            .iter()
            .map(|name| {
                self.public_relvars
                    .get(name)
                    .cloned()
                    .expect("public_relvar_order names live in public_relvars")
            })
            .collect();
        let private_relvar_slots: Vec<(String, HeadingId)> = self
            .private_relvar_order
            .iter()
            .filter(|n| self.used_private_relvars.contains(*n))
            .map(|n| (n.clone(), self.private_relvars[n]))
            .collect();
        Module {
            program_name: std::mem::take(&mut self.program_name),
            functions: std::mem::take(&mut self.functions),
            headings: std::mem::take(&mut self.headings),
            public_relvars,
            db_path_default: self.db_path_default.take(),
            db_name: self.db_name.take(),
            plans: std::mem::take(&mut self.plans),
            private_relvar_slots,
        }
    }

    /// Insert `main`'s prologue registration and slot init/release after
    /// the body is fully lowered. Runs once in `lower_root`. The database
    /// and plan registrations go right after `coddl_runtime_init`; slot
    /// init/release cover only relvars referenced via the legacy path.
    fn finalize_main_prologue(&mut self) {
        // Build the insts from immutable reads of `self` before taking the
        // mutable borrow on `self.functions`.
        let mut prologue: Vec<Inst> = Vec::new();
        if !self.plans.is_empty() {
            prologue.push(Inst::RegisterDatabase);
            for p in &self.plans {
                prologue.push(Inst::RegisterPlan { plan_id: p.plan_id });
            }
        }
        for name in &self.public_relvar_order {
            if self.legacy_used_relvars.contains(name) {
                let heading_id = self.public_relvars[name].heading_id;
                prologue.push(Inst::RelvarSlotInit {
                    name: name.clone(),
                    heading_id,
                });
            }
        }
        // Private (in-memory) relvars: init an empty slot for each used one.
        for name in &self.private_relvar_order {
            if self.used_private_relvars.contains(name) {
                let heading_id = self.private_relvars[name];
                prologue.push(Inst::PrivateRelvarSlotInit {
                    name: name.clone(),
                    heading_id,
                });
            }
        }
        let mut releases: Vec<Inst> = self
            .public_relvar_order
            .iter()
            .filter(|n| self.legacy_used_relvars.contains(*n))
            .map(|n| Inst::RelvarSlotRelease { name: n.clone() })
            .collect();
        for name in &self.private_relvar_order {
            if self.used_private_relvars.contains(name) {
                releases.push(Inst::RelvarSlotRelease { name: name.clone() });
            }
        }
        if prologue.is_empty() && releases.is_empty() {
            return;
        }

        let main = match self.functions.iter_mut().find(|f| f.name == "main") {
            Some(f) => f,
            None => return,
        };
        let block = match main.blocks.first_mut() {
            Some(b) => b,
            None => return,
        };
        let at = block
            .insts
            .iter()
            .position(|i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_runtime_init"))
            .map(|p| p + 1)
            .unwrap_or(0);
        block.insts.splice(at..at, prologue);
        if !releases.is_empty() {
            if let Some(sp) = block.insts.iter().position(
                |i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_runtime_shutdown"),
            ) {
                block.insts.splice(sp..sp, releases);
            }
        }
    }

    fn lower_program_decl(&mut self, decl: &ProgramDecl) {
        if let Some(name_tok) = decl.name() {
            self.program_name = name_tok.text().to_string();
        }
    }

    fn lower_oper_decl(&mut self, decl: &OperDecl) -> Function {
        self.reset_function_state();

        let name = decl
            .name()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        // Defined functions: surface name is the linkage name for now.
        // Adding name mangling — for overloading or module-scoped
        // symbols — slots in here once it arrives.
        let linkage_name = name.clone();

        let mut params: Vec<(String, ProcType)> = Vec::new();
        if let Some(heading) = decl.heading() {
            for param in heading.params() {
                let pname = param
                    .name()
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let pty = param
                    .type_ref()
                    .and_then(|tr| tr.name())
                    .map(|t| proc_type_from_builtin_name(t.text()))
                    .unwrap_or(ProcType::Unit);
                params.push((pname, pty));
            }
        }

        let is_main = name == "main";

        // Resolve the declared return type. Default = Unit. Main is
        // treated as Unit at the IR level (the backends special-case
        // `ret i32 0`); the typechecker rejects a declared non-Unit
        // return on `main` with T0011, so this assignment is safe.
        let declared_return = decl
            .return_type()
            .and_then(|tr| tr.name())
            .map(|t| proc_type_from_builtin_name(t.text()))
            .unwrap_or(ProcType::Unit);

        let block_id = self.fresh_block();

        // The compiled program's startup must call the runtime before
        // touching any other extern (docs/runtime.md). Today the
        // stubs are no-ops, but wiring it now means future runtime
        // work — DB connection pool, prepared-statement cache,
        // arena setup — slots in without a codegen change.
        if is_main {
            self.ensure_runtime_extern("coddl_runtime_init");
            let dst = self.fresh_value();
            self.record_type(dst, ProcType::Integer);
            self.insts.push(Inst::Call {
                dst: Some(dst),
                callee: "coddl_runtime_init".to_string(),
                args: Vec::new(),
                return_type: ProcType::Integer,
            });
            // Database / plan registration and per-relvar slot init are
            // emitted into the prologue by `finalize_main_prologue` once the
            // body is lowered — only then is it known which relvars were
            // pushed to SQL (served by `coddl_query`) versus read in-process
            // (materialized via `coddl_sqlite_relvar_init`).
        }

        let body_value = decl.body().map(|body| self.lower_block(&body));

        // Release every heap-typed function-scope local before either
        // the runtime-shutdown call (main) or the terminator (others).
        // Phase 19 doesn't yet return heap values from functions, so
        // we can release everything in the function scope here.
        self.release_top_scope_heap_locals();
        // Then the deferred `extract`-source relations — released last, after
        // every borrowed field they fed has been consumed.
        self.drain_deferred_relation_releases();

        if is_main {
            // Per-relvar slot releases are inserted before this shutdown
            // call by `finalize_main_prologue`, mirroring the slot inits it
            // emits. The runtime's own `coddl_runtime_shutdown` also frees
            // any slot still alive (defense in depth).
            self.ensure_runtime_extern("coddl_runtime_shutdown");
            let dst = self.fresh_value();
            self.record_type(dst, ProcType::Integer);
            self.insts.push(Inst::Call {
                dst: Some(dst),
                callee: "coddl_runtime_shutdown".to_string(),
                args: Vec::new(),
                return_type: ProcType::Integer,
            });
        }

        // Non-main opers with a non-Unit declared return carry their
        // body's tail-expression value out via `Return(Some(v))`.
        // Main + Unit-returning opers use `Return(None)`; the backend
        // special-cases main as `ret i32 0`.
        let terminator = if !is_main && !matches!(declared_return, ProcType::Unit) {
            match body_value {
                Some(v) => Terminator::Return(Some(v)),
                None => Terminator::Return(None),
            }
        } else {
            Terminator::Return(None)
        };

        let block = BasicBlock {
            id: block_id,
            insts: std::mem::take(&mut self.insts),
            terminator,
        };

        Function {
            name,
            linkage_name,
            params,
            return_type: declared_return,
            blocks: vec![block],
        }
    }

    /// Lower a block. Returns the block's value — the tail
    /// expression's `ValueId` if there is one, otherwise a fresh
    /// placeholder representing Unit (never consumed downstream).
    fn lower_block(&mut self, block: &Block) -> ValueId {
        for stmt in block.statements() {
            match stmt {
                Stmt::Let(l) => self.lower_let_stmt(&l),
                Stmt::Assign(a) => self.lower_assign_stmt(&a),
                Stmt::Truncate(t) => self.lower_truncate_stmt(&t),
                Stmt::Delete(d) => self.lower_delete_stmt(&d),
                Stmt::Insert(i) => self.lower_insert_stmt(&i),
                Stmt::Update(u) => self.lower_update_stmt(&u),
                Stmt::ExprStmt(e) => self.lower_expr_stmt(&e),
            }
        }
        match block.tail_expr() {
            Some(expr) => self.lower_expr(&expr),
            None => {
                let v = self.fresh_value();
                self.record_type(v, ProcType::Unit);
                v
            }
        }
    }

    /// Lower a relational assignment `R := <expr>;`. A **private** target stores
    /// the RHS relation value into its in-memory slot (move semantics; the slot
    /// owns the value, the runtime releases the previous one). A **public**
    /// target is a write to the SQL-backed relvar: the RHS is recognized as an
    /// assignment shape and emitted as surgical DML, never hydrated.
    fn lower_assign_stmt(&mut self, stmt: &AssignStmt) {
        let Some(target_expr) = stmt.target() else { return };
        let Expr::NameRef(target) = &target_expr else {
            return; // typechecker rejected a non-name target (T0033)
        };
        let Some(name_tok) = target.ident() else { return };
        let name = name_tok.text().to_string();
        let Some(value_expr) = stmt.value() else { return };

        // `R := R` does nothing — elide it entirely (the typechecker already
        // warned, T0051). This holds for both a public and a private target.
        if matches!(&value_expr, Expr::NameRef(v) if v.ident().is_some_and(|t| t.text() == name)) {
            return;
        }

        // Public target → surgical DML via assignment-RHS recognition.
        if self.public_relvars.contains_key(&name) {
            self.lower_public_assign(&name, &target_expr, &value_expr);
            return;
        }

        // Private target → in-memory slot store.
        let value = self.lower_expr(&value_expr);
        self.used_private_relvars.insert(name.clone());
        self.insts.push(Inst::RelvarSlotStore { name, value });
    }

    /// Lower `truncate R;` — clear every tuple. It desugars to `R := R minus R`:
    /// a **public** relvar hits the self-subtraction arm of `emit_assignment`
    /// (a whole-table `DELETE FROM t`, never hydrated); a **private** relvar
    /// stores the empty `R minus R` value back into its in-memory slot.
    fn lower_truncate_stmt(&mut self, stmt: &TruncateStmt) {
        let Some(operand) = stmt.operand() else { return };
        let Expr::NameRef(target) = &operand else {
            return; // typechecker rejected a non-name operand (T0033)
        };
        let Some(name_tok) = target.ident() else { return };
        let name = name_tok.text().to_string();

        // Public target → surgical whole-table delete via the `R := R minus R`
        // self-subtraction shape.
        if self.public_relvars.contains_key(&name) {
            let Some(dialect) = self.require_public_write(&name, &operand) else {
                return;
            };
            let Some(target_rel) = self.build_rel_expr(&operand) else {
                return;
            };
            let value_rel = RelExpr::Minus {
                lhs: Box::new(target_rel.clone()),
                rhs: Box::new(target_rel.clone()),
            };
            if let Ok(query) = emit_assignment(&target_rel, &value_rel, dialect) {
                self.emit_dml(query);
            }
            return;
        }

        // Private target → `R minus R` is the empty relation; store it into the
        // slot (the two reads lower exactly as the literal `R minus R` would).
        let lhs = self.lower_expr(&operand);
        let rhs = self.lower_expr(&operand);
        let value = self.emit_minus(lhs, rhs);
        self.used_private_relvars.insert(name.clone());
        self.insts.push(Inst::RelvarSlotStore { name, value });
    }

    /// Lower `delete R where p;` — remove the matching tuples. It desugars to
    /// `R := R minus (R where p)`: a **public** relvar hits the DELETE arm of
    /// `emit_assignment` (`DELETE FROM t WHERE p`, never hydrated); a **private**
    /// relvar stores the kept rows `R minus (R where p)` back into its slot.
    fn lower_delete_stmt(&mut self, stmt: &DeleteStmt) {
        let Some(operand) = stmt.operand() else { return };
        // The operand is the `where`-restriction `R where p` (typecheck guarantees
        // the shape); the relvar is the `where` lhs.
        let Expr::Binary(bin) = &operand else { return };
        let Some(lhs_expr) = bin.lhs() else { return };
        let Expr::NameRef(target) = &lhs_expr else { return };
        let Some(name_tok) = target.ident() else { return };
        let name = name_tok.text().to_string();

        // Public target → surgical `DELETE FROM t WHERE p` via the
        // `R := R minus (R where p)` shape.
        if self.public_relvars.contains_key(&name) {
            let Some(dialect) = self.require_public_write(&name, &operand) else {
                return;
            };
            // Build the target `RelvarRef` and the restriction `Restrict{t, p}`,
            // then the `Minus{t, Restrict}` the DELETE arm recognizes.
            let recognized = self
                .build_rel_expr(&lhs_expr)
                .zip(self.build_rel_expr(&operand))
                .and_then(|(t, restrict)| {
                    let value = RelExpr::Minus {
                        lhs: Box::new(t.clone()),
                        rhs: Box::new(restrict),
                    };
                    emit_assignment(&t, &value, dialect).ok()
                });
            match recognized {
                Some(query) => self.emit_dml(query),
                // The predicate didn't push (a restriction the single-predicate
                // model can't express surgically): decline rather than hydrate —
                // never a silent partial delete.
                None => self.diagnostics.push(Diagnostic::error(
                    self.node_span(operand.syntax()),
                    "T0049",
                    format!(
                        "cannot delete from public relvar `{name}`: predicate is not a \
                         recognized surgical shape"
                    ),
                )),
            }
            return;
        }

        // Private target → the kept rows `R minus (R where p)` stored back (the
        // operands lower exactly as the literal `R minus (R where p)` would).
        let lhs_val = self.lower_expr(&lhs_expr);
        let rhs_val = self.lower_expr(&operand);
        let value = self.emit_minus(lhs_val, rhs_val);
        self.used_private_relvars.insert(name.clone());
        self.insts.push(Inst::RelvarSlotStore { name, value });
    }

    /// Lower `insert R <source>;` — add tuples. It desugars to `R := R union
    /// <source>`: a **public** relvar pushes `Or{ RelvarRef(t), source }` through
    /// `emit_assignment` (an idempotent `INSERT … WHERE NOT EXISTS`) when the
    /// source is SQL-backed, else ships its rows (`ship_union_insert`); a
    /// **private** relvar stores the in-process union back into its slot.
    fn lower_insert_stmt(&mut self, stmt: &InsertStmt) {
        let Some(target_expr) = stmt.target() else { return };
        let Expr::NameRef(target) = &target_expr else { return };
        let Some(name_tok) = target.ident() else { return };
        let name = name_tok.text().to_string();
        let Some(source_expr) = stmt.source() else { return };

        // Public target → idempotent INSERT via the `R := R union source` shape.
        if self.public_relvars.contains_key(&name) {
            let Some(dialect) = self.require_public_write(&name, &target_expr) else {
                return;
            };
            let Some(target_rel) = self.build_rel_expr(&target_expr) else {
                return;
            };
            // Pushable source → a single pushed idempotent INSERT.
            let pushed = self.build_rel_expr(&source_expr).and_then(|s| {
                let value = RelExpr::Or {
                    lhs: Box::new(target_rel.clone()),
                    rhs: Box::new(s),
                };
                emit_assignment(&target_rel, &value, dialect).ok()
            });
            if let Some(query) = pushed {
                self.emit_dml(query);
                return;
            }
            // In-memory source (a relation literal / private relvar) → row-ship.
            self.ship_union_insert(&target_rel, &source_expr, dialect);
            return;
        }

        // Private target → the in-process union `R union source` stored back.
        let lhs_val = self.lower_expr(&target_expr);
        let rhs_val = self.lower_expr(&source_expr);
        let value = self.emit_union(lhs_val, rhs_val);
        self.used_private_relvars.insert(name.clone());
        self.insts.push(Inst::RelvarSlotStore { name, value });
    }

    /// Ship an in-memory relation's rows into a public base relvar as an
    /// idempotent batched-`VALUES` insert (`Inst::InsertFrom`) — the runtime
    /// fallback for `R := R union <in-memory e>` / `insert R <in-memory source>`
    /// when the source can't be pushed (a relation literal or a private relvar).
    /// `target_rel` is the destination `RelvarRef`; `source_expr` is lowered to
    /// the relation value whose rows are shipped. Returns `true` if it emitted.
    fn ship_union_insert(
        &mut self,
        target_rel: &RelExpr,
        source_expr: &Expr,
        dialect: Dialect,
    ) -> bool {
        let Ok(template) = emit_insert_template(target_rel, dialect) else {
            return false;
        };
        let result_heading_id = self.intern_heading(&template.result_heading);
        let plan_id = self.register_plan(&template, result_heading_id);
        let src = self.lower_expr(source_expr);
        let ProcType::Relation(heading_id) = self.value_type(src) else {
            return false;
        };
        self.insts.push(Inst::InsertFrom {
            plan_id,
            src,
            heading_id,
        });
        // `source` is an anonymous sub-expression (not bound to a local), so its
        // relation payload is a temporary — release it once the insert has
        // shipped its rows (the fresh-source discipline `extract` /
        // `write_relation` use).
        let is_owned = self
            .locals
            .iter()
            .any(|layer| layer.values().any(|(vid, _)| *vid == src));
        if !is_owned {
            self.insts.push(Inst::Release { src });
        }
        true
    }

    /// Lower `update R where p { c: e };` — overwrite named attributes of the
    /// matching tuples. It desugars to `R := (R where ¬p) union ((R where p)
    /// «sub»)` (`UPDATE t SET … WHERE p`), or a bare substitute over `R` for
    /// update-all. A **public** relvar pushes through `emit_assignment`'s update
    /// arm; a **private** relvar computes the union (or the bare substitute) in
    /// process and stores it back into its slot.
    fn lower_update_stmt(&mut self, stmt: &UpdateStmt) {
        let Some(operand) = stmt.operand() else { return };
        // Root relvar + the `where`-restriction, if any. The operand is `R` or
        // `R where p` (typecheck guaranteed the shape).
        let (root_expr, has_where) = match &operand {
            Expr::NameRef(_) => (operand.clone(), false),
            Expr::Binary(b) if matches!(b.op_kind(), Some(BinaryOp::Where)) => {
                let Some(lhs) = b.lhs() else { return };
                (lhs, true)
            }
            _ => return,
        };
        let Expr::NameRef(target) = &root_expr else { return };
        let Some(name_tok) = target.ident() else { return };
        let name = name_tok.text().to_string();

        // Collect the `{ target: value }` pairs (typecheck guaranteed each side).
        let mut pairs: Vec<(String, Expr)> = Vec::new();
        for (nt, v) in stmt.pairs() {
            let (Some(nt), Some(v)) = (nt, v) else { return };
            pairs.push((nt.text().to_string(), v));
        }
        // `update` overwrites the target attributes — drop them (regardless of
        // what the values read), unlike `replace` which drops the read attrs.
        let removed: HashSet<String> = pairs.iter().map(|(n, _)| n.clone()).collect();

        // Public target → surgical UPDATE via the substitute-union shape.
        if self.public_relvars.contains_key(&name) {
            self.lower_public_update(&name, &root_expr, &operand, has_where, &pairs, &removed);
            return;
        }

        // Private target → compute the result in process. The substitute runs
        // over the matching rows `R where p` (or all rows `R` for update-all).
        let matching = if has_where {
            self.lower_expr(&operand)
        } else {
            self.lower_expr(&root_expr)
        };
        let changed = self.emit_substitute(matching, pairs, removed);
        let result = if has_where {
            // unchanged = R minus (R where p) ≡ R where ¬p (no AST-level negation).
            let r = self.lower_expr(&root_expr);
            let matching_again = self.lower_expr(&operand);
            let unchanged = self.emit_minus(r, matching_again);
            self.emit_union(unchanged, changed)
        } else {
            changed
        };
        self.used_private_relvars.insert(name.clone());
        self.insts.push(Inst::RelvarSlotStore { name, value: result });
    }

    /// Lower the public (SQL-backed) `update`: build `Or{ Restrict(t, ¬p),
    /// «sub»(Restrict(t, p)) }` (update-where) or a bare `«sub»(RelvarRef(t))`
    /// (update-all) and route it through `emit_assignment` → `emit_update`. A
    /// `where`-predicate that isn't a single pushable comparison, or a value the
    /// SQL renderer can't express, declines with T0049 (never a silent wipe).
    fn lower_public_update(
        &mut self,
        name: &str,
        root_expr: &Expr,
        operand: &Expr,
        has_where: bool,
        pairs: &[(String, Expr)],
        removed: &HashSet<String>,
    ) {
        let Some(dialect) = self.require_public_write(name, root_expr) else {
            return;
        };
        let Some(target_rel) = self.build_rel_expr(root_expr) else {
            return;
        };

        // The substitute input (`Restrict(t, p)` or `RelvarRef(t)`) and, for
        // update-where, the complement `Restrict(t, ¬p)`.
        let (sub_input, complement) = if has_where {
            let Some(restrict) = self.build_rel_expr(operand) else {
                self.decline_public_update(name, operand);
                return;
            };
            let RelExpr::Restrict { input: base, pred } = &restrict else {
                self.decline_public_update(name, operand);
                return;
            };
            // The only `Predicate` form is a single comparison; negate it.
            let Predicate::AttrCmp { attr, op, value } = pred;
            let complement = RelExpr::Restrict {
                input: base.clone(),
                pred: Predicate::AttrCmp {
                    attr: attr.clone(),
                    op: op.negate(),
                    value: value.clone(),
                },
            };
            (restrict, Some(complement))
        } else {
            (target_rel.clone(), None)
        };

        // Build the substitute pairs (scalar + type); a non-pushable value
        // declines. `removed` = the target attrs.
        let in_heading = sub_input.heading();
        let mut sub_pairs: Vec<(String, Type, ScalarExpr)> = Vec::new();
        for (attr, value) in pairs {
            let Some(scalar) = self.build_scalar_expr(value) else {
                self.decline_public_update(name, operand);
                return;
            };
            let ty = scalar_result_type(&scalar, &in_heading);
            sub_pairs.push((attr.clone(), ty, scalar));
        }
        let substitute = self.build_substitute_chain(sub_input, sub_pairs, removed.clone());

        let value_rel = match complement {
            Some(c) => RelExpr::Or {
                lhs: Box::new(c),
                rhs: Box::new(substitute),
            },
            None => substitute,
        };

        match emit_assignment(&target_rel, &value_rel, dialect) {
            Ok(query) => self.emit_dml(query),
            Err(_) => self.decline_public_update(name, operand),
        }
    }

    /// Decline a public `update` that isn't a recognized surgical shape (a
    /// compound/unpushable predicate, or a value the SQL renderer can't express)
    /// — surface T0049 rather than a hydrating rewrite.
    fn decline_public_update(&mut self, name: &str, span_node: &Expr) {
        self.diagnostics.push(Diagnostic::error(
            self.node_span(span_node.syntax()),
            "T0049",
            format!("cannot update public relvar `{name}`: not a recognized surgical shape"),
        ));
    }

    /// Public-write preflight shared by relational assignment and the verb
    /// statements (`truncate`/`delete`/`insert`/`update`): a public relvar is
    /// writable only when it maps to a base table (`WritePolicy::ReadWrite`, not
    /// a view → T0050) and the backend offers a SQL dialect to emit against
    /// (T0049). Pushes the diagnostic and returns `None` if either fails;
    /// otherwise the dialect to emit with. `span_node` locates the diagnostic.
    fn require_public_write(&mut self, name: &str, span_node: &Expr) -> Option<Dialect> {
        if self.public_relvar_write_policy.get(name) != Some(&WritePolicy::ReadWrite) {
            self.diagnostics.push(Diagnostic::error(
                self.node_span(span_node.syntax()),
                "T0050",
                format!("cannot assign to public relvar `{name}`: it maps to a non-writable view"),
            ));
            return None;
        }
        match self.dialect {
            Some(dialect) => Some(dialect),
            None => {
                self.diagnostics.push(Diagnostic::error(
                    self.node_span(span_node.syntax()),
                    "T0049",
                    format!(
                        "cannot assign to public relvar `{name}`: backend does not support SQL writes"
                    ),
                ));
                None
            }
        }
    }

    /// Lower `R := <rhs>;` where `R` is a public relvar: recognize the RHS shape
    /// and emit surgical DML (`Inst::Dml`). The RHS is **never materialized** —
    /// `build_rel_expr` pushes it, `emit_assignment` recognizes it, and the SQL
    /// runs server-side.
    fn lower_public_assign(&mut self, name: &str, target_expr: &Expr, value_expr: &Expr) {
        // A writable base relvar plus a SQL dialect to emit against (else
        // T0050 / T0049). Shared with the verb statements (`truncate`, …).
        let Some(dialect) = self.require_public_write(name, target_expr) else {
            return;
        };
        // Recognize the RHS shape: build both operands' RelIR (the target is a
        // bare `RelvarRef`; the RHS pushes only if `build_rel_expr` accepts it),
        // then `emit_assignment`. A pushable shape becomes a single surgical
        // statement (`Inst::Dml`).
        let recognized = self
            .build_rel_expr(target_expr)
            .zip(self.build_rel_expr(value_expr))
            .and_then(|(t, r)| emit_assignment(&t, &r, dialect).ok());
        if let Some(query) = recognized {
            self.emit_dml(query);
            return;
        }

        // Not pushable: `R := R union <in-memory e>` (a relation literal, or a
        // private relvar) ships `e`'s rows from the process into `R` at runtime —
        // an idempotent batched-`VALUES` insert (`Inst::InsertFrom`).
        if let Some(e) = self.union_insert_source(name, value_expr) {
            if let Some(target_rel) = self.build_rel_expr(target_expr) {
                if self.ship_union_insert(&target_rel, &e, dialect) {
                    return;
                }
            }
        }

        // Replace-all fallback. `R` is absent from a recognized surgical shape;
        // empty `R` and refill it from the RHS (two `Inst::Dml` in the
        // transaction — atomic).
        let Some(target_rel) = self.build_rel_expr(target_expr) else {
            return;
        };
        let RelExpr::RelvarRef { table_name: t, .. } = &target_rel else {
            return;
        };
        let t = t.clone();

        let x = self.build_rel_expr(value_expr);

        // Self-referential but unrecognized (e.g. a compound-predicate keep-filter
        // whose negation is a disjunction the single-predicate model can't push):
        // it should be surgical, so decline rather than do a non-surgical,
        // hydrating replace-all.
        if x.as_ref().is_some_and(|x| x.references_table(&t)) {
            self.diagnostics.push(Diagnostic::error(
                self.node_span(value_expr.syntax()),
                "T0049",
                format!(
                    "assignment to public relvar `{name}` is self-referential but not a \
                     recognized surgical shape"
                ),
            ));
            return;
        }

        // Independent **pushable** `X` → pure-SQL replace-all: truncate, then
        // `INSERT INTO t SELECT <X>` (no hydration).
        if let Some(insert) = x
            .as_ref()
            .and_then(|x| emit_replace_insert(&target_rel, x, dialect).ok())
        {
            if let Ok(truncate) = emit_truncate(&target_rel, dialect) {
                self.emit_dml(truncate);
                self.emit_dml(insert);
                return;
            }
        }

        // Independent **in-memory** `X` (a relation literal, or a private relvar)
        // → truncate, then ship its rows (reuses the batched-`VALUES` insert; the
        // empty table makes the template's `NOT EXISTS` always insert).
        if let Ok(template) = emit_insert_template(&target_rel, dialect) {
            if let Ok(truncate) = emit_truncate(&target_rel, dialect) {
                self.emit_dml(truncate);
                let result_heading_id = self.intern_heading(&template.result_heading);
                let plan_id = self.register_plan(&template, result_heading_id);
                let src = self.lower_expr(value_expr);
                if let ProcType::Relation(heading_id) = self.value_type(src) {
                    self.insts.push(Inst::InsertFrom {
                        plan_id,
                        src,
                        heading_id,
                    });
                    let is_owned = self
                        .locals
                        .iter()
                        .any(|layer| layer.values().any(|(vid, _)| *vid == src));
                    if !is_owned {
                        self.insts.push(Inst::Release { src });
                    }
                    return;
                }
            }
        }

        // Unreachable for a well-typed relation RHS, but stay total.
        self.diagnostics.push(Diagnostic::error(
            self.node_span(value_expr.syntax()),
            "T0049",
            format!("assignment to public relvar `{name}` is not a supported write shape"),
        ));
    }

    /// For `R := R union e`, return the *other* operand `e` when the RHS is a
    /// `union` with the target relvar `name` as one operand (union is
    /// commutative). `None` otherwise. Used to route a non-pushable union (a
    /// relation literal, or a private relvar) to the runtime row-shipping insert.
    fn union_insert_source(&self, name: &str, value_expr: &Expr) -> Option<Expr> {
        let Expr::Binary(b) = value_expr else {
            return None;
        };
        if b.op_kind() != Some(BinaryOp::Union) {
            return None;
        }
        let (lhs, rhs) = (b.lhs()?, b.rhs()?);
        let is_target = |e: &Expr| {
            matches!(e, Expr::NameRef(n) if n.ident().is_some_and(|t| t.text() == name))
        };
        if is_target(&lhs) {
            Some(rhs)
        } else if is_target(&rhs) {
            Some(lhs)
        } else {
            None
        }
    }

    fn lower_let_stmt(&mut self, stmt: &LetStmt) {
        // RHS expression always lowers first; the binding name then
        // adopts its `ValueId`. Missing name (parser recovery) is
        // dropped silently — the diagnostic-free invariant means
        // we'd never reach lowering with one.
        let value_expr = match stmt.value() {
            Some(v) => v,
            None => return,
        };
        // Binding transparency: when a SQL dialect is active and the RHS is a
        // pushable relvar-rooted relational expression, record it as a
        // deferred `RelExpr` alias and emit nothing. Uses of the name fold the
        // expression down into one pushed query (`let gg = Greetings; gg where
        // id = 1` → a single `SELECT … WHERE`), and an unused binding emits no
        // query at all. Gating on `try_push` (a pure, non-emitting check)
        // guarantees the alias is materializable wherever it is later forced.
        // A `transaction [...]` RHS isn't relvar-rooted (`build_rel_expr`
        // returns `None`), so it materializes here — keeping public-relvar
        // reads inside their transaction.
        if let (Some(dialect), Some(name_tok)) = (self.dialect, stmt.name()) {
            if let Some(rel) = self.build_rel_expr(&value_expr) {
                if crate::cut::try_push(&rel, dialect).is_some() {
                    self.bind_alias(name_tok.text().to_string(), rel);
                    return;
                }
            }
        }
        // If the RHS is a NameRef to an existing heap-typed binding,
        // the new let creates a second owner of the same value —
        // emit a retain so the refcount reflects both bindings. Pure
        // `RelationLit` RHS produces a fresh allocation already at
        // rc=1, so no retain is needed for that path. For `Text` this
        // also covers a borrowed-source alias (`let s = g.message; let t = s;`):
        // the retain is safe (immortal literal → no-op; cell-loaded → bumps
        // the shared rc) and the new local is marked owned so its scope-exit
        // release balances the retain.
        let rhs_is_existing_name = matches!(value_expr, Expr::NameRef(_));
        let id = self.lower_expr(&value_expr);
        let ty = self.value_type(id);
        let alias_of_heap_text = rhs_is_existing_name && matches!(ty, ProcType::Text);
        if rhs_is_existing_name && (Self::is_heap_managed(&ty) || matches!(ty, ProcType::Text)) {
            self.insts.push(Inst::Retain { src: id });
        }
        if alias_of_heap_text {
            self.mark_text_owned(id);
        }
        if let Some(name_tok) = stmt.name() {
            self.bind_local(name_tok.text().to_string(), id, ty);
        }
    }

    fn lower_expr_stmt(&mut self, stmt: &ExprStmt) {
        if let Some(expr) = stmt.expr() {
            let _ = self.lower_expr(&expr);
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> ValueId {
        // Try the SQL pushdown cut first: a relvar-rooted relational subtree
        // becomes one `Inst::Query` fired lazily at this force point. On a
        // miss (not pushable, or no pushable backend) fall through to the
        // legacy in-process lowering below.
        if let Some(v) = self.try_lower_pushed(expr) {
            return v;
        }
        self.guard_no_full_relvar_pull(expr);
        match expr {
            Expr::Literal(lit) => self.lower_literal(lit),
            Expr::Call(call) => self.lower_call(call),
            Expr::Transaction(t) => self.lower_transaction_expr(t),
            Expr::TupleLit(t) => self.lower_tuple_lit(t),
            Expr::RelationLit(r) => self.lower_relation_lit(r),
            Expr::FieldAccess(f) => self.lower_field_access(f),
            Expr::BoolLit(b) => self.lower_bool_lit(b),
            Expr::Binary(b) => self.lower_binary_expr(b),
            Expr::Unary(u) => self.lower_unary_expr(u),
            Expr::Project(p) => self.lower_project_expr(p),
            Expr::Replace(r) => self.lower_replace_expr(r),
            Expr::Rename(r) => self.lower_rename_expr(r),
            Expr::Wrap(w) => self.lower_wrap_expr(w),
            Expr::Unwrap(u) => self.lower_unwrap_expr(u),
            Expr::Extend(e) => self.lower_extend_expr(e),
            Expr::Tclose(t) => self.lower_tclose_expr(t),
            Expr::NameRef(n) => self.lower_name_ref(n),
        }
    }

    /// Development tripwire for the scalability gap S1 in `.local/optimizations.md`.
    ///
    /// `expr` is a relational expression the cut just *declined* to push (we're
    /// past the `try_lower_pushed` miss). If one of its relational operands is an
    /// **unfiltered public-relvar scan** — relvar-rooted with no pushed
    /// restriction, i.e. a `SELECT … FROM t` over the whole table — then lowering
    /// this operator in-process pulls every row of that relvar into memory to do
    /// work the backend should have done. That doesn't scale, so panic to surface
    /// the gap.
    ///
    /// Deliberately narrow, matching the rule "pulling a whole *query* is fine,
    /// pulling a whole *public relvar* to process in-process is not":
    /// - `transaction [ Greetings ]` never reaches here — a bare relvar pushes as
    ///   a query (the result *is* the whole relvar; that's the user's query).
    /// - `Greetings where <nonpushable p>` fires — the `where` declined, but the
    ///   operand `Greetings` is a full-table scan feeding an in-process filter.
    /// - `(Greetings where <pushable p>) where <nonpushable q>` does *not* fire —
    ///   the operand carries a pushed `Restrict`, so a filtered subset (a query),
    ///   not the whole relvar, is pulled.
    /// - Genuinely in-memory operands (relation literals, `private` relvars,
    ///   prior in-process results) are not relvar-rooted, so they never fire.
    ///
    /// Becomes a proper partial-pushdown / `MaterializeAtBoundary` decision once
    /// that lands (`docs/relir.md`); until then it's a loud "needs pushdown work".
    fn guard_no_full_relvar_pull(&self, expr: &Expr) {
        let operands: Vec<Expr> = match expr {
            Expr::Binary(b) => match b.op_kind() {
                // `where`'s relational operand is the lhs (rhs is the predicate).
                Some(BinaryOp::Where) => b.lhs().into_iter().collect(),
                // The AND/OR-family binaries take two relational operands.
                Some(
                    BinaryOp::Join
                    | BinaryOp::Times
                    | BinaryOp::Intersect
                    | BinaryOp::Compose
                    | BinaryOp::Union
                    | BinaryOp::Minus,
                ) => b.lhs().into_iter().chain(b.rhs()).collect(),
                // A scalar binary (arithmetic / comparison / logical) is not a
                // relational operator — nothing to guard.
                _ => return,
            },
            Expr::Project(p) => p.input().into_iter().collect(),
            Expr::Replace(r) => r.input().into_iter().collect(),
            Expr::Rename(r) => r.input().into_iter().collect(),
            Expr::Wrap(w) => w.input().into_iter().collect(),
            Expr::Unwrap(u) => u.input().into_iter().collect(),
            Expr::Extend(e) => e.input().into_iter().collect(),
            Expr::Tclose(t) => t.input().into_iter().collect(),
            _ => return,
        };
        for operand in operands {
            let Some(rel) = self.build_rel_expr(&operand) else {
                continue;
            };
            if rel.origin() == StorageOrigin::RelvarRooted && !contains_restrict(&rel) {
                panic!(
                    "pushdown gap (S1): an in-process relational operator would pull the \
                     whole public relvar `{}` into memory (an unfiltered `SELECT … FROM` \
                     feeding in-process work). The cut could not push this subtree — it \
                     needs pushdown / partial-materialization work. See \
                     .local/optimizations.md S1.",
                    relvar_root_name(&rel).unwrap_or("?"),
                );
            }
        }
    }

    /// Lower a `replace` whose operand the cut declined to push — an in-memory
    /// relation. Every `replace` value computes (a bare-ref relabel is rejected
    /// by typecheck → `rename`), so it desugars to `extend → project all-but →
    /// rename` — the in-process counterpart of the SQL desugar (compute the new
    /// attribute, drop the operand attributes the value reads, rename a temp
    /// back to the target when the new name collided).
    fn lower_replace_expr(&mut self, re: &ReplaceExpr) -> ValueId {
        let src = re
            .input()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked replace has a relation operand");

        // `replace` removes the attributes each value *reads* (compute-and-
        // consume); collect that set, then emit the shared substitute chain.
        let src_heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("replace on non-relation `{other}` survived typecheck"),
        };
        let in_heading = self.headings[src_heading_id.0 as usize].clone();
        let mut pairs: Vec<(String, Expr)> = Vec::new();
        let mut removed: HashSet<String> = HashSet::new();
        for (name_tok, value) in re.pairs() {
            let new = name_tok.expect("typechecked replace pair has a name").text().to_string();
            let value = value.expect("typechecked replace pair has a value");
            let mut refs: HashSet<String> = HashSet::new();
            ast_attr_refs(&value, &mut refs);
            for r in refs {
                if in_heading.lookup(&r).is_some() {
                    removed.insert(r);
                }
            }
            pairs.push((new, value));
        }
        self.emit_substitute(src, pairs, removed)
    }

    /// Emit the in-process substitute chain over `src` (`extend → project all-but
    /// → rename`), overwriting each `(new, value)` pair and dropping the
    /// attributes in `removed`. A pair whose `new` already exists is extended
    /// under a temp and renamed back. Shared by `replace` (removed = the attrs
    /// the values read) and `update` (removed = the target attrs). Releases each
    /// consumed intermediate (and `src` when no local owns it).
    fn emit_substitute(
        &mut self,
        src: ValueId,
        pairs: Vec<(String, Expr)>,
        removed: HashSet<String>,
    ) -> ValueId {
        let src_heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("substitute on non-relation `{other}` survived typecheck"),
        };
        let in_heading = self.headings[src_heading_id.0 as usize].clone();
        let mut extend_pairs: Vec<(String, Expr)> = Vec::new();
        let mut renames: Vec<(String, String)> = Vec::new();
        for (new, value) in pairs {
            let extend_name = if in_heading.lookup(&new).is_some() {
                let t = format!("__coddl_replace_tmp_{new}");
                renames.push((t.clone(), new));
                t
            } else {
                new
            };
            extend_pairs.push((extend_name, value));
        }
        let keep: Vec<String> = in_heading
            .attrs()
            .iter()
            .map(|(n, _)| n.clone())
            .chain(extend_pairs.iter().map(|(n, _)| n.clone()))
            .filter(|n| !removed.contains(n))
            .collect();

        // Compose: extend → project all-but → rename, releasing each consumed
        // intermediate (the operand is released only when no local owns it).
        let ext = self.emit_extend(src, extend_pairs);
        self.release_if_unowned(src);
        let proj = self.emit_project(ext, &keep);
        self.release_if_unowned(ext);
        if renames.is_empty() {
            return proj;
        }
        let dst = self.emit_rename(proj, renames);
        self.release_if_unowned(proj);
        dst
    }

    /// Lower a `rename` whose operand the cut declined to push — an in-memory
    /// relation. Every value is a bare attribute reference (typecheck enforces
    /// it), so this is a pure relabel: one `Inst::Rename`.
    fn lower_rename_expr(&mut self, re: &RenameExpr) -> ValueId {
        let src = re
            .input()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked rename has a relation operand");
        let renames: Vec<(String, String)> = re
            .renames()
            .into_iter()
            .filter_map(|(old, new)| Some((old?.text().to_string(), new?.text().to_string())))
            .collect();
        let dst = self.emit_rename(src, renames);
        self.release_if_unowned(src);
        dst
    }

    /// Lower a `wrap` whose operand the cut declined to push — an in-memory
    /// relation. Group attributes into tuple-valued attributes via one
    /// `Inst::Restructure` (a leaf-cell re-layout).
    fn lower_wrap_expr(&mut self, we: &WrapExpr) -> ValueId {
        let src = we
            .input()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked wrap has a relation operand");
        let in_heading = self.relation_heading(src, "wrap");
        let wraps = wrap_spec(&in_heading, we);
        // Reuse `RelExpr::Wrap::heading()` as the single source of truth for the
        // result heading (a dummy materialized input supplies `in_heading`).
        let dst_heading = RelExpr::Wrap {
            input: Box::new(RelExpr::MaterializedRelvar {
                name: String::new(),
                heading: in_heading,
            }),
            wraps,
        }
        .heading();
        let dst = self.emit_restructure(src, dst_heading);
        self.release_if_unowned(src);
        dst
    }

    /// Lower an `unwrap` whose operand the cut declined to push — an in-memory
    /// relation. Expand tuple-valued attributes to their components via one
    /// `Inst::Restructure`.
    fn lower_unwrap_expr(&mut self, ue: &UnwrapExpr) -> ValueId {
        let src = ue
            .input()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked unwrap has a relation operand");
        let in_heading = self.relation_heading(src, "unwrap");
        let names: Vec<String> = ue.attrs().map(|t| t.text().to_string()).collect();
        let dst_heading = RelExpr::Unwrap {
            input: Box::new(RelExpr::MaterializedRelvar {
                name: String::new(),
                heading: in_heading,
            }),
            names,
        }
        .heading();
        let dst = self.emit_restructure(src, dst_heading);
        self.release_if_unowned(src);
        dst
    }

    /// The heading of an already-lowered relation value.
    fn relation_heading(&self, src: ValueId, op: &str) -> Heading {
        match self.value_type(src) {
            ProcType::Relation(id) => self.headings[id.0 as usize].clone(),
            other => unreachable!("{op} on non-relation `{other}` survived typecheck"),
        }
    }

    /// Restructure an already-lowered relation `src` into `dst_heading` (which
    /// must hold the same leaf cells) and emit `Inst::Restructure`. Mint-and-
    /// return: the caller releases `src`. Shared by `wrap`/`unwrap`.
    fn emit_restructure(&mut self, src: ValueId, dst_heading: Heading) -> ValueId {
        let src_heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("restructure on non-relation `{other}` survived typecheck"),
        };
        let result_heading_id = self.intern_heading(&dst_heading);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(result_heading_id));
        self.insts.push(Inst::Restructure {
            dst,
            src,
            src_heading_id,
            result_heading_id,
        });
        dst
    }

    /// Rename an already-lowered relation `src` and emit `Inst::Rename`. Computes
    /// the renamed (re-sorted) result heading and the source→dest permutation.
    /// Mint-and-return: the caller releases `src`. Reused by `lower_rename_expr`
    /// and the general-expression `replace` desugar (temp → target renames).
    fn emit_rename(&mut self, src: ValueId, renames: Vec<(String, String)>) -> ValueId {
        let src_heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("rename on non-relation `{other}` survived typecheck"),
        };
        let src_heading = self.headings[src_heading_id.0 as usize].clone();
        let renamed: Vec<(String, Type)> = src_heading
            .attrs()
            .iter()
            .map(|(name, t)| {
                let new = renames
                    .iter()
                    .find(|(old, _)| old == name)
                    .map(|(_, new)| new.clone())
                    .unwrap_or_else(|| name.clone());
                (new, t.clone())
            })
            .collect();
        let result_heading = Heading::new(renamed);
        let result_heading_id = self.intern_heading(&result_heading);
        // perm[dst_i] = the src index whose name maps to dst_i (reverse rename),
        // else the dst name itself. Both headings are canonically ordered.
        let perm: Vec<u32> = result_heading
            .attrs()
            .iter()
            .map(|(new_name, _)| {
                let src_name = renames
                    .iter()
                    .find(|(_, new)| new == new_name)
                    .map(|(old, _)| old.as_str())
                    .unwrap_or(new_name.as_str());
                src_heading
                    .attrs()
                    .iter()
                    .position(|(n, _)| n == src_name)
                    .unwrap_or(0) as u32
            })
            .collect();
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(result_heading_id));
        self.insts.push(Inst::Rename {
            dst,
            src,
            src_heading_id,
            result_heading_id,
            perm,
        });
        dst
    }

    /// Lower `R extend { c: e, … }` the cut declined to push — i.e. over an
    /// in-memory relation (a relation literal, a private relvar, or a fetched
    /// relvar whose value didn't render to SQL). Mirror `lower_where_expr`:
    /// synthesize a helper `__coddl_extend_<n>(src_record, dst_record)` that
    /// loads the operand cells, computes each new value, and writes the whole
    /// widened record into `dst`; then emit `Inst::Extend`. The typechecker
    /// restricts extend values to Integer or Text (T0046), so every new cell
    /// has a supported relation-cell layout.
    fn lower_extend_expr(&mut self, e: &ExtendExpr) -> ValueId {
        let src = e
            .input()
            .map(|i| self.lower_expr(&i))
            .expect("typechecked extend has a relation operand");
        let pairs: Vec<(String, Expr)> = e
            .pairs()
            .into_iter()
            .map(|(name_tok, value)| {
                (
                    name_tok.expect("typechecked extend pair has a name").text().to_string(),
                    value.expect("typechecked extend pair has a value"),
                )
            })
            .collect();
        let dst = self.emit_extend(src, pairs);
        self.release_if_unowned(src);
        dst
    }

    /// Synthesize an `extend` helper over an already-lowered relation `src` plus
    /// `(new_name, value_expr)` pairs, and emit `Inst::Extend`. The helper loads
    /// the operand cells, computes each value, and writes the whole widened
    /// record. Mint-and-return: the caller releases `src`. Reused by
    /// `lower_extend_expr` and the general-expression `replace` desugar.
    fn emit_extend(&mut self, src: ValueId, pairs: Vec<(String, Expr)>) -> ValueId {
        let src_heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("extend on non-relation `{other}` survived typecheck"),
        };
        let src_heading = self.headings[src_heading_id.0 as usize].clone();
        let src_layout = crate::layout::record_layout(&src_heading);

        // 2. Mint a fresh helper name.
        let helper_name = format!("__coddl_extend_{}", self.next_extend);
        self.next_extend += 1;

        // 3. Snapshot enclosing per-function state; install fresh helper state.
        //    Stash outer locals so a value referencing an enclosing `let`
        //    triggers the T0022 capture diagnostic (same as `where`).
        let saved_next_value = std::mem::replace(&mut self.next_value, 0);
        let saved_next_block = std::mem::replace(&mut self.next_block, 0);
        let saved_insts = std::mem::take(&mut self.insts);
        let saved_locals = std::mem::replace(&mut self.locals, vec![HashMap::new()]);
        let saved_aliases = std::mem::replace(&mut self.relexpr_aliases, vec![HashMap::new()]);
        let saved_value_types = std::mem::take(&mut self.value_types);
        // Isolate `owned_texts` like `value_types`: the helper resets `next_value`,
        // so its ValueIds collide with the enclosing function's. Same for the
        // deferred extract-source list (an `extract` in a computed value).
        let saved_owned_texts = std::mem::take(&mut self.owned_texts);
        let saved_deferred = std::mem::take(&mut self.deferred_relation_releases);
        self.outer_locals_for_capture = Some(saved_locals.clone());

        // 4. Helper params: `src_record` (ValueId 0), `dst_record` (ValueId 1).
        let block_id = self.fresh_block();
        let src_ptr = self.fresh_value();
        self.record_type(src_ptr, ProcType::Pointer);
        let dst_ptr = self.fresh_value();
        self.record_type(dst_ptr, ProcType::Pointer);

        // 5. AttrLoad each operand cell from `src_ptr`; bind so value `NameRef`s
        //    resolve. Remember the loaded value per name for the store step.
        let mut cell_value: HashMap<String, (ValueId, ProcType)> = HashMap::new();
        for attr in &src_layout.attrs {
            let attr_type = proc_type_from_kind(attr.kind);
            let dst = self.fresh_value();
            self.record_type(dst, attr_type.clone());
            self.insts.push(Inst::AttrLoad {
                dst,
                src: src_ptr,
                offset: attr.offset,
                attr_type: attr_type.clone(),
            });
            self.bind_local(attr.name.clone(), dst, attr_type.clone());
            cell_value.insert(attr.name.clone(), (dst, attr_type));
        }

        // 6. Lower each new value expression; collect its `(name, value, type)`.
        let mut result_attrs: Vec<(String, Type)> = src_heading.attrs().to_vec();
        for (name, value) in pairs {
            let v = self.lower_expr(&value);
            let pt = self.value_type(v);
            result_attrs.push((name.clone(), type_from_proc(&pt)));
            cell_value.insert(name, (v, pt));
        }

        // 7. Result heading + layout (canonically re-sorted with the new attrs).
        let result_heading = Heading::new(result_attrs);
        let result_heading_id = self.intern_heading(&result_heading);
        let result_layout = crate::layout::record_layout(&result_heading);

        // 8. AttrStore each result cell into `dst_ptr` at its result offset —
        //    surviving operand attrs (their loaded value) and new ones alike.
        for attr in &result_layout.attrs {
            let (value, pt) = cell_value
                .get(&attr.name)
                .expect("every result attribute has a computed value")
                .clone();
            self.insts.push(Inst::AttrStore {
                record: dst_ptr,
                offset: attr.offset,
                value,
                attr_type: pt,
            });
            // The store retains the cell (backend retain-on-store), so the new
            // relation co-owns it. Release the producer reference of a *computed*
            // owned `Text` (a per-row `||` result) now consumed into the cell —
            // leaving the cell's retained ref the sole owner. A *surviving* cell
            // is the AttrLoad'd value (borrowed, bound as a local) — not in
            // `owned_texts`, so this no-ops and the source relation keeps its ref.
            self.release_text_temp(value);
        }

        // Release any deferred extract sources before the helper returns.
        self.drain_deferred_relation_releases();

        // 9. Close the helper (void return, two pointer params).
        let block = BasicBlock {
            id: block_id,
            insts: std::mem::take(&mut self.insts),
            terminator: Terminator::Return(None),
        };
        self.functions.push(Function {
            name: helper_name.clone(),
            linkage_name: helper_name.clone(),
            params: vec![
                ("src_record".to_string(), ProcType::Pointer),
                ("dst_record".to_string(), ProcType::Pointer),
            ],
            return_type: ProcType::Unit,
            blocks: vec![block],
        });

        // 10. Restore the enclosing function's state.
        self.next_value = saved_next_value;
        self.next_block = saved_next_block;
        self.insts = saved_insts;
        self.locals = saved_locals;
        self.relexpr_aliases = saved_aliases;
        self.value_types = saved_value_types;
        self.owned_texts = saved_owned_texts;
        self.deferred_relation_releases = saved_deferred;
        self.outer_locals_for_capture = None;

        // 11. Emit Inst::Extend in the enclosing function (caller releases src).
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(result_heading_id));
        self.insts.push(Inst::Extend {
            dst,
            src,
            helper_linkage: helper_name,
            src_heading_id,
            result_heading_id,
        });
        dst
    }

    /// Release `v` if no local scope owns it — the refcount balancing the
    /// in-process relational lowerings install for chained temporaries (a fresh
    /// `RelvarRead`/relop result owned by no `let`). Idempotent in effect:
    /// owned values keep their reference for the owning local to release.
    fn release_if_unowned(&mut self, v: ValueId) {
        let owned = self
            .locals
            .iter()
            .any(|layer| layer.values().any(|(vid, _)| *vid == v));
        if !owned {
            self.insts.push(Inst::Release { src: v });
        }
    }

    /// Lower a `project` whose operand the cut declined to push — i.e. an
    /// in-memory relation (a relation literal, or a `where` over one). The
    /// pushable case never reaches here; it is served entirely by
    /// `Inst::Query` with a narrowed SELECT. Lower the operand, compute the
    /// narrowed result heading, and emit `Inst::Project`.
    fn lower_project_expr(&mut self, pe: &ProjectExpr) -> ValueId {
        // 1. Lower the relation operand in the enclosing scope.
        let src = pe
            .input()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked project has a relation operand");
        let src_heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("project on non-relation `{other}` survived typecheck"),
        };
        let src_heading = self.headings[src_heading_id.0 as usize].clone();

        // 2. Resolve the kept heading. `project { … }` keeps the listed
        //    names; `project all but { … }` keeps the complement (against the
        //    statically-known operand heading). `Heading::new` re-canonicalizes,
        //    so written order is irrelevant — matching the typechecker.
        let listed: Vec<String> = pe.attrs().map(|t| t.text().to_string()).collect();
        let all_but = pe.is_all_but();
        let narrowed: Vec<(String, Type)> = src_heading
            .attrs()
            .iter()
            .filter(|(name, _)| listed.iter().any(|k| k == name) != all_but)
            .cloned()
            .collect();
        let result_heading_id = self.intern_heading(&Heading::new(narrowed));

        // 3. Emit Inst::Project.
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(result_heading_id));
        self.insts.push(Inst::Project {
            dst,
            src,
            src_heading_id,
            result_heading_id,
        });

        // 4. Release the source if no local owns it — keeps chains like
        //    `where → project → extract` refcount-balanced without manual
        //    let-binding (the same balancing `where` installs).
        let src_owned = self
            .locals
            .iter()
            .any(|layer| layer.values().any(|(vid, _)| *vid == src));
        if !src_owned {
            self.insts.push(Inst::Release { src });
        }
        dst
    }

    /// Lower a `tclose` whose operand the cut declined to push — an in-memory
    /// relation. (v1 has no `tclose` SQL emission, so this is the only path;
    /// even a relvar-rooted operand fetches via a plain SELECT, then closes
    /// here.) The optional brace-list narrows to two columns first — sugar for
    /// `(R project { a, b }) tclose` — then `Inst::TClose` runs the fixpoint.
    fn lower_tclose_expr(&mut self, te: &TcloseExpr) -> ValueId {
        // 1. Lower the relation operand in the enclosing scope.
        let mut src = te
            .input()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked tclose has a relation operand");
        // 2. A brace-list picks two columns first (the `R tclose { a, b }`
        //    form). `emit_project` mints a fresh value, so release the
        //    pre-projection source if no local owns it.
        let names: Vec<String> = te.attrs().map(|t| t.text().to_string()).collect();
        if !names.is_empty() {
            let projected = self.emit_project(src, &names);
            let src_owned = self
                .locals
                .iter()
                .any(|layer| layer.values().any(|(vid, _)| *vid == src));
            if !src_owned {
                self.insts.push(Inst::Release { src });
            }
            src = projected;
        }
        // 3. Emit Inst::TClose (result heading == src heading).
        let dst = self.emit_tclose(src);
        // 4. Release the (possibly projected) source if no local owns it — the
        //    same balancing `where`/`project` install.
        let src_owned = self
            .locals
            .iter()
            .any(|layer| layer.values().any(|(vid, _)| *vid == src));
        if !src_owned {
            self.insts.push(Inst::Release { src });
        }
        dst
    }

    /// If `expr` is a relvar-rooted relational subtree the cut can push,
    /// bake its SQL into `Module::plans` and emit an `Inst::Query`, returning
    /// its result value. Otherwise return `None` so the caller lowers `expr`
    /// via the legacy in-process path. No-op when pushdown is disabled
    /// (`dialect` is `None`).
    fn try_lower_pushed(&mut self, expr: &Expr) -> Option<ValueId> {
        let dialect = self.dialect?;
        let rel = self.build_rel_expr(expr)?;
        let query = crate::cut::try_push(&rel, dialect)?;
        // For `coddl explain`: a successful push is a clean RelExpr root (the
        // caller returns here, so no nested sub-expression is captured twice).
        if self.collect_relir {
            self.relir.push(ExplainEntry {
                expr: rel.clone(),
                sql: query.sql.text.clone(),
            });
        }
        Some(self.emit_query(query))
    }

    /// Build a `coddl-relir` expression from a relational AST subtree, or
    /// `None` if the shape isn't one the cut handles (v1: a public-relvar
    /// leaf, optionally restricted by a single `attr = literal`). A `NameRef`
    /// that resolves to a deferred relation alias substitutes the aliased
    /// expression (binding transparency); a `NameRef` shadowed by a
    /// materialized local is not a relvar read, so it returns `None`.
    fn build_rel_expr(&self, expr: &Expr) -> Option<RelExpr> {
        match expr {
            Expr::NameRef(n) => {
                let name = n.ident()?;
                let name = name.text();
                // A relation `let`-binding recorded as an alias is transparent:
                // substitute its `RelExpr` so the surrounding algebra folds
                // down to one pushed query.
                if let Some(rel) = self.lookup_alias(name) {
                    return Some(rel.clone());
                }
                if self.lookup_local(name).is_some() {
                    return None;
                }
                // A `private` relvar is the in-memory (materialized) leaf.
                if let Some(&heading_id) = self.private_relvars.get(name) {
                    return Some(RelExpr::MaterializedRelvar {
                        name: name.to_string(),
                        heading: self.headings[heading_id.0 as usize].clone(),
                    });
                }
                let binding = self.public_relvars.get(name)?;
                Some(RelExpr::RelvarRef {
                    name: binding.name.clone(),
                    database: self.db_name.clone().unwrap_or_default(),
                    heading: self.headings[binding.heading_id.0 as usize].clone(),
                    table_name: binding.table_name.clone(),
                    columns: binding.columns.clone(),
                    keys: binding.keys.clone(),
                })
            }
            Expr::Binary(b) => self.build_rel_binary(b),
            Expr::Project(p) => {
                // Projection over a pushable subtree pushes too — the cut
                // gates on `origin()`, which `RelExpr::Project` propagates,
                // and the emitter narrows the SELECT list from the heading.
                // `keep` order is irrelevant (the heading re-sorts). For
                // `all but`, resolve the complement against the operand
                // heading so RelIR still carries a concrete `keep` set.
                let input = self.build_rel_expr(&p.input()?)?;
                let listed: Vec<String> = p.attrs().map(|t| t.text().to_string()).collect();
                let keep: Vec<String> = if p.is_all_but() {
                    input
                        .heading()
                        .attrs()
                        .iter()
                        .map(|(n, _)| n.clone())
                        .filter(|n| !listed.contains(n))
                        .collect()
                } else {
                    listed
                };
                Some(RelExpr::Project {
                    input: Box::new(input),
                    keep,
                })
            }
            Expr::Replace(r) => {
                let input = self.build_rel_expr(&r.input()?)?;
                // Every value computes (a bare-ref relabel is rejected by
                // typecheck → `rename`): build the substitute chain, removing the
                // attributes each value *reads* (compute-and-consume).
                let in_heading = input.heading();
                let mut pairs: Vec<(String, Type, ScalarExpr)> = Vec::new();
                let mut removed: HashSet<String> = HashSet::new();
                for (name_tok, value) in r.pairs() {
                    let new = name_tok?.text().to_string();
                    let value = value?;
                    let scalar = self.build_scalar_expr(&value)?;
                    scalar_attr_refs(&scalar, &mut removed);
                    let ty = scalar_result_type(&scalar, &in_heading);
                    pairs.push((new, ty, scalar));
                }
                Some(self.build_substitute_chain(input, pairs, removed))
            }
            Expr::Rename(r) => {
                // A pure relabel → one `Rename` node (pushes as `col AS new`).
                let input = self.build_rel_expr(&r.input()?)?;
                let mut renames = Vec::new();
                for (old, new) in r.renames() {
                    let (Some(old), Some(new)) = (old, new) else {
                        return None;
                    };
                    renames.push((old.text().to_string(), new.text().to_string()));
                }
                Some(RelExpr::Rename {
                    input: Box::new(input),
                    renames,
                })
            }
            Expr::Tclose(t) => {
                // `R tclose { a, b }` ≡ `(R project { a, b }) tclose` — wrap the
                // operand in a `Project` when a brace-list is present, then a
                // `TClose`. v1 has no `tclose` SQL emission (sqlemit's `resolve`
                // errs on `TClose`), so a relvar-rooted closure still declines
                // the push and runs in-process; building the RelExpr is what
                // lets the `explain` path render the `TClose` node.
                let mut input = self.build_rel_expr(&t.input()?)?;
                let keep: Vec<String> = t.attrs().map(|tok| tok.text().to_string()).collect();
                if !keep.is_empty() {
                    input = RelExpr::Project {
                        input: Box::new(input),
                        keep,
                    };
                }
                Some(RelExpr::TClose {
                    input: Box::new(input),
                })
            }
            Expr::Extend(e) => {
                // `R extend { c: e, … }` — add each computed column. Build the
                // operand, then walk each value expression into a `ScalarExpr`
                // (declining the push — `None` — if a value uses anything the
                // SQL renderer can't express yet, e.g. a comparison or call).
                // The result type is computed from the operand heading; the
                // typechecker is the authority, so this just mirrors its rule.
                let input = self.build_rel_expr(&e.input()?)?;
                let in_heading = input.heading();
                let mut extends = Vec::new();
                for (name_tok, value) in e.pairs() {
                    let name = name_tok?.text().to_string();
                    let scalar = self.build_scalar_expr(&value?)?;
                    let ty = scalar_result_type(&scalar, &in_heading);
                    extends.push((name, ty, scalar));
                }
                Some(RelExpr::Extend {
                    input: Box::new(input),
                    extends,
                })
            }
            Expr::Wrap(w) => {
                // Build the operand + the `(new, components)` spec. v1 has no
                // `wrap` SQL emission (sqlemit's `resolve` errs), so a
                // relvar-rooted wrap declines the push and restructures
                // in-process; building the RelExpr lets `explain` render it.
                let input = self.build_rel_expr(&w.input()?)?;
                let wraps = wrap_spec(&input.heading(), w);
                Some(RelExpr::Wrap {
                    input: Box::new(input),
                    wraps,
                })
            }
            Expr::Unwrap(u) => {
                let input = self.build_rel_expr(&u.input()?)?;
                let names: Vec<String> = u.attrs().map(|t| t.text().to_string()).collect();
                Some(RelExpr::Unwrap {
                    input: Box::new(input),
                    names,
                })
            }
            _ => None,
        }
    }

    /// Build the substitute chain `Rename?(Project(Extend(input)))` that
    /// overwrites each `(new, type, scalar)` pair, dropping the attributes in
    /// `removed`. A pair whose `new` already exists in the heading is extended
    /// under a temp `__coddl_replace_tmp_<new>` and renamed back (so the Extend
    /// never collides). Shared by `replace` (removed = the attributes the values
    /// *read*) and `update` (removed = the *target* attributes); `peel_substitute`
    /// recovers the SET pairs regardless of what `Project` drops.
    fn build_substitute_chain(
        &self,
        input: RelExpr,
        pairs: Vec<(String, Type, ScalarExpr)>,
        removed: HashSet<String>,
    ) -> RelExpr {
        let in_heading = input.heading();
        let mut extends: Vec<(String, Type, ScalarExpr)> = Vec::new();
        let mut renames: Vec<(String, String)> = Vec::new();
        for (new, ty, scalar) in pairs {
            let extend_name = if in_heading.lookup(&new).is_some() {
                let t = format!("__coddl_replace_tmp_{new}");
                renames.push((t.clone(), new));
                t
            } else {
                new
            };
            extends.push((extend_name, ty, scalar));
        }
        let keep: Vec<String> = in_heading
            .attrs()
            .iter()
            .map(|(n, _)| n.clone())
            .chain(extends.iter().map(|(n, _, _)| n.clone()))
            .filter(|n| !removed.contains(n))
            .collect();
        let mut node = RelExpr::Project {
            input: Box::new(RelExpr::Extend {
                input: Box::new(input),
                extends,
            }),
            keep,
        };
        if !renames.is_empty() {
            node = RelExpr::Rename {
                input: Box::new(node),
                renames,
            };
        }
        node
    }

    /// Walk an `extend` value expression into a RelIR [`ScalarExpr`], or `None`
    /// if it uses anything the SQL renderer can't express yet (a comparison,
    /// call, etc.) — in which case the whole `extend` declines the push. Covers
    /// attribute references, Integer/Text/Character literals, and the
    /// arithmetic/concatenation binary operators (Chunk 1's scalars). Cannot
    /// reuse `literal_value`, which drops `CHAR_LIT` (the predicate/bind path
    /// has no Character `Value`).
    fn build_scalar_expr(&self, expr: &Expr) -> Option<ScalarExpr> {
        match expr {
            Expr::NameRef(n) => Some(ScalarExpr::Attr(n.ident()?.text().to_string())),
            Expr::Literal(l) => {
                let tok = l.token()?;
                match tok.kind() {
                    SyntaxKind::INTEGER_LIT => {
                        Some(ScalarExpr::Int(parse_integer_literal(tok.text())))
                    }
                    SyntaxKind::STRING_LIT => {
                        Some(ScalarExpr::Str(String::from_utf8(decode_string_literal(tok.text())).ok()?))
                    }
                    SyntaxKind::CHAR_LIT => Some(ScalarExpr::Char(decode_char_literal(tok.text()))),
                    _ => None,
                }
            }
            Expr::Binary(b) => {
                let op = match b.op_kind()? {
                    BinaryOp::Add => ScalarBinOp::Add,
                    BinaryOp::Sub => ScalarBinOp::Sub,
                    BinaryOp::Mul => ScalarBinOp::Mul,
                    BinaryOp::Div => ScalarBinOp::Div,
                    BinaryOp::Concat => ScalarBinOp::Concat,
                    _ => return None,
                };
                let lhs = self.build_scalar_expr(&b.lhs()?)?;
                let rhs = self.build_scalar_expr(&b.rhs()?)?;
                Some(ScalarExpr::Bin {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            _ => None,
        }
    }

    /// Flatten a `where` predicate into pushable conjuncts, in left-to-right
    /// order. A top-level `and` chain (`p and q and …`) splits into its operands
    /// recursively; each leaf must be a pushable `attr <cmp> literal`
    /// ([`build_predicate`]). Returns `false` as soon as any leaf isn't pushable
    /// (leaving `out` partially filled — the caller discards it and declines the
    /// whole push, so the restriction runs in-process where arbitrary Boolean
    /// predicates are evaluated per tuple). `where` is heading-preserving, so all
    /// conjuncts resolve against the same `heading`. The resulting one-`Restrict`-
    /// per-conjunct tree is exactly what stacked `R where p where q` builds, so
    /// the two spellings emit identical SQL (`resolve` ANDs them in one `WHERE`).
    fn collect_conjuncts(&self, expr: &Expr, heading: &Heading, out: &mut Vec<Predicate>) -> bool {
        if let Expr::Binary(b) = expr {
            if matches!(b.op_kind(), Some(BinaryOp::And)) {
                let (Some(lhs), Some(rhs)) = (b.lhs(), b.rhs()) else {
                    return false;
                };
                return self.collect_conjuncts(&lhs, heading, out)
                    && self.collect_conjuncts(&rhs, heading, out);
            }
        }
        match self.build_predicate(expr, heading) {
            Some(p) => {
                out.push(p);
                true
            }
            None => false,
        }
    }

    /// Recognize a single `attr = literal` (or `literal = attr`) restriction
    /// predicate over `heading`. Anything else (conjunctions, attr-vs-attr,
    /// non-literal operands, comparisons other than `=`) returns `None` so
    /// the restriction falls back to the in-process `where` path.
    fn build_predicate(&self, expr: &Expr, heading: &Heading) -> Option<Predicate> {
        let b = match expr {
            Expr::Binary(b) => b,
            _ => return None,
        };
        // A pushable restriction is a single `attr <cmp> literal`. Map the
        // surface comparison operator to a RelIR `CmpOp`; any other operator
        // (logical, arithmetic, …) declines the push and runs in-process.
        let op = match b.op_kind()? {
            BinaryOp::Eq => CmpOp::Eq,
            BinaryOp::NotEq => CmpOp::Ne,
            BinaryOp::Lt => CmpOp::Lt,
            BinaryOp::LtEq => CmpOp::LtEq,
            BinaryOp::Gt => CmpOp::Gt,
            BinaryOp::GtEq => CmpOp::GtEq,
            _ => return None,
        };
        let lhs = b.lhs()?;
        let rhs = b.rhs()?;
        // The attribute may be either operand. With it on the right
        // (`literal OP attr`) the operator is flipped so the stored predicate is
        // always `attr OP' literal` (`5 < id` ⇒ `id > 5`).
        let (attr, op, lit_expr) = match (attr_ref_name(&lhs), attr_ref_name(&rhs)) {
            (Some(a), None) => (a, op, rhs),
            (None, Some(a)) => (a, op.flip(), lhs),
            // attr-vs-attr or literal-vs-literal: not a pushable comparison.
            _ => return None,
        };
        heading.lookup(&attr)?;
        let value = self.literal_value(&lit_expr)?;
        Some(Predicate::AttrCmp { attr, op, value })
    }

    /// Convert a literal AST node to a RelIR `Literal`, or `None` for forms
    /// the pushdown doesn't bind yet (rationals, characters, non-UTF-8 text).
    fn literal_value(&self, expr: &Expr) -> Option<RelLiteral> {
        match expr {
            Expr::Literal(lit) => {
                let token = lit.token()?;
                match token.kind() {
                    SyntaxKind::INTEGER_LIT => {
                        Some(RelLiteral::Integer(parse_integer_literal(token.text())))
                    }
                    SyntaxKind::STRING_LIT => {
                        let bytes = decode_string_literal(token.text());
                        String::from_utf8(bytes).ok().map(RelLiteral::Text)
                    }
                    _ => None,
                }
            }
            Expr::BoolLit(b) => b.value().map(RelLiteral::Boolean),
            _ => None,
        }
    }

    /// Lower a baked `SqlQuery` to an `Inst::Query`: dedup the plan by its
    /// text-stable id, emit one `Inst::Const` per bind value, and return the
    /// SSA value holding the (relation) result.
    /// Register a baked `SqlQuery` as a module plan, deduping by its text-stable
    /// id, and return the dense per-module plan id. `result_heading_id` is the
    /// interned heading the runtime marshals rows into (unused for DML, which
    /// returns no rows — pass the operand heading). Shared by `emit_query` and
    /// `emit_dml`.
    fn register_plan(&mut self, query: &SqlQuery, result_heading_id: HeadingId) -> u32 {
        if let Some(id) = self.plan_ids.get(&query.plan_id.0) {
            *id
        } else {
            let id = self.next_plan_id;
            self.next_plan_id += 1;
            self.plans.push(PlanEntry {
                plan_id: id,
                db_name: self.db_name.clone().unwrap_or_default(),
                sql: query.sql.text.clone(),
                param_count: query.sql.param_count,
                result_heading_id,
            });
            self.plan_ids.insert(query.plan_id.0, id);
            id
        }
    }

    /// Emit one `Inst::Const` per bind value and return the `(ValueId, ProcType)`
    /// param list a `Query`/`Dml` instruction passes to the runtime. Shared by
    /// `emit_query` and `emit_dml`.
    fn emit_params(&mut self, query: &SqlQuery) -> Vec<(ValueId, ProcType)> {
        let mut params: Vec<(ValueId, ProcType)> = Vec::with_capacity(query.params.len());
        for v in &query.params {
            let (value, ty) = match v {
                Value::Integer(n) => (Const::Integer(*n), ProcType::Integer),
                Value::Text(s) => (Const::Text(s.clone().into_bytes()), ProcType::Text),
                Value::Boolean(b) => (Const::Boolean(*b), ProcType::Boolean),
            };
            let dst = self.fresh_value();
            self.record_type(dst, ty.clone());
            self.insts.push(Inst::Const {
                dst,
                value,
                ty: ty.clone(),
            });
            params.push((dst, ty));
        }
        params
    }

    fn emit_query(&mut self, query: SqlQuery) -> ValueId {
        let result_heading_id = self.intern_heading(&query.result_heading);
        let plan_id = self.register_plan(&query, result_heading_id);
        let params = self.emit_params(&query);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(result_heading_id));
        self.insts.push(Inst::Query {
            dst,
            plan_id,
            params,
            heading_id: result_heading_id,
        });
        dst
    }

    /// Register a baked DML `SqlQuery` and emit an `Inst::Dml` to fire it for
    /// effect (no result bound). Mirrors `emit_query` minus the result value.
    fn emit_dml(&mut self, query: SqlQuery) {
        // The DML plan returns no rows; its registered heading is unused but
        // `PlanEntry` carries one, so intern the operand heading honestly.
        let result_heading_id = self.intern_heading(&query.result_heading);
        let plan_id = self.register_plan(&query, result_heading_id);
        let params = self.emit_params(&query);
        self.insts.push(Inst::Dml { plan_id, params });
    }

    /// Lower a unary prefix expression. Phase 21 handles `Extract`:
    /// emit `Inst::Extract` with the source's heading id. If the
    /// source isn't bound to any local (i.e., it's a temporary —
    /// e.g., a freshly-allocated `R where p`), emit `Inst::Release`
    /// after extract so the heap payload is freed.
    fn lower_unary_expr(&mut self, ue: &UnaryExpr) -> ValueId {
        let op = ue.op_kind().expect("typechecked unary expr has an op");
        match op {
            UnaryOp::Extract => {
                let operand_expr = ue
                    .operand()
                    .expect("typechecked extract has an operand");
                let src = self.lower_expr(&operand_expr);
                let heading_id = match self.value_type(src) {
                    ProcType::Relation(id) => id,
                    other => unreachable!(
                        "extract on non-relation `{other}` survived typecheck"
                    ),
                };
                let heading = self.headings[heading_id.0 as usize].clone();
                let dst = self.fresh_value();
                self.record_type(dst, ProcType::Tuple(heading));
                self.insts.push(Inst::Extract {
                    dst,
                    src,
                    heading_id,
                });
                // If the source isn't owned by a local, it's a temporary that
                // must be released — but NOT here. Extract copied the record's
                // cells into the tuple as *borrowed* `(ptr,len)` values; the
                // relation drop walker frees those `Text` cells, so releasing
                // the source now would dangle them. Defer to function exit,
                // after every use of the extracted fields (including uses past a
                // `transaction [...]` this extract sat inside). A let-bound
                // source is released at its own scope exit, which is likewise
                // after the extracted fields are consumed.
                let is_owned = self
                    .locals
                    .iter()
                    .any(|layer| layer.values().any(|(vid, _)| *vid == src));
                if !is_owned {
                    self.deferred_relation_releases.push(src);
                }
                dst
            }
        }
    }

    /// Resolve a `NameRef`. The active `locals` scope is consulted
    /// first. When inside a `where` predicate
    /// (`outer_locals_for_capture` is `Some`), a miss in the active
    /// scope checks the saved outer scope; a hit there is a capture,
    /// which Phase 20 deferred (T0022). Names that resolve nowhere
    /// fall through to a Unit placeholder ValueId — diagnostic-free
    /// input doesn't reach this branch in practice.
    fn lower_name_ref(&mut self, n: &NameRef) -> ValueId {
        if let Some(name_tok) = n.ident() {
            let name = name_tok.text();
            // An aliased relation binding is always resolved by the pushdown
            // cut (`try_lower_pushed` runs before this in `lower_expr`).
            // Reaching here for one means the bind-time `try_push` gate and the
            // force-time push disagree — a lowerer bug, not a user error.
            debug_assert!(
                self.lookup_alias(name).is_none(),
                "alias `{name}` reached lower_name_ref; pushdown should have resolved it"
            );
            if let Some((v, _ty)) = self.lookup_local(name) {
                return v;
            }
            // Public relvar reference: emit a slot load + retain. The
            // typechecker has already enforced this only happens inside
            // a `transaction [...]` (T0025); the consumer (`where` /
            // `extract` / `write_relation`) is responsible for releasing
            // the temporary via the same fresh-source detection Phase 21
            // installed for extract.
            if let Some(binding) = self.public_relvars.get(name).cloned() {
                // Reaching here means the cut didn't push this relvar read, so
                // it stays in-process — mark it so `main` materializes its slot.
                self.legacy_used_relvars.insert(binding.name.clone());
                let dst = self.fresh_value();
                self.record_type(dst, ProcType::Relation(binding.heading_id));
                self.insts.push(Inst::RelvarRead {
                    dst,
                    name: binding.name,
                    heading_id: binding.heading_id,
                });
                return dst;
            }
            // Private relvar reference: an in-memory slot load + retain (same
            // `RelvarRead` node as public, no SQL source). Mark it so `main`
            // inits / releases its slot.
            if let Some(&heading_id) = self.private_relvars.get(name) {
                self.used_private_relvars.insert(name.to_string());
                let dst = self.fresh_value();
                self.record_type(dst, ProcType::Relation(heading_id));
                self.insts.push(Inst::RelvarRead {
                    dst,
                    name: name.to_string(),
                    heading_id,
                });
                return dst;
            }
            if let Some(outer) = &self.outer_locals_for_capture {
                let captured = outer.iter().rev().any(|l| l.contains_key(name));
                if captured {
                    self.diagnostics.push(Diagnostic::error(
                        self.node_span(n.syntax()),
                        "T0022",
                        format!(
                            "identifier `{name}` is captured from an outer scope; \
                             `where`-predicate captures are not yet supported"
                        ),
                    ));
                }
            }
        }
        let v = self.fresh_value();
        self.record_type(v, ProcType::Unit);
        v
    }

    fn lower_bool_lit(&mut self, b: &BoolLit) -> ValueId {
        let value = b.value().unwrap_or(false);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Boolean);
        self.insts.push(Inst::Const {
            dst,
            value: Const::Boolean(value),
            ty: ProcType::Boolean,
        });
        dst
    }

    /// Lower a binary infix expression. Dispatches on the parsed
    /// op kind. `Where` is the relational case (synthesizes a
    /// predicate helper Function); everything else is a scalar
    /// `Inst::ScalarOp`.
    fn lower_binary_expr(&mut self, bin: &BinaryExpr) -> ValueId {
        let op = bin.op_kind().expect("typechecked binary expr has an op");
        if matches!(op, BinaryOp::Where) {
            return self.lower_where_expr(bin);
        }
        if matches!(
            op,
            BinaryOp::Join
                | BinaryOp::Times
                | BinaryOp::Compose
                | BinaryOp::Intersect
                | BinaryOp::Union
                | BinaryOp::Minus
        ) {
            return self.lower_join_inprocess(bin);
        }
        let scalar_op = match op {
            BinaryOp::Eq => ScalarOp::Eq,
            BinaryOp::NotEq => ScalarOp::NotEq,
            BinaryOp::Lt => ScalarOp::Lt,
            BinaryOp::Gt => ScalarOp::Gt,
            BinaryOp::LtEq => ScalarOp::LtEq,
            BinaryOp::GtEq => ScalarOp::GtEq,
            BinaryOp::And => ScalarOp::And,
            BinaryOp::Or => ScalarOp::Or,
            BinaryOp::Add => ScalarOp::Add,
            BinaryOp::Sub => ScalarOp::Sub,
            BinaryOp::Mul => ScalarOp::Mul,
            BinaryOp::Div => ScalarOp::Div,
            BinaryOp::Concat => ScalarOp::Concat,
            BinaryOp::Where
            | BinaryOp::Join
            | BinaryOp::Times
            | BinaryOp::Compose
            | BinaryOp::Intersect
            | BinaryOp::Union
            | BinaryOp::Minus => {
                unreachable!("handled above")
            }
        };
        let mut lhs = bin
            .lhs()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.fresh_value());
        let mut rhs = bin
            .rhs()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.fresh_value());
        // The operand machine type and the result type depend on the op:
        // comparison/logical compare arbitrary scalars → Boolean; arithmetic
        // is Integer → Integer; concat normalizes Character operands to Text →
        // Text (so `ScalarOp::Concat` always sees Text operands).
        let (operand_type, result_type) = match scalar_op {
            ScalarOp::Add | ScalarOp::Sub | ScalarOp::Mul | ScalarOp::Div => {
                (ProcType::Integer, ProcType::Integer)
            }
            ScalarOp::Concat => {
                lhs = self.coerce_to_text(lhs);
                rhs = self.coerce_to_text(rhs);
                (ProcType::Text, ProcType::Text)
            }
            ScalarOp::Eq
            | ScalarOp::NotEq
            | ScalarOp::Lt
            | ScalarOp::Gt
            | ScalarOp::LtEq
            | ScalarOp::GtEq
            | ScalarOp::And
            | ScalarOp::Or => (self.value_type(lhs), ProcType::Boolean),
        };
        let dst = self.fresh_value();
        self.record_type(dst, result_type.clone());
        self.insts.push(Inst::ScalarOp {
            dst,
            op: scalar_op,
            operand_type,
            lhs,
            rhs,
        });
        // A `Concat` allocates a fresh heap `Text` — mark it owned. Then
        // release any owned `Text` *operands* that no local owns: chained
        // concats (`a||b||c` — the inner result), `Character`→`Text`
        // coercions, and `coddl_text_eq` operands all borrow then drop here.
        // No-op for Integer/Boolean operands and for let-bound owned locals.
        if matches!(result_type, ProcType::Text) {
            self.mark_text_owned(dst);
        }
        self.release_text_temp(lhs);
        self.release_text_temp(rhs);
        dst
    }

    /// Normalize a scalar operand to `Text` for concatenation: a `Character`
    /// value is converted via [`Inst::CharToText`]; a value already `Text`
    /// passes through unchanged.
    fn coerce_to_text(&mut self, v: ValueId) -> ValueId {
        if matches!(self.value_type(v), ProcType::Character) {
            let dst = self.fresh_value();
            self.record_type(dst, ProcType::Text);
            self.insts.push(Inst::CharToText { dst, src: v });
            // `CharToText` allocates a fresh heap `Text` (`coddl_char_to_text`).
            self.mark_text_owned(dst);
            dst
        } else {
            v
        }
    }

    /// Build the RelIR for a binary relational expression (`where`, `join`,
    /// `times`, `compose`). `join`/`times` → the Algebra-A `AND` node;
    /// `compose` → `AND` with the shared attributes projected away (the canonical
    /// AND-then-REMOVE). Operands build recursively; the cut decides SQL vs
    /// in-process by `origin()`. Shared by `build_rel_expr` (the SQL-push path)
    /// and `lower_join_inprocess` (the in-process path) so the lowering is
    /// identical on both. `None` for non-relational binaries.
    fn build_rel_binary(&self, b: &BinaryExpr) -> Option<RelExpr> {
        match b.op_kind() {
            Some(BinaryOp::Where) => {
                let input = self.build_rel_expr(&b.lhs()?)?;
                let heading = input.heading();
                // Decompose a conjunctive predicate `p and q and …` into one
                // `Restrict` per conjunct — the identical RelIR `R where p where
                // q` produces, which `coddl-sqlemit`'s `resolve` then coalesces
                // into a single `WHERE p AND q`. So the two surface spellings
                // converge on one pushed query. Declines the whole push (→ the
                // in-process `where` path, which evaluates arbitrary Boolean
                // predicates per tuple) if any conjunct isn't a pushable
                // `attr <cmp> literal`. `where` is heading-preserving, so every
                // conjunct resolves against the same operand `heading`.
                let mut preds = Vec::new();
                if !self.collect_conjuncts(&b.rhs()?, &heading, &mut preds) {
                    return None;
                }
                let mut expr = input;
                for pred in preds {
                    expr = RelExpr::Restrict {
                        input: Box::new(expr),
                        pred,
                    };
                }
                Some(expr)
            }
            // `join` / `times` / `intersect` all lower to the A-core `AND`
            // node: `intersect` is `AND` on identical headings (a join on every
            // attribute = set intersection). The heading check that
            // distinguishes the three is the typechecker's, not the lowerer's.
            Some(BinaryOp::Join) | Some(BinaryOp::Times) | Some(BinaryOp::Intersect) => {
                let lhs = self.build_rel_expr(&b.lhs()?)?;
                let rhs = self.build_rel_expr(&b.rhs()?)?;
                Some(RelExpr::And {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            // `union` → the A-core `OR` node (identical headings, typechecked).
            Some(BinaryOp::Union) => {
                let lhs = self.build_rel_expr(&b.lhs()?)?;
                let rhs = self.build_rel_expr(&b.rhs()?)?;
                Some(RelExpr::Or {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            // `minus` → the A-core `AND NOT` node (identical headings, typechecked).
            Some(BinaryOp::Minus) => {
                let lhs = self.build_rel_expr(&b.lhs()?)?;
                let rhs = self.build_rel_expr(&b.rhs()?)?;
                Some(RelExpr::Minus {
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            // `A compose B` → `AND` then REMOVE the shared attributes: a
            // `Project` keeping only the attributes that appear in exactly one
            // operand. (Typecheck guarantees ≥1 shared attribute.)
            Some(BinaryOp::Compose) => {
                let lhs = self.build_rel_expr(&b.lhs()?)?;
                let rhs = self.build_rel_expr(&b.rhs()?)?;
                let shared = lhs.heading().shared_names(&rhs.heading());
                let union = lhs.heading().union(&rhs.heading()).ok()?;
                let keep: Vec<String> = union
                    .attrs()
                    .iter()
                    .map(|(name, _)| name.clone())
                    .filter(|name| !shared.contains(name))
                    .collect();
                Some(RelExpr::Project {
                    input: Box::new(RelExpr::And {
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    }),
                    keep,
                })
            }
            _ => None,
        }
    }

    /// Lower an in-process relational binary — `join` / `times` / `intersect`
    /// (→ `Inst::Join`), `compose` (→ `Inst::Join` + `Inst::Project`), or
    /// `union` (→ `Inst::Union`). Builds the RelIR and consumes it via the
    /// in-process RelExpr→ProcIR path (`MaterializedRelvar` → slot read, `And`
    /// → `Inst::Join`, `Or` → `Inst::Union`, `Project` → `Inst::Project`). Falls
    /// back to lowering the operands directly for shapes the consumer declines
    /// (e.g. a relation-literal operand), dispatching on the surface operator.
    fn lower_join_inprocess(&mut self, bin: &BinaryExpr) -> ValueId {
        if let (Some(lhs_e), Some(rhs_e)) = (bin.lhs(), bin.rhs()) {
            if let Some(rel) = self.build_rel_binary(bin) {
                if let Some(v) = self.lower_relexpr_inprocess(&rel) {
                    return v;
                }
            }
            let lhs = self.lower_expr(&lhs_e);
            let rhs = self.lower_expr(&rhs_e);
            // `union` / `minus` are set ops, not joins — dispatch before
            // `emit_join`. (The primary path above handles the common case; this
            // fallback fires for shapes the RelExpr consumer declines, e.g. a
            // relation-literal operand.)
            if matches!(bin.op_kind(), Some(BinaryOp::Union)) {
                return self.emit_union(lhs, rhs);
            }
            if matches!(bin.op_kind(), Some(BinaryOp::Minus)) {
                return self.emit_minus(lhs, rhs);
            }
            let joined = self.emit_join(lhs, rhs);
            // `compose` removes the shared attributes after the join.
            if matches!(bin.op_kind(), Some(BinaryOp::Compose)) {
                let keep = self.compose_keep(lhs, rhs);
                return self.emit_project(joined, &keep);
            }
            return joined;
        }
        let v = self.fresh_value();
        self.record_type(v, ProcType::Unit);
        v
    }

    /// The `compose` keep-list (attributes appearing in exactly one operand),
    /// computed from two already-lowered relation values' headings.
    fn compose_keep(&self, lhs: ValueId, rhs: ValueId) -> Vec<String> {
        let heading_of = |v: ValueId| match self.value_type(v) {
            ProcType::Relation(id) => self.headings[id.0 as usize].clone(),
            other => unreachable!("compose operand non-relation `{other}` survived typecheck"),
        };
        let lhs_h = heading_of(lhs);
        let rhs_h = heading_of(rhs);
        let shared = lhs_h.shared_names(&rhs_h);
        lhs_h
            .union(&rhs_h)
            .expect("typechecked compose has compatible shared attributes")
            .attrs()
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !shared.contains(name))
            .collect()
    }

    /// Consume a materialized `RelExpr` subtree into ProcIR. `Some(value)` for
    /// the nodes the in-process path handles today (`MaterializedRelvar`, `And`,
    /// `Project`); `None` otherwise so the caller falls back.
    fn lower_relexpr_inprocess(&mut self, rel: &RelExpr) -> Option<ValueId> {
        match rel {
            RelExpr::MaterializedRelvar { name, .. } => {
                let &heading_id = self.private_relvars.get(name)?;
                self.used_private_relvars.insert(name.clone());
                let dst = self.fresh_value();
                self.record_type(dst, ProcType::Relation(heading_id));
                self.insts.push(Inst::RelvarRead {
                    dst,
                    name: name.clone(),
                    heading_id,
                });
                Some(dst)
            }
            RelExpr::And { lhs, rhs } => {
                let l = self.lower_relexpr_inprocess(lhs)?;
                let r = self.lower_relexpr_inprocess(rhs)?;
                Some(self.emit_join(l, r))
            }
            RelExpr::Or { lhs, rhs } => {
                let l = self.lower_relexpr_inprocess(lhs)?;
                let r = self.lower_relexpr_inprocess(rhs)?;
                Some(self.emit_union(l, r))
            }
            RelExpr::Minus { lhs, rhs } => {
                let l = self.lower_relexpr_inprocess(lhs)?;
                let r = self.lower_relexpr_inprocess(rhs)?;
                Some(self.emit_minus(l, r))
            }
            // `compose` lowers to `Project{And}`: lower the join, then narrow to
            // the kept attributes via `Inst::Project`.
            RelExpr::Project { input, keep } => {
                let src = self.lower_relexpr_inprocess(input)?;
                Some(self.emit_project(src, keep))
            }
            RelExpr::TClose { input } => {
                let src = self.lower_relexpr_inprocess(input)?;
                Some(self.emit_tclose(src))
            }
            // wrap/unwrap restructure into the node's (already-computed) heading.
            RelExpr::Wrap { input, .. } | RelExpr::Unwrap { input, .. } => {
                let src = self.lower_relexpr_inprocess(input)?;
                let dst_heading = rel.heading();
                Some(self.emit_restructure(src, dst_heading))
            }
            _ => None,
        }
    }

    /// Emit `Inst::Join` over two already-lowered relation values, computing
    /// the union result heading. (RC mirrors the existing read path: operands
    /// are read temps; the result is rc=1.)
    fn emit_join(&mut self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let lhs_heading_id = match self.value_type(lhs) {
            ProcType::Relation(id) => id,
            other => unreachable!("join lhs non-relation `{other}` survived typecheck"),
        };
        let rhs_heading_id = match self.value_type(rhs) {
            ProcType::Relation(id) => id,
            other => unreachable!("join rhs non-relation `{other}` survived typecheck"),
        };
        let lhs_heading = self.headings[lhs_heading_id.0 as usize].clone();
        let rhs_heading = self.headings[rhs_heading_id.0 as usize].clone();
        let result_heading = lhs_heading
            .union(&rhs_heading)
            .expect("typechecked join has compatible shared attributes");
        let result_heading_id = self.intern_heading(&result_heading);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(result_heading_id));
        self.insts.push(Inst::Join {
            dst,
            lhs,
            rhs,
            lhs_heading_id,
            rhs_heading_id,
            result_heading_id,
        });
        dst
    }

    /// Emit `Inst::Union` over two already-lowered relation values with
    /// identical headings (surface `union`). The result heading is that shared
    /// heading; the runtime concatenates and re-seals (content-aware dedup).
    fn emit_union(&mut self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let heading_id = match self.value_type(lhs) {
            ProcType::Relation(id) => id,
            other => unreachable!("union lhs non-relation `{other}` survived typecheck"),
        };
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        self.insts.push(Inst::Union {
            dst,
            lhs,
            rhs,
            heading_id,
        });
        dst
    }

    /// Emit `Inst::Minus` over two already-lowered relation values with
    /// identical headings (surface `minus`). The result heading is that shared
    /// heading; the runtime keeps each `lhs` record not present in `rhs`.
    fn emit_minus(&mut self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let heading_id = match self.value_type(lhs) {
            ProcType::Relation(id) => id,
            other => unreachable!("minus lhs non-relation `{other}` survived typecheck"),
        };
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        self.insts.push(Inst::Minus {
            dst,
            lhs,
            rhs,
            heading_id,
        });
        dst
    }

    /// Emit `Inst::TClose` over an already-lowered binary relation value
    /// (surface `tclose`). The result heading equals the operand heading —
    /// closure is direction-agnostic and adds tuples without changing the
    /// heading — so one `heading_id` describes both.
    fn emit_tclose(&mut self, src: ValueId) -> ValueId {
        let heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("tclose on non-relation `{other}` survived typecheck"),
        };
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        self.insts.push(Inst::TClose {
            dst,
            src,
            heading_id,
        });
        dst
    }

    /// Emit `Inst::Project` narrowing an already-lowered relation value to the
    /// `keep` attributes (the relation-level counterpart of `lower_project_expr`
    /// steps 1–3). The result heading re-canonicalizes via `Heading::new`.
    fn emit_project(&mut self, src: ValueId, keep: &[String]) -> ValueId {
        let src_heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("project on non-relation `{other}` survived typecheck"),
        };
        let src_heading = self.headings[src_heading_id.0 as usize].clone();
        let narrowed: Vec<(String, Type)> = src_heading
            .attrs()
            .iter()
            .filter(|(name, _)| keep.iter().any(|k| k == name))
            .cloned()
            .collect();
        let result_heading_id = self.intern_heading(&Heading::new(narrowed));
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(result_heading_id));
        self.insts.push(Inst::Project {
            dst,
            src,
            src_heading_id,
            result_heading_id,
        });
        dst
    }

    /// Lower `R where pred`: synthesize a helper function
    /// `__coddl_where_<n>` that takes a record pointer and returns
    /// Boolean, populate its body by lowering the predicate against
    /// a scope whose only entries are the heading's attributes
    /// (pre-loaded via `Inst::AttrLoad`), then emit `Inst::Where` in
    /// the enclosing function.
    fn lower_where_expr(&mut self, bin: &BinaryExpr) -> ValueId {
        // 1. Lower the relation operand in the enclosing function's
        //    scope.
        let src = bin
            .lhs()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked where has a relation lhs");
        let heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("where on non-relation `{other}` survived typecheck"),
        };
        let heading = self.headings[heading_id.0 as usize].clone();
        let layout = crate::layout::record_layout(&heading);

        // 2. Mint a fresh predicate function name.
        let pred_name = format!("__coddl_where_{}", self.next_where);
        self.next_where += 1;

        // 3. Snapshot the enclosing function's per-function state,
        //    install a fresh state for the predicate, and stash the
        //    outer locals on `outer_locals_for_capture` so the
        //    predicate's NameRef walk can detect captures.
        let saved_next_value = std::mem::replace(&mut self.next_value, 0);
        let saved_next_block = std::mem::replace(&mut self.next_block, 0);
        let saved_insts = std::mem::take(&mut self.insts);
        let saved_locals = std::mem::replace(&mut self.locals, vec![HashMap::new()]);
        let saved_aliases = std::mem::replace(&mut self.relexpr_aliases, vec![HashMap::new()]);
        let saved_value_types = std::mem::take(&mut self.value_types);
        // The helper resets `next_value` to 0, so its ValueIds collide with the
        // enclosing function's; `owned_texts` is keyed by ValueId, so isolate it
        // too (a predicate may concat: `where g = "a" || s`). Same for the
        // deferred extract-source list (an `extract` inside the predicate).
        let saved_owned_texts = std::mem::take(&mut self.owned_texts);
        let saved_deferred = std::mem::take(&mut self.deferred_relation_releases);
        self.outer_locals_for_capture = Some(saved_locals.clone());

        // 4. Build the predicate body. The function has a single
        //    parameter `record_ptr: Pointer`. Pre-emit `AttrLoad` for
        //    each heading attribute at function entry; bind each in
        //    the predicate scope under its source-level name.
        let block_id = self.fresh_block();
        let record_ptr = self.fresh_value();
        self.record_type(record_ptr, ProcType::Pointer);
        for attr in &layout.attrs {
            let attr_type = proc_type_from_kind(attr.kind);
            let dst = self.fresh_value();
            self.record_type(dst, attr_type.clone());
            self.insts.push(Inst::AttrLoad {
                dst,
                src: record_ptr,
                offset: attr.offset,
                attr_type: attr_type.clone(),
            });
            self.bind_local(attr.name.clone(), dst, attr_type);
        }

        // 5. Lower the predicate body.
        let pred_value = bin
            .rhs()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked where has a predicate rhs");

        // Release any deferred extract sources before the predicate returns —
        // `pred_value` (a Boolean) has already consumed the borrowed fields.
        self.drain_deferred_relation_releases();

        // 6. Close the predicate function.
        let block = BasicBlock {
            id: block_id,
            insts: std::mem::take(&mut self.insts),
            terminator: Terminator::Return(Some(pred_value)),
        };
        self.functions.push(Function {
            name: pred_name.clone(),
            linkage_name: pred_name.clone(),
            params: vec![("record_ptr".to_string(), ProcType::Pointer)],
            return_type: ProcType::Boolean,
            blocks: vec![block],
        });

        // 7. Restore the enclosing function's state.
        self.next_value = saved_next_value;
        self.next_block = saved_next_block;
        self.insts = saved_insts;
        self.locals = saved_locals;
        self.relexpr_aliases = saved_aliases;
        self.value_types = saved_value_types;
        self.owned_texts = saved_owned_texts;
        self.deferred_relation_releases = saved_deferred;
        self.outer_locals_for_capture = None;

        // 8. Emit Inst::Where in the enclosing function.
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        self.insts.push(Inst::Where {
            dst,
            src,
            predicate_linkage: pred_name,
            heading_id,
        });
        // If the where's source isn't owned by any local (e.g. it's a
        // fresh `RelvarRead` chained directly into `where`), release
        // the temporary now that the predicate has finished reading it.
        // Same pattern Phase 21 installed for `extract`'s source —
        // generalised so chains like `RelvarRead → where → extract`
        // stay balanced without manual let-binding.
        let src_owned = self
            .locals
            .iter()
            .any(|layer| layer.values().any(|(vid, _)| *vid == src));
        if !src_owned {
            self.insts.push(Inst::Release { src });
        }
        dst
    }

    /// Lower a `{a: e1, b: e2, …}` tuple literal. Each field's
    /// expression lowers in source order; the resulting
    /// `(name, ValueId)` pairs are reordered to canonical (name-sorted)
    /// heading order in the emitted `Inst::TupleLit`. The heading
    /// itself is built from the per-field static types — which the
    /// typechecker already enforces match the surface declaration.
    fn lower_tuple_lit(&mut self, tup: &TupleLit) -> ValueId {
        let mut field_pairs: Vec<(String, ValueId, ProcType)> = Vec::new();
        // `Text` cell values consumed directly into this tuple — collected so
        // `lower_relation_lit` can release the producer ref if the tuple becomes
        // a relation cell. A direct `Text` field contributes its value; a *fresh*
        // nested `TupleLit` field contributes its own collected temps. A
        // `NameRef`-aliased field (tuple or text) is skipped — its value may be
        // referenced elsewhere, so releasing it here would double-free.
        let mut cell_text_temps: Vec<ValueId> = Vec::new();
        for field in tup.fields() {
            let name_tok = match field.name() {
                Some(t) => t,
                None => continue,
            };
            let value_expr = match field.value() {
                Some(v) => v,
                None => continue,
            };
            let id = self.lower_expr(&value_expr);
            let ty = self.value_type(id);
            match &ty {
                ProcType::Text if !matches!(value_expr, Expr::NameRef(_)) => {
                    cell_text_temps.push(id);
                }
                ProcType::Tuple(_) if matches!(value_expr, Expr::TupleLit(_)) => {
                    if let Some(sub) = self.tuple_cell_text_temps.get(&id) {
                        cell_text_temps.extend(sub.iter().copied());
                    }
                }
                _ => {}
            }
            field_pairs.push((name_tok.text().to_string(), id, ty));
        }
        // Canonical order — `Heading::new` will sort the type-level
        // pairs identically; emitting the SSA fields in the same order
        // means backends can iterate the heading and the fields in
        // lockstep without re-sorting.
        field_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let heading = Heading::new(
            field_pairs
                .iter()
                .map(|(n, _, ty)| (n.clone(), type_from_proc(ty)))
                .collect(),
        );
        let fields: Vec<(String, ValueId)> = field_pairs
            .into_iter()
            .map(|(n, v, _)| (n, v))
            .collect();
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Tuple(heading.clone()));
        self.insts.push(Inst::TupleLit {
            dst,
            fields,
            heading,
        });
        if !cell_text_temps.is_empty() {
            self.tuple_cell_text_temps.insert(dst, cell_text_temps);
        }
        dst
    }

    /// Lower `<expr>.<field>`. The base's `ProcType` must be a
    /// `Tuple(H)` after typecheck; the field's `ProcType` is derived
    /// from `H`'s entry for the named attribute via `proc_type_from_type`.
    fn lower_field_access(&mut self, fa: &FieldAccess) -> ValueId {
        let base_expr = fa.base().expect("typechecked field-access has a base");
        let src = self.lower_expr(&base_expr);
        let src_ty = self.value_type(src);
        let heading = match &src_ty {
            ProcType::Tuple(h) => h.clone(),
            other => unreachable!("field access on non-tuple `{other}` survived typecheck"),
        };
        let field_name = fa
            .field()
            .expect("typechecked field-access has a field token")
            .text()
            .to_string();
        let field_type = heading
            .lookup(&field_name)
            .map(proc_type_from_type)
            .unwrap_or_else(|| {
                unreachable!("unknown field `{field_name}` survived typecheck")
            });
        let dst = self.fresh_value();
        self.record_type(dst, field_type.clone());
        self.insts.push(Inst::TupleField {
            dst,
            src,
            field_name,
            field_type,
        });
        dst
    }

    fn lower_transaction_expr(&mut self, txn: &TransactionExpr) -> ValueId {
        // Wrap the body in synthetic begin/commit calls. The runtime
        // externs are no-ops in v1 (all public-relvar reads are served
        // from the materialized in-memory slot) but the shape is
        // load-bearing for the conformance rule: T0025 forces every
        // public-relvar access to be inside a transaction, and the
        // bracket pair is where real BEGIN/COMMIT discipline will
        // land when write-through arrives.
        self.push_local_scope();
        self.emit_tx_call("coddl_begin_tx");
        let value = match txn.body() {
            Some(b) => self.lower_block(&b),
            None => self.fresh_value(),
        };
        self.emit_tx_call("coddl_commit_tx");
        // If the body's tail value is a heap-typed local in this scope, it
        // *escapes* as the transaction's result — retain it so the scope
        // release below leaves the caller a live `rc=1` reference. (A relation
        // returned from a transaction, e.g. `let x = R; x`, is a real case;
        // without this the local is freed before the caller can use it.) A
        // fresh tail value not bound to a local isn't in the release set, so it
        // survives without a retain.
        let val_ty = self.value_type(value);
        let in_scope = self
            .locals
            .last()
            .map(|scope| scope.values().any(|(v, _)| *v == value))
            .unwrap_or(false);
        // Owned `Text` escapes a transaction the same way a relation does
        // (`let m = transaction [ let t = "a"||b; t ]`): retain before the
        // scope release so the caller keeps a live reference. The escaped
        // ValueId stays in `owned_texts` (function-global), so the outer
        // binding's scope-exit release balances this retain.
        let escapes = in_scope && self.needs_scope_release(value, &val_ty);
        if escapes {
            self.insts.push(Inst::Retain { src: value });
        }
        self.release_top_scope_heap_locals();
        self.pop_local_scope();
        value
    }

    /// Emit a synthetic `Inst::Call` to a transaction runtime extern.
    /// The dst is allocated and typed `Integer` (`CoddlStatus`) but
    /// never consumed — the no-op runtime always returns Ok in v1.
    fn emit_tx_call(&mut self, linkage: &'static str) {
        self.ensure_runtime_extern(linkage);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Integer);
        self.insts.push(Inst::Call {
            dst: Some(dst),
            callee: linkage.to_string(),
            args: Vec::new(),
            return_type: ProcType::Integer,
        });
    }

    /// Lower a `Relation { <tuple-lit>, … }` literal. Each nested
    /// `TupleLit` lowers to its Phase-18 `Inst::TupleLit`; the
    /// resulting `ValueId`s become operands of an
    /// `Inst::RelationLit { dst, tuples, heading_id }`. The heading
    /// is the first tuple's; we intern it so backends emit at most
    /// one static descriptor per unique heading. Empty literals are
    /// kept out by the typechecker (T0018); reaching here with zero
    /// tuples is an internal bug.
    fn lower_relation_lit(&mut self, rel: &RelationLit) -> ValueId {
        let tuples: Vec<TupleLit> = rel.tuples().collect();
        assert!(
            !tuples.is_empty(),
            "empty relation literal survived typecheck (T0018)"
        );
        let mut tuple_values: Vec<ValueId> = Vec::with_capacity(tuples.len());
        let mut heading: Option<Heading> = None;
        for tup in &tuples {
            let v = self.lower_tuple_lit(tup);
            tuple_values.push(v);
            if heading.is_none() {
                if let ProcType::Tuple(h) = self.value_type(v) {
                    heading = Some(h);
                }
            }
        }
        let heading = heading.expect("typechecked tuple has a heading");
        let heading_id = self.intern_heading(&heading);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        // Backend `RelationLit` retain-on-store gives the relation its own
        // reference to each `Text` cell. Release the producer reference of any
        // owned `Text` *temporary* consumed directly into a cell so the cell's
        // retained ref is the sole owner. `release_text_temp` no-ops on locals
        // and literals; the collected temps are single-use fresh expressions.
        let cell_temps: Vec<ValueId> = tuple_values
            .iter()
            .filter_map(|tv| self.tuple_cell_text_temps.remove(tv))
            .flatten()
            .collect();
        self.insts.push(Inst::RelationLit {
            dst,
            tuples: tuple_values,
            heading_id,
        });
        for t in cell_temps {
            self.release_text_temp(t);
        }
        dst
    }

    fn lower_literal(&mut self, lit: &Literal) -> ValueId {
        let token = lit.token().expect("typechecked literal has a token");
        let (value, ty) = match token.kind() {
            SyntaxKind::STRING_LIT => (
                Const::Text(decode_string_literal(token.text())),
                ProcType::Text,
            ),
            SyntaxKind::INTEGER_LIT => {
                let n = parse_integer_literal(token.text());
                (Const::Integer(n), ProcType::Integer)
            }
            SyntaxKind::CHAR_LIT => {
                let cp = decode_char_literal(token.text());
                (Const::Character(cp), ProcType::Character)
            }
            // RATIONAL_LIT / APPROXIMATE_LIT land here as the language
            // exercises them. The typechecker already accepts them; lowering
            // catches up when the runtime grows to consume them.
            other => unreachable!("literal kind {other:?} not yet lowered"),
        };
        let dst = self.fresh_value();
        self.record_type(dst, ty.clone());
        self.insts.push(Inst::Const { dst, value, ty });
        dst
    }

    fn lower_call(&mut self, call: &CallExpr) -> ValueId {
        let callee_name = match call.callee() {
            Some(Expr::NameRef(n)) => n.ident().map(|t| t.text().to_string()),
            _ => None,
        };
        let surface = callee_name.expect("typechecked call has a NameRef callee");

        // Polymorphic-Relation builtins are lowered to specialized
        // ProcIR ops carrying their argument's `HeadingId`. The
        // backends look the descriptor up in `Module::headings` to
        // emit the per-call-site descriptor pointer.
        if surface == "write_relation" {
            return self.lower_write_relation_call(call);
        }

        let ext = self
            .lookup_extern(&surface)
            .unwrap_or_else(|| unreachable!("unknown callee `{surface}` survived typecheck"));
        let linkage = ext.linkage.to_string();
        let return_type = ext.return_type.clone();

        // Lower each argument in the order the operator declared its
        // parameters; the typechecker has guaranteed every declared
        // parameter is supplied exactly once.
        let arg_list = call.args().expect("typechecked call has an arg list");
        let supplied: Vec<NamedArg> = arg_list.args().collect();
        let mut arg_values: Vec<ValueId> = Vec::with_capacity(ext.params.len());
        for (pname, _) in ext.params {
            let arg = supplied
                .iter()
                .find(|a| a.name().map(|t| t.text().to_string()).as_deref() == Some(*pname))
                .unwrap_or_else(|| unreachable!("missing arg `{pname}` survived typecheck"));
            let value_expr = arg.value().expect("typechecked named arg has a value");
            arg_values.push(self.lower_expr(&value_expr));
        }

        self.ensure_extern(ext);

        let returns_text = matches!(return_type, ProcType::Text);
        let dst = if matches!(return_type, ProcType::Unit) {
            None
        } else {
            let v = self.fresh_value();
            self.record_type(v, return_type.clone());
            Some(v)
        };
        // Snapshot the `Text` arguments before `arg_values` moves into the
        // Call: a builtin borrows its `(ptr,len)` operands, so an *owned*
        // `Text` temp passed inline (`write_line{message:"a"||name}`) must be
        // released after the call. (Filtered to owned temps by `release_text_temp`.)
        let text_args: Vec<ValueId> = arg_values
            .iter()
            .copied()
            .filter(|v| matches!(self.value_type(*v), ProcType::Text))
            .collect();
        self.insts.push(Inst::Call {
            dst,
            callee: linkage,
            args: arg_values,
            return_type,
        });
        // `read_line` hands back a fresh heap `Text` — own it so scope exit
        // (or a consuming temp-release) frees it.
        if returns_text {
            if let Some(v) = dst {
                self.mark_text_owned(v);
            }
        }
        for v in text_args {
            self.release_text_temp(v);
        }
        // For Unit-returning calls there is no real SSA value; return a
        // fresh id so the surrounding expression machinery has a place
        // to plug in once it grows real consumers. Today nothing reads
        // it.
        dst.unwrap_or_else(|| {
            let v = self.fresh_value();
            self.record_type(v, ProcType::Unit);
            v
        })
    }

    /// Lower `write_relation { rel: <expr> }` to `Inst::WriteRelation`.
    /// The `rel` argument's static type is `ProcType::Relation(id)`;
    /// we pull the id off via `value_type` and embed it in the
    /// instruction so the backend doesn't need value-type tracking.
    /// `write_relation` returns Unit; the surrounding expression
    /// machinery gets a placeholder ValueId.
    fn lower_write_relation_call(&mut self, call: &CallExpr) -> ValueId {
        let arg_list = call.args().expect("typechecked call has an arg list");
        let rel_arg = arg_list
            .args()
            .find(|a| a.name().map(|t| t.text().to_string()).as_deref() == Some("rel"))
            .expect("typechecked write_relation has a `rel` arg");
        let rel_expr = rel_arg.value().expect("rel arg has a value expression");
        let rel = self.lower_expr(&rel_expr);
        let heading_id = match self.value_type(rel) {
            ProcType::Relation(id) => id,
            other => unreachable!(
                "write_relation got non-relation arg type `{other}` past typecheck"
            ),
        };
        self.insts.push(Inst::WriteRelation { rel, heading_id });
        let v = self.fresh_value();
        self.record_type(v, ProcType::Unit);
        v
    }
}

/// The attribute name if `expr` is a bare `NameRef`, else `None`. Used by
/// predicate recognition to tell `attr = literal` from `literal = attr`.
fn attr_ref_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::NameRef(n) => n.ident().map(|t| t.text().to_string()),
        _ => None,
    }
}

/// Whether a RelIR subtree contains any `Restrict` — a pushed `where` that
/// reduces cardinality. Used by [`Lowerer::guard_no_full_relvar_pull`] to tell a
/// full-relvar scan (no `Restrict`, the whole table) from a filtered query
/// (a `Restrict` pushed). `Restrict` is the only row-reducing node today;
/// `Project`/`Rename`/`Extend`/`Wrap`/`Unwrap`/`TClose` only reshape, and the
/// set-ops/join recurse into both operands.
fn contains_restrict(rel: &RelExpr) -> bool {
    match rel {
        RelExpr::Restrict { .. } => true,
        RelExpr::Project { input, .. }
        | RelExpr::Rename { input, .. }
        | RelExpr::Extend { input, .. }
        | RelExpr::TClose { input }
        | RelExpr::Wrap { input, .. }
        | RelExpr::Unwrap { input, .. } => contains_restrict(input),
        RelExpr::And { lhs, rhs } | RelExpr::Or { lhs, rhs } | RelExpr::Minus { lhs, rhs } => {
            contains_restrict(lhs) || contains_restrict(rhs)
        }
        RelExpr::RelvarRef { .. } | RelExpr::MaterializedRelvar { .. } => false,
    }
}

/// The application-level name of the public relvar a RelIR subtree is rooted in
/// (following the `lhs` of binary nodes), for the [`Lowerer::guard_no_full_relvar_pull`]
/// panic message. `None` for a materialized root.
fn relvar_root_name(rel: &RelExpr) -> Option<&str> {
    match rel {
        RelExpr::RelvarRef { name, .. } => Some(name),
        RelExpr::Restrict { input, .. }
        | RelExpr::Project { input, .. }
        | RelExpr::Rename { input, .. }
        | RelExpr::Extend { input, .. }
        | RelExpr::TClose { input }
        | RelExpr::Wrap { input, .. }
        | RelExpr::Unwrap { input, .. } => relvar_root_name(input),
        RelExpr::And { lhs, .. } | RelExpr::Or { lhs, .. } | RelExpr::Minus { lhs, .. } => {
            relvar_root_name(lhs)
        }
        RelExpr::MaterializedRelvar { .. } => None,
    }
}

/// Convert a `record_layout` attribute kind tag to its machine-level
/// `ProcType`. Mirrors the runtime's `CoddlAttrKind`. Used by the
/// predicate-function synthesis to type the per-attribute `AttrLoad`
/// SSA values.
fn proc_type_from_kind(kind: u32) -> ProcType {
    use crate::layout::kind_tag;
    match kind {
        k if k == kind_tag::INTEGER => ProcType::Integer,
        k if k == kind_tag::BOOLEAN => ProcType::Boolean,
        k if k == kind_tag::TEXT => ProcType::Text,
        other => unreachable!("unsupported attr kind {other} in predicate"),
    }
}

/// Convert a surface `Type` (as it appears in a `Heading`) to its
/// machine-level `ProcType`. The mapping is total over the types the
/// Convert a surface `Type` (as it appears in a `Heading`) to its
/// machine-level `ProcType`. Pure on scalar / tuple cases. The
/// `Relation` case is handled by `Lowerer::proc_type_from_type`
/// (which needs the heading interner); the free function below is
/// the simple total mapping for non-relation cells. Phase 19's tuple
/// cells don't yet carry relations, so this path is fine.
fn proc_type_from_type(ty: &Type) -> ProcType {
    match ty {
        Type::Integer => ProcType::Integer,
        Type::Rational => ProcType::Rational,
        Type::Approximate => ProcType::Approximate,
        Type::Text => ProcType::Text,
        Type::Character => ProcType::Character,
        Type::Binary => ProcType::Binary,
        Type::Byte => ProcType::Byte,
        Type::Boolean => ProcType::Boolean,
        Type::Tuple(h) => ProcType::Tuple(h.clone()),
        Type::Relation(_) => {
            unreachable!(
                "Type::Relation inside a non-relation context — use Lowerer::proc_type_from_type"
            )
        }
        Type::Unknown => unreachable!("Type::Unknown survived typecheck"),
    }
}

/// Recover a surface `Type` for a `ProcType` we previously derived
/// from one. Used by tuple-literal lowering to round-trip
/// per-field types through `Heading::new`. The mapping is exact for
/// scalar and tuple cells; relation cells need the lowerer's
/// heading table to recover the surface heading, so this free
/// function rejects them — Phase 19's tuple-literal walk doesn't
/// yet need to thread `ProcType::Relation` back through `Type`.
fn type_from_proc(pt: &ProcType) -> Type {
    match pt {
        ProcType::Integer => Type::Integer,
        ProcType::Rational => Type::Rational,
        ProcType::Approximate => Type::Approximate,
        ProcType::Text => Type::Text,
        ProcType::Character => Type::Character,
        ProcType::Binary => Type::Binary,
        ProcType::Byte => Type::Byte,
        ProcType::Boolean => Type::Boolean,
        ProcType::Unit => Type::unit(),
        ProcType::Tuple(h) => Type::Tuple(h.clone()),
        ProcType::Pointer => {
            unreachable!("Pointer ProcType in a tuple field — not reachable in Phase 19")
        }
        ProcType::Relation(_) => {
            unreachable!(
                "ProcType::Relation in a tuple cell — needs heading interner; not reachable in Phase 19"
            )
        }
    }
}

fn proc_type_from_builtin_name(name: &str) -> ProcType {
    match name {
        "Integer" => ProcType::Integer,
        "Rational" => ProcType::Rational,
        "Approximate" => ProcType::Approximate,
        "Text" => ProcType::Text,
        "Character" => ProcType::Character,
        "Binary" => ProcType::Binary,
        "Byte" => ProcType::Byte,
        "Boolean" => ProcType::Boolean,
        // Typechecker already rejected anything else via T0005; the
        // diagnostic-free invariant means we never get here.
        other => unreachable!("unknown type `{other}` survived typecheck"),
    }
}

/// Decode the body of a `STRING_LIT` token (with surrounding `"`s) to
/// raw UTF-8 bytes. Recognizes the escape set spelled out in
/// `docs/grammar.md`: `\n`, `\r`, `\t`, `\"`, `\\`, `\u{...}`.
fn decode_string_literal(text: &str) -> Vec<u8> {
    let inner = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    let mut out = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(esc) = chars.next() else { break };
        match esc {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '"' => out.push(b'"'),
            '\\' => out.push(b'\\'),
            'u' => {
                // `\u{XXXX}` — the lexer already validated the form.
                if chars.next() != Some('{') {
                    break;
                }
                let mut hex = String::new();
                for h in chars.by_ref() {
                    if h == '}' {
                        break;
                    }
                    hex.push(h);
                }
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            _ => unreachable!("unknown escape `\\{esc}` survived lexing"),
        }
    }
    out
}

/// The result type of an `extend` value, mirroring the typechecker's rule:
/// arithmetic is `Integer`, concatenation is `Text`, an attribute reference
/// takes the operand heading's type, and a literal takes its own type. The
/// checker is the type authority (a clean typecheck guarantees the `Attr`
/// lookup succeeds); this just re-states the rule so the `Extend` node can
/// carry the type without relir re-deriving it.
/// The `(new_name, components_heading)` spec for a `wrap`, gathering each
/// component's type from the operand heading. Unknown components are dropped —
/// the typechecker has already reported them (T0027). Shared by `lower_wrap_expr`
/// (AST/in-process) and `build_rel_expr` (the RelIR/pushable path).
fn wrap_spec(in_heading: &Heading, we: &WrapExpr) -> Vec<(String, Heading)> {
    we.pairs()
        .filter_map(|pair| {
            let new = pair.name()?.text().to_string();
            let comps: Vec<(String, Type)> = pair
                .wrapped()
                .filter_map(|t| {
                    let n = t.text().to_string();
                    in_heading.lookup(&n).map(|ty| (n, ty.clone()))
                })
                .collect();
            Some((new, Heading::new(comps)))
        })
        .collect()
}

/// Collect the attribute names an AST scalar `Expr` references — the in-process
/// counterpart of the SQL `scalar_attr_refs` (which walks the built
/// `ScalarExpr`). Used by the general-expression `replace` desugar to compute
/// the removed set when the value isn't SQL-renderable. Mirrors the
/// typechecker's `attr_refs`, so the three agree on which attributes a value
/// reads. Walks `NameRef`/`Binary`/`Unary`.
fn ast_attr_refs(expr: &Expr, into: &mut HashSet<String>) {
    match expr {
        Expr::NameRef(n) => {
            if let Some(tok) = n.ident() {
                into.insert(tok.text().to_string());
            }
        }
        Expr::Binary(b) => {
            if let Some(lhs) = b.lhs() {
                ast_attr_refs(&lhs, into);
            }
            if let Some(rhs) = b.rhs() {
                ast_attr_refs(&rhs, into);
            }
        }
        Expr::Unary(u) => {
            if let Some(operand) = u.operand() {
                ast_attr_refs(&operand, into);
            }
        }
        _ => {}
    }
}

/// Collect the attribute names a `ScalarExpr` references — the "removed set" of
/// a general-expression `replace`. Walks `Attr` (a leaf ref) and `Bin` (both
/// operands); literals contribute nothing.
fn scalar_attr_refs(e: &ScalarExpr, into: &mut HashSet<String>) {
    match e {
        ScalarExpr::Attr(name) => {
            into.insert(name.clone());
        }
        ScalarExpr::Bin { lhs, rhs, .. } => {
            scalar_attr_refs(lhs, into);
            scalar_attr_refs(rhs, into);
        }
        _ => {}
    }
}

fn scalar_result_type(e: &ScalarExpr, heading: &Heading) -> Type {
    match e {
        ScalarExpr::Attr(name) => heading.lookup(name).cloned().unwrap_or(Type::Unknown),
        ScalarExpr::Int(_) => Type::Integer,
        ScalarExpr::Str(_) => Type::Text,
        ScalarExpr::Char(_) => Type::Character,
        ScalarExpr::Bin { op, .. } => match op {
            ScalarBinOp::Concat => Type::Text,
            _ => Type::Integer,
        },
    }
}

/// Decode the body of a `CHAR_LIT` token (with surrounding `'`s) to its
/// Unicode scalar value. The lexer guarantees exactly one codepoint and the
/// same escape set as `STRING_LIT` (`\n`, `\r`, `\t`, `\"`, `\\`, `\u{...}`).
fn decode_char_literal(text: &str) -> u32 {
    let inner = text
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(text);
    let mut chars = inner.chars();
    let c = chars.next().expect("lexer rejects empty char literal");
    if c != '\\' {
        return c as u32;
    }
    match chars.next().expect("lexer rejects a lone backslash") {
        'n' => '\n' as u32,
        'r' => '\r' as u32,
        't' => '\t' as u32,
        '"' => '"' as u32,
        '\'' => '\'' as u32,
        '\\' => '\\' as u32,
        'u' => {
            // `\u{XXXX}` — the lexer already validated the form.
            debug_assert_eq!(chars.next(), Some('{'));
            let hex: String = chars.by_ref().take_while(|h| *h != '}').collect();
            u32::from_str_radix(&hex, 16).expect("lexer validated the codepoint")
        }
        esc => unreachable!("unknown escape `\\{esc}` survived lexing"),
    }
}

/// Parse an `INTEGER_LIT` lexeme into its `i64` value. Handles the
/// four bases the lexer recognizes (`0x`, `0b`, `0o`, `0d`) plus the
/// default decimal form. Underscores between digits are stripped.
fn parse_integer_literal(text: &str) -> i64 {
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    let (radix, digits) = if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        (16, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        (2, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        (8, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0d")
        .or_else(|| cleaned.strip_prefix("0D"))
    {
        (10, rest)
    } else {
        (10, cleaned.as_str())
    };
    i64::from_str_radix(digits, radix).expect("lexer validated the digits")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_WORLD: &str = "program hello_world;\n\
                               \n\
                               oper main {}\n\
                               [\n\
                                   write_line{message: \"Hello, world!\"};\n\
                               ];\n";

    fn lower_ok(src: &str) -> Module {
        let out = lower(src, FileId(0));
        // Lowering requires no *errors*; warnings (e.g. T0032 unused-binding)
        // are orthogonal and don't block code generation.
        let errors: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.severity == coddl_diagnostics::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        out.module
            .expect("module should be produced on clean check")
    }

    #[test]
    fn hello_world_lowers_to_four_functions() {
        // `main` plus three runtime externs: write_line for the user
        // call, init + shutdown for the auto-wrapped startup
        // housekeeping docs/runtime.md requires.
        let m = lower_ok(HELLO_WORLD);
        let names: Vec<_> = m.functions.iter().map(|f| f.name.as_str()).collect();
        for needed in [
            "main",
            "write_line",
            "coddl_runtime_init",
            "coddl_runtime_shutdown",
        ] {
            assert!(names.contains(&needed), "expected {needed} in {names:?}");
        }
        assert_eq!(m.functions.len(), 4);
    }

    // ── SQL pushdown ──────────────────────────────────────────────────

    const HELLO_WORLD_DB: &str = "\
program hello_world_db;\n\
database greetings;\n\
\n\
public relvar Greetings {\n\
    id: Integer,\n\
    message: Text,\n\
}\n\
key { id };\n\
\n\
oper main {}\n\
[\n\
    let g = transaction [\n\
        extract (Greetings where id = 1)\n\
    ];\n\
    write_line { message: g.message };\n\
];\n";

    fn greetings_plan() -> Plan {
        use coddl_plan::{BackendKind, ResolvedPublicRelvar, WritePolicy};
        use coddl_types::RelvarTable;
        Plan {
            program_name: "hello_world_db".to_string(),
            database_name: Some("greetings".to_string()),
            cd_relvars: RelvarTable::default(),
            cddb_relvars: RelvarTable::default(),
            backend_kind: BackendKind::Sqlite,
            resolved: vec![ResolvedPublicRelvar {
                app_name: "Greetings".to_string(),
                catalog_name: "Greetings".to_string(),
                heading: Heading::new(vec![
                    ("id".to_string(), Type::Integer),
                    ("message".to_string(), Type::Text),
                ]),
                table_name: "greetings".to_string(),
                columns: vec![
                    ("id".to_string(), "id".to_string()),
                    ("message".to_string(), "message".to_string()),
                ],
                keys: vec![vec!["id".to_string()]],
                write_policy: WritePolicy::ReadOnly,
            }],
            db_file_default: Some("/tmp/greetings.sqlite".to_string()),
        }
    }

    /// `greetings_plan` with the base relvar marked writable — the shape a
    /// SQL-backed write target has (surgical DML lowering exercises this).
    fn greetings_rw_plan() -> Plan {
        use coddl_plan::WritePolicy;
        let mut plan = greetings_plan();
        for r in &mut plan.resolved {
            r.write_policy = WritePolicy::ReadWrite;
        }
        plan
    }

    fn lower_ok_with_plan(src: &str, plan: &Plan) -> Module {
        let out = lower_with_plan(src, FileId(0), Some(plan));
        // Only errors block lowering; T0032 unused-binding warnings don't.
        let errors: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.severity == coddl_diagnostics::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        out.module.expect("module should be produced on clean check")
    }

    #[test]
    fn explain_captures_pushed_relir_with_its_sql() {
        let out = explain_with_plan(HELLO_WORLD_DB, FileId(0), Some(&greetings_plan()));
        assert_eq!(out.relir.len(), 1, "one pushed query expected");
        let entry = &out.relir[0];
        assert_eq!(
            entry.sql,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(
            entry.expr.render(),
            "Restrict { id = 1 }\n  RelvarRef Greetings { db: greetings, table: greetings }"
        );
    }

    #[test]
    fn lower_does_not_capture_relir() {
        // The compile path never clones a RelExpr — `relir` stays empty.
        let out = lower_with_plan(HELLO_WORLD_DB, FileId(0), Some(&greetings_plan()));
        assert!(out.relir.is_empty());
    }

    const HELLO_WORLD_DB_CONJUNCT: &str = "\
program hello_world_db;\n\
database greetings;\n\
\n\
public relvar Greetings {\n\
    id: Integer,\n\
    message: Text,\n\
}\n\
key { id };\n\
\n\
oper main {}\n\
[\n\
    let g = transaction [\n\
        extract (Greetings where id = 1 and message = \"hi\")\n\
    ];\n\
    write_line { message: g.message };\n\
];\n";

    const HELLO_WORLD_DB_OR_WHERE: &str = "\
program hello_world_db;\n\
database greetings;\n\
\n\
public relvar Greetings {\n\
    id: Integer,\n\
    message: Text,\n\
}\n\
key { id };\n\
\n\
oper main {}\n\
[\n\
    let g = transaction [\n\
        Greetings where id = 1 or id = 2\n\
    ];\n\
];\n";

    #[test]
    #[should_panic(expected = "pushdown gap")]
    fn nonpushable_where_over_whole_public_relvar_trips_the_guard() {
        // `Greetings where (id = 1 or id = 2)` — the disjunction doesn't push, so
        // the whole `where` declines and its operand `Greetings` is an unfiltered
        // full-table scan feeding an in-process filter: pulling the whole relvar
        // into memory. The S1 tripwire fires.
        let _ = lower_ok_with_plan(HELLO_WORLD_DB_OR_WHERE, &greetings_plan());
    }

    const HELLO_WORLD_DB_PARTIAL_WHERE: &str = "\
program hello_world_db;\n\
database greetings;\n\
\n\
public relvar Greetings {\n\
    id: Integer,\n\
    message: Text,\n\
}\n\
key { id };\n\
\n\
oper main {}\n\
[\n\
    let g = transaction [\n\
        Greetings where id = 1 where id = 2 or id = 3\n\
    ];\n\
];\n";

    #[test]
    fn pushed_filter_then_residual_does_not_trip_the_guard() {
        // `(Greetings where id = 1) where (id = 2 or id = 3)` — the inner `where`
        // pushes (`WHERE id = 1`), so only a filtered subset (a query) is pulled;
        // the residual disjunction runs in-process over it. Not a whole-relvar
        // pull, so the guard stays quiet (it would panic if it fired).
        let _ = lower_ok_with_plan(HELLO_WORLD_DB_PARTIAL_WHERE, &greetings_plan());
    }

    #[test]
    fn conjunctive_where_pushes_as_one_select_with_and() {
        // `R where p and q` decomposes into one `Restrict` per conjunct — the
        // same tree `R where p where q` builds — and `resolve` coalesces them
        // into a single `WHERE p AND q`. So the conjunction form pushes (it used
        // to decline and run in-process) and matches the stacked spelling.
        let out = explain_with_plan(HELLO_WORLD_DB_CONJUNCT, FileId(0), Some(&greetings_plan()));
        assert_eq!(out.relir.len(), 1, "one pushed query expected");
        let entry = &out.relir[0];
        assert_eq!(
            entry.sql,
            // Conjuncts in source order; full heading keeps key `id` → no DISTINCT.
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ? AND "message" = ?"#
        );
        assert_eq!(
            entry.expr.render(),
            "Restrict { message = \"hi\" }\n  \
             Restrict { id = 1 }\n    \
             RelvarRef Greetings { db: greetings, table: greetings }"
        );

        // It really pushed: no in-process predicate helper was synthesized.
        let m = lower_ok_with_plan(HELLO_WORLD_DB_CONJUNCT, &greetings_plan());
        assert!(
            !m.functions
                .iter()
                .any(|f| f.name.starts_with("__coddl_where_")),
            "conjunctive where should push, not synthesize a predicate helper:\n{m}"
        );
        assert_eq!(m.plans.len(), 1, "exactly one baked plan");
        assert_eq!(m.plans[0].param_count, 2, "two bound conjunct literals");
    }

    #[test]
    fn relvar_where_lowers_to_one_query_with_no_slot_init() {
        let m = lower_ok_with_plan(HELLO_WORLD_DB, &greetings_plan());
        let main = m.functions.iter().find(|f| f.name == "main").expect("main");
        let insts = &main.blocks[0].insts;

        let queries = insts
            .iter()
            .filter(|i| matches!(i, Inst::Query { .. }))
            .count();
        assert_eq!(queries, 1, "expected exactly one Inst::Query in:\n{m}");

        // The pushed subtree replaces the legacy materialize + filter path.
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::RelvarSlotInit { .. })),
            "startup slot init should be suppressed in:\n{m}"
        );
        assert!(
            !insts
                .iter()
                .any(|i| matches!(i, Inst::RelvarSlotRelease { .. })),
            "slot release should be suppressed in:\n{m}"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Where { .. })),
            "where should be pushed to SQL, not run in-process"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::RelvarRead { .. })),
            "relvar read should be served by the query"
        );
        assert!(
            !m.functions
                .iter()
                .any(|f| f.name.starts_with("__coddl_where_")),
            "no predicate helper should be synthesized for a pushed where"
        );

        // Exactly one baked plan, with the expected SQL and bind count.
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            // No DISTINCT: the full heading keeps key `id`, so already a set.
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(m.plans[0].param_count, 1);
        assert_eq!(m.plans[0].db_name, "greetings");

        // The prologue registers the database and the plan.
        assert!(
            insts.iter().any(|i| matches!(i, Inst::RegisterDatabase)),
            "prologue should register the database in:\n{m}"
        );
        assert_eq!(
            insts
                .iter()
                .filter(|i| matches!(i, Inst::RegisterPlan { .. }))
                .count(),
            1,
            "prologue should register exactly one plan"
        );
    }

    // ── binding transparency (relation `let`-aliases fold into pushdown) ──

    const BT_HEAD: &str = "\
program hello_world_db;\n\
database greetings;\n\
public relvar Greetings { id: Integer, message: Text } key { id };\n\
";

    fn bt_main_insts(src: &str) -> (Module, Vec<Inst>) {
        let m = lower_ok_with_plan(src, &greetings_plan());
        let insts = m
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main")
            .blocks[0]
            .insts
            .clone();
        (m, insts)
    }

    #[test]
    fn binding_transparency_pushes_where_through_let() {
        // `gg` and `greeting` are transparent aliases, so `extract greeting`
        // folds to the same single pushed query as `extract (Greetings where
        // id = 1)` — no `SELECT *`, no in-process `where`.
        let src = format!(
            "{BT_HEAD}oper main {{}} [\n\
             let m = transaction [\n\
                 let gg = Greetings;\n\
                 let greeting = gg where id = 1;\n\
                 extract greeting\n\
             ];\n\
             ];\n"
        );
        let (m, insts) = bt_main_insts(&src);
        assert_eq!(
            insts.iter().filter(|i| matches!(i, Inst::Query { .. })).count(),
            1,
            "should fold to one pushed query in:\n{m}"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Where { .. })),
            "where should push through the binding, not run in-process:\n{m}"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::RelvarRead { .. })),
            "no in-process relvar read:\n{m}"
        );
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?"#
        );
    }

    #[test]
    fn unused_relvar_binding_emits_no_query() {
        // `gg` is bound but never forced — its alias emits nothing, so only the
        // `where`d read runs (one query, not a stray `SELECT *`).
        let src = format!(
            "{BT_HEAD}oper main {{}} [\n\
             let m = transaction [\n\
                 let gg = Greetings;\n\
                 extract (Greetings where id = 1)\n\
             ];\n\
             ];\n"
        );
        let (m, insts) = bt_main_insts(&src);
        assert_eq!(
            insts.iter().filter(|i| matches!(i, Inst::Query { .. })).count(),
            1,
            "the unused `gg` alias should add no query in:\n{m}"
        );
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?"#
        );
    }

    #[test]
    fn binding_to_transaction_result_stays_in_process() {
        // A `transaction [...]` result is a materialized value, not an alias,
        // so a `where` over it outside the transaction runs in-process — a
        // public relvar can't be read outside its transaction.
        let src = format!(
            "{BT_HEAD}oper main {{}} [\n\
             let g = transaction [ Greetings ];\n\
             let hw = g where id = 1;\n\
             let t = extract hw;\n\
             ];\n"
        );
        let (m, insts) = bt_main_insts(&src);
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Where { .. })),
            "where over a transaction-result binding must be in-process:\n{m}"
        );
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            r#"SELECT "id", "message" FROM "greetings""#,
            "the relvar materializes (SELECT *) inside the transaction:\n{m}"
        );
    }

    #[test]
    fn binding_transparency_pushes_project_through_let() {
        // `project` folds through the binding into a narrowed pushed SELECT.
        let src = format!(
            "{BT_HEAD}oper main {{}} [\n\
             let m = transaction [\n\
                 let gg = Greetings;\n\
                 let p = gg project {{message}};\n\
                 p\n\
             ];\n\
             ];\n"
        );
        let (m, insts) = bt_main_insts(&src);
        assert_eq!(
            insts.iter().filter(|i| matches!(i, Inst::Query { .. })).count(),
            1,
            "project should fold to one pushed query in:\n{m}"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Project { .. })),
            "project should push through the binding, not run in-process:\n{m}"
        );
        assert!(
            m.plans[0].sql.contains(r#""message""#) && !m.plans[0].sql.contains(r#""id""#),
            "expected a narrowed SELECT on `message` in:\n{m}"
        );
    }

    const HELLO_WORLD_DB_PROJECT: &str = "\
program hello_world_db;\n\
database greetings;\n\
\n\
public relvar Greetings {\n\
    id: Integer,\n\
    message: Text,\n\
}\n\
key { id };\n\
\n\
oper main {}\n\
[\n\
    let g = transaction [\n\
        extract (Greetings where id = 1 project {message})\n\
    ];\n\
    write_line { message: g.message };\n\
];\n";

    #[test]
    fn relvar_project_lowers_to_one_query_with_narrowed_sql() {
        let m = lower_ok_with_plan(HELLO_WORLD_DB_PROJECT, &greetings_plan());
        let main = m.functions.iter().find(|f| f.name == "main").expect("main");
        let insts = &main.blocks[0].insts;

        // The whole `Greetings where id = 1 project {message}` pushes to one
        // query; no in-process ops survive.
        let queries = insts
            .iter()
            .filter(|i| matches!(i, Inst::Query { .. }))
            .count();
        assert_eq!(queries, 1, "expected exactly one Inst::Query in:\n{m}");
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Where { .. })),
            "where should be pushed, not in-process"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::RelvarRead { .. })),
            "relvar read should be served by the query"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::RelvarSlotInit { .. })),
            "startup slot init should be suppressed in:\n{m}"
        );

        // The projection narrows the SELECT list to the single attribute.
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            // No DISTINCT: `where id = 1` on the key bounds cardinality to ≤ 1.
            r#"SELECT "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(m.plans[0].param_count, 1);
    }

    #[test]
    fn project_over_relation_literal_lowers_to_in_process_project() {
        // A relation-literal projection isn't relvar-rooted, so the cut
        // declines and it lowers in-process to `Inst::Project` (not a
        // pushed query), narrowing the heading to the kept attribute.
        let src = "oper main {} [ let _s = Relation { {a: 1, b: 2} } project {a}; ];";
        let out = lower(src, FileId(0));
        assert!(
            out.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            out.diagnostics
        );
        let m = out.module.expect("module on clean lowering");
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Query { .. })),
            "relation-literal project must not push to SQL:\n{m}"
        );
        let result_heading_id = insts
            .iter()
            .find_map(|i| match i {
                Inst::Project {
                    result_heading_id, ..
                } => Some(*result_heading_id),
                _ => None,
            })
            .expect("expected an Inst::Project");
        let h = &m.headings[result_heading_id.0 as usize];
        assert_eq!(h.attrs().len(), 1, "projection should narrow to one attr");
        assert!(h.lookup("a").is_some());
        assert!(h.lookup("b").is_none(), "`b` should be projected away");
    }

    #[test]
    fn relvar_all_but_pushes_same_narrowed_sql() {
        // `project all but {id}` over {id, message} keeps {message} — same
        // pushed SQL as `project {message}` (the complement resolves in the
        // frontend; RelIR carries a concrete keep set).
        let src = HELLO_WORLD_DB_PROJECT.replace("project {message}", "project all but {id}");
        let m = lower_ok_with_plan(&src, &greetings_plan());
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            r#"SELECT "message" FROM "greetings" WHERE "id" = ?"#
        );
    }

    #[test]
    fn project_all_but_over_relation_literal_keeps_complement_in_process() {
        // `Relation {{a, b}} project all but {a}` lowers in-process to
        // `Inst::Project` whose result heading is the complement `{b}`.
        let src = "oper main {} [ let _s = Relation { {a: 1, b: 2} } project all but {a}; ];";
        let out = lower(src, FileId(0));
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let m = out.module.expect("module");
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let result_heading_id = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::Project {
                    result_heading_id, ..
                } => Some(*result_heading_id),
                _ => None,
            })
            .expect("expected an Inst::Project");
        let h = &m.headings[result_heading_id.0 as usize];
        assert_eq!(h.attrs().len(), 1);
        assert!(h.lookup("b").is_some(), "complement keeps `b`");
        assert!(h.lookup("a").is_none(), "`a` was removed");
    }

    // ── arithmetic & concatenation lowering ──────────────────────────

    #[test]
    fn char_literal_lowers_to_const_character() {
        let src = "oper main {} [ let _c = 'a'; ];";
        let out = lower(src, FileId(0));
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let m = out.module.expect("module");
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks[0].insts.iter().any(
                |i| matches!(i, Inst::Const { value: Const::Character(c), .. } if *c == 'a' as u32)
            ),
            "char literal lowers to Const::Character"
        );
    }

    #[test]
    fn concat_with_char_lowers_char_to_text_then_concat() {
        // `"x" || 'y'` — the Character operand is normalized to Text via
        // `Inst::CharToText`, then concatenated via `ScalarOp::Concat`.
        let src = "oper main {} [ let _s = \"x\" || 'y'; ];";
        let out = lower(src, FileId(0));
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let m = out.module.expect("module");
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        assert!(
            insts.iter().any(|i| matches!(i, Inst::CharToText { .. })),
            "char operand normalized via CharToText"
        );
        assert!(
            insts
                .iter()
                .any(|i| matches!(i, Inst::ScalarOp { op: ScalarOp::Concat, .. })),
            "concatenation lowers to ScalarOp::Concat"
        );
    }

    #[test]
    fn arithmetic_in_where_predicate_lowers_scalar_add() {
        // `a + b > 2` over a relation literal lowers in-process; the predicate
        // helper computes `a + b` via `ScalarOp::Add`.
        let src = "oper main {} [ let _s = Relation { {a: 1, b: 2} } where a + b > 2; ];";
        let out = lower(src, FileId(0));
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let m = out.module.expect("module");
        let pred = m
            .functions
            .iter()
            .find(|f| f.name.starts_with("__coddl_where_"))
            .expect("predicate helper function");
        let has_add = pred
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| matches!(i, Inst::ScalarOp { op: ScalarOp::Add, .. }));
        assert!(has_add, "predicate body computes a + b via ScalarOp::Add");
    }

    #[test]
    fn relvar_rename_pushes_aliased_sql() {
        // `Greetings where id=1 rename {identifier: id, msg: message}` pushes
        // to one query with the relabel expressed via `AS`.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    let g = transaction [ extract (Greetings where id = 1 rename {identifier: id, msg: message}) ];
    write_line { message: g.msg };
];
";
        let m = lower_ok_with_plan(src, &greetings_plan());
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            r#"SELECT "id" AS "identifier", "message" AS "msg" FROM "greetings" WHERE "id" = ?"#
        );
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(
            main.blocks[0]
                .insts
                .iter()
                .filter(|i| matches!(i, Inst::Query { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn relvar_extend_pushes_computed_column_sql() {
        // `Greetings where id=1 extend {twice: id + id}` pushes to one query
        // with the computed column expressed via `(<expr>) AS`.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    let g = transaction [ extract (Greetings where id = 1 extend {twice: id + id}) ];
    write_line { message: g.message };
];
";
        let m = lower_ok_with_plan(src, &greetings_plan());
        assert_eq!(m.plans.len(), 1);
        assert_eq!(
            m.plans[0].sql,
            r#"SELECT "id", "message", ("id" + "id") AS "twice" FROM "greetings" WHERE "id" = ?"#
        );
    }

    #[test]
    fn self_assignment_private_relvar_emits_nothing() {
        // `R := R` on a private relvar is dead code: it lowers to no
        // instruction at all (no slot store). The typechecker already warned.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ R := R; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .all(|i| !matches!(i, Inst::RelvarSlotStore { .. })),
            "R := R must emit no slot store"
        );
    }

    #[test]
    fn self_assignment_public_relvar_emits_nothing() {
        // `Greetings := Greetings` is elided before the write-policy check, so
        // it emits no DML and reports no error even on a read-only relvar.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    transaction [ Greetings := Greetings; ];
];
";
        let m = lower_ok_with_plan(src, &greetings_plan());
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .all(|i| !matches!(i, Inst::Dml { .. } | Inst::InsertFrom { .. })),
            "Greetings := Greetings must emit no DML"
        );
    }

    #[test]
    fn truncate_public_relvar_emits_dml() {
        // `truncate Greetings` desugars to `Greetings := Greetings minus
        // Greetings` → a whole-table delete, emitted as surgical DML.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    transaction [ truncate Greetings; ];
];
";
        let m = lower_ok_with_plan(src, &greetings_rw_plan());
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| matches!(i, Inst::Dml { .. })),
            "truncate Greetings must emit surgical DML"
        );
    }

    #[test]
    fn truncate_private_relvar_emits_minus_and_store() {
        // `truncate R` on a private relvar lowers to the empty `R minus R`
        // value stored back into the slot.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ truncate R; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts: Vec<_> = main.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Minus { .. })),
            "truncate R must compute R minus R"
        );
        assert!(
            insts
                .iter()
                .any(|i| matches!(i, Inst::RelvarSlotStore { .. })),
            "truncate R must store the empty result into the slot"
        );
    }

    #[test]
    fn delete_public_relvar_emits_dml() {
        // `delete Greetings where id = 1` desugars to `Greetings := Greetings
        // minus (Greetings where id = 1)` → a surgical `DELETE … WHERE id = ?`.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    transaction [ delete Greetings where id = 1; ];
];
";
        let m = lower_ok_with_plan(src, &greetings_rw_plan());
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| matches!(i, Inst::Dml { .. })),
            "delete must emit surgical DML"
        );
    }

    #[test]
    fn delete_private_relvar_emits_minus_and_store() {
        // `delete R where a = 1` on a private relvar lowers to the kept rows
        // `R minus (R where a = 1)` stored back into the slot.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ delete R where a = 1; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts: Vec<_> = main.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Minus { .. })),
            "delete R where p must compute R minus (R where p)"
        );
        assert!(
            insts
                .iter()
                .any(|i| matches!(i, Inst::RelvarSlotStore { .. })),
            "delete must store the kept rows into the slot"
        );
    }

    #[test]
    fn insert_tuple_set_public_ships_rows() {
        // `insert Greetings { {…} }` ships the literal's rows — the tuple-set
        // isn't SQL-backed, so an idempotent batched-VALUES InsertFrom.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    transaction [ insert Greetings { {id: 7, message: \"x\"} }; ];
];
";
        let m = lower_ok_with_plan(src, &greetings_rw_plan());
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| matches!(i, Inst::InsertFrom { .. })),
            "insert of a tuple-set must ship rows via InsertFrom"
        );
    }

    #[test]
    fn insert_private_relvar_emits_union_and_store() {
        // `insert R { {a: 1} }` on a private relvar lowers to `R union <lit>`
        // stored back into the slot.
        let src = "program p; private relvar R { a: Integer } key { a }; \
                   oper main {} [ insert R { {a: 1} }; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts: Vec<_> = main.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Union { .. })),
            "insert R must compute R union source"
        );
        assert!(
            insts
                .iter()
                .any(|i| matches!(i, Inst::RelvarSlotStore { .. })),
            "insert must store the union into the slot"
        );
    }

    #[test]
    fn update_public_where_emits_dml() {
        // `update Greetings where id = 1 { message: … }` desugars to the
        // substitute-union shape → a surgical `UPDATE … SET … WHERE id = ?`.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    transaction [ update Greetings where id = 1 { message: \"hi\" }; ];
];
";
        let m = lower_ok_with_plan(src, &greetings_rw_plan());
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| matches!(i, Inst::Dml { .. })),
            "update must emit surgical DML"
        );
    }

    #[test]
    fn update_public_all_emits_dml() {
        // Update-all (no `where`) → a bare substitute → `UPDATE … SET …`.
        let src = "\
program hello_world_db;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [
    transaction [ update Greetings { message: \"hi\" }; ];
];
";
        let m = lower_ok_with_plan(src, &greetings_rw_plan());
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| matches!(i, Inst::Dml { .. })),
            "update-all must emit surgical DML"
        );
    }

    #[test]
    fn update_private_where_emits_minus_union_store() {
        // `update R where a = 1 { b: … }` private → (R minus (R where a=1)) union
        // ((R where a=1) «sub»), stored back.
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R where a = 1 { b: \"x\" }; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts: Vec<_> = main.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(insts.iter().any(|i| matches!(i, Inst::Minus { .. })), "unchanged = R minus matching");
        assert!(insts.iter().any(|i| matches!(i, Inst::Union { .. })), "result = unchanged union changed");
        assert!(
            insts.iter().any(|i| matches!(i, Inst::RelvarSlotStore { .. })),
            "update stores the result into the slot"
        );
    }

    #[test]
    fn update_private_all_emits_substitute_store_no_union() {
        // Update-all private → a bare substitute (Extend → …) stored back, with
        // no minus/union (there are no unchanged rows to preserve).
        let src = "program p; private relvar R { a: Integer, b: Text } key { a }; \
                   oper main {} [ update R { b: \"x\" }; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts: Vec<_> = main.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(insts.iter().any(|i| matches!(i, Inst::Extend { .. })), "substitute extends the new value");
        assert!(
            insts.iter().any(|i| matches!(i, Inst::RelvarSlotStore { .. })),
            "update-all stores the substituted relation"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Union { .. } | Inst::Minus { .. })),
            "update-all has no unchanged-rows union"
        );
    }

    #[test]
    fn extend_over_relation_literal_lowers_in_process() {
        // A materialized operand lowers to `Inst::Extend` plus a synthesized
        // `__coddl_extend_<n>` helper (two pointer params, void return).
        let src = "oper main {} [ let _s = Relation { {a: 1, b: 2} } extend {c: a + b}; ];";
        let out = lower(src, FileId(0));
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let m = out.module.expect("module");
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks[0]
                .insts
                .iter()
                .any(|i| matches!(i, Inst::Extend { .. })),
            "main emits Inst::Extend"
        );
        let helper = m
            .functions
            .iter()
            .find(|f| f.name.starts_with("__coddl_extend_"))
            .expect("synthesized extend helper");
        assert_eq!(helper.params.len(), 2, "helper has src + dst pointer params");
        assert_eq!(helper.return_type, ProcType::Unit);
        // The helper computes `a + b` (a ScalarOp::Add) and stores cells.
        let insts = &helper.blocks[0].insts;
        assert!(
            insts
                .iter()
                .any(|i| matches!(i, Inst::ScalarOp { op: ScalarOp::Add, .. })),
            "helper computes a + b"
        );
        assert!(
            insts.iter().any(|i| matches!(i, Inst::AttrStore { .. })),
            "helper stores the widened cells"
        );
    }

    #[test]
    fn rename_over_relation_literal_lowers_to_inst_rename() {
        // `Relation {{a, b}} rename {z: a}` lowers in-process to `Inst::Rename`
        // with the renamed (re-sorted) result heading {b, z} and perm [1, 0]
        // (dst b ← src 1, dst z ← src 0).
        let src = "oper main {} [ let _s = Relation { {a: 1, b: 2} } rename {z: a}; ];";
        let out = lower(src, FileId(0));
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        let m = out.module.expect("module");
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let (result_heading_id, perm) = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::Rename {
                    result_heading_id,
                    perm,
                    ..
                } => Some((*result_heading_id, perm.clone())),
                _ => None,
            })
            .expect("expected an Inst::Rename");
        let h = &m.headings[result_heading_id.0 as usize];
        assert!(h.lookup("z").is_some(), "renamed to `z`");
        assert!(h.lookup("b").is_some(), "`b` kept");
        assert!(h.lookup("a").is_none(), "`a` renamed away");
        assert_eq!(perm, vec![1, 0], "dst [b, z] ← src [a, b] indices");
    }

    #[test]
    fn materialized_where_still_lowers_in_process_with_pushdown_enabled() {
        // A relation-literal `where` is Materialized, not relvar-rooted, so
        // even with a SQLite backend (pushdown on) it stays in-process.
        use coddl_plan::BackendKind;
        use coddl_types::RelvarTable;
        let plan = Plan {
            program_name: "rel_lit".to_string(),
            database_name: None,
            cd_relvars: RelvarTable::default(),
            cddb_relvars: RelvarTable::default(),
            backend_kind: BackendKind::Sqlite,
            resolved: vec![],
            db_file_default: None,
        };
        let src = "program rel_lit;\n\
                   oper main {}\n\
                   [\n\
                       write_relation { rel: Relation { {a: 1}, {a: 2} } where a = 2 };\n\
                   ];\n";
        let m = lower_ok_with_plan(src, &plan);
        let main = m.functions.iter().find(|f| f.name == "main").expect("main");
        let insts = &main.blocks[0].insts;
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Where { .. })),
            "materialized where should stay in-process in:\n{m}"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Query { .. })),
            "a relation literal must not be pushed to SQL"
        );
        assert!(m.plans.is_empty(), "no plans for a materialized where");
    }

    #[test]
    fn hello_world_main_body_is_init_const_call_shutdown() {
        let m = lower_ok(HELLO_WORLD);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(main.blocks.len(), 1);
        let block = &main.blocks[0];
        assert_eq!(block.insts.len(), 4);

        // 1. init wrapper call.
        match &block.insts[0] {
            Inst::Call {
                callee,
                args,
                return_type: ProcType::Integer,
                ..
            } => {
                assert_eq!(callee, "coddl_runtime_init");
                assert!(args.is_empty());
            }
            other => panic!("expected init Call, got {other:?}"),
        }

        // 2. string constant.
        match &block.insts[1] {
            Inst::Const {
                value: Const::Text(bytes),
                ty: ProcType::Text,
                ..
            } => assert_eq!(bytes, b"Hello, world!"),
            other => panic!("expected Const Text, got {other:?}"),
        }

        // 3. write_line call.
        match &block.insts[2] {
            Inst::Call {
                dst: None,
                callee,
                args,
                return_type: ProcType::Unit,
            } => {
                assert_eq!(callee, "coddl_write_line");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected write_line Call, got {other:?}"),
        }

        // 4. shutdown wrapper call.
        match &block.insts[3] {
            Inst::Call {
                callee,
                args,
                return_type: ProcType::Integer,
                ..
            } => {
                assert_eq!(callee, "coddl_runtime_shutdown");
                assert!(args.is_empty());
            }
            other => panic!("expected shutdown Call, got {other:?}"),
        }
        assert!(matches!(block.terminator, Terminator::Return(None)));
    }

    #[test]
    fn let_binding_threaded_into_write_line_call() {
        // The let-bound `msg` becomes the same ValueId the call site
        // uses for `message`. The const + call instructions are the
        // body's payload (sandwiched between init/shutdown).
        let src = "oper main {} [ let msg = \"hi\"; write_line{message: msg}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        // Find the Const Text and the call to coddl_write_line.
        let const_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::Const {
                    dst,
                    value: Const::Text(bytes),
                    ..
                } if bytes == b"hi" => Some(*dst),
                _ => None,
            })
            .expect("Const Text \"hi\" present");
        let call_arg = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if callee == "coddl_write_line" => {
                    Some(args.first().copied())
                }
                _ => None,
            })
            .expect("write_line call present")
            .expect("write_line call has an arg");
        assert_eq!(
            call_arg, const_dst,
            "let binding should thread its ValueId to the call site"
        );
    }

    #[test]
    fn transaction_tail_expression_value_flows_out() {
        // `transaction [ "ok" ]` as the RHS of a let: the let's bound
        // ValueId is the same one Const Text "ok" produces. The
        // following write_line call references it.
        let src = "oper main {} [ let ok = transaction [ \"ok\" ]; write_line{message: ok}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;

        let ok_const_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::Const {
                    dst,
                    value: Const::Text(bytes),
                    ..
                } if bytes == b"ok" => Some(*dst),
                _ => None,
            })
            .expect("Const Text \"ok\" present");
        let call_arg = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if callee == "coddl_write_line" => {
                    Some(args.first().copied())
                }
                _ => None,
            })
            .expect("write_line call present")
            .expect("write_line call has an arg");
        assert_eq!(call_arg, ok_const_dst);
    }

    #[test]
    fn hello_world_extern_has_no_blocks() {
        let m = lower_ok(HELLO_WORLD);
        let ext = m.functions.iter().find(|f| f.name == "write_line").unwrap();
        assert!(ext.is_extern());
        assert_eq!(ext.linkage_name, "coddl_write_line");
        assert_eq!(ext.params.len(), 1);
        assert_eq!(ext.params[0].0, "message");
        assert_eq!(ext.params[0].1, ProcType::Text);
        assert_eq!(ext.return_type, ProcType::Unit);
    }

    #[test]
    fn read_line_lowers_to_text_returning_call() {
        // `read_line` registers a `coddl_read_line` extern returning Text,
        // and its call's `dst` (the Text result) is the ValueId the let
        // binding threads into the following `write_line`. The len-out
        // ABI param is a codegen concern, so ProcIR keeps the clean
        // `(prompt: Text) -> Text` signature.
        let src = "oper main {} [ let n = read_line{prompt: \"p\"}; write_line{message: n}; ];";
        let m = lower_ok(src);

        let ext = m.functions.iter().find(|f| f.name == "read_line").unwrap();
        assert!(ext.is_extern());
        assert_eq!(ext.linkage_name, "coddl_read_line");
        assert_eq!(ext.params.len(), 1);
        assert_eq!(ext.params[0].0, "prompt");
        assert_eq!(ext.params[0].1, ProcType::Text);
        assert_eq!(ext.return_type, ProcType::Text);

        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        let read_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call {
                    callee,
                    dst,
                    return_type,
                    ..
                } if callee == "coddl_read_line" => {
                    assert_eq!(*return_type, ProcType::Text);
                    Some(dst.expect("read_line call binds a dst"))
                }
                _ => None,
            })
            .expect("read_line call present");
        let write_arg = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if callee == "coddl_write_line" => {
                    Some(args.first().copied())
                }
                _ => None,
            })
            .expect("write_line call present")
            .expect("write_line call has an arg");
        assert_eq!(
            write_arg, read_dst,
            "read_line's Text result should thread into the write_line arg"
        );
    }

    // ── scalar Text refcounting (owned/borrowed provenance) ──────────

    /// All `Inst::Release` source ValueIds in `main`'s entry block.
    fn main_releases(m: &Module) -> Vec<ValueId> {
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        main.blocks[0]
            .insts
            .iter()
            .filter_map(|i| match i {
                Inst::Release { src } => Some(*src),
                _ => None,
            })
            .collect()
    }

    /// The dst of the first `ScalarOp::Concat` in `main`'s entry block.
    fn first_concat_dst(m: &Module) -> ValueId {
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::ScalarOp {
                    dst,
                    op: ScalarOp::Concat,
                    ..
                } => Some(*dst),
                _ => None,
            })
            .expect("a Concat present")
    }

    #[test]
    fn owned_text_local_released_at_scope_exit() {
        // `let m = "a" || "b";` — the concat result is owned and bound to a
        // local; it must be released exactly once at function epilogue. The
        // immortal-literal operands are borrowed and never released.
        let src = "oper main {} [ let m = \"a\" || \"b\"; write_line { message: m }; ];";
        let module = lower_ok(src);
        let concat = first_concat_dst(&module);
        let releases = main_releases(&module);
        assert_eq!(
            releases.iter().filter(|v| **v == concat).count(),
            1,
            "owned concat local released exactly once; releases={releases:?}"
        );
        assert_eq!(releases.len(), 1, "only the concat local; releases={releases:?}");
    }

    #[test]
    fn chained_concat_releases_intermediate() {
        // `"a" || "b" || "c"` = `("a"||"b") || "c"`: the inner concat is an
        // owned temporary consumed by the outer concat — it must be released,
        // as must the final result bound to `m`. Two releases total.
        let src = "oper main {} [ let m = \"a\" || \"b\" || \"c\"; write_line { message: m }; ];";
        let module = lower_ok(src);
        let inner = first_concat_dst(&module);
        let releases = main_releases(&module);
        assert!(
            releases.contains(&inner),
            "inner concat temporary released; releases={releases:?}"
        );
        assert_eq!(releases.len(), 2, "inner temp + outer local; releases={releases:?}");
    }

    #[test]
    fn inline_concat_argument_released_after_call() {
        // `write_line { message: "a" || name }` with `name` a borrowed param:
        // the inline concat is an owned temporary, released right after the
        // call consumes it. The borrowed `name` is never released.
        let src = "oper greet { name: Text } [ write_line { message: \"a\" || name }; ];";
        let module = lower_ok(src);
        let greet = module.functions.iter().find(|f| f.name == "greet").unwrap();
        let concat = greet
            .blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::ScalarOp { dst, op: ScalarOp::Concat, .. } => Some(*dst),
                _ => None,
            })
            .expect("a Concat present");
        let releases: Vec<ValueId> = greet.blocks[0]
            .insts
            .iter()
            .filter_map(|i| match i {
                Inst::Release { src } => Some(*src),
                _ => None,
            })
            .collect();
        assert_eq!(releases, vec![concat], "only the inline concat temp; got {releases:?}");
    }

    #[test]
    fn borrowed_text_field_is_not_released() {
        // `t.message` is a `TupleField` — a borrowed `(ptr,len)` into the
        // tuple, NOT an owned heap Text. It must never be released (that would
        // be a premature free). The literal is borrowed too. Zero releases.
        let src = "oper main {} [ let t = { message: \"hi\" }; write_line { message: t.message }; ];";
        let module = lower_ok(src);
        assert!(
            main_releases(&module).is_empty(),
            "borrowed Text field must not be released"
        );
    }

    #[test]
    fn string_literal_decodes_escapes() {
        let src = "oper main {} [ write_line{message: \"a\\nb\"}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let block = &main.blocks[0];
        // `main`'s body is wrapped by init/shutdown; the user's
        // string constant lives between them.
        let text_const = block
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::Const {
                    value: Const::Text(bytes),
                    ..
                } => Some(bytes.as_slice()),
                _ => None,
            })
            .expect("expected a Const Text in main");
        assert_eq!(text_const, b"a\nb");
    }

    #[test]
    fn program_name_carried_through() {
        let src = "program greet; oper main {} [];";
        let m = lower_ok(src);
        assert_eq!(m.program_name, "greet");
    }

    #[test]
    fn multiple_opers_lower_independently() {
        let src = "oper foo {} []; oper bar {} [];";
        let m = lower_ok(src);
        let defined: Vec<&str> = m
            .functions
            .iter()
            .filter(|f| !f.is_extern())
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(defined, vec!["foo", "bar"]);
    }

    #[test]
    fn typecheck_error_returns_none_module() {
        let src = "oper main {} [ write_lne{message: \"x\"}; ];";
        let out = lower(src, FileId(0));
        assert!(out.module.is_none());
        assert!(out.diagnostics.iter().any(|d| d.code == "T0001"));
    }

    #[test]
    fn call_to_write_line_uses_coddl_prefix_in_linkage_name() {
        // Among `main`'s calls, exactly one routes a Text argument —
        // that's the write_line site. Its callee must be the linkage
        // name, not the surface name.
        let m = lower_ok(HELLO_WORLD);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let call = main
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if !args.is_empty() => Some(callee.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(call, "coddl_write_line");
    }

    #[test]
    fn non_main_oper_with_return_type_emits_typed_return() {
        // Integer chosen because both backends already handle scalar
        // returns. Text returns are blocked on the C ABI return-pair
        // codegen — a future phase.
        let src = "oper compute {} -> Integer [ 42 ]; oper main {} [];";
        let m = lower_ok(src);
        let compute = m.functions.iter().find(|f| f.name == "compute").unwrap();
        assert_eq!(compute.return_type, ProcType::Integer);
        assert_eq!(compute.blocks.len(), 1);
        match &compute.blocks[0].terminator {
            Terminator::Return(Some(_)) => {}
            other => panic!("expected Return(Some(_)), got {other:?}"),
        }
    }

    #[test]
    fn oper_without_explicit_return_stays_unit() {
        let src = "oper noop {} [];";
        let m = lower_ok(src);
        let noop = m.functions.iter().find(|f| f.name == "noop").unwrap();
        assert_eq!(noop.return_type, ProcType::Unit);
        assert!(matches!(
            noop.blocks[0].terminator,
            Terminator::Return(None)
        ));
    }

    #[test]
    fn integer_literal_decodes_decimal_and_hex() {
        assert_eq!(parse_integer_literal("42"), 42);
        assert_eq!(parse_integer_literal("0x2a"), 42);
        assert_eq!(parse_integer_literal("0b101010"), 42);
        assert_eq!(parse_integer_literal("0o52"), 42);
        assert_eq!(parse_integer_literal("1_000"), 1000);
    }

    // ── Tuple lit + field access (Phase 18) ──────────────────────────

    #[test]
    fn tuple_let_field_access_threaded_through_call() {
        // The tuple's `message` field becomes a TupleField project;
        // its value flows into write_line's `message` argument.
        let src = "oper main {} [ \
                   let t = {message: \"hi\"}; \
                   write_line{message: t.message}; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;

        // Find the TupleLit instruction.
        let tuple_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleLit { dst, fields, .. } => {
                    assert_eq!(fields.len(), 1);
                    assert_eq!(fields[0].0, "message");
                    Some(*dst)
                }
                _ => None,
            })
            .expect("TupleLit emitted");

        // Find the TupleField projecting `message` from the tuple.
        let field_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleField {
                    dst,
                    src,
                    field_name,
                    field_type,
                } if *src == tuple_dst && field_name == "message" => {
                    assert_eq!(*field_type, ProcType::Text);
                    Some(*dst)
                }
                _ => None,
            })
            .expect("TupleField emitted");

        // Find the write_line call and confirm it consumes the field's
        // ValueId as its argument.
        let arg = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if callee == "coddl_write_line" => {
                    Some(args.first().copied())
                }
                _ => None,
            })
            .expect("write_line call present")
            .expect("write_line call has an arg");
        assert_eq!(arg, field_dst);
    }

    #[test]
    fn empty_tuple_lit_emits_inst_with_empty_heading() {
        // `{}` in expression position must lower to an Inst::TupleLit
        // with no fields and an empty heading.
        let src = "oper main {} [ let _u = {}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let inst = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleLit { fields, heading, .. } => Some((fields.clone(), heading.clone())),
                _ => None,
            })
            .expect("TupleLit emitted");
        assert!(inst.0.is_empty());
        assert!(inst.1.is_empty());
    }

    #[test]
    fn tuple_fields_emitted_in_canonical_order() {
        // Source order is reversed alphabetically; the emitted
        // Inst::TupleLit's fields list and heading must both be sorted.
        let src = "oper main {} [ let _t = {z: 1, a: 2}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let (fields, heading) = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleLit { fields, heading, .. } => Some((fields.clone(), heading.clone())),
                _ => None,
            })
            .expect("TupleLit emitted");
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "z"]);
        let attr_names: Vec<&str> = heading.attrs().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(attr_names, vec!["a", "z"]);
    }

    // ── Where + predicate synthesis (Phase 20) ───────────────────────

    #[test]
    fn where_synthesizes_predicate_function_and_emits_inst_where() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let s = r where a = 2; \
                   ];";
        let m = lower_ok(src);
        // Predicate helper function exists.
        let pred = m
            .functions
            .iter()
            .find(|f| f.name.starts_with("__coddl_where_"))
            .expect("synthesized predicate function");
        assert_eq!(pred.params.len(), 1);
        assert_eq!(pred.return_type, ProcType::Boolean);
        // Predicate body contains an AttrLoad + ScalarOp.
        let pred_insts = &pred.blocks[0].insts;
        assert!(
            pred_insts.iter().any(|i| matches!(i, Inst::AttrLoad { .. })),
            "predicate should AttrLoad heading attrs"
        );
        assert!(
            pred_insts.iter().any(|i| matches!(i, Inst::ScalarOp { .. })),
            "predicate body should ScalarOp"
        );
        // Main contains an Inst::Where.
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let main_insts = &main.blocks[0].insts;
        assert!(
            main_insts.iter().any(|i| matches!(i, Inst::Where { .. })),
            "main should emit Inst::Where"
        );
    }

    #[test]
    fn capture_in_where_predicate_diagnoses_t0022() {
        // `n` is bound in the enclosing scope, not in the heading. The
        // lowerer must emit T0022 because Phase 20 deferred captures.
        let src = "oper main {} [ \
                   let n = 5; \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let s = r where a = n; \
                   ];";
        let out = lower(src, FileId(0));
        assert!(
            out.diagnostics.iter().any(|d| d.code == "T0022"),
            "expected T0022, got {:?}",
            out.diagnostics
        );
        assert!(out.module.is_none(), "module should be None on T0022");
    }

    // ── extract (Phase 21) ───────────────────────────────────────────

    #[test]
    fn extract_on_temporary_defers_source_release_to_epilogue() {
        // The `r where a = 2` is a fresh temporary. Extract copies its cells
        // into the tuple as *borrowed* `(ptr,len)` values, so releasing the
        // source immediately would free `Text` cells the borrowed fields still
        // point at (the relation drop walker frees them). The release is
        // therefore deferred to the function epilogue — present, but NOT the
        // instruction right after Extract.
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let t = extract (r where a = 2); \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        let extract_idx = insts
            .iter()
            .position(|i| matches!(i, Inst::Extract { .. }))
            .expect("Inst::Extract emitted");
        let extract_src = match &insts[extract_idx] {
            Inst::Extract { src, .. } => *src,
            _ => unreachable!(),
        };
        // The release exists (the temporary is freed at function exit) ...
        let release_idx = insts
            .iter()
            .position(|i| matches!(i, Inst::Release { src } if *src == extract_src))
            .expect("extract source released at the function epilogue");
        // ... but it is deferred, not emitted immediately after Extract.
        assert!(
            release_idx > extract_idx + 1,
            "source release should be deferred to the epilogue; extract@{extract_idx} release@{release_idx}"
        );
    }

    #[test]
    fn extract_on_let_bound_does_not_release_source() {
        // When the source is a let-bound name, the scope owns the
        // refcount — extract should NOT emit an immediate Release
        // (that would double-free at scope exit).
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let t = extract r; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        let extract_idx = insts
            .iter()
            .position(|i| matches!(i, Inst::Extract { .. }))
            .unwrap();
        let extract_src = match &insts[extract_idx] {
            Inst::Extract { src, .. } => *src,
            _ => unreachable!(),
        };
        // No Release of `extract_src` should appear between the
        // Extract and the function epilogue's scope-exit release.
        // There IS exactly one Release of `r`'s ValueId — but it
        // sits at the function epilogue (after the second
        // RelationLit's let-stmt finishes), not immediately after
        // the Extract. Verify there's exactly one Release for the
        // source.
        let count = insts
            .iter()
            .filter(|i| matches!(i, Inst::Release { src } if *src == extract_src))
            .count();
        assert_eq!(count, 1, "let-bound source should see exactly one Release (at scope exit)");
    }
}

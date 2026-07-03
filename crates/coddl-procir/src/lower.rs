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
    AssignStmt, AstNode, BinaryExpr, BinaryOp, Block, BoolLit, CallExpr, DeleteStmt, DoWhileStmt,
    Expr, ExprStmt,
    InsertStmt,
    ExtendExpr, FieldAccess, ForStmt, IfExpr, IndexExpr, Item,
    LetStmt, Literal, LoadStmt, NameRef, NamedArg, OperDecl, ProgramDecl, ProjectExpr, RelationLit,
    RenameExpr,
    ReplaceExpr, Root, SequenceLit, Stmt, TcloseExpr, TransactionExpr, TruncateStmt, TupleLit,
    TypeRef, UnaryExpr, UnaryOp, UnwrapExpr, UpdateStmt, VarStmt, WhileStmt, WrapExpr,
};
use coddl_syntax::{parse_format_template, SyntaxKind, TemplateChunk};
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
    // `cardinality { self } -> Integer`. Polymorphic over `Relation H` and
    // `Sequence T`; both store their element/tuple count in the RC header's
    // `length` slot, so one runtime read (`coddl_rc_length`) serves either.
    // The `self` param type is an ABI sentinel only — the generic `lower_call`
    // path ignores it and lowers whatever argument is supplied; both
    // `Relation(_)` and `Sequence(_)` are a single pointer, so this declares
    // the extern as `i64 coddl_rc_length(ptr)`.
    BuiltinExtern {
        surface: "cardinality",
        linkage: "coddl_rc_length",
        params: &[("self", ProcType::Relation(HeadingId(0)))],
        return_type: ProcType::Integer,
    },
];

struct BuiltinExtern {
    surface: &'static str,
    linkage: &'static str,
    params: &'static [(&'static str, ProcType)],
    return_type: ProcType,
}

/// A user-defined operator's lowered signature, collected in a pre-pass over
/// the program's `oper` declarations so a call site (`lower_call`) can resolve
/// a non-builtin callee regardless of declaration order. Unlike a
/// `BuiltinExtern`, a user op lowers to an in-module `Function` whose linkage
/// name is its surface name, so there's no separate `linkage` field and no
/// `ensure_extern` — the backend finds it among `Module::functions`.
struct UserOpSig {
    params: Vec<(String, ProcType)>,
    return_type: ProcType,
}

// Internal `to_text` conversions — not user-callable surfaces (so absent from
// `BUILTIN_EXTERNS`), but declared like any Text-returning extern: the length
// crosses back through the synthesized `len_out` (the `returns_fat_pointer`
// path), same as `read_line`. `lower_to_text` references these directly.
const INT_TO_TEXT_EXTERN: BuiltinExtern = BuiltinExtern {
    surface: "int_to_text",
    linkage: "coddl_int_to_text",
    params: &[("n", ProcType::Integer)],
    return_type: ProcType::Text,
};
const BOOL_TO_TEXT_EXTERN: BuiltinExtern = BuiltinExtern {
    surface: "bool_to_text",
    linkage: "coddl_bool_to_text",
    params: &[("b", ProcType::Boolean)],
    return_type: ProcType::Text,
};

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
    /// Instructions accumulated into the block currently being built
    /// (`current_block`). Moved into a `BasicBlock` by `finish_block`.
    insts: Vec<Inst>,
    /// Finished basic blocks of the current function, in *finish order*.
    /// A block is finished (pushed here) only once its terminator is known,
    /// which guarantees the entry block lands first and every predecessor
    /// precedes the block it branches to — the ordering both backends rely
    /// on. A straight-line body produces a single entry block, as before.
    blocks: Vec<BasicBlock>,
    /// Id of the block currently being built.
    current_block: BlockId,
    /// Parameters of `current_block` (SSA values bound on block entry).
    /// Non-empty only for an `if` merge block that carries the join value.
    current_block_params: Vec<(ValueId, ProcType)>,
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
    /// `let x = f"…"` bindings: the format template's token text, keyed by name.
    /// A `FormatText` is compile-time-only and never a runtime value, so — like
    /// `relexpr_aliases` — the binding emits nothing and carries no `ValueId`;
    /// `lower_format_call` folds the stored template in at each use site.
    /// Parallel to `locals` (same push/pop discipline).
    format_templates: Vec<HashMap<String, String>>,
    /// Names declared by an uninitialized `var x;` that are not yet bound —
    /// the *pending* set. The first assignment binds the name into `locals`
    /// (at this same layer) and removes it here. Parallel to `locals` (same
    /// push/pop discipline). Definite assignment (T0079) guarantees a pending
    /// var is never read, so it never needs a value until its first write.
    pending_uninit: Vec<HashSet<String>>,
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
    /// ValueIds of the current function's parameters (`ValueId(0..N)`, matching
    /// the backends' parameter seeding). Parameters are *borrowed* — the caller
    /// owns the argument — so they are bound as body locals (resolving a body
    /// reference like `self`) but excluded from the scope-exit release.
    param_value_ids: Vec<ValueId>,
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
    /// Signatures of every user-defined `oper`, collected in a pre-pass over
    /// `lower_root` before any body is lowered, so a call to an operator
    /// defined later in the file still resolves. `lower_call` consults this
    /// after the builtin special-cases; a hit emits an in-module `Inst::Call`.
    user_opers: HashMap<String, UserOpSig>,
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
            blocks: Vec::new(),
            current_block: BlockId(0),
            current_block_params: Vec::new(),
            locals: vec![HashMap::new()],
            relexpr_aliases: vec![HashMap::new()],
            format_templates: vec![HashMap::new()],
            pending_uninit: vec![HashSet::new()],
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
            param_value_ids: Vec::new(),
            tuple_cell_text_temps: HashMap::new(),
            deferred_relation_releases: Vec::new(),
            user_opers: HashMap::new(),
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
        self.format_templates.push(HashMap::new());
        self.pending_uninit.push(HashSet::new());
    }

    fn pop_local_scope(&mut self) {
        self.locals.pop();
        self.relexpr_aliases.pop();
        self.format_templates.pop();
        self.pending_uninit.pop();
    }

    fn bind_local(&mut self, name: String, v: ValueId, ty: ProcType) {
        self.locals
            .last_mut()
            .expect("scope stack empty")
            .insert(name, (v, ty));
    }

    /// Point an existing local binding at a new SSA value — the effect of a
    /// `var` reassignment (`x := …`). Updates the innermost scope layer that
    /// holds `name` in place, so the binding keeps its declaration layer while
    /// its current value changes. No-op if unbound (the typechecker guarantees
    /// a `var` binding reached here).
    fn rebind_local(&mut self, name: &str, v: ValueId, ty: ProcType) {
        for layer in self.locals.iter_mut().rev() {
            if layer.contains_key(name) {
                layer.insert(name.to_string(), (v, ty));
                return;
            }
        }
    }

    /// Whether `name` is a declared-but-unbound `var x;` awaiting its first
    /// assignment (see `pending_uninit`).
    fn is_pending(&self, name: &str) -> bool {
        self.pending_uninit.iter().any(|l| l.contains(name))
    }

    /// Bind a pending uninitialized `var` on its **first** assignment: install
    /// it in `locals` at its *declaration* layer (so it survives an `if` arm
    /// that first-assigns it) and clear it from `pending_uninit`. No-op if the
    /// name wasn't pending.
    fn bind_pending_first_assign(&mut self, name: &str, v: ValueId, ty: ProcType) {
        for layer in (0..self.pending_uninit.len()).rev() {
            if self.pending_uninit[layer].remove(name) {
                self.locals[layer].insert(name.to_string(), (v, ty));
                return;
            }
        }
    }

    /// Names that appear as the target of an `x := …` reassignment anywhere in
    /// `block`, including nested loops / `if` arms / transactions. Over-collects
    /// relvar targets and inner-scope names; callers intersect with the outer
    /// `locals` to keep only carried outer `var`s.
    fn collect_reassigned_names(&self, block: &Block, out: &mut Vec<String>) {
        for node in block.syntax().descendants() {
            if node.kind() == SyntaxKind::ASSIGN_STMT {
                if let Some(assign) = AssignStmt::cast(node) {
                    if let Some(Expr::NameRef(t)) = assign.target() {
                        if let Some(id) = t.ident() {
                            out.push(id.text().to_string());
                        }
                    }
                }
            }
        }
    }

    /// The outer `var`s reassigned within `body`, captured as
    /// `(name, pre-join value, type)` for block-parameter threading across a
    /// control-flow join (a loop back-edge or an `if` merge). Value-typed vars
    /// thread with no refcount work and are unconditionally correct. A carried
    /// var of a heap-managed / `Text` type is **not yet lowered** — refcount-
    /// correct heap mutation across a join is future work — so each emits T0076
    /// at `span` and is excluded (the error makes the IR unsafe to run, so the
    /// body's own straight-line rebind never executes).
    fn carried_value_vars(
        &mut self,
        bodies: &[Option<&Block>],
        span: Span,
    ) -> Vec<(String, ValueId, ProcType)> {
        let mut names = Vec::new();
        for body in bodies {
            if let Some(b) = body {
                self.collect_reassigned_names(b, &mut names);
            }
        }
        let mut seen = HashSet::new();
        let mut carried = Vec::new();
        for name in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some((v, ty)) = self.lookup_local(&name) else {
                continue; // a relvar or inner-scope name — not an outer var
            };
            if Self::is_heap_managed(&ty) || matches!(ty, ProcType::Text) {
                self.diagnostics.push(Diagnostic::error(
                    span,
                    "T0076",
                    format!(
                        "reassigning the heap-typed variable `{name}` across a loop or \
                         `if` is not yet lowered"
                    ),
                ));
                continue;
            }
            carried.push((name, v, ty));
        }
        carried
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

    /// Record a `let x = f"…"` binding — its template token text, folded in by
    /// `lower_format_call`. Emits no instruction and binds no `ValueId`.
    fn bind_format_template(&mut self, name: String, text: String) {
        self.format_templates
            .last_mut()
            .expect("scope stack empty")
            .insert(name, text);
    }

    /// Resolve a name to its `let`-bound format-template text, innermost first.
    fn lookup_format_template(&self, name: &str) -> Option<&str> {
        self.format_templates
            .iter()
            .rev()
            .find_map(|l| l.get(name).map(String::as_str))
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

    /// Open a function/helper body: mint the entry block and make it current.
    /// Returns the entry `BlockId` (always `BlockId(0)` after a reset — the
    /// backends seed function parameters into `blocks.first()`).
    fn begin_function_body(&mut self) -> BlockId {
        let entry = self.fresh_block();
        self.current_block = entry;
        self.current_block_params.clear();
        entry
    }

    /// Seal `current_block` with `terminator`, moving its accumulated
    /// instructions and parameters into a finished `BasicBlock`.
    fn finish_block(&mut self, terminator: Terminator) {
        let block = BasicBlock {
            id: self.current_block,
            params: std::mem::take(&mut self.current_block_params),
            insts: std::mem::take(&mut self.insts),
            terminator,
        };
        self.blocks.push(block);
    }

    /// Make `id` the current block, with the given block parameters. `insts`
    /// is already empty (the previous block was sealed by `finish_block`).
    fn start_block(&mut self, id: BlockId, params: Vec<(ValueId, ProcType)>) {
        self.current_block = id;
        self.current_block_params = params;
    }

    fn reset_function_state(&mut self) {
        self.next_value = 0;
        self.next_block = 0;
        self.insts.clear();
        self.blocks.clear();
        self.current_block = BlockId(0);
        self.current_block_params.clear();
        self.locals.clear();
        self.locals.push(HashMap::new());
        self.relexpr_aliases.clear();
        self.relexpr_aliases.push(HashMap::new());
        self.format_templates.clear();
        self.format_templates.push(HashMap::new());
        self.pending_uninit.clear();
        self.pending_uninit.push(HashSet::new());
        self.value_types.clear();
        self.owned_texts.clear();
        self.param_value_ids.clear();
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
        matches!(ty, ProcType::Relation(_) | ProcType::Sequence(_))
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
            // Parameters are borrowed (the caller owns them) — never release.
            if self.param_value_ids.contains(&v) {
                continue;
            }
            if self.needs_scope_release(v, &ty) {
                self.insts.push(Inst::Release { src: v });
            }
        }
    }

    /// If `value` is a heap-managed binding in the current top scope, retain it
    /// so it survives the scope-exit [`Self::release_top_scope_heap_locals`]
    /// that follows — it escapes as the scope's result (return-of-local, e.g.
    /// `[ let s = a || b; s ]`). The retain balances that release, leaving the
    /// consumer a live `rc=1` reference; the consumer's own release frees it. A
    /// fresh tail temporary (not bound to a local) isn't in the release set, so
    /// it needs no retain; a value-type or borrowed (`param_value_ids`) result
    /// no-ops via [`Self::needs_scope_release`]. Shared by the function
    /// epilogue, `if`-arm exit, and transaction exit.
    fn retain_if_escaping_local(&mut self, value: ValueId) {
        let ty = self.value_type(value);
        let in_scope = self
            .locals
            .last()
            .map(|s| s.values().any(|(v, _)| *v == value))
            .unwrap_or(false);
        if in_scope && self.needs_scope_release(value, &ty) {
            self.insts.push(Inst::Retain { src: value });
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
    /// Register a runtime extern (a block-less `Function` the backends emit as a
    /// `declare`) under its linkage name, deduped by name. `params`/`return_type`
    /// give the ABI signature used for the declaration; the call site supplies
    /// the actual operands.
    fn ensure_runtime_extern(
        &mut self,
        linkage: &'static str,
        params: Vec<(String, ProcType)>,
        return_type: ProcType,
    ) {
        if !self.seen_externs.insert(linkage) {
            return;
        }
        self.functions.push(Function {
            name: linkage.to_string(),
            linkage_name: linkage.to_string(),
            params,
            return_type,
            blocks: Vec::new(),
        });
    }

    // ── Walks ────────────────────────────────────────────────────────

    fn lower_root(&mut self, root: &Root) -> Module {
        // Pre-pass: record every user-defined operator's signature so a call
        // to an operator declared later in the file resolves during body
        // lowering. A user op may share a name with a built-in (open
        // overloading), but built-ins live in a separate table, so this insert
        // never clobbers one; the typechecker caps it at one user overload per
        // name (T0060), so the by-name `user_opers` map stays unambiguous.
        for item in root.items() {
            if let Item::OperDecl(o) = item {
                let (name, params, return_type) = Self::oper_signature(&o);
                self.user_opers.insert(
                    name,
                    UserOpSig {
                        params,
                        return_type,
                    },
                );
            }
        }
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
        // The prologue goes right after `coddl_runtime_init` and the releases
        // right before `coddl_runtime_shutdown`. Both calls sit in a *single*
        // block each, but not necessarily the same one: `init` is always in the
        // entry block (emitted before the body), while `shutdown` rides the
        // block current at body end — the entry for a straight-line `main`, or
        // a merge block if `main`'s body ends in control flow (an `if`). Locate
        // each by the block that holds its call so both cases splice correctly.
        if !prologue.is_empty() {
            if let Some(block) = main.blocks.iter_mut().find(|b| {
                b.insts.iter().any(
                    |i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_runtime_init"),
                )
            }) {
                let at = block
                    .insts
                    .iter()
                    .position(|i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_runtime_init"))
                    .map(|p| p + 1)
                    .unwrap_or(0);
                block.insts.splice(at..at, prologue);
            }
        }
        if !releases.is_empty() {
            if let Some(block) = main.blocks.iter_mut().find(|b| {
                b.insts.iter().any(
                    |i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_runtime_shutdown"),
                )
            }) {
                if let Some(sp) = block.insts.iter().position(
                    |i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_runtime_shutdown"),
                ) {
                    block.insts.splice(sp..sp, releases);
                }
            }
        }
    }

    fn lower_program_decl(&mut self, decl: &ProgramDecl) {
        if let Some(name_tok) = decl.name() {
            self.program_name = name_tok.text().to_string();
        }
    }

    /// Extract a user `oper`'s lowered signature — `(name, params, return
    /// type)` — from its declaration. Shared by the `lower_root` pre-pass
    /// (which records it in `user_opers` for call resolution) and
    /// `lower_oper_decl` (which builds the `Function`), so the call-site view
    /// of an operator never drifts from the emitted function. An absent param
    /// type or return clause maps to `Unit`, mirroring the typechecker's
    /// defaults for a clean program (the only input lowering sees).
    fn oper_signature(decl: &OperDecl) -> (String, Vec<(String, ProcType)>, ProcType) {
        let name = decl
            .name()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        let mut params: Vec<(String, ProcType)> = Vec::new();
        if let Some(heading) = decl.heading() {
            for param in heading.params() {
                let pname = param
                    .name()
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let pty = param
                    .type_ref()
                    .map(|tr| proc_type_from_type_ref(&tr))
                    .unwrap_or(ProcType::Unit);
                params.push((pname, pty));
            }
        }
        let return_type = decl
            .return_type()
            .map(|tr| proc_type_from_type_ref(&tr))
            .unwrap_or(ProcType::Unit);
        (name, params, return_type)
    }

    fn lower_oper_decl(&mut self, decl: &OperDecl) -> Function {
        self.reset_function_state();

        // Surface name doubles as the linkage name for now. Adding name
        // mangling — for overloading or module-scoped symbols — slots into
        // `oper_signature` once it arrives. The declared return type defaults
        // to Unit; main is treated as Unit at the IR level (the backends
        // special-case `ret i32 0`), and the typechecker rejects a declared
        // non-Unit return on `main` with T0011, so that is safe.
        let (name, params, declared_return) = Self::oper_signature(decl);
        let linkage_name = name.clone();
        let is_main = name == "main";

        // Bind parameters as body locals so a body reference (e.g. `self`)
        // resolves to the parameter value rather than the `Unit` placeholder.
        // Parameters occupy `ValueId(0..N)` in declared order — matching the
        // backends' parameter seeding — so mint them directly and advance
        // `next_value` past them before any `fresh_value()`. They are
        // *borrowed* (the caller owns the argument); `param_value_ids` excludes
        // them from the scope-exit release.
        self.param_value_ids = (0..params.len() as u32).map(ValueId).collect();
        for (i, (pname, pty)) in params.iter().enumerate() {
            let vid = ValueId(i as u32);
            self.bind_local(pname.clone(), vid, pty.clone());
            self.record_type(vid, pty.clone());
        }
        self.next_value = params.len() as u32;

        self.begin_function_body();

        // The compiled program's startup must call the runtime before
        // touching any other extern (docs/runtime.md). Today the
        // stubs are no-ops, but wiring it now means future runtime
        // work — DB connection pool, prepared-statement cache,
        // arena setup — slots in without a codegen change.
        if is_main {
            self.ensure_runtime_extern("coddl_runtime_init", Vec::new(), ProcType::Integer);
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

        // When the body's tail value is actually returned (a non-`main`,
        // non-Unit oper — the same condition that builds `Return(Some(v))`
        // below), and that value is a heap-managed local, it escapes the
        // function: retain it so the scope release below leaves the caller a
        // live reference (return-of-local, `[ let s = a || b; s ]`). `main` and
        // Unit-returning opers discard the tail value, so retaining it would
        // leak — hence the guard.
        let returns_value = !is_main && !matches!(declared_return, ProcType::Unit);
        if returns_value {
            if let Some(v) = body_value {
                self.retain_if_escaping_local(v);
            }
        }

        // Release every heap-typed function-scope local before either the
        // runtime-shutdown call (main) or the terminator (others). The escaping
        // return value, if any, was retained just above so it survives.
        self.release_top_scope_heap_locals();
        // Then the deferred `extract`-source relations — released last, after
        // every borrowed field they fed has been consumed.
        self.drain_deferred_relation_releases();

        if is_main {
            // Per-relvar slot releases are inserted before this shutdown
            // call by `finalize_main_prologue`, mirroring the slot inits it
            // emits. The runtime's own `coddl_runtime_shutdown` also frees
            // any slot still alive (defense in depth).
            self.ensure_runtime_extern("coddl_runtime_shutdown", Vec::new(), ProcType::Integer);
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

        self.finish_block(terminator);

        Function {
            name,
            linkage_name,
            params,
            return_type: declared_return,
            blocks: std::mem::take(&mut self.blocks),
        }
    }

    /// Lower a block. Returns the block's value — the tail
    /// expression's `ValueId` if there is one, otherwise a fresh
    /// placeholder representing Unit (never consumed downstream).
    fn lower_block(&mut self, block: &Block) -> ValueId {
        for stmt in block.statements() {
            match stmt {
                Stmt::Let(l) => self.lower_let_stmt(&l),
                Stmt::Var(v) => self.lower_var_stmt(&v),
                Stmt::Assign(a) => self.lower_assign_stmt(&a),
                Stmt::Truncate(t) => self.lower_truncate_stmt(&t),
                Stmt::Delete(d) => self.lower_delete_stmt(&d),
                Stmt::Insert(i) => self.lower_insert_stmt(&i),
                Stmt::Update(u) => self.lower_update_stmt(&u),
                Stmt::ExprStmt(e) => self.lower_expr_stmt(&e),
                Stmt::For(f) => self.lower_for_stmt(&f),
                Stmt::While(w) => self.lower_while_stmt(&w),
                Stmt::DoWhile(d) => self.lower_do_while_stmt(&d),
                Stmt::Load(l) => self.lower_load_stmt(&l),
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

        // A local `var` reassignment: rebind the name to the new value. The
        // typechecker guarantees only a mutable `var` reaches here as a local —
        // never a relvar (not in `locals`), a `let`/parameter (T0074), or a loop
        // counter (T0072). Its value flows across control-flow joins via the
        // block-parameter threading in `lower_counted_loop` / `lower_if_expr`.
        if let Some((old_v, old_ty)) = self.lookup_local(&name) {
            let rhs_is_existing_name = matches!(value_expr, Expr::NameRef(_));
            let new_v = self.lower_expr(&value_expr);
            let new_ty = self.value_type(new_v);
            // Drop the previous value if this binding owned it (owned Text /
            // relation / sequence); a borrowed or value-typed old value no-ops.
            if self.needs_scope_release(old_v, &old_ty) {
                self.insts.push(Inst::Release { src: old_v });
            }
            // Take ownership of the new value the way a `let`/`var` decl does:
            // an aliasing RHS is retained (an aliased `Text` marked owned) so
            // the binding's scope-exit release stays balanced.
            if rhs_is_existing_name
                && (Self::is_heap_managed(&new_ty) || matches!(new_ty, ProcType::Text))
            {
                self.insts.push(Inst::Retain { src: new_v });
                if matches!(new_ty, ProcType::Text) {
                    self.mark_text_owned(new_v);
                }
            }
            self.rebind_local(&name, new_v, new_ty);
            return;
        }

        // First assignment to a declared-but-unbound `var x;`. Definite
        // assignment (T0079) guarantees this precedes any read, so binding it
        // here (at its declaration layer) is enough — no placeholder needed.
        if self.is_pending(&name) {
            let rhs_is_existing_name = matches!(value_expr, Expr::NameRef(_));
            let new_v = self.lower_expr(&value_expr);
            let new_ty = self.value_type(new_v);
            if rhs_is_existing_name
                && (Self::is_heap_managed(&new_ty) || matches!(new_ty, ProcType::Text))
            {
                self.insts.push(Inst::Retain { src: new_v });
                if matches!(new_ty, ProcType::Text) {
                    self.mark_text_owned(new_v);
                }
            }
            self.bind_pending_first_assign(&name, new_v, new_ty);
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
        // `let x = f"…"` binds a reusable format template. A `FormatText` is
        // compile-time-only and never a runtime value, so — like a deferred
        // `RelExpr` alias — record the template text and emit nothing;
        // `lower_format_call` folds it in at each `format { template: x, … }`
        // use. The typechecker guarantees only a direct `f"…"` literal reaches
        // here (never a runtime `Text`).
        if let Expr::Literal(lit) = &value_expr {
            if lit.token().map(|t| t.kind()) == Some(SyntaxKind::FORMAT_STRING_LIT) {
                if let (Some(tok), Some(name_tok)) = (lit.token(), stmt.name()) {
                    self.bind_format_template(
                        name_tok.text().to_string(),
                        tok.text().to_string(),
                    );
                }
                return;
            }
        }
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
        let id = self.lower_binding_rhs(&value_expr, stmt.type_ref());
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

    /// Lower `var <name> := <expr>;` — a mutable binding. Unlike `let`, a `var`
    /// always **materializes** its RHS to a concrete value bound via
    /// `bind_local` (never a deferred `RelExpr` alias — a value that can be
    /// reassigned can't be a deferred query). Otherwise identical to `let`:
    /// an aliasing RHS is retained so the refcount reflects the new owner.
    fn lower_var_stmt(&mut self, stmt: &VarStmt) {
        let value_expr = match stmt.value() {
            // Uninitialized `var x;` — record it as pending in this scope layer;
            // the first assignment binds it (nothing is emitted until then).
            // Definite assignment (T0079) guarantees it isn't read before that.
            None => {
                if let Some(name_tok) = stmt.name() {
                    self.pending_uninit
                        .last_mut()
                        .expect("scope stack empty")
                        .insert(name_tok.text().to_string());
                }
                return;
            }
            Some(v) => v,
        };
        let rhs_is_existing_name = matches!(value_expr, Expr::NameRef(_));
        let id = self.lower_binding_rhs(&value_expr, stmt.type_ref());
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
            Expr::SequenceLit(s) => self.lower_sequence_lit(s),
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
            Expr::Index(i) => self.lower_index_expr(i),
            Expr::If(i) => self.lower_if_expr(i),
            Expr::NameRef(n) => self.lower_name_ref(n),
        }
    }

    /// Lower a `for` loop — dispatch on the header form. The counted form runs
    /// a compiler-managed induction variable; the element form (`for name in
    /// seq`) desugars onto the same counted loop. Both build one CFG via
    /// [`Self::lower_counted_loop`].
    fn lower_for_stmt(&mut self, stmt: &ForStmt) {
        if stmt.is_for_in() {
            self.lower_for_in_stmt(stmt);
        } else {
            self.lower_for_counted_stmt(stmt);
        }
    }

    /// Lower a counted `for i := lo to hi do [ … ]` loop. Both bounds are
    /// evaluated **once**, in the entry block (they dominate the header); the
    /// counter is bound as the loop-scoped body local `i`.
    fn lower_for_counted_stmt(&mut self, stmt: &ForStmt) {
        let lo = stmt
            .lower_bound()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.fresh_value());
        let hi = stmt
            .upper_bound()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.fresh_value());
        let name = stmt.var_name().map(|t| t.text().to_string());
        let span = self.node_span(stmt.syntax());
        self.lower_counted_loop(lo, hi, stmt.body(), span, |this, counter| {
            if let Some(n) = &name {
                this.bind_local(n.clone(), counter, ProcType::Integer);
            }
        });
    }

    /// Lower an element `for name in seq do [ … ]` loop by desugaring onto the
    /// counted loop: `for __i := 0 to cardinality(seq) - 1 do [ let name =
    /// seq[__i]; <body> ]`. The sequence is lowered **once** and held in an
    /// outer scope — owned like a `let __seq = <seq>`, with an alias-retain when
    /// it borrows an existing binding — so it is released exactly once after the
    /// loop; the element is read per iteration via the same bounds-checked index
    /// path as `s[i]` (owned-copy retain for a heap `Text`). Empty-safe: an
    /// empty sequence gives `0 to -1`, i.e. zero iterations.
    fn lower_for_in_stmt(&mut self, stmt: &ForStmt) {
        // Outer scope holds the sequence for the loop's whole duration.
        self.push_local_scope();

        // Lower the iterable once and own it like `let __seq = <iterable>`:
        // retain when it aliases an existing binding so the scope-exit release
        // balances (mirrors `lower_let_stmt`). `Sequence` is heap-managed, so
        // the outer scope's release frees it after the loop.
        let iterable = stmt.iterable();
        let iterable_is_name = matches!(iterable, Some(Expr::NameRef(_)));
        let seq = iterable
            .as_ref()
            .map(|e| self.lower_expr(e))
            .unwrap_or_else(|| self.fresh_value());
        let seq_ty = self.value_type(seq);
        if iterable_is_name && Self::is_heap_managed(&seq_ty) {
            self.insts.push(Inst::Retain { src: seq });
        }
        self.bind_local("__seq".to_string(), seq, seq_ty.clone());
        let elem_ty = match &seq_ty {
            ProcType::Sequence(elem) => (**elem).clone(),
            // A non-Sequence operand is T0073 at typecheck; this is recovery.
            _ => ProcType::Unit,
        };

        // hi = cardinality(seq) - 1, lo = 0. Reuse the `cardinality` builtin's
        // extern registration (deduped by surface name) so its `coddl_rc_length`
        // symbol is declared once even if the program also calls `cardinality`.
        let card_ext = self
            .lookup_extern("cardinality")
            .expect("the `cardinality` builtin extern exists");
        let card_linkage = card_ext.linkage.to_string();
        let card_ret = card_ext.return_type.clone();
        self.ensure_extern(card_ext);
        let card = self.fresh_value();
        self.record_type(card, card_ret.clone());
        self.insts.push(Inst::Call {
            dst: Some(card),
            callee: card_linkage,
            args: vec![seq],
            return_type: card_ret,
        });
        let one = self.fresh_value();
        self.record_type(one, ProcType::Integer);
        self.insts.push(Inst::Const {
            dst: one,
            value: Const::Integer(1),
            ty: ProcType::Integer,
        });
        let hi = self.fresh_value();
        self.record_type(hi, ProcType::Integer);
        self.insts.push(Inst::ScalarOp {
            dst: hi,
            op: ScalarOp::Sub,
            operand_type: ProcType::Integer,
            lhs: card,
            rhs: one,
        });
        let lo = self.fresh_value();
        self.record_type(lo, ProcType::Integer);
        self.insts.push(Inst::Const {
            dst: lo,
            value: Const::Integer(0),
            ty: ProcType::Integer,
        });

        // Counted loop over [0, card-1], binding `name = seq[__i]` per iteration.
        let name = stmt.var_name().map(|t| t.text().to_string());
        let span = self.node_span(stmt.syntax());
        self.lower_counted_loop(lo, hi, stmt.body(), span, |this, i| {
            let elem = this.lower_seq_index_value(seq, i, elem_ty.clone());
            if let Some(n) = &name {
                this.bind_local(n.clone(), elem, elem_ty.clone());
            }
        });

        // Release the sequence once, after the loop (current == exit block).
        self.release_top_scope_heap_locals();
        self.pop_local_scope();
    }

    /// Build the counted-loop CFG (the project's first **cyclic** CFG), leaving
    /// the exit block current. Shape, with `entry` the block current on entry:
    ///
    /// ```text
    /// entry:  Br header [lo]
    /// header (param %i): %c = %i <= hi; CondBr %c -> body, exit
    /// body:   <prologue>; <body>; %inc = %i + 1; Br header [%inc]   ← back-edge
    /// exit:   …                                                     ← current
    /// ```
    ///
    /// `hi` (already lowered) dominates the header. The counter rides the header
    /// block's parameter — the block-param join `if` uses for its merge, now fed
    /// from two predecessors (`lo` on the entry edge, `%inc` on the back-edge).
    /// `to` is inclusive (`<=`), so `lo > hi` runs zero times. `body_prologue`
    /// runs inside the body's local scope, after the counter is available and
    /// before the user block — the counted form binds the counter there, the
    /// element form binds `name = seq[counter]`.
    fn lower_counted_loop(
        &mut self,
        lo: ValueId,
        hi: ValueId,
        user_body: Option<Block>,
        loop_span: Span,
        mut body_prologue: impl FnMut(&mut Self, ValueId),
    ) {
        // Outer value-typed `var`s reassigned in the body ride extra header
        // block parameters — the SSA join of the entry edge (pre-loop values)
        // and the back-edge (end-of-iteration values). Captured before any
        // block is sealed, while `locals` still holds their pre-loop values.
        let carried = self.carried_value_vars(&[user_body.as_ref()], loop_span);

        let header = self.fresh_block();
        let body = self.fresh_block();
        let exit = self.fresh_block();

        // Entry edge: seed the counter with the lower bound, then each carried
        // var with its pre-loop value.
        let mut entry_args = vec![lo];
        entry_args.extend(carried.iter().map(|(_, v, _)| *v));
        self.finish_block(Terminator::Br {
            target: header,
            args: entry_args,
        });

        // Header: the counter plus one parameter per carried var (the SSA
        // joins). Rebind each carried var to its parameter so the condition and
        // body read the joined value. Test `i <= hi` and branch to body/exit.
        let counter = self.fresh_value();
        self.record_type(counter, ProcType::Integer);
        let mut header_params = vec![(counter, ProcType::Integer)];
        let mut carried_params: Vec<(String, ValueId, ProcType)> =
            Vec::with_capacity(carried.len());
        for (name, _, ty) in &carried {
            let p = self.fresh_value();
            self.record_type(p, ty.clone());
            header_params.push((p, ty.clone()));
            carried_params.push((name.clone(), p, ty.clone()));
        }
        self.start_block(header, header_params);
        for (name, p, ty) in &carried_params {
            self.rebind_local(name, *p, ty.clone());
        }
        let cmp = self.fresh_value();
        self.record_type(cmp, ProcType::Boolean);
        self.insts.push(Inst::ScalarOp {
            dst: cmp,
            op: ScalarOp::LtEq,
            operand_type: ProcType::Integer,
            lhs: counter,
            rhs: hi,
        });
        self.finish_block(Terminator::CondBr {
            cond: cmp,
            then_block: body,
            else_block: exit,
        });

        // Body: run the prologue (binds the loop variable), lower the body
        // (reassignments rebind carried vars in `locals`), release any heap
        // locals it allocated (once per iteration), then compute `i + 1` and
        // branch back to the header carrying each var's current value.
        self.start_block(body, Vec::new());
        self.push_local_scope();
        body_prologue(self, counter);
        if let Some(b) = user_body {
            self.lower_block(&b);
        }
        self.release_top_scope_heap_locals();
        self.pop_local_scope();
        // The increment and back-edge are emitted into whatever block the body
        // ended in — an inner `if` may have introduced blocks — which
        // `current_block` / `self.insts` already track.
        let one = self.fresh_value();
        self.record_type(one, ProcType::Integer);
        self.insts.push(Inst::Const {
            dst: one,
            value: Const::Integer(1),
            ty: ProcType::Integer,
        });
        let inc = self.fresh_value();
        self.record_type(inc, ProcType::Integer);
        self.insts.push(Inst::ScalarOp {
            dst: inc,
            op: ScalarOp::Add,
            operand_type: ProcType::Integer,
            lhs: counter,
            rhs: one,
        });
        let mut back_args = vec![inc];
        back_args.extend(self.carried_current_values(&carried));
        self.finish_block(Terminator::Br {
            target: header,
            args: back_args,
        });

        // Exit: the header parameters dominate the sole exit edge, so each
        // carried var's final value is its header parameter.
        self.start_block(exit, Vec::new());
        for (name, p, ty) in &carried_params {
            self.rebind_local(name, *p, ty.clone());
        }
    }

    /// Lower a `while <cond> do [ … ]` pre-test loop — the counted-loop CFG
    /// minus the counter/increment, with the user condition re-evaluated in the
    /// header each iteration:
    ///
    /// ```text
    /// entry:            Br header [carried…]
    /// header(params P): rebind carried→P; <eval cond>; CondBr cond -> body, exit
    /// body:             <user body>; Br header [carried_now…]   ← back-edge
    /// exit:             rebind carried→P
    /// ```
    ///
    /// Outer value-typed `var`s reassigned in the body ride the header block
    /// params — the SSA join of the entry edge (pre-loop values) and the
    /// back-edge (end-of-iteration values); a heap-typed carry is deferred
    /// (T0076, in `carried_value_vars`). The condition reads the joined values
    /// via the rebound header params. Empty-safe: the condition is tested first.
    fn lower_while_stmt(&mut self, stmt: &WhileStmt) {
        let span = self.node_span(stmt.syntax());
        let body = stmt.body();
        let carried = self.carried_value_vars(&[body.as_ref()], span);

        let header = self.fresh_block();
        let body_block = self.fresh_block();
        let exit = self.fresh_block();

        // Entry edge: seed each carried var with its pre-loop value.
        let entry_args: Vec<ValueId> = carried.iter().map(|(_, v, _)| *v).collect();
        self.finish_block(Terminator::Br {
            target: header,
            args: entry_args,
        });

        // Header: one param per carried var; rebind, evaluate the condition,
        // branch to body or exit.
        let mut header_params: Vec<(ValueId, ProcType)> = Vec::with_capacity(carried.len());
        let mut carried_params: Vec<(String, ValueId, ProcType)> =
            Vec::with_capacity(carried.len());
        for (name, _, ty) in &carried {
            let p = self.fresh_value();
            self.record_type(p, ty.clone());
            header_params.push((p, ty.clone()));
            carried_params.push((name.clone(), p, ty.clone()));
        }
        self.start_block(header, header_params);
        for (name, p, ty) in &carried_params {
            self.rebind_local(name, *p, ty.clone());
        }
        let cond = stmt
            .condition()
            .map(|c| self.lower_expr(&c))
            .unwrap_or_else(|| self.fresh_value());
        self.finish_block(Terminator::CondBr {
            cond,
            then_block: body_block,
            else_block: exit,
        });

        // Body: lower it in its own scope; the back-edge carries current values.
        // Emitted into whatever block the body ended in (an inner `if` may have
        // introduced blocks).
        self.start_block(body_block, Vec::new());
        self.push_local_scope();
        if let Some(b) = body {
            self.lower_block(&b);
        }
        self.release_top_scope_heap_locals();
        self.pop_local_scope();
        let back_args = self.carried_current_values(&carried);
        self.finish_block(Terminator::Br {
            target: header,
            args: back_args,
        });

        // Exit: the header params dominate the sole exit edge, so each carried
        // var's final value (condition false on entry to this iteration) is its
        // header parameter.
        self.start_block(exit, Vec::new());
        for (name, p, ty) in &carried_params {
            self.rebind_local(name, *p, ty.clone());
        }
    }

    /// Lower a `do [ … ] while <cond>` post-test loop. The body is the loop
    /// header — entered from the pre-loop entry *and* the back-edge — so it
    /// carries the block params; the condition is evaluated after the body and a
    /// tiny `latch` supplies the back-edge args (a `CondBr` carries none):
    ///
    /// ```text
    /// entry:          Br body [carried…]
    /// body(params P): rebind carried→P; <user body>; <eval cond>;
    ///                 CondBr cond -> latch, exit
    /// latch:          Br body [carried_now…]        ← back-edge
    /// exit:           rebind carried→carried_now
    /// ```
    ///
    /// The body runs once before the first test (the post-test caveat). The
    /// condition reads each carried var's end-of-iteration value (post-body).
    /// The block holding the `CondBr` dominates both `latch` and `exit`, so
    /// `exit` binds each carried var to its final `carried_now` value.
    fn lower_do_while_stmt(&mut self, stmt: &DoWhileStmt) {
        let span = self.node_span(stmt.syntax());
        let body = stmt.body();
        let carried = self.carried_value_vars(&[body.as_ref()], span);

        let body_block = self.fresh_block();
        let latch = self.fresh_block();
        let exit = self.fresh_block();

        // Entry edge: seed the body params with each carried var's pre-loop value.
        let entry_args: Vec<ValueId> = carried.iter().map(|(_, v, _)| *v).collect();
        self.finish_block(Terminator::Br {
            target: body_block,
            args: entry_args,
        });

        // Body header: one param per carried var; rebind, then run the body.
        let mut body_params: Vec<(ValueId, ProcType)> = Vec::with_capacity(carried.len());
        let mut carried_params: Vec<(String, ValueId, ProcType)> =
            Vec::with_capacity(carried.len());
        for (name, _, ty) in &carried {
            let p = self.fresh_value();
            self.record_type(p, ty.clone());
            body_params.push((p, ty.clone()));
            carried_params.push((name.clone(), p, ty.clone()));
        }
        self.start_block(body_block, body_params);
        for (name, p, ty) in &carried_params {
            self.rebind_local(name, *p, ty.clone());
        }
        self.push_local_scope();
        if let Some(b) = body {
            self.lower_block(&b);
        }
        self.release_top_scope_heap_locals();
        self.pop_local_scope();
        // Post-test: capture the end-of-iteration values, evaluate the condition
        // on them (emitted into whatever block the body ended in), then loop back
        // (latch) or leave (exit).
        let carried_now = self.carried_current_values(&carried);
        let cond = stmt
            .condition()
            .map(|c| self.lower_expr(&c))
            .unwrap_or_else(|| self.fresh_value());
        self.finish_block(Terminator::CondBr {
            cond,
            then_block: latch,
            else_block: exit,
        });

        // Latch: the back-edge, feeding the current values into the body params.
        self.start_block(latch, Vec::new());
        self.finish_block(Terminator::Br {
            target: body_block,
            args: carried_now.clone(),
        });

        // Exit: the CondBr block dominates this edge, so each carried var's final
        // value is its end-of-iteration value.
        self.start_block(exit, Vec::new());
        for ((name, _, ty), v) in carried.iter().zip(carried_now.iter()) {
            self.rebind_local(name, *v, ty.clone());
        }
    }

    /// Lower `if <cond> then [ … ] else [ … ]`. The condition computes in the
    /// current block, which is sealed with a `CondBr`; each arm is its own
    /// block that branches to a shared merge block, passing its value as the
    /// merge block's parameter (the SSA join). Without `else`, the false edge
    /// jumps straight to the merge and the value is Unit (the statement form);
    /// a Unit result carries no merge parameter in either form.
    ///
    /// Blocks are sealed in the order entry → then → else → merge, so the
    /// entry stays first and every predecessor precedes the block it branches
    /// to — the ordering the backends rely on. Nesting composes: an `if` in an
    /// arm seals its own blocks between that arm's `start` and `Br`.
    fn lower_if_expr(&mut self, ife: &IfExpr) -> ValueId {
        let cond_expr = ife.condition().expect("typechecked if has a condition");
        let span = self.node_span(ife.syntax());
        let then_body = ife.then_body();
        let else_body = ife.else_body();

        // Value-typed outer vars reassigned in either arm are carried through
        // the merge as block parameters — the SSA join of the two edges. The
        // not-taken edge forwards the pre-`if` value (a missing `else` gets an
        // explicit skip block for that, since `CondBr` carries no args).
        // Captured before the arms rebind `locals`.
        let carried = self.carried_value_vars(&[then_body.as_ref(), else_body.as_ref()], span);

        let cond = self.lower_expr(&cond_expr);

        // No `else` and nothing mutated: the false edge goes straight to the
        // merge (statement form, Unit value) — no skip block, three blocks.
        if else_body.is_none() && carried.is_empty() {
            let then_id = self.fresh_block();
            let merge_id = self.fresh_block();
            self.finish_block(Terminator::CondBr {
                cond,
                then_block: then_id,
                else_block: merge_id,
            });
            self.start_block(then_id, Vec::new());
            self.lower_if_arm(then_body);
            self.finish_block(Terminator::Br {
                target: merge_id,
                args: Vec::new(),
            });
            let result = self.fresh_value();
            self.record_type(result, ProcType::Unit);
            self.start_block(merge_id, Vec::new());
            return result;
        }

        // Introduced vars: a `var x;` (pending, unbound) assigned in *both*
        // arms — definitely assigned after the `if`, so it also rides the merge
        // as a block parameter, but with no pre-`if` value (each arm
        // first-assigns/rebinds it). Detected by name here; its type is known
        // only after the then-arm infers it (heap ⇒ T0076, like heap-carried).
        let introduced_names = self.introduced_var_names(then_body.as_ref(), else_body.as_ref());

        let then_id = self.fresh_block();
        let else_id = self.fresh_block();
        let merge_id = self.fresh_block();
        self.finish_block(Terminator::CondBr {
            cond,
            then_block: then_id,
            else_block: else_id,
        });

        // Then arm.
        self.start_block(then_id, Vec::new());
        let then_val = self.lower_if_arm(then_body);
        let ty = self.value_type(then_val);
        let is_unit = matches!(ty, ProcType::Unit);
        // Introduced vars are now bound (first-assigned in the then-arm); keep
        // the value-typed ones (name, type, then-value). A heap type crossing
        // the merge is deferred (T0076).
        let mut introduced: Vec<(String, ProcType, ValueId)> = Vec::new();
        for name in &introduced_names {
            if let Some((v, vty)) = self.lookup_local(name) {
                if Self::is_heap_managed(&vty) || matches!(vty, ProcType::Text) {
                    self.diagnostics.push(Diagnostic::error(
                        span,
                        "T0076",
                        format!(
                            "initializing the heap-typed variable `{name}` on both branches \
                             of an `if` is not yet lowered"
                        ),
                    ));
                } else {
                    introduced.push((name.clone(), vty, v));
                }
            }
        }
        let mut then_args = if is_unit { Vec::new() } else { vec![then_val] };
        then_args.extend(self.carried_current_values(&carried));
        then_args.extend(introduced.iter().map(|(_, _, v)| *v));
        self.finish_block(Terminator::Br {
            target: merge_id,
            args: then_args,
        });

        // Arms are alternatives, not sequential: restore each carried var to
        // its pre-`if` value before the else/skip edge. Introduced vars are
        // left bound (to the then-value); the else-arm rebinds them.
        for (name, prev, cty) in &carried {
            self.rebind_local(name, *prev, cty.clone());
        }

        // Else arm — the real `else` block, or an empty skip block forwarding
        // the pre-`if` values when there is no `else`.
        self.start_block(else_id, Vec::new());
        let else_val = if else_body.is_some() {
            self.lower_if_arm(else_body)
        } else {
            // No `else`: the value is Unit (typecheck guarantees a Unit then).
            let v = self.fresh_value();
            self.record_type(v, ProcType::Unit);
            v
        };
        let mut else_args = if is_unit { Vec::new() } else { vec![else_val] };
        else_args.extend(self.carried_current_values(&carried));
        else_args.extend(self.introduced_current_values(&introduced));
        self.finish_block(Terminator::Br {
            target: merge_id,
            args: else_args,
        });

        // Merge: the join value (unless Unit) plus one parameter per carried
        // then introduced var. A `Text` join value is owned downstream (safe:
        // releasing an immortal literal arm is a no-op; an owned-temp arm is
        // freed).
        let result = self.fresh_value();
        self.record_type(result, ty.clone());
        let mut params = Vec::new();
        if !is_unit {
            if matches!(ty, ProcType::Text) {
                self.mark_text_owned(result);
            }
            params.push((result, ty));
        }
        let mut carried_params: Vec<(String, ValueId, ProcType)> = Vec::with_capacity(carried.len());
        for (name, _, cty) in &carried {
            let p = self.fresh_value();
            self.record_type(p, cty.clone());
            params.push((p, cty.clone()));
            carried_params.push((name.clone(), p, cty.clone()));
        }
        let mut introduced_params: Vec<(String, ValueId, ProcType)> =
            Vec::with_capacity(introduced.len());
        for (name, ity, _) in &introduced {
            let p = self.fresh_value();
            self.record_type(p, ity.clone());
            params.push((p, ity.clone()));
            introduced_params.push((name.clone(), p, ity.clone()));
        }
        self.start_block(merge_id, params);
        for (name, p, cty) in carried_params.iter().chain(introduced_params.iter()) {
            self.rebind_local(name, *p, cty.clone());
        }
        result
    }

    /// Names assigned in *both* arms of an `if` that are currently pending
    /// (`var x;` unbound) — the vars an `if` *introduces* (definitely assigned
    /// on both paths). Only when both arms exist; a missing `else` can't make a
    /// var definitely assigned.
    fn introduced_var_names(
        &self,
        then_b: Option<&Block>,
        else_b: Option<&Block>,
    ) -> Vec<String> {
        let (Some(t), Some(e)) = (then_b, else_b) else {
            return Vec::new();
        };
        let mut then_names = Vec::new();
        self.collect_reassigned_names(t, &mut then_names);
        let mut else_names = Vec::new();
        self.collect_reassigned_names(e, &mut else_names);
        let else_set: HashSet<&String> = else_names.iter().collect();
        let mut seen = HashSet::new();
        then_names
            .iter()
            .filter(|n| else_set.contains(*n) && self.is_pending(n) && seen.insert((*n).clone()))
            .cloned()
            .collect()
    }

    /// The current SSA value of each introduced var (read from `locals` after
    /// an arm rebinds it) — the arguments that arm passes to the merge.
    fn introduced_current_values(&self, introduced: &[(String, ProcType, ValueId)]) -> Vec<ValueId> {
        introduced
            .iter()
            .map(|(name, _, _)| {
                self.lookup_local(name)
                    .map(|(v, _)| v)
                    .expect("introduced var is assigned on both arms")
            })
            .collect()
    }

    /// The current SSA value of each carried var (read from `locals`) — the
    /// arguments an arm passes on its edge to a merge/back-edge block.
    fn carried_current_values(&self, carried: &[(String, ValueId, ProcType)]) -> Vec<ValueId> {
        carried
            .iter()
            .map(|(name, _, _)| {
                self.lookup_local(name)
                    .map(|(v, _)| v)
                    .expect("carried var stays bound through the arm")
            })
            .collect()
    }

    /// Lower one `if` arm block in its own local scope, releasing arm-local
    /// heap bindings at the arm's exit (before it branches to the merge). The
    /// arm's tail value flows out as the join value and is a temporary, so it
    /// is not among the released bindings. An absent block (parse recovery)
    /// yields a fresh Unit value.
    fn lower_if_arm(&mut self, block: Option<Block>) -> ValueId {
        match block {
            Some(b) => {
                self.push_local_scope();
                let val = self.lower_block(&b);
                // The arm's tail value always flows out to the merge block, so
                // retain it if it's a heap-managed local before releasing the
                // arm scope (return-of-local from an arm).
                self.retain_if_escaping_local(val);
                self.release_top_scope_heap_locals();
                self.pop_local_scope();
                val
            }
            None => {
                let v = self.fresh_value();
                self.record_type(v, ProcType::Unit);
                v
            }
        }
    }

    /// Lower `s[i]` — postfix sequence indexing (0-based). Delegates the
    /// bounds-checked read to [`Self::lower_seq_index_value`], which the
    /// `for … in` desugar shares.
    fn lower_index_expr(&mut self, ie: &IndexExpr) -> ValueId {
        let seq_expr = ie
            .sequence()
            .expect("typechecked index has a sequence operand");
        let seq = self.lower_expr(&seq_expr);
        let elem_proc = match self.value_type(seq) {
            ProcType::Sequence(elem) => *elem,
            other => unreachable!("index on non-sequence `{other}` survived typecheck"),
        };
        let idx_expr = ie.index().expect("typechecked index has an index operand");
        let idx = self.lower_expr(&idx_expr);
        self.lower_seq_index_value(seq, idx, elem_proc)
    }

    /// Read `seq[idx]` — the bounds-checked, 0-based element. The runtime
    /// `coddl_seq_index` bounds-checks `idx` against the sequence's length
    /// (aborting on out-of-bounds) and returns the element *record* pointer
    /// `payload + idx * record_size`; an `AttrLoad` at offset 0 then reads the
    /// synthetic `value` cell. A heap `Text` element is retained into an owned
    /// copy so it outlives the sequence's release (it may be returned or
    /// consumed past the container's lifetime); a value-type element passes
    /// through as-is. Shared by the postfix `s[i]` operator and the `for … in`
    /// desugar.
    fn lower_seq_index_value(
        &mut self,
        seq: ValueId,
        idx: ValueId,
        elem_type: ProcType,
    ) -> ValueId {
        self.ensure_runtime_extern(
            "coddl_seq_index",
            vec![
                ("seq".to_string(), ProcType::Pointer),
                ("index".to_string(), ProcType::Integer),
            ],
            ProcType::Pointer,
        );
        let rec = self.fresh_value();
        self.record_type(rec, ProcType::Pointer);
        self.insts.push(Inst::Call {
            dst: Some(rec),
            callee: "coddl_seq_index".to_string(),
            args: vec![seq, idx],
            return_type: ProcType::Pointer,
        });

        // A tuple element (a `Sequence Tuple H` from `load`): explode the record
        // into per-attribute cells and bundle them into a compile-time
        // `ValueRepr::Tuple`, exactly like `Inst::Extract`. The cells are
        // *borrows* into the sequence — which outlives every use (`for…in`
        // retains the sequence for the loop; a `names[i]` var lives for its
        // scope) — so unlike the scalar path below we do NOT retain them, which
        // also avoids leaking a multi-attribute tuple's unread `Text` fields.
        if let ProcType::Tuple(heading) = &elem_type {
            let layout = crate::layout::record_layout(heading);
            let mut fields: Vec<(String, ValueId)> = Vec::with_capacity(layout.attrs.len());
            for attr in &layout.attrs {
                let attr_ty = heading
                    .lookup(&attr.name)
                    .map(proc_type_from_type)
                    .expect("record_layout attribute is in the heading");
                let cell = self.fresh_value();
                self.record_type(cell, attr_ty.clone());
                self.insts.push(Inst::AttrLoad {
                    dst: cell,
                    src: rec,
                    offset: attr.offset,
                    attr_type: attr_ty,
                });
                fields.push((attr.name.clone(), cell));
            }
            let dst = self.fresh_value();
            self.record_type(dst, elem_type.clone());
            self.insts.push(Inst::TupleLit {
                dst,
                fields,
                heading: heading.clone(),
            });
            return dst;
        }

        // Read the synthetic single `value` cell (offset 0 of the element
        // record) — `AttrLoad` handles both scalar and `(ptr, len)` Text cells.
        let elem = self.fresh_value();
        self.record_type(elem, elem_type.clone());
        self.insts.push(Inst::AttrLoad {
            dst: elem,
            src: rec,
            offset: 0,
            attr_type: elem_type.clone(),
        });

        // Owned copy: retain a heap `Text` element (the load is a borrow into
        // the sequence's cell). Value-type elements need no retain.
        if matches!(elem_type, ProcType::Text) {
            self.insts.push(Inst::Retain { src: elem });
            self.mark_text_owned(elem);
        }

        elem
    }

    /// Lower `load <target> from <relExpr> [ order [ <sort-item>… ] ];` — the
    /// RM Pro 7 iteration gate. Force the source relation to a runtime pointer,
    /// emit `Inst::Load` (sort into an ordered `Sequence` of tuple records), and
    /// bind the result to the pre-declared `var` target (its first assignment).
    /// The source is copied+retained by `coddl_load_ordered`, so a *temporary*
    /// source is released right after (unlike `extract`, which borrows into it).
    fn lower_load_stmt(&mut self, stmt: &LoadStmt) {
        let source_expr = stmt.source().expect("typechecked load has a source");
        let rel = self.lower_expr(&source_expr);
        let heading_id = match self.value_type(rel) {
            ProcType::Relation(id) => id,
            other => unreachable!("load source non-relation `{other}` survived typecheck"),
        };
        let heading = self.headings[heading_id.0 as usize].clone();

        // Each order key → an index into the canonical (name-sorted) source
        // heading, bit 31 set for a descending key. Empty for no `order` clause.
        let keys: Vec<u32> = stmt
            .sort_items()
            .filter_map(|item| {
                let name = item.attr()?.text().to_string();
                let idx = heading.attrs().iter().position(|(n, _)| *n == name)? as u32;
                Some(idx | (u32::from(item.is_desc()) << 31))
            })
            .collect();

        let seq_ty = ProcType::Sequence(Box::new(ProcType::Tuple(heading)));
        let seq = self.fresh_value();
        self.record_type(seq, seq_ty.clone());
        self.insts.push(Inst::Load {
            dst: seq,
            src: rel,
            heading_id,
            keys,
        });

        // `coddl_load_ordered` fully copies + retains the source's cells, so a
        // temporary source (not bound to a local) can be released now.
        let is_owned = self
            .locals
            .iter()
            .any(|layer| layer.values().any(|(vid, _)| *vid == rel));
        if !is_owned {
            self.insts.push(Inst::Release { src: rel });
        }

        // Bind the deferred-init `var` target (registered pending by
        // `lower_var_stmt`); scope exit releases the heap `Sequence`.
        if let Some(target) = stmt.target() {
            self.bind_pending_first_assign(&target.text().to_string(), seq, seq_ty);
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
        let saved_format_templates =
            std::mem::replace(&mut self.format_templates, vec![HashMap::new()]);
        let saved_pending = std::mem::replace(&mut self.pending_uninit, vec![HashSet::new()]);
        let saved_value_types = std::mem::take(&mut self.value_types);
        // Isolate `owned_texts` like `value_types`: the helper resets `next_value`,
        // so its ValueIds collide with the enclosing function's. Same for the
        // deferred extract-source list (an `extract` in a computed value).
        let saved_owned_texts = std::mem::take(&mut self.owned_texts);
        let saved_deferred = std::mem::take(&mut self.deferred_relation_releases);
        // The helper builds its own blocks; isolate the enclosing function's
        // block-building state (a computed value may contain an `if`).
        let saved_blocks = std::mem::take(&mut self.blocks);
        let saved_current_block = self.current_block;
        let saved_current_block_params = std::mem::take(&mut self.current_block_params);
        self.outer_locals_for_capture = Some(saved_locals.clone());

        // 4. Helper params: `src_record` (ValueId 0), `dst_record` (ValueId 1).
        self.begin_function_body();
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
        self.finish_block(Terminator::Return(None));
        self.functions.push(Function {
            name: helper_name.clone(),
            linkage_name: helper_name.clone(),
            params: vec![
                ("src_record".to_string(), ProcType::Pointer),
                ("dst_record".to_string(), ProcType::Pointer),
            ],
            return_type: ProcType::Unit,
            blocks: std::mem::take(&mut self.blocks),
        });

        // 10. Restore the enclosing function's state.
        self.next_value = saved_next_value;
        self.next_block = saved_next_block;
        self.insts = saved_insts;
        self.blocks = saved_blocks;
        self.current_block = saved_current_block;
        self.current_block_params = saved_current_block_params;
        self.locals = saved_locals;
        self.relexpr_aliases = saved_aliases;
        self.format_templates = saved_format_templates;
        self.pending_uninit = saved_pending;
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
        let saved_format_templates =
            std::mem::replace(&mut self.format_templates, vec![HashMap::new()]);
        let saved_pending = std::mem::replace(&mut self.pending_uninit, vec![HashSet::new()]);
        let saved_value_types = std::mem::take(&mut self.value_types);
        // The helper resets `next_value` to 0, so its ValueIds collide with the
        // enclosing function's; `owned_texts` is keyed by ValueId, so isolate it
        // too (a predicate may concat: `where g = "a" || s`). Same for the
        // deferred extract-source list (an `extract` inside the predicate).
        let saved_owned_texts = std::mem::take(&mut self.owned_texts);
        let saved_deferred = std::mem::take(&mut self.deferred_relation_releases);
        // Isolate the enclosing function's block-building state (a predicate
        // may contain an `if`).
        let saved_blocks = std::mem::take(&mut self.blocks);
        let saved_current_block = self.current_block;
        let saved_current_block_params = std::mem::take(&mut self.current_block_params);
        self.outer_locals_for_capture = Some(saved_locals.clone());

        // 4. Build the predicate body. The function has a single
        //    parameter `record_ptr: Pointer`. Pre-emit `AttrLoad` for
        //    each heading attribute at function entry; bind each in
        //    the predicate scope under its source-level name.
        self.begin_function_body();
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
        self.finish_block(Terminator::Return(Some(pred_value)));
        self.functions.push(Function {
            name: pred_name.clone(),
            linkage_name: pred_name.clone(),
            params: vec![("record_ptr".to_string(), ProcType::Pointer)],
            return_type: ProcType::Boolean,
            blocks: std::mem::take(&mut self.blocks),
        });

        // 7. Restore the enclosing function's state.
        self.next_value = saved_next_value;
        self.next_block = saved_next_block;
        self.insts = saved_insts;
        self.blocks = saved_blocks;
        self.current_block = saved_current_block;
        self.current_block_params = saved_current_block_params;
        self.locals = saved_locals;
        self.relexpr_aliases = saved_aliases;
        self.format_templates = saved_format_templates;
        self.pending_uninit = saved_pending;
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
        // The body's tail value is the transaction's result — if it's a
        // heap-managed local in this scope it escapes, so retain it before the
        // scope release leaves the caller a live reference (a relation or owned
        // `Text` returned from a transaction, `let x = R; x` /
        // `let m = transaction [ let t = "a"||b; t ]`). The escaped ValueId
        // stays in `owned_texts` (function-global), so the outer binding's
        // scope-exit release balances this retain.
        self.retain_if_escaping_local(value);
        self.release_top_scope_heap_locals();
        self.pop_local_scope();
        value
    }

    /// Emit a synthetic `Inst::Call` to a transaction runtime extern.
    /// The dst is allocated and typed `Integer` (`CoddlStatus`) but
    /// never consumed — the no-op runtime always returns Ok in v1.
    fn emit_tx_call(&mut self, linkage: &'static str) {
        self.ensure_runtime_extern(linkage, Vec::new(), ProcType::Integer);
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
    /// Sequences parse and typecheck but are not yet executable — their
    /// runtime representation and iteration land with `load` (milestone
    /// step 6). Emitting an error here marks the lowered IR unsafe, so the
    /// driver reports T0064 and skips codegen rather than producing a
    /// program that can't run. The returned placeholder value is never
    /// used — the IR is discarded once a lowering error is present.
    fn lower_sequence_lit(&mut self, seq: &SequenceLit) -> ValueId {
        let elements: Vec<Expr> = seq.elements().collect();
        if elements.is_empty() {
            // An empty `Sequence []` carries no element to derive the payload
            // layout from here; its execution lands with `load`.
            self.diagnostics.push(Diagnostic::error(
                self.node_span(seq.syntax()),
                "T0064",
                "empty sequence values are not yet executable",
            ));
            let dst = self.fresh_value();
            self.record_type(dst, ProcType::Unit);
            return dst;
        }

        // Lower each element value, in order.
        let elem_values: Vec<ValueId> = elements.iter().map(|e| self.lower_expr(e)).collect();

        // Element type from the first element (typecheck guarantees the rest
        // are assignable to it).
        let elem_proc = self.value_type(elem_values[0]);

        // A `Sequence` is physically a kind-tagged, *unsealed* relation over a
        // synthetic single-attribute heading `{ value: elem }`, so element
        // storage and the drop walker reuse the relation machinery.
        let heading = Heading::new(vec![("value".to_string(), type_from_proc(&elem_proc))]);
        let heading_id = self.intern_heading(&heading);

        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Sequence(Box::new(elem_proc)));

        self.insts.push(Inst::SequenceLit {
            dst,
            elements: elem_values.clone(),
            heading_id,
        });

        // Retain-on-store (in codegen) gives the sequence its own reference to
        // each heap element; balance the producer reference for owned temps
        // (a no-op for string literals and locals), mirroring
        // `lower_relation_lit`.
        for v in elem_values {
            self.release_text_temp(v);
        }

        dst
    }

    /// Lower a binding's RHS. Special-cases an **empty** `Relation {}` so it can
    /// take its heading from a `Relation { H }` annotation (a *headed* empty
    /// relation); with no annotation (or a non-relation one) it stays `relfalse`.
    /// Every other RHS lowers via `lower_expr`. Called by `lower_let_stmt` /
    /// `lower_var_stmt` in place of the bare `lower_expr`.
    fn lower_binding_rhs(&mut self, value_expr: &Expr, type_ref: Option<TypeRef>) -> ValueId {
        if let Expr::RelationLit(r) = value_expr {
            if r.tuples().next().is_none() {
                let heading = type_ref
                    .and_then(|tr| match coddl_types::resolve_type_ref_quiet(&tr) {
                        Type::Relation(h) => Some(h),
                        _ => None,
                    })
                    .unwrap_or_else(Heading::empty);
                return self.lower_empty_relation_lit(heading);
            }
        }
        self.lower_expr(value_expr)
    }

    /// Lower an empty relation literal to a fresh sealed zero-tuple relation of
    /// `heading`: `relfalse` when `heading` is empty, a headed empty relation
    /// otherwise. `alloc(0, 0)` is a valid zero-length relation and the seal of
    /// zero records is a no-op.
    fn lower_empty_relation_lit(&mut self, heading: Heading) -> ValueId {
        let heading_id = self.intern_heading(&heading);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        self.insts.push(Inst::RelationLit {
            dst,
            tuples: Vec::new(),
            heading_id,
        });
        dst
    }

    fn lower_relation_lit(&mut self, rel: &RelationLit) -> ValueId {
        let tuples: Vec<TupleLit> = rel.tuples().collect();
        // `Relation {}` = `relfalse`: the nullary empty relation. Build it with
        // the empty heading. (A *headed* empty relation — the same literal under
        // a `Relation { H }` annotation — is built by `lower_binding_rhs`, which
        // supplies the heading.) The sibling `reltrue` (`Relation { {} }`) takes
        // the general path below with a single empty tuple.
        if tuples.is_empty() {
            return self.lower_empty_relation_lit(Heading::empty());
        }
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
        // Resolve the callee to a method name plus, for the UFCS method-call
        // form `receiver.method { … }`, the lowered receiver value — injected
        // below as the `self` argument (`x.m { … }` ≡ `m { self: x, … }`).
        // A bare `NameRef` callee is the ordinary prefix call.
        let (surface, self_val): (String, Option<ValueId>) = match call.callee() {
            Some(Expr::NameRef(n)) => (
                n.ident()
                    .map(|t| t.text().to_string())
                    .expect("typechecked NameRef call has an ident"),
                None,
            ),
            Some(Expr::FieldAccess(fa)) => {
                let recv = fa.base().expect("typechecked UFCS call has a receiver");
                let v = self.lower_expr(&recv);
                let method = fa
                    .field()
                    .expect("typechecked UFCS call has a method name")
                    .text()
                    .to_string();
                (method, Some(v))
            }
            _ => unreachable!("typechecked call has a NameRef or FieldAccess callee"),
        };

        // Polymorphic-Relation builtins are lowered to specialized
        // ProcIR ops carrying their argument's `HeadingId`. The
        // backends look the descriptor up in `Module::headings` to
        // emit the per-call-site descriptor pointer.
        if surface == "write_relation" {
            return self.lower_write_relation_call(call);
        }
        // String interpolation: both are compile-time / overloaded
        // constructs with no single `coddl_*` symbol, so they are lowered
        // bespoke and kept out of `BUILTIN_EXTERNS`.
        if surface == "format" {
            return self.lower_format_call(call);
        }
        if surface == "to_text" {
            return self.lower_to_text_call(call, self_val);
        }
        // The `write_line { template, args }` overload: fold the template like
        // `format`, then write the resulting `Text`. Only reached for the
        // template form — the `message: Text` form flows through the generic
        // extern path below.
        if surface == "write_line" && call_has_named_arg(call, "template") {
            return self.lower_write_line_format_call(call);
        }
        // A non-builtin callee is a user-defined operator — an in-module
        // function. (Names are unique across builtins ∪ user ops, so this
        // never shadows a builtin.)
        if self.user_opers.contains_key(&surface) {
            return self.lower_user_call(&surface, call, self_val);
        }

        let ext = self
            .lookup_extern(&surface)
            .unwrap_or_else(|| unreachable!("unknown callee `{surface}` survived typecheck"));
        let linkage = ext.linkage.to_string();
        let return_type = ext.return_type.clone();

        // Lower each argument in the order the operator declared its
        // parameters; the typechecker has guaranteed every declared
        // parameter is supplied exactly once. For a UFCS call the `self`
        // parameter is bound to the receiver rather than a brace argument.
        let arg_list = call.args().expect("typechecked call has an arg list");
        let supplied: Vec<NamedArg> = arg_list.args().collect();
        let mut arg_values: Vec<ValueId> = Vec::with_capacity(ext.params.len());
        for (pname, _) in ext.params {
            if *pname == "self" {
                if let Some(v) = self_val {
                    arg_values.push(v);
                    continue;
                }
            }
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

    /// Lower a call to a user-defined operator to an in-module `Inst::Call`
    /// whose callee is the operator's surface name (its linkage name).
    /// Arguments are lowered by matching each declared parameter name, the
    /// same name-driven order the builtin path uses; the typechecker has
    /// guaranteed each declared parameter is supplied exactly once. A Text
    /// result is marked owned so the caller's binding releases it at scope
    /// exit — the callee returned it live (its tail-expression temporary is
    /// not a scope-bound local, so `lower_oper_decl`'s epilogue doesn't free
    /// it). User ops are in-module functions, so there is no `ensure_extern`.
    ///
    /// Note: a user operator's *parameters* are not yet bound as body locals,
    /// and the caller/callee ownership convention for a `Text` *argument* is
    /// unsettled, so owned-Text temps passed as arguments are not released
    /// here. Only nullary user ops are exercised today; arg ownership lands
    /// with parameter binding.
    fn lower_user_call(
        &mut self,
        surface: &str,
        call: &CallExpr,
        self_val: Option<ValueId>,
    ) -> ValueId {
        let (params, return_type) = {
            let sig = self
                .user_opers
                .get(surface)
                .expect("lower_user_call invoked only for a known user op");
            (sig.params.clone(), sig.return_type.clone())
        };

        let arg_list = call.args().expect("typechecked call has an arg list");
        let supplied: Vec<NamedArg> = arg_list.args().collect();
        let mut arg_values: Vec<ValueId> = Vec::with_capacity(params.len());
        for (pname, _) in &params {
            // For a UFCS call the `self` parameter is bound to the receiver
            // (`self_val`) rather than a brace argument.
            let value = if pname.as_str() == "self" && self_val.is_some() {
                self_val.expect("guarded by is_some")
            } else {
                let arg = supplied
                    .iter()
                    .find(|a| {
                        a.name().map(|t| t.text().to_string()).as_deref() == Some(pname.as_str())
                    })
                    .unwrap_or_else(|| unreachable!("missing arg `{pname}` survived typecheck"));
                let value_expr = arg.value().expect("typechecked named arg has a value");
                self.lower_expr(&value_expr)
            };
            arg_values.push(value);
        }

        let returns_text = matches!(return_type, ProcType::Text);
        let dst = if matches!(return_type, ProcType::Unit) {
            None
        } else {
            let v = self.fresh_value();
            self.record_type(v, return_type.clone());
            Some(v)
        };
        self.insts.push(Inst::Call {
            dst,
            callee: surface.to_string(),
            args: arg_values,
            return_type,
        });
        if returns_text {
            if let Some(v) = dst {
                self.mark_text_owned(v);
            }
        }
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

    /// Lower `to_text { self: <scalar> }` to an **owned** heap `Text`. The
    /// overload was already resolved by the typechecker; here we dispatch on
    /// the lowered value's machine type. Returning an owned value means the
    /// result is always safe to bind or concatenate without aliasing the
    /// source (an identity `Text` is retained, not handed back bare).
    fn lower_to_text_call(&mut self, call: &CallExpr, self_val: Option<ValueId>) -> ValueId {
        // UFCS `x.to_text {}` supplies `self` via the receiver; the prefix
        // form `to_text { self: x }` supplies it in the braces.
        let v = if let Some(sv) = self_val {
            sv
        } else {
            let arg_list = call.args().expect("typechecked call has an arg list");
            let self_arg = arg_list
                .args()
                .find(|a| a.name().map(|t| t.text().to_string()).as_deref() == Some("self"))
                .expect("typechecked to_text has a `self` arg");
            let value_expr = self_arg.value().expect("self arg has a value");
            self.lower_expr(&value_expr)
        };
        self.lower_to_text(v)
    }

    /// Convert an already-lowered scalar `v` to a fresh, independently-owned
    /// heap `Text`. Slice B-core: `Text` (a deep copy — *not* an alias, so the
    /// result never shares a refcount with a source local or a `params` cell,
    /// both of which are released elsewhere) and `Character` (`CharToText`).
    /// `Integer`/`Boolean` arrive with their runtime routines. Shared by
    /// `to_text` and `format`.
    fn lower_to_text(&mut self, v: ValueId) -> ValueId {
        let vty = self.value_type(v);
        // A user `to_text { self: T }` overload takes precedence for value
        // types the built-in conversions don't cover (e.g. a `Sequence`).
        // Resolve it by the value's ProcType against the overload's `self`
        // parameter — the same name + heading rule the typechecker used.
        let user_return = self.user_opers.get("to_text").and_then(|sig| {
            sig.params
                .iter()
                .any(|(n, self_ty)| n.as_str() == "self" && *self_ty == vty)
                .then(|| sig.return_type.clone())
        });
        if let Some(return_type) = user_return {
            let returns_text = matches!(return_type, ProcType::Text);
            let dst = self.fresh_value();
            self.record_type(dst, return_type.clone());
            self.insts.push(Inst::Call {
                dst: Some(dst),
                callee: "to_text".to_string(),
                args: vec![v],
                return_type,
            });
            if returns_text {
                self.mark_text_owned(dst);
            }
            return dst;
        }
        match vty {
            ProcType::Text => {
                // Copy via concat with "" — the cheapest way to get a
                // standalone owned `Text`. Aliasing the source instead would
                // either leak (a borrowing call skips releasing a local) or
                // dangle (a `format` placeholder borrows a cell freed after
                // the fold).
                let empty = self.emit_text_const(Vec::new());
                self.concat_text(empty, v)
            }
            ProcType::Character => self.coerce_to_text(v),
            ProcType::Integer => self.call_text_conv(&INT_TO_TEXT_EXTERN, v),
            ProcType::Boolean => self.call_text_conv(&BOOL_TO_TEXT_EXTERN, v),
            other => unreachable!(
                "to_text has no overload for `{other}` (Text, Character, Integer, Boolean)"
            ),
        }
    }

    /// Emit a call to a Text-returning conversion extern (`coddl_int_to_text`
    /// / `coddl_bool_to_text`) over one scalar argument. The result is a fresh
    /// owned heap `Text` (rc=1); the backend supplies the trailing `len_out`
    /// for the fat-pointer return, exactly as for `read_line`.
    fn call_text_conv(&mut self, ext: &'static BuiltinExtern, arg: ValueId) -> ValueId {
        self.ensure_extern(ext);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Text);
        self.insts.push(Inst::Call {
            dst: Some(dst),
            callee: ext.linkage.to_string(),
            args: vec![arg],
            return_type: ProcType::Text,
        });
        self.mark_text_owned(dst);
        dst
    }

    /// Lower the `write_line { template: f"…", args: { … } }` overload: fold
    /// the template and args into a `Text` exactly as `format` does, then hand
    /// that value to the `coddl_write_line` extern (the same symbol the
    /// `message: Text` overload uses). The folded `Text` is an owned temp, so
    /// release it after the call — mirroring the generic extern path's
    /// `text_args` cleanup. Returns a fresh Unit value.
    fn lower_write_line_format_call(&mut self, call: &CallExpr) -> ValueId {
        let msg = self.lower_format_call(call);
        let ext = self
            .lookup_extern("write_line")
            .expect("write_line extern is registered");
        self.ensure_extern(ext);
        self.insts.push(Inst::Call {
            dst: None,
            callee: ext.linkage.to_string(),
            args: vec![msg],
            return_type: ProcType::Unit,
        });
        self.release_text_temp(msg);
        let v = self.fresh_value();
        self.record_type(v, ProcType::Unit);
        v
    }

    /// Lower the `format { template: f"…", args: { … } }` intrinsic to a
    /// `to_text`/`||` concatenation, the desugar string interpolation is
    /// built on. The template is scanned (the typechecker already validated
    /// it) into literal chunks and placeholders; each placeholder reads its
    /// value from the `args` tuple via a borrowed `TupleField`, is
    /// converted with `to_text`, and the pieces are concatenated. `args`
    /// is materialized once, so per-field effects run once and repeated
    /// placeholders are handled correctly.
    fn lower_format_call(&mut self, call: &CallExpr) -> ValueId {
        let arg_list = call.args().expect("typechecked call has an arg list");
        let mut template_text: Option<String> = None;
        let mut args_expr: Option<Expr> = None;
        for arg in arg_list.args() {
            match arg.name().map(|t| t.text().to_string()).as_deref() {
                Some("template") => match arg.value() {
                    // Inline `f"…"` literal.
                    Some(Expr::Literal(lit)) => {
                        template_text = lit.token().map(|t| t.text().to_string());
                    }
                    // A `let x = f"…"` template, reused here — fold in the text
                    // recorded at the binding site.
                    Some(Expr::NameRef(n)) => {
                        if let Some(ident) = n.ident() {
                            template_text =
                                self.lookup_format_template(ident.text()).map(str::to_string);
                        }
                    }
                    _ => {}
                },
                Some("args") => args_expr = arg.value(),
                _ => {}
            }
        }
        let text = template_text.expect("typechecked format has an f\"…\" template");
        let chunks = parse_format_template(&text).unwrap_or_default();

        // Materialize `args` once iff a placeholder needs it.
        let needs_args = chunks
            .iter()
            .any(|c| matches!(c, TemplateChunk::Placeholder { .. }));
        let args_tv = if needs_args {
            args_expr.as_ref().map(|e| self.lower_expr(e))
        } else {
            None
        };
        let args_heading = args_tv.map(|tv| match self.value_type(tv) {
            ProcType::Tuple(h) => h,
            other => unreachable!("format args lowered to non-tuple `{other}`"),
        });

        // Each chunk becomes a `Text` piece: literal const, or a placeholder
        // read (TupleField → to_text). Placeholder pieces are owned; literal
        // pieces are borrowed consts.
        let mut pieces: Vec<ValueId> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            match chunk {
                TemplateChunk::Literal(bytes) => {
                    pieces.push(self.emit_text_const(bytes.clone()));
                }
                TemplateChunk::Placeholder { name, .. } => {
                    let tv = args_tv.expect("placeholder requires materialized args");
                    let heading = args_heading.as_ref().expect("args heading");
                    let field_type = heading
                        .lookup(name)
                        .map(proc_type_from_type)
                        .unwrap_or_else(|| {
                            unreachable!("placeholder `{name}` missing from args heading past typecheck")
                        });
                    let field = self.fresh_value();
                    self.record_type(field, field_type.clone());
                    self.insts.push(Inst::TupleField {
                        dst: field,
                        src: tv,
                        field_name: name.clone(),
                        field_type,
                    });
                    pieces.push(self.lower_to_text(field));
                }
            }
        }

        // Fold into one `Text`. Single owned piece (lone placeholder) is
        // returned as-is; a lone literal const is fine to bind borrowed; an
        // empty template yields "".
        let result = match pieces.len() {
            0 => self.emit_text_const(Vec::new()),
            1 => pieces[0],
            _ => {
                let mut acc = pieces[0];
                for &p in &pieces[1..] {
                    acc = self.concat_text(acc, p);
                }
                acc
            }
        };

        // Release the owned `Text` cells the materialized `args` tuple
        // holds (mirrors `lower_relation_lit`). A `NameRef`-aliased args
        // tuple contributes none, so this is a no-op there.
        if let Some(tv) = args_tv {
            if let Some(temps) = self.tuple_cell_text_temps.remove(&tv) {
                for t in temps {
                    self.release_text_temp(t);
                }
            }
        }

        result
    }

    /// Emit a borrowed `Text` constant (`Inst::Const`), like a string
    /// literal — not marked owned.
    fn emit_text_const(&mut self, bytes: Vec<u8>) -> ValueId {
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Text);
        self.insts.push(Inst::Const {
            dst,
            value: Const::Text(bytes),
            ty: ProcType::Text,
        });
        dst
    }

    /// Concatenate two `Text` values into a fresh owned `Text`, releasing any
    /// owned-temp operands — the same shape as a `||` in `lower_binary_expr`.
    fn concat_text(&mut self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Text);
        self.insts.push(Inst::ScalarOp {
            dst,
            op: ScalarOp::Concat,
            operand_type: ProcType::Text,
            lhs,
            rhs,
        });
        self.mark_text_owned(dst);
        self.release_text_temp(lhs);
        self.release_text_temp(rhs);
        dst
    }
}

/// True iff `call` supplies an argument literally named `name`. Mirrors the
/// checker's discriminator that routes `write_line { template, args }` to the
/// format-writing overload.
fn call_has_named_arg(call: &CallExpr, name: &str) -> bool {
    call.args()
        .map(|list| {
            list.args()
                .any(|a| a.name().map(|t| t.text().to_string()).as_deref() == Some(name))
        })
        .unwrap_or(false)
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
        Type::FormatText => {
            unreachable!("Type::FormatText is compile-time-only and never lowered")
        }
        Type::Sequence(elem) => ProcType::Sequence(Box::new(proc_type_from_type(elem))),
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
        ProcType::Sequence(elem) => Type::Sequence(Box::new(type_from_proc(elem))),
    }
}

/// Resolve a (possibly generator-applied) type-ref to a `ProcType`,
/// mirroring the typechecker's `resolve_type_ref`. `Sequence T` recurses
/// into the element type-ref; any other head is a built-in scalar name.
fn proc_type_from_type_ref(tr: &TypeRef) -> ProcType {
    match tr.name() {
        Some(tok) if tok.text() == "Sequence" => {
            let elem = tr
                .element()
                .map(|e| proc_type_from_type_ref(&e))
                .unwrap_or(ProcType::Unit);
            ProcType::Sequence(Box::new(elem))
        }
        Some(tok) => proc_type_from_builtin_name(tok.text()),
        None => ProcType::Unit,
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

    #[test]
    fn user_oper_call_lowers_to_in_module_call_and_owns_text() {
        let src = "program p;\n\
                   oper greet {} -> Text [ \"hi\" ];\n\
                   oper main {} [ let g = greet {}; write_line { message: g }; ];";
        let m = lower_ok(src);

        // `greet` is emitted as an in-module function that returns its body
        // value (a Text), not an extern.
        let greet = m
            .functions
            .iter()
            .find(|f| f.name == "greet")
            .expect("greet function emitted");
        assert!(matches!(greet.return_type, ProcType::Text));
        assert!(greet.blocks.iter().any(
            |b| matches!(b.terminator, Terminator::Return(Some(_)))
        ));

        // `main` calls `greet` by its surface linkage name (no extern symbol),
        // binds a Text dst, and releases the owned result at scope exit.
        let main = m
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main emitted");
        let insts = &main.blocks[0].insts;
        let call = insts.iter().find_map(|i| match i {
            Inst::Call {
                callee,
                dst,
                return_type,
                ..
            } if callee == "greet" => Some((dst.is_some(), return_type.clone())),
            _ => None,
        });
        let (binds_dst, ret_ty) = call.expect("main emits a call to greet");
        assert!(binds_dst, "the greet call binds a dst value");
        assert!(matches!(ret_ty, ProcType::Text));
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Release { .. })),
            "main releases the owned Text returned by greet"
        );
    }

    #[test]
    fn loop_carries_reassigned_var_as_block_param() {
        // A `var` accumulator reassigned in a counted loop rides an extra
        // header block parameter alongside the counter (the SSA join).
        let src = "program p;\n\
                   oper main {} [ var total := 0; for i := 1 to 3 do [ total := total + i; ]; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").expect("main");
        // The loop header is the block sealed with a `CondBr`; it carries the
        // counter plus the one carried var → two parameters.
        let header = main
            .blocks
            .iter()
            .find(|b| matches!(b.terminator, Terminator::CondBr { .. }))
            .expect("loop header block");
        assert_eq!(header.params.len(), 2, "counter + carried `total`");
    }

    #[test]
    fn if_both_arms_introduce_var_as_merge_block_param() {
        // A `var x;` assigned on both arms is definitely assigned after the
        // `if` — it rides the merge block as an (Integer) block parameter.
        let src = "program p;\n\
                   oper main {} [ var x; if true then [ x := 1; ] else [ x := 2; ]; let _y = x; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").expect("main");
        assert!(
            main.blocks
                .iter()
                .any(|b| b.params.iter().any(|(_, t)| matches!(t, ProcType::Integer))),
            "expected a merge block parameter carrying the introduced `x`"
        );
    }

    #[test]
    fn heap_var_carried_across_loop_diagnoses_t0076() {
        // Refcount-correct heap mutation across a join is future work — a
        // `Text` var reassigned inside a loop is a lowering error, not a
        // miscompile.
        let src = "program p;\n\
                   oper main {} [ var s := \"a\"; for i := 1 to 2 do [ s := s; ]; ];";
        let out = lower(src, FileId(0));
        assert!(
            out.diagnostics.iter().any(|d| d.code == "T0076"),
            "expected T0076, got {:?}",
            out.diagnostics
        );
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
    fn empty_relation_literal_lowers_to_relfalse() {
        // `Relation {}` = relfalse: an `Inst::RelationLit` with zero tuples and
        // the empty (nullary) heading — no T0018, no lowering assert.
        let src = "oper main {} [ let _f = Relation {}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let (tuples_len, heading_id) = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::RelationLit {
                    tuples, heading_id, ..
                } => Some((tuples.len(), *heading_id)),
                _ => None,
            })
            .expect("expected an Inst::RelationLit");
        assert_eq!(tuples_len, 0, "relfalse has zero tuples");
        assert!(
            m.headings[heading_id.0 as usize].attrs().is_empty(),
            "relfalse carries the empty (nullary) heading"
        );
    }

    #[test]
    fn headed_empty_relation_interns_the_annotation_heading() {
        // `let r: Relation { name: Text } = Relation {}` lowers to a zero-tuple
        // RelationLit whose interned heading is `{name}` — not the empty (∅)
        // heading of relfalse.
        let src = "oper main {} [ let _r: Relation { name: Text } = Relation {}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let (tuples_len, heading_id) = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::RelationLit {
                    tuples, heading_id, ..
                } => Some((tuples.len(), *heading_id)),
                _ => None,
            })
            .expect("expected an Inst::RelationLit");
        assert_eq!(tuples_len, 0, "still an empty relation (zero tuples)");
        let h = &m.headings[heading_id.0 as usize];
        assert!(h.lookup("name").is_some(), "carries the annotation's `name`");
        assert_eq!(h.attrs().len(), 1, "exactly the annotation heading");
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
    fn oper_param_reference_lowers_to_param_value() {
        // A body reference to a parameter resolves to the parameter's ValueId
        // (params occupy ValueId(0..N)), not the old `Unit` placeholder. Here
        // `echo`'s body is just `self`, so it returns parameter 0.
        let src = "program p; \
                   oper echo { self: Text } -> Text [ self ]; \
                   oper main {} [ let g = echo { self: \"x\" }; \
                   write_line { message: g }; ];";
        let m = lower_ok(src);
        let echo = m.functions.iter().find(|f| f.name == "echo").expect("echo fn");
        let term = &echo.blocks.last().expect("a block").terminator;
        assert!(
            matches!(term, Terminator::Return(Some(v)) if *v == ValueId(0)),
            "expected echo to return its `self` param ValueId(0), got {term:?}"
        );
    }

    #[test]
    fn nonempty_sequence_literal_lowers_to_sequence_lit() {
        // A non-empty sequence literal now constructs at runtime — no T0064.
        let src = "oper main {} [ let _s = Sequence [ \"a\", \"b\" ]; \
                   write_line { message: \"ok\" }; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let has_seq = main
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| matches!(i, Inst::SequenceLit { .. }));
        assert!(has_seq, "expected a SequenceLit instruction");
    }

    #[test]
    fn format_dispatches_sequence_placeholder_to_user_to_text() {
        // `{names}` over a Sequence has no built-in `to_text`; it must lower to
        // a call to the user `to_text { self: Sequence Text }` overload rather
        // than hitting the built-in conversion's `unreachable!`.
        let src = "program p; \
                   oper to_text { self: Sequence Text } -> Text [ \"x\" ]; \
                   oper main {} [ let names = Sequence [ \"a\" ]; \
                   let m = format { template: f\"{names}\", args: { names } }; \
                   write_line { message: m }; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let calls_to_text = main
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| matches!(i, Inst::Call { callee, .. } if callee == "to_text"));
        assert!(calls_to_text, "expected a call to the user `to_text`");
    }

    #[test]
    fn empty_sequence_literal_is_not_yet_executable_emits_t0064() {
        // An empty sequence carries no element to derive the payload layout
        // from at this stage; its execution lands with `load`. Graceful —
        // a diagnostic, not a panic; no module, so codegen never runs.
        let src = "oper main {} [ let _s: Sequence Integer = Sequence []; ];";
        let out = lower(src, FileId(0));
        assert!(
            out.module.is_none(),
            "expected no module for an empty sequence"
        );
        assert!(
            out.diagnostics.iter().any(|d| d.code == "T0064"),
            "expected T0064, got {:?}",
            out.diagnostics
        );
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

    // ── `if … then [ … ] else [ … ]` multi-block lowering ────────────

    #[test]
    fn if_with_else_lowers_to_four_blocks_with_merge_param() {
        // entry (CondBr) → then (Br) + else (Br) → merge (Return of its param).
        let src = "oper pick { self: Boolean } -> Integer \
                   [ if self then [ 1 ] else [ 2 ] ];";
        let m = lower_ok(src);
        let pick = m.functions.iter().find(|f| f.name == "pick").unwrap();
        assert_eq!(pick.blocks.len(), 4, "entry + then + else + merge");
        assert!(
            matches!(pick.blocks[0].terminator, Terminator::CondBr { .. }),
            "entry ends in CondBr, got {:?}",
            pick.blocks[0].terminator
        );
        let brs = pick
            .blocks
            .iter()
            .filter(|b| matches!(b.terminator, Terminator::Br { .. }))
            .count();
        assert_eq!(brs, 2, "both arms branch to the merge");
        // Exactly one block carries a parameter (the Integer join value), and
        // it is the merge block that returns that parameter.
        let merge = pick
            .blocks
            .iter()
            .find(|b| !b.params.is_empty())
            .expect("a merge block with a parameter");
        assert_eq!(merge.params.len(), 1);
        assert_eq!(merge.params[0].1, ProcType::Integer);
        assert!(matches!(merge.terminator, Terminator::Return(Some(_))));
    }

    #[test]
    fn if_without_else_lowers_to_three_blocks_no_param() {
        // entry (CondBr false→merge) → then (Br merge) → merge (Return None).
        let src = "oper act { self: Boolean } [ if self then [ {} ]; ];";
        let m = lower_ok(src);
        let act = m.functions.iter().find(|f| f.name == "act").unwrap();
        assert_eq!(act.blocks.len(), 3, "entry + then + merge");
        assert!(matches!(act.blocks[0].terminator, Terminator::CondBr { .. }));
        let brs = act
            .blocks
            .iter()
            .filter(|b| matches!(b.terminator, Terminator::Br { .. }))
            .count();
        assert_eq!(brs, 1, "only the then-arm branches to the merge");
        assert!(
            act.blocks.iter().all(|b| b.params.is_empty()),
            "a Unit `if` carries no merge parameter"
        );
    }

    // ── counted `for` loop — the first cyclic CFG ────────────────────

    #[test]
    fn for_counted_lowers_to_back_edge_cfg() {
        // entry (Br→header, seeding the counter) → header (Integer counter
        // param, CondBr) → body (Br→header — the back-edge) → exit (Return).
        let src = "oper counted {} [ for i := 0 to 2 do [ let _x = i; ]; ];";
        let m = lower_ok(src);
        let f = m.functions.iter().find(|f| f.name == "counted").unwrap();
        assert_eq!(f.blocks.len(), 4, "entry + header + body + exit");

        // Exactly one block — the header — carries a parameter: the counter.
        let headers: Vec<_> = f.blocks.iter().filter(|b| !b.params.is_empty()).collect();
        assert_eq!(headers.len(), 1, "only the header carries a block param");
        let header = headers[0];
        assert_eq!(header.params.len(), 1);
        assert_eq!(header.params[0].1, ProcType::Integer, "counter is Integer");
        assert!(
            matches!(header.terminator, Terminator::CondBr { .. }),
            "header ends in CondBr, got {:?}",
            header.terminator
        );

        // The back-edge: a `Br` to the header from a block that appears *after*
        // the header in program order (the defining property of a loop).
        let header_id = header.id;
        let header_idx = f.blocks.iter().position(|b| b.id == header_id).unwrap();
        let has_back_edge = f.blocks.iter().enumerate().any(|(idx, b)| {
            idx > header_idx
                && matches!(&b.terminator, Terminator::Br { target, .. } if *target == header_id)
        });
        assert!(has_back_edge, "a later block branches back to the header");

        // The entry seeds the counter: `Br → header` with one argument.
        match &f.blocks[0].terminator {
            Terminator::Br { target, args } => {
                assert_eq!(*target, header_id, "entry branches to the header");
                assert_eq!(args.len(), 1, "entry seeds the counter with the lower bound");
            }
            other => panic!("entry should end in Br, got {other:?}"),
        }
    }

    #[test]
    fn for_in_desugars_onto_counted_loop() {
        // `for name in names` lowers to the same counted-loop CFG plus the
        // desugar's `cardinality` (bound) and per-element index calls.
        let src = "oper main {} [ let names = Sequence [\"a\", \"b\"]; \
                   for name in names do [ write_line { message: name }; ]; ];";
        let m = lower_ok(src);
        let f = m.functions.iter().find(|f| f.name == "main").unwrap();

        // The same 4-block counted CFG with an Integer counter and a back-edge.
        assert_eq!(f.blocks.len(), 4, "entry + header + body + exit");
        let header = f
            .blocks
            .iter()
            .find(|b| !b.params.is_empty())
            .expect("header block with the counter param");
        assert_eq!(header.params[0].1, ProcType::Integer, "counter is Integer");
        let header_idx = f.blocks.iter().position(|b| b.id == header.id).unwrap();
        assert!(
            f.blocks.iter().enumerate().any(|(idx, b)| idx > header_idx
                && matches!(&b.terminator, Terminator::Br { target, .. } if *target == header.id)),
            "a later block branches back to the header (the back-edge)"
        );

        // The desugar's runtime calls: `cardinality` (→ `coddl_rc_length`) for
        // the bound, and `coddl_seq_index` for the per-iteration element read.
        let calls: Vec<&str> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match i {
                Inst::Call { callee, .. } => Some(callee.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            calls.contains(&"coddl_rc_length"),
            "expected a cardinality call, got {calls:?}"
        );
        assert!(
            calls.contains(&"coddl_seq_index"),
            "expected a per-element index call, got {calls:?}"
        );
    }

    #[test]
    fn while_lowers_to_header_cond_back_edge_cfg() {
        // entry (Br→header) → header (carried `j` param, condition, CondBr) →
        // body (Br→header — the back-edge) → exit.
        let src = "program p;\n\
                   oper w {} [ var j := 0; while j < 3 do [ j := j + 1; ]; ];";
        let m = lower_ok(src);
        let f = m.functions.iter().find(|f| f.name == "w").unwrap();
        assert_eq!(f.blocks.len(), 4, "entry + header + body + exit");

        // The header is the block sealed with a CondBr; it carries the one
        // carried var `j` — no counter (unlike the counted loop).
        let header = f
            .blocks
            .iter()
            .find(|b| matches!(b.terminator, Terminator::CondBr { .. }))
            .expect("loop header block");
        assert_eq!(header.params.len(), 1, "carried `j` only");
        assert_eq!(header.params[0].1, ProcType::Integer);

        // The back-edge: a later block branches back to the header.
        let header_idx = f.blocks.iter().position(|b| b.id == header.id).unwrap();
        assert!(
            f.blocks.iter().enumerate().any(|(idx, b)| idx > header_idx
                && matches!(&b.terminator, Terminator::Br { target, .. } if *target == header.id)),
            "a later block branches back to the header"
        );

        // Entry seeds the carried var: Br → header with one arg.
        match &f.blocks[0].terminator {
            Terminator::Br { target, args } => {
                assert_eq!(*target, header.id);
                assert_eq!(args.len(), 1, "entry seeds the carried `j`");
            }
            other => panic!("entry should end in Br, got {other:?}"),
        }
    }

    #[test]
    fn do_while_lowers_to_body_header_latch_cfg() {
        // entry (Br→body) → body (carried `k` param, body work + condition,
        // CondBr) → latch (Br→body — the empty back-edge) → exit.
        let src = "program p;\n\
                   oper d {} [ var k := 0; do [ k := k + 1; ] while k < 3; ];";
        let m = lower_ok(src);
        let f = m.functions.iter().find(|f| f.name == "d").unwrap();
        assert_eq!(f.blocks.len(), 4, "entry + body + latch + exit");

        // The body is both the loop header (carries the param) and the test
        // block (ends in CondBr) — the post-test shape.
        let body = f
            .blocks
            .iter()
            .find(|b| matches!(b.terminator, Terminator::CondBr { .. }))
            .expect("body/test block");
        assert_eq!(body.params.len(), 1, "carried `k`");
        assert_eq!(body.params[0].1, ProcType::Integer);

        // The latch: a later block whose sole role is the back-edge Br→body; it
        // carries no instructions (contrast `while`, whose back-edge block holds
        // the body work).
        let body_idx = f.blocks.iter().position(|b| b.id == body.id).unwrap();
        let latch = f
            .blocks
            .iter()
            .enumerate()
            .find(|(idx, b)| *idx > body_idx
                && matches!(&b.terminator, Terminator::Br { target, .. } if *target == body.id))
            .map(|(_, b)| b)
            .expect("a later block branches back to the body (the latch)");
        assert!(
            latch.insts.is_empty(),
            "the latch is an empty back-edge, got {:?}",
            latch.insts
        );

        // Entry seeds the body param and enters the body unconditionally.
        match &f.blocks[0].terminator {
            Terminator::Br { target, args } => {
                assert_eq!(*target, body.id, "entry enters the body unconditionally");
                assert_eq!(args.len(), 1, "entry seeds the carried `k`");
            }
            other => panic!("entry should end in Br, got {other:?}"),
        }
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

    // ── string interpolation: format + to_text ──────────────────────

    #[test]
    fn format_lowers_to_concat_with_placeholder_field() {
        let src = "oper main {} [ \
                   let name_in = read_line { prompt: \"n: \" }; \
                   let message = format { template: f\"Hello, {name}!\", args: { name: name_in } }; \
                   write_line { message }; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;

        // `args` is materialized once …
        assert!(
            insts.iter().any(|i| matches!(i, Inst::TupleLit { .. })),
            "expected args TupleLit"
        );
        // … and `{name}` is read out of it via TupleField.
        assert!(
            insts.iter().any(
                |i| matches!(i, Inst::TupleField { field_name, .. } if field_name == "name")
            ),
            "expected a TupleField for `name`"
        );
        // The two literal chunks become Text consts.
        let text_consts: Vec<String> = insts
            .iter()
            .filter_map(|i| match i {
                Inst::Const {
                    value: Const::Text(b),
                    ..
                } => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect();
        assert!(text_consts.iter().any(|s| s == "Hello, "), "{text_consts:?}");
        assert!(text_consts.iter().any(|s| s == "!"), "{text_consts:?}");
        // Three pieces fold via at least two Concats.
        let concats = insts
            .iter()
            .filter(|i| matches!(i, Inst::ScalarOp { op: ScalarOp::Concat, .. }))
            .count();
        assert!(concats >= 2, "expected ≥2 concats, got {concats}");
    }

    #[test]
    fn format_integer_placeholder_calls_int_to_text() {
        let src = "oper main {} [ \
                   let message = format { template: f\"count: {n}\", args: { n: 7 } }; \
                   write_line { message }; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks[0].insts.iter().any(
                |i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_int_to_text")
            ),
            "expected a coddl_int_to_text call for the Integer placeholder"
        );
    }

    #[test]
    fn write_line_format_overload_folds_then_writes() {
        // `write_line { template, args }` folds the template exactly like
        // `format` (TupleLit + TupleField), then writes the result through the
        // same `coddl_write_line` extern — with no intermediate `message` let.
        let src = "oper main {} [ \
                   write_line { template: f\"Hello, {name}!\", args: { name: \"X\" } }; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;

        assert!(
            insts.iter().any(|i| matches!(i, Inst::TupleLit { .. })),
            "expected args TupleLit"
        );
        assert!(
            insts.iter().any(
                |i| matches!(i, Inst::TupleField { field_name, .. } if field_name == "name")
            ),
            "expected a TupleField for `name`"
        );
        let write = insts
            .iter()
            .find(|i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_write_line"))
            .expect("expected a coddl_write_line call");
        assert!(
            matches!(write, Inst::Call { dst: None, return_type: ProcType::Unit, args, .. } if args.len() == 1),
            "write_line call should take the folded Text and return Unit, got {write:?}"
        );
    }

    #[test]
    fn to_text_character_lowers_to_char_to_text() {
        let src = "oper main {} [ let s = to_text { self: 'a' }; write_line { message: s }; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks[0]
                .insts
                .iter()
                .any(|i| matches!(i, Inst::CharToText { .. })),
            "expected CharToText for to_text on a Character"
        );
    }

    #[test]
    fn cardinality_lowers_to_coddl_rc_length_call() {
        // Both the `Sequence` and the `Relation` overload lower a
        // `cardinality {}` to a borrow-only call to the runtime's
        // `coddl_rc_length`, returning Integer — the count lives in the
        // shared RC-header `length` slot, so one symbol serves either.
        let src = "oper main {} [ \
                   let xs = Sequence [ \"a\", \"b\", \"c\" ]; \
                   let _ns = cardinality { self: xs }; \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let _nr = cardinality { self: r }; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let calls = main.blocks[0]
            .insts
            .iter()
            .filter(|i| {
                matches!(i, Inst::Call { callee, return_type, .. }
                    if callee == "coddl_rc_length"
                        && matches!(return_type, ProcType::Integer))
            })
            .count();
        assert_eq!(
            calls, 2,
            "both cardinality calls lower to coddl_rc_length -> Integer"
        );

        // The extern is declared once as a block-less in-module function so
        // each backend emits an import for it.
        let ext = m
            .functions
            .iter()
            .find(|f| f.linkage_name == "coddl_rc_length")
            .expect("coddl_rc_length extern declared");
        assert!(ext.blocks.is_empty(), "extern is a declaration only");
        assert!(matches!(ext.return_type, ProcType::Integer));
    }

    #[test]
    fn ufcs_method_call_lowers_like_prefix_call() {
        // `xs.cardinality {}` ≡ `cardinality { self: xs }` — a borrow-only
        // `coddl_rc_length` call with the receiver as the sole argument.
        let src = "oper main {} [ \
                   let xs = Sequence [ \"a\", \"b\" ]; \
                   let _n = xs.cardinality {}; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let calls = main.blocks[0]
            .insts
            .iter()
            .filter(|i| matches!(i, Inst::Call { callee, .. } if callee == "coddl_rc_length"))
            .count();
        assert_eq!(calls, 1, "the method call lowers to one coddl_rc_length call");
    }

    #[test]
    fn ufcs_user_oper_method_lowers_to_in_module_call() {
        // `"hi".echo {}` ≡ `echo { self: "hi" }` — an in-module call passing
        // the receiver as the sole argument.
        let src = "oper echo { self: Text } -> Text [ self ]; \
                   oper main {} [ let g = \"hi\".echo {}; write_line { message: g }; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert!(
            main.blocks[0].insts.iter().any(|i| matches!(
                i,
                Inst::Call { callee, args, .. } if callee == "echo" && args.len() == 1
            )),
            "expected an in-module `echo` call with the receiver as its sole arg"
        );
    }

    #[test]
    fn return_of_owned_local_retains_before_release() {
        // `[ let s = "a" || "b"; s ]` returns a bound owned-`Text` local. The
        // epilogue retains it (escaping) so the scope release doesn't free the
        // value the caller receives — return-of-local.
        let src = "oper f {} -> Text [ let s = \"a\" || \"b\"; s ]; oper main {} [];";
        let m = lower_ok(src);
        let f = m.functions.iter().find(|f| f.name == "f").unwrap();
        let block = &f.blocks[0];
        let ret = match &block.terminator {
            Terminator::Return(Some(v)) => *v,
            other => panic!("expected Return(Some(_)), got {other:?}"),
        };
        assert!(
            block
                .insts
                .iter()
                .any(|i| matches!(i, Inst::Retain { src } if *src == ret)),
            "the returned owned local must be retained (escaping) before the epilogue release"
        );
    }

    #[test]
    fn owned_local_not_returned_is_released_without_escaping_retain() {
        // A Unit-returning oper binds an owned `Text` it does *not* return: it
        // is released at scope exit, with no spurious escaping retain (which
        // would leak).
        let src = "oper g {} [ let _s = \"a\" || \"b\"; ];";
        let m = lower_ok(src);
        let g = m.functions.iter().find(|f| f.name == "g").unwrap();
        let insts = &g.blocks[0].insts;
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Release { .. })),
            "the non-returned owned local should be released"
        );
        assert!(
            !insts.iter().any(|i| matches!(i, Inst::Retain { .. })),
            "no escaping retain for a value that isn't returned"
        );
    }

    #[test]
    fn sequence_index_lowers_to_seq_index_attrload_retain() {
        // `s[i]` lowers to a bounds-checked `coddl_seq_index` call (-> Pointer,
        // the element record), an `AttrLoad` of the synthetic `value` cell at
        // offset 0, and — because the element is `Text` — a `Retain` into an
        // owned copy.
        let src = "oper main {} [ \
                   let xs = Sequence [ \"a\", \"b\" ]; \
                   let _e = xs[1]; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;

        // The runtime call returns a Pointer (the element record) from two args.
        let (call_dst, args, ret) = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call {
                    dst,
                    callee,
                    args,
                    return_type,
                } if callee == "coddl_seq_index" => {
                    Some((dst.unwrap(), args.clone(), return_type.clone()))
                }
                _ => None,
            })
            .expect("coddl_seq_index call emitted");
        assert!(matches!(ret, ProcType::Pointer));
        assert_eq!(args.len(), 2, "seq + index args");

        // An AttrLoad at offset 0 reads the Text element cell from that Pointer.
        assert!(
            insts.iter().any(|i| matches!(i,
                Inst::AttrLoad { src, offset: 0, attr_type, .. }
                    if *src == call_dst && matches!(attr_type, ProcType::Text))),
            "AttrLoad of the Text element at offset 0 of the record pointer"
        );

        // The Text element is retained into an owned copy.
        assert!(
            insts.iter().any(|i| matches!(i, Inst::Retain { .. })),
            "Text element retained into an owned copy"
        );

        // The extern is declared once as a block-less `(Pointer, Integer) -> Pointer`.
        let ext = m
            .functions
            .iter()
            .find(|f| f.linkage_name == "coddl_seq_index")
            .expect("coddl_seq_index extern declared");
        assert!(ext.blocks.is_empty(), "extern is a declaration only");
        assert!(matches!(ext.return_type, ProcType::Pointer));
        assert_eq!(ext.params.len(), 2);
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

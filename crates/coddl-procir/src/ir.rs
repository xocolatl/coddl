//! ProcIR data types.
//!
//! SSA-shaped, backend-agnostic. A `Module` carries one `Function` per
//! `oper` decl plus a synthetic extern `Function` for each runtime
//! symbol referenced. Each `Function` is a sequence of `BasicBlock`s;
//! each `BasicBlock` is a list of `Inst` plus a `Terminator`. Every
//! instruction defines at most one SSA value.
//!
//! See `docs/procir.md` for the spec and design rationale.

use std::fmt;

pub use coddl_types::{Heading, Type};

/// One compilation unit.
#[derive(Clone, Debug)]
pub struct Module {
    /// From `program <name>;`. Empty if the source had no program decl.
    pub program_name: String,
    pub functions: Vec<Function>,
    /// Per-module heading interner. Backends emit one static
    /// descriptor (`@.heading.<id>`) per entry; `ProcType::Relation`
    /// and the new relation-shaped instructions reference headings
    /// by their index into this vector. Stable across iterations
    /// (push-only — never reorder once an id is handed out).
    pub headings: Vec<Heading>,
    /// Plan-resolved public relvars referenced by this program, in
    /// source declaration order. Each entry drives codegen of one
    /// static slot global + one runtime materialization call in
    /// `main`'s prologue and one release in its epilogue. Empty when
    /// the source has no public relvars (or no plan was supplied).
    pub public_relvars: Vec<PublicRelvarBinding>,
    /// Default SQLite database path baked into the binary at compile
    /// time — canonicalised, absolute. `None` when the program declares
    /// no public relvars. Runtime resolution applies an env-var
    /// override before falling back to this default (see
    /// `coddl_resolve_op_field`).
    pub db_path_default: Option<String>,
    /// Database name from the `database <name>;` binding in the `.cd`
    /// source. Used by the runtime resolver to build the env-var key
    /// (`CODDL_<DB_UPPER>_FILE`). `None` when the program declares
    /// no public relvars.
    pub db_name: Option<String>,
    /// Static query plans baked from relvar-rooted relational subtrees the
    /// optimizer pushed to the backend. Each entry drives one
    /// `coddl_register_plan` call in `main`'s prologue (via
    /// `Inst::RegisterPlan`); `Inst::Query` references an entry by its
    /// `plan_id`. Empty when nothing was pushed.
    pub plans: Vec<PlanEntry>,
    /// In-memory `private` relvars that need a runtime slot (those read or
    /// assigned), each with its interned heading. Drives codegen of one slot
    /// global + an empty-init in `main`'s prologue and a release in its
    /// epilogue. Name-sorted for deterministic emission.
    pub private_relvar_slots: Vec<(String, HeadingId)>,
}

impl Module {
    /// Intern a heading: return the existing `HeadingId` if equal, or
    /// push a new one and return its id. Linear scan; the table is
    /// small in practice (one entry per unique heading the user
    /// program names).
    pub fn intern_heading(&mut self, h: &Heading) -> HeadingId {
        if let Some(i) = self.headings.iter().position(|existing| existing == h) {
            return HeadingId(i as u32);
        }
        let id = HeadingId(self.headings.len() as u32);
        self.headings.push(h.clone());
        id
    }
}

/// Index into a `Module::headings` vector. Stable for the module's
/// lifetime; rendered as `heading_<n>` in IR text.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct HeadingId(pub u32);

/// One public-relvar entry on a `Module`. The codegen layer turns each
/// entry into one slot global + one materialization call + one release;
/// the runtime materializer reads `table_name` + `columns` to prepare
/// the right SELECT against the SQLite catalog. Column order is
/// heading-canonical (attribute-name sorted), matching the
/// per-attribute byte layout `record_layout` produces.
#[derive(Clone, Debug)]
pub struct PublicRelvarBinding {
    /// Application-side name (e.g. `Greetings`).
    pub name: String,
    /// Heading id into `Module::headings`; identifies which static
    /// descriptor backs the materialization payload.
    pub heading_id: HeadingId,
    /// Physical SQLite table name from the `.cdstore` binding.
    pub table_name: String,
    /// Physical column names, paired with the application-side
    /// attribute names. In heading-canonical order. The
    /// app-name / col-name split lets future `.cdstore` rename clauses
    /// land without a schema change here.
    pub columns: Vec<(String, String)>,
    /// Declared candidate keys (one inner `Vec` per key). Threaded to
    /// `RelExpr::RelvarRef` so the SQL emitter can elide a redundant
    /// `DISTINCT`.
    pub keys: Vec<Vec<String>>,
}

/// One baked query plan on a `Module`. Codegen turns each entry into a
/// `coddl_register_plan` call in `main`'s prologue: the SQL text and the
/// logical database name become static byte constants, the result heading
/// reuses the module's heading descriptor at `result_heading_id`, and
/// `param_count` tells the runtime how many `CoddlParam`s a matching
/// `Inst::Query` supplies. `plan_id` is a dense per-module id (its own
/// namespace — not the storage layer's text-hash id).
#[derive(Clone, Debug)]
pub struct PlanEntry {
    /// Dense per-module plan id. Referenced by `Inst::Query` and
    /// `Inst::RegisterPlan`, and passed to `coddl_register_plan` /
    /// `coddl_query` at runtime.
    pub plan_id: u32,
    /// Logical database the plan runs against (the `database <name>;`
    /// handle). Resolved to a connection path through the runtime's
    /// database registry.
    pub db_name: String,
    /// The baked SQL text, ready to prepare (`?N` placeholders for SQLite).
    pub sql: String,
    /// Number of bind placeholders in `sql` (`?1..?N`). For a cardinality-1
    /// sibling plan this counts the scalar args *plus* the trailing cell
    /// binds the runtime fills from the dispatch slot's single row.
    pub param_count: u32,
    /// One entry per relation-valued parameter (`__CODDL_REL_<slot>__`
    /// marker) in `sql`, in slot order; the runtime validates a matching
    /// `Inst::Query`'s bound relations against these before expanding the
    /// markers. Empty for a plan with no shipped relation.
    pub rel_params: Vec<RelParamReg>,
    /// Heading id (into `Module::headings`) of the rows the plan returns.
    pub result_heading_id: HeadingId,
    /// Dense plan id of this plan's cardinality-1 sibling, when one was
    /// baked: the specialized `WHERE shared = ?N…` form of a root `matching`
    /// over a shipped relation. The runtime fires it instead of this plan
    /// when the dispatch slot holds exactly one row, binding the row's cells
    /// after the scalar args. The sibling is an ordinary entry of its own;
    /// nothing references it from an `Inst::Query`.
    pub card1_alt: Option<u32>,
    /// Which slot's runtime cardinality drives the `card1_alt` dispatch.
    /// Meaningful only when `card1_alt` is set (v1 bakes a sibling only for
    /// a single-slot plan, so this is always 0 today — carried explicitly so
    /// the registration ABI doesn't assume it).
    pub dispatch_slot: u32,
}

/// Registration metadata for one relation-valued parameter of a plan — the
/// per-slot half of what `coddl_register_plan` receives (the codegen emits
/// these as an interleaved `[arity, flags]` static array).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelParamReg {
    /// Column count the bound relation must have.
    pub arity: u32,
    /// Whether an empty relation at this slot makes the whole result
    /// provably empty — the runtime then returns a fresh empty relation
    /// without firing a statement (the empty-slot short-circuit).
    pub absorbs_empty: bool,
}

/// A function — either a defined one (non-empty `blocks`) or an extern
/// declaration (`blocks.is_empty()`).
#[derive(Clone, Debug)]
pub struct Function {
    /// User-visible Coddl name. For an extern, this matches the
    /// surface symbol the user wrote (`write_line`).
    pub name: String,
    /// C-ABI symbol the backend emits. For `main`, `"main"`; for an
    /// extern, the declared `coddl_*` name. The lowering pass sets
    /// this explicitly so backends never have to derive it.
    pub linkage_name: String,
    pub params: Vec<(String, ProcType)>,
    pub return_type: ProcType,
    /// Empty for an extern declaration.
    pub blocks: Vec<BasicBlock>,
}

impl Function {
    pub fn is_extern(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct BasicBlock {
    pub id: BlockId,
    /// Block parameters — SSA values bound on entry to this block, in order.
    /// Empty for the entry block (function parameters are seeded from the
    /// signature) and for the arms of an `if`; a merge block created by
    /// `lower_if_expr` carries one parameter (the if-expression's value).
    /// A predecessor's [`Terminator::Br`] supplies one argument per param.
    pub params: Vec<(ValueId, ProcType)>,
    pub insts: Vec<Inst>,
    pub terminator: Terminator,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BlockId(pub u32);

/// An SSA value name. Rendered `%0`, `%1`, … in the `Display` form.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ValueId(pub u32);

#[derive(Clone, Debug)]
pub enum Inst {
    /// Materialize a compile-time constant.
    Const {
        dst: ValueId,
        value: Const,
        ty: ProcType,
    },
    /// Call a function by linkage name. `dst` is `None` when the
    /// callee returns `Unit`.
    Call {
        dst: Option<ValueId>,
        callee: String,
        args: Vec<ValueId>,
        return_type: ProcType,
    },
    /// Build a tuple value from its fields, in canonical heading
    /// order. The `heading` is the type-level shape; `fields` holds
    /// the SSA values, one per attribute, paired with the attribute
    /// name. Tuples are pure value types — no heap, no RC. Backends
    /// represent them as a compile-time grouping over the field SSA
    /// values; at ABI boundaries the fields flatten into their
    /// component scalar operands (the same shape as `Text → (ptr,
    /// len)`, recursive for nested tuples).
    TupleLit {
        dst: ValueId,
        fields: Vec<(String, ValueId)>,
        heading: Heading,
    },
    /// Project a single field out of a tuple. `field_type` is the
    /// attribute's ProcType (carried so backends needn't re-lookup
    /// through the heading). Like `TupleLit`, this is a compile-time
    /// projection — no runtime work.
    TupleField {
        dst: ValueId,
        src: ValueId,
        field_name: String,
        field_type: ProcType,
    },
    /// Materialize a flattened tuple `src` into a heap record — the boxed
    /// representation of a large tuple (`layout::tuple_is_boxed`). Lowering
    /// allocates a `length = 1` RC payload (`CoddlKind::Relation`), stores each
    /// attribute's flattened cells (retain-on-store for Text/relation cells),
    /// and does **not** seal (a tuple is one record, not a set). `dst` carries
    /// `ProcType::Tuple(heading)` but its ABI value is a single pointer. Emitted
    /// for a large-tuple literal and to box a small tuple at a return site.
    TupleBox {
        dst: ValueId,
        src: ValueId,
        heading_id: HeadingId,
    },
    /// Read a boxed tuple `src` (one RC record) back into a flattened tuple
    /// value — the inverse of [`Inst::TupleBox`]. Reuses the per-attribute
    /// record read (no cardinality check; a box is always one record). `dst`
    /// carries `ProcType::Tuple(heading)`, flattened. Emitted at a call whose
    /// result is a *small* tuple (the return ABI hands back a boxed pointer).
    TupleUnbox {
        dst: ValueId,
        src: ValueId,
        heading_id: HeadingId,
    },
    /// Build a relation value from its tuple operands. Each `tuples[i]`
    /// is a `ValueId` typed `ProcType::Tuple(h)` where `h` matches the
    /// heading at `heading_id`. Lowering allocates an RC payload,
    /// writes each tuple's flattened bytes into the record buffer,
    /// then calls `coddl_relation_seal` (sort + adjacent dedup). The
    /// resulting `dst` carries `ProcType::Relation(heading_id)`.
    RelationLit {
        dst: ValueId,
        tuples: Vec<ValueId>,
        heading_id: HeadingId,
    },
    /// Construct a `Sequence` value from its elements, in order. Each
    /// `element` is a `ValueId` of the sequence's element type, stored as
    /// the single cell of one record in the synthetic single-attribute
    /// heading at `heading_id`. Lowering allocates an RC payload (kind
    /// `Sequence`), writes each element into its record, and — unlike
    /// `RelationLit` — does **not** seal: sequences are ordered and
    /// duplicate-preserving. The `dst` carries `ProcType::Sequence(elem)`.
    SequenceLit {
        dst: ValueId,
        elements: Vec<ValueId>,
        heading_id: HeadingId,
    },
    /// Increment the refcount of `src`. Emitted by the lowerer when a
    /// heap-typed value is bound to a `let` whose RHS is a `NameRef`
    /// to an already-bound source (so both bindings hold a count).
    /// Backends lower to a single `coddl_rc_retain` call.
    Retain { src: ValueId },
    /// Decrement the refcount of `src`. Emitted by the lowerer at
    /// scope-exit points for every heap-typed local. Backends lower
    /// to a single `coddl_rc_release` call.
    Release { src: ValueId },
    /// Print a relation. Polymorphic over heading via the `heading_id`
    /// — the lowerer carries it from the argument value's
    /// `ProcType::Relation(_)` static type. Backends lower to
    /// `call coddl_write_relation(rel_ptr, &heading_descriptor)`. The
    /// `write_relation` builtin's surface call lowers to this instead
    /// of a generic `Inst::Call` so the backend doesn't have to
    /// special-case the descriptor lookup in its `Inst::Call` path.
    WriteRelation { rel: ValueId, heading_id: HeadingId },
    /// Scalar binary operator. The result type depends on `op`: comparison /
    /// Boolean ops yield `ProcType::Boolean`, arithmetic ops yield
    /// `ProcType::Integer`, and `Concat` yields `ProcType::Text`.
    /// `operand_type` is the (shared) operand type — for `Concat` it is always
    /// `Text` (the lowerer converts a `Character` operand via
    /// [`Inst::CharToText`] first) — so backends pick the right native op.
    ScalarOp {
        dst: ValueId,
        op: ScalarOp,
        operand_type: ProcType,
        lhs: ValueId,
        rhs: ValueId,
    },
    /// Convert a `Character` (an inline codepoint) to a one-character `Text`
    /// value. Emitted by the lowerer to normalize a `Character` operand of
    /// `||` before a `ScalarOp::Concat`. Backends call the runtime's
    /// `coddl_char_to_text` (payload pointer) and `coddl_utf8_len` (byte
    /// length) to build the resulting `(ptr, len)` Text.
    CharToText { dst: ValueId, src: ValueId },
    /// Load one attribute from a record pointer at the static byte
    /// offset. Used inside predicate helper functions to read the
    /// row's attributes. `attr_type` is the attribute's machine-level
    /// type (Integer, Boolean, Text). Backends emit a byte-offset
    /// `getelementptr` + `load`.
    AttrLoad {
        dst: ValueId,
        src: ValueId,
        offset: u32,
        attr_type: ProcType,
    },
    /// Store one scalar into a record cell at the static byte offset — the dual
    /// of [`Inst::AttrLoad`]. Used inside an `extend` helper to write the
    /// widened record's cells (surviving operand attributes and computed new
    /// ones). `record` is the destination record pointer; `value` is the scalar
    /// to store; `attr_type` is its machine-level type (Integer or Text).
    /// Defines no SSA value. Backends emit a byte-offset `getelementptr` +
    /// `store` (Text stores the `(ptr, len)` pair at `offset` / `offset + 8`).
    AttrStore {
        record: ValueId,
        offset: u32,
        value: ValueId,
        attr_type: ProcType,
    },
    /// Restrict a relation by a predicate. `predicate_linkage` is the
    /// linkage name of a synthesized helper function with C ABI
    /// `fn(*const u8) -> i8` (non-zero = keep). Backends emit a call
    /// to `coddl_relation_where(src, &descriptor, &predicate)`.
    /// `heading_id` indexes the same per-module heading table the
    /// other relation ops use.
    Where {
        dst: ValueId,
        src: ValueId,
        predicate_linkage: String,
        heading_id: HeadingId,
    },
    /// Extend a relation with computed attributes (surface `extend`), run
    /// in-process. `helper_linkage` is the linkage name of a synthesized helper
    /// with C ABI `fn(*const u8 src_record, *mut u8 dst_record)` that fills the
    /// whole widened record (surviving cells permuted to result offsets + new
    /// computed cells). Backends emit a call to `coddl_relation_extend(src,
    /// &src_descriptor, &result_descriptor, &helper)`. `src_heading_id` /
    /// `result_heading_id` index the per-module heading table.
    Extend {
        dst: ValueId,
        src: ValueId,
        helper_linkage: String,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
    },
    /// Project a relation onto a subset of its attributes (surface
    /// `project`), run in-process. Backends emit a call to
    /// `coddl_relation_project(src, &src_descriptor, &result_descriptor)`,
    /// which narrows each record to the kept attributes and re-seals
    /// (RM Pro 3). `src_heading_id` is the operand's heading;
    /// `result_heading_id` is the narrowed heading that `dst` carries as
    /// `ProcType::Relation`. Both index the per-module heading table.
    Project {
        dst: ValueId,
        src: ValueId,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
    },
    /// Rename a relation's attributes in-process (surface `rename`). Backends
    /// emit a static `u32` permutation (`perm[dst_i]` = the source attribute
    /// index for destination attribute `dst_i`) and a call to
    /// `coddl_relation_rename(src, &src_descriptor, &result_descriptor, perm,
    /// perm_len)`, which permutes each record into the renamed (re-sorted)
    /// layout and re-seals. `dst` carries the renamed heading at
    /// `result_heading_id`.
    Rename {
        dst: ValueId,
        src: ValueId,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
        perm: Vec<u32>,
    },
    /// Force a relation into an ordered `Sequence` (surface `load … from … order
    /// [ … ]`, the RM Pro 7 iteration gate). Backends emit a call to
    /// `coddl_load_ordered(src, &@.heading.<heading_id>, keys, key_count)`, which
    /// sorts `src`'s records by the order keys and returns a `Sequence` payload
    /// reusing the source layout (each element record *is* a source tuple). `keys`
    /// is a static `u32` array emitted like `Rename`'s `perm`: each entry is an
    /// index into the source heading's canonical attrs, with bit 31 set for a
    /// descending key (empty for no `order` clause). `dst` carries
    /// `ProcType::Sequence(Tuple(H))`.
    Load {
        dst: ValueId,
        src: ValueId,
        heading_id: HeadingId,
        keys: Vec<u32>,
    },
    /// Collect a `Sequence` back into a relation **set** (the reverse `load
    /// <relvar> from <sequence>` form — the inverse of [`Inst::Load`]). Backends
    /// emit a call to `coddl_relation_from_sequence(src, &@.heading.<heading_id>)`,
    /// which copies the sequence's element tuples, retains their `Text` cells, and
    /// seals (sort + dedup, RM Pro 1, 3). `heading_id` is the element-tuple
    /// heading; `dst` carries `ProcType::Relation(H)`.
    Collect {
        dst: ValueId,
        src: ValueId,
        heading_id: HeadingId,
    },
    /// Restructure a relation between two layouts that hold the same leaf cells
    /// (surface `wrap` / `unwrap`). Backends emit a call to
    /// `coddl_relation_restructure(src, &src_descriptor, &result_descriptor)`,
    /// which flattens both descriptors to leaves, matches them by name, permutes
    /// each record into the destination layout, and re-seals. `dst` carries the
    /// restructured heading at `result_heading_id`.
    Restructure {
        dst: ValueId,
        src: ValueId,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
    },
    /// Natural join two in-memory relations (surface `join`, Algebra-A AND).
    /// Backends emit a call to `coddl_relation_join(lhs, &lhs_descriptor, rhs,
    /// &rhs_descriptor, &result_descriptor)`, which matches records on the
    /// shared attributes, emits the union of attributes, and re-seals.
    /// `dst` carries the union heading at `result_heading_id`. All three index
    /// the per-module heading table.
    Join {
        dst: ValueId,
        lhs: ValueId,
        rhs: ValueId,
        lhs_heading_id: HeadingId,
        rhs_heading_id: HeadingId,
        result_heading_id: HeadingId,
    },
    /// Set union of two in-memory relations with identical headings (surface
    /// `union`, Algebra-A OR). Backends emit a call to
    /// `coddl_relation_union(lhs, rhs, &descriptor)`, which concatenates both
    /// payloads and re-seals (content-aware dedup). Identical headings ⇒ one
    /// `heading_id` for both operands and the result.
    Union {
        dst: ValueId,
        lhs: ValueId,
        rhs: ValueId,
        heading_id: HeadingId,
    },
    /// Set difference of two in-memory relations with identical headings
    /// (surface `minus`, Algebra-A AND-NOT). Backends emit a call to
    /// `coddl_relation_minus(lhs, rhs, &descriptor)`, which keeps each `lhs`
    /// record not present in `rhs` (content-aware membership; no re-seal —
    /// the result is a subset of the already-sealed `lhs`).
    Minus {
        dst: ValueId,
        lhs: ValueId,
        rhs: ValueId,
        heading_id: HeadingId,
    },
    /// Gate an in-memory relation by a Boolean (surface `when`, the IR-level
    /// `R times ⟨c⟩` with the condition lifted to reltrue/relfalse). Backends
    /// emit a call to `coddl_relation_when(src, cond, &descriptor)`: `cond ≠ 0`
    /// retains and returns `src` itself, `cond = 0` returns a fresh empty
    /// relation with the same heading — O(1), no copy, no re-seal. `cond` is a
    /// Boolean value; `heading_id` describes both operand and result.
    Gate {
        dst: ValueId,
        src: ValueId,
        cond: ValueId,
        heading_id: HeadingId,
    },
    /// Relational COALESCE of two in-memory relations with identical headings
    /// (surface `otherwise`): the primary if it is nonempty, else the fallback
    /// (the IR-level `R union (D times (reltrue minus (R project {})))` — arms
    /// exclusive, so no union/dedup ever runs). Backends emit a call to
    /// `coddl_relation_otherwise(primary, fallback)` — a header length check
    /// plus a retain of the winner, O(1); the result is already a sealed set
    /// because it *is* one of the operands. No descriptor is needed at run
    /// time, so the instruction carries none; `dst`'s heading is recorded by
    /// the lowerer like every other value.
    Otherwise {
        dst: ValueId,
        primary: ValueId,
        fallback: ValueId,
    },
    /// Transitive closure of a binary relation (surface `tclose`, Algebra-A
    /// `◄TCLOSE►`). Backends emit a call to `coddl_relation_tclose(src,
    /// &descriptor)`, which iterates a naive fixpoint (compose the result with
    /// the input edge set until no new pair is added) and re-seals. The result
    /// heading equals the (binary) operand heading, so one `heading_id`
    /// describes both operand and result.
    TClose {
        dst: ValueId,
        src: ValueId,
        heading_id: HeadingId,
    },
    /// Compare two same-heading relations (typechecked), producing a Boolean
    /// `dst`. Observational (RM Pre 8): content-aware record comparison —
    /// indifferent to seal state and physical row order, never a pointer or
    /// payload compare. Backends emit a call to `coddl_relation_eq(lhs, rhs,
    /// &descriptor)` or `coddl_relation_subset(lhs, rhs, &descriptor,
    /// proper)`. Only three shapes reach codegen: the lowerer handles `<>`
    /// (negate `Eq`) and `>=`/`>` (swap the operands of `Subset`).
    RelCompare {
        dst: ValueId,
        op: RelCmpOp,
        lhs: ValueId,
        rhs: ValueId,
        heading_id: HeadingId,
    },
    /// Collapse a single-row relation to a tuple (TTM RM Pre 10).
    /// Backends emit a call to `coddl_extract_check_cardinality(src,
    /// &descriptor)` which aborts if cardinality ≠ 1, then read each
    /// attribute from the returned record pointer into per-field
    /// scalar values, bundling them into a `ValueRepr::Tuple` for
    /// `dst`. `dst` carries `ProcType::Tuple(heading)` where the
    /// heading lives in the per-module heading table at
    /// `heading_id`.
    Extract {
        dst: ValueId,
        src: ValueId,
        heading_id: HeadingId,
    },
    /// Materialize one public relvar from SQLite at program start and
    /// stash the resulting RC pointer in its slot global. Emitted once
    /// per public relvar in `main`'s prologue, after
    /// `coddl_runtime_init` and before the body. The static
    /// `(table_name, columns, db_name, default_path, descriptor)`
    /// fields live on the corresponding `Module::public_relvars`
    /// entry; backends look up by `name`. Lowers to a single call to
    /// `coddl_sqlite_relvar_init`.
    RelvarSlotInit { name: String, heading_id: HeadingId },
    /// Release the RC pointer in the named relvar's slot. Emitted once
    /// per public relvar in `main`'s epilogue, before
    /// `coddl_runtime_shutdown`. Backends load the slot's pointer +
    /// call `coddl_rc_release`.
    RelvarSlotRelease { name: String },
    /// Read a public relvar's currently-materialized value into a new
    /// SSA value. Backends emit `load ptr from @<name>_slot` + a
    /// `coddl_rc_retain` so the consumer holds its own refcount; the
    /// lowerer's existing temp-source release logic frees it if the
    /// consumer doesn't bind it.
    RelvarRead {
        dst: ValueId,
        name: String,
        heading_id: HeadingId,
    },
    /// Initialize an in-memory `private` relvar's slot with an empty
    /// relation at program start. Emitted once per used private relvar in
    /// `main`'s prologue (after `coddl_runtime_init`, before the body).
    /// Unlike `RelvarSlotInit` there is no SQL source — the slot starts
    /// empty and is filled by `RelvarSlotStore`. Lowers to a single call to
    /// `coddl_relvar_slot_init_empty`.
    PrivateRelvarSlotInit { name: String, heading_id: HeadingId },
    /// Store a relation value into a relvar's slot — relational assignment
    /// `R := <expr>`. Move semantics: the runtime releases the slot's
    /// previous value (if any) and takes ownership of `value`, so the
    /// lowerer emits no release for the RHS. Lowers to a single call to
    /// `coddl_relvar_slot_store`.
    RelvarSlotStore { name: String, value: ValueId },
    /// Register the logical database so the runtime can resolve its
    /// connection path. Emitted once in `main`'s prologue when the program
    /// has at least one pushed plan, after `coddl_runtime_init`. Backends
    /// resolve the path via `coddl_resolve_op_field` (env override then the
    /// baked default) and call `coddl_register_database`, reading the name
    /// and default path from `Module::db_name` / `Module::db_path_default`.
    RegisterDatabase,
    /// Register one baked query plan with the runtime. Emitted once per
    /// `Module::plans` entry in `main`'s prologue, after `RegisterDatabase`.
    /// Backends look the entry up by `plan_id` and call `coddl_register_plan`
    /// with its SQL text, database name, param count, rel-param arities, and
    /// the result heading descriptor.
    RegisterPlan { plan_id: u32 },
    /// Execute a registered plan with the given bind parameters and bind the
    /// returned sealed relation to `dst`. Fire-on-call: the prepared
    /// statement runs at this point (the force site), lazily. Each param
    /// pairs the bind SSA value with its scalar `ProcType` so backends pick
    /// the right `CoddlParam` kind and field. `rels` are the plan's
    /// **relation-valued** parameters in slot order — each pairs an in-memory
    /// relation value with its static heading descriptor (like
    /// [`Inst::InsertFrom`]'s source); the runtime expands the plan's
    /// `__CODDL_REL_<slot>__` markers with the bound rows (numbered after the
    /// scalar params) before preparing. Empty for an ordinary query. `dst`
    /// carries `ProcType::Relation(heading_id)` — the plan's result heading.
    /// Backends build a `CoddlParam` array plus a `CoddlRelParam` array and
    /// call `coddl_query`.
    Query {
        dst: ValueId,
        plan_id: u32,
        params: Vec<(ValueId, ProcType)>,
        rels: Vec<(ValueId, HeadingId)>,
        heading_id: HeadingId,
    },
    /// Execute a registered **DML** plan (a `DELETE`/`INSERT`/`UPDATE`) with the
    /// given bind parameters, for its effect only — no result is bound. Like
    /// [`Inst::Query`] it fires at this point (inside the enclosing
    /// `transaction [...]`'s begin/commit pair), and each param pairs the bind
    /// SSA value with its scalar `ProcType`. Backends build a `CoddlParam` array
    /// and call `coddl_exec`. The plan's registered result heading is unused
    /// (DML returns no rows).
    Dml {
        plan_id: u32,
        params: Vec<(ValueId, ProcType)>,
    },
    /// Insert the rows of an **in-memory** relation `src` into a public relvar,
    /// idempotently. `plan_id` is a registered *insert template* — an
    /// `INSERT … SELECT … FROM __CODDL_REL_0__ … WHERE NOT EXISTS (…)` whose
    /// rel-param marker the runtime expands to a `(VALUES …)` of `(?,…)`
    /// row-groups, in batches (an insert is cumulative, so batching is safe —
    /// unlike a read query, which never splits).
    /// Backends pass `src`'s relation pointer + its static heading descriptor
    /// (like [`Inst::WriteRelation`]) to `coddl_exec_insert`, which iterates the
    /// relation, binds each row's cells, and runs the template in batches. Used
    /// for `t := t union <literal-or-private>` where the source can't be pushed
    /// to SQL. Fires inside the enclosing `transaction [...]`.
    InsertFrom {
        plan_id: u32,
        src: ValueId,
        heading_id: HeadingId,
    },
}

/// The relation-comparison kinds `Inst::RelCompare` carries. `Eq` is
/// observational set equality (surface `=`); `Subset` is the subset test
/// (surface `<=`; `proper: true` for the strict `<`). The negated and
/// swapped surface spellings (`<>`, `>=`, `>`) never reach codegen — the
/// lowerer negates `Eq` and swaps `Subset`'s operands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RelCmpOp {
    Eq,
    Subset { proper: bool },
}

/// Scalar binary operator kinds. The comparison ops (`Eq`…`GtEq`) and the
/// Boolean `And`/`Or` produce a Boolean; the arithmetic ops (`Add`…`Div`)
/// produce an Integer; `Concat` produces a Text. Backends lower them to
/// native `icmp` / `and` / `or` / `add` / `sub` / `mul` / `sdiv` ops, or (for
/// `Concat`) a call to the runtime's `coddl_text_concat`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScalarOp {
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
    /// `Boolean → Boolean` prefix negation (surface `not` / `¬`). Unary:
    /// backends read only `lhs` (the lowerer passes a dummy `rhs = lhs`)
    /// and emit a single `xor i1 x, true`.
    Not,
    /// `Integer × Integer → Integer`. `Div` truncates toward zero (surface
    /// `div`).
    Add,
    Sub,
    Mul,
    Div,
    /// `Integer × Integer → Rational` — exact division (surface `/`). Builds a
    /// reduced fraction via the `coddl_rational_from_ints` runtime helper;
    /// produces a compound `ValueRepr::Rational`.
    RatioFromInts,
    /// `Rational × Rational → Rational` arithmetic (surface `+ - * /` on
    /// Rationals). Each lowers to a `coddl_rational_{add,sub,mul,div}` helper
    /// call producing a compound `ValueRepr::Rational`.
    RationalAdd,
    RationalSub,
    RationalMul,
    RationalDiv,
    /// `Text × Text → Text` (operands normalized to Text by the lowerer, so a
    /// `Character` operand is converted via [`Inst::CharToText`] first).
    Concat,
}

#[derive(Clone, Debug)]
pub enum Terminator {
    Return(Option<ValueId>),
    /// Two-way conditional branch on a `Boolean` value. Neither target
    /// takes branch arguments — values that must survive the join flow
    /// through the merge block's parameters via the arms' [`Terminator::Br`]
    /// (see `lower_if_expr`). LLVM lowers this to `br i1`; Cranelift to
    /// `brif`.
    CondBr {
        cond: ValueId,
        then_block: BlockId,
        else_block: BlockId,
    },
    /// Unconditional branch to `target`, passing `args` as that block's
    /// parameters (SSA join). LLVM realizes the params as `phi` nodes at the
    /// top of `target`; Cranelift as `target`'s block parameters.
    Br {
        target: BlockId,
        args: Vec<ValueId>,
    },
    /// Reserved for control-flow paths the typechecker has ruled out
    /// (e.g. a divergent branch). Not produced by hello-world.
    Unreachable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Const {
    Integer(i64),
    /// String literal payload as UTF-8 bytes (escapes already decoded).
    Text(Vec<u8>),
    /// A `Character` literal as its Unicode scalar value (escapes already
    /// decoded). Stored inline as a codepoint (`i32` at the ABI).
    Character(u32),
    /// An `Approximate` value as its **canonical** IEEE-754 double bit
    /// pattern (`u64`, not `f64`, so the enum stays `Eq`). NaN is collapsed
    /// to one quiet-NaN pattern and `−0.0` to `+0.0` on ingest, so bit
    /// equality is a proper (reflexive) equality. `double` at the ABI.
    Approximate(u64),
    /// A bounded exact `Rational` as a **reduced** `(numer, denom)` pair
    /// (`gcd(|n|,d) = 1`, `d > 0`, `0 = (0,1)`). Two `i64`s — a compound
    /// value at the ABI (like `Text`'s `(ptr, len)`), 16-byte cell.
    Rational(i64, i64),
    /// `true` / `false` — Boolean literal value.
    Boolean(bool),
    /// The `Tuple {}` value — produced where the source had `{}` or
    /// an implicit unit return.
    Unit,
}

/// Machine-level type. Not the surface `Type` from `coddl-types` —
/// `Sequence` becomes a runtime handle (`Pointer`). `Tuple(H)`
/// carries the same heading the typechecker reasoned about; at ABI
/// boundaries each attribute flattens into its component scalar
/// operands (nested tuples recursively). `Relation(HeadingId)`
/// carries a per-module heading interner id; the value is a single
/// pointer at the ABI level (the RC-managed payload), with the
/// heading living in static data and reached via the descriptor.
/// Every built-in scalar gets a variant from day one so backends
/// can pattern-match exhaustively.
///
/// Not `Copy` — the `Tuple` variant carries a heap-backed heading.
/// Clone is cheap relative to typical compile-time data sizes; runtime
/// never touches `ProcType` values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcType {
    Integer,
    Rational,
    Approximate,
    Text,
    Character,
    Binary,
    Byte,
    Boolean,
    Unit,
    Pointer,
    Tuple(Heading),
    Relation(HeadingId),
    /// Ordered, finite list of values of one element type — an RC'd heap
    /// value (kind `Sequence`). Physically a kind-tagged, *unsealed*
    /// relation over a synthetic single-attribute heading, so element
    /// storage and drop reuse the relation machinery. Carries the element
    /// `ProcType` (not a heading id) so it round-trips through the free
    /// `proc_type_from_type` in tuple-field contexts.
    Sequence(Box<ProcType>),
}

// ── Display ──────────────────────────────────────────────────────────

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block_{}", self.0)
    }
}

impl fmt::Display for ProcType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcType::Integer => f.write_str("Integer"),
            ProcType::Rational => f.write_str("Rational"),
            ProcType::Approximate => f.write_str("Approximate"),
            ProcType::Text => f.write_str("Text"),
            ProcType::Character => f.write_str("Character"),
            ProcType::Binary => f.write_str("Binary"),
            ProcType::Byte => f.write_str("Byte"),
            ProcType::Boolean => f.write_str("Boolean"),
            ProcType::Unit => f.write_str("Unit"),
            ProcType::Pointer => f.write_str("Pointer"),
            ProcType::Tuple(h) => write!(f, "Tuple {h}"),
            ProcType::Relation(id) => write!(f, "Relation heading_{}", id.0),
            ProcType::Sequence(elem) => write!(f, "Sequence {elem}"),
        }
    }
}

impl fmt::Display for Const {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Const::Integer(n) => write!(f, "{n}"),
            Const::Text(bytes) => {
                f.write_str("\"")?;
                for &b in bytes {
                    match b {
                        b'\n' => f.write_str("\\n")?,
                        b'\r' => f.write_str("\\r")?,
                        b'\t' => f.write_str("\\t")?,
                        b'"' => f.write_str("\\\"")?,
                        b'\\' => f.write_str("\\\\")?,
                        0x20..=0x7e => write!(f, "{}", b as char)?,
                        _ => write!(f, "\\x{b:02x}")?,
                    }
                }
                f.write_str("\"")
            }
            Const::Character(cp) => match char::from_u32(*cp) {
                Some(c) => write!(f, "'{}'", c.escape_default()),
                None => write!(f, "'\\u{{{cp:x}}}'"),
            },
            // Print the exponent form so it re-reads as an `Approximate` literal.
            Const::Approximate(bits) => write!(f, "{:e}", f64::from_bits(*bits)),
            Const::Rational(n, d) => write!(f, "{n}/{d}"),
            Const::Boolean(b) => f.write_str(if *b { "true" } else { "false" }),
            Const::Unit => f.write_str("{}"),
        }
    }
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inst::Const { dst, value, ty } => write!(f, "{dst} = const {ty} {value}"),
            Inst::Call {
                dst,
                callee,
                args,
                return_type: _,
            } => {
                if let Some(d) = dst {
                    write!(f, "{d} = ")?;
                }
                write!(f, "call {callee}(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(")")
            }
            Inst::TupleLit { dst, fields, .. } => {
                write!(f, "{dst} = tuple_lit {{")?;
                for (i, (name, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {v}")?;
                }
                f.write_str("}")
            }
            Inst::TupleField {
                dst,
                src,
                field_name,
                ..
            } => write!(f, "{dst} = field {src}.{field_name}"),
            Inst::TupleBox {
                dst,
                src,
                heading_id,
            } => write!(f, "{dst} = tuple_box heading_{} {src}", heading_id.0),
            Inst::TupleUnbox {
                dst,
                src,
                heading_id,
            } => write!(f, "{dst} = tuple_unbox heading_{} {src}", heading_id.0),
            Inst::RelationLit {
                dst,
                tuples,
                heading_id,
            } => {
                write!(f, "{dst} = relation_lit heading_{} {{", heading_id.0)?;
                for (i, v) in tuples.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("}")
            }
            Inst::SequenceLit {
                dst,
                elements,
                heading_id,
            } => {
                write!(f, "{dst} = sequence_lit heading_{} [", heading_id.0)?;
                for (i, v) in elements.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Inst::Retain { src } => write!(f, "retain {src}"),
            Inst::Release { src } => write!(f, "release {src}"),
            Inst::WriteRelation { rel, heading_id } => {
                write!(f, "write_relation {rel} heading_{}", heading_id.0)
            }
            Inst::ScalarOp {
                dst,
                op,
                operand_type: _,
                lhs,
                rhs,
            } => write!(f, "{dst} = scalar_op {op} {lhs} {rhs}"),
            Inst::CharToText { dst, src } => write!(f, "{dst} = char_to_text {src}"),
            Inst::AttrLoad {
                dst,
                src,
                offset,
                attr_type,
            } => write!(f, "{dst} = attr_load {src}+{offset} : {attr_type}"),
            Inst::AttrStore {
                record,
                offset,
                value,
                attr_type,
            } => write!(f, "attr_store {record}+{offset} = {value} : {attr_type}"),
            Inst::Where {
                dst,
                src,
                predicate_linkage,
                heading_id,
            } => write!(
                f,
                "{dst} = where {src} by {predicate_linkage} heading_{}",
                heading_id.0
            ),
            Inst::Extend {
                dst,
                src,
                helper_linkage,
                src_heading_id,
                result_heading_id,
            } => write!(
                f,
                "{dst} = extend {src} by {helper_linkage} heading_{} -> heading_{}",
                src_heading_id.0, result_heading_id.0
            ),
            Inst::Project {
                dst,
                src,
                src_heading_id,
                result_heading_id,
            } => write!(
                f,
                "{dst} = project {src} heading_{} -> heading_{}",
                src_heading_id.0, result_heading_id.0
            ),
            Inst::Rename {
                dst,
                src,
                src_heading_id,
                result_heading_id,
                perm,
            } => write!(
                f,
                "{dst} = rename {src} heading_{} -> heading_{} perm{perm:?}",
                src_heading_id.0, result_heading_id.0
            ),
            Inst::Load {
                dst,
                src,
                heading_id,
                keys,
            } => write!(
                f,
                "{dst} = load {src} heading_{} keys{keys:?}",
                heading_id.0
            ),
            Inst::Collect {
                dst,
                src,
                heading_id,
            } => write!(f, "{dst} = collect {src} heading_{}", heading_id.0),
            Inst::Restructure {
                dst,
                src,
                src_heading_id,
                result_heading_id,
            } => write!(
                f,
                "{dst} = restructure {src} heading_{} -> heading_{}",
                src_heading_id.0, result_heading_id.0
            ),
            Inst::Join {
                dst,
                lhs,
                rhs,
                lhs_heading_id,
                rhs_heading_id,
                result_heading_id,
            } => write!(
                f,
                "{dst} = join {lhs} heading_{} {rhs} heading_{} -> heading_{}",
                lhs_heading_id.0, rhs_heading_id.0, result_heading_id.0
            ),
            Inst::Union {
                dst,
                lhs,
                rhs,
                heading_id,
            } => write!(f, "{dst} = union {lhs} {rhs} -> heading_{}", heading_id.0),
            Inst::Minus {
                dst,
                lhs,
                rhs,
                heading_id,
            } => write!(f, "{dst} = minus {lhs} {rhs} -> heading_{}", heading_id.0),
            Inst::Gate {
                dst,
                src,
                cond,
                heading_id,
            } => write!(
                f,
                "{dst} = gate {src} by {cond} -> heading_{}",
                heading_id.0
            ),
            Inst::Otherwise {
                dst,
                primary,
                fallback,
            } => write!(f, "{dst} = otherwise {primary} {fallback}"),
            Inst::TClose {
                dst,
                src,
                heading_id,
            } => write!(f, "{dst} = tclose {src} -> heading_{}", heading_id.0),
            Inst::RelCompare {
                dst,
                op,
                lhs,
                rhs,
                heading_id,
            } => {
                let name = match op {
                    RelCmpOp::Eq => "rel_eq",
                    RelCmpOp::Subset { proper: false } => "rel_subset",
                    RelCmpOp::Subset { proper: true } => "rel_proper_subset",
                };
                write!(f, "{dst} = {name} {lhs} {rhs} @ heading_{}", heading_id.0)
            }
            Inst::Extract {
                dst,
                src,
                heading_id,
            } => write!(f, "{dst} = extract {src} heading_{}", heading_id.0),
            Inst::RelvarSlotInit { name, heading_id } => {
                write!(f, "relvar_slot_init {name} heading_{}", heading_id.0)
            }
            Inst::RelvarSlotRelease { name } => write!(f, "relvar_slot_release {name}"),
            Inst::RelvarRead {
                dst,
                name,
                heading_id,
            } => write!(f, "{dst} = relvar_read {name} heading_{}", heading_id.0),
            Inst::PrivateRelvarSlotInit { name, heading_id } => {
                write!(
                    f,
                    "private_relvar_slot_init {name} heading_{}",
                    heading_id.0
                )
            }
            Inst::RelvarSlotStore { name, value } => {
                write!(f, "relvar_slot_store {name} {value}")
            }
            Inst::RegisterDatabase => f.write_str("register_database"),
            Inst::RegisterPlan { plan_id } => write!(f, "register_plan plan_{plan_id}"),
            Inst::Query {
                dst,
                plan_id,
                params,
                rels,
                heading_id,
            } => {
                write!(f, "{dst} = query plan_{plan_id} heading_{} (", heading_id.0)?;
                for (i, (v, _ty)) in params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")?;
                if !rels.is_empty() {
                    f.write_str(" rels (")?;
                    for (i, (v, hid)) in rels.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{v} heading_{}", hid.0)?;
                    }
                    f.write_str(")")?;
                }
                Ok(())
            }
            Inst::Dml { plan_id, params } => {
                write!(f, "dml plan_{plan_id} (")?;
                for (i, (v, _ty)) in params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            Inst::InsertFrom {
                plan_id,
                src,
                heading_id,
            } => write!(
                f,
                "insert_from plan_{plan_id} {src} heading_{}",
                heading_id.0
            ),
        }
    }
}

impl fmt::Display for ScalarOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ScalarOp::Eq => "eq",
            ScalarOp::NotEq => "ne",
            ScalarOp::Lt => "lt",
            ScalarOp::Gt => "gt",
            ScalarOp::LtEq => "le",
            ScalarOp::GtEq => "ge",
            ScalarOp::And => "and",
            ScalarOp::Or => "or",
            ScalarOp::Not => "not",
            ScalarOp::Add => "add",
            ScalarOp::Sub => "sub",
            ScalarOp::Mul => "mul",
            ScalarOp::Div => "sdiv",
            ScalarOp::RatioFromInts => "ratio_from_ints",
            ScalarOp::RationalAdd => "rat_add",
            ScalarOp::RationalSub => "rat_sub",
            ScalarOp::RationalMul => "rat_mul",
            ScalarOp::RationalDiv => "rat_div",
            ScalarOp::Concat => "concat",
        })
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::Return(None) => f.write_str("return"),
            Terminator::Return(Some(v)) => write!(f, "return {v}"),
            Terminator::CondBr {
                cond,
                then_block,
                else_block,
            } => write!(f, "condbr {cond} -> {then_block}, {else_block}"),
            Terminator::Br { target, args } => {
                write!(f, "br {target}")?;
                if !args.is_empty() {
                    f.write_str("(")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{a}")?;
                    }
                    f.write_str(")")?;
                }
                Ok(())
            }
            Terminator::Unreachable => f.write_str("unreachable"),
        }
    }
}

impl fmt::Display for BasicBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.params.is_empty() {
            writeln!(f, "    {}:", self.id)?;
        } else {
            write!(f, "    {}(", self.id)?;
            for (i, (v, ty)) in self.params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{v}: {ty}")?;
            }
            writeln!(f, "):")?;
        }
        for inst in &self.insts {
            writeln!(f, "        {inst}")?;
        }
        write!(f, "        {}", self.terminator)
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_extern() {
            f.write_str("  extern fn ")?;
        } else {
            f.write_str("  fn ")?;
        }
        // For an extern, the linkage name *is* the visible identity.
        // For a defined function the surface name is what reads
        // naturally — debugging text, not the linker symbol.
        if self.is_extern() {
            write!(f, "{}", self.linkage_name)?;
        } else {
            write!(f, "{}", self.name)?;
        }
        f.write_str("(")?;
        for (i, (pname, pty)) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{pname}: {pty}")?;
        }
        write!(f, ") -> {}", self.return_type)?;
        if self.is_extern() {
            return Ok(());
        }
        f.write_str(" {\n")?;
        for block in &self.blocks {
            writeln!(f, "{block}")?;
        }
        f.write_str("  }")
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "module {} {{", self.program_name)?;
        for (i, h) in self.headings.iter().enumerate() {
            writeln!(f, "  heading_{i} = {h}")?;
        }
        if !self.headings.is_empty() {
            writeln!(f)?;
        }
        for r in &self.public_relvars {
            writeln!(
                f,
                "  public_relvar {} : heading_{} table \"{}\"",
                r.name, r.heading_id.0, r.table_name
            )?;
        }
        if !self.public_relvars.is_empty() {
            writeln!(f)?;
        }
        for p in &self.plans {
            writeln!(
                f,
                "  plan plan_{} db \"{}\" heading_{} params {} sql \"{}\"",
                p.plan_id, p.db_name, p.result_heading_id.0, p.param_count, p.sql
            )?;
        }
        if !self.plans.is_empty() {
            writeln!(f)?;
        }
        for func in &self.functions {
            writeln!(f, "{func}")?;
        }
        f.write_str("}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extern_write_line() -> Function {
        Function {
            name: "write_line".to_string(),
            linkage_name: "coddl_write_line".to_string(),
            params: vec![("message".to_string(), ProcType::Text)],
            return_type: ProcType::Unit,
            blocks: Vec::new(),
        }
    }

    fn defined_main() -> Function {
        Function {
            name: "main".to_string(),
            linkage_name: "main".to_string(),
            params: Vec::new(),
            return_type: ProcType::Unit,
            blocks: vec![BasicBlock {
                id: BlockId(0),
                params: Vec::new(),
                insts: vec![
                    Inst::Const {
                        dst: ValueId(0),
                        value: Const::Text(b"Hello, world!".to_vec()),
                        ty: ProcType::Text,
                    },
                    Inst::Call {
                        dst: None,
                        callee: "coddl_write_line".to_string(),
                        args: vec![ValueId(0)],
                        return_type: ProcType::Unit,
                    },
                ],
                terminator: Terminator::Return(None),
            }],
        }
    }

    #[test]
    fn module_display_round_trips_simple_extern() {
        let m = Module {
            program_name: "hello_world".to_string(),
            functions: vec![extern_write_line()],
            headings: Vec::new(),
            public_relvars: Vec::new(),
            db_path_default: None,
            db_name: None,
            plans: Vec::new(),
            private_relvar_slots: Vec::new(),
        };
        let text = format!("{m}");
        assert!(text.starts_with("module hello_world {"));
        assert!(text.contains("extern fn coddl_write_line(message: Text) -> Unit"));
        assert!(text.ends_with("}"));
    }

    #[test]
    fn module_display_includes_basic_block_label() {
        let m = Module {
            program_name: "hello_world".to_string(),
            functions: vec![extern_write_line(), defined_main()],
            headings: Vec::new(),
            public_relvars: Vec::new(),
            db_path_default: None,
            db_name: None,
            plans: Vec::new(),
            private_relvar_slots: Vec::new(),
        };
        let text = format!("{m}");
        assert!(text.contains("block_0:"), "no block label in:\n{text}");
        assert!(text.contains("%0 = const Text \"Hello, world!\""));
        assert!(text.contains("call coddl_write_line(%0)"));
        assert!(text.contains("return"));
    }

    #[test]
    fn value_id_renders_with_percent_prefix() {
        assert_eq!(ValueId(0).to_string(), "%0");
        assert_eq!(ValueId(42).to_string(), "%42");
    }

    #[test]
    fn proctype_display_covers_all_variants() {
        // Match force: if a variant is added without a Display arm,
        // this match becomes non-exhaustive and the test stops
        // compiling.
        for ty in [
            ProcType::Integer,
            ProcType::Rational,
            ProcType::Approximate,
            ProcType::Text,
            ProcType::Character,
            ProcType::Binary,
            ProcType::Byte,
            ProcType::Boolean,
            ProcType::Unit,
            ProcType::Pointer,
            ProcType::Tuple(Heading::empty()),
            ProcType::Relation(HeadingId(0)),
        ] {
            let s = ty.to_string();
            assert!(!s.is_empty());
            assert!(s.chars().next().unwrap().is_ascii_uppercase());
        }
    }

    #[test]
    fn inst_display_call_with_args() {
        let inst = Inst::Call {
            dst: Some(ValueId(2)),
            callee: "do_thing".to_string(),
            args: vec![ValueId(0), ValueId(1)],
            return_type: ProcType::Integer,
        };
        assert_eq!(inst.to_string(), "%2 = call do_thing(%0, %1)");

        let void_call = Inst::Call {
            dst: None,
            callee: "noop".to_string(),
            args: Vec::new(),
            return_type: ProcType::Unit,
        };
        assert_eq!(void_call.to_string(), "call noop()");
    }
}

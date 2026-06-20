# Risks worth deciding early

A short list of design decisions where the cost of getting it wrong is much higher than the cost of deciding deliberately. Each one is open — the doc captures the question, the principal tradeoff, and the trigger that forces the decision (so it doesn't get deferred forever).

When one of these is resolved, it graduates out of this file into the topic doc it belongs in (and into [conformance.md](conformance.md) if the resolution is binding).

## 1. Materialization thresholds

First-class relations mean the runtime constantly chooses between in-memory and temp-table representation. The threshold (size-based, attribute-shape-based?) determines a huge amount of runtime behavior.

**Decide before** the runtime allocator is written. Pick a default policy (size-based, with an explicit `@materialize` / `@stream` annotation as escape hatches), even if the threshold values themselves get tuned later.

## 2. How honest about SQL are you willing to be?

Operators-as-relations (see [relir.md](relir.md)) makes surface `extend`/`where`/`summarize` all reduce to JOIN at the A level, which is push-down-friendly — but pushing down requires SQL-expressible scalar functions.

Start by pushing pure-relational algebra; evaluate scalar UDFs in the in-process runtime library unless they have a known SQL equivalent. The aggressive option — registering Coddl operators as SQLite UDFs to extend the pushdown surface — is real but Postgres-asymmetric (SQLite makes it easy, Postgres requires C-loaded extensions).

**Decide before** scalar UDFs are first pushed to a backend. Either commit to the simple cut (UDFs always in-process) or commit to SQLite-callable UDFs and accept the dialect asymmetry.

## 3. Possrep canonicalization

RM Pre 8's "indistinguishable" rule means a user-defined type with a non-canonical possrep (e.g., `Rational { N: N, D: D }` without a coprime constraint; polar `Point { R: R, θ: θ }` for the origin allowing any θ) breaks equality.

The compiler must require possrep constraints that force a canonical form, or refuse to synthesize `=` and warn loudly.

**Decide before** shipping user-defined types: is canonicalization the user's responsibility (require, refuse otherwise) or the system's (rewrite to canonical form behind the scenes)?

## 4. Transition constraint pre-image capture

VSS 4's primed-relvar syntax requires the runtime to keep a snapshot of every relvar touched within a statement until the constraint check completes. For multi-relvar transitions this is non-trivial.

**Decide before** adding VSS 4 to the runtime: is the snapshot row-level (delta sets) or relvar-level (copy-on-write)?

## 5. The Assignment Principle for views

RM Pre 21: inserting into a view must fail if the inserted tuple wouldn't appear in the view. Generically computing this from a virtual-relvar definition is hard; the Manifesto allows the system to refuse views it can't update.

**Decide early**: which view shapes Coddl will accept updates against, which it will reject at definition time, which it will accept and check at runtime.

## 6. Heading polymorphism design space

VSS 7 is deferred for v1 (see [conformance.md](conformance.md)), but the type system must keep headings first-class so that future row-polymorphic operator signatures don't require a rewrite.

Don't bake monomorphic dispatch into the IR; allow heading-typed parameters at the type-rep level even if no surface syntax yet exposes them.

## 7. Specialize vs. runtime-plan

Specializing relation-polymorphic functions on heading at compile time keeps things simple but can blow up code size in pathological cases.

**Decide early**: have the runtime planner (see [runtime.md](runtime.md), "Reaching the engines") ready from the start so you can fall back when specialization isn't viable.

## 8. FFI struct-layout single source of truth

ProcIR's tuple/value layout, the Rust runtime's `#[repr(C)]` types, and the LLVM IR text the compiler emits all describe the same memory. Drift between them is silent at compile time and a debug nightmare at runtime.

Build a single layout description (a Rust type with derives that generates both the LLVM struct emission and the matching `#[repr(C)]` declaration) before the second value type lands. Same for the tagged-union row representation. This is a long-term-planning bill we pay now or pay tenfold later.

## 9. `Integer` precision and arithmetic cost

TTM's `INTEGER` (Coddl's `Integer`) is mathematically unbounded; shipping it as the only integer built-in forces bignum arithmetic on what 99% of users will use as a machine int.

**Decide before** user-defined possrep machinery ships: keep `Integer` unbounded and lean on user-defined `Int32`/`Int64`, or add bounded built-ins at the cost of one more documented type. The performance principle leans toward bounded built-ins; the conformance principle leans toward keeping the TTM-derived name unbounded.

## 10. Non-SQL backends: generalizing `Backend::emit_select`

The storage abstraction (see [storage.md](storage.md)) hard-codes SQL: `Backend::emit_select(plan) -> SqlString` plus the `Dialect` enum assume every backend consumes SQL text. SQLite and Postgres fit; MongoDB (aggregation pipelines), Neo4j (Cypher), and other non-relational stores don't — and even with a generalized emitter, only their fixed-schema, null-free subset maps to a relvar (RM Pro 4). A flat source like CSV has no query engine at all: it can only feed the in-process engine as a materialized relation (no pushdown), which is the eager-hydration cost the SQL path exists to avoid.

The expensive-to-retrofit layer is already protected: RelIR is backend-agnostic — a leaf is rooted in a *logical database*, and `StorageOrigin` is a pushable-or-not flag carrying no backend kind or dialect (see [relir.md](relir.md)). A future non-SQL backend is therefore a backend-layer change, not an IR rewrite; the one remaining SQL-ism is the single `emit_select -> SqlString` signature, cheap to change while few `Backend` impls exist.

**Decide at the second backend** (Postgres), or whenever a non-SQL backend first becomes a goal — whichever comes first. Don't generalize the return type before then: with no working backend yet there is no concrete second example to design the abstraction against, and abstracting on imagined requirements reliably yields the wrong shape. Until then the SQL assumption stays localized in [`coddl-sqlemit`](sqlemit.md), the backend crates, and the runtime's prepared-statement path; the IR and the cut stay agnostic.

## 11. Decisions surfaced by the audit but not yet on this list

A recent docs audit flagged several questions worth tracking here once they harden into real risks rather than open questions:

- **Text collation**: byte-equality vs Unicode-equality for `Text` `=`. SQLite collations vs Postgres collations diverge.
- **Approximate IEEE-754 strictness**: which arithmetic guarantees does `Approximate` give? Coddl-defined or backend-defined?
- **Sum-type scalar mechanics**: the doc mentions sum-type scalars as "permitted but not recommended" for missing information (vertical decomposition preferred). The actual selector + accessor + matching mechanism for sums isn't designed yet.
- **`oper` declaration surface**: used in examples and method-call sugar, but the production isn't fully spelled out in [grammar.md](grammar.md) yet.

These are tracked here so they don't slip out of view. Move them into properly-scoped risks (with decide-before triggers) when one of them becomes a near-term concern.

## 12. RC dealloc size vs. sealed length

`coddl_rc_alloc` sizes a relation payload to its pre-seal row count, but `coddl_rc_release` recomputes the dealloc `Layout` from the header's `length` field — which `coddl_relation_seal` *shrinks* when it dedups. So any path that allocates N rows, seals down to M < N, then releases, frees a block allocated for N rows with a `Layout` for M — a size-mismatched `dealloc` (UB on most allocators). Present today in `coddl_sqlite_relvar_init` and `coddl_relation_where` (both worst-case alloc then trim); `coddl_query` inherits the same `finalize_relation` shape. `SELECT DISTINCT` makes shrinkage unlikely on the pushed path (rows arrive unique), so it isn't reliably triggered — but it's a real latent fault, not a theoretical one.

**Decide before** the data layer is considered production-hardened (and certainly before any non-`DISTINCT` or duplicate-producing path ships): either store the *allocated* row count in the header separately from the logical `length` (and free against the allocated count), or seal into a right-sized buffer before the RC alloc. Until then, every relation-producing path must keep the allocation's true row count recoverable at release time.

## 13. Optimizer normal form: minimal Algebra A vs. rich logical operators

When Coddl grows its own logical optimizer, what vocabulary does it rewrite over? **Minimal Algebra A** — reduce everything to the Appendix-A primitives (◄AND►/◄OR►/◄NOT►/◄REMOVE►/◄RENAME►), so the rewrite-rule set is small and uniform and equivalence/normalization is tractable (this is what A is designed for). Or a **rich logical algebra** — keep `Join`/`Project`/`Restrict`/`Rename` as first-class operators, the way production optimizers (Cascades/Volcano, Postgres) do, because each carries optimization-relevant structure (a projection's keep-set, a restriction's predicate, a join's operands) that column-pruning / predicate-pushdown / join-reordering consume directly.

RelIR today is a deliberate **hybrid**: minimal where it buys a uniform algebra — `join`/`times`/`intersect`/`compose` are all the one `RelExpr::And`, and `union`/`minus` are planned as ◄OR►/◄AND NOT►, with ◄NOT►/◄OR► reasoned about symbolically and never materialized (see [relir.md](relir.md)) — and rich where the operator carries structure the (future) optimizer and *both* emitters want: `Project` is project-*over* with a concrete keep-set (`project all but` normalizes into it), `Restrict` keeps its predicate, `Rename` its map. The decisive current factor is that both backends are high-level: SQL has `SELECT`/`JOIN`/`WHERE`, the runtime has `coddl_relation_project`/`_join`/`_where`, so emission is a direct walk of the logical operators; a fully-minimal IR would instead push *re-clustering* (REMOVEs back into a SELECT list, ANDs back into a JOIN) into `coddl-sqlemit` **and** the in-process path.

The pull toward minimal is real specifically for a **hand-written rewrite optimizer**: a small primitive set means fewer, more-uniform rules and easier equivalence proofs. The pull toward rich is that reducing `Project`/`Restrict` below their logical form discards exactly the structure those rewrites need (projection pushdown is a keep-set operation; predicate pushdown wants the predicate whole), and for relvar-rooted queries the heavy algebraic work is delegated to the backend's own optimizer anyway.

**Decide before** building the Coddl-level logical optimizer (the first non-trivial RelIR→RelIR rewrite pass beyond the origin-only cut). Until then RelIR stays the current hybrid. This is independent of operator *completeness* — `project all but` is the full ◄REMOVE► operation today regardless of which normal form the optimizer eventually adopts. **Current lean: minimal**, especially for the self-written optimizer; deferred for now.

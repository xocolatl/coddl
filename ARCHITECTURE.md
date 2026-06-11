# Coddl — Architecture Sketch

A compiler for a relational language conforming to Date and Darwen's *Third Manifesto*. Query fragments compile to SQL and run against a pluggable storage backend (SQLite first, Postgres later); everything else compiles to LLVM IR and links against a small Rust runtime exposed through C ABI.

Coddl is its own D — not Tutorial D. It conforms to TTM's RM/OO Prescriptions and Proscriptions (§3) and designs its own surface syntax, IRs, and runtime around the principles below.

**Core principles** — binding on every design choice in this document. A proposal that violates one needs an explicit override, not a quiet exception.

1. **Performance.** Runtime cost is a first-class concern. The host language (Rust), the runtime (no GC, no managed RTS in user binaries), the FFI layer (zero-copy `#[repr(C)]` values), and the IR (Algebra A — push-down-friendly) are chosen for it. Features that force unavoidable overhead the user can't opt out of are rejected. When two designs are otherwise equivalent, the one with the lower steady-state cost wins.
2. **Long-term planning.** IR shapes, type representations, and crate boundaries are designed so deferred Manifesto features (VSS 7 heading polymorphism, transition constraints, type inheritance) and unanticipated extensions land without a rewrite. No painting into corners — keep the data structures wider than current need, and the boundaries semantic rather than expedient.
3. **Conformance over convenience.** When TTM prescribes a behavior, Coddl ships it — even when a non-conforming shortcut would be easier. Sanctioned design freedoms (host language, surface syntax, evaluation strategy, IR choice) are enumerated in §3 and bounded there.
4. **Few primitives, layered sugar.** Algebra A core operators (§4); operators-as-relations; no special cases. Surface sugar — EXTEND, WHERE, SUMMARIZE — desugars during lowering. Sugar lives in one place, not woven through the IR.

## 1. Host language

**Rust** (chosen). Sum-type enums and exhaustive pattern matching are the right tools for AST/IR work; the language has no garbage collector and no managed runtime to drag into end-user binaries; FFI is `extern "C"` natively; and `#[repr(C)]` plus the borrow checker make the LLVM/runtime ABI tractable to keep correct over time. Performance and long-term-planning principles both push hard toward Rust over the GC'd alternatives we considered.

Stack:
- **Parser**: `chumsky`. Combinator-based, top-tier error reporting, evolves well as the grammar changes. `winnow` is the perf-first alternative — revisit if parser cost ever shows up in real profiles, but error quality wins at this stage of the language.
- **LLVM**: emit LLVM IR as text and shell out to `llc`/`clang`. We deliberately avoid `llvm-sys`/`inkwell` — version-coupling churn, build complexity, and we don't need programmatic IR introspection in the foreseeable plan. Text emission is fast and forward-compatible.
- **Databases**: backend-specific crates behind a trait. `rusqlite` for SQLite, `postgres` (sync) for Postgres. Avoid `sqlx`'s compile-time SQL checking — we emit our own SQL and don't want a second SQL parser in our build.
- **Pretty-printing / formatting**: the `pretty` crate for SQL and LLVM IR emission where the structure benefits from combinators; plain `std::fmt::Write` where it doesn't.
- **Build**: a single Cargo workspace, one crate per subsystem (see §8). Release builds with LTO and `codegen-units = 1` for the compiler binary; the runtime is a `staticlib` so user binaries don't take a dynamic linker hit.
- **Runtime**: a Rust crate exposing `extern "C"` symbols (see §6). Compiled Coddl binaries link against it directly — no managed runtime, no allocator surprises. The RelIR→SQL emitter (`coddl-sqlemit`) is a single crate used by both the compiler and the runtime; no duplication, no FFI seam between them.

## 2. Pipeline

```
source.cdl
   │
   ▼  lex + parse (chumsky; uniform named-argument prefix syntax — see §3)
  AST
   │
   ▼  name resolution, module/import resolution
  Resolved AST
   │
   ▼  type checking (possreps + headings; constraint inference)
  Typed AST
   │
   ▼  lowering — splits into two IRs at the relational boundary
   │
   ├──────────────► RelIR  (Algebra A core + sugar layer; see §4)
   │                  │
   │                  ▼  desugar to A core
   │                  ▼  optimize (push-down, FD-aware key inference, dead-attr elim)
   │                  │
   │                  ▼  SQL emit (dialect = backend method)
   │                  SQL string + parameter slots
   │
   └──────────────► ProcIR (SSA-ish, LLVM-shaped)
                      │
                      ▼  LLVM IR text emission (`pretty` crate)
                      │
                      ▼  llc / clang   (target triple selects native or wasm32)
                      object file → linked against libcoddl_runtime (Rust staticlib)
```

The two IRs meet only at query-call sites: ProcIR holds the relation as a `Relation` handle (see §9) plus the parameters it needs to bind; the runtime returns rows that ProcIR consumes as tuples.

Every frontend pass also returns a `Vec<Diagnostic>` alongside its (possibly partial) output — the CLI driver renders them to the terminal; `coddl-lsp` serializes them as `PublishDiagnostics` (see §12). The pipeline above is the happy path; on the unhappy path, partial results and diagnostics flow back together rather than the pipeline halting.

## 3. Conformance to the Third Manifesto

Coddl conforms to *The Third Manifesto* (Date & Darwen, 3rd ed., 2014). The RM/OO Prescriptions and Proscriptions listed below are binding on design choices throughout this document.

**Coddl is its own D.** Tutorial D is the Manifesto's reference D, useful as a study aid and prior-art benchmark, not a spec Coddl follows. Where TTM prescribes behavior, Coddl conforms. Where TTM is silent, Coddl picks the answer aligned with the core principles in the intro — convergence with Tutorial D's specific choice is incidental, not a goal. The design choices TTM doesn't dictate, and which this document fixes, are:

1. **Host language and runtime stack.** Rust + LLVM-text codegen + a C-ABI Rust runtime. See §1, §6.
2. **Surface syntax.** Uniform named-argument prefix style, in the spirit of the form the Manifesto's authors propose in ch. 5 (pp. 127–128) but never adopt. See "Surface syntax" below.
3. **Evaluation strategy.** Lazy relations, strict scalars. TTM doesn't address evaluation; this is our choice. See §9.
4. **Canonical RelIR.** Algebra A as the IR core, which the authors recommend for any industrial-strength D (Appendix A). See §4.

Anything beyond this list is *not* a sanctioned design freedom — propose explicitly and add to the list rather than slipping it in.

### Adopted (RM/OO Prescriptions and Proscriptions — non-negotiable)

- **Scalar types** carry possreps with selectors and THE_ accessors; named types are disjoint; no implicit coercion (RM Pre 1–5).
- **TUPLE H and RELATION H** are type generators with structural identity by heading (RM Pre 6–7). Tuple/relation type equality is set-equality of `{name → type}` pairs.
- **No nulls. Ever.** Missing information is handled by **vertical decomposition**: split the relvar so the absence of a fact is the absence of a tuple in a side relvar, rather than a placeholder in an attribute. This is the canonical TTM answer — see ch. 7 RM Pro 4 and exercise 7.9. The type system *permits* a user-defined sum-type scalar (e.g., an `Optional` with `Some`/`None` possreps), since arbitrary user-defined scalars are allowed, but it isn't the recommended approach and shouldn't be the first thing reached for. The SQL backend must never emit `NULL` for an attribute value, never emit `NULLABLE` columns, never use `IS NULL` predicates, and must wrap any operator that SQL would otherwise produce a null from (see §5).
- **No duplicate tuples**, **no ordinal-position semantics** for attributes or tuples, **no composite attributes** (use TUPLE-typed attributes instead), **no domain-check override**, **no internal-level constructs in source** (RM Pro 1, 2, 3, 6, 8, 9).
- **No tuple-at-a-time operations on relvars or relations** (RM Pro 7). Iteration over a relation is only available via the `LOAD` construct (§9) which orders, materializes into an array, then iterates the array — the iteration boundary forces a deliberate materialization.
- **First-class TUPLE and RELATION types**, including parameters, return values, attribute types (so relation-valued attributes are allowed) (RM Pre 6–7, 9–10, 13).
- **Compile-time type checking** (OO Pre 1). Type-constraint violations (a selector argument failing its POSSREP CONSTRAINT) remain run-time.
- **Computational completeness** (OO Pre 3). Coddl is the whole language; no host required.
- **Explicit transactions, nested transactions** (OO Pre 4, 5).
- **Aggregate identity on empty sets** (OO Pre 6) — SUM=0, AND=TRUE, OR=FALSE, etc.
- **Relvars are not domains; no pointer attributes** in database relvars (OO Pro 1, 2).
- **Observational equality** (RM Pre 8): two values are equal iff indistinguishable under every operator on their type.
- **Multiple assignment** with the Manifesto's stated semantics (RM Pre 21): expand sugar; fold duplicate targets via WITH; evaluate all RHSs; assign atomically; check database constraints at the end of the whole MA (not per individual assignment, not at COMMIT).
- **Database constraints checked at statement boundaries** (not deferred to COMMIT) (RM Pre 23).
- **The Assignment Principle for views** — an INSERT into a view must fail if the inserted tuple would not appear in the view's defining expression (RM Pre 21).
- **The catalog is itself a set of relvars** — metacircular, queryable by ordinary relational expressions (RM Pre 25).

### Adopted (RM Very Strong Suggestions worth committing to in v1)

- **System keys** (VSS 1): `DEFAULT` operator-invocation clauses, a relational `TAG` operator (window-function lowering: `ROW_NUMBER() OVER (PARTITION BY …)`), nonupdatable system-default attributes.
- **Candidate-key inference** (VSS 3), minimally: propagate FDs through project/equijoin/restrict and surface inferred keys to the catalog. Best-effort.
- **Transition constraints** (VSS 4): primed-relvar syntax (`S'`) in `CONSTRAINT` bodies; pre-image captured by the runtime over delta sets, not by SQL triggers.
- **Quota queries** (VSS 5): `RANK r BY (DESC attr AS rankcol)` desugaring at the parser, lowering to `RANK()`/`DENSE_RANK()` window functions.

### Not adopted (matching the current Manifesto edition)

- **Foreign-key shorthand** (former VSS 2) — the authors formally deleted this VSS in later editions, and Coddl follows suit. Users write the general subset-constraint form directly: `CONSTRAINT SP{S#} ⊆ S{S#}` (RM Pre 23). This is what FK shorthand desugared to anyway, and it sidesteps the positional-matching example the authors regretted.

### Deferred to a later milestone

- **Generalized transitive closure** (VSS 6) — depends on VSS 7. Ship plain `TCLOSE` first.
- **User-defined heading-polymorphic operators** (VSS 7). Design the type system so adding row/heading polymorphism later doesn't force a rewrite: keep headings first-class in the type representation, don't hardwire monomorphic dispatch.
- **Type inheritance** (OO Pre 2, IM Pres). Conditional in the Manifesto. Coddl omits inheritance in v1; if added, it conforms to Part IV of the Manifesto in full.

### Skipped

- **SQL migration** (VSS 8). Out of scope for v1. Influence on the design is limited to: keep the type system extensible enough to add a parallel `SQL_*` type family later, and keep built-in operator names addressable (don't hardwire `=` to one type).

### Surface syntax

Tutorial D's own authors observe (ch. 5, "A Remark on Syntax", pp. 127–128) that Tutorial D's operator syntax "is not very consistent" — mixed prefix/infix, positional matching that "violates the spirit, if not the letter, of RM Proscription 1." They sketch a uniform style they prefer but stop short of adopting: prefix for everything, argument matching by name, braces for argument bundles:

```
CARTESIAN { Y 2.5, X 5.0 }     -- not CARTESIAN ( 5.0, 2.5 )
JOIN      { left R, right S }  -- name the slots
```

**Coddl takes this as its default.** Concessions: infix forms for `=`, `<`, `+` and friends are retained (the named-prefix form is clumsy for ubiquitous dyadic ops on identifier-unfriendly names); a small set of monadic operators (`COUNT`, `SIN`, `IS_*`) keep parenthesized positional form. Everything else — including the relational algebra, selectors, EXTEND, SUMMARIZE, GROUP, UNGROUP — is named-prefix with braces. This eliminates the relational-algebra/scalar-op syntactic distinction the authors regret, and matches RM Pro 1 (no ordinal-position semantics) at the surface where it's easiest to enforce.

## 4. Two IRs, one boundary

### RelIR — Algebra A core with a sugar layer

The Manifesto's authors argue (Appendix A) that any industrial-strength D should be *mappable to* Algebra A — a foundational set of primitives in the spirit of predicate logic — even if surface syntax uses higher-level operators. Coddl takes that seriously: **RelIR's core is Algebra A**, and surface operators are sugar that desugars during the lowering pass.

**A core**: `AND` (natural join, generalizes TIMES and INTERSECT), `OR` (heading-agnostic union), `NOT` (relational complement), `REMOVE` (project-away one attribute — existential elimination), `RENAME`, plus `TCLOSE`. Minimally these reduce further to `REMOVE` + `NOR` (or `NAND`) + `TCLOSE`, but the seven above are the practical primitives.

**Sugar layer** (desugars to A core): `Project`, `Restrict (WHERE)`, `Join`, `Union`, `Minus`, `Intersect`, `SemiJoin`, `SemiMinus`, `Extend`, `Summarize`, `Group`, `Ungroup`, `Wrap`, `Unwrap`. Crucially, **operators are themselves relations** in the A formulation: a scalar function `f(X, Y) -> Z` is an (n+1)-ary relcon `F{X, Y, Z}`, and `EXTEND r ADD (X+Y AS C)` desugars to `r JOIN (PLUS RENAME(X AS A, Y AS B, Z AS C))`. `WHERE`-clauses similarly desugar to JOINs against constant relations. This collapses much of the operator zoo into pure JOIN-and-REMOVE, which is what the optimizer actually wants.

Every RelIR node carries:
- a **heading** (RM Pre 9): `{attribute → declared type}`
- an **FD set** for candidate-key inference (VSS 3)
- a **constraint set** for constraint inference (RM Pre 23): the boolean predicates known to hold on the relation's tuples
- a **storage origin** flag: rooted in relvars (push to SQL) vs. rooted in materialized values (in-process executor) vs. mixed

The optimizer's job is to draw the SQL-vs-in-process cut as close to the leaves as possible (see §9).

### ProcIR — procedural SSA IR

SSA blocks with typed values, plus a small set of relation-aware ops:
- `query(plan_id, [params...]) -> Relation`
- `load(Relation, OrderSpec) -> Array<Tuple>` — the only sanctioned iteration path
- `assign_relvar(name, plan_id, [params...])` (relational assignment)
- `multi_assign([(target, plan_id, params)…])` — atomic, MA semantics per RM Pre 21
- `begin_tx / commit_tx / rollback_tx`

These lower to calls into the runtime ABI. There is no `force` op: relation expressions evaluate on each use against current relvar state; explicit materialization, when wanted, is `LOAD ARRAY` or assignment to a temporary relvar.

**Backend-agnostic by design.** ProcIR is shaped for SSA codegen in general, not LLVM specifically — a long-term-planning concession that costs little now and preserves room to add backends without rewriting the IR. The IR carries no LLVM-specific intrinsic names, metadata, or calling conventions at the node level; per-backend specifics live in the codegen crate (§8).

- **LLVM IR text (v1).** Emit text, shell out to `llc`/`clang`. The same emitter covers native targets (x86-64, aarch64) *and* `wasm32-*` via the target triple — WASM-via-LLVM is essentially free at the codegen layer.
- **Cranelift (planned).** Both IRs are SSA with the same value-model surface; the lowering is largely a different printer over the same ProcIR walk. Use cases: REPL JIT for fast query iteration, and toolchain-free AOT for deployments that don't want `clang` in the image.
- **Direct WASM via `wasm-encoder` (optional).** Worth keeping the door open for browser/wasmtime targets that don't want LLVM at all in the build. Lower priority than Cranelift; revisit when the use case lands.

Runtime portability is the harder half — see §6 and §8 (Cargo features) for how the SQL backends get gated out of `wasm32-*` builds.

## 5. Storage abstraction

A pair of Rust traits, one for the (pure) SQL-emitting half and one for the (effectful) connection half:

```rust
trait Backend {
    type Conn: Conn;
    fn dialect(&self) -> Dialect;
    fn emit_select(&self, plan: &RelPlan) -> SqlString;
    fn emit_ddl(&self, schema: &Schema) -> Vec<SqlString>;
    fn type_map(&self) -> &TypeMap;                       // CoddlType ↔ SQL type
    fn open(&self, dsn: &Dsn) -> Result<Self::Conn>;
}

trait Conn {
    fn prepare(&mut self, sql: &SqlString) -> Result<StmtId>;
    fn bind_and_step<'a>(&'a mut self, id: StmtId, params: &[Value]) -> Result<RowIter<'a>>;
    fn materialize_temp(&mut self, heading: &Heading, rows: &[Tuple]) -> Result<TempRelRef>;
}
```

Crates: `coddl-backend-sqlite`, `coddl-backend-postgres`. Selection is a Cargo-feature on the runtime crate; the LLVM-emitted binary links against exactly one runtime that wraps the chosen `Conn`. If passing backends around as values gets clumsy with the associated-type trait, switch to a `dyn`-friendly `BackendOps` record-of-fn-pointers — the per-call dispatch cost is negligible against query latency. Decide once the second backend lands. Cargo features also gate SQL backends out of `wasm32-*` builds where the C dependencies of `rusqlite`/`postgres` don't link (see §6).

Keep SQL emission to a **portable subset** (CTEs, window functions, standard joins) and isolate dialect divergence behind backend methods. Golden-file tests per backend: `RelIR plan → expected SQL` for each dialect.

### Mandatory SQL emission rules (Manifesto-driven)

These are not optimizations; they're correctness requirements imposed by the Manifesto's proscriptions. The emitter enforces all of them by construction:

| Rule | Reason |
|---|---|
| `SELECT DISTINCT` on every projection; `UNION` never `UNION ALL` | RM Pro 3 (no duplicates). |
| Always enumerate columns explicitly in a deterministic (name-sorted) order. Never emit `SELECT *`. Never emit `INSERT … VALUES` without a column list. Never emit bare `UNION`/`INTERSECT`/`EXCEPT` — use `… CORRESPONDING …` (or simulate by aligning explicit lists). | RM Pro 1 (no ordinal attribute order). |
| Never declare a column `NULL`; always `NOT NULL`. Reject SQL DDL paths that would allow nullable columns. | RM Pro 4 (no nulls). |
| Outer joins are forbidden in lowered SQL. Coddl source has no construct that compiles to one; the type system can't express "this attribute might not have a value" as an attribute property. | RM Pro 4. |
| Aggregates: wrap to honor identity (OO Pre 6). Emit `COALESCE(SUM(x), 0)`, `COALESCE(MAX(x), CAST(<lowest> AS T))`, etc. AVG over empty is undefined — emit a guarded expression that signals an error if the result would be queried. | OO Pre 6. |
| Relational assignment `R := expr` compiles inside a transaction to `DELETE FROM R; INSERT INTO R (…) SELECT … FROM (…)` (or `TRUNCATE` + `INSERT` on Postgres). Single-tuple INSERT/UPDATE/DELETE in source desugars to a relational-assignment expression first; the backend never sees the singular form. | RM Pre 21, RM Pro 7. |
| Always emit explicit `BEGIN` / `COMMIT`. Never rely on SQL's implicit transaction start. Set constraints `IMMEDIATE` at session start; never `INITIALLY DEFERRED`. | OO Pre 4; RM Pre 23 (statement-boundary check). |
| Avoid SQL `CHARACTER` / `CHAR(n)` entirely; use `VARCHAR`/`TEXT`. SQL's `CHAR` pads with trailing blanks under equality — violates RM Pre 8. | RM Pre 8. |
| Every base table emitted from a relvar has a `PRIMARY KEY` from the relvar's declared candidate key (RM Pre 15). The candidate key with the fewest attributes wins ties; the rest become `UNIQUE`. The compiler verifies minimality before emission. | RM Pre 15. |
| `TABLE_DEE` / `TABLE_DUM` (nullary relations): emit as `(SELECT) WHERE TRUE` / `WHERE FALSE`. SQLite/Postgres tolerate this; non-conforming backends would need a synthesized dummy column. | RM Pro 5. |
| SQLite-specific: `BOOLEAN` lowers to `INTEGER CHECK (col IN (0, 1))`. Avoid the SQLite affinity-coercion footguns by always `CAST`-ing on `INSERT`. | dialect quirk. |

### Sending in-memory relations back into SQL

Same as before (§9, "Relations flowing back into SQL"): backend method `materializeIntoTemp` ships an in-memory relation to a temp table the next query can reference like a relvar. SQLite: temp tables / `carray`. Postgres: temp tables / `UNNEST` for small / `COPY` for large.

## 6. Runtime (`libcoddl_runtime`)

A Rust crate exposing `extern "C"` entry points, built as a `staticlib` by default (`cdylib` later if plugin loading lands). Compiled Coddl binaries link against it directly — no managed runtime, no garbage collector, no startup overhead beyond the program's own. Responsibilities:
- Own the DB connection pool.
- Cache prepared statements by `plan_id` (compiler assigns at codegen time).
- Marshal LLVM-side value structs ↔ backend parameter binders. `#[repr(C)]` Rust structs match the layout LLVM emits exactly; no marshaling cost beyond field reads, no FFI shim allocation. A single source-of-truth description (see §10 risk #8) generates both the LLVM struct text and the Rust `#[repr(C)]` declaration so they can't drift.
- Provide a row iterator the LLVM-emitted code can drive (cursor handle + `coddl_next` returning a tagged-union row).
- Host the in-process RelIR executor (§9) and the RelIR→SQL emitter (the same crate the compiler uses, `coddl-sqlemit` — no duplication, no FFI seam between compiler and runtime).
- Map errors to a single error code + thread-local message.

LLVM IR calls these exports as plain C functions. The runtime is where SQLite vs Postgres lives at runtime — the compiled program is backend-agnostic if we're disciplined about not leaking dialect-specific values through the ABI.

**Performance posture.** The runtime is on the hot path for every relation operation that crosses the SQL/in-process boundary. Allocate per-query with a bump arena; free at query completion (a typed arena per heading is the natural unit). Avoid `Box<dyn Trait>` on tuple values; specialize over heading or use a fixed-size value layout. Pull row buffers from prepared statements directly into Coddl tuple memory where the dialect permits — zero-copy is the default, copy only when alignment or lifetime forces it. Abort-on-panic (`panic = "abort"`) for release builds: smaller stack-unwinding tables and a single failure mode at the FFI boundary.

**FFI boundary discipline.** Values crossing into LLVM-emitted code are `#[repr(C)]` or primitive. No Rust enums-with-payload across the boundary unless tagged-C-style. No `Vec`/`String` raw pointers without an explicit owner declaration. The discipline is enforced by a single layout-description module in the runtime crate, mirrored from there into LLVM codegen.

**Portability and backends as features.** SQL backends are Cargo features on the runtime crate (`sqlite`, `postgres`). `wasm32-*` builds drop these — the C dependencies of `rusqlite`/`postgres` don't link to wasm32-unknown-unknown — and either run with only the in-process executor (materialized relations, no DB) or proxy SQL through wasm host imports if a wasmtime/JS host is in play. Same crate split, different feature set at build time.

**Why Rust over plain C for the runtime.** A C `libcoddl_runtime` would be ~50–300 KB smaller as a `staticlib`; nothing else recommends it for our case. The two non-trivial runtime jobs — the in-process RelIR executor and the RelIR→SQL emitter — are tree walks over sum types, which Rust enums + pattern matching handle naturally and C reinvents painfully. The SQL emitter is the same crate the compiler uses; a C runtime would either duplicate it (two versions to keep in lockstep forever — against long-term planning) or call into a Rust crate (a Rust runtime with extra steps). Connection pooling and prepared-statement caching are markedly less code against `rusqlite`/`postgres` than against `sqlite3.h`/`libpq-fe.h`. Where binary size or non-Rust embedding ever does matter, the hot value-marshaling layer can drop to `#![no_std]` Rust or a small C TU without touching the executor or emitter — picking Rust now doesn't lock out a leaner future.

## 7. Type system

### Scalar types

A scalar type is a named, finite set of values disjoint from every other scalar type. Each user-defined scalar type carries one or more **possible representations** (possreps) — abstract representations made up of named, typed components — and a (possibly trivial) `CONSTRAINT` predicate that defines which possrep tuples denote real values of the type (RM Pre 4–5, p. 144–151).

For every possrep `PR` of type `T` the system synthesizes:
- A **selector** of declared type `T`, one parameter per component (selector name = possrep name). Every value of `T` must be producible by an all-literal selector invocation.
- A **THE_C accessor** per component `C`: read-only in source position; pseudovariable in target position (`THE_C(V) := x` is sugar for `V := PR(…, x in slot C, …)`).

**Type constraints** (the `POSSREP CONSTRAINT` predicate) are checked at every selector invocation — that's the sole choke point because values of `T` can only be constructed via the selector. Type-constraint violations are run-time errors; argument-type mismatches are compile-time.

**Built-in scalar types (v1)**: `INTEGER`, `RATIONAL`, `CHARACTER`, `CHAR`, `BOOLEAN`. The names happen to overlap most Ds' built-ins because TTM's Appendix C modeling exercises assume them, but the list is Coddl's choice, not borrowed. Everything else — `DATE`, `TIMESTAMP`, `UUID`, `BYTES`, fixed-width numerics, decimal, currency — is a user-defined scalar type with one or more declared possreps. This is the modeling exercise TTM Appendix C walks through; Coddl ships a small standard library of these definitions but they aren't built into the language. Each built-in has fixed mappings to (a) LLVM type, (b) SQLite affinity + `CHECK` constraints where needed, (c) Postgres type; user-defined scalars get their mappings via possrep components.

`INTEGER` is mathematically unbounded per TTM, which forces big-integer arithmetic at runtime — a real cost against the performance principle. Whether to also ship bounded-width built-ins (`INT32`/`INT64`) as primitives, or to keep them as user-defined possrep-constrained scalars over `INTEGER`, is an open decision (§10 risk #8).

**No implicit coercion.** Distinct named scalar types are disjoint; `INTEGER` and `RATIONAL` cannot be silently mixed. Equality `=` is type-monomorphic per RM Pre 8 ("indistinguishable for all operators on T").

**No nulls.** Period. The type system has no nullable-attribute facility. Missing information is a database-design problem the user solves through **vertical decomposition** — splitting the relvar so the absence of a fact is the absence of a tuple in a side relvar (the canonical TTM answer; ch. 7, RM Pro 4). A user-defined sum-type scalar (`Optional` with `Some`/`None` possreps) is permitted by the type system but not the recommended approach. The SQL backend never sees a request to emit a NULL.

### Type generators

- `TUPLE { a: T, b: U, … }` and `RELATION { a: T, b: U, … }` are type generators producing structurally-identified types: `TUPLE H1 = TUPLE H2` iff `H1 = H2` as sets of `<name, type>` pairs. Same for `RELATION`. Attribute order is immaterial. Both generators may take zero attributes (`TABLE_DEE` and `TABLE_DUM` are the only inhabitants of `RELATION { }`).
- Headings may include relation-valued and tuple-valued attributes (nesting permitted; RM Pre 6–7).
- A *relvar* is a named variable of some `RELATION H` type. Per RM Pre 14, every relvar has at least one declared candidate key (RM Pre 15), possibly the empty key (which forces cardinality ≤ 1). Coddl classifies relvars by lifetime and provenance, with one of the following kinds at declaration time — a database relvar (`REAL`/`BASE` — backed by storage; or `VIRTUAL` — a view) or an application relvar (`PRIVATE` to the running program; or `PUBLIC` — the program's view onto a slice of the database). The same four-kind classification appears in Tutorial D (ch. 5 p. 105) because the underlying distinctions are real ones, not because we're copying it.

### Relations are fully first-class

Relations can be bound to variables, passed to and returned from operators, stored in tuples, nested inside other relations, used as function arguments and results everywhere a scalar can. The calling convention treats them uniformly (see §9).

### Type inference and constraint inference

Type inference for relational expressions is mandatory and mechanical from operator semantics (RM Pre 18): every RelIR node's heading is the heading of its operands transformed by its operator. The optimizer further runs:
- **FD propagation** for candidate-key inference (VSS 3) — best-effort.
- **Constraint propagation** (RM Pre 23): predicates known to hold on operands propagate through restrict, project, join, extend, etc. Used for view-constraint checking and as optimizer hints.

### Where constraints can live

Integrity constraints attach only to **database relvars** (real, virtual). Coddl does not support constraints on application relvars (private or public), tuple variables, or scalar variables — there's "no logical reason why it should not," as TTM acknowledges (ch. 5 p. 106), but the cost in implementation complexity outweighs the payoff for the use cases we've identified so far. Revisit if a concrete need surfaces.

## 8. Project layout (Cargo workspace)

```
coddl/
  Cargo.toml                       # workspace
  crates/
    coddl-diagnostics/             # shared span + diagnostic types (used by every frontend crate)
    coddl-syntax/                  # chumsky lexer/parser, AST (with error recovery)
    coddl-types/                   # type checker, type representation
    coddl-relir/                   # relational IR + optimizer
    coddl-procir/                  # procedural IR (backend-agnostic SSA)
    coddl-sqlemit/                 # RelIR → SQL (dialect-agnostic core; used by compiler and runtime)
    coddl-execlocal/               # in-process RelIR executor over materialized relations
    coddl-backend-sqlite/          # Cargo feature on the runtime
    coddl-backend-postgres/        # Cargo feature on the runtime
    coddl-codegen-llvm/            # ProcIR → LLVM IR text emission (v1)
    coddl-codegen-cranelift/       # ProcIR → Cranelift (planned; REPL JIT + toolchain-free AOT)
    coddl-codegen-wasm/            # ProcIR → wasm-encoder (optional; revisit when needed)
    coddl-runtime/                 # extern "C" staticlib linked into compiled binaries
    coddl-driver/                  # CLI: compile, run, repl, fmt
    coddl-lsp/                     # tower-lsp language server; thin adapter over the frontend crates (see §12)
    coddl-fmt/                     # canonical formatter — same library behind `coddl fmt` and the LSP (see §13)
  editors/
    vscode/                        # VSCode extension: TextMate grammar + language client (see §12)
  tests/
    golden/                        # SQL emission goldens per backend
    e2e/                           # compile + run end-to-end
  examples/
```

Release builds: LTO on, `codegen-units = 1` for the driver and runtime crates; `panic = "abort"` for the runtime (smaller unwinding tables, single failure mode at the FFI seam). `wasm32-*` targets build the runtime with `--no-default-features` to drop the SQL backend crates.

## 9. Execution model

**Relations are lazy.** Scalars are strict. A relation expression is a thunk: it doesn't run at construction, only when something needs its tuples — iteration via `LOAD`, being shipped into another query, being assigned to a relvar, being compared with `=`, being passed to a user-defined operator that consumes it. There is **no `force` keyword** in Coddl; each use re-evaluates the expression against current relvar state. (Laziness itself is design choice #3 in §3; TTM doesn't address evaluation strategy.) Equality is by value (heading + tuple set), so two relations built by different routes that yield the same tuples are equal regardless of evaluation history (RM Pre 8).

Because relations are first-class, the calling convention has to be uniform: any function that takes a relation must accept a value it can read, re-query, and pass onward. The runtime may memoize a handle's result when it can prove the source relvars haven't changed since the previous use, but that's an optimization invisible to the user.

### Iteration: the LOAD primitive

There is no tuple-at-a-time access to relvars or relations (RM Pro 7). The only iteration primitive is `LOAD`, which forces the relation, imposes an order, and writes the tuples into a local array:

```
VAR A ARRAY TUPLE { S# S#, QTY QTY } ;
LOAD A FROM ( SP WHERE P# = P#('P1') ) { S#, QTY } ORDER ( ASC S# ) ;
DO i := 1 TO COUNT(A) ;
  -- process A[i]
END DO ;
```

`LOAD` is the syntactic and semantic gate between the set-oriented and procedural worlds: it forces the relation, imposes an order (the order is part of the operation, not a property of the relation), and writes the tuples into a local array. The array is then iterable by a counted `DO` loop. This is the *only* sanctioned path; the compiler rejects any other attempt to step through tuples one at a time.

The reverse direction — `LOAD <relvar target> FROM <array var ref>` — is also supported: it assigns the (set-valued) projection of the array's tuples back into a relvar. Useful for round-tripping procedurally-built arrays into relational form.

### Multiple assignment

`A1, A2, …, An ;` is a single statement with the semantics of RM Pre 21:
1. Expand all syntactic shorthands (INSERT/UPDATE/DELETE/THE_ pseudovariable) into `target := expr` form.
2. Fold duplicate targets by rewriting `Vq := Xq` as `Vq := WITH Xp AS Vq : Xq` and dropping the earlier assignment. Repeat.
3. Evaluate every RHS expression. Capture results.
4. Apply all assignments to their targets atomically.
5. Check every applicable database constraint at the end of the whole MA (not between assignments).

The procedural IR therefore has a `multi_assign` primitive, not just a sequence of individual `assign` calls. The runtime evaluates all RHSs first (against the pre-MA database state), then commits the writes in one logical step, then runs constraint checks.

### Transactions

`BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK` are explicit (OO Pre 4). Nested transactions are supported (OO Pre 5): a nested `BEGIN` starts a child; child `COMMIT` is conditional on the parent; child `ROLLBACK` undoes only the child's work. The SQL backend uses SAVEPOINT for child transactions, but the runtime tracks the parent/child relationship explicitly because SQL `SAVEPOINT` doesn't model true nesting.

A relation handle captured before a write within the same transaction **re-evaluates on use** and so sees post-write state — the consequence of the lazy/thunk semantics above. If the user wants to freeze the pre-write tuples, they `LOAD` the relation into an array (or assign it to a private relvar) before the write. This avoids any pre-image / copy-on-write machinery in the runtime.

### Relation values at runtime

A first-class relation is one of three things, behind a single `Relation` handle:

1. **Plan-backed** — a `plan_id` plus its already-bound parameters. The default. Each use re-evaluates against current relvar state. The runtime may memoize the result when source-relvar invalidation is provably absent, but that's an optimization, not a semantic guarantee.
2. **Materialized** — a runtime-owned buffer of tuples (arena-allocated, or a backend temp table for large ones). Used when tuples are already in memory: relation literals (`RELATION { tup1, tup2 }`), results of the in-process executor, in-memory inputs being shipped back into SQL via temp table.
3. **Cursor** — a live result set being drained. Compiler-only optimization for `LOAD ... ORDER (...)` flows where the array is consumed once and never escapes — lets the runtime stream rows from the backend into the array slot-by-slot instead of buffering them all.

Materialization strategy:
- Small (under a threshold, say 10k tuples or N bytes): in-memory arena, columnar or row, indexed on demand if hit by a join.
- Large: backend temp table (`CREATE TEMP TABLE`), so the next query that uses it can still push down.

### Relations flowing back into SQL

This is the part first-class relations make non-trivial: a relation value built or filtered in procedural code may be the input to a subsequent query. Options the runtime needs to handle, picked per backend:

- **SQLite**: register `carray`/virtual-table modules, or `CREATE TEMP TABLE` + bulk insert. Start with temp tables.
- **Postgres**: `UNNEST` over arrays for small relations; `COPY` into a temp table for larger ones; table-valued parameters via temp tables are the portable bet.

The backend trait gets one more method: `materialize_into_temp(conn, heading, rows) -> TempRelRef`, and SQL emission can reference a `TempRelRef` as if it were a relvar.

### Plan registration and execution

- Each compile-time query becomes a `plan_id` with a SQL template and parameter signature.
- Codegen registers all plans at process start: `coddl_register_plan(id, sql, param_types, result_heading)`.
- At call sites, ProcIR computes parameters (including any `TempRelRef`s built from in-memory relations), calls `coddl_exec(plan_id, params) -> Relation`, and either iterates, materializes, or hands the handle onward.

### Plans built at runtime

Because relations are first-class, you can write functions whose query shape depends on which relation is passed in. The compiler can't always pre-bake the SQL for these. Two strategies, layered:

1. **Specialize when possible.** Monomorphize over relation headings at compile time (like Rust generics): every concrete call site gets its own `plan_id`. Covers most of the practical cases.
2. **Plan-at-runtime fallback.** For genuinely dynamic relational composition, the runtime owns a small RelIR→SQL emitter (the same crate the compiler uses) and emits + prepares SQL on first call, caching by plan shape. This is why `coddl-sqlemit` must be usable as a library, not just a compiler phase.

### In-process relational executor

You also need to evaluate algebra over relations the SQL backend never saw — relations constructed in code, the results of joining a materialized relation with another materialized relation when neither came from the DB, etc. A small **in-process executor** for RelIR over materialized relations (volcano-style iterators, hash joins, sort-merge) is a required component, not optional. It's also gold for tests and the REPL.

So the runtime has two execution engines side-by-side:
- the SQL backend, for any subplan rooted in relvars,
- the in-process executor, for subplans rooted in materialized values.

The RelIR optimizer's job is to draw the line between them as low (close to the leaves) as possible: push everything that touches a relvar into SQL, do the rest in-process.

## 10. Risks worth deciding early

1. **Materialization thresholds.** First-class relations mean the runtime constantly chooses between in-memory and temp-table representation. Pick a default policy (size-based, with an explicit `@materialize` / `@stream` annotation as escape hatches) before you write the runtime allocator.
2. **How honest about SQL are you willing to be?** Operators-as-relations (§4) makes EXTEND/WHERE/SUMMARIZE all reduce to JOIN, which is push-down-friendly — but pushing down requires SQL-expressible scalar functions. Start by pushing pure-relational algebra; evaluate scalar UDFs in the in-process executor unless they have a known SQL equivalent.
3. **POSSREP canonicalization.** RM Pre 8's "indistinguishable" rule means a user-defined type with a non-canonical possrep (e.g., `RATIONAL{N, D}` without `COPROME` constraint; polar `POINT{R, θ}` for the origin allowing any θ) breaks equality. The compiler must require POSSREP CONSTRAINTs that force a canonical form, or refuse to synthesize `=` and warn loudly. Decide whether canonicalization is the user's responsibility (require, refuse otherwise) or the system's (rewrite to canonical form behind the scenes) before shipping user-defined types.
4. **Transition constraint pre-image capture.** VSS 4's primed-relvar syntax requires the runtime to keep a snapshot of every relvar touched within a statement until the constraint check completes. For multi-relvar transitions this is non-trivial; decide whether the snapshot is row-level (delta sets) or relvar-level (copy-on-write) before adding VSS 4 to the runtime.
5. **The Assignment Principle for views.** RM Pre 21: inserting into a view must fail if the inserted tuple wouldn't appear in the view. Generically computing this from a virtual-relvar definition is hard; the Manifesto allows the system to refuse views it can't update. Decide early: which view shapes Coddl will accept updates against, which it will reject at definition time, which it will accept and check at runtime.
6. **Heading polymorphism design space.** VSS 7 is deferred for v1, but the type system must keep headings first-class so that future row-polymorphic operator signatures don't require a rewrite. Don't bake monomorphic dispatch into the IR; allow heading-typed parameters at the type-rep level even if no surface syntax yet exposes them.
7. **Specialize vs. runtime-plan.** Specializing relation-polymorphic functions on heading at compile time keeps things simple but can blow up code size in pathological cases. Have the runtime planner (§9, "Plans built at runtime") ready from the start so you can fall back when specialization isn't viable.
8. **FFI struct-layout single source of truth.** ProcIR's tuple/value layout, the Rust runtime's `#[repr(C)]` types, and the LLVM IR text the compiler emits all describe the same memory. Drift between them is silent at compile time and a debug nightmare at runtime. Build a single layout description (a Rust type with derives that generates both the LLVM struct emission and the matching `#[repr(C)]` declaration) before the second value type lands. Same for the tagged-union row representation. This is a long-term-planning bill we pay now or pay tenfold later.
9. **`INTEGER` precision and arithmetic cost.** TTM's `INTEGER` is mathematically unbounded; shipping it as the only integer built-in forces bignum arithmetic on what 99% of users will use as a machine int. Decide before user-defined possrep machinery ships: keep `INTEGER` unbounded and lean on user-defined `INT32`/`INT64`, or add bounded built-ins at the cost of one more documented type. The performance principle leans toward bounded built-ins; the conformance principle leans toward keeping the TTM name unbounded.

## 11. First milestone

1. Lex + parse the uniform-prefix-syntax core (RM Pre 1, 6–10, 13–14, 18): scalar declarations, possrep/selector, relvar declarations, JOIN, WHERE/`restrict`, EXTEND, simple SUMMARIZE, RENAME, project. Multiple assignment. **Establish the spans-on-every-node and diagnostics-as-values discipline from §12 here** — these are project-wide invariants, not LSP-conditional. The parser uses `chumsky`'s error-recovery mode from day one.
2. Type-check headings, possreps, and selector signatures. Enforce no-nulls, no-duplicates at the type level. Verify candidate keys are declared and minimal. Type errors propagate via `Error` types, not cascades.
3. Lower to RelIR (sugar → A core during the same pass). Emit SQLite SQL honoring every rule in §5.
4. Hand-write the Rust runtime that runs the SQL and prints rows — no LLVM yet. Implement explicit transactions and multiple assignment.
5. Add the in-process RelIR executor for `RELATION` literals and constructed relations.
6. Add ProcIR + the LLVM codegen crate with `LOAD`, counted `DO` loops, and `query → relation → load → iterate`. Link the runtime as a `staticlib` and confirm the FFI struct layout matches the LLVM emission.
7. Add the Postgres backend behind the same `Backend` trait. Confirm the golden SQL tests fork cleanly per dialect.
8. Add user-defined scalar types with possreps, selectors, THE_ ops, and POSSREP CONSTRAINTs. Confirm equality works through the possrep round-trip.

VSS adoptions (system keys/TAG, FK shorthand, candidate-key inference, transition constraints, RANK quota queries) come after the milestone above is end-to-end on a toy program.

## 12. Editor tooling (LSP + VSCode extension)

A VSCode extension shipping a TextMate grammar for instant lexical highlighting, paired with `coddl-lsp` — a Rust language server built on the same frontend crates as the compiler. v1 scope is the two capabilities currently committed: **syntax highlighting and diagnostics (warnings/errors)**. Hover, go-to-definition, find-references, completion, and semantic-token enhancements are designed-for but not v1 work.

### Crates and project shape

- `coddl-diagnostics` — shared diagnostic data type: `(file_id, byte_range)` span + severity + code + message + optional related-spans. Every frontend crate (`coddl-syntax`, `coddl-types`, `coddl-relir`, `coddl-sqlemit`) produces and consumes this type. The CLI driver renders to terminal; `coddl-lsp` serializes to `PublishDiagnostics`.
- `coddl-lsp` — language server binary on `tower-lsp` over stdio. Owns document state and request dispatch; no analysis logic of its own — it calls into the frontend crates and forwards their output. Adding hover / go-to-def later is straightforward once `coddl-types` exposes symbol tables.
- `editors/vscode/` — VSCode extension (TypeScript). Ships the TextMate grammar (`syntaxes/coddl.tmLanguage.json`), language configuration (brackets, comments, indent rules), and a client that spawns `coddl-lsp` from `PATH` or a configured location.

Tree-sitter (more accurate, incremental highlighting) is a possible upgrade later; maintaining a second parser in lockstep with `coddl-syntax` is real cost and defers until concrete demand surfaces.

### Discipline this imposes on the frontend (lands in milestone 1, not "when the LSP arrives")

The LSP isn't an add-on bolted on at the end — its requirements shape the rest of the frontend. These constraints land on the compiler from day one, in line with long-term planning:

1. **Spans on every AST/IR node.** Every token, every AST node, every typed-AST node, every diagnostic carries `(file_id, byte_range)`. Retrofitting spans is a project-wide refactor — write them in from the first lexer token.
2. **Error recovery in the parser.** `chumsky` produces a best-effort AST with `Error` nodes rather than failing on the first syntax error. The type checker treats `Error` types as propagating-but-not-cascading — don't pile a hundred type errors on top of one parse error.
3. **Diagnostics-as-values.** No `panic!` or `eprintln!` for user-visible errors anywhere in the frontend. Every pass returns its diagnostics in a `Vec<Diagnostic>` alongside the (possibly partial) result. CLI and LSP differ only in presentation.
4. **Pure analyses.** Every frontend pass is `fn(Input) -> (Output, Vec<Diagnostic>)` — no globals, no I/O, no hidden state. The LSP can call any pass on any buffer at any time.

### Performance posture

v1: full re-parse + re-typecheck per buffer edit. Coddl programs are small and the Rust frontend is fast; latency won't be the bottleneck on realistic files. **Long-term planning:** route analyses through `salsa` (rust-analyzer's incremental-computation library) once response latency matters. The pure-analysis discipline above makes that migration mechanical rather than architectural — every pass is already shaped like a salsa query.

### Out of scope for v1

Code lenses, refactorings, debug adapter protocol. Sockets for these live in `coddl-lsp` once core diagnostics + hover + go-to-def land. Formatting (`coddl fmt` and `textDocument/formatting`) is in scope — it's covered separately in §13 because it has its own design implications for the parser.

## 13. Code formatter (`coddl fmt`)

A canonical formatter for Coddl source, exposed two ways from one library: `coddl fmt` (driver subcommand, à la `cargo fmt`) and `textDocument/formatting` in `coddl-lsp` (format-on-save). Both paths call into `coddl-fmt`; there is no second implementation.

### CST over AST-with-trivia

This is the load-bearing decision. The compiler's typechecker doesn't care about whitespace or comments — they're noise for analysis. The formatter cares about every byte. Three options:

1. **AST + side-channel trivia.** Parser emits the AST and a parallel list of (byte-range, trivia) entries. Formatter walks the AST and consults the list. Cheap up-front; every formatter pass re-decides "where does this comment attach?", and edge cases proliferate.
2. **AST with attached trivia.** Each AST node holds leading/trailing trivia. Bloats the AST for every consumer; non-formatter passes pay the memory cost.
3. **Concrete syntax tree (CST) + AST view.** Parser produces a lossless tree — every token, every trivia, every byte. The AST is a typed view derived from the CST; the typechecker walks the AST, the formatter walks the CST, both share the same backing storage. This is `rust-analyzer`'s approach via `rowan`.

**Coddl picks option 3.** Long-term planning: the formatter, the LSP semantic-tokens path, and incremental re-analysis under `salsa` (§12) all want a lossless tree. Retrofitting one is a parser rewrite — the kind of corner-painting this project explicitly avoids. `coddl-syntax` produces a CST from day one; `coddl-types`, `coddl-relir`, and friends consume an AST view derived from it.

`coddl-diagnostics::Span` carries through unchanged — it's still `(file_id, byte_range)`, which the CST can produce for any node trivially.

### Formatting rules (v1)

The formatter is opinionated and has few knobs. A `fmt` whose output drifts between versions or wobbles with bikeshedding is worse than a stricter one. Initial rules:

- **Indent**: 4 spaces (`indent_width` config; revisit if real demand surfaces).
- **Line width**: 100 columns soft; hard if a single token can't be split.
- **Braces**: `{` on the same line as the keyword/operator that opens them; `}` on its own line aligned with the opener — except trivial single-line bodies (`OP { x 1, y 2 }`) which stay inline up to the line-width limit.
- **Named arguments inside braces**: one per line if any single arg makes the whole call exceed the line width; otherwise stay on the line. No alignment of names across lines (it churns under add/remove).
- **Operator spacing**: one space around `=`, `<`, `>`, `+`, `-`, `*`, `/`, `,`; no space around `.`.
- **Trailing commas**: required in multi-line bracketed lists, forbidden in single-line ones (so adding then removing a wrap is idempotent).
- **Blank lines**: preserve user blank lines between top-level items, collapsed to at most one consecutive blank.
- **Comments**: preserved as-is, attached to the following node by default. Block-leading `--` comments stay on their own line; trailing `--` comments stay trailing.

Idempotency is a unit-test invariant: `fmt(fmt(x)) == fmt(x)` for every input in `examples/` and `tests/`.

### Edition versioning

Formatter output is versioned, à la `rustfmt`'s editions. A project's `coddl.toml` carries `format.edition = "2026"` (or whichever); default = newest edition the compiler knows. Edition bumps are explicit opt-in; old projects keep their formatting until they update. This buys the freedom to evolve the rules without breaking every committed file in every downstream project.

### Performance posture

Format-on-save needs to be fast: milliseconds, not tens of milliseconds. The CST walk is O(n); the printer is O(n); no re-parsing inside the formatter. The frontend already serves both the CLI and the LSP from the same pure passes (§12), so the formatter inherits the same discipline — `fn(source) -> (formatted, Vec<Diagnostic>)`, no globals, no I/O.

### Out of scope for v1

Auto-import sorting, comment reflow at line-width limits, configurable rules beyond `indent_width` and `format.edition`, format-only-the-diff (`coddl fmt --check` is in scope; rustfmt-style range-only formatting in the LSP can land later). Add these once the rules above stabilize and the idempotency tests stick.

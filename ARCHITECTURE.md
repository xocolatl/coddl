# Coddl — Architecture Sketch

A compiler for a D-family relational language conforming to Date and Darwen's *Third Manifesto*. Query fragments compile to SQL and run against a pluggable storage backend (SQLite first, Postgres later). Everything else compiles to LLVM IR and links against a small native runtime that owns the DB connection.

Coddl is *not* Tutorial D. It honors the same prescriptions and proscriptions but diverges on surface syntax (see §3) and adopts Algebra A as the canonical intermediate form (see §4).

## 1. Host language

**Haskell** (chosen). ADTs are the right tool for AST/IR work, pattern matching keeps the lowering passes readable, and GHC's RTS is well-suited to hosting the runtime end-users link against.

Stack:
- **Parser**: `megaparsec`. Good error messages, easy to evolve.
- **LLVM**: emit LLVM IR as text and shell out to `llc`/`clang`. `llvm-hs` is the alternative but its version-coupling churn isn't worth it for a project that doesn't need programmatic IR introspection; we can always switch later. Haskell is excellent at producing well-formatted text.
- **Databases**: backend-specific libraries behind a typeclass. `hasql` for Postgres (fast, type-safe), `direct-sqlite` for SQLite. Avoid HDBC.
- **Pretty-printing / formatting**: `prettyprinter` for both SQL and LLVM IR emission.
- **Build**: a single `cabal.project` with one package per crate-equivalent (see §7).
- **Runtime**: also Haskell, exposed via `foreign export ccall`. Compiled Coddl binaries link against the GHC RTS. The cost (RTS dependency in end-user binaries) is worth the benefit: the RelIR→SQL emitter is a single library used by both the compiler and the runtime, with no duplication.

## 2. Pipeline

```
source.cdl
   │
   ▼  lex + parse (megaparsec; uniform named-argument prefix syntax — see §3)
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
                      ▼  LLVM IR text emission (prettyprinter)
                      │
                      ▼  llc / clang
                      object file → linked against libcoddl_runtime + GHC RTS
```

The two IRs meet only at query-call sites: ProcIR holds the relation as a `Relation` handle (see §9) plus the parameters it needs to bind; the runtime returns rows that ProcIR consumes as tuples.

## 3. Conformance to the Third Manifesto

Coddl conforms to *The Third Manifesto* (Date & Darwen, 3rd ed., 2014). The summary below is binding on design choices throughout this document.

### Adopted (RM/OO Prescriptions and Proscriptions — non-negotiable)

- **Scalar types** carry possreps with selectors and THE_ accessors; named types are disjoint; no implicit coercion (RM Pre 1–5).
- **TUPLE H and RELATION H** are type generators with structural identity by heading (RM Pre 6–7). Tuple/relation type equality is set-equality of `{name → type}` pairs.
- **No nulls. Ever.** Missing-information modeling uses a user-defined `Maybe[T]` ADT and a database-design choice (split tables, sentinel relvars). The SQL backend must never emit `NULL` for an attribute value, never emit `NULLABLE` columns, never use `IS NULL` predicates, and must wrap any operator that SQL would otherwise produce a null from (RM Pro 4, see §4).
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
- **Foreign-key shorthand** (VSS 2 — formally deleted in later revisions, but worth keeping as parser sugar that desugars to a subset-constraint). Defer cascade actions.
- **Candidate-key inference** (VSS 3), minimally: propagate FDs through project/equijoin/restrict and surface inferred keys to the catalog. Best-effort.
- **Transition constraints** (VSS 4): primed-relvar syntax (`S'`) in `CONSTRAINT` bodies; pre-image captured by the runtime over delta sets, not by SQL triggers.
- **Quota queries** (VSS 5): `RANK r BY (DESC attr AS rankcol)` desugaring at the parser, lowering to `RANK()`/`DENSE_RANK()` window functions.

### Deferred to a later milestone

- **Generalized transitive closure** (VSS 6) — depends on VSS 7. Ship plain `TCLOSE` first.
- **User-defined heading-polymorphic operators** (VSS 7). Design the type system so adding row/heading polymorphism later doesn't force a rewrite: keep headings first-class in the type representation, don't hardwire monomorphic dispatch.
- **Type inheritance** (OO Pre 2, IM Pres). Conditional in the Manifesto. Coddl omits inheritance in v1; if added, it conforms to Part IV of the Manifesto in full.

### Skipped

- **SQL migration** (VSS 8). Out of scope for v1. Influence on the design is limited to: keep the type system extensible enough to add a parallel `SQL_*` type family later, and keep built-in operator names addressable (don't hardwire `=` to one type).

### Syntactic divergence from Tutorial D

Tutorial D's authors themselves admit (ch. 5, "A Remark on Syntax", pp. 127–128) that Tutorial D's operator syntax "is not very consistent" — mixed prefix/infix, positional matching that "violates the spirit, if not the letter, of RM Proscription 1." They propose a uniform style: prefix for everything, argument matching by name, braces for argument bundles:

```
CARTESIAN { Y 2.5, X 5.0 }     -- not CARTESIAN ( 5.0, 2.5 )
JOIN      { left R, right S }  -- name the slots
```

**Coddl adopts this uniform named-argument prefix style as the default.** Concessions: infix forms for `=`, `<`, `+` and friends are retained (the named-prefix form is clumsy for ubiquitous dyadic ops on identifier-unfriendly names); a small set of monadic operators (`COUNT`, `SIN`, `IS_*`) keep parenthesized positional form. Everything else — including the relational algebra, selectors, EXTEND, SUMMARIZE, GROUP, UNGROUP — is named-prefix with braces. This eliminates the relational-algebra/scalar-op syntactic distinction the authors regret.

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

### ProcIR — procedural / LLVM-bound IR

SSA blocks with typed values, plus a small set of relation-aware ops:
- `query(plan_id, [params...]) -> Relation`
- `force(Relation) -> MaterializedRelation`
- `load(Relation, OrderSpec) -> Array<Tuple>` — the only sanctioned iteration path
- `assign_relvar(name, plan_id, [params...])` (relational assignment)
- `multi_assign([(target, plan_id, params)…])` — atomic, MA semantics per RM Pre 21
- `begin_tx / commit_tx / rollback_tx`

These lower to calls into the runtime ABI.

## 5. Storage abstraction

A pair of typeclasses, one for the (pure) SQL-emitting half and one for the (effectful) connection half:

```haskell
class Backend b where
  dialect      :: Proxy b -> Dialect
  emitSelect   :: Proxy b -> RelPlan -> SqlString
  emitDdl      :: Proxy b -> Schema  -> [SqlString]
  typeMap      :: Proxy b -> TypeMap            -- CoddlType ↔ SQL type
  open         :: Proxy b -> DSN -> IO (Conn b)

class Conn c where
  prepare         :: c -> SqlString -> IO StmtId
  bindAndStep     :: c -> StmtId -> [Value] -> IO RowIter
  materializeTemp :: c -> Heading -> [Tuple] -> IO TempRelRef
```

Packages: `coddl-backend-sqlite`, `coddl-backend-postgres`. The compiler picks one at build/CLI time; the LLVM-emitted binary links against exactly one runtime that wraps the chosen `Conn` instance. Don't go overboard with `Proxy`/`Tagged` machinery — a per-backend record-of-functions (à la `BackendOps`) may end up cleaner than a typeclass once you start passing backends around as values; decide once the second backend exists.

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

A Haskell library exposed via `foreign export ccall`. Compiled Coddl binaries link against it and the GHC RTS. Responsibilities:
- Own the DB connection pool.
- Cache prepared statements by `plan_id` (compiler assigns at codegen time).
- Marshal LLVM-side value structs ↔ backend parameter binders. Use `Foreign.Storable` and `CStruct`-shaped types; keep the on-the-wire layout matched by hand to what LLVM codegen emits, with a generator producing both sides from a single description if it starts drifting.
- Provide a row iterator the LLVM-emitted code can drive (cursor handle + `coddl_next` returning a tagged-union row).
- Host the in-process RelIR executor (§8) and the RelIR→SQL emitter (the same library the compiler uses).
- Map errors to a single error code + thread-local message.

LLVM IR calls these exports as plain C functions. The runtime is where SQLite vs Postgres lives at runtime — the compiled program is backend-agnostic if you're disciplined about not leaking dialect-specific values through the ABI.

**On the GHC RTS in user binaries.** It brings garbage collection, green threads, and some startup cost. None of these conflict with LLVM-emitted code so long as foreign calls don't allocate Haskell heap pointers that escape into LLVM-managed memory. Coddl-side values that cross the boundary (tuples, relation handles) must be pinned or copied. Document this discipline in the runtime package.

## 7. Type system

### Scalar types

A scalar type is a named, finite set of values disjoint from every other scalar type. Each user-defined scalar type carries one or more **possible representations** (possreps) — abstract representations made up of named, typed components — and a (possibly trivial) `CONSTRAINT` predicate that defines which possrep tuples denote real values of the type (RM Pre 4–5, p. 144–151).

For every possrep `PR` of type `T` the system synthesizes:
- A **selector** of declared type `T`, one parameter per component (selector name = possrep name). Every value of `T` must be producible by an all-literal selector invocation.
- A **THE_C accessor** per component `C`: read-only in source position; pseudovariable in target position (`THE_C(V) := x` is sugar for `V := PR(…, x in slot C, …)`).

**Type constraints** (the `POSSREP CONSTRAINT` predicate) are checked at every selector invocation — that's the sole choke point because values of `T` can only be constructed via the selector. Type-constraint violations are run-time errors; argument-type mismatches are compile-time.

Built-in scalar types: `INT`, `RATIONAL`, `BOOLEAN`, `CHARACTER` (variable length, no padding), `TEXT`, `DATE`, `TIMESTAMP`, `UUID`, `BYTES`. Each has fixed mappings to (a) LLVM type, (b) SQLite affinity + `CHECK` constraints where needed, (c) Postgres type.

**No implicit coercion.** Distinct named scalar types are disjoint; `INT` and `RATIONAL` cannot be silently mixed. Equality `=` is type-monomorphic per RM Pre 8 ("indistinguishable for all operators on T").

**No nulls.** Period. The type system has no nullable-attribute facility. Missing-information modeling is a database-design problem the user solves with a `Maybe[T]` sum (defined in user code), with sentinel relvars, or with split tables. The SQL backend never sees a request to emit a NULL.

### Type generators

- `TUPLE { a: T, b: U, … }` and `RELATION { a: T, b: U, … }` are type generators producing structurally-identified types: `TUPLE H1 = TUPLE H2` iff `H1 = H2` as sets of `<name, type>` pairs. Same for `RELATION`. Attribute order is immaterial. Both generators may take zero attributes (`TABLE_DEE` and `TABLE_DUM` are the only inhabitants of `RELATION { }`).
- Headings may include relation-valued and tuple-valued attributes (nesting permitted; RM Pre 6–7).
- A *relvar* is a named, persistent variable of some `RELATION H` type, backed by storage in the chosen backend. Relvars are real, virtual (views), or application-private (RM Pre 14). Every relvar has at least one declared candidate key (RM Pre 15) — including possibly the empty key (which forces cardinality ≤ 1).

### Relations are fully first-class

Relations can be bound to variables, passed to and returned from operators, stored in tuples, nested inside other relations, used as function arguments and results everywhere a scalar can. The calling convention treats them uniformly (see §9).

### Type inference and constraint inference

Type inference for relational expressions is mandatory and mechanical from operator semantics (RM Pre 18): every RelIR node's heading is the heading of its operands transformed by its operator. The optimizer further runs:
- **FD propagation** for candidate-key inference (VSS 3) — best-effort.
- **Constraint propagation** (RM Pre 23): predicates known to hold on operands propagate through restrict, project, join, extend, etc. Used for view-constraint checking and as optimizer hints.

## 8. Project layout (Cabal multi-package)

```
coddl/
  cabal.project
  packages/
    coddl-syntax/            # megaparsec lexer/parser, AST
    coddl-types/             # type checker, type representation
    coddl-relir/             # relational IR + optimizer
    coddl-procir/            # procedural IR
    coddl-sqlemit/           # RelIR → SQL (dialect-agnostic core; used by both compiler and runtime)
    coddl-execlocal/         # in-process RelIR executor over materialized relations
    coddl-backend-sqlite/
    coddl-backend-postgres/
    coddl-llvm/              # ProcIR → LLVM IR (text emission via prettyprinter)
    coddl-runtime/           # foreign-export-ccall library linked into compiled binaries
    coddl-driver/            # CLI: compile, run, repl
  test/
    golden/                  # SQL emission goldens per backend
    e2e/                     # compile + run end-to-end
  examples/
```

## 9. Execution model

**Relations are lazy.** Scalars are strict. A relation expression doesn't run until it's forced — by iteration, by materialization, by being shipped into another query, or by an explicit `force`. Equality is by value (heading + tuple set), so two relations built by different routes that yield the same tuples are equal regardless of evaluation history. This is a language-level commitment per RM Pre 8.

Because relations are first-class, the calling convention has to be uniform: any function that takes a relation must accept a value it can read, re-query, and pass onward. Combined with laziness, that means **materialization happens on first force**, not at construction; streamed cursors and plan-backed handles let multiple forces share work or avoid it entirely.

### Iteration: the LOAD primitive

There is no tuple-at-a-time access to relvars or relations (RM Pro 7). The only iteration primitive is `LOAD`, modeled on Tutorial D:

```
VAR A ARRAY TUPLE { S# S#, QTY QTY } ;
LOAD A FROM ( SP WHERE P# = P#('P1') ) { S#, QTY } ORDER ( ASC S# ) ;
DO i := 1 TO COUNT(A) ;
  -- process A[i]
END DO ;
```

`LOAD` is the syntactic and semantic gate between the set-oriented and procedural worlds: it forces the relation, imposes an order (the order is part of the operation, not a property of the relation), and writes the tuples into a local array. The array is then iterable by a counted `DO` loop. This is the *only* sanctioned path; the compiler rejects any other attempt to step through tuples one at a time.

### Multiple assignment

`A1, A2, …, An ;` is a single statement with the semantics of RM Pre 21:
1. Expand all syntactic shorthands (INSERT/UPDATE/DELETE/THE_ pseudovariable) into `target := expr` form.
2. Fold duplicate targets by rewriting `Vq := Xq` as `Vq := WITH Xp AS Vq : Xq` and dropping the earlier assignment. Repeat.
3. Evaluate every RHS expression. Capture results.
4. Apply all assignments to their targets atomically.
5. Check every applicable database constraint at the end of the whole MA (not between assignments).

The procedural IR therefore has a `multi_assign` primitive, not just a sequence of individual `assign` calls. The runtime evaluates all RHSs first (against the pre-MA database state), then commits the writes in one logical step, then runs constraint checks.

### Transactions

`BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK` are explicit (OO Pre 4). Nested transactions are supported (OO Pre 5): a nested `BEGIN` starts a child; child `COMMIT` is conditional on the parent; child `ROLLBACK` undoes only the child's work. A relation handle captured before a write within the same transaction continues to read pre-write contents (snapshot semantics by default). The SQL backend uses SAVEPOINT for child transactions, but the runtime tracks the parent/child relationship explicitly because SQL `SAVEPOINT` doesn't model true nesting.

### Relation values at runtime

A first-class relation is one of three things, behind a single `Relation` handle:

1. **Plan-backed** — a `plan_id` plus its already-bound parameters. Hasn't executed yet. Cheapest. Used when the value is consumed by another query (the optimizer splices the plan in) or iterated exactly once.
2. **Materialized** — a runtime-owned buffer of tuples (arena-allocated, or a backend temp table for large ones). Used when the value is consumed more than once, passed into procedural code that operates on it without re-querying, or escapes a scope.
3. **Cursor** — a live result set being drained. Compiler-only optimization for `for tup in rel { … }` loops where `rel` is provably single-use.

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

## 11. First milestone

1. Lex + parse the uniform-prefix-syntax core (RM Pre 1, 6–10, 13–14, 18): scalar declarations, possrep/selector, relvar declarations, JOIN, WHERE/`restrict`, EXTEND, simple SUMMARIZE, RENAME, project. Multiple assignment.
2. Type-check headings, possreps, and selector signatures. Enforce no-nulls, no-duplicates at the type level. Verify candidate keys are declared and minimal.
3. Lower to RelIR (sugar → A core during the same pass). Emit SQLite SQL honoring every rule in §5.
4. Hand-write a runtime that runs the SQL and prints rows — no LLVM yet. Implement explicit transactions and multiple assignment.
5. Add the in-process RelIR executor for `RELATION` literals and constructed relations.
6. Add ProcIR + LLVM with `LOAD`, counted `DO` loops, and `query → relation → load → iterate`.
7. Add the Postgres backend behind the same `Backend` typeclass. Confirm the golden SQL tests fork cleanly per dialect.
8. Add user-defined scalar types with possreps, selectors, THE_ ops, and POSSREP CONSTRAINTs. Confirm equality works through the possrep round-trip.

VSS adoptions (system keys/TAG, FK shorthand, candidate-key inference, transition constraints, RANK quota queries) come after the milestone above is end-to-end on a toy program.

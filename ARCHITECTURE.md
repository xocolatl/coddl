# Coddl — Architecture Sketch

A compiler for a Tutorial-D-flavored relational language. Query fragments compile to SQL and run against a pluggable storage backend (SQLite first, Postgres later). Everything else compiles to LLVM IR and links against a small native runtime that owns the DB connection.

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
   ▼  lex + parse (logos + hand-written Pratt parser, or LALRPOP)
  AST
   │
   ▼  name resolution, module/import resolution
  Resolved AST
   │
   ▼  type checking (scalars + relation types)
  Typed AST
   │
   ▼  lowering — splits into two IRs at the relational boundary
   │
   ├──────────────► RelIR  (relational algebra plan)
   │                  │
   │                  ▼  optimize (push-down, dead-attr elim, constant folding)
   │                  │
   │                  ▼  SQL emit  (dialect = backend trait)
   │                  SQL string + parameter slots
   │
   └──────────────► ProcIR (SSA-ish, LLVM-shaped)
                      │
                      ▼  LLVM IR codegen (inkwell)
                      │
                      ▼  llc / lld
                      object file → linked against libcoddl_runtime
```

The two IRs meet only at query-call sites: ProcIR holds the SQL handle as an opaque value plus the parameter list it needs to bind; the runtime returns rows that ProcIR consumes as tuples.

## 3. Two IRs, one boundary

### RelIR — relational algebra plan

Nodes mirror Tutorial D operators, not SQL: `Project`, `Restrict`, `Join`, `Rename`, `Extend`, `Summarize`, `Group`, `Ungroup`, `Union`, `Minus`, `Intersect`, `TClose`. Each node carries a heading (attribute → type). Pure, immutable, easy to optimize and to translate to SQL.

Why not lower straight to SQL from the AST? Because (a) the same RelIR also feeds future backends, (b) it's the right level for algebraic rewrites, and (c) it makes a future in-memory executor (for tests, REPL, small relations) a drop-in alternative.

### ProcIR — procedural / LLVM-bound IR

SSA blocks with typed values, plus a small set of relation-aware ops:
- `query(plan_id, [params...]) -> Cursor`
- `next(Cursor) -> Option<Tuple>`
- `assign_relvar(name, plan_id, [params...])` (DDL/DML side)

These lower to calls into the runtime ABI.

## 4. Storage abstraction

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

Keep SQL emission to a **portable subset** (CTEs, window functions, standard joins) and isolate dialect divergence (boolean vs int, `RETURNING`, recursive CTE syntax, identifier quoting, JSON, upsert) behind backend methods. Golden-file tests per backend: `RelIR plan → expected SQL` for each dialect.

## 5. Runtime (`libcoddl_runtime`)

A Haskell library exposed via `foreign export ccall`. Compiled Coddl binaries link against it and the GHC RTS. Responsibilities:
- Own the DB connection pool.
- Cache prepared statements by `plan_id` (compiler assigns at codegen time).
- Marshal LLVM-side value structs ↔ backend parameter binders. Use `Foreign.Storable` and `CStruct`-shaped types; keep the on-the-wire layout matched by hand to what LLVM codegen emits, with a generator producing both sides from a single description if it starts drifting.
- Provide a row iterator the LLVM-emitted code can drive (cursor handle + `coddl_next` returning a tagged-union row).
- Host the in-process RelIR executor (§8) and the RelIR→SQL emitter (the same library the compiler uses).
- Map errors to a single error code + thread-local message.

LLVM IR calls these exports as plain C functions. The runtime is where SQLite vs Postgres lives at runtime — the compiled program is backend-agnostic if you're disciplined about not leaking dialect-specific values through the ABI.

**On the GHC RTS in user binaries.** It brings garbage collection, green threads, and some startup cost. None of these conflict with LLVM-emitted code so long as foreign calls don't allocate Haskell heap pointers that escape into LLVM-managed memory. Coddl-side values that cross the boundary (tuples, relation handles) must be pinned or copied. Document this discipline in the runtime crate.

## 6. Type system

- Scalars: `Int`, `Real`, `Bool`, `Char`, `Text`, `Date`, `Timestamp`, `UUID`, `Bytes`. Each has a fixed mapping to (a) LLVM type, (b) SQLite affinity, (c) Postgres type. Mapping tables live next to the backend.
- Relations: `Rel{a: T, b: U, …}` is **fully first-class**. They can be bound to variables, passed to and returned from functions, stored in tuples, nested inside other relations, used as function arguments at every call site a scalar can. A *relvar* is a named, persistent binding backed by a base table; a relation value is the unnamed, possibly transient counterpart.
- Tuples: `Tup{a: T, b: U, …}`. Heading-compatible with the relation it comes from.
- Optional sum types for `NULL` modeling — Tutorial D famously avoids NULL; you'll want to decide early whether to expose nullability or wrap it in `Maybe[T]` at the language level and translate to NULL at the SQL boundary.

## 7. Project layout (Cabal multi-package)

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

## 8. Execution model

**Relations are lazy.** Scalars are strict. A relation expression doesn't run until it's forced — by iteration, by materialization, by being shipped into another query, or by an explicit `force`. Equality is by value (heading + tuple set), so two relations built by different routes that yield the same tuples are equal regardless of evaluation history. This is a language-level commitment, not just a runtime trick; the type system and any future effect tracking must respect it.

Because relations are first-class, the calling convention has to be uniform: any function that takes a relation must accept a value it can read, re-query, and pass onward. Combined with laziness, that means **materialization happens on first force**, not at construction; streamed cursors and plan-backed handles let multiple forces share work or avoid it entirely.

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

## 9. Risks worth deciding early

1. **Materialization thresholds.** First-class relations mean the runtime constantly chooses between in-memory and temp-table representation. Pick a default policy (size-based, with an explicit `@materialize` / `@stream` annotation as escape hatches) before you write the runtime allocator.
2. **How honest about SQL are you willing to be?** Pushing everything down (joins, aggregates, `Extend` with scalar UDFs) means writing SQL UDF shims or restricting `Extend` to backend-expressible expressions. Easiest start: push pure-relational algebra; evaluate anything else in the in-process executor.
3. **Transactions and identity.** Tutorial D has clean update semantics; SQL has transactions. Decide whether a Coddl block maps to a SQL transaction implicitly or only when annotated. First-class relations sharpen this: a relation handle captured before a write is observed must still read its old contents (or explicitly opt into "live"). Snapshot semantics by default is the sane choice.
4. **NULL.** See §6.
5. **Monomorphization vs. runtime planning.** Specializing every relation-polymorphic function on heading is simple but can blow up code size in pathological cases. Have the runtime planner ready from the start so you can fall back.

## 10. First milestone

1. Lex + parse a Tutorial-D-ish subset (relvar declarations, `JOIN`, `WHERE`, `EXTEND`, simple `SUMMARIZE`).
2. Type-check headings.
3. Lower to RelIR; emit SQLite SQL.
4. Hand-write a runtime that runs the SQL and prints rows — no LLVM yet.
5. Add ProcIR + LLVM with a single operation: "run this query and print rows from native code."
6. Add the Postgres backend behind the same trait. Confirm the golden tests fork cleanly.

Once 1–6 work end-to-end on a toy program, the rest is filling in operators.

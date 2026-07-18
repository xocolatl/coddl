# Storage abstraction

Coddl talks to persistent storage through a pair of Rust traits — `Backend` (pure, SQL-emitting half) and `Conn` (effectful, connection half) — so that the rest of the compiler stays backend-agnostic. SQLite is v1 (public relvars pushed/read through the connection, base relvars writable via surgical DML, real transactions); Postgres lands later.

This doc covers the **abstraction** (the traits, the design rationale, the `database` declaration) and the **concrete v1 SQLite implementation** (Phase 22: public relvars, materialization, transaction stubs). For the SQL emission rules that govern *what* the backend sees, see [sqlemit.md](sqlemit.md). For the broader runtime architecture (the SQL engine plus the in-process runtime library) see [runtime.md](runtime.md). For the surface typing rules around public relvars, see [typecheck.md](typecheck.md); for the IR shape, [procir.md](procir.md); for the per-backend emission, [codegen.md](codegen.md).

## Why an abstraction at all

RM Pro 6 forbids internal-level constructs in `D` source. A Coddl program does not name a `.sqlite` file, a `pg_hba.conf`, a `JDBC URL`, or a column type. The mapping from a `public relvar` in source to a physical table in some database lives in companion files (`.cddb` for the catalog, `.cdstore` for the physical binding — see [plan.md](plan.md)). The source code names the **database** (a logical handle) by binding `database <name>;`; everything else is the plan layer's job.

The `Backend` / `Conn` split exists for the same reason the IR split exists (see [principles.md](principles.md) "Long-term planning"): the boundary is *semantic* (pure dialect emission vs. effectful connection) rather than expedient. Adding Postgres later doesn't require touching the compiler — only adding a second `impl Backend`.

## The traits

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

Crates: `coddl-backend-sqlite`, `coddl-backend-postgres`. Selection is a Cargo feature on the runtime crate; the LLVM-emitted binary links against exactly one runtime that wraps the chosen `Conn`. If passing backends around as values gets clumsy with the associated-type trait, switch to a `dyn`-friendly `BackendOps` record-of-fn-pointers — the per-call dispatch cost is negligible against query latency. Decide once the second backend lands. Cargo features also gate SQL backends out of `wasm32-*` builds where the C dependencies of `rusqlite` / `postgres` don't link (see [workspace.md](workspace.md), [runtime.md](runtime.md) "Portability").

`materialize_temp` is the boundary primitive for sending in-memory relations back into SQL — the portable seam for trait-path backends. The bundled-SQLite realization lives directly at the runtime force point instead (`fire_escalated`, `coddl-runtime/src/sqlite.rs`, which wraps rusqlite without going through `Conn`); see [sqlemit.md](sqlemit.md) "Sending in-memory relations back into SQL" for the mechanism and per-backend strategy.

## The `database` declaration

Every Coddl program with public relvars must declare which database it binds to:

```
program hello_world_db;
database greetings;

public relvar Greetings { id: Integer, message: Text }
key { id };
```

`database <name>;` introduces a logical handle that the plan layer resolves against companion `.cddb` (catalog) and `.cdstore` (physical binding) files. The Coddl source contains no file path, no connection string, no dialect — RM Pro 6.

At runtime, operational fields are resolved through environment overrides keyed by the database name: `CODDL_<DBNAME>_FILE` for the SQLite path on the example above, expanded uppercase from `database greetings;` → `CODDL_GREETINGS_FILE`. The baked default (from the `.cdstore`) wins when the env var isn't set. See "Path resolution" below.

## SQL emission, by reference

The `Backend::emit_select(plan) -> SqlString` method is where RelIR becomes SQL. The rules that emission must follow — `SELECT DISTINCT` on every projection, never `NULL`, always explicit columns, etc. — are correctness requirements imposed by TTM and live in [sqlemit.md](sqlemit.md). This doc deliberately doesn't repeat them; if you're writing or auditing a new backend, the emission table in `sqlemit.md` is the contract.

Keep SQL emission to a **portable subset** (CTEs, window functions, standard joins) and isolate dialect divergence behind backend methods. Golden-file tests per backend (`tests/golden/`) lock down `RelIR plan → expected SQL` per dialect; the [validation matrix](validation.md) confirms that the *results* match across backends regardless of textual differences.

---

# v1: SQLite implementation (Phase 22)

The rest of this doc pins the current SQLite-backed pipeline. Phase 22 brings public relvars to life: the four-file `.cd` / `.cddb` / `.cdstore` chain (validated by Phase 16's plan layer) drives a runtime materialization pass at program startup, public-relvar references resolve to in-memory `Relation H` values, and `transaction [...]` becomes load-bearing with the TTM OO Pre 4 conformance check.

## Discovery → plan → runtime

```
.cd  ┐
.cddb├─→ coddl_plan::discover_and_validate ─→ Plan ─→ coddl_procir::lower_with_plan ─→ Module ─→ backends ─→ binary
.cdstore┘                                                                                              │
                                                                                                       ▼
                                                                                              libcoddl_runtime
                                                                                              (rusqlite, bundled)
```

The driver (`coddl compile` / `coddl run`) runs plan discovery when
the input is a `.cd` file path. Plan diagnostics flow through the
standard channel; on success, the resolved `Plan` carries:

- `program_name`, `database_name` from the `.cd`.
- `resolved: Vec<ResolvedPublicRelvar>` — one entry per public relvar:
  `(app_name, catalog_name, heading, table_name, columns, write_policy)`.
- `db_file_default: Option<String>` — the `.cdstore`'s `file: "..."`
  directive canonicalised against the `.cdstore`'s parent directory.
  Baked into the binary as a string constant; the runtime resolver
  applies an env-var override before falling back to this default.

`coddl_procir::lower_with_plan` walks the `.cd`'s `oper main`, injects
one `Inst::RelvarSlotInit` per public relvar after `coddl_runtime_init`,
one matching `Inst::RelvarSlotRelease` before
`coddl_runtime_shutdown`, and resolves bare-name relvar references to
`Inst::RelvarRead`. The codegen layer emits per-relvar slot globals
plus the string-constant payloads each backend's call to
`coddl_sqlite_relvar_init` needs.

## Path resolution: env override + baked default

The runtime always goes through one resolver:

```
const char *coddl_resolve_op_field(env_name, env_len, default, default_len, *out_len);
```

It reads `getenv(env_name)`; on hit, returns the env string (length
written to `*out_len`). On miss, returns `default` unchanged.

The env-var convention is `CODDL_<DBNAME>_<FIELD>`, where DBNAME is
the uppercase form of the `database <name>;` binding and FIELD is the
operational field (today: `FILE` for the SQLite path). For
`hello-world-db` the lookup is `CODDL_GREETINGS_FILE`.

The baked default is the absolute canonical path computed at plan
time. v1 binaries built without env overrides aren't relocatable on
their own (the path is baked); setting `CODDL_<DBNAME>_FILE` at
startup makes them relocatable today.

## Supported attribute types (v1)

Public relvar attributes can be Integer, Boolean, or Text at the
runtime *read* path: the runtime's materialization marshals each cell
via `record_layout::cell_kind` into the canonical byte layout
(`docs/runtime.md`).

The DDL *write* path is wider. `coddl provision` (`docs/plan.md`)
creates + seeds base tables through `emit_ddl`, which covers the full
spellable-with-a-literal scalar set — Integer, Text,
Boolean, Rational, Approximate, Character — so a `.cddb` INIT value can
seed a Rational or Approximate column even though a compiled v1 program
cannot yet *read* one back. That read/write asymmetry is the same
per-cell-codec gap below, not a DDL limit.

Rational, Approximate, Character defer (for reads) until the runtime adds
per-cell codec entries. Nested **Tuple** and **Relation** cells are fully
representable in relation *values* (query results / intermediates — the
inline sub-region and the RC-pointer cell respectively, see
`docs/runtime.md`), but neither is a legal *storage-backed column*: the
typechecker rejects a relation- or tuple-valued attribute in a `public`
(`.cd`) or `base` (`.cddb`) relvar heading with **T0101** at check time
(a `private` relvar is in-process state and takes any heading). The
designed endpoint for persisting them is decomposition, below.

## Schema vocabulary and DDL emission (v1)

`Backend::emit_ddl(schema: &Schema) -> Vec<SqlString>` and
`Backend::type_map() -> &TypeMap` render a base relvar's physical schema.
Their inputs are the **neutral** column vocabulary in `coddl-sqlemit`
(`Schema` / `Column` / `ColKind` / `TypeMap`) — deliberately free of any
`coddl-types`/`coddl-relir` surface, so the permanent-Rust backend
(`coddl-backend-sqlite`) consumes them across the self-hosting seam
(`docs/principles.md`). The `coddl_types::Type → ColKind` fold lives up in
`coddl-provision`, the crate that sees both the relational middle and the
storage bottom; the backend only ever sees the neutral `Schema`.

- `ColKind` covers the six spellable-with-a-literal scalars — Integer,
  Text, Boolean, Rational, Approximate, Character. `Binary`/`Byte` are
  literal-less (no INIT value is expressible) and are rejected by the
  fold rather than mapped.
- `Schema { table, columns: Vec<Column>, pk: Vec<String> }`. Columns
  arrive **heading-sorted** (the fold supplies the order; `emit_ddl` is a
  pure renderer and does not re-sort). `pk` is **name-sorted** and
  non-empty — a candidate key is a set (RM Pro 1), so its SQL column order
  is arbitrary and name-sorting is the deterministic canonical choice.
- `Column.not_null` is **conceptual** totality (RM Pro 4) — true for every
  real relvar column. Physical nullability is the backend's call.
- `TypeMap` is just the `ColKind → base-SQL-keyword` map (what
  `PRAGMA table_info` reports: `INTEGER`/`TEXT`/`REAL`). Each backend
  authors its own values (SQLite's live in `coddl-backend-sqlite`); the
  schema-diff (`coddl provision`) compares against it. Structural quirks a
  bare keyword can't carry stay in `emit_ddl`.

The SQLite `emit_ddl` renders one `CREATE TABLE` per relvar following the
`docs/sqlemit.md` rules: every total column `NOT NULL` **except** an
`Approximate` (`REAL`), which stays nullable so SQLite can encode the
`NaN` value as `NULL` (the sanctioned NaN channel — a `NOT NULL` `REAL`
column would reject a legitimate NaN store; see `value_to_sqlite`); a
`Boolean` `INTEGER` gets `CHECK (col IN (0, 1))`; the key is a
**table-level** `PRIMARY KEY(…)`, never an inline `INTEGER PRIMARY KEY`
(whose rowid-alias would accept NULL); and every identifier is routed
through `quote_ident`. Postgres, which stores NaN natively, will make its
Approximate columns `NOT NULL` — which is exactly why `emit_ddl` is a
per-backend method rather than a shared emitter.

## Provision executor and schema diff (v1)

`coddl provision` (`docs/plan.md`) reconciles a SQLite database to the state a
catalog declares. The executor is `coddl_backend_sqlite::provision(db_path,
tables) -> Result<Report, ProvisionError>`, taking one `ProvisionTable {
schema: Schema, rows: Vec<Row> }` per base relvar — the neutral vocabulary
above, never a `Heading`/`CatalogPlan`, so it stays on the permanent-Rust side
of the self-hosting seam (`docs/principles.md`). The `Type → ColKind` and
`RelationLit → Vec<Row>` folds that build those inputs live up in
`coddl-provision`; the row cells are positional in the schema's heading-sorted
column order (cell `i` binds column `i`).

The reconcile is **one transaction** — SQLite's DDL + DML are transactional and
`sqlite_master`/`PRAGMA` reads don't force an implicit commit, so create +
delete + insert are atomic under a single `BEGIN`:

```
open db_path  READ_WRITE | CREATE | NO_MUTEX          (provision creates the file; the read path is READ_ONLY)
BEGIN
  # Pass 1 — reconcile schema (tables processed in the caller-supplied, name-sorted order)
  for t in tables:
      match sqlite_master.type WHERE name = t.table:
          absent   => emit_ddl(t.schema) → CREATE TABLE          (created = true)
          "table"  => if !diff_table(conn, t.schema).is_empty():  Err(SchemaMismatch) → ROLLBACK
          other    => Err(NotATable)  # view / index / trigger — never dropped
  # Pass 2 — truncate + replenish to INIT rows
  for t in tables:
      DELETE FROM t.table
      if t.rows: batched INSERT (cols name-sorted; ≤ INSERT_PARAM_BUDGET binds/batch, values bound never interpolated)
COMMIT            # any error ⇒ ROLLBACK, leaving the database byte-identical
```

It is **not** a migrator: a table that exists but doesn't match its declared
schema is a rollback + error, never a drop-recreate. That is what keeps
`provision` non-destructive — the invariant test provisions against a
deliberately-mismatched table and asserts the file is byte-identical afterward.

`diff_table(conn, schema) -> Result<SchemaDiff, ProvisionError>` is **policy-free**
— it returns a neutral `SchemaDiff` (empty ⇔ the table matches) and never
decides what to do about a difference. `provision`'s policy is "non-empty diff ⇒
`SchemaMismatch` ⇒ rollback"; the future `migrate` will consume the same
`SchemaDiff` to emit `ALTER`s. One seam, two commands. The diff compares the
`PRAGMA table_info` projection (column name-set, declared type string, NOT NULL
flag, and PK column *set*) against the exact oracle `emit_ddl` used: the
`type_map().sql_type` keyword, the "every total column `NOT NULL` except
`Approximate`" rule, and the name-sorted key columns.

**v1 blind spot:** `PRAGMA table_info` cannot see a `CHECK` constraint, so a
`Boolean` column (`INTEGER CHECK (c IN (0,1))`) is introspection-indistinguishable
from an `Integer` column — both report declared type `INTEGER`, `notnull = 1`,
and diff clean. Accepted for v1; a robust `sqlite_master.sql` comparison is
deferred to `migrate`. There is likewise no FK-ordering hazard in v1 (the
`.cddb` declares no foreign keys); when FKs land, Pass 2 must split into an
all-tables `DELETE` pass (child→parent) before any `INSERT` pass (parent→child),
since the per-table truncate+fill above would trip a cross-table constraint.

## Provision fold and diagnostics (`coddl-provision`)

The executor above is deliberately blind to the relational middle. The crate that
bridges the two is `coddl-provision` — the orchestration layer that sees *both*
sides of the self-hosting seam, exactly as `coddl-runtime` does. Its entry point
is `provision_catalog(cddb_path) -> ProvisionOutcome { report: Option<Report>,
diagnostics: Vec<Diagnostic> }`: it resolves the catalog
(`coddl_plan::resolve_catalog`), folds each base relvar into a `ProvisionTable`,
resolves the target file the way the runtime does, and drives the executor. The
database is touched only if resolution **and** every fold succeed; a catalog that
doesn't typecheck never reaches SQL.

Two folds turn the resolved catalog into neutral shapes:

- **`Heading → Schema`.** Each attribute's `coddl_types::Type` maps to a neutral
  `ColKind` (the six spellable scalars — `Integer`/`Text`/`Boolean`/`Rational`/
  `Approximate`/`Character`); `Binary`/`Byte` (literal-less) and the non-scalars
  are **rejected** (PV0001), never mapped. Columns keep the heading-canonical
  (attribute-name-sorted) order; the first candidate key's attributes translate
  to their physical columns and name-sort into the `PRIMARY KEY`.
- **INIT `Expr → Row`s.** A `.cddb` INIT cell is a *constant expression*, not
  merely a literal (Chunk 3 widened it), so provision **evaluates** it through the
  shared constant-folder `coddl_consteval::fold_const_scalar` — the very folder
  `coddl-procir` uses for module `let`s and pushdown-predicate constants, extracted
  down into `coddl-consteval` (deps `coddl-syntax` + `coddl-relir`) so both
  consumers depend downward on one source of truth rather than sideways on each
  other. Each folded `Literal` becomes a storage `Value`, widening an `Integer`
  literal to `n/1` in a `Rational` column (the Chunk-3 INIT tolerance). Cells are
  placed positionally in `schema.columns` order.

Fold-time validation is all pre-SQL: exact-duplicate tuples coalesce (a relation
is a set — RM Pro 2), two tuples that share a key but differ in a non-key
attribute are a clean error (PV0005), and any failure aborts before the database
is opened. The target file is resolved by replicating the runtime's rule exactly —
the `CODDL_<DBNAME>_FILE` env override (DBNAME = the uppercased `database` header),
else the `.cdstore` baked default — so `provision` and `run` always agree on which
file to open.

**Diagnostics (`PV####`).**

| Code | Meaning |
|---|---|
| PV0001 | attribute (or key attribute) has no provisionable column type |
| PV0002 | INIT value is not a relation literal (only `Relation { … }` is seedable in v1) |
| PV0003 | an INIT cell is not a constant scalar (or doesn't match its column type) |
| PV0004 | evaluating an INIT cell failed (overflow, division by zero) |
| PV0005 | two INIT tuples share a key but differ in a non-key attribute |
| PV0006 | cannot resolve a target database file (no env override, no baked default) |
| PV0007 | backend is not SQLite (v1 supports SQLite only) |
| PV0008 | a table exists but does not match the catalog (rollback — provision never migrates) |
| PV0009 | a managed name resolves to a non-table object (view/index — never dropped) |
| PV0010 | database open or SQL failure during provisioning |

**Graceful declines (documented v1 limits).** The constant-folder covers scalar
expressions only, so an INIT that uses a pure built-in *call*, or a non-literal
constant *relation* form (`Relation { … } union Relation { … }`), is declined with
a diagnostic rather than evaluated — no example needs them. `Approximate`
arithmetic (`-2e0`) is likewise gated upstream (no float path; T0109) and inherited
here.

## Nested attributes: the decomposition endpoint (designed, not built)

TTM requires relation-valued attributes in database relvars (RM Pre 7;
the ch. 6 `SPQ` relvar with constraint DBC7 is a base relvar), so T0101
is a staging gate, not a policy. The designed answer keeps the honest
heading in the conceptual layer and decomposes in the **`.cdstore`**
(conceptual → physical) mapping — never a serialized blob/JSON column,
which would be opaque to pushdown and backend-specific
([principles.md](principles.md) §1):

- The `.cddb` declares the real heading:
  `base relvar R { a: Integer, b: Relation { x: Text } } key { a };`.
- The `.cdstore` maps the RVA to a **child table** rather than a column:
  the parent table holds the scalar attributes; the child table holds
  the parent's key columns plus the nested heading's columns, primary
  key = all columns (it stores a set). A name collision between a
  parent-key column and a nested column (`b: Relation { a: … }` under a
  parent keyed by `a`) is resolved in the *physical* namespace by the
  column map (`parent: { a: "parent_a" }`) — that mapping layer exists
  precisely so conceptual and physical names diverge.
- **Absence of child rows = the empty nested relation** — the same
  "absence of a fact is absence of a tuple" principle as the no-nulls
  missing-information answer. No `NULL`s, no outer joins; the runtime's
  null-pointer cell *is* the empty relation, so reconstruction patches
  empty-`b` parents for free.
- Reads reconstruct via `coddl_relation_group` over the parent/child
  fetch; writes decompose through the surgical-write engine (one parent
  write plus child writes — write-side batching is allowed). The
  physical form **is** the ungrouped form, so `R ungroup { b }` pushes
  as a plain parent⋈child join (a negative-cost ungroup), and
  predicates over `b` (emptiness, membership) push as
  `EXISTS`/`NOT EXISTS` against the child table — the semijoin
  machinery sqlemit already emits.
- A tuple-valued column is the easy sibling (fixed width — flat leaf
  columns in the same table, the query-side `wrap` trick applied to
  storage); it stays behind T0101 until the mapping syntax lands.

**Decide before** building: the child-table `.cdstore` grammar (a
`rva <attr>: table "…" { parent: { … }, columns: { … } }` clause or
similar) and whether reconstruction happens in the marshaller (grouped
fetch) or the planner (rewrite `R` at the cut to
`(parent join child) group {…}` unioned with the empty-`b` patch for
parents with no child rows — no outer join, RM Pro 4).

## Write policy

A public relvar mapped 1:1 onto a **base** catalog relvar is directly
writable: `ResolvedPublicRelvar::write_policy == WritePolicy::ReadWrite`.
The plan layer sets this from the catalog kind (`RelvarKind::Base`). A
relvar mapped to a catalog **view** stays `WritePolicy::ReadOnly` until
view-updating (`WriteThrough`) semantics land; the lowerer rejects an
assignment to such a target (T0050).

A relational assignment to a writable public relvar is recognized and
emitted as surgical DML — the relvar is never hydrated and written back
(see [sqlemit.md](sqlemit.md#surgical-writes-assignment-rhs-recognition)).
When a `union` inserts rows that live **in the process** (a relation
literal, or a private relvar) rather than in SQL, the runtime ships them
into the table with a batched multi-row `VALUES` insert (`coddl_exec_insert`)
— **no temp table**, so no catalog churn — sized in batches under the
backend's bind-variable limit.

## Transactions

TTM OO Pre 4 forbids autocommit: every database access happens inside
an explicit `transaction [...]` block. The typechecker enforces this
at every public-relvar reference (T0025); the lowerer wraps every
`transaction [...]` body in synthetic `coddl_begin_tx` /
`coddl_commit_tx` calls.

The tx-externs issue real `BEGIN` / `COMMIT` (and `coddl_rollback_tx` a
`ROLLBACK`) on the backing connection, guarded by a process-global depth
counter so nested `transaction [...]` blocks don't issue a nested
`BEGIN` (SQLite has none). Connections are opened **read-write** and
kept live for the program, so a write made inside a transaction
(`coddl_exec`) is visible to a later read (`coddl_query`) in the same
transaction — read-after-write on one shared connection. A pure
in-process transaction (no database registered) is a clean no-op.

### Transaction purity (T0026)

Transactions must be replayable on serialization conflict (when
write-through arrives). Side-effecting builtins (`write_line`,
`write_relation`) are forbidden inside `transaction [...]`. The
`Builtins::OperSig` registry's `Purity` field encodes this; T0026
fires on a side-effecting call inside any transaction depth.

The hello-world-db pattern: the pure read sits inside, the side-
effecting print sits outside, the tuple flows between via Phase 10's
tail-expression mechanism.

```
oper main {} [
    let g = transaction [
        extract (Greetings where id = 1)   // pure read; the tail value escapes
    ];
    write_line { message: g.message };     // side effect outside
];
```

## Rollback discipline

Runtime errors mid-materialization or mid-DML (SQLite open / prepare /
execute failures, NULL columns, type mismatches) `eprintln!` + `abort()`
— the same trap discipline `extract` cardinality uses. An aborting
process leaves any open transaction uncommitted, so SQLite discards it.

`coddl_rollback_tx` issues a real `ROLLBACK` (at the outermost nesting
level). The automatic serialization-replay loop — re-running a
conflicted transaction body — lands when sum types exist in the language
and write-through arrives; it reuses this same extern.

## Slot ownership

Each public relvar gets a private slot global in the binary:
`@<Name>_slot = private unnamed_addr global ptr null`. Materialization
writes the RC pointer there; `RelvarRead` loads + retains;
`RelvarSlotRelease` (emitted in `main`'s epilogue) brings the
materialized payload's refcount to zero so the runtime frees the
allocation.

The runtime tracks slots in a parallel map for defense in depth — if
codegen ever skips the per-relvar release, the connection still
closes at shutdown.

## Linking

`rusqlite` ships with the `bundled` feature in workspace deps. The
runtime crate (`coddl-runtime`) picks it up; the staticlib's link
line therefore needs no extra `-lsqlite3` — libsqlite3 is compiled
in. The driver's `link.rs` is unchanged from Phase 8.

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

Public relvar attributes can be Integer, Boolean, or Text. The
runtime's materialization marshals each cell via
`record_layout::cell_kind` into the canonical byte layout
(`docs/runtime.md`).

Rational, Approximate, Character defer until the runtime adds per-cell
codec entries. Nested **Tuple** and **Relation** cells are fully
representable in relation *values* (query results / intermediates — the
inline sub-region and the RC-pointer cell respectively, see
`docs/runtime.md`), but neither is a legal *storage-backed column*: the
typechecker rejects a relation- or tuple-valued attribute in a `public`
(`.cd`) or `base` (`.cddb`) relvar heading with **T0101** at check time
(a `private` relvar is in-process state and takes any heading). The
designed endpoint for persisting them is decomposition, below.

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

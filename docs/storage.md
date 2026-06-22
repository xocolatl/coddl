# Storage abstraction

Coddl talks to persistent storage through a pair of Rust traits — `Backend` (pure, SQL-emitting half) and `Conn` (effectful, connection half) — so that the rest of the compiler stays backend-agnostic. SQLite is v1 (read-only public relvars hydrated at startup, no-op transactions); Postgres lands later.

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

`materialize_temp` is the boundary primitive for sending in-memory relations back into SQL — see [sqlemit.md](sqlemit.md) "Sending in-memory relations back into SQL" for the per-backend strategy.

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
codec entries. Nested **Tuple** cells are now representable in relation
*values* (query results / intermediates) as inline sub-regions — see the
heading-descriptor `sub` pointer in `docs/runtime.md` — but a tuple-valued
*public-relvar column* stays out of scope (a SQL-backed base table has no
composite column). Nested Relation cells remain out of scope for v1.

## Read-only policy

v1 SQLite-backed public relvars are read-only:
`ResolvedPublicRelvar::write_policy == WritePolicy::ReadOnly`. Writes
against them are a codegen error until view-updating semantics land
(later phase). The plan layer always populates `ReadOnly` for
SQLite-backed relvars today; the discrimination becomes load-bearing
when write-through arrives.

## Transactions (Phase 22)

TTM OO Pre 4 forbids autocommit: every database access happens inside
an explicit `transaction [...]` block. The typechecker enforces this
at every public-relvar reference (T0025); the lowerer wraps every
`transaction [...]` body in synthetic `coddl_begin_tx` /
`coddl_commit_tx` calls.

For v1, transaction tx-externs are **no-ops**. All public-relvar reads
are served from the in-memory slot materialized at startup; SQLite
isn't touched inside the transaction body. The shape exists because:

- The conformance rule (T0025) needs somewhere to land.
- Future write-through reuses the same surface — only the runtime
  bodies grow real BEGIN/COMMIT.

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

## Rollback discipline (v1)

Runtime errors mid-materialization (SQLite open / prepare failures,
NULL columns, type mismatches) `eprintln!` + `abort()` — same trap
discipline Phase 21 used for `extract` cardinality.

User-level rollback (and the serialization-replay loop) lands when sum
types exist in the language and write-through arrives. The
`coddl_rollback_tx` extern is reserved for that.

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

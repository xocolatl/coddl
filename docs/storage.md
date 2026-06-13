# Storage — public relvars, SQLite materialization, transactions

Phase 22 brings public relvars to life: the four-file `.cd` / `.cddb` /
`.cdstore` chain (validated by Phase 16's plan layer) now drives a
runtime materialization pass at program startup, public-relvar
references resolve to in-memory `Relation H` values, and
`transaction [...]` becomes load-bearing with the TTM OO Pre 4
conformance check.

This document is the spec for the storage path; sibling specs:
`docs/typecheck.md` for the surface rules, `docs/procir.md` for the
IR shape, `docs/codegen.md` for backend emission, `docs/runtime.md` for
the C ABI.

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
codec entries. Nested Tuple and Relation cells in public relvars are
out of scope for v1 (and don't make sense for a SQL-backed schema).

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

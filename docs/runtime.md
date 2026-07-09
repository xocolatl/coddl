# Runtime — `libcoddl_runtime`

A Rust crate exposing `extern "C"` entry points, built as a `staticlib` by default (`cdylib` later if plugin loading lands — see [workspace.md](workspace.md)). Compiled Coddl binaries link against it directly: no managed runtime, no garbage collector, no startup overhead beyond the program's own.

This doc has two parts: **architecture** (what the runtime hosts, the execution model, the FFI discipline — the *why*) and the **data layer spec** (RC contract, descriptors, seal, canonical printer — the *what*, exact bytes and signatures). For the IR shapes that drive these calls, see [procir.md](procir.md); for the per-backend emission of descriptors, see [codegen.md](codegen.md) "Per-module heading descriptors."

## Responsibilities

- Own the DB connection pool.
- Cache prepared statements by `plan_id` (compiler assigns at codegen time).
- Marshal LLVM-side value structs ↔ backend parameter binders. `#[repr(C)]` Rust structs match the layout LLVM emits exactly; no marshaling cost beyond field reads, no FFI shim allocation. A single source-of-truth description (see [risks.md](risks.md) risk #8) generates both the LLVM struct text and the Rust `#[repr(C)]` declaration so they can't drift.
- Provide a row iterator the LLVM-emitted code can drive (cursor handle + `coddl_next` returning a tagged-union row).
- Host the **relational runtime library** (called from compiled code), the **runtime RelIR interpreter** (for dynamic plans — see "Reaching the engines" below), and `coddl-sqlemit` as a library (so runtime-built plans lower to SQL through the same code path the compiler uses).
- Map errors to a single error code + thread-local message.

LLVM IR calls these exports as plain C functions. The runtime is where SQLite vs Postgres lives at runtime — the compiled program is backend-agnostic if we're disciplined about not leaking dialect-specific values through the ABI.

## Two execution engines

The runtime hosts two execution engines side-by-side:

- **SQL backend** — runs any subplan rooted in relvars. Subplans become SQL strings via [`coddl-sqlemit`](sqlemit.md) (at compile time for static plans, loaded as a library at runtime for dynamic ones).
- **In-process runtime library** — compiled relational primitives (`coddl_relation_where`, `coddl_relation_extend`, `coddl_relation_join`, `coddl_relation_project`, `coddl_relation_restructure` (surface `wrap`/`unwrap`), …) operating over materialized relations. Tight specialized loops; volcano-style where it pays off (hash joins, sort-merge). Tests and the REPL exercise the same primitives the compiled binary does.

The RelIR optimizer draws the cut between them as close to the leaves as possible: push everything that touches a relvar into SQL, do the rest in-process. See [relir.md](relir.md) "The cut: SQL vs in-process."

## Reaching the engines: compile-time lowering vs. runtime interpretation

Two pathways feed the engines, depending on whether the plan shape is known at compile time:

1. **Statically-known plans** (the common case). Per the cut decision:
   - SQL-rooted subtree → `coddl-sqlemit` produces SQL + a baked `plan_id`; ProcIR holds the call site as `query(plan_id, params)`.
   - In-process subtree → `coddl-execlocal` produces a ProcIR call sequence into the runtime library, which LLVM then specializes per heading.

2. **Plans built at runtime.** Relation-polymorphic operators that can't be monomorphized, or query shapes that depend on a relation passed in at runtime. The runtime hosts both `coddl-sqlemit` (as a library) and a small RelIR interpreter that walks the plan and calls the same runtime-library primitives. Slower than the static path — no LLVM specialization — but unavoidable for genuinely dynamic composition. Specialize at compile time whenever the type system permits (monomorphize on heading like Rust generics); fall back to the runtime planner/interpreter otherwise.

`coddl-execlocal` and the runtime interpreter are **two consumers of the same RelIR**, separated by when they run — compile-time lowering vs. runtime walking. Both end up calling the same runtime-library primitives.

## Lazy semantics

**Relations are lazy.** Scalars are strict. A relation expression is a thunk: it doesn't run at construction, only when something needs its tuples — iteration via `load`, being shipped into another query, being assigned to a relvar, being compared with `=`, being passed to a user-defined operator that consumes it. There is **no `force` keyword** in Coddl; each use re-evaluates the expression against current relvar state. (Laziness is one of the sanctioned design freedoms in [conformance.md](conformance.md) — TTM doesn't address evaluation strategy.) Equality is by value (heading + tuple set), so two relations built by different routes that yield the same tuples are equal regardless of evaluation history (RM Pre 8).

Because relations are first-class, the calling convention has to be uniform: any function that takes a relation must accept a value it can read, re-query, and pass onward. The runtime may memoize a handle's result when it can prove the source relvars haven't changed since the previous use, but that's an optimization invisible to the user.

## Relation values at runtime

A first-class relation is one of three things, behind a single `Relation` handle:

1. **Plan-backed** — a `plan_id` plus its already-bound parameters. The default. Each use re-evaluates against current relvar state. The runtime may memoize the result when source-relvar invalidation is provably absent, but that's an optimization, not a semantic guarantee.
2. **Materialized** — a runtime-owned buffer of tuples (arena-allocated, or a backend temp table for large ones — see [risks.md](risks.md) risk #1). Used when tuples are already in memory: relation literals (`Relation { tup1, tup2 }`), results of in-process evaluation, in-memory inputs being shipped back into SQL via temp table.
3. **Cursor** — a live result set being drained. Compiler-only optimization for `load … order [ … ]` flows where the sequence is consumed once and never escapes — lets the runtime stream rows from the backend into the sequence slot-by-slot instead of buffering them all.

## Plan registration

- Each compile-time query becomes a `plan_id` (a dense `u32` codegen assigns — its own namespace, *not* `coddl_sqlemit::PlanId`, which is a 64-bit text hash) carrying a baked SQL string, a bind-parameter count, and a result heading.
- The program prologue registers each logical database once, then each static plan:
  - `coddl_register_database(name, path)` — binds a `database <name>;` handle to its resolved connection path (codegen resolves the path via `coddl_resolve_op_field` first). By TTM a database binds exactly one backend and is the scope of a transaction, so the entry is 1:1 with a connection; `coddl_begin_tx`/`coddl_commit_tx` reuse it when write-through lands.
  - `coddl_register_plan(plan_id, db_name, sql, param_count, result_desc)` — the plan references its database by name. No separate parameter-type table: bind parameters self-describe via `CoddlParam.kind` at the call site.
- At the force point, codegen calls `coddl_query(plan_id, params, n) -> *Relation`: it resolves the plan's database, fires the prepared statement (cached by SQL text per connection) on a pool connection — so the audit `trace` hook captures it — marshals the rows into an RC relation (no seal: the query's `DISTINCT`/key already makes the rows a set), and returns the pointer (the same shape `coddl_relation_where` returns; consumed by `coddl_extract_check_cardinality`). It aborts on any hard error (unknown plan/database, parameter mismatch, prepare/step failure, NULL cell); the `*Relation` return has no status channel. (`TempRelRef`s built from in-memory relations join the parameter list once temp-table shipping lands.)
- Dynamic plans (relation-polymorphic, runtime-shaped) register later — the runtime interpreter assigns plan IDs the first time it lowers a previously-unseen plan shape and caches by shape from then on.

## Iteration: the `load` primitive

There is no tuple-at-a-time access to relvars or relations (RM Pro 7). The only iteration primitive is `load`, which forces the relation, imposes an order, and writes the tuples into a local sequence:

```
var names;
load names from rnames order [ asc name ];
for i := 0 to names.cardinality {} - 1 do [
    -- process names[i]
];
```

`names` here has type `Sequence Tuple { name: Text }` — an ordered list of tuples whose element type is read off the source relation's heading. The unannotated `var names;` is the idiom: `load` is that binding's definite-assignment site, so the element type is inferred (the explicit `var names: Sequence Tuple { name: Text };` stays legal as the checked variant). The counted `for` loop walks the sequence by position (0-based).

The order spec is an **ordered** bracket-list of sort items — `[ asc name, desc other ]`, each an optional `asc`/`desc` direction (bare defaults to `asc`) before an attribute name — the same `<sort-item>` grammar shared with window ranking (`rank [ asc score ]`). `load` has no projection slot of its own: to keep only some attributes, project in the source expression (`load names from ( rnames project { name } ) order [ asc name ]`).

`load` is the syntactic and semantic gate between the set-oriented and procedural worlds: it forces the relation, imposes an order (the order is part of the operation, not a property of the relation), and writes the tuples into a local sequence. This is the *only* sanctioned path; the compiler rejects any other attempt to step through tuples one at a time.

The **reverse** form `load <private-relvar> from <sequence>` (no `order`) runs the other direction: it seals a processed sequence's element tuples back into a relvar as a set. It lowers to `coddl_relation_from_sequence(seq, desc)` — the inverse of `coddl_load_ordered`: it copies the sequence's records, retains their `Text` cells, and **seals** (sort + dedup, RM Pro 1, 3, restoring the canonical tuple order and dropping duplicates), then stores the fresh relation into the target private relvar's in-memory slot. The source's type picks the direction at lowering (a `Relation` source is forward, a `Sequence` source is reverse); the target is a private relvar (a public-relvar reverse — a SQL DML replace — is not yet wired). An empty sequence yields an empty relation.

For a materialized (in-process) source, the forward `load` lowers to the runtime entry point `coddl_load_ordered(rel, rel_desc, keys, key_count)`. It is the relation seal's sort core **minus dedup, plus retain**: a stable sort of the record indices by the order keys (each `keys` entry an index into `rel_desc.attrs[]` with bit 31 for a descending key; comparison via the shared `cmp_cell`), then a fresh `CoddlKind::Sequence` payload allocated with the *same descriptor* — a `Sequence` is physically an unsealed relation, so each element record is a source tuple — into which the records are permuted in sorted order and every surviving `Text` cell is retained (the sequence co-owns the shared payloads; the source is left unchanged). Records equal on every order key keep their input order; no dedup runs, so a `Sequence` preserves duplicates and position. A db-relvar-rooted source instead rides a trailing SQL `ORDER BY` on the pushed `SELECT` (see [sqlemit.md](sqlemit.md) "Ordered `load` pushdown"): the rows arrive already ordered, so the lowerer emits `coddl_load_ordered` with an **empty** key array — the stable no-op sort just wraps the ordered rows into the `Sequence`. Force-then-sort in-process is the fallback for materialized sources (and for shapes the order can't ride — a root set-op or `tclose`).

The reverse direction — `load <relvar target> from <sequence var ref>` (no `order` clause) — assigns the (set-valued) projection of the sequence's tuples back into a relvar. Useful for round-tripping procedurally-built sequences into relational form.

## Multiple assignment

`A1, A2, …, An ;` is a single statement with the semantics of RM Pre 21 (see [conformance.md](conformance.md)):

1. Expand all syntactic shorthands (INSERT/UPDATE/DELETE/`THE_C` pseudovariable) into `target := expr` form.
2. Fold duplicate targets by rewriting `Vq := Xq` as `Vq := WITH Xp AS Vq : Xq` and dropping the earlier assignment. Repeat.
3. Evaluate every RHS expression. Capture results.
4. Apply all assignments to their targets atomically.
5. Check every applicable database constraint at the end of the whole MA (not between assignments).

ProcIR therefore has a `multi_assign` primitive, not just a sequence of individual assigns. The runtime evaluates all RHSs first (against the pre-MA database state), then commits the writes in one logical step, then runs constraint checks.

## Transactions

`BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK` are explicit (OO Pre 4). Nested transactions are supported (OO Pre 5): a nested `BEGIN` starts a child; child `COMMIT` is conditional on the parent; child `ROLLBACK` undoes only the child's work. The SQL backend uses `SAVEPOINT` for child transactions, but the runtime tracks the parent/child relationship explicitly because SQL `SAVEPOINT` doesn't model true nesting.

A relation handle captured before a write within the same transaction **re-evaluates on use** and so sees post-write state — the consequence of the lazy semantics above. If the user wants to freeze the pre-write tuples, they `load` the relation into a sequence (or assign it to a private relvar) before the write. This avoids any pre-image / copy-on-write machinery in the runtime.

## Performance posture

The runtime is on the hot path for every relation operation that crosses the SQL/in-process boundary. Allocate per-query with a bump arena; free at query completion (a typed arena per heading is the natural unit). Avoid `Box<dyn Trait>` on tuple values; specialize over heading or use a fixed-size value layout. Pull row buffers from prepared statements directly into Coddl tuple memory where the dialect permits — zero-copy is the default, copy only when alignment or lifetime forces it. Abort-on-panic (`panic = "abort"`) for release builds: smaller stack-unwinding tables and a single failure mode at the FFI boundary.

## FFI boundary discipline

Values crossing into LLVM-emitted code are `#[repr(C)]` or primitive. No Rust enums-with-payload across the boundary unless tagged-C-style. No `Vec`/`String` raw pointers without an explicit owner declaration. The discipline is enforced by a single layout-description module in the runtime crate, mirrored from there into LLVM codegen.

## Portability — backends as features

SQL backends are Cargo features on the runtime crate (`sqlite`, `postgres`). `wasm32-*` builds drop these — the C dependencies of `rusqlite`/`postgres` don't link to `wasm32-unknown-unknown` — and either run with only the in-process runtime library (materialized relations, no DB) or proxy SQL through wasm host imports if a wasmtime/JS host is in play. Same crate split, different feature set at build time. See [workspace.md](workspace.md) for the build configuration.

## Why Rust over plain C for the runtime

A C `libcoddl_runtime` would be ~50–300 KB smaller as a `staticlib`; nothing else recommends it for our case. The two non-trivial runtime jobs — the runtime RelIR interpreter and the RelIR→SQL emitter — are tree walks over sum types, which Rust enums + pattern matching handle naturally and C reinvents painfully. The SQL emitter is the same crate the compiler uses; a C runtime would either duplicate it (two versions to keep in lockstep forever — against [long-term planning](principles.md)) or call into a Rust crate (a Rust runtime with extra steps). Connection pooling and prepared-statement caching are markedly less code against `rusqlite`/`postgres` than against `sqlite3.h`/`libpq-fe.h`. Where binary size or non-Rust embedding ever does matter, the hot value-marshaling layer can drop to `#![no_std]` Rust or a small C TU without touching the interpreter or emitter — picking Rust now doesn't lock out a leaner future.

---

# Data layer spec

The remainder of this doc pins the exact bytes and signatures the runtime exposes today — the RC contract, the per-heading descriptor layout, the seal discipline, the canonical printer format. Phase 19+ shipping reality.

## RC contract

Every runtime-allocated payload is preceded by a 24-byte
`CoddlRcHeader` at a fixed negative offset (`HEADER_SIZE`):

```c
struct CoddlRcHeader {
    uint64_t                    rc;       // refcount
    const CoddlHeadingDesc*     desc;     // descriptor for relations; null otherwise
    uint32_t                    kind;     // CoddlKind discriminant
    uint32_t                    length;   // tuples in a relation; aux for other kinds
};
```

Codegen sees only the payload pointer; the runtime reaches the header
via `payload.sub(HEADER_SIZE)`.

### Immortal sentinel

`IMMORTAL_RC = u64::MAX` marks payloads that live in read-only
segments (string literals today, future compile-time data). Retain
and release short-circuit to no-ops on this sentinel so the same
calling convention works for both heap-managed and immortal values.

### Public API

| Symbol                     | Signature                                          | Semantics                                                                                                                |
|----------------------------|----------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------|
| `coddl_rc_alloc`           | `(payload_size: u64, length: u32, kind: u32, desc: ptr) -> ptr` | Allocate `HEADER_SIZE + payload_size` bytes, write a fresh header with `rc = 1`, return the payload pointer.             |
| `coddl_rc_retain`          | `(ptr) -> ()`                                       | Increment `rc`. No-op on null and on immortal payloads.                                                                  |
| `coddl_rc_release`         | `(ptr) -> ()`                                       | Decrement `rc`. On zero: dispatch the drop walker by `kind`, then free the entire block (`header + payload`).            |
| `coddl_relation_seal`      | `(payload, desc) -> ()`                             | Sort the relation's records by byte-wise comparison, then adjacent-dedup in place; updates the header's `length`.        |
| `coddl_write_relation`     | `(payload, desc) -> ()`                             | Print the relation, one tuple per line, in canonical heading order. Empty relation writes zero bytes.                    |
| `coddl_read_line`          | `(prompt_ptr, prompt_len, len_out) -> ptr`          | Write `prompt` (a `Text` `(ptr,len)`) to stdout without a newline, flush, then read one stdin line. Returns a fresh heap `Text` payload (trailing `\n`/`\r\n` stripped) and writes its length into `*len_out`. EOF → empty `Text`. The length crosses back through `len_out` because a fat pointer can't return by value (same idiom as `coddl_resolve_op_field`); inherits the scalar-`Text` leak (`docs/memory.md`). |
| `coddl_relation_where`     | `(src, desc, pred_fn) -> ptr`                       | Restrict `src` by `pred_fn(record_ptr) != 0`. Returns a fresh RC-managed relation (rc=1) holding the matching rows in the input's original order. Worst-case alloc; header `length` trimmed to the actual count. No re-seal — restricting a duplicate-free relation can't introduce duplicates (RM Pro 3 preserved). |
| `coddl_relation_extend`    | `(src, src_desc, result_desc, synth_fn) -> ptr`     | Extend `src` with computed attributes. For each source record, `synth_fn(src_record, dst_record)` fills the whole widened (result-heading) record — surviving cells permuted to their result offsets plus the new computed cells — so this stays oblivious to the layout (the synthesized per-tuple helper owns it). Allocates `result_record_size × count`, then **re-seals** (computing a column can change sort order and collapse formerly-distinct rows → RM Pro 3). Computed `Text` cells inherit the scalar-Text leak (`docs/memory.md`); surviving `Text` cells are shared by value, like `rename`. |
| `coddl_extract_check_cardinality` | `(src, desc) -> ptr`                          | TTM RM Pre 10 cardinality check: if `src`'s header `length` is exactly 1, returns a pointer to the single record's bytes (which equals `src` itself, since records start at the payload base). Otherwise writes `"coddl: extract: expected exactly 1 tuple, got N"` to stderr and calls `std::process::abort()`. The caller reads each attribute via the descriptor before releasing the source — the lowering's "Extract then Release" order guarantees the buffer is live during attribute reads. |
| `coddl_seq_index`          | `(seq, index: i64) -> ptr`                          | 0-based `Sequence` index (`s[i]`). Returns the element *record* pointer `payload + index * record_size` (`record_size` read from the header's `desc`); the caller `AttrLoad`s the synthetic `value` cell at offset 0. Bounds-checked here (codegen has no branching yet): a null `seq`, or `index` outside `0 ..< length`, writes a diagnostic to stderr and calls `std::process::abort()` (non-zero exit), like `coddl_extract_check_cardinality`. Borrows the header only; touches no refcount (the caller retains a `Text` element into an owned copy). |
| `coddl_sqlite_relvar_init` | `(relvar_name, relvar_name_len, db_path, db_path_len, table, table_len, columns, column_lens, column_count, desc, slot) -> CoddlStatus` | Materialize one public relvar from SQLite at startup. Opens the connection read-only (one per resolved path, via `OnceCell`), prepares `SELECT <columns> FROM <table>` in heading-canonical order, steps rows, marshals each cell into a `record_layout` buffer, allocates via `coddl_rc_alloc` (no seal: the table's rows are already a set, unique by the relvar's key), writes the RC pointer into `*slot`, and registers the slot in the runtime's slot map. NULL columns and type mismatches abort with a clear stderr message (RM Pro 4). |
| `coddl_resolve_op_field`   | `(env_name, env_name_len, default, default_len, out_len) -> ptr` | Operational-field resolver. Reads `getenv(env_name)`; on hit, returns a pointer into a per-process intern (writes the length into `*out_len`). On miss, returns `default` and writes `default_len`. The env-var convention is `CODDL_<DBNAME>_<FIELD>` (e.g. `CODDL_GREETINGS_FILE`); the database name comes from `database <name>;`. |
| `coddl_begin_tx`           | `() -> CoddlStatus`                                  | Begin a transaction. v1 no-op (the materialized in-memory slot is the source of truth; SQLite isn't touched inside a transaction body). Real BEGIN ships with write-through. |
| `coddl_commit_tx`          | `() -> CoddlStatus`                                  | Commit a transaction. v1 no-op; see `coddl_begin_tx`. |
| `coddl_rollback_tx`        | `() -> CoddlStatus`                                  | Roll back a transaction. v1 no-op; reserved for the serialization-replay loop. |

The lowerer and both backends agree on the same set of symbol names
and signatures; the `BUILTIN_EXTERNS` table in `coddl-procir::lower`
maps each generically-lowered user-facing builtin (`write_line`,
`read_line`) to its linkage name. (`write_relation` is special-cased to
`Inst::WriteRelation` rather than a generic `Inst::Call`, so it isn't in
that table.)

### Drop walker

When `coddl_rc_release` brings the refcount to zero, the runtime
dispatches on the header's `kind`. For `CoddlKind::Relation`,
`drop_relation_payload` iterates each record and releases every `Text`
cell — recursing through inline `Tuple` cells to their `Text` leaves via
the shared `walk_text_cells` traversal (the same kind-dispatch shape as
the printer's `print_cell`). Integer / Boolean cells carry no heap
pointer and are skipped; an immortal-literal `Text` cell sees `rc ==
IMMORTAL_RC` and no-ops.

This release is balanced by the **one-reference-per-cell invariant**:
every `Text` cell a relation holds was given exactly one owned reference
at production. The producers:

- **retain-on-store** — codegen retains a scalar `Text` as it is stored
  into a relation-literal cell (`emit_attr_store` / `store_attr`);
- **retain-on-copy** — each relation operator that copies cells from its
  input(s) calls `retain_text_cells` over its output *before* any
  `coddl_relation_seal` (`where` / `project` / `rename` / `join` /
  `union` / `minus` / `restructure` / `tclose`);
- **move-in** — a cell produced fresh at rc=1 (an `extend`-computed
  concat, a marshaled SQLite text) is stored without a retain.

`coddl_relation_seal`'s dedup (`dedup_records` with `release_dropped_text
= true`) releases the `Text` cells of each duplicate row it discards, so
the retain-on-copy/store of a dropped row is balanced. `tclose` runs its
*intermediate* dedups over un-retained working copies (`release_dropped_text
= false`) and retains its final output once. The descriptor's `desc`
pointer on the header supplies the layout; a null `desc` (or zero-width
record) makes the walker a no-op.

## Heading descriptors

Each unique `Heading` the typechecker reasons about gets one
descriptor emitted by each backend. The C struct layouts:

```c
struct CoddlAttrDesc {
    const uint8_t* name;       // not null-terminated; length in `name_len`
    uint32_t       name_len;
    uint32_t       kind;       // CoddlAttrKind discriminant
    uint32_t       offset;     // byte offset within a record
    // natural padding to multiple of pointer alignment
    const CoddlHeadingDesc* sub; // Tuple cell: nested descriptor; else NULL
};

struct CoddlHeadingDesc {
    uint32_t                attr_count;
    uint32_t                record_size;  // bytes per record
    const CoddlAttrDesc*    attrs;        // attr_count entries, canonical order
};
```

`CoddlAttrKind` enumerates the supported cell types:

| Value | Variant     | Cell width (bytes) | Encoding                            |
|-------|-------------|--------------------|-------------------------------------|
| 0     | `Integer`   | 8                  | `i64` host-endian                   |
| 1     | `Boolean`   | 8                  | `i64`; 0 = false, 1 = true          |
| 2     | `Text`      | 16                 | `(ptr: *const u8, len: usize)`      |
| 3     | `Character` | 8                  | Unicode codepoint (`u32`) zero-extended to `i64`; SQL binds/stores it as an integer codepoint |
| 4     | `Approximate` | 8                | IEEE-754 double, canonical bits (NaN → one pattern, `−0` → `+0`); SQL binds/stores/reads as `REAL`, with SQLite `NULL` as the encoding of `NaN` (retrieval decodes `NULL`→`NaN`) |
| 5     | `Rational`  | 32                 | reduced `(i128 numer, i128 denom)` (num @ 0, den @ 16); canonical ⇒ byte-compare is value-equality; SQL binds/stores/reads as canonical `TEXT "n/d"` |
| 10    | `Tuple`     | Σ components       | inline sub-region; `sub` → nested descriptor (0-based offsets) |

A `Tuple` cell is an **inline nested cell**: its components occupy a
contiguous sub-region whose width is the sum of their widths, and the
attribute's `sub` pointer carries the nested `CoddlHeadingDesc` (with
0-based offsets within the sub-region). The printer and the
content-aware record comparator both recurse through `sub`, adding the
parent cell's base offset — so two tuple cells with equal `Text`
content but different pointers still dedup (RM Pro 3). Every in-process
operator is tuple-aware: per-cell *copies* (`project`/`rename`/`join`/
`tclose`) size a cell with `cell_width_desc` (a `Tuple` is its whole
`sub.record_size` blob, not 8 bytes), and *equality* (join shared key,
tclose edge match, seal/dedup) goes through one `cmp_cell(ra, off_a, rb,
off_b, attr)` that recurses content-aware. `wrap`/`unwrap` themselves are
`coddl_relation_restructure`: flatten both descriptors to leaf cells,
match by name, permute each record into the destination layout.

A **pushed** wrap/unwrap needs no restructure pass: the SQL returns the
flat leaf columns and the query marshaller (`marshal_rows`, sqlite.rs)
flattens the result descriptor to leaf cells (recursing into `Tuple`
cells, accumulating the base offset) and writes the i-th SQL column into
the i-th leaf's offset — the same depth-first leaf order `record_layout`
and the pushed SELECT use, so the positional mapping reconstructs the
inline nested cell directly. Sub-word packing
(Boolean → 1 bit, Byte → 1 byte) is deferred. Relation-as-cell (kind 11)
is reserved but not yet emitted.

The `coddl-procir::layout` module owns the layout computation that
backends consume. Both backends and the runtime must agree on:

- Canonical (name-sorted) attribute order, matching
  `Heading::attrs()`.
- Per-cell width and host-endian encoding.
- Record stride (sum of cell widths; no per-record padding today).

If any of those drift, the runtime walks records by the wrong stride
or reads cells out of bounds. The hygiene gate doesn't catch this —
test discipline does. Phase 19's e2e suite is the smallest example
that exercises the full pipeline.

## Seal discipline

`coddl_relation_seal` enforces RM Pro 3 (no duplicates in a
relation **built in process** — literals, `project`, `join`/`times`)
in two steps:

1. **Sort.** Records are sorted by byte-wise comparison of their
   record buffers — purely to bring equal records adjacent for the
   dedup pass. The resulting order is **not meaningful**: a relation
   is a set with no tuple order (RM Pro 1), so output order is
   unspecified and two backends agree on a relation as a *set* of
   tuples (RM Pre 8), not byte-for-byte. (Integer/Boolean cells sort
   by their content bytes, which is cross-backend stable; Text cells
   sort by their `(ptr, len)` pair, so a Text-leading relation's order
   differs across backends — harmless precisely because order is
   unspecified.)
2. **Adjacent dedup.** Equal-byte adjacent records collapse; the
   header's `length` shrinks accordingly.

Byte-equality is the right *dedup* equivalence for Phase 19's cell set:
Integer/Boolean cells are content-encoded directly; Text cells hold
`(ptr, len)` pairs that — for compile-time string literals —
deduplicate by content at codegen time, so equal-content strings
share their pointer. If runtime-allocated Text owners land later
(user-defined strings, computed concatenation), seal will need a
content-aware comparison; the current byte-wise sort would then
under-dedupe in safe ways (treat semantically-equal strings as
distinct) but never over-dedupe.

The **SQL path does not seal**: the backend already returns a
duplicate-free set (`SELECT DISTINCT`, or a surviving key that let
`needs_distinct()` elide it; a relvar's table rows are unique by its
key), so `sqlite::finalize_relation` materializes the rows as-is —
re-sorting/re-deduping would be redundant work for an order nothing
consumes.

## Canonical printer

`coddl_write_relation` walks the relation's payload and writes one
tuple per line (in storage order, which is unspecified — RM Pro 1):

```
{a: 1}
{a: 2}
```

The format mirrors the surface tuple-literal syntax (Phase 18). Each
attribute appears in canonical heading order; the per-cell formatter
dispatches on `CoddlAttrKind`:

- `Integer` / `Boolean` — decimal / `true`/`false`.
- `Character` — `'c'`, single-quoted, escaping non-printables (matching
  the `Character` literal syntax).
- `Approximate` — exponent form (`1.5e0`), which re-reads as an
  `Approximate` literal.
- `Rational` — `n/d` (the reduced pair).
- `Text` — `"..."` with the raw bytes inside.
- Tuple / Relation cells — print as `{...}` placeholder. The
  recursive printer lands when Phase 20+ workflows demand it.

Empty relation: zero bytes (no trailing newline). The byte-equality
e2e test relies on this — a non-empty header would diverge across
implementations.

## Debug instrumentation

`coddl-runtime::live_allocations()` returns the count of currently
allocated RC blocks. In `cfg(debug_assertions)` builds it tracks
every `coddl_rc_alloc` / `coddl_rc_release`; release builds return
0. The runtime unit tests assert this counter returns to zero after
allocate-retain-release cycles.

**End-to-end leak gate.** In a balanced program the counter is 0 by
program end — every `coddl_rc_alloc` is matched by a release before
`main` finishes (relvar slots and heap locals are released in the
epilogue). So, under `cfg(debug_assertions)` and gated by the
`CODDL_LEAK_CHECK` env var, `coddl_runtime_shutdown` reads
`live_allocations()` and, when non-zero, prints
`coddl: leaked <N> allocation(s)` to stderr and **exits non-zero**.
The driver e2e tests compile the program against the **debug** runtime
staticlib; the `coddl()` test helper sets `CODDL_LEAK_CHECK=1` on every
invocation (it inherits down to the binary `coddl run` spawns), so the
gate is **default-on across the whole e2e suite** — a leaking program
exits non-zero and trips the `status.success()` assert every run test
already makes, turning a refcount imbalance into a red test instead of
silent memory growth. Caveats worth remembering: it is dynamic (only
paths a test runs), counts only `coddl_rc_alloc` blocks (not the
runtime's Rust-side registries), fires only at `main` shutdown (mainless
`coddl-web` handlers are uncovered), and a zero balance can still hide
two compensating errors. The gate is env-gated and compiles out of
release builds entirely.

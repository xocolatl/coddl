# `coddl-sqlemit` — RelIR → SQL emission

`coddl-sqlemit` consumes RelIR (see [relir.md](relir.md)) and emits SQL strings. It's the SQL-side counterpart to `coddl-execlocal`: both consume the same RelIR; `coddl-sqlemit` produces a SQL string + bind site (baked into the binary as a `plan_id`), `coddl-execlocal` produces a sequence of ProcIR calls into the in-process runtime library.

`coddl-sqlemit` runs in two places:

- **At compile time**, as the back end of the static lowering path for SQL-rooted RelIR subtrees.
- **At runtime, as a library** loaded into `coddl-runtime`, so dynamic plans (built from relation values only known at runtime) lower to SQL through exactly the same code path.

There is one implementation. The compiler and runtime share it directly (no FFI seam, no duplication — see [principles.md](principles.md) long-term planning).

## The cut decision drives what reaches `coddl-sqlemit`

The RelIR optimizer assigns each subtree a storage origin and draws the cut as close to the leaves as possible (see [relir.md](relir.md) "The cut"). A subtree where every leaf is a public relvar in the same backend is a candidate to push to SQL — `coddl-sqlemit` consumes the subtree and produces one SQL plan. A subtree with materialized leaves stays in-process; `coddl-execlocal` (or the runtime interpreter) takes it.

A mixed-origin subtree gets a `MaterializeAtBoundary` node inserted by the optimizer. The boundary is the contact point: one side becomes a temp-table populated by the in-process engine, or the SQL side materializes into a runtime-owned buffer before joining in-process. The decision is the cost model's.

## Mandatory SQL emission rules

These are not optimizations; they're correctness requirements imposed by TTM's Prescriptions and Proscriptions (see [conformance.md](conformance.md)). The emitter enforces all of them by construction — never as a post-pass, never as a "we should add a check for this." Violating one breaks Coddl's conformance contract.

| Rule | Reason |
|---|---|
| The result of every emitted query is a **set** — no duplicate rows. `SELECT DISTINCT` is emitted **unless the compiler proves the result is already a set** (see the note below); `UNION` never `UNION ALL`. | RM Pro 3 (no duplicates). |
| Always enumerate columns explicitly in a deterministic (name-sorted) order. Never emit `SELECT *`. Never emit `INSERT … VALUES` without a column list. Never emit bare `UNION` / `INTERSECT` / `EXCEPT` — use `… CORRESPONDING …` (or simulate by aligning explicit lists). | RM Pro 1 (no ordinal attribute order). |
| Never declare a column `NULL`; always `NOT NULL`. Reject SQL DDL paths that would allow nullable columns. | RM Pro 4 (no nulls). |
| Outer joins are forbidden in lowered SQL. Coddl source has no construct that compiles to one; the type system can't express "this attribute might not have a value" as an attribute property. | RM Pro 4. |
| Aggregates: wrap to honor identity (OO Pre 6). Emit `COALESCE(SUM(x), 0)`, `COALESCE(MAX(x), CAST(<lowest> AS T))`, etc. AVG over empty is undefined — emit a guarded expression that signals an error if the result would be queried. | OO Pre 6. |
| Relational assignment `R := expr` compiles inside a transaction to `DELETE FROM R; INSERT INTO R (…) SELECT … FROM (…)` (or `TRUNCATE` + `INSERT` on Postgres). Single-tuple INSERT/UPDATE/DELETE in source desugars to a relational-assignment expression first; the backend never sees the singular form. | RM Pre 21, RM Pro 7. |
| Always emit explicit `BEGIN` / `COMMIT`. Never rely on SQL's implicit transaction start. Set constraints `IMMEDIATE` at session start; never `INITIALLY DEFERRED`. | OO Pre 4; RM Pre 23 (statement-boundary check). |
| Avoid SQL `CHARACTER` / `CHAR(n)` entirely; use `VARCHAR`/`TEXT`. SQL's `CHAR` pads with trailing blanks under equality — violates RM Pre 8. | RM Pre 8. |
| Every base table emitted from a relvar has a `PRIMARY KEY` from the relvar's declared candidate key (RM Pre 15). The candidate key with the fewest attributes wins ties; the rest become `UNIQUE`. The compiler verifies minimality before emission. | RM Pre 15. |
| `reltrue` / `relfalse` (nullary relations): emit as `(SELECT) WHERE TRUE` / `WHERE FALSE`. SQLite/Postgres tolerate this; non-conforming backends would need a synthesized dummy column. | RM Pro 5. |
| SQLite-specific: Coddl `Boolean` lowers to SQL `INTEGER CHECK (col IN (0, 1))`. Avoid the SQLite affinity-coercion footguns by always `CAST`-ing on `INSERT`. | dialect quirk. |

### `DISTINCT` elision (key/cardinality-driven)

The **set invariant** above is non-negotiable and enforced by construction. The `DISTINCT` *keyword* is the mechanism, and it's emitted only when the result might actually contain duplicates — eliding a *provably redundant* `DISTINCT` upholds RM Pro 3, it doesn't relax it (the output is still always a set; we drop a no-op the database would otherwise pay a sort/hash for).

`coddl-relir` carries each relvar's declared candidate keys on its `RelvarRef` leaf and exposes `RelExpr::needs_distinct()`; `emit_select` drops `DISTINCT` when it returns false. A query needs no `DISTINCT` when either:

- a **candidate key survives** into the projected heading (`surviving_keys()` non-empty) — no two distinct rows can collide on the kept columns; or
- a restriction **bounds cardinality to ≤ 1** (`card_le_one()`) — an equality on a full candidate key (v1: a single-attribute key pinned by `attr = literal`), so any projection is trivially duplicate-free.

Otherwise (a projection that drops below every candidate key with unbounded cardinality) `DISTINCT` stays. A keyless or not-yet-analyzable leaf is conservative — it keeps `DISTINCT`. This is a compile-time down payment on candidate-key inference (VSS 3): today only declared keys propagate; inferred FDs extend `surviving_keys()`/`card_le_one()` later without touching `emit_select`.

## Dialect surface

Keep emitted SQL to a **portable subset** (CTEs, window functions, standard joins). Isolate dialect divergence behind backend methods — `emit_select` returns dialect-specific text, but the same RelIR plan should produce semantically equivalent results across backends.

Per-backend golden-file tests live in `tests/golden/`: `RelIR plan → expected SQL` per dialect. The validation matrix (see [validation.md](validation.md)) confirms that whatever the backend differences in the SQL text, the *results* match.

## Sending in-memory relations back into SQL

A relation value built or filtered in procedural code may be the input to a subsequent query. The `Conn::materialize_temp(heading, rows) -> TempRelRef` trait method (see [storage.md](storage.md)) ships an in-memory relation to a temp table the next query can reference as if it were a relvar.

- **SQLite**: temp tables / `carray`. Start with temp tables.
- **Postgres**: `UNNEST` over arrays for small relations; `COPY` into a temp table for larger ones; table-valued parameters via temp tables are the portable bet.

SQL emission can reference a `TempRelRef` the same way it references a base table — the `coddl-sqlemit` doesn't need a special path for boundary materialization; it just sees another relvar-shaped leaf.

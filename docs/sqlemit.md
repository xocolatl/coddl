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
| Joins name their key explicitly: `INNER JOIN … USING (k, …)` over the shared attributes the **typechecker** computed; disjoint headings (`times`) emit `CROSS JOIN`. Never `NATURAL JOIN`. | The `.cddb`, not the live schema, is the source of truth; see the note below. |
| Aggregates: wrap to honor identity (OO Pre 6). Emit `COALESCE(SUM(x), 0)`, `COALESCE(MAX(x), CAST(<lowest> AS T))`, etc. AVG over empty is undefined — emit a guarded expression that signals an error if the result would be queried. | OO Pre 6. |
| Relational assignment `R := expr` compiles inside a transaction to `DELETE FROM R; INSERT INTO R (…) SELECT … FROM (…)` (or `TRUNCATE` + `INSERT` on Postgres). Single-tuple INSERT/UPDATE/DELETE in source desugars to a relational-assignment expression first; the backend never sees the singular form. | RM Pre 21, RM Pro 7. |
| Always emit explicit `BEGIN` / `COMMIT`. Never rely on SQL's implicit transaction start. Set constraints `IMMEDIATE` at session start; never `INITIALLY DEFERRED`. | OO Pre 4; RM Pre 23 (statement-boundary check). |
| Avoid SQL `CHARACTER` / `CHAR(n)` entirely; use `VARCHAR`/`TEXT`. SQL's `CHAR` pads with trailing blanks under equality — violates RM Pre 8. | RM Pre 8. |
| Every base table emitted from a relvar has a `PRIMARY KEY` from the relvar's declared candidate key (RM Pre 15). The candidate key with the fewest attributes wins ties; the rest become `UNIQUE`. The compiler verifies minimality before emission. | RM Pre 15. |
| `reltrue` / `relfalse` (nullary relations): emit as `(SELECT) WHERE TRUE` / `WHERE FALSE`. SQLite/Postgres tolerate this; non-conforming backends would need a synthesized dummy column. | RM Pro 5. |
| Transitive closure (`tclose`) emits a two-CTE `WITH RECURSIVE` with the operand defined once (params appear once) and `UNION` (never `UNION ALL`) so the closure is a set. It is emitted only at the statement root — a `WITH`-prefixed query can't be a compound operand or a `FROM` subquery, so a nested/operand `TClose` declines and decomposes in-process. | RM Pro 3; the one irreducible Algebra-A operator. |
| SQLite-specific: Coddl `Boolean` lowers to SQL `INTEGER CHECK (col IN (0, 1))`. Avoid the SQLite affinity-coercion footguns by always `CAST`-ing on `INSERT`. | dialect quirk. |

### `DISTINCT` elision (key/cardinality-driven)

The **set invariant** above is non-negotiable and enforced by construction. The `DISTINCT` *keyword* is the mechanism, and it's emitted only when the result might actually contain duplicates — eliding a *provably redundant* `DISTINCT` upholds RM Pro 3, it doesn't relax it (the output is still always a set; we drop a no-op the database would otherwise pay a sort/hash for).

`coddl-relir` carries each relvar's declared candidate keys on its `RelvarRef` leaf and exposes `RelExpr::needs_distinct()`; `emit_select` drops `DISTINCT` when it returns false. A query needs no `DISTINCT` when either:

- a **candidate key survives** into the projected heading (`surviving_keys()` non-empty) — no two distinct rows can collide on the kept columns; or
- a restriction **bounds cardinality to ≤ 1** (`card_le_one()`) — an equality on a full candidate key (v1: a single-attribute key pinned by `attr = literal`), so any projection is trivially duplicate-free.

Otherwise (a projection that drops below every candidate key with unbounded cardinality) `DISTINCT` stays. A keyless or not-yet-analyzable leaf is conservative — it keeps `DISTINCT`. This is a compile-time down payment on candidate-key inference (VSS 3): today only declared keys propagate; inferred FDs extend `surviving_keys()`/`card_le_one()` later without touching `emit_select`.

### `USING` over `NATURAL JOIN`

`join` lowers to `… INNER JOIN … USING (k, …)`, naming the exact shared attributes the typechecker computed from the catalog (`coddl-sqlemit`, `RelExpr::And`); a disjoint-heading `times` lowers to `CROSS JOIN`. The emitter never produces SQL's `NATURAL JOIN`. The two read as interchangeable — both coalesce each shared column into a single output column — but they disagree on *who chooses the join key*, and that is a correctness boundary.

`USING (k)` freezes the key into the emitted SQL: the join runs on whatever the `.cddb` declares the shared attributes to be, fixed at compile time and baked into the `plan_id`. `NATURAL JOIN` re-derives the key at execution time by name-matching the **live** physical schema, making the live table a second source of truth the compiler doesn't control. The `.cddb` is the source of truth (see [storage.md](storage.md)) — so the join key must come from it, not from whatever columns a table happens to have when the query runs.

The distinction is invisible until the physical schema drifts from the `.cddb` — a DBA adds, drops, or renames a column the catalog doesn't know about. There `NATURAL` fails the worst possible way: silently.

| Physical table drifts from `.cddb` | `… USING (k)` | `NATURAL JOIN` |
|---|---|---|
| New column whose name collides with the other operand | join key unchanged; the extra column is ignored (we never emit `SELECT *`) — **stays correct** | the column is silently folded into the join key → the join over-restricts → **silent wrong rows, no error** |
| A join column is dropped or renamed | the `USING` list names a column that's gone → the engine raises *no such column* → **loud, attributable halt** | the column drops out of the inferred key → the join weakens, perhaps to a cross-product → **silent wrong or exploded result** |
| A non-join column is added or renamed | ignored, or a loud error from an explicit projection — same either way | same |

In every drift case the two handle differently, `USING` either stays correct or fails loud, while `NATURAL` returns a silently wrong answer. There is no drift scenario where `NATURAL` does better. A miscount from a join key that quietly grew or shrank is exactly the failure class Coddl's observational equality (RM Pre 8) and *correctness over convenience* ([principles.md](principles.md)) exist to forbid — here a loud error is the *good* outcome. `USING` also keeps the golden-file tests (`tests/golden/`) honest: the meaning of the emitted SQL is fixed by its text, not by the schema it happens to execute against.

`USING` is the emission-side half of this guarantee. The complementary half — failing loud the moment a backend's live schema diverges from the `.cddb`, instead of at the first dependent query — belongs at connection time in the `Backend`/`Conn` contract (see [storage.md](storage.md)). Even without it, `USING` ensures drift surfaces as a wrong-column error rather than a wrong result.

### Computed columns: `extend` and general `replace` (the peel-chain)

`extend { c: e }` pushes its computed attribute as a `SELECT (e) AS "c", …` — the value `e` is rendered by `render_scalar` against the resolved operand's columns, and the select list is the operand's surviving columns plus the computed ones (name-sorted as always). A general-expression `replace { c: e }` reaches `coddl-sqlemit` already desugared (by the lowerer) into the chain `Rename?( Project?( Extend( core ) ) )` — extend adds `c`, the project keeps all-but the attributes `e` consumed, and the rename fires only when the new name collides with a surviving attribute (via an internal `__coddl_replace_tmp_*` temp).

`emit_select` handles this by **peeling that chain at the root** rather than threading computed state through `resolve`: it peels an optional root `Rename`, then an optional `Project`, then a (required, for any computed column) `Extend`, resolves the `core` underneath, renders the computed columns against the resolved columns, then **replays** the project's keep-filter and the rename's remap onto both the resolved columns and the computed list. `resolve`'s signature and its invariant are untouched — a *genuinely* nested `Extend` (one buried under a `Restrict`/`And`, not the root desugar chain) is not peeled, so it still reaches `resolve` and declines the push (decomposing in-process). The net SQL: a collapse `replace { line_cents: unit_cents * qty }` emits `SELECT ("unit_cents" * "qty") AS "line_cents", <survivors> …` with the consumed columns absent; an in-place `replace { qty: qty + 1 }` emits `SELECT …, ("qty" + 1) AS "qty", …`. A value that isn't SQL-renderable (e.g. a `Unary`/`Call`) declines the push and runs in-process.

## Dialect surface

Keep emitted SQL to a **portable subset** (CTEs, window functions, standard joins). Isolate dialect divergence behind backend methods — `emit_select` returns dialect-specific text, but the same RelIR plan should produce semantically equivalent results across backends.

**Set operations emit unparenthesized.** A root `Or` (surface `union`) emits `<lhs> UNION <rhs>` — a bare compound `SELECT`, *not* `(<lhs>) UNION (<rhs>)`. SQLite rejects parentheses around the operands of a compound query (`(SELECT …) UNION …` is a syntax error), whereas Postgres tolerates them; the unparenthesized form is valid in both and is the portable subset. `UNION` is associative, so a nested root chain `A union B union C` emits `… UNION … UNION …` and binds correctly. Operand `$N` placeholders (Postgres) are renumbered: the right operand starts after the left's parameter count, threaded via an `emit_select` start-offset. Bare `UNION` (set semantics, never `UNION ALL`); CORRESPONDING is satisfied for free because both operands emit canonical-sorted column lists over identical (typechecked) headings. A set operation *nested under* a relational operator (`(A union B) where p`) does not push — `resolve` errs on it, so the cut runs it in-process.

**Transitive closure emits `WITH RECURSIVE`.** A root `TClose` (surface `tclose`) over a binary same-typed relation with attributes `a` (canonical `attrs[0]`, source) and `b` (`attrs[1]`, target) emits a **two-CTE** recursive query: the operand is defined **once** as a non-recursive CTE (`coddl_tc_op`) — so its bind parameters appear once — and the recursive closure CTE (`coddl_tc`) references it for both the base and recursive members, composing on `tc.b = op.a`. `WITH RECURSIVE … UNION …` (not `UNION ALL`) converges to `⋃_{k≥1} Eᵏ`; the closure is direction-agnostic, so the result heading equals the operand heading. Both SQLite and Postgres accept it (a non-recursive CTE may sit alongside a recursive one; the recursive member references the recursive CTE exactly once). A `WITH`-prefixed query **cannot be a compound (`UNION`/`EXCEPT`) operand** — SQLite also forbids parenthesizing it — so a `TClose` reached as a set-op operand or nested under another relational op is handled like a nested set-op: `resolve` errs and the cut declines, and the expression **decomposes in-process** (each closure pushes its own `WITH RECURSIVE`, the surrounding operator runs in process). This is why the root `TClose` is emitted in the `emit_select` entry, *before* the `emit_select_offset` set-op recursion. Recursion *beyond* plain TCLOSE (labels / generalized closure) stays the cut-higher case ([relir.md](relir.md)).

Per-backend golden-file tests live in `tests/golden/`: `RelIR plan → expected SQL` per dialect. The validation matrix (see [validation.md](validation.md)) confirms that whatever the backend differences in the SQL text, the *results* match.

## Sending in-memory relations back into SQL

A relation value built or filtered in procedural code may be the input to a subsequent query. The `Conn::materialize_temp(heading, rows) -> TempRelRef` trait method (see [storage.md](storage.md)) ships an in-memory relation to a temp table the next query can reference as if it were a relvar.

- **SQLite**: temp tables / `carray`. Start with temp tables.
- **Postgres**: `UNNEST` over arrays for small relations; `COPY` into a temp table for larger ones; table-valued parameters via temp tables are the portable bet.

SQL emission can reference a `TempRelRef` the same way it references a base table — the `coddl-sqlemit` doesn't need a special path for boundary materialization; it just sees another relvar-shaped leaf.

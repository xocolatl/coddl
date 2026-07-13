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
| Never declare a column `NULL`; always `NOT NULL` — **except** an SQLite `Approximate` (`REAL`) column, which must be **nullable** because `NULL` is the physical encoding of the `Approximate` `NaN` *value* (SQLite can't store `NaN`). That nullability is a value-encoding channel, not a missing-information marker; the attribute stays total. Reject SQL DDL paths that would allow nullable columns for any other purpose. | RM Pro 4 (no nulls); NaN-encoding exception. |
| Outer joins are forbidden in lowered SQL. Coddl source has no construct that compiles to one; the type system can't express "this attribute might not have a value" as an attribute property. | RM Pro 4. |
| Joins name their key explicitly: `INNER JOIN … USING (k, …)` over the shared attributes the **typechecker** computed; disjoint headings (`times`) emit `CROSS JOIN`. Never `NATURAL JOIN`. | The `.cddb`, not the live schema, is the source of truth; see the note below. |
| Aggregates: wrap to honor identity (OO Pre 6). Emit `COALESCE(SUM(x), 0)`, `COALESCE(MAX(x), CAST(<lowest> AS T))`, etc. AVG over empty is undefined — emit a guarded expression that signals an error if the result would be queried. | OO Pre 6. |
| Relational assignment `R := expr` is the write primitive. The backend **recognizes the RHS `RelExpr` shape** and emits the surgical equivalent (`DELETE` / `INSERT` / `UPDATE` / whole-table delete), falling back to replace-all (`DELETE FROM R; INSERT INTO R (…) SELECT … FROM (…)`) for an unrecognized shape — never hydrating the relvar. Single-tuple `INSERT`/`UPDATE`/`DELETE` in source desugars to a relational-assignment expression first; the backend never sees the singular form. See [Surgical writes](#surgical-writes-assignment-rhs-recognition) below. | RM Pre 21, RM Pro 7. |
| Always emit explicit `BEGIN` / `COMMIT`. Never rely on SQL's implicit transaction start. Set constraints `IMMEDIATE` at session start; never `INITIALLY DEFERRED`. | OO Pre 4; RM Pre 23 (statement-boundary check). |
| Avoid SQL `CHARACTER` / `CHAR(n)` entirely; use `VARCHAR`/`TEXT`. SQL's `CHAR` pads with trailing blanks under equality — violates RM Pre 8. | RM Pre 8. |
| Every base table emitted from a relvar has a `PRIMARY KEY` from the relvar's declared candidate key (RM Pre 15). The candidate key with the fewest attributes wins ties; the rest become `UNIQUE`. The compiler verifies minimality before emission. | RM Pre 15. |
| A restriction value binds as a positional `?`/`$n` parameter in `WHERE`-clause order — never inlined into the SQL text. It is either a compile-time literal **or a bound local**: `let s = …; R where col = s` (with `s` an in-scope local of a pushable scalar type matching `col`) pushes as `WHERE "col" = ?` with `s`'s runtime value bound, instead of loading the whole relvar and filtering in-process. Each parameter is a `ParamSource::{Lit(Value), Param(name)}` in `SqlQuery.params`, in placeholder order; the lowerer resolves a `Param` name to the local's already-lowered value at bind time. `Rational` is literal-only (see its row — the literal path pre-serializes `n/d`, a runtime rational has no such text). A general scalar-*expression* RHS (`col = x.f`, arithmetic) still declines and runs in-process. A `where not <p>` predicate pushes when `<p>` is itself a single pushable comparison (or a bare Boolean attribute): the operator is complemented via `CmpOp::negate` (`not (col = v)` ⇒ `"col" <> ?`, `not flag` ⇒ `"flag" <> ?` bound `true`), the same complement the surgical-delete keep-filter uses. A `not` over a non-single-comparison (`not (a and b)`) declines and runs in-process, where `ScalarOp::Not` evaluates it per row. | Performance (docs/principles.md): a keyed lookup by a runtime value must not force a full-table scan. |
| `reltrue` / `relfalse` (nullary relations): emit as `(SELECT) WHERE TRUE` / `WHERE FALSE`. SQLite/Postgres tolerate this; non-conforming backends would need a synthesized dummy column. | RM Pro 5. |
| Transitive closure (`tclose`) emits a two-CTE `WITH RECURSIVE` with the operand defined once (params appear once) and `UNION` (never `UNION ALL`) so the closure is a set. It is emitted only at the statement root — a `WITH`-prefixed query can't be a compound operand or a `FROM` subquery, so a nested/operand `TClose` declines and decomposes in-process. | RM Pro 3; the one irreducible Algebra-A operator. |
| Semijoin (`matching`) / antijoin (`not matching`) emit a correlated `SELECT coddl_l.* FROM (<lhs>) AS coddl_l WHERE [NOT] EXISTS (SELECT 1 FROM (<rhs>) AS coddl_r WHERE coddl_l.k = coddl_r.k AND …)`, correlated on the shared attributes. Never a join + `SELECT DISTINCT` (or `EXCEPT`) — `EXISTS` avoids the join's row-multiplication and needs no dedup. Emitted only at the statement root (like set-ops); a nested semijoin declines and runs in-process. | RM Pro 3 (`EXISTS` yields each left row at most once); performance (the semijoin SQL a planner recognizes, not a materialized join). |
| SQLite-specific: Coddl `Boolean` lowers to SQL `INTEGER CHECK (col IN (0, 1))`. Avoid the SQLite affinity-coercion footguns by always `CAST`-ing on `INSERT`. | dialect quirk. |
| Coddl `Character` binds, stores, and reads back as its **integer Unicode codepoint** (`Value::Character` → `INTEGER`), never as a SQL `CHAR`/`CHARACTER` column. A pushed `where c = 'a'` renders `"c" = ?` bound to `97`; the result column reads back as an integer codepoint into a `Character` cell. Sidesteps the RM Pre 8 `CHAR`-padding footgun (row above) and needs no char type. | RM Pre 8; SQLite has no character type. |
| Coddl `Rational` binds, stores, and reads back as canonical **`TEXT "n/d"`** (`Value::Rational` carries the reduced `(numer, denom)` pair). Neither backend has an exact-rational type (`NUMERIC` is decimal — can't hold `1/3`), so the reduced fraction serializes to text; because it's canonical (lowest terms, `d>0`), SQL text `=` coincides with value-equality, so a pushed `where r = 3.4` renders `"r" = ?` bound to `'17/5'`. Read-back parses `"n/d"` (reducing defensively) into the 16-byte cell. Plain `NOT NULL TEXT` — no NaN-analog, so no NULL games. **Ordering** (`< <= > >=`, and `order [r]`) can't use text order — `"17/5"` sorts before `"2/1"` lexically but `17/5 > 2` — so it emits `… COLLATE coddl_rational`, a SQLite collation the runtime registers on every connection that defers to the same `coddl_rational_cmp` the in-process `<` uses. Postgres has no backend yet and no such collation, so a Rational ordering push there is a hard error (`BackendError::Other`), never a silently-wrong lexicographic sort. | RM Pre 8; no native exact-rational type. |
| Coddl `Approximate` binds, stores, and reads back as SQL **`REAL`** (`Value::Approximate` carries the canonical IEEE-754 bits). `=` is *canonicalized bit-equality* (NaN → one value, `−0.0` = `+0.0`); after canonicalization, in-process bit-equality coincides with SQL numeric `=` on finite values, so a pushed `where x = 1.5e0` (renders `"x" = ?` bound to `1.5`) agrees with the in-process path. **NaN encoding**: SQLite cannot store NaN, so it *encodes* the NaN value as SQL `NULL` — an encoding of a value, **not** a Coddl null (the relvar is total; RM Pro 4 stands). So the value↔storage **encoding** is bidirectional: store `NaN`→`NULL` (finite/±Inf→`REAL`), retrieve `NULL`→`NaN` — which requires the column be **nullable** (the NaN channel). The **comparison** layer then stays NaN-correct: Approximate equality pushdown is NULL-aware — `a = b` lowers to `(a = b OR (a IS NULL AND b IS NULL))` (SQLite `IS`), `<>` to its negation — so `NaN = NaN` stays true. Postgres stores NaN natively and already treats `NaN = NaN` as true, so it needs no translation (plain `=`, non-null column). *Implemented today:* the full encoding — store `NaN`→`NULL` (`param_to_sqlite`/`value_to_sqlite`) and retrieve `NULL`→`NaN` (`marshal_rows`). *Lands when a NaN operand can actually be pushed (attr-vs-attr comparison / Approximate arithmetic):* the NULL-aware `=`/`<>` *comparison* emission (a query concern, separate from the encoding — current `attr = finite_literal` pushdown is already correct with plain `=`). | RM Pre 8 / RM Pro 3 (reflexive equality for dedup/keys); NULL here is a value encoding, not RM Pro 4 nullability. |

### `DISTINCT` elision (key/cardinality-driven)

The **set invariant** above is non-negotiable and enforced by construction. The `DISTINCT` *keyword* is the mechanism, and it's emitted only when the result might actually contain duplicates — eliding a *provably redundant* `DISTINCT` upholds RM Pro 3, it doesn't relax it (the output is still always a set; we drop a no-op the database would otherwise pay a sort/hash for).

`coddl-relir` carries each relvar's declared candidate keys on its `RelvarRef` leaf and exposes `RelExpr::needs_distinct()`; `emit_select` drops `DISTINCT` when it returns false. A query needs no `DISTINCT` when either:

- a **candidate key survives** into the projected heading (`surviving_keys()` non-empty) — no two distinct rows can collide on the kept columns; or
- a restriction **bounds cardinality to ≤ 1** (`card_le_one()`) — an equality on a full candidate key (v1: a single-attribute key pinned by `attr = literal`), so any projection is trivially duplicate-free. Only `=` *pins* a value: a `<>`/`<`/`<=`/`>`/`>=` restriction pushes and renders its operator in the `WHERE`, but it bounds nothing, so a key-dropping projection over it keeps `DISTINCT`.

Otherwise (a projection that drops below every candidate key with unbounded cardinality) `DISTINCT` stays. A keyless or not-yet-analyzable leaf is conservative — it keeps `DISTINCT`. This is a compile-time down payment on candidate-key inference (VSS 3): today only declared keys propagate; inferred FDs extend `surviving_keys()`/`card_le_one()` later without touching `emit_select`.

**We elide every provable `DISTINCT` ourselves — we never leave it for the planner** (per [principles.md](principles.md) §1, "our optimizer does all the work"; SQLite won't collapse a redundant `DISTINCT`, and we don't assume any backend will). So each provable-set case is ours to catch, tracked here:

- **Handled today:** a surviving *declared* candidate key, threaded through `Restrict`/`Project`/`Rename`/`Extend` (`surviving_keys()`); a cardinality-≤-1 restriction (`card_le_one()`); and `Semijoin` (surface `matching`/`not matching`), which inherits `lhs`'s keys — a semijoin filters `lhs`, it cannot duplicate a row.
- **Owed (tracked):** **key/FD inference through a join.** `RelExpr::And`/`Or`/`Minus` report no surviving keys today, so `join`/`compose`/`intersect`/`union` always emit `DISTINCT` even when the result is provably unique. The soundness rule for `And` (natural join on the shared attributes `J`): every candidate key of `R` survives into `R ⋈ S` when `J` contains a candidate key of `S` (each `R`-row then matches ≤ 1 `S`-row), and symmetrically for `S`'s keys; the composite `keyR ∪ keyS` always survives. `Project` already narrows surviving keys to the kept attributes, so once `And` propagates keys, `compose` (= `Project(And)`) drops its `DISTINCT` for free whenever the surviving side carries a key and the removed join key determines the other side. `intersect`/`minus` return a subset of an operand, so that operand's keys survive; `union`'s result is keyless in general (two disjoint key spaces can collide) unless a disjointness proof lands. Close these in `surviving_keys()`; `emit_select` needs no change.

The same discipline governs the in-process engine: the runtime seal is the ProcIR analogue of `DISTINCT`, and a proven-set result must skip it too (there is no planner behind ProcIR at all).

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

### `wrap` / `unwrap`: flat leaf columns, nesting in the descriptor

`wrap`/`unwrap` are pure **heading** restructures — they group attributes into a tuple-valued attribute (an inline nested cell — see [runtime.md](runtime.md)) or expand one — but the underlying SQL columns are always the flat **leaf** columns. SQLite has no composite column, so a pushed wrap emits no `$`-mangled aliases and no `JSON`: it selects the plain leaf columns, and the nesting lives entirely in the **result descriptor** (which the runtime reconstructs at materialization). `resolve` passes a `Wrap`/`Unwrap` straight through to its input (the `attr → column` map is keyed by the stable leaf names; the restructure changes only `expr.heading()`). `emit_select`'s column-list builder then **flattens** each `Tuple` attribute of the result heading to its component leaf columns, recursing depth-first in the sub-heading's canonical order — which is exactly `record_layout`'s leaf order, so the runtime's positional column→cell mapping reconstructs the inline nested cell. Net SQL for `Greetings wrap { t: {id, message} }`: `SELECT DISTINCT "id", "message" FROM "greetings"` — the wrap is invisible in the SQL; the result heading `{t: Tuple{id, message}}` and its (nested) descriptor carry it. A `wrap … unwrap` round-trip pushes as the plain flat select of the surviving columns.

### Ordered `load` pushdown: trailing `ORDER BY`

`load … from <src> order [ … ]` is the one place ordering enters SQL. Relations
are unordered (RM Pro 1), so RelIR carries **no** sort node; the order rides a
**parameter**, not a `RelExpr` node. `emit_select_ordered(expr, dialect, order)`
takes the sort keys as `(attribute-name, is_descending)` pairs (precedence order,
most-significant first) and appends `ORDER BY "attr"[ DESC], …` after the `WHERE`
and **before** the plan-id hash — so an ordered query caches distinctly from its
unordered twin. `ASC` is SQL's default, so only `DESC` is emitted.

The terms name the **output columns** — always the Coddl attribute (physical
columns get `AS "attr"` on mismatch, computed `extend` columns get `AS "attr"`),
so a single rule orders physical, renamed, and computed keys uniformly, and every
key is in the select list (satisfying Postgres's `SELECT DISTINCT … ORDER BY`).
Order keys are always scalar (the typechecker rejects relation-/tuple-valued keys),
so each is exactly one output column.

A trailing `ORDER BY` attaches only to a single standard `SELECT`. A **root**
set-op (`union`/`minus` → `UNION`/`EXCEPT`) or `tclose` (`WITH RECURSIVE`) can't
carry one in v1, so `emit_select_ordered` **declines** (returns `Err`) for those
roots when the order is non-empty; the cut turns that into a `None` and the load
sorts in-process (`coddl_load_ordered`) instead. A set-op *under* a relational op
already declined (`resolve` errs on `Or`/`Minus`). On the pushed path the rows
arrive already ordered and the SQL path never re-seals, so the lowerer emits
`coddl_load_ordered` with an **empty** key array — see [runtime.md](runtime.md)
"Iteration: the `load` primitive."

### Surgical writes: assignment-RHS recognition

`emit_assignment(target, rhs, dialect)` turns a relational assignment to a
public **base** relvar into surgical DML by recognizing the RHS `RelExpr`
shape — the relvar is never read into the process and written back. `target` is
the assignment LHS lowered to its `RelvarRef`. The recognized shapes and their
emitted SQL:

| RHS `RelExpr` shape | Emitted SQL |
|---|---|
| `Minus{ RelvarRef(t), Restrict*(RelvarRef(t), preds) }` | `DELETE FROM t WHERE <preds>` |
| `Restrict{ RelvarRef(t), p }` (keep-filter — a single restrict over the bare target) | `DELETE FROM t WHERE <¬p>` (the negated predicate) |
| `Minus{ RelvarRef(t), RelvarRef(t) }` (self-subtraction) | `DELETE FROM t` (whole-table delete) |
| `Minus{ RelvarRef(t), X }` (X same-heading, pushable, not rooted in `t`) | `DELETE FROM t WHERE EXISTS (SELECT 1 FROM (<X>) AS a WHERE t.col = a.attr AND …)` |
| `And{ RelvarRef(t), X }` (`intersect`; X same-heading, pushable) | `DELETE FROM t WHERE NOT EXISTS (SELECT 1 FROM (<X>) AS a WHERE t.col = a.attr AND …)` |
| `Or{ RelvarRef(t), e }` (e same-heading, pushable; union is commutative) | `INSERT INTO t (…) SELECT … FROM (<e>) AS a WHERE NOT EXISTS (SELECT 1 FROM t WHERE t.col = a.attr AND …)` |
| `Or{ Restrict(t, ¬p), «substitute»(Restrict(t, p)) }`, or a bare «substitute» over `t` | `UPDATE t SET c = e, … WHERE <p>` (no `WHERE` for the bare update-all form) |
| anything with `t` **absent** from the RHS (an independent value) | replace-all: `DELETE FROM t` then `INSERT INTO t (…) SELECT … FROM (<X>) AS a` (pushable `X`) or a row-shipping insert (in-memory `X`) |

The `minus`-restrict and whole-table delete rows delegate to a shared
`emit_delete` on the `minus` subtrahend, which bottoms out in the target base
relvar; the `WHERE` reuses the same `(column, RestrictValue)` collection a
`SELECT … WHERE` builds for the equivalent restriction, so a delete predicate is
byte-identical to the matching read predicate — and a bound-local value
(`delete R where col = s`) binds the same way it does in a read. The **keep-filter** row
(`t := t where p` — keep the matching rows) is the same machinery with the
predicate flipped: `emit_delete` runs over `Restrict(t, ¬p)`, the negated
restriction (`CmpOp::negate` complements the comparison — `=`↔`<>`, `<`↔`>=`,
…), so keeping `p` deletes its complement. Only a *single* restrict over the
bare target matches; a deeper keep-filter chain's negation is a disjunction the
single-predicate model can't push, so it declines (below). The anti-join delete
(`emit_anti_join_delete`, e.g. `t := t minus other_relvar`) renders `X` via
`emit_select` as a derived table whose columns are the Coddl attribute names,
then correlates every attribute (`t`'s physical column against the derived
table's attribute column) — full tuple equality (RM Pre 8) inside an `EXISTS`,
never an outer join (RM Pro 4). The **intersect** delete (`t := t intersect X` —
keep the rows present in both) is the same helper with a `negated` flag: `NOT
EXISTS` instead of `EXISTS`, deleting the `t`-rows with no match in `X`. The
union insert
(`emit_idempotent_insert`, e.g. `t := t union other_relvar`) is the mirror image:
the same derived table + all-attribute correlation, but `INSERT … SELECT …
WHERE NOT EXISTS`. The `NOT EXISTS` makes re-inserting an identical tuple a no-op
while a tuple sharing a key but differing elsewhere is *not* skipped, so `t`'s
`PRIMARY KEY` rejects it — the Golden Rule (RM Pre 23): a key-violating update
fails rather than silently dropping the tuple (so never `INSERT OR IGNORE`).

When the `union` operand is **not** pushable (an in-memory `MaterializedRelvar`,
or a relation literal — its rows live in the process, not SQL), the assignment
still inserts, but the rows are shipped at runtime rather than via a sub-SELECT.
`emit_insert_template` bakes a fixed merge `INSERT INTO t (…) SELECT v.columnN…
FROM (VALUES <marker>) AS v WHERE NOT EXISTS (…)`, where `<marker>`
([`INSERT_ROWS_MARKER`]) is a placeholder the runtime expands to one `(?,…)`
group per source row (in batches, sized under the bind-variable limit). Same
set / Golden-Rule semantics as the pushable insert — only the row source differs
(a bound `VALUES` list vs. a pushed sub-SELECT), and it uses **no temp table**
(so no catalog churn).

**The self-reference principle.** Whether the target `t` appears on the RHS
decides the kind of write. If `t` *is* on the RHS, the assignment is an
incremental transform of `t` and must be surgical — one of the `DELETE` / `INSERT`
/ `UPDATE` shapes above. A self-referential shape the single-predicate model can't
yet express surgically (e.g. a compound keep-filter `t where p1 where p2`, whose
negation is a disjunction) **declines** with the "not a recognized surgical shape"
diagnostic (T0049) rather than falling through to a hydrating replace-all — it
*should* be surgery, just not one v1 can push. If `t` is **absent** from the RHS,
the assignment sets `t` to an independent value: a **replace-all**. The lowerer
(not `emit_assignment`) drives it — `emit_truncate(t)` (`DELETE FROM t`) followed
by `emit_replace_insert(t, X)` (`INSERT INTO t (…) SELECT … FROM (<X>) AS a`, no
`NOT EXISTS` — `t` is empty post-truncate) for a pushable `X`, or the same
row-shipping insert as the in-memory `union` for a literal / `MaterializedRelvar`.
The two statements run in the enclosing transaction (atomic — a failed refill
rolls the truncate back). `X` can't read `t` (that's what "absent" means), so
truncate-then-refill needs no snapshot. The `RelExpr::references_table` walk makes
the surgical-vs-replace-all split; replace-all is taken only when it returns
`false`. The identity `t := t` is dead code: the typechecker warns (T0051) and the
lowerer elides it entirely (no instruction, public or private).

The UPDATE shape (`emit_update`) recognizes TTM's update expansion — keep the
non-matching rows, substitute the matching ones. The "changed rows" operand is a
**heading-preserving substitute chain** `Rename(Project(Extend(Restrict(t, p))))`
(what `R replace { c: e }` desugars to when the value reads the attribute it
sets); `peel_substitute` recovers the `(target ← value)` pairs (pairing each
`Extend` value with its `Rename` target). The "unchanged rows" operand must be
the exact complement `Restrict(t, ¬p)` — same attribute and value, the negated
operator (`CmpOp::negate`) — over the same `t`; otherwise it isn't an update and
declines. It emits `UPDATE t SET c = render_scalar(e), … WHERE <p>` (SET values
inline like `extend`; the `WHERE` literal is the one bound param). A bare
substitute over `t` (no complement/union) is the update-all form (no `WHERE`).

A recognized pushable assignment is registered as a DML plan and fired by the
runtime's `coddl_exec` (the write sibling of `coddl_query`); an in-memory `union`
fires `coddl_exec_insert` (which iterates the relation and runs the batched
template). Both run inside the enclosing transaction's `BEGIN`/`COMMIT` (see
[storage.md](storage.md)).

### Statement-verb sugar

Relational assignment is the write primitive; the ergonomic statement verbs are
thin sugar that **desugar in the lowerer to a recognized RHS shape**, so they add
no SQL-emission code — the desugared value flows through `emit_assignment` (public
relvars) or the in-memory slot store (private relvars) unchanged.

| Statement | Desugars to `:=` | Recognized arm |
|---|---|---|
| `truncate R` | `R := R minus R` | self-subtraction → whole-table `DELETE FROM t` |
| `delete R where p` | `R := R minus (R where p)` | `Minus{ t, Restrict(t, p) }` → `DELETE FROM t WHERE p` |
| `insert R { … }` / `insert R S` | `R := R union <source>` | `Or{ t, source }` → idempotent INSERT (pushed for a SQL-backed source, row-shipped for a literal / private source) |
| `update R where p { c: e }` | `R := (R where ¬p) union ((R where p) «sub»)` | `Or{ Restrict(t,¬p), «sub»(Restrict(t,p)) }` → `UPDATE t SET c=e WHERE p` |
| `update R { c: e }` | `R := R «sub»` | bare `«sub»(t)` → `UPDATE t SET c=e` (no `WHERE`) |

`truncate R` clears every tuple. Its operand must be a bare assignable relvar
(the typechecker rejects a restricted or compound operand, T0033, and requires a
transaction for a public relvar, T0025); the lowerer builds
`Minus{ RelvarRef(t), RelvarRef(t) }` and routes it through the same
`emit_assignment` self-subtraction arm a literal `R := R minus R` would hit (a
private relvar instead stores the empty `R minus R` value back into its slot).

`delete R where p` removes the matching tuples. Its operand is a `where`
-restriction over a bare relvar (the predicate is mandatory — a bare `delete R;`
is T0052, "use `truncate`"); the lowerer builds `Minus{ RelvarRef(t),
Restrict(RelvarRef(t), p) }` and routes it through the `emit_assignment` DELETE
arm a literal `R := R minus (R where p)` would hit (a private relvar stores the
kept rows `R minus (R where p)` into its slot). A predicate the single-predicate
model can't push declines with **T0049** rather than a hydrating partial delete —
never a silent wipe.

`insert R <source>` adds tuples. Both surface forms — the brace tuple-set
`insert R { {…}, {…} }` (a keyword-less relation literal) and the relation-expr
`insert R S` — are a single relation `source`, so the lowerer builds
`Or{ RelvarRef(t), source }` and routes it through the same `emit_assignment`
idempotent-INSERT arm a literal `R := R union <source>` would hit: a SQL-backed
source pushes (`INSERT … SELECT … WHERE NOT EXISTS`), a literal / private source
ships its rows via the shared `ship_union_insert` (the batched-`VALUES`
`Inst::InsertFrom`). A private relvar stores the in-process union into its slot.

`update R where p { c: e }` overwrites named attributes of the matching tuples.
The `{ c: e }` clause builds a **substitute chain** `Rename(Project(Extend(input,
[__tmp := e]), keep), [__tmp → c])` — the same construction `replace` uses, but
`update`'s `Project` drops the **target** attribute `c` (replace drops the attrs
the value reads), and constant / bare-reference values are allowed. The lowerer
wraps it as `Or{ Restrict(t, ¬p), «sub»(Restrict(t, p)) }` (update-with-`where`)
or a bare `«sub»(RelvarRef(t))` (update-all), which `emit_assignment` routes to
`emit_update` — Form A (`UPDATE … WHERE p`) or Form B (no `WHERE`). `peel_substitute`
recovers the SET pairs regardless of what `Project` drops, pairing each `Extend`
value with its `Rename` target. A private relvar instead computes `(R minus
(R where p)) union ((R where p) «sub»)` (or a bare substitute) in process and
stores it. A predicate that isn't a single pushable comparison, or a value the
SQL renderer can't express, declines with **T0049** rather than a hydrating
rewrite — never a silent wipe.

## Dialect surface

Keep emitted SQL to a **portable subset** (CTEs, window functions, standard joins). Isolate dialect divergence behind backend methods — `emit_select` returns dialect-specific text, but the same RelIR plan should produce semantically equivalent results across backends.

**Set operations emit unparenthesized.** A root `Or` (surface `union`) emits `<lhs> UNION <rhs>` — a bare compound `SELECT`, *not* `(<lhs>) UNION (<rhs>)`. SQLite rejects parentheses around the operands of a compound query (`(SELECT …) UNION …` is a syntax error), whereas Postgres tolerates them; the unparenthesized form is valid in both and is the portable subset. `UNION` is associative, so a nested root chain `A union B union C` emits `… UNION … UNION …` and binds correctly. Operand `$N` placeholders (Postgres) are renumbered: the right operand starts after the left's parameter count, threaded via an `emit_select` start-offset. Bare `UNION` (set semantics, never `UNION ALL`); CORRESPONDING is satisfied for free because both operands emit canonical-sorted column lists over identical (typechecked) headings. A set operation *nested under* a relational operator (`(A union B) where p`) does not push — `resolve` errs on it, so the cut runs it in-process.

**Semijoin / antijoin emit a correlated `WHERE [NOT] EXISTS`.** A root `Semijoin` (surface `matching`, `negated: false`) / antijoin (`not matching`, `negated: true`) emits `SELECT coddl_l.* FROM (<lhs>) AS coddl_l WHERE EXISTS (SELECT 1 FROM (<rhs>) AS coddl_r WHERE coddl_l."k" = coddl_r."k" AND …)`, correlated on the shared attributes (the antijoin uses `NOT EXISTS`). This is the idiomatic semijoin SQL — no `INNER JOIN` row-multiplication and no `DISTINCT`/`EXCEPT` dedup: `coddl_l.*` passes the left subquery's already-set columns through (in canonical-heading order, so it marshals against `lhs`'s heading, including any flattened Tuple leaf columns), and `EXISTS` selects a subset, so no outer `DISTINCT` is needed. The correlation reuses the same all-`=` tuple-comparison pattern the surgical anti-join delete uses (`emit_anti_join_delete`), but over the **shared** attributes (the typechecked join key, always ≥1) rather than every attribute, and in SELECT rather than DELETE position. Operand `$N` placeholders are renumbered (rhs starts after lhs). Handled at the statement root like set-ops; a nested semijoin (`(A matching B) where p`) and a Tuple-valued shared key both decline through `resolve` and run in-process, and a trailing `ORDER BY` over a semijoin root is deferred (declines to an in-process sort, like a set-op root).

**Transitive closure emits `WITH RECURSIVE`.** A root `TClose` (surface `tclose`) over a binary same-typed relation with attributes `a` (canonical `attrs[0]`, source) and `b` (`attrs[1]`, target) emits a **two-CTE** recursive query: the operand is defined **once** as a non-recursive CTE (`coddl_tc_op`) — so its bind parameters appear once — and the recursive closure CTE (`coddl_tc`) references it for both the base and recursive members, composing on `tc.b = op.a`. `WITH RECURSIVE … UNION …` (not `UNION ALL`) converges to `⋃_{k≥1} Eᵏ`; the closure is direction-agnostic, so the result heading equals the operand heading. Both SQLite and Postgres accept it (a non-recursive CTE may sit alongside a recursive one; the recursive member references the recursive CTE exactly once). A `WITH`-prefixed query **cannot be a compound (`UNION`/`EXCEPT`) operand** — SQLite also forbids parenthesizing it — so a `TClose` reached as a set-op operand or nested under another relational op is handled like a nested set-op: `resolve` errs and the cut declines, and the expression **decomposes in-process** (each closure pushes its own `WITH RECURSIVE`, the surrounding operator runs in process). This is why the root `TClose` is emitted in the `emit_select` entry, *before* the `emit_select_offset` set-op recursion. Recursion *beyond* plain TCLOSE (labels / generalized closure) stays the cut-higher case ([relir.md](relir.md)).

Per-backend golden-file tests live in `tests/golden/`: `RelIR plan → expected SQL` per dialect. The validation matrix (see [validation.md](validation.md)) confirms that whatever the backend differences in the SQL text, the *results* match.

## Sending in-memory relations back into SQL

A relation value built or filtered in procedural code may be the input to a subsequent query. The `Conn::materialize_temp(heading, rows) -> TempRelRef` trait method (see [storage.md](storage.md)) ships an in-memory relation to a temp table the next query can reference as if it were a relvar.

- **SQLite**: temp tables / `carray`. Start with temp tables.
- **Postgres**: `UNNEST` over arrays for small relations; `COPY` into a temp table for larger ones; table-valued parameters via temp tables are the portable bet.

SQL emission can reference a `TempRelRef` the same way it references a base table — the `coddl-sqlemit` doesn't need a special path for boundary materialization; it just sees another relvar-shaped leaf.

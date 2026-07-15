# RelIR — relational IR (Algebra A core + sugar)

RelIR is Coddl's relational IR. It's where the relational *algebra* lives — the operators that consume relations and produce relations, with their headings, FD sets, constraints, and the storage-origin flags that drive the SQL-vs-in-process cut.

RelIR sits **above** ProcIR in the pipeline (see [procir.md](procir.md)). It's not a peer. Each RelIR subtree is consumed by exactly one of two crates — `coddl-sqlemit` for SQL-rooted subtrees, `coddl-execlocal` for in-process subtrees — both of which emit into ProcIR. (That two-crate split is the target; today only `coddl-sqlemit` is wired — see **Implementation status** below.) ProcIR knows nothing about algebra; RelIR is the one place algebra exists.

This split is what lets the same algebra fragment run as SQL or as compiled native code without two parallel representations to keep in lockstep.

## Implementation status

This doc describes the **target** RelIR. The code implements a thin slice of it; the rest is designed-not-built. The split is called out here so the rest of the doc can describe the design without reading as a description of the current tree. (Per-operator status is tracked separately; this section stays coarse so it doesn't drift per-commit.)

**Built today**

- `RelExpr` (`coddl-relir/src/expr.rs`) — four nodes: the `RelvarRef` leaf, `Restrict` (surface `where`), `Project` (project-over *and* project-away), and `Rename`.
- Per-node **heading** inference and the **storage-origin** flag; declared candidate **keys** on the leaf, used for key-based `DISTINCT`-elision (`needs_distinct`).
- **The cut** as an origin gate — *not* a cost model: push a subtree when it touches a relvar (`RelvarRooted` **or** `Mixed`) and SQL emission succeeds (`coddl-procir/src/cut.rs`); a fully `Materialized` tree stays in-process.
- **Mixed-origin pushdown** via the **`RelParam` leaf** — a relation-valued bind parameter, the relation analogue of `RestrictValue::Param`. A relation-typed local (a bound relation value, a relation-valued tuple field like `req.path`) builds as `RelParam { slot, heading }`; a *maximal materialized subtree* under a binary operator collapses to one slot (its structural build is discarded, its whole AST recorded — the in-process machinery computes it and only its result rows ship). A semijoin rhs is **narrowed to the shared attributes** at build time (`L matching R ≡ L matching (R project {shared})` — a `RelParam` shrinks its heading, a structural rhs wraps in `Project`); the ship-time project seals, so fewer cells *and* fewer rows cross. Emission renders each slot as a `VALUES`-backed derived table (`coddl-sqlemit`, "Relation-valued parameters"); the runtime binds the rows at the force point and **dispatches on the cardinality it is already holding** — 0 at an absorbing slot returns empty without a statement, 1 at a root-`matching` rhs fires the baked cardinality-1 sibling plan (built by `RelExpr::card1_semijoin_specialization`, the first RelIR rewrite — its equality values are `RestrictValue::SlotCell` cells of the shipped row), n runs the general form. This implements the settled rule: **ship the local relation up, never pull the relvar down** (see "The cut" below).
- **SQL pushdown** of relvar-rooted `RelvarRef` / `Restrict` / `Project` / `Rename` via `coddl-sqlemit`.
- An **in-process path** for non-pushable subtrees — currently lowered to ProcIR within `coddl-procir` itself (not yet through `coddl-execlocal`).
- Restriction predicates are an `attr <cmp> value` comparison
  (`Predicate::AttrCmp { attr, op: CmpOp, value: RestrictValue }`), where `<cmp>`
  is `=`/`<>` (Integer/Text/Boolean) or `<`/`<=`/`>`/`>=` (Integer), **or** the
  tuple-independent gate `Predicate::Gate(RestrictValue)` (surface `when` —
  `R times ⟨c⟩` in restrict clothing). Only `=` pins a value, so only it bounds
  cardinality for `DISTINCT`-elision (a gate pins nothing — `card_le_one`
  contributes `false`). A **bare Boolean attribute** predicate `R where flag`
  is the equality `flag = true` (a Boolean-valued attribute is itself a
  proposition) and pushes as
  `Predicate::AttrCmp { attr, Eq, Lit(Boolean(true)) }` → `WHERE "flag" = ?` —
  the formatter canonicalizes `flag = true` to the bare form, so both surface
  spellings must push identically.
- **The gate conjunct (`when`).** `R when c` builds as `Restrict { pred:
  Gate(value) }` and inherits the restrict machinery wholesale — keys pass
  through `surviving_keys` untouched, absorption holds (`annotate_absorbs`
  walks through `Restrict`; gate-of-empty is empty). The value is a
  `RestrictValue::Param`: a **bare in-scope Boolean local** binds under its own
  name (the same discipline as a comparison's `Param`); any other condition
  expression rides the build's `conds` side table (`BuildTables` — truncated in
  lockstep with `slots` at every collapse point) and binds as the
  compiler-internal `__when_<k>` once the push commits, lowered exactly once in
  `try_lower_pushed_ordered`. SQL renders it `?N = 1` (see
  [sqlemit.md](sqlemit.md)). A gate over a **materialized** input never reaches
  SQL: the operand-collapse rule swallows the whole `when` into its `RelParam`
  slot and the slot's AST re-lowers in-process through `Inst::Gate`
  (`coddl_relation_when` — retain-or-fresh-empty, O(1); a necessity, not an
  optimization: the in-process RelExpr consumer has no `Restrict` arm, and the
  per-tuple `where`-helper path can't capture an enclosing condition, T0022).
  Note `contains_restrict` counts a gate as "filtered", muting the S1
  full-pull guard for gated-relvar operand shapes — sound, because those
  shapes push. Deferred-alias hygiene: a `let`-alias declines an RHS whose
  build produced `conds` entries (a `__when_<k>` name must not cross builds);
  a bare-local gate aliases fine. The DML recognizers likewise decline a
  gate-bearing value tree (no cond-lowering step on the write path) — it
  routes to the row-shipping paths, whose reads bind the gate properly.
- **`otherwise` (relational COALESCE) has no RelIR node in v1.** `R otherwise
  D` ≡ `R union (D times (reltrue minus (R project {})))`, but nothing ever
  builds that tree: the arms are exclusive by construction, so the lowerer
  emits one `Inst::Otherwise` (`coddl_relation_otherwise` — a header length
  check plus a retain of the winner, O(1); the result is already sealed
  because it *is* one of the operands). Operands lower to relation values
  first, so an embedded pushed plan fires at the `otherwise` site — post-fire
  empty-result substitution. Inside a larger relational op, the whole
  `otherwise` **self-collapses to one `RelParam` slot** (the same precedent as
  a non-pushable `where` over a materialized input), so `(A otherwise B) join
  Relvar` ships the coalesced rows and never kills the enclosing build.
  Tracked residue (`.local/tracking/optimizations.md`): a proper
  `RelExpr::Otherwise` node would buy relvar-rooted pushdown (the
  `UNION`/`NOT EXISTS` form) and exclusive-arms cardinality inference; today a
  relvar-rooted *fallback*'s plan fires even when the primary is nonempty.
- The value is a `RestrictValue`: a compile-time `Lit(Literal)`, a
  **bound parameter** `Param(name)` — the surface name of an in-scope
  local/parameter whose runtime value binds at query time — or a
  **slot cell** `SlotCell { slot, cell }`, a cell of a relation-valued
  parameter's single shipped row (appears only in the cardinality-1 semijoin
  sibling; the runtime fills it at dispatch time — never a lowerer bind
  argument). `Param` keeps the IR
  backend- and lowerer-agnostic (a free-variable *name*, never a ProcIR value id
  or an AST node); the lowerer resolves the name to that local's already-lowered
  value when it emits the query's bind arguments. So `let s = …; R where col = s`
  pushes as `WHERE "col" = ?` bound to `s` instead of loading `R` and filtering
  in-process. Cut-1 accepts a bare-local RHS of a pushable scalar
  (`{Integer, Text, Character, Approximate, Boolean}`; `Rational` stays
  literal-only, since the literal path pre-serializes `n/d` to text at compile
  time and a runtime rational has no such text). A general scalar-*expression*
  RHS (`col = x.f`, arithmetic) still declines — name the value first. Both
  forms render to a `?`/`$n` placeholder; see `coddl-sqlemit`'s `ParamSource`.
- A **conjunctive `where`** (`R where p and q and …`) of pushable comparisons
  pushes: the lowerer (`collect_conjuncts`) splits it into one `Restrict` per
  conjunct — the identical tree the stacked spelling `R where p where q` builds —
  and SQL emission's `resolve` coalesces stacked `Restrict`s into a single
  `WHERE p AND q …`. So the two spellings emit one identical `SELECT`; if *any*
  conjunct isn't pushable the whole restriction declines and runs in-process.
  (Disjunction is not yet a predicate; an `or` still declines the push.)

**Designed, not yet built**

- The remaining **A-core nodes** (`AND`, `OR`, `NOT`, `TCLOSE`) and the **sugar → A-core desugaring**. The four nodes above are consumed as-is; nothing is rewritten into A-core form yet. (`REMOVE` and `RENAME` already exist, as `Project` and `Rename`.)
- The rest of the **sugar layer**: `Summarize`. (`Join`/`Union`/`Minus`/`Intersect`/`Compose`/`Extend`/`Rename`/`Wrap`/`Unwrap`/`Semijoin`/`Group`/`Ungroup` are built. `Wrap`/`Unwrap` lower to `Inst::Restructure` → `coddl_relation_restructure`, in-process, SQL push landed for the leaf-column form. `Group`/`Ungroup` (TTM GROUP/UNGROUP, the relation-valued-attribute pair) lower to `Inst::Group`/`Inst::Ungroup` → `coddl_relation_group`/`_ungroup` and **never push** — a relation-valued cell has no flat-column SQL form, so sqlemit's `resolve` declines and the operand fetch pushes at its own root. `Group`'s `surviving_keys` is the survivor set (one tuple per distinct survivor combination — a genuine candidate key, the empty key when every attribute is consumed) plus any survivor-contained input key; `Ungroup`'s is the DBC7 superkey shape (`k ∪ lifted` for each survivor-contained input key `k`), else keyless. `Semijoin { lhs, rhs, negated }` is the one node covering both surface `matching` (semijoin) and `not matching` (antijoin) — see the Sugar layer section below.)
- The **optimizer** and **cost model**. Mixed-origin handling is built in its default form (`RelParam` shipping, above); a cost model would refine *which* side crosses the boundary for large local relations (temp tables — see [sqlemit.md](sqlemit.md)), not reopen the ship-up default.
- The per-node **FD set** and **constraint set** (only heading, origin, and leaf keys exist today). **Key inference through the binary nodes** now covers `And` (`join`/`times`/`intersect`/`compose`), `Minus`, and `Semijoin` — `surviving_keys()` propagates keys via the cover + composite rules so those stop emitting a redundant `DISTINCT`, per the "our optimizer does all the work" rule ([principles.md](principles.md) §1). Soundness rules and the running catalog live in [sqlemit.md](sqlemit.md) ("`DISTINCT` elision"). Still open: `Or` (`union`) stays keyless without a disjointness proof (keyless by nature, not a gap), and the in-process **seal** — the ProcIR analogue of `DISTINCT` — is not yet driven by `surviving_keys()`.
- `coddl-execlocal` (an empty stub) as the RelIR→ProcIR consumer, and the runtime RelIR interpreter (the dynamic path).
- Pushdown / predicate surface beyond `attr <cmp> literal` comparisons and their conjunctions — disjunction, attribute-vs-attribute, arithmetic in predicates, subset/superset. (Scalar comparisons `=`/`<>`/`<`/`<=`/`>`/`>=` and `and`-chains of them already push.)

## Why Algebra A

The Manifesto's authors argue (Appendix A) that any industrial-strength D should be *mappable to* Algebra A — a foundational set of primitives in the spirit of predicate logic — even if surface syntax uses higher-level operators. Coddl takes that seriously: **RelIR's core is Algebra A**, and surface operators are sugar that desugars during the lowering pass.

This is the "few primitives, layered sugar" principle from [principles.md](principles.md). Every optimization, every rewrite, every backend can work against the tiny A-core surface; the surface operator zoo is sugar, not parallel implementation.

## A core

The practical A-core primitives:

- `AND` (natural join — generalizes TIMES and INTERSECT)
- `OR` (heading-agnostic union)
- `NOT` (relational complement)
- `REMOVE` (project-away one attribute — existential elimination)
- `RENAME`
- `TCLOSE`

Minimally these reduce further to `REMOVE` + `NOR` (or `NAND`) + `TCLOSE`, but the six above are the practical primitives the optimizer pattern-matches against.

## The core is a vocabulary, not an execution plan

The A core is the **optimizer's** vocabulary — the small, closed set of operators that rewrites and the cut reason over. It is deliberately *not* the set of operations a backend executes. `coddl-sqlemit` and `coddl-execlocal` own the **physical** vocabulary and *re-expand* the core into it: a core pattern maps to the best idiomatic SQL or in-process operator, never to a literal transliteration of a primitive's set-theoretic definition.

Two consequences, both load-bearing:

- **◄NOT► and ◄OR► are never materialized.** A relational complement is unbounded (no universe relation, no nulls — RM Pro 4); a heading-agnostic ◄OR► pads with every possible value. Only the *safe patterns* reach a backend: ◄AND NOT► (set difference / anti-join — surface `minus`, identical headings) and same-heading ◄OR► (surface `union`). SQL emission pattern-matches `R AND (NOT S)` to `EXCEPT` / `NOT EXISTS`; the in-process engine runs it as an anti-join. Bare ◄NOT► / ◄OR► never escape the algebra. The surface constraints that enforce this (identical-heading `union` / `minus`, no standalone complement) are exactly the "safety mechanisms" Appendix A defers to.
- **Reduce to the *practical* core, not the minimal one.** The `REMOVE` + `NOR` / `NAND` + `TCLOSE` basis is for completeness proofs; reducing real queries to `NOR` destroys the structure codegen must pattern-match. Keeping `AND` / `OR` / `NOT` / `REMOVE` / `RENAME` distinct is what lets emission recover idiomatic operators.

Over-reducing for SQL specifically is a pessimization: SQL is itself a high-level algebra with its own planner. RelIR's job is to normalize for the cut and push restrictions toward the leaves, then hand the backend the highest-level *faithful* shape — not a minimized one — and let its optimizer choose the physical plan.

## Sugar layer

Desugars to A core during the same lowering pass that builds RelIR — sugar does not survive into the optimizer:

`Project`, `Restrict` (surface `where`), `Join`, `Union`, `Minus`, `Intersect`, `Compose`, `Semijoin`, `Extend`, `Summarize`, `Group`, `Ungroup`, `Wrap`, `Unwrap`.

**`Semijoin { lhs, rhs, negated }`** covers surface `matching` (semijoin, `negated: false`) and `not matching` (antijoin, `negated: true`). Algebraically it is `(lhs AND rhs)` projected back onto `lhs`'s heading (semijoin), or `lhs` minus that (antijoin) — but it is kept as an explicit sugar node rather than pre-desugared to `Project(And)`/`Minus` so the SQL emitter can push it as the idiomatic correlated `WHERE [NOT] EXISTS` (no join row-multiplication, no `DISTINCT`/`EXCEPT` dedup — the semijoin SQL a planner recognizes). The result heading is `lhs`'s; the typechecker requires the operands to partially overlap (the same legal domain as `join`/`compose`), so the `EXISTS` correlation on the shared attributes is never empty. In-process it expands to join+project(+minus).

`Compose` lowers to `AND` followed by `REMOVE` of the attributes common to both operands (Manifesto appendix A); it is *not* an A-core primitive.

PascalCase as Rust enum-variant names; the corresponding surface keywords are lowercase (`join`, `union`, `extend`, …) — see [grammar.md](grammar.md).

## Operators as relations

Crucially, **operators are themselves relations** in the A formulation: a scalar function `f(X, Y) -> Z` is an (n+1)-ary relcon `F{X, Y, Z}`. So surface

```
extend r add { c: x + y }
```

desugars to the A-level

```
r join (plus replace { x: a, y: b, z: c })
```

(where `plus` is the `Integer × Integer → Integer` operator viewed as a 3-ary relation). Surface `where`-clauses similarly desugar to joins against constant relations. This collapses much of the operator zoo into pure JOIN-and-REMOVE — which is what the optimizer actually wants. It also makes the SQL-pushdown surface uniform: the same machinery handles relational and scalar pushdown because everything is a join.

The operator-relation is a **reasoning** device, never a materialized one: `plus` above is the infinite relation `{<x, y, z> : x + y = z}`. The optimizer sees a uniform JOIN; the executor recognizes "join against a function-relation keyed by its parameter attributes" and runs a per-tuple scalar map. See "The core is a vocabulary, not an execution plan."

## What every RelIR node carries

- A **heading** (RM Pre 9 — see [conformance.md](conformance.md)): `{attribute → declared type}`. The shape of the relation this node produces.
- An **FD set** for candidate-key inference (VSS 3). Best-effort. Propagated through project / equijoin / restrict.
- A **constraint set** for constraint inference (RM Pre 23): the boolean predicates known to hold on the relation's tuples. Used for view-constraint checking and as optimizer hints.
- A **storage origin** flag: rooted in relvars (push to SQL) vs. rooted in materialized values (in-process) vs. mixed.

*Built today: heading and storage origin, plus declared candidate keys on the leaf. The FD set and constraint set are designed but not yet present — see Implementation status.*

## The cut: SQL vs in-process

The storage-origin flag drives the optimizer's central decision: **where each subtree runs**.

- A subtree whose every leaf is a public relvar in the same backend → push to SQL via [`coddl-sqlemit`](sqlemit.md). The optimizer can rewrite the whole subtree as one prepared SQL plan.
- A subtree whose every leaf is a `Relation` literal or private relvar (or any other materialized value) → evaluate in-process. `coddl-execlocal` lowers it to a sequence of ProcIR calls into the runtime library at compile time; the [runtime](runtime.md) RelIR interpreter walks it at runtime for dynamic plans.
- A mixed-origin subtree → **ship the local relation up into SQL, never pull the relvar down into memory** (settled while waiting for a cost model; see [principles.md](principles.md) §1). Each maximal materialized subtree collapses to a `RelParam` leaf — a relation-valued bind parameter: the in-process engine computes the local operand (a semijoin rhs narrowed to the shared attributes), and its *result rows* ship into the pushed query as a `VALUES`-backed derived table at the force point — which, holding the relation, dispatches on its cardinality: 0 at an absorbing slot short-circuits to an empty result with no statement, 1 at a root-`matching` rhs fires the baked cardinality-1 sibling plan (a plain `WHERE shared = ?N…` keyed lookup), n runs the general `EXISTS`+`VALUES` form. The whole op runs where the unbounded data lives; the bounded side crosses the boundary. A future cost model refines the mechanism for large local relations (temp tables); it does not reopen the default direction.

**Maximum pushdown** is the goal — draw the cut as close to the leaves as possible, push everything that touches a relvar into SQL, do the rest in-process. The cut is per-subtree, not per-program; one program can mix both engines freely.

*Built today: the origin gate (`Materialized` stays in-process; `RelvarRooted` and `Mixed` push when emission succeeds) and the `RelParam` shipping above. The cost model is not built; the remaining non-pushable shapes fall to the in-process path, where the S1 tripwire (`guard_no_full_relvar_pull`) panics rather than silently pulling a whole relvar — see Implementation status.*

## What pushes down and what doesn't

What pushes cleanly:
- Algebra A core (JOIN, AND, OR, AND NOT, project, replace — the `Rename` node).
- Semijoin / antijoin (surface `matching` / `not matching`) — a root `Semijoin` emits a correlated `WHERE [NOT] EXISTS` (see [sqlemit.md](sqlemit.md)).
- Plain transitive closure (TCLOSE) — a root `tclose` emits a `WITH RECURSIVE` query (see [sqlemit.md](sqlemit.md)).
- Aggregation (SUMMARIZE).
- Restriction predicates whose operators have SQL equivalents (`=`, `<`, `+`, `mod`, etc.).
- Subset and superset (the `<=` / `>=` on relations — see [grammar.md](grammar.md)).

What doesn't push (and forces the cut higher):
- User-defined scalar operators not registered with the backend (this is open — see [risks.md](risks.md) risk #2, "How honest about SQL are you willing to be?").
- Recursive / fixpoint queries beyond the dialect's `WITH RECURSIVE` support.
- A local relation whose heading carries a Tuple-valued attribute (no scalar SQL cell), or a nullary local (no zero-column `VALUES` form) — the mixed push declines and the S1 tripwire marks the gap. A plain scalar-celled local operand *does* push, shipped as a relation-valued parameter (see "The cut").

## RelIR as data, RelIR as compile target

RelIR plays two roles depending on when it's consumed:

1. **At compile time** — the static path. The optimizer walks RelIR, makes the cut, and hands each subtree to `coddl-sqlemit` (which produces a SQL string baked as a `plan_id`) or to `coddl-execlocal` (which produces a sequence of ProcIR calls into the [runtime](runtime.md) library). The RelIR itself doesn't survive; it's consumed.

2. **At runtime** — the dynamic path. For relation-polymorphic operators that can't be monomorphized, or query shapes built from a relation value at runtime, the RelIR survives as data. The runtime hosts both `coddl-sqlemit` (as a library) and a small RelIR interpreter that walks the plan and calls the same runtime-library primitives the static path uses. See [runtime.md](runtime.md) "Reaching the engines."

`coddl-execlocal` and the runtime interpreter are **two consumers of the same RelIR**, separated by when they run.

## Adjacent crates

| Crate | Role |
|---|---|
| `coddl-relir` | RelIR types + optimizer. The pure data-structure-and-rewrites home. |
| `coddl-sqlemit` | Consumes RelIR, emits SQL strings. See [sqlemit.md](sqlemit.md). |
| `coddl-execlocal` | Consumes RelIR, emits ProcIR call sequences. Compile-time only. |
| `coddl-procir` | The procedural IR both sqlemit and execlocal write into. See [procir.md](procir.md). |
| `coddl-runtime` | Hosts the in-process runtime library, the runtime RelIR interpreter, and `coddl-sqlemit` as a library. See [runtime.md](runtime.md). |

*Status: `coddl-relir` today is types + analyses plus one rewrite (`card1_semijoin_specialization`, the cardinality-1 semijoin sibling — the seed of the normalize pass); `coddl-execlocal` is an empty stub — in-process lowering currently lives in `coddl-procir`. See Implementation status.*

See [workspace.md](workspace.md) for the broader crate layout.

# RelIR — relational IR (Algebra A core + sugar)

RelIR is Coddl's relational IR. It's where the relational *algebra* lives — the operators that consume relations and produce relations, with their headings, FD sets, constraints, and the storage-origin flags that drive the SQL-vs-in-process cut.

RelIR sits **above** ProcIR in the pipeline (see [procir.md](procir.md)). It's not a peer. Each RelIR subtree is consumed by exactly one of two crates — `coddl-sqlemit` for SQL-rooted subtrees, `coddl-execlocal` for in-process subtrees — both of which emit into ProcIR. (That two-crate split is the target; today only `coddl-sqlemit` is wired — see **Implementation status** below.) ProcIR knows nothing about algebra; RelIR is the one place algebra exists.

This split is what lets the same algebra fragment run as SQL or as compiled native code without two parallel representations to keep in lockstep.

## Implementation status

This doc describes the **target** RelIR. The code implements a thin slice of it; the rest is designed-not-built. The split is called out here so the rest of the doc can describe the design without reading as a description of the current tree. (Per-operator status is tracked separately; this section stays coarse so it doesn't drift per-commit.)

**Built today**

- `RelExpr` (`coddl-relir/src/expr.rs`) — four nodes: the `RelvarRef` leaf, `Restrict` (surface `where`), `Project` (project-over *and* project-away), and `Rename`.
- Per-node **heading** inference and the **storage-origin** flag; declared candidate **keys** on the leaf, used for key-based `DISTINCT`-elision (`needs_distinct`).
- **The cut** as a trivial origin gate — *not* a cost model: push a subtree iff every leaf is relvar-rooted and SQL emission succeeds (`coddl-procir/src/cut.rs`).
- **SQL pushdown** of relvar-rooted `RelvarRef` / `Restrict` / `Project` / `Rename` via `coddl-sqlemit`.
- An **in-process path** for non-pushable subtrees — currently lowered to ProcIR within `coddl-procir` itself (not yet through `coddl-execlocal`).
- Restriction predicates are an `attr <cmp> literal` comparison
  (`Predicate::AttrCmp { attr, op: CmpOp, value }`), where `<cmp>` is `=`/`<>`
  (Integer/Text/Boolean) or `<`/`<=`/`>`/`>=` (Integer). Only `=` pins a value,
  so only it bounds cardinality for `DISTINCT`-elision. A **bare Boolean
  attribute** predicate `R where flag` is the equality `flag = true` (a
  Boolean-valued attribute is itself a proposition) and pushes as
  `Predicate::AttrCmp { attr, Eq, Boolean(true) }` → `WHERE "flag" = ?` — the
  formatter canonicalizes `flag = true` to the bare form, so both surface
  spellings must push identically.
- A **conjunctive `where`** (`R where p and q and …`) of pushable comparisons
  pushes: the lowerer (`collect_conjuncts`) splits it into one `Restrict` per
  conjunct — the identical tree the stacked spelling `R where p where q` builds —
  and SQL emission's `resolve` coalesces stacked `Restrict`s into a single
  `WHERE p AND q …`. So the two spellings emit one identical `SELECT`; if *any*
  conjunct isn't pushable the whole restriction declines and runs in-process.
  (Disjunction is not yet a predicate; an `or` still declines the push.)

**Designed, not yet built**

- The remaining **A-core nodes** (`AND`, `OR`, `NOT`, `TCLOSE`) and the **sugar → A-core desugaring**. The four nodes above are consumed as-is; nothing is rewritten into A-core form yet. (`REMOVE` and `RENAME` already exist, as `Project` and `Rename`.)
- The rest of the **sugar layer**: `Intersect`, `Compose`, `SemiJoin`, `SemiMinus`, `Summarize`, `Group`, `Ungroup`. (`Join`/`Union`/`Minus`/`Extend`/`Rename`/`Wrap`/`Unwrap` are built — the last two as `RelExpr::Wrap`/`Unwrap`, lowering to `Inst::Restructure` → `coddl_relation_restructure`, in-process; SQL push deferred.)
- The **optimizer** and **cost model**, the `MaterializeAtBoundary` node, and mixed-origin handling beyond the `StorageOrigin::Mixed` flag.
- The per-node **FD set** and **constraint set** (only heading, origin, and leaf keys exist today).
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

`Project`, `Restrict` (surface `where`), `Join`, `Union`, `Minus`, `Intersect`, `Compose`, `SemiJoin`, `SemiMinus`, `Extend`, `Summarize`, `Group`, `Ungroup`, `Wrap`, `Unwrap`.

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
- A mixed-origin subtree → the optimizer inserts a `MaterializeAtBoundary` node. Each side lowers independently; the boundary becomes either "pull SQL results into memory" or "ship in-process rows into a temp table." The cost model picks.

**Maximum pushdown** is the goal — draw the cut as close to the leaves as possible, push everything that touches a relvar into SQL, do the rest in-process. The cut is per-subtree, not per-program; one program can mix both engines freely.

*Built today: the cut is a trivial origin gate (push when every leaf is relvar-rooted); the cost model, `MaterializeAtBoundary`, and mixed-origin splitting are not yet built — see Implementation status.*

## What pushes down and what doesn't

What pushes cleanly:
- Algebra A core (JOIN, AND, OR, AND NOT, project, replace — the `Rename` node).
- Plain transitive closure (TCLOSE) — a root `tclose` emits a `WITH RECURSIVE` query (see [sqlemit.md](sqlemit.md)).
- Aggregation (SUMMARIZE).
- Restriction predicates whose operators have SQL equivalents (`=`, `<`, `+`, `mod`, etc.).
- Subset and superset (the `<=` / `>=` on relations — see [grammar.md](grammar.md)).

What doesn't push (and forces the cut higher):
- User-defined scalar operators not registered with the backend (this is open — see [risks.md](risks.md) risk #2, "How honest about SQL are you willing to be?").
- Recursive / fixpoint queries beyond the dialect's `WITH RECURSIVE` support.
- Anything that touches a private relvar or a relation literal on either side of a join.

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

*Status: `coddl-relir` today is types only (no optimizer); `coddl-execlocal` is an empty stub — in-process lowering currently lives in `coddl-procir`. See Implementation status.*

See [workspace.md](workspace.md) for the broader crate layout.

# RelIR — relational IR (Algebra A core + sugar)

RelIR is Coddl's relational IR. It's where the relational *algebra* lives — the operators that consume relations and produce relations, with their headings, FD sets, constraints, and the storage-origin flags that drive the SQL-vs-in-process cut.

RelIR sits **above** ProcIR in the pipeline (see [procir.md](procir.md)). It's not a peer. Each RelIR subtree is consumed by exactly one of two crates — `coddl-sqlemit` for SQL-rooted subtrees, `coddl-execlocal` for in-process subtrees — both of which emit into ProcIR. ProcIR knows nothing about algebra; RelIR is the one place algebra exists.

This split is what lets the same algebra fragment run as SQL or as compiled native code without two parallel representations to keep in lockstep.

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

## Sugar layer

Desugars to A core during the same lowering pass that builds RelIR — sugar does not survive into the optimizer:

`Project`, `Restrict` (surface `where`), `Join`, `Union`, `Minus`, `Intersect`, `SemiJoin`, `SemiMinus`, `Extend`, `Summarize`, `Group`, `Ungroup`, `Wrap`, `Unwrap`.

PascalCase as Rust enum-variant names; the corresponding surface keywords are lowercase (`join`, `union`, `extend`, …) — see [grammar.md](grammar.md).

## Operators as relations

Crucially, **operators are themselves relations** in the A formulation: a scalar function `f(X, Y) -> Z` is an (n+1)-ary relcon `F{X, Y, Z}`. So surface

```
extend r add { c: x + y }
```

desugars to the A-level

```
r join (plus rename { a: x, b: y, c: z })
```

(where `plus` is the `Integer × Integer → Integer` operator viewed as a 3-ary relation). Surface `where`-clauses similarly desugar to joins against constant relations. This collapses much of the operator zoo into pure JOIN-and-REMOVE — which is what the optimizer actually wants. It also makes the SQL-pushdown surface uniform: the same machinery handles relational and scalar pushdown because everything is a join.

## What every RelIR node carries

- A **heading** (RM Pre 9 — see [conformance.md](conformance.md)): `{attribute → declared type}`. The shape of the relation this node produces.
- An **FD set** for candidate-key inference (VSS 3). Best-effort. Propagated through project / equijoin / restrict.
- A **constraint set** for constraint inference (RM Pre 23): the boolean predicates known to hold on the relation's tuples. Used for view-constraint checking and as optimizer hints.
- A **storage origin** flag: rooted in relvars (push to SQL) vs. rooted in materialized values (in-process) vs. mixed.

## The cut: SQL vs in-process

The storage-origin flag drives the optimizer's central decision: **where each subtree runs**.

- A subtree whose every leaf is a public relvar in the same backend → push to SQL via [`coddl-sqlemit`](sqlemit.md). The optimizer can rewrite the whole subtree as one prepared SQL plan.
- A subtree whose every leaf is a `Relation` literal or private relvar (or any other materialized value) → evaluate in-process. `coddl-execlocal` lowers it to a sequence of ProcIR calls into the runtime library at compile time; the [runtime](runtime.md) RelIR interpreter walks it at runtime for dynamic plans.
- A mixed-origin subtree → the optimizer inserts a `MaterializeAtBoundary` node. Each side lowers independently; the boundary becomes either "pull SQL results into memory" or "ship in-process rows into a temp table." The cost model picks.

**Maximum pushdown** is the goal — draw the cut as close to the leaves as possible, push everything that touches a relvar into SQL, do the rest in-process. The cut is per-subtree, not per-program; one program can mix both engines freely.

## What pushes down and what doesn't

What pushes cleanly:
- Algebra A core (JOIN, AND, OR, AND NOT, project, rename).
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

See [workspace.md](workspace.md) for the broader crate layout.

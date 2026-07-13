# Coddl — core principles

Four principles, binding on every design choice in the project. A proposal that violates one needs an explicit override, not a quiet exception. These are the lens every other doc applies; when a topic doc says "this is the right shape because…" the answer usually traces back here.

## 1. Performance

Runtime cost is a first-class concern. The host language (Rust), the runtime (no GC, no managed RTS in user binaries), the FFI layer (zero-copy `#[repr(C)]` values), and the IR (Algebra A — push-down-friendly) are chosen for it. Features that force unavoidable overhead the user can't opt out of are rejected. When two designs are otherwise equivalent, the one with the lower steady-state cost wins.

**Our optimizer does all the work — never defer optimization to the backend.** We cannot assume the backend's SQL query planner is strong: SQLite's is weak, and how capable any given backend will be is not something we get to predict. So we never emit a naive query trusting the planner to rewrite it — we emit output already in its best shape. This is **absolute for ProcIR**, where Coddl is *both* optimizer and executor: there is no downstream optimizer at all, so an optimization we skip is an optimization that never happens. It is **binding for SQL pushdown** too — emit the semijoin's `WHERE EXISTS`, not an `INNER JOIN` + `DISTINCT` the planner would have to collapse back into a semijoin. The standing tracked corollary is **`DISTINCT` / dedup elision**: `SELECT DISTINCT` and the in-process seal each cost a sort/hash, so we drop them wherever the result is *provably* already a set — a surviving candidate key, cardinality ≤ 1, or FD/key inference propagated through the algebra (`join`/`compose`/`project`/`semijoin`/set-ops). Every provable case is ours to catch; see [sqlemit.md](sqlemit.md) ("`DISTINCT` elision") for the running catalog of what's handled and what's still owed, and [relir.md](relir.md) for the `surviving_keys()`/`needs_distinct()` mechanism.

**Mixed-origin queries ship the local relation up — never pull the relvar down.** The same principle applied to the cut: when a query mixes a public relvar with an in-process relation value, the in-process side is bounded (a request path, a literal, a private relvar) while the relvar side is not — so the local operand's rows ship into SQL as a relation-valued parameter (a `VALUES`-backed derived table) and the whole query runs where the unbounded data lives. Pulling an unfiltered relvar into the process to serve a mixed op is a rejected shape (the S1 tripwire panics on the remaining gaps rather than silently regressing). This is the settled rule *while waiting for a cost model*; a future cost model refines the boundary choice, it does not reopen the default. See [relir.md](relir.md) ("The cut") and [sqlemit.md](sqlemit.md) ("Relation-valued parameters").

## 2. Long-term planning

IR shapes, type representations, and crate boundaries are designed so deferred Manifesto features (VSS 7 heading polymorphism, transition constraints, type inheritance) and unanticipated extensions land without a rewrite. No painting into corners — keep the data structures wider than current need, and the boundaries semantic rather than expedient.

## 3. Conformance over convenience

When TTM prescribes a behavior, Coddl ships it — even when a non-conforming shortcut would be easier. Sanctioned design freedoms are enumerated in [conformance.md](conformance.md) and bounded there. Anything that looks like a new design freedom needs an explicit add to that list, not an off-the-cuff exception.

## 4. Few primitives, layered sugar

Algebra A core operators (see [relir.md](relir.md)); operators-as-relations; no special cases. Surface sugar — `extend`, `where`, `summarize` — desugars during lowering. Sugar lives in one place, not woven through the IR.

## Coddl is its own D

Tutorial D is the Manifesto's reference D, useful as a study aid and prior-art benchmark, not a spec Coddl follows. Where TTM prescribes behavior, Coddl conforms. Where TTM is silent, Coddl picks the answer aligned with the four principles above — convergence with Tutorial D's specific choice is incidental, not a goal.

The sanctioned design freedoms (host language, surface syntax, evaluation strategy, IR choice) are enumerated in [conformance.md](conformance.md) and bounded there.

## Toward self-hosting (long-term)

A standing aspiration: reimplement as much of Coddl *in Coddl* as possible — ideally all the way down. "80% self-hosted" is already a win.

The fault line follows what each layer *is*. The relational/logic layers — `coddl-types`, `coddl-relir`, the emission *logic* of `coddl-sqlemit`, type/constraint analysis — port naturally, because a compiler's data (AST, IR, symbol tables) is just relvars of nodes and edges (the "compilers as databases" lineage). The bottom — `coddl-runtime` (refcounts, C ABI), the codegen crates (LLVM text emission), `coddl-backend-sqlite` (the rusqlite FFI wrapper) — needs pointers and raw memory the surface language deliberately forbids. To reach those too, Coddl may eventually grow an `unsafe`-equivalent (marked blocks permitting pointers and raw operations); that would be a **new sanctioned design freedom** — it conflicts with today's "no pointer/box type" and "no recursive type definitions" rules (see [memory.md](memory.md)) and must be added to [conformance.md](conformance.md) explicitly, not slipped in.

This goal steers boundaries *now*, even though no layer is self-hosted in v1:

- **Dependencies must not cross the future seam backwards.** A layer that will stay Rust (the FFI bottom) must not depend on a layer destined to become Coddl (the relational middle). Example: `coddl-backend-sqlite` gets its own storage `Value` type rather than reusing `coddl-relir`'s `Literal`, so the permanent-Rust backend stays decoupled from the will-become-Coddl IR.
- **Elegant Rust in every layer; clean interfaces between them.** Self-hosting *adds* the boundary discipline above — it does not lower the bar for internals. Each layer still gets the best Rust we can write: it has to be excellent in its own right, and clean code is the clearest spec for the eventual Coddl port (you translate good code, you don't untangle bad).
- **RelIR's eventual shape is relations, not a Rust enum tree** — keep that in view when extending it.

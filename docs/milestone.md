# First milestone

The first end-to-end milestone is a toy program that compiles, runs, and reads/writes a SQLite-backed relvar. Getting there exercises every subsystem in `docs/` at least once, which is what flushes out the cross-crate seams before they harden.

## Sequence

The ordering below reflects the architecture described in [relir.md](relir.md), [procir.md](procir.md), and [runtime.md](runtime.md): RelIR is the algebra, `coddl-sqlemit` and `coddl-execlocal` are peer consumers of RelIR, the runtime hosts both the SQL backend and an in-process runtime library + RelIR interpreter.

1. **Lex + parse** the uniform-prefix-syntax core (RM Pre 1, 6–10, 13–14, 18 per [conformance.md](conformance.md)): scalar declarations, possrep/selector, relvar declarations, `join`, `where`/`restrict`, `extend`, simple `summarize`, `rename`, `project`. Multiple assignment. **Establish the spans-on-every-node and diagnostics-as-values discipline from [lsp.md](lsp.md) here** — these are project-wide invariants, not LSP-conditional. The parser does error recovery from day one (no bailing on the first syntax error; emit `PARSE_ERROR` CST nodes and continue).

2. **Type-check** headings, possreps, and selector signatures (see [typecheck.md](typecheck.md)). Enforce no-nulls, no-duplicates at the type level. Verify candidate keys are declared and minimal. Type errors propagate via `Error` types, not cascades.

3. **Lower to RelIR** (sugar → A core during the same pass; see [relir.md](relir.md)). Emit SQLite SQL via `coddl-sqlemit`, honoring every rule in [sqlemit.md](sqlemit.md).

4. **Hand-write the runtime** around: the SQL backend, the **runtime RelIR interpreter** (which walks RelIR plans for the in-process path), the prepared-statement cache, explicit transactions, and multiple assignment. Run programs by *interpreting* RelIR — no LLVM yet. See [runtime.md](runtime.md) for the engine architecture.

5. **Add the in-process runtime library** — the compiled relational primitives (`coddl_relation_where`, `coddl_relation_join`, …) that the interpreter calls into when it hits an in-process subtree. `Relation` literals and constructed relations work end-to-end at this point.

6. **Add ProcIR + the LLVM codegen crate** (see [procir.md](procir.md), [codegen.md](codegen.md)): `load`, counted `do` loops, `query → relation → load → iterate`. **Add `coddl-execlocal`** at the same time — the compile-time RelIR → ProcIR lowering for in-process subtrees. Statically-known plans now compile to native code via LLVM; the runtime interpreter stays for dynamic plans (see [runtime.md](runtime.md), "Reaching the engines"). Link the runtime as a `staticlib` and confirm the FFI struct layout matches the LLVM emission.

7. **Add the Postgres backend** behind the same `Backend` trait (see [storage.md](storage.md)). Confirm the golden SQL tests fork cleanly per dialect.

8. **Add user-defined scalar types** with possreps, selectors, `THE_C` ops, and possrep constraints. Confirm equality works through the possrep round-trip.

VSS adoptions (system keys/TAG, candidate-key inference, transition constraints, RANK quota queries — see [conformance.md](conformance.md)) come after the milestone above is end-to-end on a toy program.

## Why this ordering

Steps 1–3 are the frontend; once they land, every later step has a stable AST → RelIR → SQL pipeline to test against. Step 4 (interpreter) ships a working runtime *before* LLVM lands, which means the SQL emitter and the in-process semantics get exercised against real programs before codegen complexity enters the picture. Step 6 introduces ProcIR + `coddl-execlocal` together because they're the same architectural move (compile-time lowering of RelIR to procedural code) — splitting them would create an awkward intermediate state where ProcIR exists but isn't yet the static-plan target.

The interpreter doesn't go away when LLVM lands at step 6. It stays as the dynamic-plan engine for the cases where compile-time specialization isn't possible (relation-polymorphic operators where the heading is only known at runtime; query shapes constructed from user input). See [runtime.md](runtime.md) "Reaching the engines: compile-time lowering vs. runtime interpretation" for the long-term shape.

## What's deferred

Anything in [conformance.md](conformance.md) marked "deferred" or "skipped" — VSS 6 generalized transitive closure (depends on VSS 7), VSS 7 heading polymorphism, type inheritance, VSS 8 SQL migration. The type system stays designed to *accept* them later without rewriting (the long-term-planning principle in [principles.md](principles.md)) but the milestone doesn't ship them.

Anything in [risks.md](risks.md) that's still unresolved at step 5 needs to be resolved before the relevant later step — see each risk's "decide before" trigger.

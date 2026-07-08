# Coddl end-to-end validation

This document captures the cross-backend equivalence invariant that
the language commits to from the hello-world milestone forward, and
the discipline for keeping it true as the language grows.

For the per-backend codegen rules that make the equivalence possible
(shared C ABI for `Text`, common `main` special case, identical
runtime extern surface), see `docs/codegen.md`. For the driver flow
that exercises both backends in one process, see `docs/driver.md`.

**Last sync:** `c815e64`. Every commit that adds an example program,
changes the validation matrix, or alters how cross-backend
equivalence is enforced updates this file in the same commit.


## The equivalence invariant

> For every source program `P` and every backend `B ∈ {llvm,
> cranelift}`, `coddl run --backend=B P` produces identical stdout
> bytes and an identical exit code.

This is the contract `coddl run` ships under. It's testable:
`assert_eq!(llvm.stdout, cranelift.stdout)` is the assertion that
fails the build when a backend drifts. The same goes for the binary
produced by `coddl compile`.

The invariant exists because ProcIR is the single source of truth.
The lowering pass — *not* the backends — decides what `main` does:
how strings decompose at the C-call boundary, when init/shutdown
fire, which builtin maps to which linkage name. Both backends
walk the same IR and emit the same calls in the same order.
Divergence is therefore an emit-side bug, not a design difference.


## Validation matrix

`crates/coddl-driver/tests/e2e.rs` hosts the equivalence assertions.
Each example program contributes three test entries:

| Test                                           | What it asserts                                    |
|------------------------------------------------|----------------------------------------------------|
| `<example>_llvm_backend_*`                     | LLVM run produces the expected stdout.             |
| `<example>_cranelift_backend_*`                | Cranelift run produces the expected stdout.        |
| `<example>_byte_identical_across_backends`     | LLVM and Cranelift stdouts are byte-equal.         |

Hello-world is the seed example. As the language grows (Phase 9 and
beyond), each new example program lands with the same three-test
shape. When the test count gets unwieldy, a small driver macro or a
generated parameterization replaces the hand-rolled trio.

The matrix is intentionally minimal at the validation milestone —
one program exercising the full pipeline (lex, parse, check, lower,
two backends, runtime) is enough to prove the invariant holds. The
size of the suite grows with the language; the discipline doesn't
change.

### Canonical examples

| Program          | Files                                                                          | Surface covered                                                                                                                                  |
|------------------|--------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------|
| `hello-world`    | `examples/hello-world/hello-world.cd`                                          | Single-file program: lex → parse → typecheck → lower → both backends → link → `coddl_write_line`. The seed example (Phase 9).                    |
| `hello-world-db` | `examples/hello-world/{hello-world-db.cd, greetings.cddb, greetings.cdstore}` + locally-seeded `greetings.sqlite` | Four-file `.cd` family: Phase 16 plan discovery, public-relvar materialization from SQLite at startup, `transaction [...]` brackets, `extract (R where p)` reading one row, field access on the extracted tuple. Adds Phase 22's storage path to the equivalence proof. |
| `use-module`     | `examples/use-module/use-module.cd`                                            | Opt-in modules: `use module coddl::env;` → the `builtin relvar` `Environment` (the process environment as a relation, read via the FFI `coddl_env_snapshot`) → `where` → `load … order` → `for … in` → field access. The three e2e tests set `CODDL_DEMO` on the child process for deterministic stdout. |

Each canonical example contributes the three e2e tests in
`crates/coddl-driver/tests/e2e.rs`. The hello-world-db trio also has
two adjacent driver tests (env-var override flow + a T0025 negative
path) that prove ancillary properties but aren't part of the
strict cross-backend matrix.


## Adding a new example

1. Drop the program at `examples/<name>/<name>.cd`.
2. If the program declares one or more `public relvar`s, walk the
   companion-file checklist below before wiring tests.
3. In `crates/coddl-driver/tests/e2e.rs`, add the three test
   functions following the hello-world pattern.
4. Run `cargo test --workspace`. Both per-backend tests must pass;
   the cross-backend test must too.
5. If they pass independently but disagree on stdout, that's a
   real bug. Don't paper over with `assert!(matches!(...))` —
   chase the divergence in ProcIR or the offending backend.

### Companion-file checklist (public-relvar programs)

A `.cd` that declares any `public relvar` must ship with the
companions Phase 16's plan discovery expects, plus a seed script the
test harness can invoke. Without these, plan discovery fails before
codegen runs and the test never reaches the per-backend assertion.

- [ ] `examples/<name>/<name>.cd` with `program <name>;`,
      `database <db>;`, and the `public relvar` declarations.
- [ ] `examples/<name>/<db>.cddb` — catalog-side conceptual schema
      (`base relvar <Name> { ... } key { ... };` per public relvar).
      Headings must match (PL0007 otherwise).
- [ ] `examples/<name>/<db>.cdstore` — physical binding
      (`store for <db>;`, `backend sqlite { file: "<db>.sqlite" };`,
      one `relvar <Name>: table "<sql>" { columns: { ... } };` per
      public relvar). Column coverage is enforced by PL0009 / PL0010.
- [ ] `examples/<name>/seed-db.sh` — idempotent shell script that
      rebuilds the fixture SQLite from scratch (`rm -f <db>.sqlite`
      then `sqlite3 <db>.sqlite <<SQL ... SQL`). The script is the
      source of truth for what's in the database.
- [ ] `examples/<name>/.gitignore` listing `<db>.sqlite` — the
      fixture is locally generated, never committed.
- [ ] In the e2e tests: gate every test entry behind a
      `OnceLock`-guarded helper that runs the seed script once per
      test process (the parallel test scheduler will otherwise race
      on `rm -f` + `sqlite3 ...`). Follow the
      `ensure_hello_world_db_seeded` pattern.

The five-part hygiene gate (fmt, build, clippy, tests,
check-grammar) gates every commit; nothing about validation
shortcuts it.


## What this validation *does not* prove

- **Performance equivalence.** Both backends produce the same stdout,
  not the same throughput. Cranelift's codegen is faster to generate
  but typically slower to execute than LLVM's optimized output. The
  `coddl run` / `coddl compile` default-backend split (Cranelift for
  iteration, LLVM for delivery) acknowledges this.
- **Behavior under panic / signal.** The cross-backend assertion is
  on normal termination's stdout. Phase 21's `extract` cardinality
  abort and Phase 22's materialization-error abort have per-backend
  assertions (`!status.success()` + stderr substring match) but the
  byte-equality contract doesn't extend to abort paths. Process
  signals and runtime externs returning a nonzero status are not yet
  validated cross-backend.
- **Optimization-level invariance.** No flags toggle optimization
  yet. When `-O` lands, the matrix grows to assert equivalence at
  each level — codegen bugs often surface only at higher tiers.

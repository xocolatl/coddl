# Coddl end-to-end validation

This document captures the cross-backend equivalence invariant that
the language commits to from the hello-world milestone forward, and
the discipline for keeping it true as the language grows.

For the per-backend codegen rules that make the equivalence possible
(shared C ABI for `Text`, common `main` special case, identical
runtime extern surface), see `docs/codegen.md`. For the driver flow
that exercises both backends in one process, see `docs/driver.md`.

**Last sync:** `dae8068`. Every commit that adds an example program,
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


## Adding a new example

1. Drop the program at `examples/<name>/<name>.cd`.
2. In `crates/coddl-driver/tests/e2e.rs`, add the three test
   functions following the hello-world pattern.
3. Run `cargo test --workspace`. Both per-backend tests must pass;
   the cross-backend test must too.
4. If they pass independently but disagree on stdout, that's a
   real bug. Don't paper over with `assert!(matches!(...))` —
   chase the divergence in ProcIR or the offending backend.

The five-part hygiene gate (fmt, build, clippy, tests,
check-grammar) gates every commit; nothing about validation
shortcuts it.


## What this validation *does not* prove

- **Performance equivalence.** Both backends produce the same stdout,
  not the same throughput. Cranelift's codegen is faster to generate
  but typically slower to execute than LLVM's optimized output. The
  `coddl run` / `coddl compile` default-backend split (Cranelift for
  iteration, LLVM for delivery) acknowledges this.
- **Behavior under panic / signal.** The assertion is on a normal
  termination's stdout. Behavior on a runtime panic, a process
  signal, or a runtime extern returning a nonzero status is not yet
  validated. Those land when the language grows panic semantics.
- **Optimization-level invariance.** No flags toggle optimization
  yet. When `-O` lands, the matrix grows to assert equivalence at
  each level — codegen bugs often surface only at higher tiers.

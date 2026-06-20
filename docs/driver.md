# Driver — the `coddl` CLI

The authoritative spec for the `coddl` command-line driver: every subcommand, every flag, every exit code, and the runtime-staticlib discovery rule the compiled-binary subcommands depend on.

The driver is the user's first contact with Coddl. It calls into the frontend crates ([grammar.md](grammar.md), [typecheck.md](typecheck.md)) for `lex` / `parse` / `check`, into the plan layer ([plan.md](plan.md)) when a `.cd` declares public relvars, into [codegen.md](codegen.md) for emission, and links against the runtime [staticlib](runtime.md). Frontend diagnostics are routed through `coddl-diagnostics` (see [lsp.md](lsp.md)) so terminal output and LSP output share one source.

**Last sync:** `c42493a`. Every commit that adds, removes, or changes a subcommand, a flag, an exit code, or the runtime-discovery rule updates this file in the same commit.


## Subcommand reference

| Subcommand          | Input         | Output                                     | Default flags                  |
|---------------------|---------------|--------------------------------------------|--------------------------------|
| `lex <file>`        | file or `-`   | token stream → stdout                      | —                              |
| `parse <file>`      | file or `-`   | rust-analyzer-style CST dump → stdout      | —                              |
| `check <file>`      | file or `-`   | diagnostics → stderr                       | —                              |
| `lower <file>`      | file or `-`   | ProcIR module's `Display` form → stdout    | —                              |
| `explain <file>`    | file or `-`   | as-lowered RelIR + SQL per pushed query → stdout | —                        |
| `emit-llvm <file>`  | file or `-`   | LLVM IR text → stdout                      | —                              |
| `emit-obj <file>`   | file or `-`   | Cranelift object bytes → stdout (or `-o`)  | `-o <path>` optional           |
| `compile <file>`    | file (or `-` with `-o`) | native binary at `<output>`     | `--backend=llvm`, `-o <basename>` |
| `run <file>`        | file or `-`   | compiled binary's stdout/stderr            | `--backend=cranelift`          |
| `fmt <file>`        | file or `-`   | formatted source → stdout                  | —                              |

Every subcommand exits `0` on success, `1` on I/O / compile failure,
`2` on usage error (unknown flag, missing required argument), and
forwards the compiled binary's exit code for `run`.


## `explain`

`explain` runs the pipeline through RelIR lowering and prints, for each
relational expression the cut pushes to SQL, the **as-lowered RelIR tree**
paired with the SQL it lowered to:

```text
query 1:
  RelIR:
    Project { keep: message }
      Restrict { id = 1 }
        RelvarRef Greetings { db: greetings, table: greetings }
  SQL:
    SELECT "message" FROM "greetings" WHERE "id" = ?
```

It is the *logical* (RelIR) view of a program's queries — what
[`coddl-sqlemit`](sqlemit.md) consumes — not an optimized query plan. Two
honest limits on the naming:

- **Not optimized.** There is no logical optimizer yet, so the tree is the
  shape lowering produced, before any rewrite.
- **Not minimal Algebra A.** It is the hybrid RelIR: `join`/`times`/… collapse
  to the A `And` core, but `Restrict`/`Project`/`Rename` are still the rich
  sugar nodes, not reduced to the Appendix-A primitives (the
  operators-as-relations desugaring [relir.md](relir.md) never materializes).

Scope is the SQL-pushdown pathway only — relational subtrees evaluated
in-process (materialized / `private` leaves) are not shown. `explain` discovers
`.cd` companions for the plan exactly as `lower` does; with no pushable backend
it prints `no relational expressions were pushed to SQL`.


## `compile` and `run`

`compile` produces a runnable binary; `run` produces one in a temp
dir and executes it immediately, propagating the binary's exit code.
Both share the same pipeline: lower → emit → link with the runtime
staticlib via `clang` (LLVM) or `cc` (Cranelift).

**Flags:**

| Flag                        | Subcommands       | Default                                 |
|-----------------------------|-------------------|-----------------------------------------|
| `--backend=llvm\|cranelift` | `compile`, `run`  | `compile`: llvm; `run`: cranelift       |
| `-o <path>`                 | `compile` only    | `<basename>` of input in CWD            |

**Default backend rationale.** `run` defaults to Cranelift because
its REPL-JIT framing (see [procir.md](procir.md) "Backend-agnostic by design") is fast iteration —
codegen completes faster than LLVM's text-and-`clang` path, which
matters when the user is running a program for the first time and
just wants output. `compile` defaults to LLVM because it's the v1
optimized AOT backend — when the user is producing a deliverable,
the extra emit-and-`clang` cost buys optimization quality.

**Stdin via `-`.** `compile -` requires `-o <path>` (no input
filename to derive a default output). `run -` works with no extra
flags.

`run` rejects `-o` with usage error 2 — to write a binary, use
`coddl compile`.


## Runtime staticlib discovery

`compile` and `run` need `libcoddl_runtime.a` to link. Lookup order:

1. **`CODDL_RUNTIME` environment variable** — interpreted as an
   absolute path to the staticlib. Takes precedence.
2. **`<exe-dir>/libcoddl_runtime.a`** — the directory containing
   the `coddl` binary (`std::env::current_exe()`'s parent). In dev
   (`cargo build`), `target/debug/coddl` and
   `target/debug/libcoddl_runtime.a` are siblings — this path
   resolves automatically after `cargo build -p coddl-runtime`.

If neither resolves, the driver exits `1` and prints a
machine-readable error listing what was tried and how to fix it
(build the staticlib, or set `CODDL_RUNTIME`).

**Installed-binary placement.** When `cargo install coddl-driver`
becomes a real path, the install layout will need to provide the
staticlib alongside the executable. For Phase 7's hello-world scope,
side-by-side placement is the discipline.


## Exit codes

| Code | Cause                                                                  |
|------|------------------------------------------------------------------------|
| 0    | Success.                                                               |
| 1    | I/O failure, compile error, link error, or missing runtime staticlib.  |
| 2    | Usage error — unknown flag, missing required argument, invalid backend.|
| _N_  | For `run`, the compiled binary's exit code (forwarded unchanged).      |
| 128  | For `run`, the compiled binary was killed by a signal (no exit code).  |

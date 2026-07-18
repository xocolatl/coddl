# Driver — the `coddl` CLI

The authoritative spec for the `coddl` command-line driver: every subcommand, every flag, every exit code, and the runtime-staticlib discovery rule the compiled-binary subcommands depend on.

The driver is the user's first contact with Coddl. It calls into the frontend crates ([grammar.md](grammar.md), [typecheck.md](typecheck.md)) for `lex` / `parse` / `check`, into the plan layer ([plan.md](plan.md)) when a `.cd` declares public relvars, into [codegen.md](codegen.md) for emission, and links against the runtime [staticlib](runtime.md). Frontend diagnostics are routed through `coddl-diagnostics` (see [lsp.md](lsp.md)) so terminal output and LSP output share one source.

**Last sync:** `27b20da`. Every commit that adds, removes, or changes a subcommand, a flag, an exit code, or the runtime-discovery rule updates this file in the same commit.


## Subcommand reference

| Subcommand          | Input         | Output                                     | Default flags                  |
|---------------------|---------------|--------------------------------------------|--------------------------------|
| `lex <file>`        | file or `-`   | token stream → stdout                      | —                              |
| `parse <file>`      | file or `-`   | rust-analyzer-style CST dump → stdout      | —                              |
| `check <file>`      | file or `-`   | diagnostics → stderr                       | —                              |
| `lower <file>`      | file or `-`   | ProcIR module's `Display` form → stdout    | —                              |
| `explain <file>`    | file or `-`   | as-lowered RelIR + SQL + usage sites per pushed plan → stdout | —           |
| `emit-llvm <file>`  | file or `-`   | LLVM IR text → stdout                      | —                              |
| `emit-obj <file>`   | file or `-`   | Cranelift object bytes → stdout (or `-o`)  | `-o <path>` optional           |
| `compile <file>`    | file (or `-` with `-o`) | native binary at `<output>`     | `--backend=llvm`, `-o <basename>` |
| `run <file>`        | file or `-`   | compiled binary's stdout/stderr            | `--backend=cranelift`          |
| `provision <file>`  | `.cddb` path  | per-table reconcile summary → stdout       | —                              |
| `fmt <file>`        | file or `-`   | formatted source → stdout                  | `--check`, `--write`           |

Every subcommand exits `0` on success, `1` on I/O / compile failure,
`2` on usage error (unknown flag, missing required argument), and
forwards the compiled binary's exit code for `run`.

`fmt` has three modes: with no flag it writes the formatted source to
stdout; `--check` writes nothing and exits `1` if the input isn't already
formatted (the git pre-commit hook uses this — see `tools/git-hooks/`);
`--write` rewrites the file in place (and needs a file, not stdin).
`--check` and `--write` refuse to act on input the formatter can't parse
cleanly, and are mutually exclusive.


## `explain`

`explain` runs the pipeline through RelIR lowering and prints, for each
**plan** the cut pushes to SQL, the **as-lowered RelIR tree** paired with the
SQL it lowered to and the source line of every expression that uses it:

```text
plan 0:
  RelIR:
    Project { keep: message }
      Restrict { id = 1 }
        RelvarRef Greetings { db: greetings, table: greetings }
  SQL:
    SELECT "message" FROM "greetings" WHERE "id" = ?1
  used at:
    hello.cd:7
```

Expressions are grouped by SQL text — the same identity the runtime dedups
plans by (one `PlanId`, one prepared statement) — so two expressions that
lower to identical SQL print as one entry with two `used at:` sites, and the
list shown is exactly the program's registered statement set. Each entry is
labeled by the dense id the compiled program registers — the same "plan N"
runtime messages reference. Ids need not be contiguous here: DML write plans
share the same sequence but only reads are shown, and a plan with a baked
cardinality-1 sibling additionally prints it as
`SQL (card-1 dispatch, plan M):` — the sibling is a registered plan of its
own (and registers first, so its id typically precedes the general form's).

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

**File-kind requirement.** `compile` and `run` produce an executable, so the
input must be a `program` (it has an `oper main`). A `library` or `module`
input is a usage error (exit `2`) — a `library` has no entry point and is meant
to be linked by a foreign host, so use `emit-obj` to produce its object.
`emit-obj` accepts `program`, `library`, and `module`.

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


## `provision`

`provision` reconciles a database to the state its `.cddb` catalog declares, in
**one transaction**: for each base relvar, create-or-verify its table, then
truncate and re-seed it to the relvar's INIT value (`R := Relation { … };`). The
backend is whatever the catalog's `.cdstore` binds — `provision` is
backend-agnostic and drives the store abstraction the runtime uses, so it never
hand-maps types or speaks a backend dialect itself. See [storage.md](storage.md)
for the fold-and-reconcile pipeline and the `PV####` diagnostics.

> **v1 backend coverage.** Only the SQLite backend is implemented today; a
> catalog bound to any other backend is declined (`PV0007`). Postgres and
> MariaDB are planned and reuse this command unchanged — everything below is
> backend-independent.

It is **not** a migrator. A table that already exists but doesn't match the
catalog is a rollback + error (`PV0008`), never a drop-recreate; a managed name
that resolves to a view or other non-table object is likewise an error
(`PV0009`), never altered. The rollback leaves the database byte-identical — the
invariant that keeps `provision` from becoming a destructive `migrate`.

**Input.** A `.cddb` catalog **path** only. Stdin (`-`) is rejected (exit `2`):
there is no sibling `.cdstore` to resolve the connection target from. A
non-`.cddb` extension is a usage error (exit `2`).

**Connection target.** Resolved exactly as the compiled runtime resolves it, so
`provision` and `run` act on the same database. For the SQLite backend that
target is a file path: the `CODDL_<DBNAME>_FILE` environment variable (DBNAME =
uppercase of the `database <name>;` header) if set, else the `.cdstore`'s baked
`file:` default. Other backends resolve their own target (connection string /
DSN) by the same override-then-default rule — see [storage.md](storage.md).

**Output.** A per-table summary to stdout — one line per relvar reporting
whether the table was `created` or `verified` and how many rows were re-seeded.
Diagnostics (resolution, fold-time value errors, execution) print to stderr; a
schema mismatch's per-column detail prints as indented `note:` lines. Exit `0`
on a committed reconcile, `1` on any error (resolution, folding, or a mismatch
rollback), `2` on a usage error.

```console
$ coddl provision examples/suppliers-and-parts/suppliersandparts.cddb
provisioned 3 table(s):
  p: created, 6 row(s)
  s: created, 5 row(s)
  sp: created, 12 row(s)
```


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
| 1    | I/O failure, compile error, link error, missing runtime staticlib, or a `provision` reconcile failure (resolution/fold error or schema-mismatch rollback). |
| 2    | Usage error — unknown flag, missing required argument, invalid backend.|
| _N_  | For `run`, the compiled binary's exit code (forwarded unchanged).      |
| 128  | For `run`, the compiled binary was killed by a signal (no exit code).  |

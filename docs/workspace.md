# Coddl — workspace layout

The repository is a single Cargo workspace. One crate per subsystem; semantic boundaries over expedient ones (one of the long-term-planning bills paid up front, per [principles.md](principles.md)).

```
coddl/
  Cargo.toml                       # workspace
  crates/
    coddl-diagnostics/             # shared span + diagnostic types (used by every frontend crate)
    coddl-syntax/                  # lexer + recursive-descent parser, CST (rowan) + AST view
    coddl-types/                   # type checker, type representation
    coddl-relir/                   # relational IR + optimizer (see relir.md)
    coddl-procir/                  # procedural IR — backend-agnostic SSA (see procir.md)
    coddl-sqlemit/                 # RelIR → SQL — dialect-agnostic core; used by compiler AND runtime
    coddl-execlocal/               # RelIR → ProcIR lowering for in-process subtrees (compile-time)
    coddl-backend-sqlite/          # Cargo feature on the runtime
    coddl-backend-postgres/        # Cargo feature on the runtime
    coddl-codegen-llvm/            # ProcIR → LLVM IR text emission (v1; see codegen.md)
    coddl-codegen-cranelift/       # ProcIR → Cranelift (planned; REPL JIT + toolchain-free AOT)
    coddl-codegen-wasm/            # ProcIR → wasm-encoder (optional; revisit when needed)
    coddl-runtime/                 # extern "C" staticlib linked into compiled binaries (see runtime.md)
    coddl-driver/                  # CLI: compile, run, repl, fmt (see driver.md)
    coddl-lsp/                     # tower-lsp language server; thin adapter over the frontend crates (see lsp.md)
    coddl-fmt/                     # canonical formatter library; same library behind `coddl fmt` and the LSP (see fmt.md)
  editors/
    vscode/                        # VSCode extension: TextMate grammar + language client (see lsp.md)
  docs/                            # this directory — topic docs
  tests/
    golden/                        # SQL emission goldens per backend
    e2e/                           # compile + run end-to-end
  examples/                        # one program per directory; see validation.md for the matrix
```

## Build posture

- **Release builds**: LTO on, `codegen-units = 1` for `coddl-driver` and `coddl-runtime` crates.
- **Runtime**: built as `staticlib` by default — compiled Coddl binaries link statically, no dynamic linker hit. `cdylib` can come later if plugin loading lands.
- **`panic = "abort"`** for the runtime — smaller unwinding tables, single failure mode at the FFI seam (see [runtime.md](runtime.md)).
- **`wasm32-*` targets** build the runtime with `--no-default-features` to drop the SQL backend crates (their C dependencies don't link to `wasm32-unknown-unknown`).

## Why this many crates

Each crate corresponds to a subsystem with a stable interface. The boundary between `coddl-syntax` (CST + AST view) and `coddl-types` (type checker) is the AST. The boundary between `coddl-relir` and `coddl-procir` is the RelIR → ProcIR lowering, with `coddl-sqlemit` and `coddl-execlocal` as peer consumers of RelIR (see [relir.md](relir.md) for the cut decision). The boundary between `coddl-procir` and `coddl-codegen-llvm` is ProcIR text. The boundary between the compiler and the runtime is the `extern "C"` ABI (see [runtime.md](runtime.md)).

Keeping these boundaries semantic — not "this is what currently fits" — is what lets new backends and deferred Manifesto features land without rewriting the world.

# Coddl ProcIR

This document is the authoritative spec for the procedural SSA IR that
`coddl-procir` produces: the data types, every instruction, the
AST→ProcIR correspondences, the `Codegen` trait the backends
implement, and the (currently empty) `L####` diagnostic-code table.

For *why* the IR is shaped this way — the two-IR split (RelIR for
relational expressions, ProcIR for everything else), the
backend-agnostic node language, the LLVM-text-emission v1 strategy —
see `ARCHITECTURE.md §4 "The two IRs"`. This document never duplicates
that rationale.

**Last sync:** `1830ac1`. Every commit that adds, removes, or changes
a ProcIR data type, an instruction, an AST→IR correspondence, the
`Codegen` trait, the builtin→extern map, or an `L####` code updates
this file in the same commit; `tools/check-grammar.sh` enforces the
last of those from the hygiene gate.


## Module overview

A ProcIR `Module` is one compilation unit. Field-by-field:

| Type           | Fields                                                  |
|----------------|---------------------------------------------------------|
| `Module`       | `program_name: String`, `functions: Vec<Function>`      |
| `Function`     | `name: String`, `linkage_name: String`, `params: Vec<(String, ProcType)>`, `return_type: ProcType`, `blocks: Vec<BasicBlock>` |
| `BasicBlock`   | `id: BlockId`, `insts: Vec<Inst>`, `terminator: Terminator` |
| `BlockId`      | `u32`                                                   |
| `ValueId`      | `u32` — SSA value name, rendered `%n`                   |
| `Const`        | `Integer(i64)`, `Text(Vec<u8>)`, `Unit`                 |
| `ProcType`     | `Integer`, `Rational`, `Approximate`, `Text`, `Character`, `Binary`, `Byte`, `Boolean`, `Unit`, `Pointer` |

Key invariants the lowering pass and the backends both rely on:

- **Externs carry no blocks.** `Function::blocks.is_empty()` ⇔ the
  function is an extern declaration. Hello-world produces one such
  Function for `write_line` and one with blocks for `main`.
- **`linkage_name` is the source of truth for the symbol.** For
  defined functions today, `linkage_name == name`. For externs,
  `linkage_name` is the `coddl_*` name the runtime exposes. Backends
  emit `linkage_name` verbatim and never derive it.
- **SSA: every `Inst` defines at most one `ValueId`.** `Inst::Call`
  whose `return_type` is `Unit` has `dst == None`.
- **`ProcType` is the machine-level type, not the surface type.**
  `Tuple H` becomes a struct layout (deferred). `Relation` and
  `Sequence` become runtime handles (`Pointer`). Every built-in
  scalar gets a variant from day one so backends can pattern-match
  exhaustively.


## Instruction table

| Instruction          | Operands                                                                  | Defines               | Semantics                                                                 |
|----------------------|---------------------------------------------------------------------------|-----------------------|---------------------------------------------------------------------------|
| `Const`              | `value: Const`, `ty: ProcType`                                            | `dst: ValueId`        | Materialize a compile-time constant of type `ty`.                         |
| `Call`               | `callee: String` (linkage name), `args: Vec<ValueId>`, `return_type`      | `dst: Option<ValueId>` | Call the named function. `dst` is `None` iff `return_type == Unit`.       |

## Terminator table

| Terminator    | Operand                | Semantics                                                                |
|---------------|------------------------|--------------------------------------------------------------------------|
| `Return`      | `Option<ValueId>`      | Return from the function. `None` returns `Unit`.                         |
| `Unreachable` | —                      | Reserved for paths the typechecker has ruled out. Not emitted by hello-world. |


## AST → ProcIR correspondences

The lowering walk in `coddl-procir::lower` mirrors the typechecker's
walk shape. Each `check_<x>` in `coddl-types::checker` has a sibling
`lower_<x>` here.

| AST node       | ProcIR shape                                                                                  |
|----------------|----------------------------------------------------------------------------------------------|
| `Root`         | `Module { program_name, functions }`. Iterates items in source order.                        |
| `ProgramDecl`  | Sets `Module::program_name`. No instruction emitted.                                         |
| `OperDecl`     | One `Function` with one `BasicBlock` (`block_0`). `Function::return_type` reflects the declared `-> <type>` clause (default `ProcType::Unit`). Heading params become `Function::params` typed via `ProcType`. Non-`main` opers with a non-Unit declared return capture the body's tail-expression `ValueId` and emit `Terminator::Return(Some(v))`; otherwise the terminator is `Return(None)`. |
| `OperDecl` named `main` | As above, *plus* the body is wrapped with `Inst::Call("coddl_runtime_init")` at the top and `Inst::Call("coddl_runtime_shutdown")` at the bottom. Synthetic externs for both are registered through the same `seen_externs` dedup that handles the builtin → extern map. ARCHITECTURE.md §6 mandates this; the runtime stubs are no-ops today but wiring it in lowering means future runtime growth (DB pool, prepared-statement cache) lands without a codegen change. |
| `Heading` / `Param` / `TypeRef` | Consumed into `Function::params`.                                                |
| `Block`        | Inlined into the surrounding `Function`'s sole `BasicBlock` today; multi-block control-flow lands when `if` / `match` / `while` do. Returns the tail expression's `ValueId` if `Block::tail_expr()` is `Some`; otherwise a fresh placeholder. |
| `Stmt::Let`    | Lowers the RHS expression and binds its `ValueId` in the current local scope. No `Inst` emitted — `let` is a binding, not a computation. |
| `Stmt::ExprStmt` | `lower_expr` is called and its result discarded.                                           |
| `Expr::Literal` | `Inst::Const` of the matching `ProcType`.                                                   |
| `Expr::Call`   | Lowers each declared parameter's argument expression in source-declaration order, emits the synthetic extern `Function` on first reference, then `Inst::Call` to its `linkage_name`. |
| `Expr::NameRef` | Looks up the name in the local scope stack (innermost-first). Returns the bound `ValueId` so downstream consumers thread it through. |
| `Expr::Transaction` | Pushes a local scope, walks the body via `Block`, pops the scope. The body's `ValueId` becomes the expression's value. Transparent today — no `Inst` for the `transaction` wrapper itself. Future runtime semantics slot in here as synthetic begin/commit/rollback calls. |

Locals share the same `ValueId` namespace as computed values —
there's no separate "variable" concept in ProcIR. A `let x = expr`
just records "the name `x` refers to whatever `ValueId` lowering
produced for `expr`."


## Builtin → extern map

Surface operator names compile to runtime extern symbols. The map
lives in `crates/coddl-procir/src/lower.rs::BUILTIN_EXTERNS` and is
the single source of truth for the lowering pass.

| Surface name | Linkage name        | Signature                          |
|--------------|---------------------|------------------------------------|
| `write_line` | `coddl_write_line`  | `(message: Text) -> Unit`          |

Every entry corresponds to a built-in the typechecker already knows
about (`crates/coddl-types/src/builtins.rs`). Adding a built-in is
two coordinated edits.


## `Codegen` trait

The seam between ProcIR and any backend. Lives in
`crates/coddl-procir/src/codegen.rs`.

```rust
pub trait Codegen {
    type Output;
    type Error: std::fmt::Display;
    fn emit(&mut self, module: &Module) -> Result<Self::Output, Self::Error>;
}
```

Both Phase 6 backends implement `Codegen`. `coddl-codegen-llvm` will
pick `Output = String` (LLVM IR text). `coddl-codegen-cranelift` will
pick `Output = Vec<u8>` (object bytes). The trait knows nothing about
either side.


## Lowering diagnostics

The lowering pass is currently *infallible* on a diagnostic-free
typecheck — every reachable case has a deterministic mapping, and
typechecker-impossible cases reach `unreachable!()`. No `L####`
codes exist today.

| Code  | Trigger |
|-------|---------|
| _(none yet)_ | |

The `L####` namespace is reserved. Codes appear here in the same
commit that emits them.

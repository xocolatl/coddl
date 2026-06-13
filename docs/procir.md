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
| `Module`       | `program_name: String`, `functions: Vec<Function>`, `headings: Vec<Heading>` |
| `HeadingId`    | `u32` index into `Module::headings`; rendered `heading_<n>` |
| `Function`     | `name: String`, `linkage_name: String`, `params: Vec<(String, ProcType)>`, `return_type: ProcType`, `blocks: Vec<BasicBlock>` |
| `BasicBlock`   | `id: BlockId`, `insts: Vec<Inst>`, `terminator: Terminator` |
| `BlockId`      | `u32`                                                   |
| `ValueId`      | `u32` — SSA value name, rendered `%n`                   |
| `Const`        | `Integer(i64)`, `Text(Vec<u8>)`, `Unit`                 |
| `ProcType`     | `Integer`, `Rational`, `Approximate`, `Text`, `Character`, `Binary`, `Byte`, `Boolean`, `Unit`, `Pointer`, `Tuple(Heading)`, `Relation(HeadingId)` |

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
  `ProcType::Tuple(Heading)` carries the typechecker's `Heading`
  directly; at ABI boundaries it flattens per-attribute in canonical
  heading order (nested tuples recursively). `ProcType::Relation`
  is a single pointer at the ABI level (the RC-managed payload),
  with the heading living in static data and reached via the
  per-module descriptor table. `Sequence` becomes a runtime handle
  (`Pointer`). Every built-in scalar gets a variant from day one so
  backends can pattern-match exhaustively. `ProcType` is `Clone`,
  not `Copy` — the `Tuple` variant is heap-backed.
- **`Module::headings` is the per-module heading interner.** The
  lowerer interns each unique `Heading` it touches into this
  vector; `ProcType::Relation(HeadingId)` and the four new
  relation-shaped instructions reference headings by their index.
  Each backend emits one static descriptor per entry — see
  `docs/codegen.md` for the C-struct layout the backends produce
  and `docs/runtime.md` for the runtime's view of the same data.


## Instruction table

| Instruction          | Operands                                                                  | Defines               | Semantics                                                                 |
|----------------------|---------------------------------------------------------------------------|-----------------------|---------------------------------------------------------------------------|
| `Const`              | `value: Const`, `ty: ProcType`                                            | `dst: ValueId`        | Materialize a compile-time constant of type `ty`.                         |
| `Call`               | `callee: String` (linkage name), `args: Vec<ValueId>`, `return_type`      | `dst: Option<ValueId>` | Call the named function. `dst` is `None` iff `return_type == Unit`.       |
| `TupleLit`           | `fields: Vec<(String, ValueId)>` (canonical-order), `heading: Heading`    | `dst: ValueId`        | Bundle the fields into a tuple value typed `Tuple(heading)`. No runtime op — the value is a compile-time grouping over the field SSA values; backends carry it as a `ValueRepr::Tuple` and flatten at ABI boundaries. |
| `TupleField`         | `src: ValueId` (tuple), `field_name: String`, `field_type: ProcType`      | `dst: ValueId`        | Project one attribute out of `src`. Pure compile-time projection in backends — `dst` binds the field's existing `ValueRepr`. |
| `RelationLit`        | `tuples: Vec<ValueId>` (each typed `Tuple(h)`), `heading_id: HeadingId`   | `dst: ValueId`        | Allocate a fresh RC-managed payload (rc=1), copy each tuple's flattened bytes into the canonical-layout record buffer at the right offsets, then call `coddl_relation_seal` (sort + adjacent-dedup). `dst` carries `ProcType::Relation(heading_id)`. |
| `Retain`             | `src: ValueId` (relation pointer)                                         | —                     | Increment the refcount of `src`. Backend lowers to `call coddl_rc_retain`. Emitted by the lowerer when a `let` RHS is a `NameRef` to an already-bound heap value. |
| `Release`            | `src: ValueId` (relation pointer)                                         | —                     | Decrement the refcount of `src`; the drop walker runs on the runtime side when the count reaches zero. Backend lowers to `call coddl_rc_release`. Emitted at scope-exit for every heap-typed local. |
| `WriteRelation`      | `rel: ValueId`, `heading_id: HeadingId`                                    | —                     | Print the relation in canonical-heading order (one tuple per line). Backend lowers to `call coddl_write_relation(rel, &@.heading.<id>)`. The polymorphic `write_relation` surface builtin lowers to this instead of a generic `Inst::Call` so backends don't need to special-case the per-call descriptor lookup. |

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
| `Expr::TupleLit` | Lowers each field's value expression, sorts the `(name, ValueId, ProcType)` triples into canonical (name-sorted) heading order, then emits `Inst::TupleLit { fields, heading }`. The heading is built from the per-field static types — which the typechecker already enforces. Empty `{}` lowers to `Inst::TupleLit` with empty fields + empty heading. |
| `Expr::FieldAccess` | Lowers the base expression, asserts its `ProcType` is `Tuple(H)` (a typechecker invariant — `T0016` blocks non-tuple bases), looks up the field's `Type` in `H`, converts to `ProcType` via the same scalar/tuple recursion the lowerer uses for parameters, then emits `Inst::TupleField`. |
| `Expr::RelationLit` | Lowers each nested `TupleLit`, interns the first tuple's `Heading` into `Module::headings` (getting a `HeadingId`), then emits `Inst::RelationLit { dst, tuples, heading_id }`. `dst` is recorded as `ProcType::Relation(heading_id)` so downstream uses (field reads, write_relation calls, scope-exit releases) can route through `value_types`. |
| Surface `write_relation { rel: r }` | Special-cased in `lower_call`. The `rel` argument is lowered the usual way; its tracked `ProcType::Relation(id)` gives the heading id directly. The lowerer emits `Inst::WriteRelation { rel, heading_id }` rather than going through the generic `Inst::Call` path. |
| RC discipline | The lowerer emits `Inst::Retain` when a `let` RHS is a `NameRef` resolving to an existing heap-typed binding (so both bindings hold a count). At scope-exit (transaction exit, function epilogue) it emits `Inst::Release` for every heap-typed local. Fresh `Inst::RelationLit` results start at rc=1 and don't need a retain on their first bind. |

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

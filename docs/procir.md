# ProcIR — procedural SSA IR

ProcIR is Coddl's procedural intermediate representation: SSA blocks with typed values, ready for LLVM (today) or Cranelift / wasm-encoder (planned — see [codegen.md](codegen.md)).

This doc is the authoritative spec for what `coddl-procir` produces: data types, every instruction, the AST→ProcIR correspondences, the `Codegen` trait the backends implement, and the (currently empty) `L####` diagnostic-code table.

## ProcIR has no relational algebra

`Relation H` is an opaque type carried in SSA values, and everything you can *do* with one is a call into the [runtime](runtime.md) library. The relation-shaped instructions in the table below — `Where`, `Extract`, `RelvarRead`, `RelvarSlotInit`, `RelvarSlotRelease`, `RelationLit`, `WriteRelation`, `Query`, `RegisterDatabase`, `RegisterPlan` — are **named call sites for runtime ABI entry points**, not algebra primitives ProcIR reasons about. They get dedicated opcodes for readability, type-checking, and verifier convenience, but semantically they're calls.

This matters because:

- The algebra lives in [RelIR](relir.md) (Algebra A core + sugar). ProcIR is the procedural target after the SQL/in-process cut is drawn.
- A SQL-rooted RelIR subtree compiles to one ProcIR call: `query(plan_id, params)` with a [`coddl-sqlemit`](sqlemit.md)-baked SQL string.
- An in-process RelIR subtree compiles (via `coddl-execlocal`) to a sequence of ProcIR calls into the runtime library — `coddl_relation_where`, `coddl_relation_join`, `coddl_relation_project`, etc. `coddl-execlocal` emits these as generic call instructions; the named opcodes here are essentially sugar that happens to predate `coddl-execlocal` landing.
- Adding a new relational primitive doesn't require a new ProcIR opcode. Add a runtime function + a RelIR node + an execlocal lowering rule; ProcIR sees another call.

ProcIR's surface area grows when the *procedural* language grows new needs (closures, exceptions, async), not when the relational algebra grows.

## What ProcIR needs that's relation-adjacent (but not algebra)

Two things give ProcIR enough vocabulary to *talk about* relations without reasoning about them:

1. **A relation type** (`ProcType::Relation(heading_id)`) — so SSA values can be typed as relations and the verifier can check that you're passing a relation handle to a runtime function that expects one. This is *representation*, not algebra.
2. **A per-module heading-descriptor table** — static data the runtime needs to interpret a relation payload (record size, attribute offsets, types). Each backend emits one descriptor per `Module::headings` entry; see [codegen.md](codegen.md) for the C-struct layout the backends produce and [runtime.md](runtime.md) for the runtime's view of the same data.

## Backend-agnostic by design

ProcIR is shaped for SSA codegen in general, not LLVM specifically — a long-term-planning concession (see [principles.md](principles.md)) that costs little now and preserves room to add backends without rewriting the IR. The IR carries no LLVM-specific intrinsic names, metadata, or calling conventions at the node level; per-backend specifics live in the codegen crate.

- **LLVM IR text (v1).** Emit text, shell out to `llc`/`clang`. The same emitter covers native targets (x86-64, aarch64) *and* `wasm32-*` via the target triple — WASM-via-LLVM is essentially free at the codegen layer. See [codegen.md](codegen.md).
- **Cranelift (planned).** Both IRs are SSA with the same value-model surface; the lowering is largely a different printer over the same ProcIR walk. Use cases: REPL JIT for fast query iteration, and toolchain-free AOT for deployments that don't want `clang` in the image.
- **Direct WASM via `wasm-encoder` (optional).** Worth keeping the door open for browser/wasmtime targets that don't want LLVM at all in the build. Lower priority than Cranelift; revisit when the use case lands.

Runtime portability is the harder half — see [runtime.md](runtime.md) and [workspace.md](workspace.md) (Cargo features) for how the SQL backends get gated out of `wasm32-*` builds.

**Last sync:** `1830ac1`. Every commit that adds, removes, or changes
a ProcIR data type, an instruction, an AST→IR correspondence, the
`Codegen` trait, the builtin→extern map, or an `L####` code updates
this file in the same commit; `tools/check-grammar.sh` enforces the
last of those from the hygiene gate.


## Module overview

A ProcIR `Module` is one compilation unit. Field-by-field:

| Type           | Fields                                                  |
|----------------|---------------------------------------------------------|
| `Module`       | `program_name: String`, `functions: Vec<Function>`, `headings: Vec<Heading>`, `public_relvars: Vec<PublicRelvarBinding>`, `db_path_default: Option<String>`, `db_name: Option<String>`, `plans: Vec<PlanEntry>` |
| `PublicRelvarBinding` | `name: String`, `heading_id: HeadingId`, `table_name: String`, `columns: Vec<(String, String)>` — one entry per public relvar surfaced by the plan layer. Empty when the program declares no public relvars. |
| `PlanEntry`    | `plan_id: u32`, `db_name: String`, `sql: String`, `param_count: u32`, `result_heading_id: HeadingId` — one baked SQL plan the optimizer pushed to the backend. `plan_id` is a dense per-module id (its own namespace — *not* the storage layer's `coddl_sqlemit::PlanId` text hash). Empty when nothing was pushed. |
| `HeadingId`    | `u32` index into `Module::headings`; rendered `heading_<n>` |
| `Function`     | `name: String`, `linkage_name: String`, `params: Vec<(String, ProcType)>`, `return_type: ProcType`, `blocks: Vec<BasicBlock>` |
| `BasicBlock`   | `id: BlockId`, `params: Vec<(ValueId, ProcType)>`, `insts: Vec<Inst>`, `terminator: Terminator` — `params` are SSA values bound on block entry (the phi / block-parameter join). Empty for the entry block and the arms of an `if`; an `if` merge block carries one parameter (the join value) unless it is Unit; a counted-`for` **loop header** carries the counter, fed from two predecessors — the entry edge and the back-edge (see `Stmt::For`). |
| `BlockId`      | `u32`                                                   |
| `ValueId`      | `u32` — SSA value name, rendered `%n`                   |
| `Const`        | `Integer(i64)`, `Text(Vec<u8>)`, `Boolean(bool)`, `Unit` |
| `ProcType`     | `Integer`, `Rational`, `Approximate`, `Text`, `Character`, `Binary`, `Byte`, `Boolean`, `Unit`, `Pointer`, `Tuple(Heading)`, `Relation(HeadingId)` |

Key invariants the lowering pass and the backends both rely on:

- **Externs carry no blocks.** `Function::blocks.is_empty()` ⇔ the
  function is an extern declaration. Hello-world produces one such
  Function for `write_line` and one with blocks for `main`.
- **`linkage_name` is the source of truth for the symbol.** For a
  defined function in the entry unit (`program`/`library`), `linkage_name
  == name`. For an operator lowered from an **imported module**, it is
  **module-scoped**: `<prefix>$<name>`, where `<prefix>` is the module
  path joined with `$` (`greet$hello`, `a$b$helper`) — `$` is invalid in
  a UAX-#31 identifier, so a mangled symbol can never collide with a
  user-written one, letting two modules define a same-named private
  helper. For externs, `linkage_name` is the `coddl_*` name the runtime
  exposes. Backends emit `linkage_name` verbatim and never derive it.
- **SSA: every `Inst` defines at most one `ValueId`.** `Inst::Call`
  whose `return_type` is `Unit` has `dst == None`.
- **`ProcType` is the machine-level type, not the surface type.**
  `ProcType::Tuple(Heading)` carries the typechecker's `Heading`
  directly. A tuple's representation is a **size threshold** on the
  heading (`layout::tuple_is_boxed`, record width ≥ `TUPLE_BOX_THRESHOLD`):
  a **small** tuple flattens per-attribute in canonical heading order at
  ABI boundaries (nested tuples recursively — zero heap, free field
  access); a **large** tuple is **boxed** — one pointer to a
  `length = 1` RC record (built by `TupleBox`, read by `AttrLoad` /
  `TupleUnbox`), just like a relation. Independently, *every* non-empty
  tuple **return** crosses the ABI as one pointer: a small tuple is boxed
  at the return site and unboxed at the call. `ProcType::Relation`
  is a single pointer at the ABI level (the RC-managed payload),
  with the heading living in static data and reached via the
  per-module descriptor table. `Sequence` becomes a runtime handle
  (`Pointer`). Every built-in scalar gets a variant from day one so
  backends can pattern-match exhaustively. `ProcType` is `Clone`,
  not `Copy` — the `Tuple` variant is heap-backed.
- **`Module::headings` is the per-module heading interner.** The
  lowerer interns each unique `Heading` it touches into this
  vector; `ProcType::Relation(HeadingId)` and the
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
| `TupleField`         | `src: ValueId` (tuple), `field_name: String`, `field_type: ProcType`      | `dst: ValueId`        | Project one attribute out of a **flattened** (small) tuple `src`. Pure compile-time projection in backends — `dst` binds the field's existing `ValueRepr`. (A field of a **boxed** tuple reads via `AttrLoad` instead, with a retain on a heap field.) |
| `TupleBox`           | `src: ValueId` (flattened tuple), `heading_id: HeadingId`                 | `dst: ValueId`        | Materialize a flattened tuple into a heap record — like a one-tuple `RelationLit` minus the seal: `coddl_rc_alloc(record_size, 1, kind=Relation, desc)` + per-attribute stores (retain-on-store for Text/relation cells). `dst` is the record pointer typed `Tuple(heading)`. Emitted for a large-tuple literal and to box a small tuple at a return site. |
| `TupleUnbox`         | `src: ValueId` (boxed tuple), `heading_id: HeadingId`                     | `dst: ValueId`        | Read a boxed tuple's record back into a flattened `ValueRepr::Tuple` (per-attribute reads; no cardinality check — a box is one record). The inverse of `TupleBox`. Emitted at a small-tuple call result; the box is deferred-released after (its cells are borrowed into `dst`). |
| `RelationLit`        | `tuples: Vec<ValueId>` (each typed `Tuple(h)`), `heading_id: HeadingId`   | `dst: ValueId`        | Allocate a fresh RC-managed payload (rc=1), copy each tuple's flattened bytes into the canonical-layout record buffer at the right offsets, then call `coddl_relation_seal` (sort + adjacent-dedup). `dst` carries `ProcType::Relation(heading_id)`. |
| `Retain`             | `src: ValueId` (relation pointer)                                         | —                     | Increment the refcount of `src`. Backend lowers to `call coddl_rc_retain`. Emitted by the lowerer when a `let` RHS is a `NameRef` to an already-bound heap value. |
| `Release`            | `src: ValueId` (relation pointer)                                         | —                     | Decrement the refcount of `src`; the drop walker runs on the runtime side when the count reaches zero. Backend lowers to `call coddl_rc_release`. Emitted at scope-exit for every heap-typed local. |
| `WriteRelation`      | `rel: ValueId`, `heading_id: HeadingId`                                    | —                     | Print the relation in canonical-heading order (one tuple per line). Backend lowers to `call coddl_write_relation(rel, &@.heading.<id>)`. The polymorphic `write_relation` surface builtin lowers to this instead of a generic `Inst::Call` so backends don't need to special-case the per-call descriptor lookup. |
| `ScalarOp`           | `op: ScalarOp`, `operand_type: ProcType`, `lhs: ValueId`, `rhs: ValueId`  | `dst: ValueId`        | Scalar comparison or Boolean op. `ScalarOp` is `Eq` / `NotEq` / `Lt` / `Gt` / `LtEq` / `GtEq` / `And` / `Or`. Result is always `Boolean`. Backends lower to native `icmp` / `and` / `or`. |
| `AttrLoad`           | `src: ValueId` (record pointer), `offset: u32`, `attr_type: ProcType`     | `dst: ValueId`        | Read one attribute from a record pointer at the given byte offset. Used inside predicate helper functions. Backends emit `getelementptr i8 + load`. Width inferred from `attr_type`. |
| `Where`              | `src: ValueId` (relation), `predicate_linkage: String`, `heading_id: HeadingId` | `dst: ValueId`   | Restrict `src` by the named predicate. Backends emit `call coddl_relation_where(src, &descriptor, &predicate_fn)`. Result is a fresh `Relation` (rc=1). |
| `Extract`            | `src: ValueId` (relation), `heading_id: HeadingId`                          | `dst: ValueId`        | TTM RM Pre 10: collapse a single-row relation to a tuple. Backend emits `call coddl_extract_check_cardinality(src, &descriptor)` (aborts on length ≠ 1), then reads each attribute from the returned record pointer into per-field scalars, bundled as a `ValueRepr::Tuple`. `dst` carries `ProcType::Tuple(heading)`. |
| `Load`               | `src: ValueId` (relation), `heading_id: HeadingId`, `keys: Vec<u32>`       | `dst: ValueId`        | RM Pro 7 iteration gate (surface `load … from … order [ … ]`). Backend bakes `keys` as a read-only `u32` array (like `Rename`'s `perm`) and emits `call coddl_load_ordered(src, &descriptor, keys, key_count)`, which sorts `src`'s records by the order keys into a fresh `Sequence` reusing the source layout (each element record is a source tuple). Each `keys` entry is an index into the source heading's canonical attrs, bit 31 set for a descending key (empty = no `order`). `dst` carries `ProcType::Sequence(Tuple(H))`. |
| `Collect`            | `src: ValueId` (sequence), `heading_id: HeadingId`                         | `dst: ValueId`        | Reverse `load` (surface `load <private-relvar> from <sequence>`), the inverse of `Load`. Backend emits `call coddl_relation_from_sequence(src, &descriptor)`, which copies the sequence's element tuples, retains their `Text` cells, and **seals** (sort + dedup, RM Pro 1, 3). `heading_id` is the element-tuple heading; `dst` carries `ProcType::Relation(H)`, then stored into the target relvar's slot (`RelvarSlotStore`). |
| `RelvarSlotInit`     | `name: String`, `heading_id: HeadingId`                                    | —                     | Materialize one public relvar from SQLite into its slot global. Backend emits `call coddl_sqlite_relvar_init(...)` with the static (name, env-resolved path, table, columns, descriptor, slot) bundle. Emitted once per public relvar in `main`'s prologue, after `coddl_runtime_init`. |
| `RelvarSlotRelease`  | `name: String`                                                             | —                     | Release the RC pointer in the named relvar's slot. Backend emits `load ptr from @<name>_slot + call coddl_rc_release`. Emitted once per public relvar in `main`'s epilogue, before `coddl_runtime_shutdown`. |
| `RelvarRead`         | `name: String`, `heading_id: HeadingId`                                    | `dst: ValueId`        | Load the public relvar's RC payload from its slot + retain (so the consumer holds its own count). The lowerer's temp-source release logic frees the temporary when not bound to a local — same pattern Phase 21 installed for `extract` operands, generalized to chains like `RelvarRead → where → extract`. |
| `RegisterDatabase`   | —                                                                          | —                     | Bind the logical database to its env-resolved connection path so `Query` can reach it. Backend resolves `CODDL_<DB>_FILE` (env override → baked default) via `coddl_resolve_op_field`, then calls `coddl_register_database(name, path)` reading `Module::db_name` / `db_path_default`. Emitted once in `main`'s prologue when the module pushed any plan. |
| `RegisterPlan`       | `plan_id: u32`                                                             | —                     | Register one baked plan (looked up by id in `Module::plans`). Backend calls `coddl_register_plan(plan_id, db_name, sql, param_count, &@.heading.<result>)`. Emitted once per `Module::plans` entry in `main`'s prologue, after `RegisterDatabase`. |
| `Query`              | `plan_id: u32`, `params: Vec<(ValueId, ProcType)>`, `heading_id: HeadingId` | `dst: ValueId`       | Execute a registered plan, fire-on-call (the lazy force point), and bind the returned sealed `Relation` (rc=1) to `dst`. Backend builds a `CoddlParam` array from the bound params (each param's `ProcType` selects the kind tag + value/ptr field) and calls `coddl_query(plan_id, params, n)`. `dst` carries `ProcType::Relation(heading_id)` — the plan's result heading. |
| `Dml`                | `plan_id: u32`, `params: Vec<(ValueId, ProcType)>`                          | —                     | Execute a registered **DML** plan (`DELETE`/`INSERT`/`UPDATE`) for effect only — no result bound. Same `CoddlParam` marshaling as `Query`, but the backend calls `coddl_exec(plan_id, params, n)` (which runs `execute`, not `query`) and discards the returned status. Fires inside the enclosing `transaction [...]`'s begin/commit pair. |
| `InsertFrom`         | `plan_id: u32`, `src: ValueId`, `heading_id: HeadingId`                     | —                     | Insert an **in-memory** relation `src`'s rows into a public relvar via a registered insert *template* (an `INSERT … FROM (VALUES <marker>) … WHERE NOT EXISTS (…)`). Backend passes `src`'s relation pointer + its static heading descriptor (like `WriteRelation`) plus `plan_id` to `coddl_exec_insert`, which iterates the relation and expands the template's `VALUES` marker to batched `(?,…)` groups. For `t := t union <literal-or-private>`; fires inside the enclosing transaction. |

## Terminator table

| Terminator    | Operand                | Semantics                                                                |
|---------------|------------------------|--------------------------------------------------------------------------|
| `Return`      | `Option<ValueId>`      | Return from the function. `None` returns `Unit`.                         |
| `CondBr`      | `cond: ValueId`, `then_block: BlockId`, `else_block: BlockId` | Two-way branch on a `Boolean`. Neither target takes arguments; join values flow through the merge block's parameters via the arms' `Br`. LLVM `br i1`; Cranelift `brif`. |
| `Br`          | `target: BlockId`, `args: Vec<ValueId>` | Unconditional branch, passing `args` as `target`'s block parameters (the SSA join). LLVM realizes them as `phi` at the top of `target`; Cranelift as `target`'s block params. `target` may **precede** the branch — a back-edge (counted `for`); an `arg` may then be defined in a later block, which is legal (a `phi` operand is bound on its edge, not textually). |
| `Unreachable` | —                      | Reserved for paths the typechecker has ruled out. Not emitted by hello-world. |


## AST → ProcIR correspondences

The lowering walk in `coddl-procir::lower` mirrors the typechecker's
walk shape. Each `check_<x>` in `coddl-types::checker` has a sibling
`lower_<x>` here.

| AST node       | ProcIR shape                                                                                  |
|----------------|----------------------------------------------------------------------------------------------|
| `Root`         | `Module { program_name, functions }`. Iterates items in source order.                        |
| `ProgramDecl`  | Sets `Module::program_name`. No instruction emitted.                                         |
| `OperDecl`     | One `Function` with one `BasicBlock` (`block_0`) for a straight-line body; an early `return` or `if` adds more. `Function::return_type` reflects the declared `-> <type>` clause (default `ProcType::Unit`). Heading params become `Function::params` typed via `ProcType`. Non-`main` opers with a non-Unit declared return capture the body's tail-expression `ValueId` and emit `Terminator::Return(Some(v))` at the fall-through epilogue; otherwise the terminator is `Return(None)`. A function may have **multiple return points**: each early `return` seals its own block with a `Return` (both backends accept several `ret`s). When the body diverges on *every* path (each ends in a `return`), the fall-through epilogue is skipped — its block would be dead and doubly-terminated. |
| `OperDecl` named `main` | As above, *plus* the body is wrapped with `Inst::Call("coddl_runtime_init")` at the top and `Inst::Call("coddl_runtime_shutdown")` at the bottom. Synthetic externs for both are registered through the same `seen_externs` dedup that handles the builtin → extern map. The runtime contract (see [runtime.md](runtime.md)) mandates this; the runtime stubs are no-ops today but wiring it in lowering means future runtime growth (DB pool, prepared-statement cache) lands without a codegen change. The prologue registration is finalized *after* the body is lowered (so it's known which relvars were pushed to SQL): right after `coddl_runtime_init` the lowerer injects `Inst::RegisterDatabase` + one `Inst::RegisterPlan { plan_id }` per `Module::plans` entry, then one `Inst::RelvarSlotInit { name, heading_id }` for each relvar still read in-process (with a matching `Inst::RelvarSlotRelease { name }` before `coddl_runtime_shutdown`). A fully-pushed relvar gets no slot init/release — there is no startup materialization for it. |
| `Heading` / `Param` / `TypeRef` | Consumed into `Function::params`.                                                |
| `Block`        | Lowered into the current block (no new `BasicBlock` of its own). A straight-line body is one `BasicBlock`; `if` introduces more (see below). Returns the tail expression's `ValueId` if `Block::tail_expr()` is `Some`; otherwise a fresh placeholder. An early `return` seals the current block and sets a `diverged` flag; the remaining statements and the tail are dead code, so lowering stops and returns a placeholder. |
| `Stmt::Return` | `return [<expr>];` — an early return. Lowers the value (if any), boxes it to the return ABI (same `box_return_value_if_needed` the epilogue uses) and retains it if it escapes a local, then **unwinds**: releases every heap local across *all* active scopes (the arm/transaction scope it sits in, every enclosing scope, and the function scope — not just the top scope a normal scope-exit frees) followed by every pending deferred `extract`-source release, and seals the block with `Terminator::Return`. Typecheck (T0093) forbids a `return` inside a `transaction [...]`, so no open transaction needs committing. A `return`-terminated block/arm has bottom type `Never` (see [typecheck.md](typecheck.md)). |
| `IfExpr`       | `if <cond> then [ … ] else [ … ]`. The condition lowers into the current block, sealed with `Terminator::CondBr`; each arm is its own block that ends in a `Terminator::Br` to a shared merge block, passing the arm's value as the merge block's parameter (the SSA join). Without `else`, the false edge branches straight to the merge and the value is Unit. A Unit result carries no merge parameter. Outer `var`s reassigned in either arm ride the merge as **extra** block parameters (the same SSA join, one per var): a value-typed or **owned `Text`** carry lowers — the `Text` merge param is marked owned, so a non-reassigning (or missing-`else`) edge forwards the pre-`if` value and the merged value is freed once at scope exit, RC-balanced per path (the `var body := ""; if … then [ body := format{…} ]` web-handler idiom, the `if`-sibling of the loop-carried-`Text` lift); a relation/sequence/boxed-tuple carry is still deferred (T0076). Blocks are sealed in the order entry → then → else → merge, so the entry stays first and every predecessor precedes the block it branches to — the ordering both codegen backends rely on (LLVM `phi` predecessor labels; Cranelift block sealing). An arm that **diverges** (ends in a `return`) emits no `Br` and is dropped from the merge's predecessors: the merge value/params come from the live arm(s), and with exactly one live arm the merge has a single predecessor (valid SSA); if *both* arms diverge the merge is unreachable and the whole `if` is `Never`. `match` reuses this multi-block machinery; the counted `for` extends it with a **back-edge** (see `Stmt::For`). |
| `Stmt::For`    | `for i := lo to hi do [ … ]` — the counted loop and the project's first **cyclic** CFG. Both bounds lower into the entry block (evaluated **once**), which branches to a **header** block carrying the counter `i` (the SSA join, fed `lo` on the entry edge and `i + 1` on the back-edge) plus one block parameter per outer `var` reassigned in the body — the accumulator join. A value-typed carry threads with no refcount work; an **owned `Text`** carry lowers RC-correctly (its header param is marked owned, so the reassignment releases the previous value each iteration and the final value is released once at scope exit — the web-handler body-building idiom, `var body := ""; for r in rows do [ body := body || … ]`); a relation/sequence carry is still deferred (T0076). The header tests `i <= hi` (inclusive `to`) with a `Terminator::CondBr` to the body or the exit; the body lowers with `i` bound as a loop-scoped `Integer` local, then branches **back** to the header with `i + 1` — a `Terminator::Br` whose target *precedes* it (the first back-edge). `lo > hi` fails the first test → zero iterations. Blocks are pushed entry → header → body → exit, so the header precedes its back-edge source. LLVM realizes the header parameter as a `phi` with an incoming value defined in a later block (legal); Cranelift seals every block *after* emission (`seal_all_blocks`), since the header's back-edge predecessor is emitted after the header. **The element form `for name in seq` desugars onto this same CFG in the lowerer** — no new terminator: the sequence is lowered once into an outer scope (released after the loop), the header runs `0 to cardinality(seq) - 1` (a `coddl_rc_length` call minus one), and the body binds `name = seq[__i]` via the shared `s[i]` index path before the user block. |
| `Stmt::While`  | `while <cond> do [ … ]` — the **pre-test** loop; the counted-`for` CFG minus the counter/increment, with the user condition re-evaluated in the header each iteration. Entry branches to a **header** carrying one block parameter per outer `var` reassigned in the body (the SSA join, fed the pre-loop value on the entry edge and the end-of-iteration value on the back-edge; a value-typed or **owned `Text`** carry lowers, the latter marked owned for RC-correct per-iteration release, while a relation/sequence carry is deferred, T0076). The header rebinds those vars to its params, lowers the condition, and `Terminator::CondBr`s to the body or the exit; the body branches **back** to the header carrying each carried var's current value. The condition is tested first → empty-safe. The exit binds each carried var to its header parameter (which dominates the sole exit edge). No new terminator or codegen — the back-edge machinery is `Stmt::For`'s. |
| `Stmt::DoWhile` | `do [ … ] while <cond>` — the **post-test** loop. The **body** is the loop header (entered from the entry edge *and* the back-edge), so it carries the block parameters; the condition is evaluated after the body, and a tiny **latch** block supplies the back-edge args because a `CondBr` carries none. Shape: entry `Br body [carried…]` → body (params; rebind, run body, evaluate condition, `CondBr` to latch or exit) → latch (`Br body [carried_now…]`, the back-edge) → exit (binds each carried var to its end-of-iteration value, which the `CondBr` block dominates). The body runs once before the first test — the documented once-before-the-test caveat (an empty-sequence index hazard). Same back-edge machinery as `Stmt::For`; no codegen change. |
| `Stmt::Load`   | `load <target> from <relExpr> [ order [ <sort-item>… ] ];` — the RM Pro 7 iteration gate. **Direction is chosen from the lowered source's type.** A `Relation` source is the **forward** form: forces the source to a runtime pointer, builds `keys` (each order-key attr's index into the source's canonical heading, bit 31 for `desc`), emits `Inst::Load`, and binds the result `Sequence` to the pre-declared `var` target via `bind_pending_first_assign` (its first assignment; scope exit releases the heap `Sequence`). `coddl_load_ordered` fully copies + retains the source's cells, so a *temporary* source is released right after (unlike `extract`, which borrows into it). A `Sequence` source is the **reverse** form (`load <private-relvar> from <sequence>`, no `order`): emits `Inst::Collect` (seal the sequence's element tuples into a relation set) and stores the result into the private relvar's slot (`RelvarSlotStore`). **Tuple-element read:** `seq[i]` / `for … in` over a `Sequence Tuple H` (via `lower_seq_index_value`) explodes the element record into per-attribute `AttrLoad`s bundled as a `ValueRepr::Tuple` — the same shape `Extract` produces, so `.attr` stays a compile-time `TupleField`. The cells are borrows into the sequence (which outlives every use), so they are not retained. |
| `Stmt::Let`    | Lowers the RHS expression and binds its `ValueId` in the current local scope. No `Inst` emitted — `let` is a binding, not a computation. |
| `Stmt::ExprStmt` | `lower_expr` is called and its result discarded.                                           |
| `Expr::Literal` | `Inst::Const` of the matching `ProcType`.                                                   |
| `Expr::Call`   | A builtin callee lowers each declared parameter's argument expression in source-declaration order, emits the synthetic extern `Function` on first reference, then `Inst::Call` to its `linkage_name`. A **user-defined** callee (any non-builtin name; resolved against the current unit's own-operator table a pre-pass fills from every `oper` declaration, then its imported-operator table for a `use module` import) lowers args the same name-driven way and emits an in-module `Inst::Call` whose `callee` is the resolved operator's **linkage name** — verbatim for an own operator of the entry unit, or the exporting module's `<prefix>$<name>` for an imported call — no extern, since the callee is a `Module::functions` entry. A unit's own operator shadows a same-named import. A `Text`-returning user call marks its result owned (released at the caller's scope exit), the same as `read_line`. |
| `Expr::NameRef` | Looks up the name in the local scope stack (innermost-first). Returns the bound `ValueId` so downstream consumers thread it through. |
| `Expr::Transaction` | Pushes a local scope, emits `Inst::Call("coddl_begin_tx")`, walks the body via `Block`, emits `Inst::Call("coddl_commit_tx")`, releases heap-typed locals, pops the scope. The body's `ValueId` becomes the expression's value (so a `let g = transaction [...]` binds the tail). Tx-externs are no-ops in v1: nothing inside the body touches SQLite (reads are served from the pre-materialized slot), so begin/commit have no work to do — the shape exists so the conformance rule (T0025 / T0026) has somewhere to land. Real BEGIN/COMMIT discipline ships with write-through. |
| SQL pushdown cut | Before legacy lowering, `lower_expr` tries `try_lower_pushed`: it builds a `coddl-relir` `RelExpr` from a relvar-rooted relational subtree (a public-relvar leaf, optionally `where attr = literal`), runs the cut (`cut::try_push`), and on a hit bakes the SQL via `coddl-sqlemit`, records a `PlanEntry` on `Module::plans` (deduped by the text-stable `coddl_sqlemit::PlanId`), emits one `Inst::Const` per bind value, and emits `Inst::Query { dst, plan_id, params, heading_id }` at the force point — replacing the legacy `RelvarRead`/`Where`. Only enabled for a pushable backend (SQLite today); a non-pushable shape or backend falls through to the legacy path below. |
| `Expr::NameRef` (public relvar) | When the cut did *not* push the read (legacy path), and the plan supplies a public relvar whose surface name matches the NameRef, the lowerer emits `Inst::RelvarRead { dst, name, heading_id }` (a slot load + retain), records `dst`'s `ProcType::Relation(heading_id)`, and marks the relvar as in-process-used so `main` materializes its slot. The typechecker has already enforced this only happens inside a `transaction [...]` (T0025). Consumers (`where` / `extract` / `write_relation`) release the temporary via the existing fresh-source detection. |
| `Expr::TupleLit` | Lowers each field's value expression, sorts the `(name, ValueId, ProcType)` triples into canonical (name-sorted) heading order, then emits `Inst::TupleLit { fields, heading }`. The heading is built from the per-field static types — which the typechecker already enforces. Empty `{}` lowers to `Inst::TupleLit` with empty fields + empty heading. |
| `Expr::FieldAccess` | Lowers the base expression, asserts its `ProcType` is `Tuple(H)` (a typechecker invariant — `T0016` blocks non-tuple bases), looks up the field's `Type` in `H`, converts to `ProcType` via the same scalar/tuple recursion the lowerer uses for parameters, then emits `Inst::TupleField`. |
| `Expr::RelationLit` | Lowers each nested `TupleLit`, interns the first tuple's `Heading` into `Module::headings` (getting a `HeadingId`), then emits `Inst::RelationLit { dst, tuples, heading_id }`. `dst` is recorded as `ProcType::Relation(heading_id)` so downstream uses (field reads, write_relation calls, scope-exit releases) can route through `value_types`. |
| Surface `write_relation { rel: r }` | Special-cased in `lower_call`. The `rel` argument is lowered the usual way; its tracked `ProcType::Relation(id)` gives the heading id directly. The lowerer emits `Inst::WriteRelation { rel, heading_id }` rather than going through the generic `Inst::Call` path. |
| RC discipline | The lowerer emits `Inst::Retain` when a `let` RHS is a `NameRef` resolving to an existing heap-typed binding (so both bindings hold a count). At scope-exit (transaction exit, function epilogue) it emits `Inst::Release` for every heap-typed local. Fresh `Inst::RelationLit` results start at rc=1 and don't need a retain on their first bind. |
| `Expr::BoolLit` | `Inst::Const { value: Const::Boolean(b), ty: Boolean }`. |
| `Expr::Binary` (scalar) | Lowers lhs and rhs, then emits `Inst::ScalarOp { dst, op, operand_type, lhs, rhs }`. The `operand_type` is the lhs's tracked `ProcType` (which equals rhs's per typecheck). Result is `Boolean`. |
| `Expr::Binary` (Where) | Synthesizes a per-site predicate Function named `__coddl_where_<n>`. The function takes a single `record_ptr: Pointer` param; its body pre-emits `Inst::AttrLoad` for each heading attribute (binding each by name in a fresh scope), then lowers the user predicate against that scope, then returns the Boolean ValueId. The enclosing function emits `Inst::Where { dst, src, predicate_linkage, heading_id }`. Capture detection: the predicate lowerer holds a snapshot of the enclosing function's `locals` in `outer_locals_for_capture`; a NameRef miss in the predicate scope that hits the snapshot emits T0022 and the lowerer returns no module. |
| Parameters → ValueIds | The lowerer's convention: the first N fresh ValueIds in a function map 1:1 to the function's params in source-declared order. Backends seed their per-function value maps at entry — LLVM via direct `self.values` insertion against the param SSA name (`%record_ptr`); Cranelift via `builder.append_block_params_for_function_params` on the entry block. |
| `Expr::Unary` (Extract) | Lowers the operand (must be `ProcType::Relation(id)`), emits `Inst::Extract { dst, src, heading_id }`, and binds `dst`'s type to `ProcType::Tuple(heading)` (heading looked up via `Module::headings[id]`). If the source ValueId isn't bound to any local — i.e., it's a temporary (e.g., a freshly-allocated `R where p`) — the lowerer emits `Inst::Release { src }` immediately after the Extract, so the heap payload is freed once its scalar attributes have been copied out. Let-bound sources are left alone (the binding owns the rc; releasing here would double-free at the next use). |

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
| `read_line`  | `coddl_read_line`   | `(prompt: Text) -> Text`           |

Every entry corresponds to a built-in the typechecker already knows
about (`crates/coddl-types/src/builtins.rs`). Adding a built-in is
two coordinated edits.

ProcIR records each extern's *logical* signature — the clean
`(prompt: Text) -> Text` above. How a `Text` return crosses the C ABI
(it can't go back by value) is a codegen concern: each backend
synthesizes a trailing len-out pointer at the call site. See
`docs/codegen.md` "Fat-pointer returns".


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

Most lowering is *infallible* on a diagnostic-free typecheck — every
reachable case has a deterministic mapping, and typechecker-impossible
cases reach `unreachable!()`. The exceptions are checks that need
information only the lowering layer has (a relvar's `WritePolicy`, an RHS
`RelExpr`'s pushable shape, a `where`-predicate's captures). These reuse
the `T####` typecheck namespace (full descriptions in
[typecheck.md](typecheck.md)) rather than minting `L####` codes:

| Code  | Trigger |
|-------|---------|
| T0022 | a `where`-predicate captures an identifier from an outer scope (not yet supported) |
| T0049 | assignment to a public relvar has an RHS shape the backend cannot emit as surgical DML |
| T0050 | assignment target is a public relvar mapped to a non-writable view |

The `L####` namespace is reserved for lowering-specific codes; none
exist yet.

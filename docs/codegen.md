# Coddl codegen

This document is the authoritative spec for what the two backends —
`coddl-codegen-llvm` and `coddl-codegen-cranelift` — emit. It pins
the C ABI for each `ProcType`, the per-backend translation of each
`Inst` and `Terminator`, the surface → linkage convention for `main`,
and the artifact shape each backend produces.

For *why* the IR has the two-backend split (and the rationale for
LLVM IR text emission vs. `llvm-sys`), see
`ARCHITECTURE.md §1 "Host language"` and `§4 "The two IRs"`. For the
ProcIR shape both backends walk, see `docs/procir.md`.

**Last sync:** `e2dda44`. Every commit that adds, removes, or changes
a `ProcType` ABI mapping, an instruction translation, a backend error
variant, or the surface→linkage convention updates this file in the
same commit.


## C ABI for ProcTypes

Both backends use the same calling convention at the FFI boundary so
that runtime externs (declared in `coddl-runtime` with `#[no_mangle]
extern "C"`) are callable from either backend's output. `Text` and
`Binary` decompose into *two* operands at C-call sites — a pointer
and a length — even though ProcIR sees them as one logical `ValueId`.

| `ProcType`    | C type                | LLVM IR              | Cranelift type                        |
|---------------|-----------------------|----------------------|---------------------------------------|
| `Integer`     | `int64_t`             | `i64`                | `I64`                                 |
| `Rational`    | _(placeholder)_       | `i64`                | `I64`                                 |
| `Approximate` | `double`              | `double`             | `F64`                                 |
| `Text`        | `(const uint8_t*, size_t)` | `ptr, i64`     | `ptr, I64`                            |
| `Character`   | `uint32_t`            | `i32`                | `I32`                                 |
| `Binary`      | `(const uint8_t*, size_t)` | `ptr, i64`     | `ptr, I64`                            |
| `Byte`        | `uint8_t`             | `i8`                 | `I8`                                  |
| `Boolean`     | `_Bool`               | `i1` (LLVM) / `I8` (Cranelift) | `I8`                        |
| `Unit`        | _(no operand)_        | `void` (return only) | _(omitted from params and returns)_   |
| `Pointer`     | `void*`               | `ptr`                | pointer type from target_config       |

`Rational` and `Approximate` aren't exercised by hello-world; the
mappings are placeholders that compile-clean exhaustive matches in
both backends but aren't load-bearing yet.


## The `main` special case

C convention is `int main(void)`: the program's entry point returns
an `i32` exit code. ProcIR has no notion of process-level entry — its
`oper main` declares `() -> Unit` like any other operator. Both
backends special-case the function literally named `main`:

- The emitted `define` (LLVM) / `declare_function` (Cranelift)
  signature returns `i32` instead of `void`, regardless of the
  ProcIR `return_type`.
- The terminator emission for `Terminator::Return(None)` writes
  `ret i32 0` / `return_(&[iconst.i32(0)])` instead of `ret void`.

A user writing `oper main { x: Integer } []` is rejected by the
typechecker (`T0006`), so the backends never need to handle a
parameterized `main`.


## LLVM backend

`coddl-codegen-llvm::LlvmBackend` implements `Codegen<Output = String,
Error = LlvmEmitError>`. The walk produces clang-compatible LLVM IR
text using opaque pointers (`ptr`) throughout — works on LLVM 15+.
No target triple is written; `clang` picks the host triple.

**Module structure.** A `ModuleID = '<program_name>'` header line,
then every extern declaration in source order, then every defined
function. String-constant globals (`@.str.0`, `@.str.1`, …) are
accumulated during the walk and spliced into the output between the
extern declarations and the first `define` line.

**Per-value tracking.** Each ProcIR `ValueId` maps to a `ValueRepr`:

| Variant   | Fields                          | Use                                 |
|-----------|---------------------------------|-------------------------------------|
| `Scalar`  | `ty: String`, `op: String`      | A single LLVM operand with its type prefix. |
| `Text`    | `ptr_op: String`, `len_op: String` | Two operands for C-call expansion.       |

`Inst::Const { value: Text(bytes), ty: Text, dst }` emits a private
unnamed-address constant for the bytes and records `Text { ptr_op:
"@.str.N", len_op: "<literal length>" }` for `dst`. `Inst::Call`
expands each `Text`-typed argument into two operands at the LLVM
call site.

**Worked example.** Hello-world lowers to (the `coddl_runtime_init`
and `coddl_runtime_shutdown` calls are auto-injected by ProcIR
lowering around `main`'s body — see `docs/procir.md` — and flow
through the standard `Inst::Call` path; the backend has no special-
case for them):

```llvm
; ModuleID = 'hello_world'

declare void @coddl_write_line(ptr, i64)
declare i64 @coddl_runtime_init()
declare i64 @coddl_runtime_shutdown()

@.str.0 = private unnamed_addr constant [13 x i8] c"Hello, world!"

define i32 @main() {
block_0:
    %v0 = call i64 @coddl_runtime_init()
    call void @coddl_write_line(ptr @.str.0, i64 13)
    %v2 = call i64 @coddl_runtime_shutdown()
    ret i32 0
}
```


## Cranelift backend

`coddl-codegen-cranelift::CraneliftBackend` implements
`Codegen<Output = Vec<u8>, Error = CraneliftEmitError>`. The walk
uses `cranelift-native` for the host ISA, `cranelift-object` for the
object writer, and emits a complete native object file.

**Settings.** `is_pic = true` is set on the ISA flags — required for
Mach-O linkability (text relocations are rejected without it) and
good practice on every modern target.

**Symbol linkage.**

| Function kind     | `Linkage`         |
|-------------------|-------------------|
| Defined           | `Linkage::Export` |
| Extern declaration| `Linkage::Import` |
| String-constant data | `Linkage::Local` |

**Data section.** Each `Inst::Const { value: Text(bytes), ty: Text }`
declares a local `DataId` named `.str.N`, defines its bytes, imports
the symbol into the current function, materializes the pointer with
`builder.ins().symbol_value(pointer_type, sym_value)`, and the length
with `builder.ins().iconst(I64, bytes.len())`. The two values are
tracked as the ProcIR `dst`'s `ValueRepr::Text { ptr, len }`.

**Call sites.** The callee `FuncId` is looked up by linkage name in
a `HashMap<String, FuncId>` built during the declaration pass.
`module.declare_func_in_func` imports the callee into the current
function; `builder.ins().call(local_callee, &[ptr, len])` emits the
call. `Unit`-returning callees don't update the value map.

**Artifact shape.** A platform object file — Mach-O on macOS, ELF on
Linux. Exported symbol: `main`. Imported symbols: every distinct
extern referenced (today: `coddl_write_line`, `coddl_runtime_init`,
`coddl_runtime_shutdown` — the latter two are auto-injected by
ProcIR lowering around `main`'s body, see `docs/procir.md`; the
backend has no special-case for them). Read-only data section
contains every string literal in the module.


## End-to-end pipeline

Each backend has an `e2e.rs` integration test that exercises the
full path from source to running binary.

LLVM (`crates/coddl-codegen-llvm/tests/e2e.rs`):

```
1. lower hello-world to ProcIR
2. emit IR text via LlvmBackend
3. write text to <tmp>/hello.ll
4. ensure target/debug/libcoddl_runtime.a is built
5. clang <tmp>/hello.ll <runtime>.a -o <tmp>/hello
6. run <tmp>/hello, capture stdout
7. assert stdout == b"Hello, world!\n"
```

Cranelift (`crates/coddl-codegen-cranelift/tests/e2e.rs`) is the same
shape with `cc <tmp>/hello.o <runtime>.a -o <tmp>/hello`. Both tests
panic with a clear message if `clang` / `cc` is missing on PATH.

**Manual smoke (workspace root):**

```sh
cargo build -p coddl-runtime
cargo run -q -p coddl-driver -- emit-llvm examples/hello-world/hello-world.cd > /tmp/hello.ll
clang /tmp/hello.ll target/debug/libcoddl_runtime.a -o /tmp/hello_llvm
/tmp/hello_llvm                 # prints Hello, world!

cargo run -q -p coddl-driver -- emit-obj examples/hello-world/hello-world.cd -o /tmp/hello.o
cc /tmp/hello.o target/debug/libcoddl_runtime.a -o /tmp/hello_cranelift
/tmp/hello_cranelift            # also prints Hello, world!

diff <(/tmp/hello_llvm) <(/tmp/hello_cranelift)
# byte-identical
```


## Backend error types

Backend errors are *not* user-facing positioned diagnostics — they're
bug-in-compiler conditions reached when the ProcIR walk meets a case
the emitter doesn't yet cover. They have clear messages but no stable
codes; `tools/check-grammar.sh` does not check them.

`LlvmEmitError`:

| Variant            | Trigger                                                                |
|--------------------|------------------------------------------------------------------------|
| `UnsupportedInst`  | A `ProcType` or `Inst` variant the LLVM walk has no case for.          |

`CraneliftEmitError`:

| Variant            | Trigger                                                                |
|--------------------|------------------------------------------------------------------------|
| `IsaSetup`         | `cranelift_native::builder()` or ISA flag construction failed.         |
| `ModuleError`      | `cranelift_module::Module` or `cranelift_codegen` returned an error.   |
| `UnsupportedInst`  | A `ProcType` or `Inst` variant the Cranelift walk has no case for.     |

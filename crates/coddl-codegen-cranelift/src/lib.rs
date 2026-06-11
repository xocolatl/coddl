//! ProcIR → Cranelift codegen (planned).
//!
//! Both ProcIR and Cranelift IR are SSA with the same value-model surface,
//! so this is largely a different printer over the same ProcIR walk.
//! Use cases: REPL JIT for fast query iteration; toolchain-free AOT for
//! deployments that don't want `clang` in the image. See ARCHITECTURE.md §4.

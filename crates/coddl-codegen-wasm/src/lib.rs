//! ProcIR → direct WASM via `wasm-encoder` (optional).
//!
//! WASM-via-LLVM (the LLVM codegen with `wasm32-*` target triple) is the
//! default WASM path. This crate is for deployments that don't want LLVM
//! in the build at all — browser/wasmtime hosts. Revisit when concrete
//! demand surfaces. See `docs/procir.md` "Backend-agnostic by design"
//! and `docs/codegen.md`.

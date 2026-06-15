//! ProcIR → LLVM IR text emission.
//!
//! v1 codegen backend. We emit IR as text and shell out to `llc`/`clang`
//! rather than depending on `llvm-sys`/`inkwell` (version-coupling churn,
//! and we don't need programmatic IR introspection). The same emitter
//! covers native targets and `wasm32-*` via the target triple.
//! See `docs/codegen.md` and `docs/procir.md`.

pub mod emit;
pub mod error;

pub use emit::LlvmBackend;
pub use error::LlvmEmitError;

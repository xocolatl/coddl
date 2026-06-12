//! Backend error type.
//!
//! Like `LlvmEmitError`, these are bug-in-compiler conditions, not
//! user-facing diagnostics. The runtime errors split out the
//! Cranelift-specific failure modes (ISA setup, module bookkeeping)
//! since those carry meaningful diagnostic text the wrapped library
//! produces.

use std::fmt;

#[derive(Debug)]
pub enum CraneliftEmitError {
    /// `cranelift_native::builder()` or ISA flag construction failed.
    IsaSetup(String),
    /// `cranelift_module::Module` or `cranelift_codegen` reported an
    /// error during a declare/define call.
    ModuleError(String),
    /// Reached during the walk — shouldn't happen on a diagnostic-free
    /// `lower()`. Indicates the IR has a case the emitter doesn't yet
    /// cover.
    UnsupportedInst(String),
}

impl fmt::Display for CraneliftEmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CraneliftEmitError::IsaSetup(msg) => write!(f, "Cranelift ISA setup: {msg}"),
            CraneliftEmitError::ModuleError(msg) => write!(f, "Cranelift module: {msg}"),
            CraneliftEmitError::UnsupportedInst(msg) => {
                write!(f, "Cranelift emit: unsupported IR case: {msg}")
            }
        }
    }
}

impl std::error::Error for CraneliftEmitError {}

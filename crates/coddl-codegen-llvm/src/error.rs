//! Backend error type.
//!
//! These are *not* user-facing diagnostics — they're bug-in-compiler
//! conditions reached when the ProcIR walk hits a case the emitter
//! doesn't handle. Clear messages, no stable codes.

use std::fmt;

#[derive(Debug)]
pub enum LlvmEmitError {
    /// Reached during the walk — shouldn't happen on a diagnostic-free
    /// `lower()`. Indicates the IR has a case the emitter doesn't yet
    /// cover (a new `ProcType` variant, a new `Inst`, etc.).
    UnsupportedInst(String),
}

impl fmt::Display for LlvmEmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlvmEmitError::UnsupportedInst(msg) => {
                write!(f, "LLVM emit: unsupported IR case: {msg}")
            }
        }
    }
}

impl std::error::Error for LlvmEmitError {}

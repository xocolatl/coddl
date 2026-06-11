//! The codegen seam.
//!
//! Both `coddl-codegen-llvm` and `coddl-codegen-cranelift` implement
//! this trait. The trait is deliberately tiny — each backend picks
//! its own `Output` (IR text vs. object bytes) and `Error` type. ProcIR
//! itself stays oblivious to either side.

use crate::ir::Module;

/// Emit a backend artifact from a ProcIR module.
pub trait Codegen {
    /// The artifact the backend produces — `String` of LLVM IR text,
    /// `Vec<u8>` of object bytes, etc.
    type Output;
    /// Errors surface through the driver as user-visible diagnostics.
    type Error: std::fmt::Display;

    fn emit(&mut self, module: &Module) -> Result<Self::Output, Self::Error>;
}

//! Type checker and type representation for Coddl.
//!
//! Public surface: `check(source, file) -> CheckOutput` runs the lexer,
//! parser, and typechecker in sequence and returns every diagnostic
//! emitted. Internal modules:
//!
//! - [`ty`] — the `Type` enum.
//! - [`builtins`] — the built-in operator registry.
//! - [`checker`] — the `TypeChecker` walk.

pub mod builtins;
pub mod checker;
pub mod ty;

pub use checker::{check, CheckOutput, HintKind, TypeHint};
pub use ty::Type;

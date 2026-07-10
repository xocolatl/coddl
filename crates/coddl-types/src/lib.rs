//! Type checker and type representation for Coddl.
//!
//! Public surface: `check(source, file, file_kind) -> CheckOutput`
//! runs the lexer, parser, and typechecker in sequence and returns
//! every diagnostic emitted plus the relvar table populated from the
//! file's declarations. Internal modules:
//!
//! - [`ty`] — the `Type` enum and `Heading`.
//! - [`builtins`] — the built-in operator registry.
//! - [`relvars`] — the per-file relvar table.
//! - [`checker`] — the `TypeChecker` walk.

pub mod builtins;
pub mod checker;
pub mod relvars;
pub mod ty;

pub use checker::{
    check, check_program, resolve_type_ref_quiet, CheckOutput, CheckUnit, HintKind, PossrepScalar,
    ProgramCheckOutput, TypeHint,
};
pub use relvars::{RelvarInfo, RelvarKind, RelvarTable};
pub use ty::{Heading, Type};

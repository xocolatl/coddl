//! Procedural SSA IR — backend-agnostic.
//!
//! SSA blocks with typed values plus relation-aware ops: `query`, `load`,
//! `assign_relvar`, `multi_assign`, `begin_tx` / `commit_tx` / `rollback_tx`.
//! See ARCHITECTURE.md §4.
//!
//! The IR carries no LLVM-specific intrinsic names, metadata, or calling
//! conventions at the node level — per-backend specifics live in the
//! codegen crates (`coddl-codegen-llvm`, `coddl-codegen-cranelift`,
//! `coddl-codegen-wasm`).

pub mod codegen;
pub mod ir;
pub mod layout;
pub mod lower;

pub use codegen::Codegen;
pub use ir::{
    BasicBlock, BlockId, Const, Function, Heading, HeadingId, Inst, Module, ProcType, Terminator,
    Type, ValueId,
};
pub use layout::{cell_kind, cell_width, kind_tag, record_layout, AttrLayout, RecordLayout};
pub use lower::{lower, LowerOutput};

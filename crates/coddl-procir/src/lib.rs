//! Procedural SSA IR — backend-agnostic.
//!
//! SSA blocks with typed values plus relation-aware ops: `query`, `load`,
//! `assign_relvar`, `multi_assign`, `begin_tx` / `commit_tx` / `rollback_tx`.
//! See `docs/procir.md` — and note that the relation-aware ops are *call
//! sites* for runtime ABI entry points, not algebra primitives. The
//! algebra lives in `coddl-relir` (see `docs/relir.md`).
//!
//! The IR carries no LLVM-specific intrinsic names, metadata, or calling
//! conventions at the node level — per-backend specifics live in the
//! codegen crates (`coddl-codegen-llvm`, `coddl-codegen-cranelift`,
//! `coddl-codegen-wasm`).

pub mod codegen;
mod cut;
pub mod ir;
pub mod layout;
pub mod lower;

pub use codegen::Codegen;
pub use ir::{
    BasicBlock, BlockId, Const, Function, Heading, HeadingId, Inst, Module, PlanEntry, ProcType,
    PublicRelvarBinding, ScalarOp, Terminator, Type, ValueId,
};
pub use layout::{
    cell_kind, cell_width, kind_tag, record_layout, tuple_is_boxed, AttrLayout, RecordLayout,
    TUPLE_BOX_THRESHOLD,
};
pub use lower::{explain_with_plan, lower, lower_with_plan, ExplainEntry, LowerOutput};

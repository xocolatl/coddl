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

//! Compile-time RelIR → ProcIR lowering for in-process subtrees.
//!
//! The peer of `coddl-sqlemit`: both consume RelIR; `coddl-sqlemit`
//! emits SQL strings (for SQL-rooted subtrees), this crate emits a
//! sequence of ProcIR calls into the runtime's relational primitives
//! (`coddl_relation_where`, `coddl_relation_join`, …) for materialized-
//! rooted subtrees. The RelIR optimizer picks the SQL/in-process cut;
//! this crate produces the compile-time output for the in-process side.
//! Dynamic plans (relation-polymorphic) take the runtime RelIR
//! interpreter path instead. See `docs/relir.md` and
//! `docs/runtime.md` "Reaching the engines."

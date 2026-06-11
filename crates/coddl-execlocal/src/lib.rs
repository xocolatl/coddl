//! In-process RelIR executor over materialized relations.
//!
//! Volcano-style iterators, hash joins, sort-merge — for any sub-plan
//! whose leaves aren't relvars (relations constructed in code, results
//! of joining two materialized relations, etc.). The RelIR optimizer
//! picks the SQL/in-process cut; this crate runs the in-process side.
//! See ARCHITECTURE.md §9.

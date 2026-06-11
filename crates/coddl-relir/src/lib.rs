//! Relational IR — Algebra A core with a sugar layer that desugars
//! during lowering (ARCHITECTURE.md §4).
//!
//! Core operators: AND (natural join), OR, NOT, REMOVE, RENAME, TCLOSE.
//! Sugar: Project, Restrict (WHERE), Join, Union, Minus, Intersect,
//! SemiJoin, SemiMinus, Extend, Summarize, Group, Ungroup, Wrap, Unwrap.
//!
//! Every node carries: heading, FD set, constraint set, storage-origin
//! flag (relvar-rooted vs materialized-rooted vs mixed). The optimizer
//! draws the SQL-vs-in-process cut as close to the leaves as possible.

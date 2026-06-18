//! Relational IR — Algebra A core with a sugar layer that desugars
//! during lowering. See `docs/relir.md`.
//!
//! Core operators: AND (natural join), OR, NOT, REMOVE, RENAME, TCLOSE.
//! Sugar: Project, Restrict (WHERE), Join, Union, Minus, Intersect,
//! SemiJoin, SemiMinus, Extend, Summarize, Group, Ungroup, Wrap, Unwrap.
//!
//! Every node carries: heading, FD set, constraint set, storage-origin
//! flag (relvar-rooted vs materialized-rooted vs mixed). The optimizer
//! draws the SQL-vs-in-process cut as close to the leaves as possible.
//!
//! RelIR is backend-agnostic: a leaf is rooted in a logical database, and the
//! storage-origin flag records only whether a subtree is pushable to a
//! backend. Which backend, and its SQL dialect, are resolved at the storage
//! boundary — never in this IR.
//!
//! Today only the relvar leaf and the `Restrict` / `Project` sugar exist,
//! with heading and storage-origin inference. The A core, the remaining
//! sugar, and the FD/constraint sets grow in place.

mod expr;

pub use coddl_types::{Heading, Type};
pub use expr::{Literal, Predicate, RelExpr, StorageOrigin};

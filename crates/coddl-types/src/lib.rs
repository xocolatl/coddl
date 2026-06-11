//! Type checker and type representation.
//!
//! Possreps, selectors, and `THE_` accessors (RM Pre 4-5);
//! TUPLE and RELATION type generators (RM Pre 6-7);
//! candidate keys (RM Pre 15). See ARCHITECTURE.md §7.
//!
//! Error types propagate via an `Error` variant rather than cascading —
//! see §12 discipline #2.

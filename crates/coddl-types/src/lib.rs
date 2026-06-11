//! Type checker and type representation.
//!
//! Covers possreps, selectors and `THE_` accessors (RM Pre 4–5),
//! `Tuple` and `Relation` type generators (RM Pre 6–7), and candidate
//! keys (RM Pre 15). Type errors propagate via an `Error` variant so
//! a single mismatch never cascades into a hundred unrelated
//! diagnostics.

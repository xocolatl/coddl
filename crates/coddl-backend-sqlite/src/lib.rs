//! SQLite backend.
//!
//! Implements `coddl_sqlemit::Backend` and `coddl_sqlemit::Conn` using
//! `rusqlite`. SQLite-specific quirks (BOOLEAN as `INTEGER CHECK (col IN (0, 1))`,
//! `CAST` on INSERT to dodge affinity coercion) are handled here, not in
//! `coddl-sqlemit`. See `docs/storage.md` and `docs/sqlemit.md`.

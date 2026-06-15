//! Postgres backend.
//!
//! Implements `coddl_sqlemit::Backend` and `coddl_sqlemit::Conn` using
//! the sync `postgres` crate. Ships in-memory relations into temp tables
//! via `COPY` for large batches and `UNNEST` for small. See
//! `docs/sqlemit.md` "Sending in-memory relations back into SQL".

//! `libcoddl_runtime` — the C-ABI runtime linked into compiled Coddl
//! binaries. See ARCHITECTURE.md §6.
//!
//! Built as a `staticlib` so user binaries don't take a dynamic-linker
//! hit; also as an `rlib` so workspace crates can use the Rust API in
//! tests. Responsibilities: connection pool, prepared-statement cache,
//! row iteration, value marshaling across the FFI seam, the in-process
//! RelIR executor (via `coddl-execlocal`), and the RelIR→SQL emitter
//! (via `coddl-sqlemit` — the same crate the compiler uses).
//!
//! ## FFI discipline (§6, §10 risk #8)
//!
//! Values crossing into LLVM-emitted code MUST be `#[repr(C)]` or
//! primitive. No `Vec`/`String` raw pointers without an explicit owner
//! declaration. No Rust enums-with-payload unless tagged-C-style. The
//! struct layouts here are the single source of truth — LLVM codegen
//! mirrors them; drift between the two is a debug nightmare.

use std::sync::atomic::{AtomicU32, Ordering};

/// FFI error codes. `0` is success; any nonzero value is a failure whose
/// human-readable message is available via [`coddl_last_error_message`]
/// (thread-local). Codes are stable identifiers — never renumber.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoddlStatus {
    Ok = 0,
    NotInitialized = 1,
    BackendError = 2,
    PlanNotFound = 3,
    TypeMismatch = 4,
    Internal = 100,
}

/// Opaque handle to a registered query plan. The compiler assigns these
/// at codegen time; the runtime keys its prepared-statement cache by
/// `(plan_id, backend)`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlanId(pub u32);

/// Opaque handle to a runtime-managed `Relation` value. May be plan-backed
/// (re-evaluates on use), materialized (in-memory buffer), or cursor-backed
/// (one-shot stream). See ARCHITECTURE.md §9.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RelationHandle(pub u32);

static INITIALIZED: AtomicU32 = AtomicU32::new(0);

/// Initialize the runtime. Idempotent — second call returns `Ok` without
/// re-initializing. Must be called by the compiled program's startup
/// before any other runtime entry point.
///
/// # Safety
/// Safe to call from a single-threaded program startup. Concurrent first
/// calls are serialized by the atomic but only one wins.
#[no_mangle]
pub unsafe extern "C" fn coddl_runtime_init() -> CoddlStatus {
    INITIALIZED.store(1, Ordering::SeqCst);
    CoddlStatus::Ok
}

/// Tear down the runtime. Closes the connection pool and frees any
/// runtime-owned arenas. After this call all `RelationHandle`s and
/// `PlanId`s previously returned are invalid.
///
/// # Safety
/// Must be the last runtime call from the compiled program.
#[no_mangle]
pub unsafe extern "C" fn coddl_runtime_shutdown() -> CoddlStatus {
    INITIALIZED.store(0, Ordering::SeqCst);
    CoddlStatus::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_shutdown_are_idempotent() {
        unsafe {
            assert_eq!(coddl_runtime_init(), CoddlStatus::Ok);
            assert_eq!(coddl_runtime_init(), CoddlStatus::Ok);
            assert_eq!(coddl_runtime_shutdown(), CoddlStatus::Ok);
            assert_eq!(coddl_runtime_shutdown(), CoddlStatus::Ok);
        }
    }
}

//! `libcoddl_runtime` — the C-ABI runtime linked into compiled Coddl
//! binaries. See `docs/runtime.md`.
//!
//! Built as a `staticlib` so user binaries don't take a dynamic-linker
//! hit; also as an `rlib` so workspace crates can use the Rust API in
//! tests. Responsibilities: connection pool, prepared-statement cache,
//! row iteration, value marshaling across the FFI seam, the in-process
//! runtime library of relational primitives (called from compiled
//! code), the runtime RelIR interpreter (for dynamic plans), and
//! `coddl-sqlemit` as a library (so runtime-built plans lower to SQL
//! through the same code path the compiler uses).
//!
//! ## FFI discipline (see `docs/runtime.md`, `docs/risks.md` risk #8)
//!
//! Values crossing into LLVM-emitted code MUST be `#[repr(C)]` or
//! primitive. No `Vec`/`String` raw pointers without an explicit owner
//! declaration. No Rust enums-with-payload unless tagged-C-style. The
//! struct layouts here are the single source of truth — LLVM codegen
//! mirrors them; drift between the two is a debug nightmare.

use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};

mod audit;
pub mod rational;
pub mod rc;
pub mod relation;
pub mod sqlite;

pub use rational::{
    coddl_rational_add, coddl_rational_cmp, coddl_rational_div, coddl_rational_from_ints,
    coddl_rational_mul, coddl_rational_sub, coddl_rational_to_approx,
};
pub use rc::{
    coddl_rc_alloc, coddl_rc_length, coddl_rc_release, coddl_rc_retain, coddl_seq_index,
    live_allocations, CoddlKind, CoddlRcHeader, HEADER_SIZE, IMMORTAL_RC,
};
pub use relation::{
    coddl_extract_check_cardinality, coddl_relation_project, coddl_relation_rename,
    coddl_relation_seal, coddl_relation_where, coddl_text_eq, coddl_write_relation, CoddlAttrDesc,
    CoddlAttrKind, CoddlHeadingDesc,
};
pub use sqlite::{
    coddl_begin_tx, coddl_commit_tx, coddl_query, coddl_register_database, coddl_register_plan,
    coddl_resolve_op_field, coddl_rollback_tx, coddl_sqlite_relvar_init, CoddlParam,
};

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
/// (one-shot stream). See `docs/runtime.md` "Relation values at runtime".
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
    // Defense in depth: codegen emits one `coddl_rc_release` per
    // public-relvar slot before this call, but the runtime walks its
    // own slot table too so a missed release doesn't leak the SQLite
    // connection. Connections are dropped here, closing every open
    // handle.
    sqlite::shutdown_storage();
    INITIALIZED.store(0, Ordering::SeqCst);
    CoddlStatus::Ok
}

/// Write `len` bytes from `ptr` to stdout, followed by a newline.
/// The compiled program's `write_line` operator lowers to a call to
/// this symbol.
///
/// # Safety
/// `ptr` must point to at least `len` initialized bytes; the slice
/// is read only for the duration of the call. UTF-8 validity is the
/// caller's responsibility — the runtime writes raw bytes.
#[no_mangle]
pub unsafe extern "C" fn coddl_write_line(ptr: *const u8, len: usize) {
    let slice = std::slice::from_raw_parts(ptr, len);
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let _ = w.write_all(slice);
    let _ = w.write_all(b"\n");
}

/// Write `prompt` (a `Text` `(ptr, len)`) to stdout without a trailing
/// newline, flush, then read one line from stdin. Returns the line as a
/// freshly allocated heap `Text` payload with any trailing `\n` / `\r\n`
/// stripped, and writes its byte length into `*len_out`. On EOF (no bytes
/// read) the result is the empty `Text` (`*len_out == 0`).
///
/// The Coddl `read_line { prompt: ... }` operator lowers to a call to this
/// symbol. Because the runtime can't return a fat pointer by value, the
/// length crosses back through the `len_out` out-parameter — the same
/// convention `coddl_resolve_op_field` uses. Codegen pairs the returned
/// payload pointer with the stored length to form the `(ptr, len)` value.
///
/// The result (rc=1) is reference-counted like
/// [`coddl_text_concat`](crate::coddl_text_concat)'s — released at scope exit /
/// consumption, or by the relation drop walker once stored into a cell.
///
/// # Safety
/// `prompt_ptr` must point to at least `prompt_len` initialized bytes (or be
/// null with `prompt_len == 0`); `len_out` must point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn coddl_read_line(
    prompt_ptr: *const u8,
    prompt_len: usize,
    len_out: *mut usize,
) -> *mut u8 {
    // Emit the prompt (no newline) and flush so it shows before the read.
    {
        let stdout = std::io::stdout();
        let mut w = stdout.lock();
        if prompt_len > 0 {
            let prompt = std::slice::from_raw_parts(prompt_ptr, prompt_len);
            let _ = w.write_all(prompt);
        }
        let _ = w.flush();
    }

    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    let bytes = strip_line_ending(line.as_bytes());
    let n = bytes.len();

    let out = crate::rc::coddl_rc_alloc(n, n as u32, crate::rc::CoddlKind::Text as u32, std::ptr::null());
    if n > 0 {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, n);
    }
    *len_out = n;
    out
}

/// Drop a single trailing line terminator — `\n` or `\r\n` — from `bytes`,
/// leaving any interior or non-terminal CR/LF untouched. The slice the
/// reader hands back has at most one terminator (one `read_line`).
fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    match bytes {
        [rest @ .., b'\r', b'\n'] => rest,
        [rest @ .., b'\n'] => rest,
        _ => bytes,
    }
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

    #[test]
    fn write_line_callable_through_pointer() {
        // Smoke test: round-trip an empty slice through the C ABI and
        // confirm the call doesn't UB. Output verification is done
        // by the codegen e2e tests, which check the printed text.
        let bytes: &[u8] = b"";
        unsafe { coddl_write_line(bytes.as_ptr(), bytes.len()) };
    }

    #[test]
    fn strip_line_ending_handles_lf_crlf_and_none() {
        assert_eq!(strip_line_ending(b"Vik\n"), b"Vik");
        assert_eq!(strip_line_ending(b"Vik\r\n"), b"Vik");
        assert_eq!(strip_line_ending(b"Vik"), b"Vik");
        assert_eq!(strip_line_ending(b""), b"");
        assert_eq!(strip_line_ending(b"\n"), b"");
        // A lone trailing CR is not a terminator; only `\r\n` is.
        assert_eq!(strip_line_ending(b"Vik\r"), b"Vik\r");
        // Interior newlines are untouched; only the final one is stripped.
        assert_eq!(strip_line_ending(b"a\nb\n"), b"a\nb");
    }
}

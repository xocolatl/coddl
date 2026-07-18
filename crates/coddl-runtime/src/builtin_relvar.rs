//! Generic runtime backing for `builtin relvar`s.
//!
//! A read of a `builtin relvar` lowers to `coddl_builtin_read(handle, desc)` and
//! a whole-value assignment to `coddl_builtin_assign(handle, desc, rel)`. Both
//! dispatch on the relvar's interned qualified-name `handle`:
//!
//!   * `coddl::env::Environment` → the process environment (see [`crate::env`]),
//!   * every other handle → a per-handle in-memory relation value (Coddl's
//!     system catalog: `coddl::storage::*`, `coddl::catalog::*`).
//!
//! Every `builtin relvar` — env and catalog alike — goes through this one path;
//! there are no per-relvar symbols. Writes are whole-value: `assign` reconciles
//! the store to the given relation (env sets/unsets, the catalog replaces the
//! stored value), matching the compiler's read+assign lowering.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::rc::{coddl_rc_alloc, coddl_rc_release, coddl_rc_retain, CoddlKind};
use crate::relation::CoddlHeadingDesc;

/// The handle of `coddl::env`'s `Environment` — the one builtin relvar backed by
/// external state rather than the in-memory catalog store.
const ENV_HANDLE: &str = "coddl::env::Environment";

/// The in-memory catalog: handle → the owned RC relation payload currently held
/// for that relvar (a `usize` so the raw pointer is `Send`; the program is
/// single-threaded, but the store is a process-global static). The store keeps
/// exactly one retained reference per handle; an absent handle reads as empty.
fn catalog() -> &'static Mutex<HashMap<String, usize>> {
    static STORE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decode the handle argument as a `&str`. The bytes are a compiler-emitted
/// ASCII qualified name, valid for the program's lifetime.
unsafe fn handle_str(ptr: *const u8, len: usize) -> &'static str {
    std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len))
}

/// A fresh empty RC relation of heading `desc` (record count 0).
unsafe fn empty_relation(desc: *const CoddlHeadingDesc) -> *mut u8 {
    coddl_rc_alloc(0, 0, CoddlKind::Relation as u32, desc)
}

/// Read a `builtin relvar`'s current value as a fresh **owned** RC relation of
/// heading `desc`, dispatched on `handle`.
///
/// # Safety
/// FFI entry point. `handle_ptr`/`handle_len` are valid UTF-8 handle bytes;
/// `desc` is the heading descriptor codegen emitted for this relvar. The
/// returned payload is owned — the caller must eventually `coddl_rc_release` it.
#[no_mangle]
pub unsafe extern "C" fn coddl_builtin_read(
    handle_ptr: *const u8,
    handle_len: usize,
    desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    let handle = handle_str(handle_ptr, handle_len);
    if handle == ENV_HANDLE {
        return crate::env::env_read(desc);
    }
    // Catalog: hand back a retained copy of the stored (immutable) value, or an
    // empty relation of this heading if nothing has been assigned yet.
    let store = catalog().lock().unwrap();
    match store.get(handle) {
        Some(&p) if p != 0 => {
            let p = p as *mut u8;
            coddl_rc_retain(p);
            p
        }
        _ => empty_relation(desc),
    }
}

/// Assign a whole relation value to a `builtin relvar` — reconcile its store to
/// `rel`, dispatched on `handle`. `rel` is **borrowed** (the caller releases its
/// own temporary after this returns).
///
/// # Safety
/// FFI entry point. `handle_ptr`/`handle_len` are valid UTF-8 handle bytes;
/// `desc` is the heading descriptor; `rel` is null or a valid relation payload
/// of heading `desc`.
#[no_mangle]
pub unsafe extern "C" fn coddl_builtin_assign(
    handle_ptr: *const u8,
    handle_len: usize,
    desc: *const CoddlHeadingDesc,
    rel: *const u8,
) {
    let handle = handle_str(handle_ptr, handle_len);
    if handle == ENV_HANDLE {
        crate::env::env_assign(desc, rel);
        return;
    }
    // Catalog: retain the new value (the store keeps its own reference past the
    // caller's release), replace the entry, and release the previous value.
    let mut store = catalog().lock().unwrap();
    let prev = if rel.is_null() {
        store.remove(handle)
    } else {
        coddl_rc_retain(rel as *mut u8);
        store.insert(handle.to_string(), rel as usize)
    };
    if let Some(old) = prev {
        if old != 0 {
            coddl_rc_release(old as *mut u8);
        }
    }
}

/// Release every relation the catalog store still holds — called once at runtime
/// shutdown. The store keeps one reference per handle for the program's
/// lifetime, so without this drain a program's final catalog values would
/// register as leaks under `CODDL_LEAK_CHECK`. (The env store holds no
/// payloads — the process environment is not reference-counted — so it needs no
/// draining.)
///
/// # Safety
/// Call once, at shutdown, after no further builtin-relvar reads/assigns occur.
pub unsafe fn clear_store() {
    let mut store = catalog().lock().unwrap();
    for (_, p) in store.drain() {
        if p != 0 {
            coddl_rc_release(p as *mut u8);
        }
    }
}

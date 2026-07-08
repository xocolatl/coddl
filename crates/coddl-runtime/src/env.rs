//! `coddl::env` runtime backing — the process environment as a relation.
//!
//! `coddl_env_snapshot` is the read symbol a `coddl::env` `Environment`
//! reference lowers to a call of (see `coddl-procir`'s `BUILTIN_RELVARS`). It
//! returns the current environment as a fresh RC `Relation { name: Text, value:
//! Text }` payload, built with the same `coddl_rc_alloc` + cell layout the SQL
//! path uses. Writes (`coddl_env_set` / `coddl_env_unset`) land with env DML in
//! a later phase.

use std::sync::OnceLock;

use crate::rc::{coddl_rc_alloc, CoddlKind, CoddlRcHeader, HEADER_SIZE};
use crate::relation::{CoddlAttrDesc, CoddlAttrKind, CoddlHeadingDesc};

/// Two `Text` cells, name-sorted (`name` @ 0, `value` @ 16); each is 16 bytes
/// (`ptr` @ 0, `len` @ 8), so `record_size` is 32.
const RECORD_SIZE: usize = 32;

/// The fixed `{ name: Text, value: Text }` heading descriptor, built once and
/// leaked so the pointer stays valid for the program's lifetime — the returned
/// relation's RC header holds it, and consumers read it after this call
/// returns. Hand-written here: one more site under "FFI struct-layout single
/// source of truth" ([docs/risks.md]); it must match what codegen emits for
/// this heading.
fn env_heading_desc() -> *const CoddlHeadingDesc {
    static DESC: OnceLock<usize> = OnceLock::new();
    *DESC.get_or_init(|| {
        let attrs: &'static [CoddlAttrDesc; 2] = Box::leak(Box::new([
            CoddlAttrDesc {
                name: b"name".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"value".as_ptr(),
                name_len: 5,
                kind: CoddlAttrKind::Text as u32,
                offset: 16,
                sub: std::ptr::null(),
            },
        ]));
        let desc: &'static CoddlHeadingDesc = Box::leak(Box::new(CoddlHeadingDesc {
            attr_count: 2,
            record_size: RECORD_SIZE as u32,
            attrs: attrs.as_ptr(),
        }));
        desc as *const CoddlHeadingDesc as usize
    }) as *const CoddlHeadingDesc
}

/// Allocate an owned RC `Text` cell holding `bytes`.
unsafe fn alloc_text(bytes: &[u8]) -> *mut u8 {
    let p = coddl_rc_alloc(
        bytes.len(),
        bytes.len() as u32,
        CoddlKind::Text as u32,
        std::ptr::null(),
    );
    if !bytes.is_empty() {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    }
    p
}

/// Read the process environment as a fresh RC `Relation { name, value }`. A
/// read of the `coddl::env` `Environment` relvar lowers to a call of this.
///
/// # Safety
/// FFI entry point. The returned pointer is an owned RC relation payload the
/// caller must eventually `coddl_rc_release`.
#[no_mangle]
pub unsafe extern "C" fn coddl_env_snapshot() -> *mut u8 {
    // Skip non-UTF-8 entries: Coddl `Text` is UTF-8, and `std::env::vars()`
    // would panic on them. Env var names are unique, so no dedup/seal is needed.
    let vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| match (k.into_string(), v.into_string()) {
            (Ok(k), Ok(v)) => Some((k, v)),
            _ => None,
        })
        .collect();

    let count = vars.len();
    let desc = env_heading_desc();
    let payload = coddl_rc_alloc(
        count * RECORD_SIZE,
        count as u32,
        CoddlKind::Relation as u32,
        desc,
    );
    if payload.is_null() {
        return payload;
    }
    for (i, (name, value)) in vars.iter().enumerate() {
        let rec = payload.add(i * RECORD_SIZE);
        // name cell @ 0
        let np = alloc_text(name.as_bytes());
        std::ptr::write(rec as *mut usize, np as usize);
        std::ptr::write(rec.add(8) as *mut usize, name.len());
        // value cell @ 16
        let vp = alloc_text(value.as_bytes());
        std::ptr::write(rec.add(16) as *mut usize, vp as usize);
        std::ptr::write(rec.add(24) as *mut usize, value.len());
    }
    payload
}

/// Read a `Text` cell `(ptr @ 0, len @ 8)` as a `&str`. The bytes are UTF-8:
/// they came from a Coddl `Text` literal or a value this module wrote.
unsafe fn text_cell(cell: *const u8) -> &'static str {
    let ptr = std::ptr::read(cell as *const usize) as *const u8;
    let len = std::ptr::read(cell.add(8) as *const usize);
    std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len))
}

/// Number of records in a relation payload (from its RC header's `length`).
unsafe fn record_count(rel: *const u8) -> usize {
    (*(rel.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize
}

/// Apply an `insert`/`update` write to the environment: set each
/// `{ name, value }` record's variable (`setenv`; overwrites, so `update` and
/// `insert` share this path). The relation is borrowed, not consumed.
///
/// # Safety
/// `rel` must be null or a valid `Relation { name: Text, value: Text }` payload.
#[no_mangle]
pub unsafe extern "C" fn coddl_env_insert(rel: *const u8) {
    if rel.is_null() {
        return;
    }
    for i in 0..record_count(rel) {
        let rec = rel.add(i * RECORD_SIZE);
        std::env::set_var(text_cell(rec), text_cell(rec.add(16)));
    }
}

/// Apply a `delete` write: unset each record's variable (`unsetenv`). Only the
/// `name` column is read.
///
/// # Safety
/// `rel` must be null or a valid `Relation { name: Text, value: Text }` payload.
#[no_mangle]
pub unsafe extern "C" fn coddl_env_unset(rel: *const u8) {
    if rel.is_null() {
        return;
    }
    for i in 0..record_count(rel) {
        std::env::remove_var(text_cell(rel.add(i * RECORD_SIZE)));
    }
}

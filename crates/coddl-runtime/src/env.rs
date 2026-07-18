//! `coddl::env` runtime backing — the process environment as a relation.
//!
//! `coddl::env`'s `Environment` is a `builtin relvar`; its read and assign go
//! through the generic [`crate::builtin_relvar`] FFI, which dispatches this
//! relvar's handle here. [`env_read`] returns the current environment as a fresh
//! RC `Relation { name, value }` of the **passed** heading descriptor (no
//! hand-written descriptor — codegen supplies it, resolving the drift site under
//! [docs/risks.md] §8). [`env_assign`] reconciles the process environment to a
//! whole relation value (`setenv` each record, `unsetenv` every variable the
//! value omits).

use std::collections::HashSet;

use crate::rc::{coddl_rc_alloc, CoddlKind, CoddlRcHeader, HEADER_SIZE};
use crate::relation::CoddlHeadingDesc;

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

/// Read a `Text` cell `(ptr @ 0, len @ 8)` as a `&str`. UTF-8 by construction:
/// the bytes came from a Coddl `Text` literal or a value this module wrote.
unsafe fn text_cell(cell: *const u8) -> &'static str {
    let ptr = std::ptr::read(cell as *const usize) as *const u8;
    let len = std::ptr::read(cell.add(8) as *const usize);
    std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len))
}

/// Number of records in a relation payload (from its RC header's `length`).
unsafe fn record_count(rel: *const u8) -> usize {
    (*(rel.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize
}

/// Byte offset of the attribute named `want` within a record of heading `desc`.
/// The compiler-emitted descriptor always carries the env relvar's `{ name,
/// value }` attributes, so a lookup never fails in practice.
unsafe fn attr_offset(desc: *const CoddlHeadingDesc, want: &str) -> usize {
    let d = &*desc;
    let attrs = std::slice::from_raw_parts(d.attrs, d.attr_count as usize);
    for a in attrs {
        let name =
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(a.name, a.name_len as usize));
        if name == want {
            return a.offset as usize;
        }
    }
    0
}

/// Read the process environment as a fresh owned RC `Relation { name, value }`
/// of heading `desc`. Called from [`crate::builtin_relvar::coddl_builtin_read`]
/// for the `coddl::env::Environment` handle.
///
/// # Safety
/// `desc` is a valid `{ name: Text, value: Text }` heading descriptor. The
/// returned payload is owned — the caller `coddl_rc_release`s it.
pub(crate) unsafe fn env_read(desc: *const CoddlHeadingDesc) -> *mut u8 {
    // Skip non-UTF-8 entries: Coddl `Text` is UTF-8, and `into_string` fails
    // otherwise. Env var names are unique, so no dedup/seal is needed.
    let vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| match (k.into_string(), v.into_string()) {
            (Ok(k), Ok(v)) => Some((k, v)),
            _ => None,
        })
        .collect();

    let record_size = (*desc).record_size as usize;
    let name_off = attr_offset(desc, "name");
    let value_off = attr_offset(desc, "value");
    let count = vars.len();
    let payload = coddl_rc_alloc(
        count * record_size,
        count as u32,
        CoddlKind::Relation as u32,
        desc,
    );
    if payload.is_null() {
        return payload;
    }
    for (i, (name, value)) in vars.iter().enumerate() {
        let rec = payload.add(i * record_size);
        let np = alloc_text(name.as_bytes());
        std::ptr::write(rec.add(name_off) as *mut usize, np as usize);
        std::ptr::write(rec.add(name_off + 8) as *mut usize, name.len());
        let vp = alloc_text(value.as_bytes());
        std::ptr::write(rec.add(value_off) as *mut usize, vp as usize);
        std::ptr::write(rec.add(value_off + 8) as *mut usize, value.len());
    }
    payload
}

/// Reconcile the process environment to the whole relation value `rel`: `setenv`
/// each `{ name, value }` record, then `unsetenv` every current variable the
/// value omits. Called from [`crate::builtin_relvar::coddl_builtin_assign`] for
/// the `coddl::env::Environment` handle. `rel` is borrowed.
///
/// # Safety
/// `desc` is a valid `{ name, value }` descriptor; `rel` is null or a valid
/// relation payload of that heading.
pub(crate) unsafe fn env_assign(desc: *const CoddlHeadingDesc, rel: *const u8) {
    let record_size = (*desc).record_size as usize;
    let name_off = attr_offset(desc, "name");
    let value_off = attr_offset(desc, "value");

    // Set every variable the new value carries; remember which names it holds.
    let mut wanted: HashSet<String> = HashSet::new();
    if !rel.is_null() {
        for i in 0..record_count(rel) {
            let rec = rel.add(i * record_size);
            let name = text_cell(rec.add(name_off));
            let value = text_cell(rec.add(value_off));
            std::env::set_var(name, value);
            wanted.insert(name.to_string());
        }
    }

    // Unset every current variable the new value omits. Collect the names first
    // so the environment isn't mutated while it is being iterated.
    let current: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    for name in current {
        if !wanted.contains(&name) {
            std::env::remove_var(name);
        }
    }
}

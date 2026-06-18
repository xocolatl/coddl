//! SQLite-backed public-relvar materialization at startup.
//!
//! Phase 22 brings public relvars to life: at program start each
//! `public relvar` declared in `.cd` and bound through `.cdstore` gets
//! a one-time materialization pass — open SQLite read-only, prepare a
//! `SELECT <columns> FROM <table>` against the resolved path, step every
//! row, copy cells into a canonical-layout record buffer, allocate via
//! [`crate::rc::coddl_rc_alloc`], seal, store the RC pointer in a
//! per-relvar slot in the compiled binary.
//!
//! Path resolution: the runtime always consults `CODDL_<DB>_FILE` first
//! and falls back to the compile-time default. Bundled `rusqlite` means
//! the binary needs no system libsqlite3.
//!
//! ## Transactions (v1: no-ops)
//!
//! TTM OO Pre 4 forbids autocommit at the language surface; the
//! compiler enforces this by wrapping every `transaction [...]` body
//! in synthetic [`coddl_begin_tx`] / [`coddl_commit_tx`] calls. In v1
//! all public-relvar reads are served from the in-memory slot, so
//! these calls don't touch SQLite — they return Ok without doing
//! anything. The shape becomes load-bearing when write-through
//! arrives (Phase 22 successor): a real BEGIN/COMMIT round-trips to
//! the connection, and serialization-conflict replay reuses the same
//! externs.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};

use crate::rc::{coddl_rc_alloc, CoddlKind};
use crate::relation::{coddl_relation_seal, CoddlAttrKind, CoddlHeadingDesc};
use crate::CoddlStatus;

/// One open SQLite connection per resolved database path, opened
/// lazily on first relvar materialization. Closed at runtime shutdown.
/// `OnceCell` would suffice for one db; a Mutex<HashMap> generalises
/// trivially to multi-db programs and costs nothing today.
fn db_connections() -> &'static Mutex<HashMap<String, Connection>> {
    static CONNS: std::sync::OnceLock<Mutex<HashMap<String, Connection>>> =
        std::sync::OnceLock::new();
    CONNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every slot the runtime has materialized so far. Keyed by relvar
/// name; the value is the RC pointer's address, stored as `usize` so
/// the table is Send + Sync (raw pointers aren't). Used by
/// [`shutdown_storage`] to release any still-live slot before tearing
/// down the connection pool — defense in depth against codegen paths
/// that forget the per-relvar release.
fn relvar_slots() -> &'static Mutex<HashMap<String, usize>> {
    static SLOTS: std::sync::OnceLock<Mutex<HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve an operational field's value: env var if set, else the
/// compile-time default. The returned pointer is owned by the runtime
/// and stays valid for the program's lifetime; the caller treats it as
/// read-only borrowed bytes.
///
/// # Safety
/// The name and default slices must be valid for their lengths. The
/// returned pointer is null-terminated UTF-8 (a borrowed `CStr`); read
/// `len` from the second out-pointer if non-null.
#[no_mangle]
pub unsafe extern "C" fn coddl_resolve_op_field(
    env_name: *const u8,
    env_name_len: usize,
    default: *const u8,
    default_len: usize,
    out_len: *mut usize,
) -> *const u8 {
    let env_slice = std::slice::from_raw_parts(env_name, env_name_len);
    let env_str = match std::str::from_utf8(env_slice) {
        Ok(s) => s,
        Err(_) => "",
    };
    if let Ok(val) = std::env::var(env_str) {
        // Stash the env-derived string so its bytes outlive the call.
        let stored = intern_string(val);
        if !out_len.is_null() {
            *out_len = stored.len();
        }
        return stored.as_ptr();
    }
    if !out_len.is_null() {
        *out_len = default_len;
    }
    default
}

/// String interner for env-derived strings. Stored as `CString` so we
/// can hand out raw pointers with stable lifetimes; the bytes the
/// runtime returns are a borrow into this interner.
fn intern_string(s: String) -> &'static [u8] {
    static INTERN: std::sync::OnceLock<Mutex<Vec<CString>>> = std::sync::OnceLock::new();
    let intern = INTERN.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = intern.lock().expect("intern poisoned");
    let cstr = CString::new(s).expect("env value contains NUL");
    guard.push(cstr);
    // SAFETY: the CString is now owned by the static intern; its
    // bytes live until process exit. Hand out a borrowed slice into
    // it. The intern only grows; entries are never reallocated, so
    // the slice stays valid.
    let last = guard.last().unwrap();
    let ptr = last.as_bytes().as_ptr();
    let len = last.as_bytes().len();
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// Materialize one public relvar from SQLite into an RC-managed
/// in-memory payload, store the pointer in the slot global, and
/// register the slot for shutdown release.
///
/// `slot` is the binary's per-relvar global (a `*mut *mut u8`);
/// codegen passes its address. `desc` is the static heading
/// descriptor. Everything else is a (ptr, len) pair.
///
/// Aborts on:
/// - SQLite open failure (path doesn't resolve, not a SQLite file).
/// - Prepare failure (missing table, missing column).
/// - Per-cell type mismatch with the heading.
/// - NULL columns (RM Pro 4 — D forbids nulls).
///
/// # Safety
/// All pointers must be valid for their declared sizes. `slot` must
/// point to writable storage; the runtime stores the materialized RC
/// pointer there.
#[no_mangle]
pub unsafe extern "C" fn coddl_sqlite_relvar_init(
    relvar_name: *const u8,
    relvar_name_len: usize,
    db_path: *const u8,
    db_path_len: usize,
    table_name: *const u8,
    table_name_len: usize,
    column_ptrs: *const *const u8,
    column_lens: *const usize,
    column_count: u32,
    desc: *const CoddlHeadingDesc,
    slot: *mut *mut u8,
) -> CoddlStatus {
    if desc.is_null() || slot.is_null() {
        eprintln!("coddl: sqlite_relvar_init: null descriptor or slot");
        std::process::abort();
    }
    let relvar = bytes_to_str("relvar name", relvar_name, relvar_name_len);
    let path = bytes_to_str("db path", db_path, db_path_len);
    let table = bytes_to_str("table name", table_name, table_name_len);
    let columns: Vec<&str> = (0..column_count as usize)
        .map(|i| {
            let ptr = *column_ptrs.add(i);
            let len = *column_lens.add(i);
            bytes_to_str("column name", ptr, len)
        })
        .collect();

    // Open (or reuse) the connection.
    let conn_payload = ensure_connection(path);

    // Compose the SELECT. Columns are pre-validated by the planner
    // (PL0008–PL0010); we still quote them defensively against any
    // identifier that happens to need it.
    let select_cols = columns
        .iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let table_quoted = format!("\"{}\"", table.replace('"', "\"\""));
    let sql = format!("SELECT {select_cols} FROM {table_quoted}");

    // Marshal rows.
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let record_size = (*desc).record_size as usize;

    // Two passes: first count rows so the allocation is right-sized;
    // then marshal cells. SQLite's preparedstatement returns rows
    // streamingly so we accumulate into a Vec on the first pass too —
    // counting via `SELECT count(*)` would race with the SELECT under
    // concurrent writers. For v1 (read-only), one pass into a
    // `Vec<row_bytes>` and then memcpy into the RC payload is the
    // simplest correct shape.
    let mut row_buffers: Vec<Vec<u8>> = Vec::new();
    {
        let conn_guard = db_connections().lock().expect("conn map poisoned");
        let conn = conn_guard
            .get(path)
            .expect("connection inserted by ensure_connection");
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(err) => {
                eprintln!(
                    "coddl: sqlite_relvar_init: prepare failed for relvar `{relvar}`: {err}"
                );
                std::process::abort();
            }
        };
        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(err) => {
                eprintln!(
                    "coddl: sqlite_relvar_init: query failed for relvar `{relvar}`: {err}"
                );
                std::process::abort();
            }
        };
        loop {
            let row = match rows.next() {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(err) => {
                    eprintln!(
                        "coddl: sqlite_relvar_init: row step failed for relvar `{relvar}`: {err}"
                    );
                    std::process::abort();
                }
            };
            let mut buf = vec![0u8; record_size];
            for (i, attr) in attrs.iter().enumerate() {
                let kind = attr.kind;
                let offset = attr.offset as usize;
                if kind == CoddlAttrKind::Integer as u32 {
                    let v: i64 = match row.get(i) {
                        Ok(v) => v,
                        Err(err) => {
                            let attr_name = read_attr_name(attr);
                            eprintln!(
                                "coddl: sqlite_relvar_init: column `{attr_name}` of \
                                 `{relvar}` is not Integer (or is NULL): {err}"
                            );
                            std::process::abort();
                        }
                    };
                    let bytes = v.to_ne_bytes();
                    buf[offset..offset + 8].copy_from_slice(&bytes);
                } else if kind == CoddlAttrKind::Boolean as u32 {
                    let v: bool = match row.get(i) {
                        Ok(v) => v,
                        Err(err) => {
                            let attr_name = read_attr_name(attr);
                            eprintln!(
                                "coddl: sqlite_relvar_init: column `{attr_name}` of \
                                 `{relvar}` is not Boolean (or is NULL): {err}"
                            );
                            std::process::abort();
                        }
                    };
                    let n: i64 = if v { 1 } else { 0 };
                    buf[offset..offset + 8].copy_from_slice(&n.to_ne_bytes());
                } else if kind == CoddlAttrKind::Text as u32 {
                    let s: String = match row.get(i) {
                        Ok(s) => s,
                        Err(err) => {
                            let attr_name = read_attr_name(attr);
                            eprintln!(
                                "coddl: sqlite_relvar_init: column `{attr_name}` of \
                                 `{relvar}` is not Text (or is NULL): {err}"
                            );
                            std::process::abort();
                        }
                    };
                    let stored = intern_string(s);
                    let ptr = stored.as_ptr() as usize;
                    let len = stored.len();
                    buf[offset..offset + 8].copy_from_slice(&ptr.to_ne_bytes());
                    buf[offset + 8..offset + 16].copy_from_slice(&len.to_ne_bytes());
                } else {
                    let attr_name = read_attr_name(attr);
                    eprintln!(
                        "coddl: sqlite_relvar_init: unsupported attr kind {kind} for \
                         column `{attr_name}` of `{relvar}`"
                    );
                    std::process::abort();
                }
            }
            row_buffers.push(buf);
        }
    }

    let _ = conn_payload;

    let row_count = row_buffers.len();
    let payload_size = record_size.saturating_mul(row_count);
    let payload = coddl_rc_alloc(
        payload_size,
        row_count as u32,
        CoddlKind::Relation as u32,
        desc,
    );
    if payload.is_null() {
        eprintln!(
            "coddl: sqlite_relvar_init: allocation failed for relvar `{relvar}` \
             ({row_count} rows × {record_size} bytes)"
        );
        std::process::abort();
    }
    let dest = std::slice::from_raw_parts_mut(payload, payload_size);
    for (i, row) in row_buffers.iter().enumerate() {
        let start = i * record_size;
        dest[start..start + record_size].copy_from_slice(row);
    }
    coddl_relation_seal(payload, desc);

    *slot = payload;
    relvar_slots()
        .lock()
        .expect("slot map poisoned")
        .insert(relvar.to_string(), payload as usize);
    CoddlStatus::Ok
}

/// Read an attribute name from its descriptor entry. Bytes are not
/// null-terminated — the descriptor carries the length explicitly.
unsafe fn read_attr_name(attr: &crate::relation::CoddlAttrDesc) -> &str {
    let slice = std::slice::from_raw_parts(attr.name, attr.name_len as usize);
    std::str::from_utf8(slice).unwrap_or("<invalid utf-8>")
}

/// Decode a UTF-8 byte slice the FFI handed us, or abort with a clear
/// message describing what was malformed.
unsafe fn bytes_to_str<'a>(label: &str, ptr: *const u8, len: usize) -> &'a str {
    if ptr.is_null() {
        eprintln!("coddl: null {label} pointer");
        std::process::abort();
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    match std::str::from_utf8(slice) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("coddl: invalid UTF-8 in {label}: {err}");
            std::process::abort();
        }
    }
}

/// Get a connection for `path`, opening one if not yet present.
/// Connections are opened read-only via `SQLITE_OPEN_READ_ONLY` so
/// hand-edits to the database between materialization and reads can't
/// corrupt the in-memory snapshot. Aborts on open failure with a
/// message naming the path.
fn ensure_connection(path: &str) -> () {
    let mut guard = db_connections().lock().expect("conn map poisoned");
    if guard.contains_key(path) {
        return;
    }
    let mut conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("coddl: cannot open SQLite database `{path}`: {err}");
            std::process::abort();
        }
    };
    // Audit every statement executed on this connection. Installed here — on
    // every connection the runtime mints, before it's cached — so one hook
    // captures every query path with no per-call-site plumbing. The legacy
    // `trace` callback delivers the *expanded* SQL (bound values inlined):
    // handy for self-audit, but it can leak PII/secrets from filter values.
    conn.trace(Some(audit_sqlite_trace));
    guard.insert(path.to_string(), conn);
}

/// `rusqlite` trace callback. It is a bare `fn` (not a closure — it cannot
/// capture state), so it reaches the backend-agnostic audit sink through the
/// process globals in [`crate::audit`]. Forwards every executed statement
/// with the `sqlite` label.
fn audit_sqlite_trace(sql: &str) {
    crate::audit::record("sqlite", sql);
}

/// Begin a transaction. v1 is a no-op: the materialized in-memory
/// slot is the source of truth for reads, and SQLite isn't touched
/// inside the transaction body. Real BEGIN ships with write-through.
#[no_mangle]
pub unsafe extern "C" fn coddl_begin_tx() -> CoddlStatus {
    CoddlStatus::Ok
}

/// Commit a transaction. v1 no-op (see [`coddl_begin_tx`]).
#[no_mangle]
pub unsafe extern "C" fn coddl_commit_tx() -> CoddlStatus {
    CoddlStatus::Ok
}

/// Roll back a transaction. v1 no-op; reserved for the
/// serialization-replay loop that lands with write-through and sum
/// types in the language.
#[no_mangle]
pub unsafe extern "C" fn coddl_rollback_tx() -> CoddlStatus {
    CoddlStatus::Ok
}

/// Close every open SQLite connection. Called from
/// `coddl_runtime_shutdown` as the last act of the program.
///
/// Slot release is the codegen's responsibility — every `main` that
/// materializes a public relvar emits a paired `Inst::RelvarSlotRelease`
/// before the runtime shutdown call. By the time we get here the slots
/// are already at rc=0; re-releasing would double-free.
pub unsafe fn shutdown_storage() {
    let mut slots = relvar_slots().lock().expect("slot map poisoned");
    slots.clear();
    let mut conns = db_connections().lock().expect("conn map poisoned");
    conns.clear(); // drops every Connection (closes the SQLite handles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn resolve_op_field_returns_default_on_miss() {
        unsafe {
            let env = b"CODDL_DOESNOTEXIST_XYZ";
            let default = b"/tmp/fallback.sqlite";
            let mut out_len: usize = 0;
            let p = coddl_resolve_op_field(
                env.as_ptr(),
                env.len(),
                default.as_ptr(),
                default.len(),
                &mut out_len,
            );
            assert_eq!(p, default.as_ptr());
            assert_eq!(out_len, default.len());
        }
    }

    #[test]
    fn resolve_op_field_returns_env_on_hit() {
        unsafe {
            std::env::set_var("CODDL_TESTRESOLVE_FILE", "/srv/x.sqlite");
            let env = b"CODDL_TESTRESOLVE_FILE";
            let default = b"/tmp/fallback.sqlite";
            let mut out_len: usize = 0;
            let p = coddl_resolve_op_field(
                env.as_ptr(),
                env.len(),
                default.as_ptr(),
                default.len(),
                &mut out_len,
            );
            assert!(!p.is_null());
            assert_ne!(p, default.as_ptr());
            assert_eq!(out_len, "/srv/x.sqlite".len());
            std::env::remove_var("CODDL_TESTRESOLVE_FILE");
        }
    }

    #[test]
    fn tx_externs_are_no_ops_returning_ok() {
        unsafe {
            assert_eq!(coddl_begin_tx(), CoddlStatus::Ok);
            assert_eq!(coddl_commit_tx(), CoddlStatus::Ok);
            assert_eq!(coddl_rollback_tx(), CoddlStatus::Ok);
        }
    }

    #[test]
    fn sqlite_relvar_init_round_trips_one_row() {
        // Build a tempfile SQLite with a single greeting row, drive
        // the materializer against it, then read back the slot pointer
        // and check the RC header.
        use crate::rc::CoddlRcHeader;
        use rusqlite::params;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let path_str = path.to_string_lossy().to_string();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL)",
                params![],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO greetings (id, message) VALUES (?1, ?2)",
                params![1i64, "hello world"],
            )
            .unwrap();
        }

        // Build a heading descriptor matching `{id: Integer, message: Text}`
        // in canonical (sorted) attr-name order: id, message.
        let id_name = b"id";
        let message_name = b"message";
        let attrs = [
            crate::relation::CoddlAttrDesc {
                name: id_name.as_ptr(),
                name_len: id_name.len() as u32,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
            },
            crate::relation::CoddlAttrDesc {
                name: message_name.as_ptr(),
                name_len: message_name.len() as u32,
                kind: CoddlAttrKind::Text as u32,
                offset: 8, // Integer = 8 bytes
            },
        ];
        let record_size = 8 /*id*/ + 16 /*text*/;
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size,
            attrs: attrs.as_ptr(),
        };

        // Slot global the materializer writes into.
        let mut slot: *mut u8 = ptr::null_mut();

        let id_col = b"id";
        let msg_col = b"message";
        let col_ptrs: [*const u8; 2] = [id_col.as_ptr(), msg_col.as_ptr()];
        let col_lens: [usize; 2] = [id_col.len(), msg_col.len()];

        let table_name = b"greetings";
        let relvar_name = b"Greetings";
        let status = unsafe {
            coddl_sqlite_relvar_init(
                relvar_name.as_ptr(),
                relvar_name.len(),
                path_str.as_ptr(),
                path_str.len(),
                table_name.as_ptr(),
                table_name.len(),
                col_ptrs.as_ptr(),
                col_lens.as_ptr(),
                2,
                &desc,
                &mut slot,
            )
        };
        assert_eq!(status, CoddlStatus::Ok);
        assert!(!slot.is_null());

        // The header should report 1 row.
        let header = unsafe { &*(slot.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader) };
        assert_eq!(header.length, 1);
        assert_eq!(header.rc, 1);

        // Release via the runtime shutdown path so we exercise the
        // slot-cleanup hook too.
        unsafe { shutdown_storage() };
    }
}

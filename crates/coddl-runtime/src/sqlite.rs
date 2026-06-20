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

use rusqlite::{params_from_iter, Connection, OpenFlags};

use crate::rc::{coddl_rc_alloc, CoddlKind};
use crate::relation::{CoddlAttrDesc, CoddlAttrKind, CoddlHeadingDesc};
use crate::{CoddlStatus, PlanId};

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

/// One registered logical database. By TTM a database binds exactly one
/// backend and is the scope of a transaction, so this is 1:1 with a
/// connection. v1 carries only the resolved file path; `backend_kind` /
/// credentials slot in here when a second backend or write-through lands.
/// Long-term the live `Connection` folds into this entry; today the entry
/// resolves a name to a path and the existing path-keyed [`db_connections`]
/// pool still owns the connection, so the legacy materialization path is
/// untouched.
struct DbEntry {
    path: String,
}

/// Logical-database registry, keyed by the `database <name>;` handle (e.g.
/// `greetings`). Populated in the program prologue by
/// [`coddl_register_database`]; read by [`coddl_query`] to find the
/// connection a plan runs against.
fn database_registry() -> &'static Mutex<HashMap<String, DbEntry>> {
    static DBS: std::sync::OnceLock<Mutex<HashMap<String, DbEntry>>> = std::sync::OnceLock::new();
    DBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One registered query plan: the SQL text codegen baked from RelIR, the
/// logical database it targets, the number of bind parameters, and a pointer
/// to the result heading descriptor (stored as `usize` so the map stays
/// `Send + Sync` — the descriptor is a codegen-emitted static, valid for the
/// program's life, same discipline as [`relvar_slots`]).
struct PlanEntry {
    db_name: String,
    sql: String,
    param_count: u32,
    desc: usize,
}

/// Plan registry, keyed by the codegen-assigned [`PlanId`] (a dense `u32`,
/// its own namespace — **not** `coddl_sqlemit::PlanId`, which is a 64-bit
/// text hash). Populated by [`coddl_register_plan`]; read by [`coddl_query`].
fn plan_registry() -> &'static Mutex<HashMap<u32, PlanEntry>> {
    static PLANS: std::sync::OnceLock<Mutex<HashMap<u32, PlanEntry>>> = std::sync::OnceLock::new();
    PLANS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A bind parameter crossing the FFI boundary into [`coddl_query`].
///
/// `#[repr(C)]`, fields ordered so there is no interior padding hole (the
/// `u32` tag is last). This layout is the single source of truth the codegen
/// layer mirrors when it builds the parameter array. `kind` is a
/// [`CoddlAttrKind`] discriminant: `Integer`/`Boolean` carry their value in
/// `i` (Boolean as 0/1), `Text` carries `(ptr, len)`. Any other kind aborts.
#[repr(C)]
pub struct CoddlParam {
    pub i: i64,
    pub ptr: *const u8,
    pub len: usize,
    pub kind: u32,
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

    // Marshal rows. The row-stepping loop and the alloc/finalize step are
    // shared verbatim with `coddl_query` via [`marshal_rows`] /
    // [`finalize_relation`] so the canonical record layout and NULL-rejection
    // (RM Pro 4) live in exactly one place. The table's rows are already a set
    // (unique by the relvar's key), so `finalize_relation` does not dedup/seal.
    // `ctx` parameterizes the abort messages so this path keeps its relvar-named
    // diagnostics.
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let record_size = (*desc).record_size as usize;
    let ctx = MarshalCtx {
        site: "sqlite_relvar_init",
        step_subject: format!("relvar `{relvar}`"),
        of_subject: format!("`{relvar}`"),
    };

    let row_buffers: Vec<Vec<u8>> = {
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
        marshal_rows(&mut rows, attrs, record_size, &ctx)
    };

    let _ = conn_payload;

    let payload = finalize_relation(&row_buffers, record_size, desc, &ctx);

    *slot = payload;
    relvar_slots()
        .lock()
        .expect("slot map poisoned")
        .insert(relvar.to_string(), payload as usize);
    CoddlStatus::Ok
}

/// Abort-message context for [`marshal_rows`] / [`finalize_relation`]. Lets
/// the shared marshalling name its source differently per caller: the legacy
/// relvar path keeps its byte-identical "relvar `X`" diagnostics, while the
/// query path names the plan.
struct MarshalCtx {
    /// Call-site label in the `coddl: <site>: ...` message prefix.
    site: &'static str,
    /// Subject in "...failed for {step_subject}" (row-step / allocation).
    step_subject: String,
    /// Subject in "column `X` of {of_subject}" (per-cell decode).
    of_subject: String,
}

/// Step every row of `rows` and decode its cells into fixed-stride record
/// buffers, driven by the result heading's per-attribute kind. Shared by
/// `coddl_sqlite_relvar_init` and [`coddl_query`] so the canonical record
/// layout lives in one place. Aborts on a row-step error, a per-cell type
/// mismatch, a NULL cell (RM Pro 4 — D has no nulls), or an unsupported kind;
/// these are schema/codegen bugs, not recoverable conditions.
///
/// # Safety
/// `rows` must be a live cursor over a statement whose SELECT list lines up
/// positionally with `attrs` (heading-canonical order); each record is exactly
/// `record_size` bytes. Interned Text bytes live for the program's lifetime.
unsafe fn marshal_rows(
    rows: &mut rusqlite::Rows,
    attrs: &[CoddlAttrDesc],
    record_size: usize,
    ctx: &MarshalCtx,
) -> Vec<Vec<u8>> {
    let mut row_buffers: Vec<Vec<u8>> = Vec::new();
    loop {
        let row = match rows.next() {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(err) => {
                eprintln!(
                    "coddl: {}: row step failed for {}: {err}",
                    ctx.site, ctx.step_subject
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
                            "coddl: {}: column `{attr_name}` of {} is not Integer (or is NULL): {err}",
                            ctx.site, ctx.of_subject
                        );
                        std::process::abort();
                    }
                };
                buf[offset..offset + 8].copy_from_slice(&v.to_ne_bytes());
            } else if kind == CoddlAttrKind::Boolean as u32 {
                let v: bool = match row.get(i) {
                    Ok(v) => v,
                    Err(err) => {
                        let attr_name = read_attr_name(attr);
                        eprintln!(
                            "coddl: {}: column `{attr_name}` of {} is not Boolean (or is NULL): {err}",
                            ctx.site, ctx.of_subject
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
                            "coddl: {}: column `{attr_name}` of {} is not Text (or is NULL): {err}",
                            ctx.site, ctx.of_subject
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
                    "coddl: {}: unsupported attr kind {kind} for column `{attr_name}` of {}",
                    ctx.site, ctx.of_subject
                );
                std::process::abort();
            }
        }
        row_buffers.push(buf);
    }
    row_buffers
}

/// Allocate an RC relation and copy the marshalled record buffers in. The rows
/// come straight from the backend, which already hands back a duplicate-free
/// set — the query carries `SELECT DISTINCT`, or `needs_distinct()` elided it
/// only because a surviving key guarantees uniqueness — so there is nothing to
/// dedup in process. We deliberately do **not** seal here: a relation is a set
/// with no tuple order (RM Pro 1), so the backend's row order is left as-is and
/// is not made canonical. (Sealing would re-sort + re-dedup purely for an order
/// nothing consumes — the printer emits whatever order it finds, `extract`
/// works on a cardinality-1 relation, and relation `=` is observational, not a
/// sorted-payload memcmp.) Returns the payload pointer (rc=1, kind=Relation).
/// The caller decides whether to stash it in a relvar slot (init) or return it
/// as a transient handle (query). Aborts on allocation failure.
///
/// # Safety
/// `desc` must outlive the returned relation (a codegen-emitted static); each
/// buffer in `row_buffers` must be exactly `record_size` bytes.
unsafe fn finalize_relation(
    row_buffers: &[Vec<u8>],
    record_size: usize,
    desc: *const CoddlHeadingDesc,
    ctx: &MarshalCtx,
) -> *mut u8 {
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
            "coddl: {}: allocation failed for {} ({row_count} rows × {record_size} bytes)",
            ctx.site, ctx.step_subject
        );
        std::process::abort();
    }
    let dest = std::slice::from_raw_parts_mut(payload, payload_size);
    for (i, row) in row_buffers.iter().enumerate() {
        let start = i * record_size;
        dest[start..start + record_size].copy_from_slice(row);
    }
    payload
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

/// Register a logical database: bind a `database <name>;` handle to its
/// resolved connection path. Called once per database in the program prologue,
/// after codegen has resolved the path via [`coddl_resolve_op_field`]. A repeat
/// registration overwrites (idempotent — the resolved path is stable). By TTM
/// a database binds exactly one backend, so this entry is 1:1 with a connection.
///
/// # Safety
/// Both (ptr, len) pairs must describe valid UTF-8 for their lengths.
#[no_mangle]
pub unsafe extern "C" fn coddl_register_database(
    name: *const u8,
    name_len: usize,
    path: *const u8,
    path_len: usize,
) -> CoddlStatus {
    let name = bytes_to_str("database name", name, name_len);
    let path = bytes_to_str("database path", path, path_len);
    database_registry()
        .lock()
        .expect("database registry poisoned")
        .insert(
            name.to_string(),
            DbEntry {
                path: path.to_string(),
            },
        );
    CoddlStatus::Ok
}

/// Register a static query plan: the SQL codegen baked from a relvar-rooted
/// RelIR subtree, the logical database it runs against, its bind-parameter
/// count, and the result heading descriptor. Called once per plan in the
/// program prologue. Aborts on a null descriptor or a duplicate `plan_id`
/// (static plan ids are unique by construction — a collision is a codegen bug).
///
/// # Safety
/// `result_desc` must point to a heading descriptor that lives for the
/// program's lifetime (a codegen-emitted static). The (ptr, len) pairs must
/// describe valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn coddl_register_plan(
    plan_id: PlanId,
    db_name: *const u8,
    db_name_len: usize,
    sql: *const u8,
    sql_len: usize,
    param_count: u32,
    result_desc: *const CoddlHeadingDesc,
) -> CoddlStatus {
    if result_desc.is_null() {
        eprintln!(
            "coddl: register_plan: null result descriptor for plan {}",
            plan_id.0
        );
        std::process::abort();
    }
    let db_name = bytes_to_str("plan database name", db_name, db_name_len);
    let sql = bytes_to_str("plan SQL", sql, sql_len);
    let mut registry = plan_registry().lock().expect("plan registry poisoned");
    if registry.contains_key(&plan_id.0) {
        eprintln!(
            "coddl: register_plan: plan_id {} already registered",
            plan_id.0
        );
        std::process::abort();
    }
    registry.insert(
        plan_id.0,
        PlanEntry {
            db_name: db_name.to_string(),
            sql: sql.to_string(),
            param_count,
            desc: result_desc as usize,
        },
    );
    CoddlStatus::Ok
}

/// Execute a registered plan with the given bind parameters and return a fresh
/// sealed RC `Relation` (rc=1) — the same payload shape [`coddl_relation_where`]
/// returns and [`coddl_extract_check_cardinality`] consumes. Fire-on-call: the
/// statement runs now, lazily, at the force point. The result is a transient
/// handle the caller releases via `coddl_rc_release` (it is *not* a relvar slot).
///
/// Runs on a connection minted by [`ensure_connection`], so the audit `trace`
/// hook captures the statement. Aborts on any hard error (unknown plan
/// or database, parameter-count or kind mismatch, prepare/step failure, NULL
/// cell) — these are codegen/schema bugs, and the `*Relation` return type has
/// no status channel.
///
/// # Safety
/// `plan_id` must have been registered by [`coddl_register_plan`]. `params`
/// must point to `n` valid [`CoddlParam`] values (or be null when `n == 0`).
#[no_mangle]
pub unsafe extern "C" fn coddl_query(
    plan_id: PlanId,
    params: *const CoddlParam,
    n: usize,
) -> *mut u8 {
    // Look up the plan; clone what we need and drop the registry lock before
    // taking any other lock (no lock-order coupling).
    let (db_name, sql, param_count, desc_addr) = {
        let registry = plan_registry().lock().expect("plan registry poisoned");
        match registry.get(&plan_id.0) {
            Some(entry) => (
                entry.db_name.clone(),
                entry.sql.clone(),
                entry.param_count,
                entry.desc,
            ),
            None => {
                eprintln!("coddl: query: no plan registered for plan_id {}", plan_id.0);
                std::process::abort();
            }
        }
    };
    let desc = desc_addr as *const CoddlHeadingDesc;

    if n != param_count as usize {
        eprintln!(
            "coddl: query: plan {} expects {param_count} param(s), got {n}",
            plan_id.0
        );
        std::process::abort();
    }

    // Resolve the plan's logical database to its connection path.
    let path = {
        let registry = database_registry()
            .lock()
            .expect("database registry poisoned");
        match registry.get(&db_name) {
            Some(entry) => entry.path.clone(),
            None => {
                eprintln!(
                    "coddl: query: plan {} references unregistered database `{db_name}`",
                    plan_id.0
                );
                std::process::abort();
            }
        }
    };

    // Open (or reuse) the connection. This locks the pool internally, so it
    // must run *before* we re-lock it below — the Mutex is not reentrant.
    ensure_connection(&path);

    // Bind parameters: CoddlParam -> owned rusqlite Value. Text bytes are
    // copied, so nothing borrows the caller's array past this point.
    let param_slice: &[CoddlParam] = if params.is_null() || n == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(params, n)
    };
    let bindings: Vec<rusqlite::types::Value> = param_slice
        .iter()
        .map(|p| param_to_sqlite(p, plan_id.0))
        .collect();

    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let record_size = (*desc).record_size as usize;
    let ctx = MarshalCtx {
        site: "query",
        step_subject: format!("plan {}", plan_id.0),
        of_subject: format!("plan {}", plan_id.0),
    };

    // Fire the prepared statement and marshal its rows under one pool guard.
    // `prepare_cached` keys by SQL text per connection, so repeat queries of
    // the same plan reuse the compiled statement. The CachedStatement / Rows /
    // &Connection / guard are all frame-local and drop at the block's end.
    let row_buffers = {
        let conn_guard = db_connections().lock().expect("conn map poisoned");
        let conn = conn_guard
            .get(&path)
            .expect("connection inserted by ensure_connection");
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("coddl: query: prepare failed for plan {}: {err}", plan_id.0);
                std::process::abort();
            }
        };
        let mut rows = match stmt.query(params_from_iter(bindings.iter())) {
            Ok(r) => r,
            Err(err) => {
                eprintln!(
                    "coddl: query: execution failed for plan {}: {err}",
                    plan_id.0
                );
                std::process::abort();
            }
        };
        marshal_rows(&mut rows, attrs, record_size, &ctx)
    };

    finalize_relation(&row_buffers, record_size, desc, &ctx)
}

/// Lower one [`CoddlParam`] to an owned rusqlite bind value. Boolean binds as
/// the 0/1 integer SQLite stores it as; Text copies its bytes so the bound
/// value owns them. Aborts on an unsupported kind or non-UTF-8 Text.
///
/// # Safety
/// For a Text param, `(ptr, len)` must describe valid bytes for the call.
unsafe fn param_to_sqlite(p: &CoddlParam, plan_id: u32) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    if p.kind == CoddlAttrKind::Integer as u32 || p.kind == CoddlAttrKind::Boolean as u32 {
        Value::Integer(p.i)
    } else if p.kind == CoddlAttrKind::Text as u32 {
        if p.ptr.is_null() {
            return Value::Text(String::new());
        }
        let slice = std::slice::from_raw_parts(p.ptr, p.len);
        match std::str::from_utf8(slice) {
            Ok(s) => Value::Text(s.to_string()),
            Err(err) => {
                eprintln!("coddl: query: plan {plan_id}: invalid UTF-8 in Text parameter: {err}");
                std::process::abort();
            }
        }
    } else {
        eprintln!(
            "coddl: query: plan {plan_id}: unsupported parameter kind {}",
            p.kind
        );
        std::process::abort();
    }
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
    plan_registry()
        .lock()
        .expect("plan registry poisoned")
        .clear();
    database_registry()
        .lock()
        .expect("database registry poisoned")
        .clear();
    let mut conns = db_connections().lock().expect("conn map poisoned");
    conns.clear(); // drops every Connection (closes the SQLite handles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    /// Serialize tests that touch the process-global connection pool / plan /
    /// database registries, so one test's `shutdown_storage()` can't clear
    /// another's state mid-run. Poison-tolerant so a panicking test doesn't
    /// wedge the rest.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The `{id: Integer, message: Text}` heading in canonical order. Names are
    /// `'static` byte literals, so the returned descriptors stay valid after the
    /// array moves to the caller (who binds it and takes `.as_ptr()`).
    fn greetings_attrs() -> [CoddlAttrDesc; 2] {
        [
            CoddlAttrDesc {
                name: b"id".as_ptr(),
                name_len: 2,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
            },
            CoddlAttrDesc {
                name: b"message".as_ptr(),
                name_len: 7,
                kind: CoddlAttrKind::Text as u32,
                offset: 8,
            },
        ]
    }

    /// Seed a temp `.sqlite` with a two-row `greetings` table. Two rows let the
    /// query tests prove the `WHERE` filter ran: a missing/ignored predicate
    /// would return both rows. Returns the tempfile (keep it alive) and its path.
    fn seed_two_row_greetings() -> (tempfile::NamedTempFile, String) {
        use rusqlite::params;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path_str = tmp.path().to_string_lossy().to_string();
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute(
            "CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL)",
            params![],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO greetings (id, message) VALUES (1, 'hello world'), (2, 'goodbye')",
            params![],
        )
        .unwrap();
        (tmp, path_str)
    }

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

        let _g = test_guard();

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

    #[test]
    fn register_then_query_filters_to_one_row() {
        use crate::rc::{coddl_rc_release, CoddlRcHeader};

        let _g = test_guard();
        let (_tmp, path_str) = seed_two_row_greetings();
        let attrs = greetings_attrs();
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 24,
            attrs: attrs.as_ptr(),
        };
        let db = b"greetings";
        let sql = br#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?"#;

        unsafe {
            assert_eq!(
                coddl_register_database(db.as_ptr(), db.len(), path_str.as_ptr(), path_str.len()),
                CoddlStatus::Ok
            );
            assert_eq!(
                coddl_register_plan(
                    PlanId(0),
                    db.as_ptr(),
                    db.len(),
                    sql.as_ptr(),
                    sql.len(),
                    1,
                    &desc,
                ),
                CoddlStatus::Ok
            );

            let param = CoddlParam {
                i: 1,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let rel = coddl_query(PlanId(0), &param, 1);
            assert!(!rel.is_null());

            // One row back — the WHERE filtered out id=2 — sealed RC relation.
            let header = &*(rel.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader);
            assert_eq!(header.length, 1);
            assert_eq!(header.rc, 1);
            assert_eq!(header.kind, CoddlKind::Relation as u32);

            // Record: id (i64) at offset 0, message (ptr, len) at offsets 8/16.
            let id = ptr::read(rel as *const i64);
            assert_eq!(id, 1);
            let msg_ptr = usize::from_ne_bytes(
                std::slice::from_raw_parts(rel.add(8), 8).try_into().unwrap(),
            ) as *const u8;
            let msg_len = usize::from_ne_bytes(
                std::slice::from_raw_parts(rel.add(16), 8).try_into().unwrap(),
            );
            let msg = std::str::from_utf8(std::slice::from_raw_parts(msg_ptr, msg_len)).unwrap();
            assert_eq!(msg, "hello world");

            coddl_rc_release(rel);
            shutdown_storage();
        }
    }

    #[test]
    fn query_empty_result_is_zero_length() {
        use crate::rc::{coddl_rc_release, CoddlRcHeader};

        let _g = test_guard();
        let (_tmp, path_str) = seed_two_row_greetings();
        let attrs = greetings_attrs();
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 24,
            attrs: attrs.as_ptr(),
        };
        let db = b"greetings";
        let sql = br#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?"#;

        unsafe {
            coddl_register_database(db.as_ptr(), db.len(), path_str.as_ptr(), path_str.len());
            coddl_register_plan(
                PlanId(0),
                db.as_ptr(),
                db.len(),
                sql.as_ptr(),
                sql.len(),
                1,
                &desc,
            );

            // No row has id = 99 → an empty (length-0) relation, not an abort.
            let param = CoddlParam {
                i: 99,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let rel = coddl_query(PlanId(0), &param, 1);
            assert!(!rel.is_null());
            let header = &*(rel.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader);
            assert_eq!(header.length, 0);

            coddl_rc_release(rel);
            shutdown_storage();
        }
    }

    #[test]
    fn query_reuses_prepared_statement() {
        use crate::rc::{coddl_rc_release, CoddlRcHeader};

        let _g = test_guard();
        let (_tmp, path_str) = seed_two_row_greetings();
        let attrs = greetings_attrs();
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 24,
            attrs: attrs.as_ptr(),
        };
        let db = b"greetings";
        let sql = br#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?"#;

        unsafe {
            coddl_register_database(db.as_ptr(), db.len(), path_str.as_ptr(), path_str.len());
            coddl_register_plan(
                PlanId(0),
                db.as_ptr(),
                db.len(),
                sql.as_ptr(),
                sql.len(),
                1,
                &desc,
            );

            // Two queries through the same plan hit rusqlite's prepared-statement
            // cache on the second call; both must return the right single row.
            let p1 = CoddlParam {
                i: 1,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let r1 = coddl_query(PlanId(0), &p1, 1);
            assert_eq!((*(r1.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader)).length, 1);
            assert_eq!(ptr::read(r1 as *const i64), 1);
            coddl_rc_release(r1);

            let p2 = CoddlParam {
                i: 2,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let r2 = coddl_query(PlanId(0), &p2, 1);
            assert_eq!((*(r2.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader)).length, 1);
            assert_eq!(ptr::read(r2 as *const i64), 2);
            coddl_rc_release(r2);

            shutdown_storage();
        }
    }
}

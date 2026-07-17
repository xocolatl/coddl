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
//! ## Transactions
//!
//! TTM OO Pre 4 forbids autocommit at the language surface; the compiler
//! enforces this by wrapping every `transaction [...]` body in synthetic
//! [`coddl_begin_tx`] / [`coddl_commit_tx`] calls. These issue real `BEGIN` /
//! `COMMIT` (and [`coddl_rollback_tx`] a `ROLLBACK`) on every open connection,
//! guarded by a process-global depth counter so nested blocks don't
//! double-`BEGIN`. Connections are opened read-write and kept live, so a write
//! made inside a transaction (via [`coddl_exec`]) is visible to a later read
//! ([`coddl_query`]) in the same transaction — read-after-write on one shared
//! connection. Serialization-conflict replay will reuse the same externs.

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
    static SLOTS: std::sync::OnceLock<Mutex<HashMap<String, usize>>> = std::sync::OnceLock::new();
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
/// logical database it targets, the number of bind placeholders, the spec of
/// each relation-valued parameter (one per `__CODDL_REL_<slot>__` marker in
/// the text, in slot order), a pointer to the result heading descriptor
/// (stored as `usize` so the map stays `Send + Sync` — the descriptor is a
/// codegen-emitted static, valid for the program's life, same discipline as
/// [`relvar_slots`]), and the cardinality-1 sibling linkage.
struct PlanEntry {
    db_name: String,
    sql: String,
    param_count: u32,
    rel_specs: Vec<RelSpec>,
    /// The 0-based indices of the scalar binds that are `when`-gate
    /// conditions on the absorbing spine — a false bind at any of them makes
    /// the result provably empty, so [`coddl_query`] returns a fresh empty
    /// relation without firing a statement (the false-gate statement skip).
    /// Validated `< param_count` at registration.
    gate_params: Vec<u32>,
    desc: usize,
    /// Plan id of the cardinality-1 sibling — the specialized `WHERE shared
    /// = ?N…` form [`coddl_query`] fires instead of this plan when the
    /// dispatch slot's relation holds exactly one row, binding the row's
    /// cells after the scalar params.
    card1_alt: Option<u32>,
    /// Which relation-parameter slot's cardinality drives the dispatch.
    dispatch_slot: u32,
}

/// One relation-valued parameter's registration spec (the decoded half of
/// the codegen's interleaved `[arity, flags]` pair array).
#[derive(Clone, Copy)]
struct RelSpec {
    /// Leaf-column count the bound relation must have.
    arity: u32,
    /// Whether an empty relation at this slot makes the whole result
    /// provably empty — [`coddl_query`] then returns a fresh empty relation
    /// without firing a statement (the empty-slot short-circuit).
    absorbs_empty: bool,
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

/// A **relation-valued** bind parameter crossing the FFI boundary into
/// [`coddl_query`]: an in-memory relation payload plus its static heading
/// descriptor (the same `(src, desc)` pair [`coddl_exec_insert`] takes). The
/// runtime expands the plan's matching `__CODDL_REL_<slot>__` marker with the
/// relation's rows — a `(VALUES …)` table primary whose cells bind numbered
/// after the plan's scalar parameters, a typed zero-row `SELECT` when the
/// relation is empty, or a stable per-(plan, slot) session temp table when
/// the projected binds pass the ceiling ([`fire_escalated`]). `#[repr(C)]`:
/// two pointers, no padding; codegen mirrors this layout when it builds the
/// array.
#[repr(C)]
pub struct CoddlRelParam {
    pub src: *const u8,
    pub desc: *const CoddlHeadingDesc,
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
                eprintln!("coddl: sqlite_relvar_init: prepare failed for relvar `{relvar}`: {err}");
                std::process::abort();
            }
        };
        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("coddl: sqlite_relvar_init: query failed for relvar `{relvar}`: {err}");
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

/// Flatten a heading descriptor to its leaf cells as `(attr, absolute_offset)`,
/// recursing into `Tuple` cells (a pushed `wrap` result) and accumulating the
/// base offset. Scalar/Text cells are leaves. The order is depth-first in the
/// descriptor's (name-sorted) attr order — matching `record_layout`'s leaf order
/// and the pushed SELECT's column order, so the positional column→cell mapping
/// holds.
///
/// # Safety
/// `attrs` must be a valid attr slice; any `Tuple` cell's `sub` must be null or a
/// valid descriptor pointer.
unsafe fn flatten_query_leaves<'a>(
    attrs: &'a [CoddlAttrDesc],
    base: usize,
    out: &mut Vec<(&'a CoddlAttrDesc, usize)>,
) {
    for a in attrs {
        let off = base + a.offset as usize;
        if a.kind == CoddlAttrKind::Tuple as u32 {
            if !a.sub.is_null() {
                let sub = &*a.sub;
                let sub_attrs = std::slice::from_raw_parts(sub.attrs, sub.attr_count as usize);
                flatten_query_leaves(sub_attrs, off, out);
            }
        } else {
            out.push((a, off));
        }
    }
}

/// Step every row of `rows` and decode its cells into fixed-stride record
/// buffers, driven by the result heading's per-attribute kind (flattened to
/// leaves, so a `Tuple` cell's components are read from consecutive columns).
/// Shared by `coddl_sqlite_relvar_init` and [`coddl_query`] so the canonical
/// record layout lives in one place. Aborts on a row-step error, a per-cell type
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
    // Flatten the descriptor to leaf cells once. The SELECT returns the flat
    // leaf columns in `record_layout`'s depth-first order, so the i-th result
    // column is the i-th leaf, written at its absolute record offset. A `Tuple`
    // cell (from a pushed `wrap`) is an inline sub-region — recurse into its
    // sub-descriptor, accumulating the base offset.
    let mut leaves: Vec<(&CoddlAttrDesc, usize)> = Vec::new();
    flatten_query_leaves(attrs, 0, &mut leaves);
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
        for (i, &(attr, offset)) in leaves.iter().enumerate() {
            let kind = attr.kind;
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
            } else if kind == CoddlAttrKind::Approximate as u32 {
                // SQLite has no NaN storage — it *encodes* the Approximate NaN
                // value as SQL NULL. So a NULL here decodes back to NaN; a real
                // value stores its canonical bits (`−0` → `+0`). This is an
                // encoding of a value, not a Coddl null: the relvar is total
                // (RM Pro 4), and NULL is just SQLite's byte-pattern for NaN.
                let v: Option<f64> = match row.get(i) {
                    Ok(v) => v,
                    Err(err) => {
                        let attr_name = read_attr_name(attr);
                        eprintln!(
                            "coddl: {}: column `{attr_name}` of {} is not Approximate/REAL: {err}",
                            ctx.site, ctx.of_subject
                        );
                        std::process::abort();
                    }
                };
                let bits = match v {
                    None => f64::NAN.to_bits(), // SQLite NULL ⇒ our NaN
                    Some(x) if x == 0.0 => 0,   // collapse ±0
                    // SQLite can't return a NaN REAL (it stored it as NULL), so
                    // `Some` is always finite or ±Inf here.
                    Some(x) => x.to_bits(),
                };
                buf[offset..offset + 8].copy_from_slice(&bits.to_ne_bytes());
            } else if kind == CoddlAttrKind::Character as u32 {
                // Stored as an integer codepoint; read it into the 8-byte cell
                // exactly like Integer (the printer decodes it back to a char).
                let v: i64 = match row.get(i) {
                    Ok(v) => v,
                    Err(err) => {
                        let attr_name = read_attr_name(attr);
                        eprintln!(
                            "coddl: {}: column `{attr_name}` of {} is not Character/Integer (or is NULL): {err}",
                            ctx.site, ctx.of_subject
                        );
                        std::process::abort();
                    }
                };
                buf[offset..offset + 8].copy_from_slice(&v.to_ne_bytes());
            } else if kind == CoddlAttrKind::Rational as u32 {
                // Stored as canonical `"n/d"` TEXT; parse to the reduced
                // (numer, denom) i64 pair and write the 16-byte cell (num @ 0,
                // den @ 8). Reduce defensively so a foreign non-canonical value
                // still compares by value.
                let s: String = match row.get(i) {
                    Ok(s) => s,
                    Err(err) => {
                        let attr_name = read_attr_name(attr);
                        eprintln!(
                            "coddl: {}: column `{attr_name}` of {} is not Rational/TEXT (or is NULL): {err}",
                            ctx.site, ctx.of_subject
                        );
                        std::process::abort();
                    }
                };
                let (num, den) = parse_rational(&s).unwrap_or_else(|| {
                    eprintln!(
                        "coddl: {}: column `{}` of {} is not a canonical rational `n/d`: {s:?}",
                        ctx.site,
                        read_attr_name(attr),
                        ctx.of_subject
                    );
                    std::process::abort();
                });
                buf[offset..offset + 8].copy_from_slice(&num.to_ne_bytes());
                buf[offset + 8..offset + 16].copy_from_slice(&den.to_ne_bytes());
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
                // Allocate the cell as a refcounted heap `Text` (kind=Text,
                // rc=1) and copy the bytes in. The relation owns this rc=1
                // reference; its drop walker releases the cell when the relvar
                // slot drops. (Previously interned into a process-global static
                // that leaked and had no RC header — unsafe once the drop walker
                // releases `Text` cells.)
                let bytes = s.as_bytes();
                let n = bytes.len();
                let payload = crate::rc::coddl_rc_alloc(
                    n,
                    n as u32,
                    crate::rc::CoddlKind::Text as u32,
                    std::ptr::null(),
                );
                if n > 0 {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), payload, n);
                }
                buf[offset..offset + 8].copy_from_slice(&(payload as usize).to_ne_bytes());
                buf[offset + 8..offset + 16].copy_from_slice(&n.to_ne_bytes());
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

/// Parse a `"n/d"` rational string to its **reduced** `(numer, denom)` i64
/// pair (`gcd(|n|,d) = 1`, `d > 0`). Returns `None` on a malformed string, a
/// zero denominator, or a component that overflows i64 (a stored value outside
/// the bounded type's range is an error). Reduces defensively so a foreign
/// non-canonical value (`34/10`) still compares by value.
fn parse_rational(s: &str) -> Option<(i64, i64)> {
    let (n_str, d_str) = s.split_once('/')?;
    let n: i64 = n_str.trim().parse().ok()?;
    let d: i64 = d_str.trim().parse().ok()?;
    if d == 0 {
        return None;
    }
    if n == 0 {
        return Some((0, 1));
    }
    // gcd of magnitudes (Euclid).
    let (mut a, mut b) = (n.unsigned_abs(), d.unsigned_abs());
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    let g = a as i64;
    let (mut n, mut d) = (n / g, d / g);
    if d < 0 {
        n = -n;
        d = -d;
    }
    Some((n, d))
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
/// Connections are opened **read-write** (`SQLITE_OPEN_READ_WRITE`) so surgical
/// DML inside a transaction can write through them, and kept live so a read
/// later in the same transaction sees the uncommitted write (read-after-write
/// on one shared connection). Aborts on open failure with a message naming the
/// path.
fn ensure_connection(path: &str) -> () {
    let mut guard = db_connections().lock().expect("conn map poisoned");
    if guard.contains_key(path) {
        return;
    }
    let mut conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
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
    // Register the `coddl_rational` collation so pushed `Rational` ordering
    // (`WHERE r < ? COLLATE coddl_rational`, `ORDER BY r COLLATE ...`) sorts by
    // numeric value, not lexicographically. Equality never uses it (canonical
    // `"n/d"` text `=` already agrees with value-equality).
    if let Err(err) = conn.create_collation("coddl_rational", rational_collation) {
        eprintln!("coddl: cannot register `coddl_rational` collation on `{path}`: {err}");
        std::process::abort();
    }
    guard.insert(path.to_string(), conn);
}

/// Numeric collation over two canonical `"n/d"` rational strings — the body of
/// `COLLATE coddl_rational`. Parses each and defers to the same
/// [`crate::rational::coddl_rational_cmp`] the in-process `<` operator uses, so
/// pushed and in-process ordering agree exactly. A malformed operand (shouldn't
/// occur — the column holds canonical text) falls back to a raw byte compare.
fn rational_collation(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (parse_rational(a), parse_rational(b)) {
        (Some((n1, d1)), Some((n2, d2))) => {
            match crate::rational::coddl_rational_cmp(n1, d1, n2, d2) {
                c if c < 0 => Ordering::Less,
                c if c > 0 => Ordering::Greater,
                _ => Ordering::Equal,
            }
        }
        _ => a.cmp(b),
    }
}

/// `rusqlite` trace callback. It is a bare `fn` (not a closure — it cannot
/// capture state), so it reaches the backend-agnostic audit sink through the
/// process globals in [`crate::audit`]. Forwards every executed statement
/// with the `sqlite` label.
fn audit_sqlite_trace(sql: &str) {
    crate::audit::record("sqlite", sql);
}

/// Process-global transaction-nesting depth. The compiler wraps each
/// `transaction [...]` body in begin/commit, and those can nest; SQLite has no
/// nested `BEGIN`, so only the outermost begin issues a real `BEGIN` and the
/// outermost commit/rollback the real `COMMIT`/`ROLLBACK`.
fn tx_depth() -> &'static std::sync::atomic::AtomicUsize {
    static DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    &DEPTH
}

/// Run a transaction-control statement (`BEGIN`/`COMMIT`/`ROLLBACK`) on every
/// registered database's connection, opening it if needed. Aborts on failure —
/// a control statement that can't run is a runtime/codegen bug, not a user
/// error. An empty registry (a pure in-process program) is a clean no-op.
fn tx_broadcast(verb: &str) -> CoddlStatus {
    // Snapshot the registered paths, dropping the registry lock before taking
    // the connection-pool lock (no lock-order coupling; mirrors `coddl_query`).
    let paths: Vec<String> = {
        let registry = database_registry()
            .lock()
            .expect("database registry poisoned");
        registry.values().map(|e| e.path.clone()).collect()
    };
    for path in &paths {
        ensure_connection(path);
    }
    let guard = db_connections().lock().expect("conn map poisoned");
    for path in &paths {
        if let Some(conn) = guard.get(path) {
            if let Err(err) = conn.execute_batch(verb) {
                eprintln!("coddl: transaction: `{verb}` failed on `{path}`: {err}");
                std::process::abort();
            }
        }
    }
    CoddlStatus::Ok
}

/// Begin a transaction. Issues a real `BEGIN` on every open connection — but
/// only at the outermost nesting level (the depth counter guards against
/// SQLite's lack of nested `BEGIN`). The shared read-write connection means a
/// later read in the same transaction sees writes made here.
#[no_mangle]
pub unsafe extern "C" fn coddl_begin_tx() -> CoddlStatus {
    use std::sync::atomic::Ordering;
    if tx_depth().fetch_add(1, Ordering::SeqCst) == 0 {
        return tx_broadcast("BEGIN");
    }
    CoddlStatus::Ok
}

/// Commit the current transaction (a real `COMMIT` at the outermost level).
#[no_mangle]
pub unsafe extern "C" fn coddl_commit_tx() -> CoddlStatus {
    use std::sync::atomic::Ordering;
    let prev = tx_depth().fetch_sub(1, Ordering::SeqCst);
    debug_assert!(prev > 0, "commit_tx without a matching begin_tx");
    if prev == 1 {
        return tx_broadcast("COMMIT");
    }
    CoddlStatus::Ok
}

/// Roll back the current transaction (a real `ROLLBACK` at the outermost
/// level). Reserved for the serialization-replay loop; explicit today for
/// tests and future write-through conflict handling.
#[no_mangle]
pub unsafe extern "C" fn coddl_rollback_tx() -> CoddlStatus {
    use std::sync::atomic::Ordering;
    let prev = tx_depth().fetch_sub(1, Ordering::SeqCst);
    debug_assert!(prev > 0, "rollback_tx without a matching begin_tx");
    if prev == 1 {
        return tx_broadcast("ROLLBACK");
    }
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

/// Register a static query plan: the SQL codegen baked from a RelIR subtree,
/// the logical database it runs against, its scalar bind-parameter count, the
/// arities of its relation-valued parameters (`rel_arities` points at
/// `n_rels` `u32`s, in slot order; null when `n_rels == 0`), and the result
/// heading descriptor. Called once per plan in the program prologue. Aborts
/// on a null descriptor or a duplicate `plan_id` (static plan ids are unique
/// by construction — a collision is a codegen bug).
///
/// # Safety
/// `result_desc` must point to a heading descriptor that lives for the
/// program's lifetime (a codegen-emitted static). The (ptr, len) pairs must
/// describe valid UTF-8; `rel_specs` must be valid for `2 * n_rels` `u32`
/// reads (interleaved `[arity, flags]` pairs, flags bit 0 = absorbs_empty);
/// `gate_params` must be valid for `n_gates` `u32` reads (0-based indices of
/// the skip-eligible gate binds; null when `n_gates == 0`).
/// `card1_alt` is the sibling's dense plan id, or −1 for none.
#[no_mangle]
pub unsafe extern "C" fn coddl_register_plan(
    plan_id: PlanId,
    db_name: *const u8,
    db_name_len: usize,
    sql: *const u8,
    sql_len: usize,
    param_count: u32,
    rel_specs: *const u32,
    n_rels: usize,
    result_desc: *const CoddlHeadingDesc,
    card1_alt: i64,
    dispatch_slot: u32,
    gate_params: *const u32,
    n_gates: usize,
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
    let rel_specs: Vec<RelSpec> = if rel_specs.is_null() || n_rels == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(rel_specs, n_rels * 2)
            .chunks_exact(2)
            .map(|pair| RelSpec {
                arity: pair[0],
                absorbs_empty: pair[1] & 1 != 0,
            })
            .collect()
    };
    let gate_params: Vec<u32> = if gate_params.is_null() || n_gates == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(gate_params, n_gates).to_vec()
    };
    // A gate index names one of the plan's scalar binds; out of range is a
    // codegen bug, and validating here lets `coddl_query` index unchecked.
    if let Some(bad) = gate_params.iter().find(|&&i| i >= param_count) {
        eprintln!(
            "coddl: register_plan: plan {} gate index {bad} out of range \
             ({param_count} scalar param(s))",
            plan_id.0
        );
        std::process::abort();
    }
    let card1_alt = match card1_alt {
        -1 => None,
        id if id >= 0 && id <= u32::MAX as i64 => Some(id as u32),
        other => {
            eprintln!(
                "coddl: register_plan: plan {} has out-of-range card1_alt {other}",
                plan_id.0
            );
            std::process::abort();
        }
    };
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
            rel_specs,
            gate_params,
            desc: result_desc as usize,
            card1_alt,
            dispatch_slot,
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
/// `rels` are the plan's **relation-valued** parameters (`n_rels` entries, in
/// slot order; null when `n_rels == 0`): each slot's `__CODDL_REL_<slot>__`
/// marker is substituted with a table primary built from the bound relation's
/// rows — a `(VALUES …)` of numbered groups, or a typed zero-row SELECT when
/// the relation is empty — and its cells bind after the scalar params
/// (scalars are `?1..?param_count`; slot cells number onward, in slot order),
/// so the bind vector is scalars ++ slot cells regardless of where each
/// marker sits in the text. A read never batch-splits; past
/// [`QUERY_BIND_CEILING`] projected binds the shipped slots escalate into
/// stable per-(plan, slot) session temp tables instead ([`fire_escalated`])
/// and the statement binds scalars only. Scalar binds alone past the ceiling
/// abort loud — there is nothing to escalate.
///
/// Runs on a connection minted by [`ensure_connection`], so the audit `trace`
/// hook captures the (expanded) statement. Aborts on any hard error (unknown
/// plan or database, parameter-count/arity or kind mismatch, prepare/step
/// failure, NULL cell) — these are codegen/schema bugs, and the `*Relation`
/// return type has no status channel.
///
/// # Safety
/// `plan_id` must have been registered by [`coddl_register_plan`]. `params`
/// must point to `n` valid [`CoddlParam`] values (or be null when `n == 0`);
/// `rels` must point to `n_rels` valid [`CoddlRelParam`] values (or be null
/// when `n_rels == 0`).
#[no_mangle]
pub unsafe extern "C" fn coddl_query(
    plan_id: PlanId,
    params: *const CoddlParam,
    n: usize,
    rels: *const CoddlRelParam,
    n_rels: usize,
) -> *mut u8 {
    // Look up the plan; clone what we need and drop the registry lock before
    // taking any other lock (no lock-order coupling).
    let (db_name, sql, param_count, rel_specs, gate_params, desc_addr, card1_alt, dispatch_slot) = {
        let registry = plan_registry().lock().expect("plan registry poisoned");
        match registry.get(&plan_id.0) {
            Some(entry) => (
                entry.db_name.clone(),
                entry.sql.clone(),
                entry.param_count,
                entry.rel_specs.clone(),
                entry.gate_params.clone(),
                entry.desc,
                entry.card1_alt,
                entry.dispatch_slot,
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
    if n_rels != rel_specs.len() {
        eprintln!(
            "coddl: query: plan {} expects {} relation param(s), got {n_rels}",
            plan_id.0,
            rel_specs.len()
        );
        std::process::abort();
    }

    let param_slice: &[CoddlParam] = if params.is_null() || n == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(params, n)
    };
    let rel_slice: &[CoddlRelParam] = if rels.is_null() || n_rels == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(rels, n_rels)
    };

    // Validate every bound relation's arity up front and read its row count —
    // the count is free (the RC header holds it) and it drives the
    // cardinality dispatch: 0 at an absorbing slot → empty result without a
    // statement; 1 at the dispatch slot → the specialized sibling plan;
    // otherwise the general marker-expansion form.
    let mut row_counts: Vec<usize> = Vec::with_capacity(rel_slice.len());
    for (slot, rel) in rel_slice.iter().enumerate() {
        let arity = if rel.desc.is_null() {
            0
        } else {
            (*rel.desc).attr_count as usize
        };
        if arity != rel_specs[slot].arity as usize {
            eprintln!(
                "coddl: query: plan {}: relation param {slot} has arity {arity}, expected {}",
                plan_id.0, rel_specs[slot].arity
            );
            std::process::abort();
        }
        row_counts.push(rel_row_count(rel));
    }

    let ctx = MarshalCtx {
        site: "query",
        step_subject: format!("plan {}", plan_id.0),
        of_subject: format!("plan {}", plan_id.0),
    };
    let record_size = (*desc).record_size as usize;

    // Empty-slot short-circuit: an empty relation at an absorbing slot makes
    // the whole result provably empty (the empty relation is the join
    // family's multiplicative zero), so return a fresh empty relation on the
    // plan's result descriptor without preparing a statement or even touching
    // the connection. Observationally identical to firing (RM Pre 8).
    if rel_specs
        .iter()
        .zip(&row_counts)
        .any(|(spec, &count)| spec.absorbs_empty && count == 0)
    {
        return finalize_relation(&[], record_size, desc, &ctx);
    }

    // False-gate short-circuit — the Boolean sibling of the check above: a
    // `when`-gate conjunct on the absorbing spine binding false makes the
    // WHERE unsatisfiable, so the result is provably empty and the statement
    // never fires. Indices were validated `< param_count` at registration;
    // a non-Boolean bind at a gate index is a codegen bug.
    for &i in &gate_params {
        let p = &param_slice[i as usize];
        if p.kind != CoddlAttrKind::Boolean as u32 {
            eprintln!(
                "coddl: query: plan {}: gate param {i} has kind {}, expected Boolean",
                plan_id.0, p.kind
            );
            std::process::abort();
        }
        if p.i == 0 {
            return finalize_relation(&[], record_size, desc, &ctx);
        }
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
    let mut bindings: Vec<rusqlite::types::Value> = param_slice
        .iter()
        .map(|p| param_to_sqlite(p, plan_id.0))
        .collect();

    // Cardinality-1 dispatch: the dispatch slot holds exactly one row, so the
    // existence test degenerates to an equality conjunction and the baked
    // sibling plan (`WHERE shared = ?N…`) fires instead — for a keyed lookup
    // that is the direct-PK plan, no EXISTS and no DISTINCT. The sibling's
    // binds are this plan's scalars (`?1..?k`, same argument list) followed
    // by the row's cells (`?k+1..?k+m`, decoded in the shipped descriptor's
    // canonical order — the same order its equality conjuncts were emitted
    // in). The row's values bind as parameters like everything else; the
    // audit log's inlined values are trace expansion, not inlining.
    if let Some(alt_id) = card1_alt {
        if row_counts.get(dispatch_slot as usize) == Some(&1) {
            let (alt_sql, alt_param_count, alt_n_rels, alt_desc) = {
                let registry = plan_registry().lock().expect("plan registry poisoned");
                match registry.get(&alt_id) {
                    Some(entry) => (
                        entry.sql.clone(),
                        entry.param_count,
                        entry.rel_specs.len(),
                        entry.desc,
                    ),
                    None => {
                        eprintln!(
                            "coddl: query: plan {} names unregistered card-1 sibling {alt_id}",
                            plan_id.0
                        );
                        std::process::abort();
                    }
                }
            };
            // The sibling was registered from the same emission with the same
            // result heading, no markers of its own (v1 bakes it only for a
            // single-slot plan); a mismatch is a codegen bug.
            if alt_n_rels != 0 || alt_desc != desc_addr {
                eprintln!(
                    "coddl: query: card-1 sibling {alt_id} of plan {} is malformed",
                    plan_id.0
                );
                std::process::abort();
            }
            let rel = &rel_slice[dispatch_slot as usize];
            bindings.extend(decode_relation_cells(rel.src, rel.desc, "query"));
            if bindings.len() != alt_param_count as usize {
                eprintln!(
                    "coddl: query: card-1 sibling {alt_id} of plan {} expects {alt_param_count} \
                     bind(s), got {}",
                    plan_id.0,
                    bindings.len()
                );
                std::process::abort();
            }
            return fire_and_marshal(&path, &alt_sql, &bindings, desc, alt_id, &ctx);
        }
    }

    // Past the ceiling, the general form escalates: every slot ships into its
    // stable temp table and the statement binds scalars only. The projection
    // is exact — `rel_table_primary` decodes `count × arity` cells per slot
    // (0 for an empty relation) — so under-ceiling plans take the VALUES path
    // bit-for-bit as before. Only scalars alone past the ceiling still abort:
    // there is nothing left to escalate.
    let shipped_cells: usize = rel_specs
        .iter()
        .zip(&row_counts)
        .map(|(spec, &count)| count * spec.arity as usize)
        .sum();
    if bindings.len() + shipped_cells > QUERY_BIND_CEILING {
        if bindings.len() > QUERY_BIND_CEILING {
            eprintln!(
                "coddl: query: plan {}: {} scalar bind(s) alone exceed the \
                 {QUERY_BIND_CEILING} ceiling; nothing to escalate",
                plan_id.0,
                bindings.len()
            );
            std::process::abort();
        }
        return fire_escalated(
            &path, &sql, &bindings, rel_slice, &rel_specs, desc, plan_id.0, &ctx,
        );
    }

    // General form: substitute each slot's marker with the table primary
    // built from its rows and append its cells to the bind vector (scalars
    // first, then slot cells in slot order — matching the numbered
    // placeholders).
    let mut sql = sql;
    for (slot, rel) in rel_slice.iter().enumerate() {
        let marker = coddl_sqlemit::rel_param_marker(slot);
        if !sql.contains(&marker) {
            eprintln!(
                "coddl: query: plan {}: marker `{marker}` missing from plan SQL",
                plan_id.0
            );
            std::process::abort();
        }
        let (primary, cells) = rel_table_primary(rel, bindings.len(), plan_id.0);
        sql = sql.replacen(&marker, &primary, 1);
        bindings.extend(cells);
    }
    // The escalation check above projected this exactly; drift means
    // `rel_table_primary` learned to decode a different cell count than
    // `count × arity` and the projection must move with it.
    debug_assert!(
        bindings.len() <= QUERY_BIND_CEILING,
        "VALUES expansion diverged from the projected bind count"
    );

    fire_and_marshal(&path, &sql, &bindings, desc, plan_id.0, &ctx)
}

/// Prepare (cached), execute, and marshal one read statement's rows on an
/// already-held connection — the statement core shared by
/// [`fire_and_marshal`] (which takes the pool guard itself) and
/// [`fire_escalated`] (which holds one guard across populate + select +
/// cleanup). `prepare_cached` keys by SQL text per connection, so repeat
/// queries of the same text reuse the compiled statement; the audit `trace`
/// hook captures the (expanded) statement on execution.
///
/// # Safety
/// As [`marshal_rows`]: the statement's SELECT list must line up positionally
/// with `attrs` (heading-canonical order), each record exactly `record_size`
/// bytes.
unsafe fn query_rows_on(
    conn: &Connection,
    sql: &str,
    bindings: &[rusqlite::types::Value],
    attrs: &[CoddlAttrDesc],
    record_size: usize,
    plan_id: u32,
    ctx: &MarshalCtx,
) -> Vec<Vec<u8>> {
    let mut stmt = match conn.prepare_cached(sql) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("coddl: query: prepare failed for plan {plan_id}: {err}");
            std::process::abort();
        }
    };
    let mut rows = match stmt.query(params_from_iter(bindings.iter())) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("coddl: query: execution failed for plan {plan_id}: {err}");
            std::process::abort();
        }
    };
    marshal_rows(&mut rows, attrs, record_size, ctx)
}

/// Prepare, execute, and marshal one read statement into a fresh RC
/// `Relation` — the shared tail of [`coddl_query`]'s general and
/// cardinality-1 paths (the escalated path is [`fire_escalated`]).
///
/// # Safety
/// `desc` must be a valid heading descriptor outliving the returned relation;
/// the connection for `path` must have been opened by `ensure_connection`.
unsafe fn fire_and_marshal(
    path: &str,
    sql: &str,
    bindings: &[rusqlite::types::Value],
    desc: *const CoddlHeadingDesc,
    plan_id: u32,
    ctx: &MarshalCtx,
) -> *mut u8 {
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let record_size = (*desc).record_size as usize;
    // Fire the prepared statement and marshal its rows under one pool guard.
    // The CachedStatement / Rows / &Connection / guard are all frame-local
    // and drop at the block's end.
    let row_buffers = {
        let conn_guard = db_connections().lock().expect("conn map poisoned");
        let conn = conn_guard
            .get(path)
            .expect("connection inserted by ensure_connection");
        query_rows_on(conn, sql, bindings, attrs, record_size, plan_id, ctx)
    };

    finalize_relation(&row_buffers, record_size, desc, ctx)
}

/// Temp-table escalation — the general form's stand-in past the bind ceiling
/// (a read never batch-splits, so an over-ceiling `(VALUES …)` expansion is
/// not an option). Every slot's marker substitutes to its stable
/// per-(plan, slot) session temp table ([`temp_rel_table`] — stable name ⇒
/// stable statement text ⇒ one `prepare_cached` entry across every escalated
/// fire of the plan), every slot is populated (clear + batched INSERT; an
/// empty slot stays a cleared table — the escalated stand-in for the typed
/// zero-row SELECT, still no NULL token, RM Pro 4), the SELECT fires with
/// scalar binds only (guaranteed under the ceiling by the caller), and every
/// table is cleared after the read so the shipped rows don't outlive it.
///
/// ONE pool-guard acquisition spans populate + select + cleanup: connections
/// are opened `NO_MUTEX`, the pool guard *is* their serialization, so nothing
/// may interleave with the populated tables mid-read.
///
/// # Safety
/// As [`fire_and_marshal`], plus: `rel_slice`/`rel_specs` must be the plan's
/// validated relation parameters (arity checked against each descriptor).
#[allow(clippy::too_many_arguments)]
unsafe fn fire_escalated(
    path: &str,
    plan_sql: &str,
    scalars: &[rusqlite::types::Value],
    rel_slice: &[CoddlRelParam],
    rel_specs: &[RelSpec],
    desc: *const CoddlHeadingDesc,
    plan_id: u32,
    ctx: &MarshalCtx,
) -> *mut u8 {
    let mut sql = plan_sql.to_string();
    let mut tables: Vec<String> = Vec::with_capacity(rel_slice.len());
    for slot in 0..rel_slice.len() {
        let marker = coddl_sqlemit::rel_param_marker(slot);
        if !sql.contains(&marker) {
            eprintln!("coddl: query: plan {plan_id}: marker `{marker}` missing from plan SQL");
            std::process::abort();
        }
        let table = temp_rel_table(plan_id, slot);
        sql = sql.replacen(&marker, &table, 1);
        tables.push(table);
    }

    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let record_size = (*desc).record_size as usize;
    let row_buffers = {
        let conn_guard = db_connections().lock().expect("conn map poisoned");
        let conn = conn_guard
            .get(path)
            .expect("connection inserted by ensure_connection");
        for ((table, rel), spec) in tables.iter().zip(rel_slice).zip(rel_specs) {
            populate_temp_rel(conn, table, rel, spec.arity as usize, plan_id);
        }
        let buffers = query_rows_on(conn, &sql, scalars, attrs, record_size, plan_id, ctx);
        for table in &tables {
            exec_stmt_on(conn, &format!("DELETE FROM {table}"), plan_id);
        }
        buffers
    };

    finalize_relation(&row_buffers, record_size, desc, ctx)
}

/// Execute a registered **DML** plan (`DELETE`/`INSERT`/`UPDATE`) for its
/// effect only. Mirrors [`coddl_query`] but runs `execute` — there are no
/// result rows to marshal — and returns a [`CoddlStatus`]. The write lands on
/// the shared read-write connection inside the enclosing transaction's
/// BEGIN/COMMIT pair, so a later read in the same transaction sees it
/// (read-after-write).
///
/// Aborts on prepare/execute failure (the same loud-failure discipline as
/// `coddl_query`) — a DML plan that won't run is a codegen/schema bug, and the
/// status channel is reserved for future conflict handling.
///
/// # Safety
/// `plan_id` must have been registered by [`coddl_register_plan`]. `params`
/// must point to `n` valid [`CoddlParam`] values (or be null when `n == 0`).
#[no_mangle]
pub unsafe extern "C" fn coddl_exec(
    plan_id: PlanId,
    params: *const CoddlParam,
    n: usize,
) -> CoddlStatus {
    // Look up the plan; clone what we need and drop the registry lock before
    // taking any other lock (no lock-order coupling).
    let (db_name, sql, param_count) = {
        let registry = plan_registry().lock().expect("plan registry poisoned");
        match registry.get(&plan_id.0) {
            Some(entry) => (entry.db_name.clone(), entry.sql.clone(), entry.param_count),
            None => {
                eprintln!("coddl: exec: no plan registered for plan_id {}", plan_id.0);
                std::process::abort();
            }
        }
    };

    if n != param_count as usize {
        eprintln!(
            "coddl: exec: plan {} expects {param_count} param(s), got {n}",
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
                    "coddl: exec: plan {} references unregistered database `{db_name}`",
                    plan_id.0
                );
                std::process::abort();
            }
        }
    };

    // Open (or reuse) the connection before re-locking the pool (the Mutex is
    // not reentrant), then bind params: CoddlParam -> owned rusqlite Value.
    ensure_connection(&path);
    let param_slice: &[CoddlParam] = if params.is_null() || n == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(params, n)
    };
    let bindings: Vec<rusqlite::types::Value> = param_slice
        .iter()
        .map(|p| param_to_sqlite(p, plan_id.0))
        .collect();

    // Fire the prepared statement for effect under one pool guard.
    let conn_guard = db_connections().lock().expect("conn map poisoned");
    let conn = conn_guard
        .get(&path)
        .expect("connection inserted by ensure_connection");
    let mut stmt = match conn.prepare_cached(&sql) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("coddl: exec: prepare failed for plan {}: {err}", plan_id.0);
            std::process::abort();
        }
    };
    match stmt.execute(params_from_iter(bindings.iter())) {
        Ok(_) => CoddlStatus::Ok,
        Err(err) => {
            eprintln!(
                "coddl: exec: execution failed for plan {}: {err}",
                plan_id.0
            );
            std::process::abort();
        }
    }
}

/// Conservative ceiling on bind variables per statement — under SQLite's
/// historical `SQLITE_MAX_VARIABLE_NUMBER` floor (999). Batches of insert rows
/// are sized so `rows × arity` stays below it.
const INSERT_PARAM_BUDGET: usize = 900;

/// Ceiling on a read query's total bind variables (scalars + every shipped
/// relation's cells), under SQLite's historical `SQLITE_MAX_VARIABLE_NUMBER`
/// floor. A read is **never batch-split** — splitting a `(VALUES …)` operand
/// changes the query's meaning for any non-distributive surrounding shape —
/// so past the ceiling the shipped slots escalate into stable per-(plan,
/// slot) session temp tables ([`fire_escalated`]) and the statement binds
/// scalars only. Scalar binds alone past the ceiling fail loud: nothing to
/// escalate. See docs/sqlemit.md "Sending in-memory relations back into SQL".
const QUERY_BIND_CEILING: usize = 999;

/// Decode every row of an in-memory relation payload into owned rusqlite bind
/// values (row-major), reusing the record layout `coddl_write_relation`
/// reads. Covers every scalar cell kind: Integer/Boolean/Character as their
/// 8-byte integer (Character is its codepoint), Approximate as REAL with the
/// NaN value encoded as SQL NULL (the mirror of [`param_to_sqlite`] /
/// `marshal_rows`), Rational as canonical `TEXT "n/d"`, Text as owned copied
/// bytes. A Tuple- or Relation-valued cell aborts — compile-time emission
/// declines those headings, so reaching one here is a compiler bug. Returns
/// the empty vector for a null/empty relation.
///
/// # Safety
/// `src`/`desc` must describe a valid relation payload (as for
/// `coddl_write_relation`), or be null.
/// Row count of a relation-valued parameter — a free read of the RC header
/// (a null payload is the empty relation). This is what makes the force
/// point's cardinality dispatch cost nothing: the shipped relation is already
/// in hand, its count already counted.
///
/// # Safety
/// `rel.src` must be a valid relation payload or null.
unsafe fn rel_row_count(rel: &CoddlRelParam) -> usize {
    use crate::rc::{CoddlRcHeader, HEADER_SIZE};
    if rel.src.is_null() {
        return 0;
    }
    let header = rel.src.sub(HEADER_SIZE) as *const CoddlRcHeader;
    (*header).length as usize
}

unsafe fn decode_relation_cells(
    src: *const u8,
    desc: *const CoddlHeadingDesc,
    site: &str,
) -> Vec<rusqlite::types::Value> {
    use crate::rc::{CoddlRcHeader, HEADER_SIZE};
    use rusqlite::types::Value;

    if src.is_null() || desc.is_null() {
        return Vec::new();
    }
    let header = src.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let count = (*header).length as usize;
    let arity = (*desc).attr_count as usize;
    if count == 0 || arity == 0 {
        return Vec::new();
    }
    let record_size = (*desc).record_size as usize;
    let attrs = std::slice::from_raw_parts((*desc).attrs, arity);
    let payload = std::slice::from_raw_parts(src, count * record_size);

    let mut cells: Vec<Value> = Vec::with_capacity(count * arity);
    for record_idx in 0..count {
        let record = &payload[record_idx * record_size..(record_idx + 1) * record_size];
        for attr in attrs {
            let offset = attr.offset as usize;
            if attr.kind == CoddlAttrKind::Integer as u32
                || attr.kind == CoddlAttrKind::Boolean as u32
                || attr.kind == CoddlAttrKind::Character as u32
            {
                // Character binds as its integer codepoint (SQLite has no
                // char type); Boolean as the 0/1 it stores.
                let bytes: [u8; 8] = record[offset..offset + 8].try_into().unwrap();
                cells.push(Value::Integer(i64::from_ne_bytes(bytes)));
            } else if attr.kind == CoddlAttrKind::Approximate as u32 {
                // Canonical IEEE-754 bits; the NaN *value* encodes as SQL
                // NULL (SQLite can't store NaN) — a value encoding, not a
                // Coddl null (RM Pro 4 stands).
                let bytes: [u8; 8] = record[offset..offset + 8].try_into().unwrap();
                let v = f64::from_bits(u64::from_ne_bytes(bytes));
                cells.push(if v.is_nan() {
                    Value::Null
                } else {
                    Value::Real(v)
                });
            } else if attr.kind == CoddlAttrKind::Rational as u32 {
                // Reduced `(numer, denom)` pair → canonical `TEXT "n/d"`
                // (canonical form makes SQL text-`=` value-equality).
                let n_bytes: [u8; 8] = record[offset..offset + 8].try_into().unwrap();
                let d_bytes: [u8; 8] = record[offset + 8..offset + 16].try_into().unwrap();
                cells.push(Value::Text(format!(
                    "{}/{}",
                    i64::from_ne_bytes(n_bytes),
                    i64::from_ne_bytes(d_bytes)
                )));
            } else if attr.kind == CoddlAttrKind::Text as u32 {
                let ptr_bytes: [u8; 8] = record[offset..offset + 8].try_into().unwrap();
                let len_bytes: [u8; 8] = record[offset + 8..offset + 16].try_into().unwrap();
                let cptr = usize::from_ne_bytes(ptr_bytes) as *const u8;
                let len = usize::from_ne_bytes(len_bytes);
                let s = if cptr.is_null() {
                    String::new()
                } else {
                    match std::str::from_utf8(std::slice::from_raw_parts(cptr, len)) {
                        Ok(s) => s.to_string(),
                        Err(err) => {
                            eprintln!("coddl: {site}: non-UTF-8 Text cell: {err}");
                            std::process::abort();
                        }
                    }
                };
                cells.push(Value::Text(s));
            } else {
                eprintln!(
                    "coddl: {site}: cell kind {} has no SQL representation",
                    attr.kind
                );
                std::process::abort();
            }
        }
    }
    cells
}

/// Render `n_groups` numbered `(?b+1, ?b+2, …)` VALUES row-groups of `arity`
/// cells each, numbering from `base + 1`. Numbered placeholders keep every
/// bind site's index independent of its position in the statement text, so
/// runtime-expanded groups compose with a plan's compile-time scalar
/// placeholders (`?1..?param_count`) without renumbering anything.
fn values_groups(arity: usize, n_groups: usize, base: usize) -> String {
    let mut idx = base;
    let mut groups = Vec::with_capacity(n_groups);
    for _ in 0..n_groups {
        let cells: Vec<String> = (0..arity)
            .map(|_| {
                idx += 1;
                format!("?{idx}")
            })
            .collect();
        groups.push(format!("({})", cells.join(", ")));
    }
    groups.join(", ")
}

/// Session temp table backing plan `plan_id`'s slot `slot` in the escalated
/// (past-the-bind-ceiling) form. Stable per (plan, slot) so the escalated
/// SELECT's text is stable — one `prepare_cached` entry across repeat
/// escalated fires. A plain unquoted identifier in the `coddl_l` /
/// `coddl_v0` / `coddl_ow_p` alias convention; SQLite resolves unqualified
/// names temp-schema-first, so a same-named user table is shadowed, never
/// hit.
fn temp_rel_table(plan_id: u32, slot: usize) -> String {
    format!("coddl_rp_{plan_id}_{slot}")
}

/// `prepare_cached` + `execute` one no-result statement on a held connection,
/// aborting on failure — the read path's loud-failure discipline, for the
/// escalation's DDL/DML around the read itself.
fn exec_stmt_on(conn: &Connection, sql: &str, plan_id: u32) {
    let mut stmt = match conn.prepare_cached(sql) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("coddl: query: prepare failed for plan {plan_id}: {err}");
            std::process::abort();
        }
    };
    if let Err(err) = stmt.execute([]) {
        eprintln!("coddl: query: execution failed for plan {plan_id}: {err}");
        std::process::abort();
    }
}

/// Create-if-absent, clear, and batch-fill one escalated slot's temp table.
/// Columns are bare `column1…columnN` — the exact names a `(VALUES …)` table
/// exposes (which is what the emitted SELECT aliases from), and **no type
/// keyword**, so BLOB affinity and no insert coercion: the table behaves
/// exactly like the VALUES row source it replaces. Population batches mirror
/// [`coddl_exec_insert`] (`INSERT_PARAM_BUDGET / arity` rows per statement —
/// splitting is safe here because population is cumulative, unlike the read
/// itself). An empty relation leaves the table cleared. Runs inside any open
/// user transaction; a ROLLBACK may resurrect rows, harmlessly — every use
/// clears first.
///
/// # Safety
/// `rel` must carry a valid (or null-empty) relation payload + descriptor of
/// the given `arity` (≥ 1, validated against the plan's spec by the caller).
unsafe fn populate_temp_rel(
    conn: &Connection,
    table: &str,
    rel: &CoddlRelParam,
    arity: usize,
    plan_id: u32,
) {
    let cols: Vec<String> = (1..=arity).map(|i| format!("column{i}")).collect();
    exec_stmt_on(
        conn,
        &format!(
            "CREATE TEMP TABLE IF NOT EXISTS {table} ({})",
            cols.join(", ")
        ),
        plan_id,
    );
    exec_stmt_on(conn, &format!("DELETE FROM {table}"), plan_id);

    let cells = decode_relation_cells(rel.src, rel.desc, "query");
    let batch_rows = (INSERT_PARAM_BUDGET / arity).max(1);
    for batch in cells.chunks(batch_rows * arity) {
        let n_groups = batch.len() / arity;
        let sql = format!(
            "INSERT INTO {table} VALUES {}",
            values_groups(arity, n_groups, 0)
        );
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("coddl: query: prepare failed for plan {plan_id}: {err}");
                std::process::abort();
            }
        };
        if let Err(err) = stmt.execute(params_from_iter(batch.iter())) {
            eprintln!("coddl: query: execution failed for plan {plan_id}: {err}");
            std::process::abort();
        }
    }
}

/// The SQL **table primary** substituted for one relation-parameter marker,
/// plus the decoded cells to bind: a `(VALUES …)` of numbered groups over the
/// relation's rows (numbering from `base + 1`), or — for an empty relation —
/// a typed zero-row SELECT naming the same positional `column1…N` columns a
/// VALUES table exposes. The empty form's dummies are type-shaped literals
/// (`0` / `0.0` / `'0/1'` / `''`), never returned (`WHERE 0`), and **no NULL
/// token is ever emitted** (RM Pro 4).
///
/// # Safety
/// `rel` must carry a valid (or null-empty) relation payload + descriptor.
unsafe fn rel_table_primary(
    rel: &CoddlRelParam,
    base: usize,
    plan_id: u32,
) -> (String, Vec<rusqlite::types::Value>) {
    let arity = if rel.desc.is_null() {
        0
    } else {
        (*rel.desc).attr_count as usize
    };
    if arity == 0 {
        // Emission declines nullary relation parameters; a zero-arity
        // descriptor here is a codegen bug.
        eprintln!("coddl: query: plan {plan_id}: nullary relation parameter");
        std::process::abort();
    }
    let cells = decode_relation_cells(rel.src, rel.desc, "query");
    if cells.is_empty() {
        let attrs = std::slice::from_raw_parts((*rel.desc).attrs, arity);
        let cols: Vec<String> = attrs
            .iter()
            .enumerate()
            .map(|(i, attr)| {
                let dummy = if attr.kind == CoddlAttrKind::Approximate as u32 {
                    "0.0"
                } else if attr.kind == CoddlAttrKind::Rational as u32 {
                    "'0/1'"
                } else if attr.kind == CoddlAttrKind::Text as u32 {
                    "''"
                } else {
                    "0"
                };
                format!("{dummy} AS column{}", i + 1)
            })
            .collect();
        return (format!("(SELECT {} WHERE 0)", cols.join(", ")), cells);
    }
    let n_groups = cells.len() / arity;
    (
        format!("(VALUES {})", values_groups(arity, n_groups, base)),
        cells,
    )
}

/// Insert the rows of an **in-memory** relation `src` into a public relvar,
/// idempotently, via the registered insert template `plan_id` (an
/// `INSERT … SELECT … FROM __CODDL_REL_0__ … WHERE NOT EXISTS (…)`). Decodes
/// each row's cells via [`decode_relation_cells`], then expands the
/// template's slot-0 marker to a `(VALUES …)` of numbered `(?N,…)` groups and
/// binds the cells — a bulk multi-row `INSERT`, **no temp table** (so no
/// catalog churn) and no per-row round-trip. Batched so `rows × arity` stays
/// under the bind-variable limit — safe here because an insert is cumulative,
/// unlike a read (`coddl_query` never splits); `prepare_cached` reuses the
/// full-batch statement. Runs inside the current transaction; the
/// `NOT EXISTS` keeps it idempotent and a key-clash hits the `PRIMARY KEY`
/// (the Golden Rule). Aborts on prepare/execute failure.
///
/// # Safety
/// `plan_id` must be a registered insert template. `src`/`desc` must describe a
/// valid relation payload (as for [`coddl_write_relation`]).
#[no_mangle]
pub unsafe extern "C" fn coddl_exec_insert(
    plan_id: PlanId,
    src: *const u8,
    desc: *const CoddlHeadingDesc,
) -> CoddlStatus {
    let (db_name, template) = {
        let registry = plan_registry().lock().expect("plan registry poisoned");
        match registry.get(&plan_id.0) {
            Some(entry) => (entry.db_name.clone(), entry.sql.clone()),
            None => {
                eprintln!(
                    "coddl: exec_insert: no plan registered for plan_id {}",
                    plan_id.0
                );
                std::process::abort();
            }
        }
    };

    // An empty (or null) relation inserts nothing.
    let cells = decode_relation_cells(src, desc, "exec_insert");
    if cells.is_empty() {
        return CoddlStatus::Ok;
    }
    let arity = (*desc).attr_count as usize;

    // Resolve the plan's database to its connection path (as `coddl_exec`).
    let path = {
        let registry = database_registry()
            .lock()
            .expect("database registry poisoned");
        match registry.get(&db_name) {
            Some(entry) => entry.path.clone(),
            None => {
                eprintln!(
                    "coddl: exec_insert: plan {} references unregistered database `{db_name}`",
                    plan_id.0
                );
                std::process::abort();
            }
        }
    };
    ensure_connection(&path);

    // Substitute the slot-0 marker for a batch's worth of numbered `(?N,…)`
    // groups (the template carries no scalar placeholders, so numbering
    // starts at 1). Batch so `rows × arity` stays under the bind limit.
    let marker = coddl_sqlemit::rel_param_marker(0);
    let batch_rows = (INSERT_PARAM_BUDGET / arity).max(1);

    let conn_guard = db_connections().lock().expect("conn map poisoned");
    let conn = conn_guard
        .get(&path)
        .expect("connection inserted by ensure_connection");
    for batch in cells.chunks(batch_rows * arity) {
        let n_groups = batch.len() / arity;
        let values = format!("(VALUES {})", values_groups(arity, n_groups, 0));
        let sql = template.replacen(&marker, &values, 1);
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(err) => {
                eprintln!(
                    "coddl: exec_insert: prepare failed for plan {}: {err}",
                    plan_id.0
                );
                std::process::abort();
            }
        };
        if let Err(err) = stmt.execute(params_from_iter(batch.iter())) {
            eprintln!(
                "coddl: exec_insert: execution failed for plan {}: {err}",
                plan_id.0
            );
            std::process::abort();
        }
    }
    CoddlStatus::Ok
}

/// Lower one [`CoddlParam`] to an owned rusqlite bind value. Boolean binds as
/// the 0/1 integer SQLite stores it as; Text copies its bytes so the bound
/// value owns them. Aborts on an unsupported kind or non-UTF-8 Text.
///
/// # Safety
/// For a Text param, `(ptr, len)` must describe valid bytes for the call.
unsafe fn param_to_sqlite(p: &CoddlParam, plan_id: u32) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    if p.kind == CoddlAttrKind::Integer as u32
        || p.kind == CoddlAttrKind::Boolean as u32
        || p.kind == CoddlAttrKind::Character as u32
    {
        // Character binds as its integer codepoint (SQLite has no char type).
        Value::Integer(p.i)
    } else if p.kind == CoddlAttrKind::Approximate as u32 {
        // The `i` slot carries the double's canonical bits. SQLite encodes the
        // NaN *value* as SQL NULL (it can't store NaN), so a NaN binds as NULL
        // and a finite/±Inf value binds as REAL. The reverse of `marshal_rows`.
        let v = f64::from_bits(p.i as u64);
        if v.is_nan() {
            Value::Null
        } else {
            Value::Real(v)
        }
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
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"message".as_ptr(),
                name_len: 7,
                kind: CoddlAttrKind::Text as u32,
                offset: 8,
                sub: std::ptr::null(),
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
    fn coddl_exec_and_transactions_round_trip() {
        // The sole test that drives the process-global transaction-depth
        // counter (serialized by `test_guard`); it exercises `coddl_exec`
        // (DELETE), read-after-write on the shared connection, ROLLBACK
        // restoring rows, and COMMIT persisting them.
        use crate::rc::{coddl_rc_release, CoddlRcHeader};
        use rusqlite::params;

        let _g = test_guard();
        let (_tmp, path_str) = seed_two_row_greetings();
        let attrs = greetings_attrs();
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 24,
            attrs: attrs.as_ptr(),
        };
        let db = b"greetings";
        let select_sql = br#"SELECT "id", "message" FROM "greetings""#;
        let delete_sql = br#"DELETE FROM "greetings" WHERE "id" = ?1"#;

        // The number of rows a fresh read returns.
        let read_len = |plan: u32| -> usize {
            unsafe {
                let rel = coddl_query(PlanId(plan), ptr::null(), 0, ptr::null(), 0);
                assert!(!rel.is_null());
                let header = &*(rel.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader);
                let len = header.length as usize;
                coddl_rc_release(rel);
                len
            }
        };

        unsafe {
            assert_eq!(
                coddl_register_database(db.as_ptr(), db.len(), path_str.as_ptr(), path_str.len()),
                CoddlStatus::Ok
            );
            // Plan 0: read-all. Plan 1: surgical DELETE of one row.
            assert_eq!(
                coddl_register_plan(
                    PlanId(0),
                    db.as_ptr(),
                    db.len(),
                    select_sql.as_ptr(),
                    select_sql.len(),
                    0,
                    ptr::null(),
                    0,
                    &desc,
                    -1,
                    0,
                    ptr::null(),
                    0,
                ),
                CoddlStatus::Ok
            );
            assert_eq!(
                coddl_register_plan(
                    PlanId(1),
                    db.as_ptr(),
                    db.len(),
                    delete_sql.as_ptr(),
                    delete_sql.len(),
                    1,
                    ptr::null(),
                    0,
                    &desc,
                    -1,
                    0,
                    ptr::null(),
                    0,
                ),
                CoddlStatus::Ok
            );

            let id1 = CoddlParam {
                i: 1,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };

            // ── DELETE then read-after-write, then ROLLBACK ──
            assert_eq!(coddl_begin_tx(), CoddlStatus::Ok);
            assert_eq!(read_len(0), 2, "two rows before delete");
            assert_eq!(coddl_exec(PlanId(1), &id1, 1), CoddlStatus::Ok);
            assert_eq!(read_len(0), 1, "read-after-write sees the delete");
            assert_eq!(coddl_rollback_tx(), CoddlStatus::Ok);

            // ── ROLLBACK restored the row; a second tx COMMITs the delete ──
            assert_eq!(coddl_begin_tx(), CoddlStatus::Ok);
            assert_eq!(read_len(0), 2, "rollback restored the deleted row");
            assert_eq!(coddl_exec(PlanId(1), &id1, 1), CoddlStatus::Ok);
            assert_eq!(coddl_commit_tx(), CoddlStatus::Ok);

            shutdown_storage();
        }

        // The COMMIT persisted: a fresh connection sees exactly one row.
        let conn = Connection::open(&path_str).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM greetings", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "commit persisted the delete");
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
                sub: std::ptr::null(),
            },
            crate::relation::CoddlAttrDesc {
                name: message_name.as_ptr(),
                name_len: message_name.len() as u32,
                kind: CoddlAttrKind::Text as u32,
                offset: 8, // Integer = 8 bytes
                sub: std::ptr::null(),
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
        let sql = br#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?1"#;

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
                    ptr::null(),
                    0,
                    &desc,
                    -1,
                    0,
                    ptr::null(),
                    0,
                ),
                CoddlStatus::Ok
            );

            let param = CoddlParam {
                i: 1,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let rel = coddl_query(PlanId(0), &param, 1, ptr::null(), 0);
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
                std::slice::from_raw_parts(rel.add(8), 8)
                    .try_into()
                    .unwrap(),
            ) as *const u8;
            let msg_len = usize::from_ne_bytes(
                std::slice::from_raw_parts(rel.add(16), 8)
                    .try_into()
                    .unwrap(),
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
        let sql = br#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?1"#;

        unsafe {
            coddl_register_database(db.as_ptr(), db.len(), path_str.as_ptr(), path_str.len());
            coddl_register_plan(
                PlanId(0),
                db.as_ptr(),
                db.len(),
                sql.as_ptr(),
                sql.len(),
                1,
                ptr::null(),
                0,
                &desc,
                -1,
                0,
                ptr::null(),
                0,
            );

            // No row has id = 99 → an empty (length-0) relation, not an abort.
            let param = CoddlParam {
                i: 99,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let rel = coddl_query(PlanId(0), &param, 1, ptr::null(), 0);
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
        let sql = br#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?1"#;

        unsafe {
            coddl_register_database(db.as_ptr(), db.len(), path_str.as_ptr(), path_str.len());
            coddl_register_plan(
                PlanId(0),
                db.as_ptr(),
                db.len(),
                sql.as_ptr(),
                sql.len(),
                1,
                ptr::null(),
                0,
                &desc,
                -1,
                0,
                ptr::null(),
                0,
            );

            // Two queries through the same plan hit rusqlite's prepared-statement
            // cache on the second call; both must return the right single row.
            let p1 = CoddlParam {
                i: 1,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let r1 = coddl_query(PlanId(0), &p1, 1, ptr::null(), 0);
            assert_eq!(
                (*(r1.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader)).length,
                1
            );
            assert_eq!(ptr::read(r1 as *const i64), 1);
            coddl_rc_release(r1);

            let p2 = CoddlParam {
                i: 2,
                ptr: ptr::null(),
                len: 0,
                kind: CoddlAttrKind::Integer as u32,
            };
            let r2 = coddl_query(PlanId(0), &p2, 1, ptr::null(), 0);
            assert_eq!(
                (*(r2.sub(crate::rc::HEADER_SIZE) as *const CoddlRcHeader)).length,
                1
            );
            assert_eq!(ptr::read(r2 as *const i64), 2);
            coddl_rc_release(r2);

            shutdown_storage();
        }
    }
}

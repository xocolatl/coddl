//! Relation values: layout, seal, printer, drop walker.
//!
//! Records are fixed-stride byte buffers in heading-canonical
//! (name-sorted) attribute order. The per-heading [`CoddlHeadingDesc`]
//! tells the runtime where each attribute lives within a record and
//! what kind it is. The descriptor lives in read-only data emitted
//! by each backend; the runtime treats it as immutable for the
//! lifetime of the program.
//!
//! ## Cell kinds (v1)
//!
//! | `CoddlAttrKind` | Width    | Encoding                                |
//! |-----------------|----------|-----------------------------------------|
//! | `Integer`       | 8 bytes  | `i64` little-endian (host)              |
//! | `Boolean`       | 8 bytes  | `i64`; 0 = false, 1 = true              |
//! | `Text`          | 16 bytes | `(ptr: *const u8, len: usize)`          |
//!
//! Phase 19 ships these three. Other scalar types and recursive
//! Tuple / Relation cells slot in as later phases need them; the
//! descriptor format already has the `Tuple` and `Relation` kind
//! tags reserved.
//!
//! ## Seal discipline (RM Pro 3)
//!
//! [`coddl_relation_seal`] enforces "no duplicate tuples" on a relation
//! built in process (literals, `project`, `join`/`times`): it sorts
//! records by byte-wise comparison only to bring equal records adjacent,
//! then dedups in place by trimming the header's `length`. The resulting
//! order is **not** meaningful — a relation is a set with no tuple order
//! (RM Pro 1), so output order is unspecified and two backends agree on a
//! relation as a *set* of tuples (RM Pre 8), not byte-for-byte. (For
//! Text-leading relations the byte sort even orders by string pointer,
//! which differs across backends — harmless precisely because order is
//! unspecified.) The SQL path does **not** seal: the backend already
//! returns a duplicate-free set (see `sqlite::finalize_relation`).
//!
//! ## Printer
//!
//! [`coddl_write_relation`] prints one tuple per line as
//! `{name: value, name: value}\n`. Attributes appear in canonical
//! heading order (matching the descriptor). Tuple and Relation cells
//! print as `{...}` placeholders in Phase 19; the recursive printer
//! lands when nested compound types become a real workflow.

use std::io::Write;

use crate::rc::{coddl_rc_alloc, coddl_rc_release, CoddlKind, CoddlRcHeader, HEADER_SIZE};

/// Per-attribute kind tag in the heading descriptor. Stable
/// integers — backends and runtime mirror these constants. The same
/// scheme that drives the printer drives the drop walker.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoddlAttrKind {
    Integer = 0,
    Boolean = 1,
    Text = 2,
    // Reserved: Tuple = 10, Relation = 11. Not yet emitted; the
    // printer / drop walker route on these will land when nested
    // compound cells do.
}

/// One attribute in a heading descriptor.
#[repr(C)]
#[derive(Debug)]
pub struct CoddlAttrDesc {
    /// UTF-8 bytes of the attribute name. Not null-terminated.
    pub name: *const u8,
    pub name_len: u32,
    /// `CoddlAttrKind` numeric value.
    pub kind: u32,
    /// Byte offset within a record.
    pub offset: u32,
}

/// One heading descriptor. Lives in read-only data; backends emit
/// one per unique heading per `Module`.
#[repr(C)]
#[derive(Debug)]
pub struct CoddlHeadingDesc {
    pub attr_count: u32,
    pub record_size: u32,
    pub attrs: *const CoddlAttrDesc,
}

/// Dedup a relation's payload in place to uphold RM Pro 3 (no duplicate
/// tuples), updating the header's `length`. Sorting is just the mechanism
/// — it brings equal records adjacent so one linear pass removes them. The
/// resulting record order is an implementation byproduct, not meaningful: a
/// relation is a set with no tuple order (RM Pro 1), so callers must not
/// rely on it (and for Text-leading relations it is not even cross-backend
/// stable, since Text cells sort by pointer).
///
/// # Safety
/// `ptr` must point to a payload returned by `coddl_rc_alloc` whose
/// kind tag is `Relation` and whose header `desc` matches the
/// supplied descriptor (or any descriptor with the same layout).
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_seal(ptr: *mut u8, desc: *const CoddlHeadingDesc) {
    if ptr.is_null() || desc.is_null() {
        return;
    }
    let header = ptr.sub(HEADER_SIZE) as *mut CoddlRcHeader;
    let record_size = (*desc).record_size as usize;
    let count = (*header).length as usize;
    if count <= 1 || record_size == 0 {
        return;
    }

    // Build a Vec of record indices, sort it by byte-wise comparison
    // of the records, then permute the records into a fresh buffer
    // and dedup adjacent equal ones.
    //
    // Byte-wise comparison is total because the layout is canonical
    // and every cell's encoding is fixed-width and host-endian
    // consistent. Text cells compare by pointer/length pair, which
    // is equality-correct for interned/static strings (Phase 19's
    // Text payloads come from compile-time string constants — they
    // dedupe by content at compile time, so equal-content strings
    // share the same pointer and length).
    let payload = std::slice::from_raw_parts_mut(ptr, count * record_size);

    let mut indices: Vec<usize> = (0..count).collect();
    indices.sort_by(|&a, &b| {
        let ra = &payload[a * record_size..(a + 1) * record_size];
        let rb = &payload[b * record_size..(b + 1) * record_size];
        ra.cmp(rb)
    });

    let mut sorted: Vec<u8> = Vec::with_capacity(count * record_size);
    for &i in &indices {
        sorted.extend_from_slice(&payload[i * record_size..(i + 1) * record_size]);
    }

    // Adjacent dedup.
    let mut write_idx = 0usize;
    for read_idx in 0..count {
        if read_idx == 0 {
            write_idx = 1;
            continue;
        }
        let prev = &sorted[(write_idx - 1) * record_size..write_idx * record_size];
        let cur = &sorted[read_idx * record_size..(read_idx + 1) * record_size];
        if prev != cur {
            if read_idx != write_idx {
                let (head, tail) = sorted.split_at_mut(read_idx * record_size);
                let dest = &mut head[write_idx * record_size..(write_idx + 1) * record_size];
                dest.copy_from_slice(&tail[..record_size]);
            }
            write_idx += 1;
        }
    }

    // Copy the sorted+deduped records back into the payload.
    payload[..write_idx * record_size].copy_from_slice(&sorted[..write_idx * record_size]);
    (*header).length = write_idx as u32;
}

/// Print a relation: one tuple per line in canonical heading order,
/// shaped as `{name: value, name: value}\n`. Empty relation prints
/// zero bytes.
///
/// # Safety
/// `ptr` must satisfy the same preconditions as `coddl_relation_seal`.
/// Initialize an in-memory `private` relvar slot with an empty relation.
/// Allocates a 0-row relation carrying `desc` and stores its RC pointer into
/// `*slot`. There is no SQL source; the slot is later filled by relational
/// assignment (`coddl_relvar_slot_store`).
///
/// # Safety
/// `desc` must outlive the slot; `slot` must point to a writable `*mut u8`.
#[no_mangle]
pub unsafe extern "C" fn coddl_relvar_slot_init_empty(
    desc: *const CoddlHeadingDesc,
    slot: *mut *mut u8,
) {
    *slot = coddl_rc_alloc(0, 0, CoddlKind::Relation as u32, desc);
}

/// Store `value` into a relvar slot — relational assignment `R := <expr>`.
/// Move semantics: the slot's previous value (if any) is released and the slot
/// takes ownership of `value`, so the caller must not also release it.
///
/// # Safety
/// `slot` must point to a writable `*mut u8` previously initialized by a slot
/// init; `value` must be an RC relation payload the caller owns.
#[no_mangle]
pub unsafe extern "C" fn coddl_relvar_slot_store(value: *mut u8, slot: *mut *mut u8) {
    let old = *slot;
    if !old.is_null() {
        coddl_rc_release(old);
    }
    *slot = value;
}

/// Natural join two relations on their shared attributes (surface `join`,
/// Algebra-A AND). A pair of records matches when all shared cells are
/// byte-equal; each match emits the union of attributes — every result
/// attribute copied from whichever side defines it (lhs preferred for shared).
/// Zero shared attributes ⇒ Cartesian product. Worst-case allocation, then
/// `coddl_relation_seal` (sort + dedup, RM Pro 3).
///
/// # Safety
/// All pointers must be non-null payloads / descriptors from the runtime and
/// must outlive the call.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_join(
    lhs: *const u8,
    lhs_desc: *const CoddlHeadingDesc,
    rhs: *const u8,
    rhs_desc: *const CoddlHeadingDesc,
    result_desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    if lhs.is_null()
        || rhs.is_null()
        || lhs_desc.is_null()
        || rhs_desc.is_null()
        || result_desc.is_null()
    {
        return std::ptr::null_mut();
    }
    let lhs_count = (*(lhs.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let rhs_count = (*(rhs.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let lhs_rec = (*lhs_desc).record_size as usize;
    let rhs_rec = (*rhs_desc).record_size as usize;
    let res_rec = (*result_desc).record_size as usize;

    let lhs_attrs =
        std::slice::from_raw_parts((*lhs_desc).attrs, (*lhs_desc).attr_count as usize);
    let rhs_attrs =
        std::slice::from_raw_parts((*rhs_desc).attrs, (*rhs_desc).attr_count as usize);
    let res_attrs =
        std::slice::from_raw_parts((*result_desc).attrs, (*result_desc).attr_count as usize);

    // Shared attributes → equality test pairs (lhs_off, rhs_off, width, is_text).
    // `is_text` selects content comparison over raw bytes: a Text cell is a
    // 16-byte (ptr, len) fat pointer, and equal text from two different string
    // constants has different pointers — so the shared cells must be compared by
    // content, not by their fat-pointer bytes.
    let mut shared: Vec<(usize, usize, usize, bool)> = Vec::new();
    for la in lhs_attrs {
        let lname = std::slice::from_raw_parts(la.name, la.name_len as usize);
        for ra in rhs_attrs {
            let rname = std::slice::from_raw_parts(ra.name, ra.name_len as usize);
            if lname == rname {
                let is_text = la.kind == CoddlAttrKind::Text as u32;
                shared.push((
                    la.offset as usize,
                    ra.offset as usize,
                    cell_width(la.kind),
                    is_text,
                ));
                break;
            }
        }
    }

    // Result-attribute copies: (res_off, from_lhs, src_off, width). Each result
    // attribute is defined by exactly one side (lhs preferred for shared).
    let mut moves: Vec<(usize, bool, usize, usize)> = Vec::new();
    for d in res_attrs {
        let dname = std::slice::from_raw_parts(d.name, d.name_len as usize);
        let mut placed = false;
        for la in lhs_attrs {
            let lname = std::slice::from_raw_parts(la.name, la.name_len as usize);
            if lname == dname {
                moves.push((d.offset as usize, true, la.offset as usize, cell_width(d.kind)));
                placed = true;
                break;
            }
        }
        if placed {
            continue;
        }
        for ra in rhs_attrs {
            let rname = std::slice::from_raw_parts(ra.name, ra.name_len as usize);
            if rname == dname {
                moves.push((d.offset as usize, false, ra.offset as usize, cell_width(d.kind)));
                break;
            }
        }
    }

    let cap = lhs_count.saturating_mul(rhs_count);
    let out = crate::rc::coddl_rc_alloc(
        res_rec.saturating_mul(cap),
        0,
        crate::rc::CoddlKind::Relation as u32,
        result_desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }

    let mut written = 0usize;
    for li in 0..lhs_count {
        let lrec = lhs.add(li * lhs_rec);
        for ri in 0..rhs_count {
            let rrec = rhs.add(ri * rhs_rec);
            let mut matched = true;
            for &(loff, roff, w, is_text) in &shared {
                let eq = if is_text {
                    let (lptr, llen) = read_text_cell(lrec, loff);
                    let (rptr, rlen) = read_text_cell(rrec, roff);
                    coddl_text_eq(lptr, llen, rptr, rlen) != 0
                } else {
                    let a = std::slice::from_raw_parts(lrec.add(loff), w);
                    let b = std::slice::from_raw_parts(rrec.add(roff), w);
                    a == b
                };
                if !eq {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
            }
            let orec = out.add(written * res_rec);
            for &(res_off, from_lhs, src_off, w) in &moves {
                let src = if from_lhs {
                    lrec.add(src_off)
                } else {
                    rrec.add(src_off)
                };
                std::ptr::copy_nonoverlapping(src, orec.add(res_off), w);
            }
            written += 1;
        }
    }
    (*(out.sub(HEADER_SIZE) as *mut CoddlRcHeader)).length = written as u32;
    coddl_relation_seal(out, result_desc);
    out
}

#[no_mangle]
pub unsafe extern "C" fn coddl_write_relation(ptr: *const u8, desc: *const CoddlHeadingDesc) {
    if ptr.is_null() || desc.is_null() {
        return;
    }
    let header = ptr.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let record_size = (*desc).record_size as usize;
    let count = (*header).length as usize;
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let payload = std::slice::from_raw_parts(ptr, count * record_size);

    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for record_idx in 0..count {
        let record = &payload[record_idx * record_size..(record_idx + 1) * record_size];
        let _ = w.write_all(b"{");
        for (i, attr) in attrs.iter().enumerate() {
            if i > 0 {
                let _ = w.write_all(b", ");
            }
            let name_slice = std::slice::from_raw_parts(attr.name, attr.name_len as usize);
            let _ = w.write_all(name_slice);
            let _ = w.write_all(b": ");
            print_cell(&mut w, attr, record);
        }
        let _ = w.write_all(b"}\n");
    }
}

/// Format one cell within a record to `w`. Dispatches on
/// `CoddlAttrKind`. Cells the printer doesn't recognize yet (Tuple,
/// Relation) print as `{...}` so the printer remains total.
unsafe fn print_cell<W: Write>(w: &mut W, attr: &CoddlAttrDesc, record: &[u8]) {
    let offset = attr.offset as usize;
    if attr.kind == CoddlAttrKind::Integer as u32 {
        let bytes: [u8; 8] = record[offset..offset + 8].try_into().unwrap();
        let value = i64::from_ne_bytes(bytes);
        let _ = write!(w, "{value}");
    } else if attr.kind == CoddlAttrKind::Boolean as u32 {
        let bytes: [u8; 8] = record[offset..offset + 8].try_into().unwrap();
        let value = i64::from_ne_bytes(bytes);
        let _ = w.write_all(if value != 0 { b"true" } else { b"false" });
    } else if attr.kind == CoddlAttrKind::Text as u32 {
        let ptr_bytes: [u8; 8] = record[offset..offset + 8].try_into().unwrap();
        let len_bytes: [u8; 8] = record[offset + 8..offset + 16].try_into().unwrap();
        let ptr = usize::from_ne_bytes(ptr_bytes) as *const u8;
        let len = usize::from_ne_bytes(len_bytes);
        let _ = w.write_all(b"\"");
        if !ptr.is_null() {
            let slice = std::slice::from_raw_parts(ptr, len);
            let _ = w.write_all(slice);
        }
        let _ = w.write_all(b"\"");
    } else {
        // Tuple, Relation, or future cells.
        let _ = w.write_all(b"{...}");
    }
}

/// Drop-walker entry for relation-kind payloads. Walks each record
/// and, for any heap-cell attribute (none in Phase 19 — strings are
/// immortal compile-time constants), releases the contained pointer
/// before the payload block is freed.
///
/// Phase 19's runtime cells are all stack-equivalent (Integer,
/// Boolean) or point at immortal data (Text from string literals).
/// This function is therefore a no-op today; the hook exists so
/// Phase 20+ relation-of-relations and Tuple-cells-with-Text-owners
/// land without re-plumbing.
///
/// # Safety
/// Called only by `coddl_rc_release` when refcount reaches zero. The
/// header must already have been read; the payload block is freed
/// after this returns.
/// Restrict a relation by a predicate. Returns a fresh RC-managed
/// relation (rc=1) containing the source rows for which
/// `pred_fn(record_ptr)` is non-zero. `src` is left unchanged.
///
/// The output is allocated at worst-case size (`record_size * length`)
/// then trimmed via the header's `length` field. Filter preserves
/// the sealed order, so no re-seal is needed.
///
/// # Safety
/// `src` must point to a payload returned by `coddl_rc_alloc` whose
/// kind is `Relation` and whose header carries the same descriptor
/// as `desc`. `pred_fn` must be safe to call across the FFI boundary
/// with arbitrary record pointers from the source payload.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_where(
    src: *const u8,
    desc: *const CoddlHeadingDesc,
    pred_fn: extern "C" fn(*const u8) -> i8,
) -> *mut u8 {
    if src.is_null() || desc.is_null() {
        return std::ptr::null_mut();
    }
    let header = src.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let record_size = (*desc).record_size as usize;
    let count = (*header).length as usize;

    // Allocate worst-case output.
    let payload_size = record_size * count;
    let out = crate::rc::coddl_rc_alloc(
        payload_size,
        count as u32,
        crate::rc::CoddlKind::Relation as u32,
        desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }

    // Loop, evaluate, conditional-copy.
    let mut written: usize = 0;
    for i in 0..count {
        let record_ptr = src.add(i * record_size);
        let keep = (pred_fn)(record_ptr) != 0;
        if keep {
            let dst_slot = out.add(written * record_size);
            std::ptr::copy_nonoverlapping(record_ptr, dst_slot, record_size);
            written += 1;
        }
    }

    // Trim the output's length. Restriction preserves the input's
    // sorted/deduped order, so no re-seal is needed.
    let out_header = out.sub(HEADER_SIZE) as *mut CoddlRcHeader;
    (*out_header).length = written as u32;

    out
}

/// Compare two text values for byte-exact equality. Returns `1` when the two
/// `(ptr, len)` slices are equal, `0` otherwise. This is the in-process
/// counterpart to the SQL backend's `=` on Text: a compiled `where` predicate
/// over a Text attribute calls this rather than an integer compare, because a
/// Text cell is a 16-byte `(ptr, len)` pair, not an inline scalar.
///
/// # Safety
/// Each `(ptr, len)` pair must describe `len` readable bytes, or have
/// `len == 0` (in which case the pointer is never dereferenced). The bytes
/// come from compile-time string constants or sealed relation cells, both
/// immutable for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn coddl_text_eq(
    a_ptr: *const u8,
    a_len: usize,
    b_ptr: *const u8,
    b_len: usize,
) -> i8 {
    if a_len != b_len {
        return 0;
    }
    if a_len == 0 {
        return 1;
    }
    let a = std::slice::from_raw_parts(a_ptr, a_len);
    let b = std::slice::from_raw_parts(b_ptr, b_len);
    (a == b) as i8
}

/// Byte width of one cell of the given [`CoddlAttrKind`]. Mirrors the
/// record-layout table in the module header: scalar cells are 8 bytes,
/// `Text` is a 16-byte `(ptr, len)` pair.
fn cell_width(kind: u32) -> usize {
    if kind == CoddlAttrKind::Text as u32 {
        16
    } else {
        // Integer, Boolean, and any future 8-byte scalar.
        8
    }
}

/// Read a `Text` cell — a 16-byte `(ptr, len)` fat pointer — from `rec` at
/// `off`, returning `(ptr, len)`. Mirrors the layout `print_cell` reads;
/// byte-copies via `from_ne_bytes` so it's safe regardless of record alignment.
///
/// # Safety
/// `rec.add(off)` must point at 16 readable bytes (one Text cell).
unsafe fn read_text_cell(rec: *const u8, off: usize) -> (*const u8, usize) {
    let ptr_bytes: [u8; 8] = std::slice::from_raw_parts(rec.add(off), 8)
        .try_into()
        .unwrap();
    let len_bytes: [u8; 8] = std::slice::from_raw_parts(rec.add(off + 8), 8)
        .try_into()
        .unwrap();
    (
        usize::from_ne_bytes(ptr_bytes) as *const u8,
        usize::from_ne_bytes(len_bytes),
    )
}

/// Project a relation onto a subset of its attributes. Returns a fresh
/// RC-managed relation (rc=1) whose heading is `dst_desc` — the kept
/// attributes — with each record's cells copied from the source by
/// attribute name. `src` is left unchanged.
///
/// Projection can collapse distinct source rows (those that agreed on
/// the kept attributes but differed on a dropped one) into duplicates,
/// so the output is **sealed** (sort + adjacent-dedup) before return to
/// uphold RM Pro 3. The output is allocated worst-case (`count` rows)
/// and trimmed by the seal.
///
/// `dst_desc` must list a subset of `src_desc`'s attribute names; the
/// runtime resolves each destination attribute to its source offset by
/// name (both descriptors are emitted by codegen). Cells are copied by
/// value — `Text` cells carry the same `(ptr, len)` as the source, which
/// points at the same immortal string data (no ownership transfer).
///
/// # Safety
/// `src` must point to a payload returned by `coddl_rc_alloc` whose kind
/// is `Relation` and whose header carries the same descriptor as
/// `src_desc`. Both descriptors must outlive this call (read-only data
/// symbols the codegen emitted).
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_project(
    src: *const u8,
    src_desc: *const CoddlHeadingDesc,
    dst_desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    if src.is_null() || src_desc.is_null() || dst_desc.is_null() {
        return std::ptr::null_mut();
    }
    let header = src.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let count = (*header).length as usize;
    let src_record_size = (*src_desc).record_size as usize;
    let dst_record_size = (*dst_desc).record_size as usize;

    let src_attrs = std::slice::from_raw_parts((*src_desc).attrs, (*src_desc).attr_count as usize);
    let dst_attrs = std::slice::from_raw_parts((*dst_desc).attrs, (*dst_desc).attr_count as usize);

    // Resolve each destination attribute to its source offset once, by
    // name: `(src_offset, dst_offset, width)`.
    let mut moves: Vec<(usize, usize, usize)> = Vec::with_capacity(dst_attrs.len());
    for d in dst_attrs {
        let dname = std::slice::from_raw_parts(d.name, d.name_len as usize);
        let s = src_attrs.iter().find(|s| {
            let sname = std::slice::from_raw_parts(s.name, s.name_len as usize);
            sname == dname
        });
        // A well-typed projection always finds the attribute; skip
        // defensively if a malformed descriptor pair ever doesn't.
        if let Some(s) = s {
            moves.push((s.offset as usize, d.offset as usize, cell_width(d.kind)));
        }
    }

    // Allocate worst-case output; the seal trims after dedup.
    let out = crate::rc::coddl_rc_alloc(
        dst_record_size * count,
        count as u32,
        crate::rc::CoddlKind::Relation as u32,
        dst_desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }

    for i in 0..count {
        let src_rec = src.add(i * src_record_size);
        let dst_rec = out.add(i * dst_record_size);
        for &(src_off, dst_off, width) in &moves {
            std::ptr::copy_nonoverlapping(src_rec.add(src_off), dst_rec.add(dst_off), width);
        }
    }

    if dst_record_size == 0 {
        // Nullary projection (`project {}`): every record is the empty
        // tuple, so the result collapses to `reltrue` (one empty tuple)
        // when the source had any rows, else `relfalse`. `coddl_relation_seal`
        // can't dedup zero-width records (it early-returns on
        // `record_size == 0`), so collapse explicitly to keep the result a
        // set (RM Pro 3).
        let out_header = out.sub(HEADER_SIZE) as *mut CoddlRcHeader;
        (*out_header).length = u32::from(count > 0);
    } else {
        // Projection may have created duplicates; seal restores the
        // set + canonical order (RM Pro 3).
        coddl_relation_seal(out, dst_desc);
    }
    out
}

/// Rename a relation's attributes in-process. Returns a fresh RC-managed
/// relation (rc=1) whose heading is `dst_desc` — the renamed, re-sorted
/// attributes — with each record's cells permuted from the source per `perm`.
/// `src` is left unchanged.
///
/// Renaming re-canonicalizes the heading (it is name-sorted), so record byte
/// offsets shift: `perm[dst_i]` is the index in `src_desc` of the source
/// attribute that becomes `dst_desc.attrs[dst_i]`. Cells are copied by value;
/// the result is then **sealed** to restore the canonical sort under the new
/// layout. (Dedup is a no-op — rename is a bijection, creating no duplicates —
/// but the byte order changes, so the sort must be redone.)
///
/// # Safety
/// `src` must point to a payload from `coddl_rc_alloc` (kind `Relation`,
/// descriptor `src_desc`). Both descriptors and `perm` (length `perm_count` ==
/// `dst_desc.attr_count`) must outlive this call.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_rename(
    src: *const u8,
    src_desc: *const CoddlHeadingDesc,
    dst_desc: *const CoddlHeadingDesc,
    perm_ptr: *const u32,
    perm_count: usize,
) -> *mut u8 {
    if src.is_null() || src_desc.is_null() || dst_desc.is_null() {
        return std::ptr::null_mut();
    }
    let header = src.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let count = (*header).length as usize;
    let src_record_size = (*src_desc).record_size as usize;
    let dst_record_size = (*dst_desc).record_size as usize;

    let src_attrs = std::slice::from_raw_parts((*src_desc).attrs, (*src_desc).attr_count as usize);
    let dst_attrs = std::slice::from_raw_parts((*dst_desc).attrs, (*dst_desc).attr_count as usize);
    let perm = if perm_ptr.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(perm_ptr, perm_count)
    };

    // Per-dst-attribute byte move `(src_offset, dst_offset, width)`, resolving
    // the source attribute via `perm`.
    let mut moves: Vec<(usize, usize, usize)> = Vec::with_capacity(dst_attrs.len());
    for (dst_i, d) in dst_attrs.iter().enumerate() {
        let src_i = perm.get(dst_i).copied().unwrap_or(0) as usize;
        if let Some(s) = src_attrs.get(src_i) {
            moves.push((s.offset as usize, d.offset as usize, cell_width(d.kind)));
        }
    }

    let out = crate::rc::coddl_rc_alloc(
        dst_record_size * count,
        count as u32,
        crate::rc::CoddlKind::Relation as u32,
        dst_desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }

    for i in 0..count {
        let src_rec = src.add(i * src_record_size);
        let dst_rec = out.add(i * dst_record_size);
        for &(src_off, dst_off, width) in &moves {
            std::ptr::copy_nonoverlapping(src_rec.add(src_off), dst_rec.add(dst_off), width);
        }
    }

    if dst_record_size == 0 {
        // Renaming an empty heading: collapse to reltrue/relfalse (seal can't
        // dedup zero-width records).
        let out_header = out.sub(HEADER_SIZE) as *mut CoddlRcHeader;
        (*out_header).length = u32::from(count > 0);
    } else {
        coddl_relation_seal(out, dst_desc);
    }
    out
}

/// Cardinality-checked collapse from a relation to a tuple (the TTM
/// RM Pre 10 primitive). Returns the single record's payload
/// pointer if the relation has exactly one row; otherwise writes a
/// diagnostic to stderr and `std::process::abort()`s. The caller
/// reads each attribute via the descriptor before releasing the
/// source relation.
///
/// # Safety
/// `src` must point to a payload returned by `coddl_rc_alloc` whose
/// kind is `Relation`. `desc` must outlive this call (typically a
/// read-only data symbol the codegen emitted).
#[no_mangle]
pub unsafe extern "C" fn coddl_extract_check_cardinality(
    src: *const u8,
    desc: *const CoddlHeadingDesc,
) -> *const u8 {
    if src.is_null() || desc.is_null() {
        eprintln!("coddl: extract: null relation or descriptor");
        std::process::abort();
    }
    let header = src.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let length = (*header).length;
    if length != 1 {
        eprintln!("coddl: extract: expected exactly 1 tuple, got {length}");
        std::process::abort();
    }
    // The first record sits at the payload pointer's base — the
    // payload already starts at record 0.
    src
}

pub(crate) unsafe fn drop_relation_payload(_payload: *mut u8, _header: &CoddlRcHeader) {
    // Phase 19: no heap cells to release. Future phases iterate
    // `header.length` records, look at the descriptor's per-attr
    // kind, and release nested heap pointers (Text owned, Relation,
    // Tuple-with-heap-content).
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rc::{coddl_rc_alloc, coddl_rc_release, CoddlKind};

    #[test]
    fn text_eq_compares_bytes_not_pointers() {
        unsafe {
            let a = b"hello world";
            let b = b"hello world"; // distinct allocation, equal contents
            assert_eq!(coddl_text_eq(a.as_ptr(), a.len(), b.as_ptr(), b.len()), 1);
            let shorter = b"hello";
            assert_eq!(
                coddl_text_eq(a.as_ptr(), a.len(), shorter.as_ptr(), shorter.len()),
                0
            );
            let same_len = b"hella world"; // same length, different byte
            assert_eq!(
                coddl_text_eq(a.as_ptr(), a.len(), same_len.as_ptr(), same_len.len()),
                0
            );
            // Both empty: pointers are never dereferenced.
            assert_eq!(coddl_text_eq(std::ptr::null(), 0, std::ptr::null(), 0), 1);
        }
    }

    #[test]
    fn seal_sorts_and_dedups_single_int_records() {
        // The `live_allocations()` counter is shared across all
        // parallel-running tests in this binary, so we can't assert
        // exact before/after balance here — `alloc_retain_release_balances`
        // owns the balance check inside a single test scope. This
        // test asserts the observable effects of seal: in-place sort
        // + adjacent dedup with the header length updated.
        let attrs = [CoddlAttrDesc {
            name: b"a".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: attrs.as_ptr(),
        };
        unsafe {
            let payload = coddl_rc_alloc(
                3 * 8,
                3,
                CoddlKind::Relation as u32,
                &desc as *const CoddlHeadingDesc,
            );
            assert!(!payload.is_null());
            // Write 3 records: 2, 1, 1 (will sort to 1, 1, 2; dedup to 1, 2).
            let slot = |idx: usize| payload.add(idx * 8) as *mut i64;
            std::ptr::write(slot(0), 2);
            std::ptr::write(slot(1), 1);
            std::ptr::write(slot(2), 1);
            coddl_relation_seal(payload, &desc as *const CoddlHeadingDesc);
            // Header length must now be 2.
            let header = payload.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 2);
            // Records sorted ascending.
            assert_eq!(std::ptr::read(slot(0)), 1);
            assert_eq!(std::ptr::read(slot(1)), 2);
            coddl_rc_release(payload);
        }
    }

    #[test]
    fn join_zero_shared_is_cartesian_product() {
        // Disjoint headings ⇒ no shared cells to match on, so the join is the
        // Cartesian product (`times`): every lhs row paired with every rhs row.
        // Locks in the vacuous-truth match in `coddl_relation_join` when the
        // shared-attribute count is 0. Tuple order is not meaningful (RM Pro 1),
        // so we compare the result as a set of (a, b) pairs.
        let lhs_attrs = [CoddlAttrDesc {
            name: b"a".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let lhs_desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: lhs_attrs.as_ptr(),
        };
        let rhs_attrs = [CoddlAttrDesc {
            name: b"b".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let rhs_desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: rhs_attrs.as_ptr(),
        };
        // Result heading `{a, b}` in canonical order: a at 0, b at 8.
        let res_attrs = [
            CoddlAttrDesc {
                name: b"a".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
            },
            CoddlAttrDesc {
                name: b"b".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
            },
        ];
        let res_desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 16,
            attrs: res_attrs.as_ptr(),
        };
        unsafe {
            // lhs { {a:1}, {a:2} }, rhs { {b:10}, {b:20} } (pre-sealed sets).
            let lhs = coddl_rc_alloc(2 * 8, 2, CoddlKind::Relation as u32, &lhs_desc);
            std::ptr::write(lhs.add(0) as *mut i64, 1);
            std::ptr::write(lhs.add(8) as *mut i64, 2);
            let rhs = coddl_rc_alloc(2 * 8, 2, CoddlKind::Relation as u32, &rhs_desc);
            std::ptr::write(rhs.add(0) as *mut i64, 10);
            std::ptr::write(rhs.add(8) as *mut i64, 20);

            let out = coddl_relation_join(lhs, &lhs_desc, rhs, &rhs_desc, &res_desc);
            assert!(!out.is_null());
            let len = (*(out.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
            assert_eq!(len, 4, "2 × 2 Cartesian product has 4 tuples");

            let mut pairs: Vec<(i64, i64)> = (0..len)
                .map(|i| {
                    let rec = out.add(i * 16);
                    (
                        std::ptr::read(rec as *const i64),
                        std::ptr::read(rec.add(8) as *const i64),
                    )
                })
                .collect();
            pairs.sort();
            assert_eq!(pairs, vec![(1, 10), (1, 20), (2, 10), (2, 20)]);

            coddl_rc_release(out);
            coddl_rc_release(rhs);
            coddl_rc_release(lhs);
        }
    }

    #[test]
    fn join_matches_shared_text_by_content_not_pointer() {
        // A join whose shared attribute is `Text` must match on string content,
        // not on the 16-byte (ptr, len) fat pointer: equal text from two
        // different constants has different pointers. This is the path
        // `intersect` (identical headings, so every attr shared) first exercises.
        // Heading {id: Integer, name: Text}: id@0 (8), name@8 (ptr@8, len@16); 24.
        let mk_attrs = || {
            [
                CoddlAttrDesc {
                    name: b"id".as_ptr(),
                    name_len: 2,
                    kind: CoddlAttrKind::Integer as u32,
                    offset: 0,
                },
                CoddlAttrDesc {
                    name: b"name".as_ptr(),
                    name_len: 4,
                    kind: CoddlAttrKind::Text as u32,
                    offset: 8,
                },
            ]
        };
        let lhs_attrs = mk_attrs();
        let rhs_attrs = mk_attrs();
        let res_attrs = mk_attrs();
        let desc = |a: &[CoddlAttrDesc]| CoddlHeadingDesc {
            attr_count: 2,
            record_size: 24,
            attrs: a.as_ptr(),
        };
        let lhs_desc = desc(&lhs_attrs);
        let rhs_desc = desc(&rhs_attrs);
        let res_desc = desc(&res_attrs);

        // Distinct heap allocations of "Grace" so the two cells hold different
        // pointers — a raw-byte compare of the fat pointer would miss the match.
        let grace_a: Vec<u8> = b"Grace".to_vec();
        let grace_b: Vec<u8> = b"Grace".to_vec();
        let zoe: Vec<u8> = b"Zoe".to_vec();
        assert_ne!(grace_a.as_ptr(), grace_b.as_ptr());

        unsafe {
            let write_row = |rec: *mut u8, id: i64, s: &[u8]| {
                std::ptr::write(rec as *mut i64, id);
                std::ptr::write(rec.add(8) as *mut usize, s.as_ptr() as usize);
                std::ptr::write(rec.add(16) as *mut usize, s.len());
            };
            // lhs { (2, grace_a) }; rhs { (2, grace_b), (5, zoe) }.
            let lhs = coddl_rc_alloc(24, 1, CoddlKind::Relation as u32, &lhs_desc);
            write_row(lhs, 2, &grace_a);
            let rhs = coddl_rc_alloc(2 * 24, 2, CoddlKind::Relation as u32, &rhs_desc);
            write_row(rhs, 2, &grace_b);
            write_row(rhs.add(24), 5, &zoe);

            let out = coddl_relation_join(lhs, &lhs_desc, rhs, &rhs_desc, &res_desc);
            assert!(!out.is_null());
            let len = (*(out.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
            assert_eq!(len, 1, "only (2, Grace) is in both operands");

            // The surviving row is (2, "Grace").
            let id = std::ptr::read(out as *const i64);
            let (ptr, slen) = read_text_cell(out, 8);
            let name = std::slice::from_raw_parts(ptr, slen);
            assert_eq!(id, 2);
            assert_eq!(name, b"Grace");

            coddl_rc_release(out);
            coddl_rc_release(rhs);
            coddl_rc_release(lhs);
        }
    }

    /// Predicate function used by `where_keeps_matching_records`.
    /// Returns 1 iff the i64 at offset 0 is `2`.
    extern "C" fn equals_two(record: *const u8) -> i8 {
        unsafe {
            let v = *(record as *const i64);
            if v == 2 {
                1
            } else {
                0
            }
        }
    }

    #[test]
    fn where_keeps_matching_records() {
        let attrs = [CoddlAttrDesc {
            name: b"a".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: attrs.as_ptr(),
        };
        unsafe {
            // Build source: { {a:1}, {a:2}, {a:3} } (pre-sealed).
            let src = coddl_rc_alloc(
                3 * 8,
                3,
                CoddlKind::Relation as u32,
                &desc as *const CoddlHeadingDesc,
            );
            std::ptr::write(src.add(0) as *mut i64, 1);
            std::ptr::write(src.add(8) as *mut i64, 2);
            std::ptr::write(src.add(16) as *mut i64, 3);

            let filtered = coddl_relation_where(src, &desc, equals_two);
            assert!(!filtered.is_null());
            let header = filtered.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 1);
            assert_eq!(std::ptr::read(filtered as *const i64), 2);

            coddl_rc_release(filtered);
            coddl_rc_release(src);
        }
    }

    /// `{a, b}` source descriptor: two Integer columns, canonical order.
    fn ab_desc() -> ([CoddlAttrDesc; 2], CoddlHeadingDesc) {
        let attrs = [
            CoddlAttrDesc {
                name: b"a".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
            },
            CoddlAttrDesc {
                name: b"b".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
            },
        ];
        // `attrs.as_ptr()` would dangle once the array moves out of this
        // fn; callers own the array and rebuild the desc from it.
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 16,
            attrs: std::ptr::null(),
        };
        (attrs, desc)
    }

    #[test]
    fn project_narrows_and_dedups() {
        // Keep `a`, drop `b`: three rows collapse to two distinct `a`s.
        let (src_attrs, mut src_desc) = ab_desc();
        src_desc.attrs = src_attrs.as_ptr();
        let dst_attrs = [CoddlAttrDesc {
            name: b"a".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let dst_desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: dst_attrs.as_ptr(),
        };
        unsafe {
            let src = coddl_rc_alloc(3 * 16, 3, CoddlKind::Relation as u32, &src_desc);
            let put = |row: usize, a: i64, b: i64| {
                std::ptr::write(src.add(row * 16) as *mut i64, a);
                std::ptr::write(src.add(row * 16 + 8) as *mut i64, b);
            };
            put(0, 1, 10);
            put(1, 1, 20); // same `a` as row 0 → duplicate after projection
            put(2, 2, 30);

            let out = coddl_relation_project(src, &src_desc, &dst_desc);
            assert!(!out.is_null());
            let header = out.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 2, "projection should dedup {{a:1}}");
            assert_eq!(std::ptr::read(out.add(0) as *const i64), 1);
            assert_eq!(std::ptr::read(out.add(8) as *const i64), 2);

            coddl_rc_release(out);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn project_resolves_offsets_by_name() {
        // Keep `b` (the *second* source column): the kept cell must be
        // read from `b`'s source offset (8) and written to dst offset 0.
        let (src_attrs, mut src_desc) = ab_desc();
        src_desc.attrs = src_attrs.as_ptr();
        let dst_attrs = [CoddlAttrDesc {
            name: b"b".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let dst_desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: dst_attrs.as_ptr(),
        };
        unsafe {
            let src = coddl_rc_alloc(2 * 16, 2, CoddlKind::Relation as u32, &src_desc);
            std::ptr::write(src.add(0) as *mut i64, 1);
            std::ptr::write(src.add(8) as *mut i64, 20);
            std::ptr::write(src.add(16) as *mut i64, 2);
            std::ptr::write(src.add(24) as *mut i64, 10);

            let out = coddl_relation_project(src, &src_desc, &dst_desc);
            assert!(!out.is_null());
            let header = out.sub(HEADER_SIZE) as *const CoddlRcHeader;
            // `b` values {20, 10} → sealed ascending {10, 20}.
            assert_eq!((*header).length, 2);
            assert_eq!(std::ptr::read(out.add(0) as *const i64), 10);
            assert_eq!(std::ptr::read(out.add(8) as *const i64), 20);

            coddl_rc_release(out);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn rename_permutes_and_reseals() {
        // {a, b} rename {a: z} → {b, z}: `z` gets `a`'s value, and the
        // canonical order flips (sorted by b,z instead of a,b) so seal re-sorts.
        let src_attrs = [
            CoddlAttrDesc {
                name: b"a".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
            },
            CoddlAttrDesc {
                name: b"b".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
            },
        ];
        let src_desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 16,
            attrs: src_attrs.as_ptr(),
        };
        // dst {b, z}: b@0, z@8.
        let dst_attrs = [
            CoddlAttrDesc {
                name: b"b".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
            },
            CoddlAttrDesc {
                name: b"z".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
            },
        ];
        let dst_desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 16,
            attrs: dst_attrs.as_ptr(),
        };
        // dst b ← src attr 1 (b); dst z ← src attr 0 (a).
        let perm: [u32; 2] = [1, 0];
        unsafe {
            let s = coddl_rc_alloc(2 * 16, 2, CoddlKind::Relation as u32, &src_desc);
            // sealed input: {a:1,b:5}, {a:2,b:3}
            std::ptr::write(s.add(0) as *mut i64, 1);
            std::ptr::write(s.add(8) as *mut i64, 5);
            std::ptr::write(s.add(16) as *mut i64, 2);
            std::ptr::write(s.add(24) as *mut i64, 3);

            let out = coddl_relation_rename(s, &src_desc, &dst_desc, perm.as_ptr(), 2);
            assert!(!out.is_null());
            let header = out.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 2);
            let read = |row: usize, off: usize| std::ptr::read(out.add(row * 16 + off) as *const i64);
            // re-sorted by (b, z): {b:3, z:2}, {b:5, z:1}
            assert_eq!(read(0, 0), 3, "b");
            assert_eq!(read(0, 8), 2, "z == a of the {{a:2}} row");
            assert_eq!(read(1, 0), 5, "b");
            assert_eq!(read(1, 8), 1, "z == a of the {{a:1}} row");

            coddl_rc_release(out);
            coddl_rc_release(s);
        }
    }

    #[test]
    fn project_to_empty_heading_collapses_to_reltrue() {
        // `project {}` over a multi-row relation: every row becomes the
        // empty tuple, so the set collapses to one (`reltrue`), not N.
        let (src_attrs, mut src_desc) = ab_desc();
        src_desc.attrs = src_attrs.as_ptr();
        // A zero-attribute heading still needs a valid (non-null, aligned)
        // attrs pointer — `slice::from_raw_parts` forbids null even at len 0.
        // The codegen emits exactly this (an empty `@.attrs.N` array).
        let dst_attrs: [CoddlAttrDesc; 0] = [];
        let dst_desc = CoddlHeadingDesc {
            attr_count: 0,
            record_size: 0,
            attrs: dst_attrs.as_ptr(),
        };
        unsafe {
            let src = coddl_rc_alloc(3 * 16, 3, CoddlKind::Relation as u32, &src_desc);
            for (row, (a, b)) in [(1i64, 10i64), (2, 20), (3, 30)].into_iter().enumerate() {
                std::ptr::write(src.add(row * 16) as *mut i64, a);
                std::ptr::write(src.add(row * 16 + 8) as *mut i64, b);
            }
            let out = coddl_relation_project(src, &src_desc, &dst_desc);
            assert!(!out.is_null());
            let header = out.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 1, "nullary projection is reltrue");
            coddl_rc_release(out);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn extract_success_returns_input_pointer() {
        // Single-row relation: extract returns the same pointer it
        // got. Abort paths can't be tested in-process; the e2e suite
        // covers them.
        let attrs = [CoddlAttrDesc {
            name: b"a".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: attrs.as_ptr(),
        };
        unsafe {
            let payload = coddl_rc_alloc(
                8,
                1,
                CoddlKind::Relation as u32,
                &desc as *const CoddlHeadingDesc,
            );
            std::ptr::write(payload as *mut i64, 42);
            let record_ptr = coddl_extract_check_cardinality(payload, &desc);
            assert_eq!(record_ptr, payload);
            assert_eq!(std::ptr::read(record_ptr as *const i64), 42);
            coddl_rc_release(payload);
        }
    }

    #[test]
    fn write_relation_prints_one_tuple_per_line() {
        // Smoke test: ensure `coddl_write_relation` doesn't UB on a
        // small payload. Output verification happens in the
        // driver-level e2e tests, which can capture stdout properly.
        let attrs = [CoddlAttrDesc {
            name: b"a".as_ptr(),
            name_len: 1,
            kind: CoddlAttrKind::Integer as u32,
            offset: 0,
        }];
        let desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 8,
            attrs: attrs.as_ptr(),
        };
        unsafe {
            let payload = coddl_rc_alloc(
                2 * 8,
                2,
                CoddlKind::Relation as u32,
                &desc as *const CoddlHeadingDesc,
            );
            std::ptr::write(payload.add(0) as *mut i64, 1);
            std::ptr::write(payload.add(8) as *mut i64, 2);
            coddl_write_relation(payload, &desc as *const CoddlHeadingDesc);
            coddl_rc_release(payload);
        }
    }
}

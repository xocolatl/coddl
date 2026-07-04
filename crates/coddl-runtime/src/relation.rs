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
    /// Inline nested-tuple cell: a contiguous sub-region. The attribute's
    /// `sub` descriptor describes the tuple's components (0-based offsets);
    /// the printer / comparator recurse through it.
    Tuple = 10,
    // Reserved: Relation = 11. Not yet emitted.
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
    /// For a `Tuple` cell (`kind == Tuple`): pointer to the nested heading
    /// descriptor for the inline sub-region (its attr offsets are 0-based
    /// within the sub-region). Null for scalar cells.
    pub sub: *const CoddlHeadingDesc,
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
/// — it brings equal records adjacent so one linear pass removes them.
/// Comparison is content-aware (`record_cmp`): `Text` cells compare by string
/// content, not by their `(ptr, len)` fat pointer, so a tuple present in two
/// independently-sourced operands (whose equal strings have *different*
/// pointers) is correctly deduped — this is what makes `union`'s concat+seal
/// correct. The resulting record order is an implementation byproduct, not
/// meaningful: a relation is a set with no tuple order (RM Pro 1), so callers
/// must not rely on it.
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

    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let payload = std::slice::from_raw_parts_mut(ptr, count * record_size);
    // The payload's cells are owned (retain-on-store / retain-on-copy ran before
    // this seal), so release each dropped duplicate's `Text` cells.
    let new_len = dedup_records(payload, record_size, attrs, true);
    (*header).length = new_len as u32;
}

/// Sort + adjacent-dedup a record buffer in place (content-aware via
/// `record_cmp`), returning the number of surviving (unique) records. The
/// first `new_len * record_size` bytes of `payload` hold the unique records in
/// sorted order; the tail is left as scratch. The caller updates any header
/// `length`.
///
/// Records compare via `record_cmp`: scalar cells by their fixed-width bytes,
/// `Text` cells by string content — never the `(ptr, len)` fat pointer, since
/// equal-content strings from different sources have different pointers
/// (relation-literal constants are not deduped across literals, and
/// `intern_string` is append-only). For an all-scalar record this matches a
/// whole-record byte compare. Shared by [`coddl_relation_seal`] and
/// [`coddl_relation_tclose`].
///
/// # Safety
/// `payload.len()` must be a multiple of `record_size > 0`, and every `Text`
/// cell's `(ptr, len)` must describe `len` readable bytes (or `len == 0`).
/// `release_dropped_text`: release each discarded duplicate's `Text` cells.
/// True from [`coddl_relation_seal`], where the payload's cells are *owned*
/// (each holds a per-slot reference from retain-on-store / retain-on-copy or a
/// fresh rc=1). False from [`coddl_relation_tclose`], whose intermediate
/// dedups run over *un-retained* working copies — releasing there would be an
/// over-release.
unsafe fn dedup_records(
    payload: &mut [u8],
    record_size: usize,
    attrs: &[CoddlAttrDesc],
    release_dropped_text: bool,
) -> usize {
    let count = payload.len() / record_size;
    if count <= 1 {
        return count;
    }

    // Sort record indices, permute into a fresh buffer, then drop adjacent
    // duplicates.
    let mut indices: Vec<usize> = (0..count).collect();
    indices.sort_by(|&a, &b| {
        let ra = &payload[a * record_size..(a + 1) * record_size];
        let rb = &payload[b * record_size..(b + 1) * record_size];
        record_cmp(ra, rb, attrs)
    });

    let mut sorted: Vec<u8> = Vec::with_capacity(count * record_size);
    for &i in &indices {
        sorted.extend_from_slice(&payload[i * record_size..(i + 1) * record_size]);
    }

    // Adjacent dedup (content-aware equality).
    let mut write_idx = 0usize;
    for read_idx in 0..count {
        if read_idx == 0 {
            write_idx = 1;
            continue;
        }
        let is_dup = {
            let prev = &sorted[(write_idx - 1) * record_size..write_idx * record_size];
            let cur = &sorted[read_idx * record_size..(read_idx + 1) * record_size];
            record_cmp(prev, cur, attrs) == std::cmp::Ordering::Equal
        };
        if !is_dup {
            if read_idx != write_idx {
                let (head, tail) = sorted.split_at_mut(read_idx * record_size);
                let dest = &mut head[write_idx * record_size..(write_idx + 1) * record_size];
                dest.copy_from_slice(&tail[..record_size]);
            }
            write_idx += 1;
        } else if release_dropped_text {
            // `cur` is a discarded duplicate — release the per-slot `Text`
            // reference it holds (equal-content cells can be distinct pointers,
            // so this releases `cur`'s own cells, not the survivor's). Survivor
            // slots keep their references and are copied back into `payload`.
            let rec = sorted[read_idx * record_size..].as_mut_ptr();
            release_record_text_cells(rec, attrs);
        }
    }

    // Copy the sorted+deduped records back into the payload.
    payload[..write_idx * record_size].copy_from_slice(&sorted[..write_idx * record_size]);
    write_idx
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

    // Shared attributes → equality test triples (lhs_off, rhs_off, lhs_attr_idx).
    // `cmp_cell` compares each shared cell content-aware: a Text cell is a
    // 16-byte (ptr, len) fat pointer and equal text from two string constants has
    // different pointers, and a tuple cell recurses into its components — so
    // shared cells are compared by content, not by their raw bytes.
    let mut shared: Vec<(usize, usize, usize)> = Vec::new();
    for (li, la) in lhs_attrs.iter().enumerate() {
        let lname = std::slice::from_raw_parts(la.name, la.name_len as usize);
        for ra in rhs_attrs {
            let rname = std::slice::from_raw_parts(ra.name, ra.name_len as usize);
            if lname == rname {
                shared.push((la.offset as usize, ra.offset as usize, li));
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
                moves.push((d.offset as usize, true, la.offset as usize, cell_width_desc(d)));
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
                moves.push((d.offset as usize, false, ra.offset as usize, cell_width_desc(d)));
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
            let lrec_s = std::slice::from_raw_parts(lrec, lhs_rec);
            let rrec_s = std::slice::from_raw_parts(rrec, rhs_rec);
            let mut matched = true;
            for &(loff, roff, li_attr) in &shared {
                // Same-named shared attrs have identical type; `lhs_attrs[li_attr]`
                // supplies the kind + sub-descriptor for the content-aware compare.
                if cmp_cell(lrec_s, loff, rrec_s, roff, &lhs_attrs[li_attr])
                    != std::cmp::Ordering::Equal
                {
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
    // Retain each copied `Text` cell before sealing — dedup's per-dropped-row
    // release then keeps the count balanced.
    retain_text_cells(out, written, result_desc);
    coddl_relation_seal(out, result_desc);
    out
}

/// Set union of two relations with identical headings (surface `union`,
/// Algebra-A OR restricted to matching headings). Concatenate both payloads
/// into a worst-case `lhs_count + rhs_count` buffer, then `coddl_relation_seal`
/// (content-aware sort + dedup) drops the cross-operand duplicates so the result
/// is a set (RM Pro 3). Identical headings ⇒ one shared descriptor. Text cells
/// copy by value — the `(ptr, len)` references immortal string data, so seal's
/// content-aware dedup is what makes a shared tuple collapse to one.
///
/// # Safety
/// All pointers must be non-null payloads / a descriptor from the runtime and
/// must outlive the call; both operands must share the `desc` layout.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_union(
    lhs: *const u8,
    rhs: *const u8,
    desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    if lhs.is_null() || rhs.is_null() || desc.is_null() {
        return std::ptr::null_mut();
    }
    let lhs_count = (*(lhs.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let rhs_count = (*(rhs.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let rec = (*desc).record_size as usize;
    let total = lhs_count + rhs_count;
    let out = crate::rc::coddl_rc_alloc(
        rec.saturating_mul(total),
        total as u32,
        crate::rc::CoddlKind::Relation as u32,
        desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }
    // Concatenate lhs records then rhs records; seal dedups the overlap.
    if lhs_count > 0 {
        std::ptr::copy_nonoverlapping(lhs, out, lhs_count * rec);
    }
    if rhs_count > 0 {
        std::ptr::copy_nonoverlapping(rhs, out.add(lhs_count * rec), rhs_count * rec);
    }
    // Retain each copied `Text` cell (both operands) before sealing; dedup's
    // per-dropped-row release balances the overlap it collapses.
    retain_text_cells(out, total, desc);
    coddl_relation_seal(out, desc);
    out
}

/// Set difference of two relations with identical headings (surface `minus`,
/// Algebra-A AND-NOT). Keep each `lhs` record that does **not** appear in `rhs`.
/// Membership is content-aware (`record_cmp`) — never a full-record byte
/// compare, since a `Text` cell is a `(ptr, len)` fat pointer and equal-content
/// strings from different sources have different pointers. **No re-seal:** the
/// result is a subset of the already-sealed `lhs`, so it stays sorted+unique
/// (same reasoning as `coddl_relation_where`).
///
/// # Safety
/// All pointers must be non-null payloads / a descriptor from the runtime and
/// must outlive the call; both operands must share the `desc` layout.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_minus(
    lhs: *const u8,
    rhs: *const u8,
    desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    if lhs.is_null() || rhs.is_null() || desc.is_null() {
        return std::ptr::null_mut();
    }
    let lhs_count = (*(lhs.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let rhs_count = (*(rhs.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    let rec = (*desc).record_size as usize;
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    // Worst case: every lhs record survives (rhs disjoint).
    let out = crate::rc::coddl_rc_alloc(
        rec.saturating_mul(lhs_count),
        0,
        crate::rc::CoddlKind::Relation as u32,
        desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }
    let mut written = 0usize;
    for li in 0..lhs_count {
        let lrec = std::slice::from_raw_parts(lhs.add(li * rec), rec);
        let in_rhs = (0..rhs_count).any(|ri| {
            let rrec = std::slice::from_raw_parts(rhs.add(ri * rec), rec);
            record_cmp(lrec, rrec, attrs) == std::cmp::Ordering::Equal
        });
        if !in_rhs {
            std::ptr::copy_nonoverlapping(lrec.as_ptr(), out.add(written * rec), rec);
            written += 1;
        }
    }
    (*(out.sub(HEADER_SIZE) as *mut CoddlRcHeader)).length = written as u32;
    // Retain each surviving `Text` cell (no seal — a subset of sealed lhs).
    retain_text_cells(out, written, desc);
    out
}

/// Transitive closure of a binary relation (surface `tclose`, Algebra-A
/// ◄TCLOSE►) — the one genuinely irreducible relational operator. The operand
/// is a relation of exactly two identically-typed attributes (typechecked);
/// treat `attrs[0]` as the source and `attrs[1]` as the target. The choice is
/// arbitrary because closure is **direction-agnostic**: `TC(reverse G) =
/// reverse(TC G)`, and writing the closure pair back into the `{a, b}` heading
/// undoes the very swap that reversing the pair performs, so the result is the
/// same relation either way — hence one `desc` for operand and result.
///
/// A naive fixpoint composes the accumulating result with the *original* edge
/// set: for a result pair `x → y` and an input edge `y → z`, add `x → z`;
/// repeat until a round adds nothing new. `R_{i+1} = R_i ∪ (R_i ∘ E)` converges
/// to `∪_{k≥1} E^k`, the transitive closure. Each round's additions are merged
/// and content-deduped (`dedup_records`), so the loop terminates — the pair set
/// is finite (bounded by distinct source×target pairs). Cell matching (`cell_eq`)
/// and dedup are content-aware (`Text` by content, not pointer), so closures
/// over string-keyed graphs are correct. 0 or 1 input edges → the input is its
/// own closure (the first round composes nothing new).
///
/// # Safety
/// `rel`/`desc` must be a non-null payload/descriptor from the runtime and must
/// outlive the call; `desc` must describe a binary (2-attribute) heading whose
/// two attributes share a kind.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_tclose(
    rel: *const u8,
    desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    if rel.is_null() || desc.is_null() {
        return std::ptr::null_mut();
    }
    let record_size = (*desc).record_size as usize;
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    let count = (*(rel.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
    // Defensive: a malformed (non-binary or zero-width) descriptor can't be
    // closed — copy the input through unchanged.
    if attrs.len() != 2 || record_size == 0 {
        let out = crate::rc::coddl_rc_alloc(
            record_size.saturating_mul(count),
            count as u32,
            CoddlKind::Relation as u32,
            desc,
        );
        if !out.is_null() && count > 0 {
            std::ptr::copy_nonoverlapping(rel, out, count * record_size);
        }
        return out;
    }
    let off_a = attrs[0].offset as usize; // source cell
    let off_b = attrs[1].offset as usize; // target cell
    // Both key attributes have identical type (typechecked); `attrs[0]` supplies
    // the kind + sub-descriptor for both the width and the content-aware match.
    let w = cell_width_desc(&attrs[0]);

    // The accumulating edge set, seeded with a copy of the input records.
    let mut result: Vec<u8> = std::slice::from_raw_parts(rel, count * record_size).to_vec();

    loop {
        let prev_len = result.len();
        let cur_count = prev_len / record_size;
        // Compose each accumulated pair (x → y) with each input edge (y → z).
        let mut round: Vec<u8> = Vec::new();
        for ri in 0..cur_count {
            let r = &result[ri * record_size..(ri + 1) * record_size];
            for ei in 0..count {
                let e = std::slice::from_raw_parts(rel.add(ei * record_size), record_size);
                // r's target (off_b) == e's source (off_a)? (content-aware,
                // tuple-aware via `cmp_cell`).
                if cmp_cell(r, off_b, e, off_a, &attrs[0]) == std::cmp::Ordering::Equal {
                    // New pair (x → z): r's source cell + e's target cell, into
                    // a zeroed record (padding bytes are never read by
                    // `record_cmp`, which walks only the two attribute cells).
                    let mut cand = vec![0u8; record_size];
                    cand[off_a..off_a + w].copy_from_slice(&r[off_a..off_a + w]);
                    cand[off_b..off_b + w].copy_from_slice(&e[off_b..off_b + w]);
                    round.extend_from_slice(&cand);
                }
            }
        }
        if round.is_empty() {
            break;
        }
        result.extend_from_slice(&round);
        // Working copies are un-retained, so dedup must not release dropped
        // cells here; the final output's cells are retained once below.
        let new_count = dedup_records(&mut result, record_size, attrs, false);
        result.truncate(new_count * record_size);
        if result.len() == prev_len {
            break; // fixpoint: the round added no pair that survived dedup
        }
    }

    let n = result.len() / record_size;
    let out = crate::rc::coddl_rc_alloc(
        record_size.saturating_mul(n),
        n as u32,
        CoddlKind::Relation as u32,
        desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }
    if n > 0 {
        std::ptr::copy_nonoverlapping(result.as_ptr(), out, n * record_size);
    }
    // The output's `Text` cells are copies of the (un-retained) working set —
    // retain each so the new relation co-owns it.
    retain_text_cells(out, n, desc);
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
            print_cell(&mut w, attr, record, 0);
        }
        let _ = w.write_all(b"}\n");
    }
}

/// Format one cell within a record to `w`. Dispatches on
/// `CoddlAttrKind`. Cells the printer doesn't recognize yet (Tuple,
/// Relation) print as `{...}` so the printer remains total.
unsafe fn print_cell<W: Write>(w: &mut W, attr: &CoddlAttrDesc, record: &[u8], base: usize) {
    let offset = base + attr.offset as usize;
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
    } else if attr.kind == CoddlAttrKind::Tuple as u32 {
        // Inline nested-tuple cell: recurse through the sub-descriptor at this
        // cell's base offset, rendering `{name: val, …}` in its (name-sorted)
        // order. The sub-attr offsets are 0-based within the sub-region, so the
        // recursion's base is this cell's record position.
        let _ = w.write_all(b"{");
        if !attr.sub.is_null() {
            let sub = &*attr.sub;
            let sub_attrs = std::slice::from_raw_parts(sub.attrs, sub.attr_count as usize);
            for (i, sa) in sub_attrs.iter().enumerate() {
                if i > 0 {
                    let _ = w.write_all(b", ");
                }
                let name_slice = std::slice::from_raw_parts(sa.name, sa.name_len as usize);
                let _ = w.write_all(name_slice);
                let _ = w.write_all(b": ");
                print_cell(w, sa, record, offset);
            }
        }
        let _ = w.write_all(b"}");
    } else {
        // Relation or future cells.
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

    // Retain each copied `Text` cell so the new relation co-owns it (no seal,
    // so the written count is final).
    retain_text_cells(out, written, desc);

    out
}

/// Build a widened relation by computing one or more new attributes per tuple
/// (surface `extend`). For each source record, `synth_fn(src_record,
/// dst_record)` fills the **entire** destination (result-heading) record — the
/// surviving source cells permuted to their result offsets, plus the computed
/// new cells — so this function stays oblivious to the layout (the synthesized
/// helper owns it). The result is re-sealed, because computing a column can
/// change sort order and can collapse formerly-distinct rows into duplicates
/// (RM Pro 3). `src` is left unchanged.
///
/// `Text` cell ownership: a computed cell (`synth_fn`'s `coddl_text_concat`
/// result) arrives at rc=1 and is moved into the record; a surviving source
/// cell is retained by `synth_fn` so the new relation co-owns it. The re-seal's
/// dedup releases any cell whose row it drops, and the drop walker releases the
/// survivors — so the relation owns exactly one reference per cell slot. See
/// `docs/memory.md`.
///
/// # Safety
/// `src` must point to a `Relation` payload whose header descriptor matches
/// `src_desc`. `result_desc` must describe the widened heading. `synth_fn` must
/// be safe to call with a readable source record and a writable
/// `result_desc.record_size`-byte destination record.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_extend(
    src: *const u8,
    src_desc: *const CoddlHeadingDesc,
    result_desc: *const CoddlHeadingDesc,
    synth_fn: extern "C" fn(*const u8, *mut u8),
) -> *mut u8 {
    if src.is_null() || src_desc.is_null() || result_desc.is_null() {
        return std::ptr::null_mut();
    }
    let header = src.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let count = (*header).length as usize;
    let src_record_size = (*src_desc).record_size as usize;
    let result_record_size = (*result_desc).record_size as usize;

    let out = crate::rc::coddl_rc_alloc(
        result_record_size * count,
        count as u32,
        crate::rc::CoddlKind::Relation as u32,
        result_desc,
    );
    if out.is_null() {
        return std::ptr::null_mut();
    }

    // Each helper call fills one full widened record from its source record.
    for i in 0..count {
        let src_record = src.add(i * src_record_size);
        let dst_record = out.add(i * result_record_size);
        (synth_fn)(src_record, dst_record);
    }

    // Re-seal: extend changes record content, so the sorted order may break and
    // distinct source rows may now coincide. `seal` sorts + content-aware
    // dedups and trims the header length.
    coddl_relation_seal(out, result_desc);

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

/// Concatenate two `Text` values (surface `||`) into a freshly allocated
/// heap `Text` payload. Returns the payload pointer; the caller pairs it with
/// the length `a_len + b_len` to form the `(ptr, len)` Text value (the runtime
/// can't return a fat pointer by value, so the length is recomputed at the
/// call site).
///
/// The result (rc=1) is reference-counted: the lowerer releases it at scope
/// exit / consumption, or, once stored into a relation cell, the relation drop
/// walker frees it (see `docs/memory.md`).
///
/// # Safety
/// Each `(ptr, len)` pair must describe `len` readable bytes (or `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn coddl_text_concat(
    a_ptr: *const u8,
    a_len: usize,
    b_ptr: *const u8,
    b_len: usize,
) -> *mut u8 {
    let total = a_len + b_len;
    let out = crate::rc::coddl_rc_alloc(
        total,
        total as u32,
        crate::rc::CoddlKind::Text as u32,
        std::ptr::null(),
    );
    if a_len > 0 {
        std::ptr::copy_nonoverlapping(a_ptr, out, a_len);
    }
    if b_len > 0 {
        std::ptr::copy_nonoverlapping(b_ptr, out.add(a_len), b_len);
    }
    out
}

/// UTF-8-encode a `Character` (Unicode scalar value) into a freshly allocated
/// heap `Text` payload. Returns the payload pointer; the caller pairs it with
/// [`coddl_utf8_len`] of the same codepoint to form the `(ptr, len)` Text.
/// Used to normalize a `Character` operand of `||` to `Text`. No caller-side
/// safety obligation — `cp` is a plain value — so this is a safe `extern "C"`.
///
/// Reference-counted like [`coddl_text_concat`]'s result.
#[no_mangle]
pub extern "C" fn coddl_char_to_text(cp: u32) -> *mut u8 {
    let mut buf = [0u8; 4];
    let bytes = match char::from_u32(cp) {
        Some(c) => c.encode_utf8(&mut buf).as_bytes(),
        // A `Character` is always a valid codepoint by construction; degrade
        // to an empty Text rather than panic across the FFI boundary.
        None => &[],
    };
    let n = bytes.len();
    // SAFETY: `coddl_rc_alloc` returns a fresh `n`-byte payload; we copy
    // exactly `n` bytes from the on-stack `buf` into it.
    unsafe {
        let out = crate::rc::coddl_rc_alloc(
            n,
            n as u32,
            crate::rc::CoddlKind::Text as u32,
            std::ptr::null(),
        );
        if n > 0 {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, n);
        }
        out
    }
}

/// The UTF-8 byte length (1–4) of a `Character`'s codepoint — the length that
/// pairs with [`coddl_char_to_text`]'s payload. A pure function (no
/// allocation) so codegen can obtain the Text length without a fat-pointer
/// return. Agrees byte-for-byte with `coddl_char_to_text` by construction
/// (`char::len_utf8` ↔ `char::encode_utf8`).
#[no_mangle]
pub extern "C" fn coddl_utf8_len(cp: u32) -> usize {
    char::from_u32(cp).map_or(0, |c| c.len_utf8())
}

/// Format an `Integer` (`i64` at the current machine representation) as a
/// decimal `Text`. The byte length crosses back through the trailing
/// `len_out` pointer — the fat-pointer-return convention of
/// [`coddl_read_line`](crate::coddl_read_line). The `to_text { self: Integer }`
/// overload (and `format` placeholders of Integer type) lower to a call here.
/// Reference-counted like [`coddl_text_concat`]'s result (rc=1).
///
/// # Safety
/// `len_out` must point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn coddl_int_to_text(n: i64, len_out: *mut usize) -> *mut u8 {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let out = crate::rc::coddl_rc_alloc(
        len,
        len as u32,
        crate::rc::CoddlKind::Text as u32,
        std::ptr::null(),
    );
    if len > 0 {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, len);
    }
    *len_out = len;
    out
}

/// Format a `Boolean` as the `Text` `"true"` / `"false"`. `b` is the `i8`
/// the backends use for `Boolean` (0 = false, non-zero = true). Same len-out
/// convention as [`coddl_int_to_text`]; rc=1 result.
///
/// # Safety
/// `len_out` must point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn coddl_bool_to_text(b: i8, len_out: *mut usize) -> *mut u8 {
    let bytes: &[u8] = if b != 0 { b"true" } else { b"false" };
    let len = bytes.len();
    let out = crate::rc::coddl_rc_alloc(
        len,
        len as u32,
        crate::rc::CoddlKind::Text as u32,
        std::ptr::null(),
    );
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, len);
    *len_out = len;
    out
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

/// Byte width of the cell described by `attr` — tuple-aware. A `Tuple` cell
/// occupies its inline sub-region (`sub.record_size`); every other cell uses
/// the scalar [`cell_width`]. Per-cell *copy* move-lists (project / rename /
/// join / tclose) must use this so a tuple cell copies as a whole blob rather
/// than being truncated to 8 bytes.
///
/// # Safety
/// For a `Tuple` cell, `attr.sub` must be null or a valid descriptor pointer.
unsafe fn cell_width_desc(attr: &CoddlAttrDesc) -> usize {
    if attr.kind == CoddlAttrKind::Tuple as u32 {
        if attr.sub.is_null() {
            0
        } else {
            (*attr.sub).record_size as usize
        }
    } else {
        cell_width(attr.kind)
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

/// Visit every `Text` leaf cell of the record at `rec`, recursing through
/// `Tuple` sub-regions, invoking `f` on each cell's payload pointer. `base` is
/// the record's absolute byte offset (0 for a top-level record; a tuple cell's
/// offset when recursing). The shared traversal behind retain-on-copy and the
/// drop / dedup release — mirrors the kind dispatch of [`print_cell`] /
/// [`cmp_cell`]. Integer/Boolean cells carry no heap pointer and are skipped.
///
/// # Safety
/// `rec` must hold a record whose layout matches `attrs` at `base`. The `Text`
/// payload pointer is handed to `f`, not dereferenced here.
unsafe fn walk_text_cells(
    rec: *mut u8,
    attrs: &[CoddlAttrDesc],
    base: usize,
    f: &mut impl FnMut(*mut u8),
) {
    for attr in attrs {
        let off = base + attr.offset as usize;
        if attr.kind == CoddlAttrKind::Text as u32 {
            let (ptr, _len) = read_text_cell(rec, off);
            f(ptr as *mut u8);
        } else if attr.kind == CoddlAttrKind::Tuple as u32 && !attr.sub.is_null() {
            let sub = &*attr.sub;
            let sub_attrs = std::slice::from_raw_parts(sub.attrs, sub.attr_count as usize);
            walk_text_cells(rec, sub_attrs, off, f);
        }
    }
}

/// Retain every `Text` cell across `count` records of `payload` (heading
/// `desc`). Called after a relation operator copies cells from its input(s) so
/// the freshly built relation co-owns each shared `Text` payload (immortal
/// literals see `rc == IMMORTAL_RC` and no-op). Must run **before** any
/// `coddl_relation_seal`, so dedup's per-dropped-row release stays balanced.
///
/// # Safety
/// `payload` must hold `count` records of `desc`'s layout, each cell a valid
/// `(ptr, len)` Text value.
unsafe fn retain_text_cells(payload: *mut u8, count: usize, desc: *const CoddlHeadingDesc) {
    if payload.is_null() || desc.is_null() {
        return;
    }
    let record_size = (*desc).record_size as usize;
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    for i in 0..count {
        walk_text_cells(payload.add(i * record_size), attrs, 0, &mut |p| {
            crate::rc::coddl_rc_retain(p)
        });
    }
}

/// Release every `Text` cell of the single record at `rec` (layout `attrs`).
/// Used by the relation drop walker and by [`dedup_records`] when discarding a
/// duplicate row — each balances one per-slot reference the relation took at
/// production (retain-on-copy / retain-on-store, or an rc=1 fresh cell).
///
/// # Safety
/// `rec` must hold one record of `attrs`' layout; cells must be valid Text.
unsafe fn release_record_text_cells(rec: *mut u8, attrs: &[CoddlAttrDesc]) {
    walk_text_cells(rec, attrs, 0, &mut |p| crate::rc::coddl_rc_release(p));
}

/// Compare a cell of `ra` (at absolute record offset `off_a`) against a cell of
/// `rb` (at `off_b`), where `attr` supplies the cell's kind (and sub-descriptor
/// for a `Tuple`). The two offsets may differ — the same attribute sits at
/// different offsets in two operands of a `join`. `Text` compares by string
/// *content* (not the `(ptr, len)` fat pointer); scalars by their fixed-width
/// bytes; a `Tuple` cell recurses over its inline sub-region's components,
/// advancing each side's offset (so two tuple cells with equal Text content but
/// different pointers compare `Equal`). Unifies the equality logic for
/// `record_cmp` (same offset both sides), `join` (shared key at differing
/// offsets), and `tclose` (edge match).
///
/// # Safety
/// `ra`/`rb` must hold a record whose layout matches `attr` (and its `sub`) at
/// the given offsets; `Text` cells must describe `len` readable bytes (or 0).
unsafe fn cmp_cell(
    ra: &[u8],
    off_a: usize,
    rb: &[u8],
    off_b: usize,
    attr: &CoddlAttrDesc,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if attr.kind == CoddlAttrKind::Text as u32 {
        let (pa, la) = read_text_cell(ra.as_ptr(), off_a);
        let (pb, lb) = read_text_cell(rb.as_ptr(), off_b);
        let sa: &[u8] = if la == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(pa, la)
        };
        let sb: &[u8] = if lb == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(pb, lb)
        };
        sa.cmp(sb)
    } else if attr.kind == CoddlAttrKind::Tuple as u32 {
        if attr.sub.is_null() {
            return Ordering::Equal;
        }
        let sub = &*attr.sub;
        let sub_attrs = std::slice::from_raw_parts(sub.attrs, sub.attr_count as usize);
        for sa in sub_attrs {
            let sub_off = sa.offset as usize;
            let ord = cmp_cell(ra, off_a + sub_off, rb, off_b + sub_off, sa);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    } else {
        let w = cell_width(attr.kind);
        ra[off_a..off_a + w].cmp(&rb[off_b..off_b + w])
    }
}

/// Total order on two records (`ra`, `rb`, same `attrs` layout) by walking
/// `attrs` and comparing each cell via [`cmp_cell`]. Two records compare
/// `Equal` iff every cell is content-equal — the basis for [`coddl_relation_seal`]'s
/// content-aware dedup. For an all-scalar record this is the old whole-record
/// byte comparison.
///
/// # Safety
/// `ra`/`rb` must each hold one record's bytes for `attrs`' layout; every
/// `Text` cell's `(ptr, len)` must describe `len` readable bytes (or `len == 0`).
unsafe fn record_cmp(ra: &[u8], rb: &[u8], attrs: &[CoddlAttrDesc]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for attr in attrs {
        let off = attr.offset as usize;
        let ord = cmp_cell(ra, off, rb, off, attr);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
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
            moves.push((s.offset as usize, d.offset as usize, cell_width_desc(d)));
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

    // Retain each copied `Text` cell before sealing (no-op when the heading is
    // empty — zero-width records carry no cells).
    retain_text_cells(out, count, dst_desc);

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

/// Force a relation into an ordered `Sequence` (RM Pro 7 iteration gate).
/// Returns a fresh RC-managed `Sequence` payload (rc=1) holding the source's
/// records **sorted by the order keys** — one tuple element per source tuple,
/// physically an unsealed relation payload (`CoddlKind::Sequence`) reusing the
/// source `rel_desc` (each element record *is* a source tuple). `rel` is left
/// unchanged; its cells are copied by value and the surviving `Text` cells are
/// retained so the sequence co-owns them.
///
/// `keys` is `key_count` bit-packed `u32`s, most-significant order key first:
/// the low 31 bits index `rel_desc.attrs[]` (canonical name-sorted order) and
/// bit 31, when set, marks that key **descending**. An empty `keys` (no `order`
/// clause) leaves the records in the source's sealed, unspecified order — the
/// sort becomes a stable no-op. Order keys are always scalar (T0082 rejects
/// relation- and tuple-valued keys), so each key's `&CoddlAttrDesc` drives
/// `cmp_cell` directly.
///
/// Unlike `coddl_relation_seal`, this does **not** dedup: a `Sequence` keeps
/// duplicates and position. Records equal on every order key keep their input
/// order (stable sort).
///
/// # Safety
/// `rel` must point to a payload from `coddl_rc_alloc` whose kind is `Relation`
/// and whose header carries `rel_desc`. `rel_desc` and `keys` (length
/// `key_count`) must outlive this call.
#[no_mangle]
pub unsafe extern "C" fn coddl_load_ordered(
    rel: *const u8,
    rel_desc: *const CoddlHeadingDesc,
    keys: *const u32,
    key_count: usize,
) -> *mut u8 {
    if rel.is_null() || rel_desc.is_null() {
        return std::ptr::null_mut();
    }
    let header = rel.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let count = (*header).length as usize;
    let record_size = (*rel_desc).record_size as usize;
    let attrs = std::slice::from_raw_parts((*rel_desc).attrs, (*rel_desc).attr_count as usize);

    // The result Sequence reuses the source descriptor — its element record is a
    // source tuple. Allocate up front; an empty relation yields an empty Sequence.
    let out = crate::rc::coddl_rc_alloc(
        record_size * count,
        count as u32,
        crate::rc::CoddlKind::Sequence as u32,
        rel_desc,
    );
    if out.is_null() || count == 0 {
        return out;
    }

    // `from_raw_parts` requires a non-null pointer even at length 0, so an empty
    // key list (no `order` clause) uses a borrowed empty slice.
    let key_specs: &[u32] = if key_count == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(keys, key_count)
    };
    let src = std::slice::from_raw_parts(rel, count * record_size);

    // Sort record indices by the order keys. `sort_by` is stable, so records
    // equal on every key keep their input order (and an empty `key_specs` makes
    // the comparator all-`Equal` → the whole sort is a no-op).
    let mut indices: Vec<usize> = (0..count).collect();
    indices.sort_by(|&a, &b| {
        let ra = &src[a * record_size..(a + 1) * record_size];
        let rb = &src[b * record_size..(b + 1) * record_size];
        for &packed in key_specs {
            let attr = &attrs[(packed & 0x7fff_ffff) as usize];
            let off = attr.offset as usize;
            let ord = cmp_cell(ra, off, rb, off, attr);
            let ord = if packed >> 31 == 1 { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });

    // Permute records into the sequence in sorted order, then retain each
    // surviving `Text` cell (the sequence co-owns the shared payloads; immortal
    // literals no-op on retain).
    for (pos, &i) in indices.iter().enumerate() {
        std::ptr::copy_nonoverlapping(
            rel.add(i * record_size),
            out.add(pos * record_size),
            record_size,
        );
    }
    retain_text_cells(out, count, rel_desc);
    out
}

/// Collect a `Sequence` back into a relation **set** (the reverse `load <relvar>
/// from <sequence>` form). Returns a fresh RC-managed relation (rc=1, kind
/// `Relation`) holding the sequence's element tuples with duplicates removed and
/// the canonical tuple order restored (RM Pro 1, 3) — the set-ifying inverse of
/// [`coddl_load_ordered`]. `seq` is left unchanged; its cells are copied by value
/// and the surviving `Text` cells are retained so the relation co-owns them.
///
/// `desc` describes the element-tuple layout (the sequence's records *are* tuples
/// of this heading, as produced by `coddl_load_ordered`). Records are copied,
/// their `Text` cells retained, then [`coddl_relation_seal`] sorts + dedups in
/// place (releasing each dropped duplicate's retained cells, so the reference
/// count stays balanced — the same copy-then-seal shape as
/// [`coddl_relation_rename`]). An empty sequence yields an empty relation.
///
/// # Safety
/// `seq` must point to a payload from `coddl_rc_alloc` whose kind is `Sequence`
/// and whose records match `desc`'s layout. `desc` must outlive this call.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_from_sequence(
    seq: *const u8,
    desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    if seq.is_null() || desc.is_null() {
        return std::ptr::null_mut();
    }
    let header = seq.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let count = (*header).length as usize;
    let record_size = (*desc).record_size as usize;

    let out = crate::rc::coddl_rc_alloc(
        record_size * count,
        count as u32,
        crate::rc::CoddlKind::Relation as u32,
        desc,
    );
    if out.is_null() || count == 0 {
        return out;
    }

    // Copy every record, retain its `Text` cells (the relation co-owns the shared
    // payloads), then seal — sort + dedup, releasing each dropped duplicate's
    // retained cells so the net references balance.
    std::ptr::copy_nonoverlapping(seq, out, count * record_size);
    retain_text_cells(out, count, desc);
    coddl_relation_seal(out, desc);
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
            moves.push((s.offset as usize, d.offset as usize, cell_width_desc(d)));
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

    // Retain each copied `Text` cell before sealing (no-op when the heading is
    // empty — zero-width records carry no cells).
    retain_text_cells(out, count, dst_desc);

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

/// One flattened leaf cell: `(name_ptr, name_len, absolute_offset, width)`.
type LeafCell = (*const u8, usize, usize, usize);

/// Flatten a heading descriptor to its leaf (scalar / Text) cells, recursing
/// into `Tuple` cells and adding each tuple's base offset. The offset returned
/// is absolute within a record; the width is the scalar/Text cell width.
///
/// # Safety
/// `desc` must be a valid heading descriptor (and its `sub` pointers valid).
unsafe fn flatten_leaves(desc: *const CoddlHeadingDesc, base: usize, out: &mut Vec<LeafCell>) {
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    for a in attrs {
        let off = base + a.offset as usize;
        if a.kind == CoddlAttrKind::Tuple as u32 {
            if !a.sub.is_null() {
                flatten_leaves(a.sub, off, out);
            }
        } else {
            out.push((a.name, a.name_len as usize, off, cell_width(a.kind)));
        }
    }
}

/// Restructure a relation from `src_desc`'s layout to `dst_desc`'s — the runtime
/// for surface `wrap` / `unwrap`. Both layouts hold the **same leaf cells**
/// (wrap/unwrap only regroup attributes into / out of tuple-valued attributes,
/// preserving every scalar value); the leaves are matched **by name** (names are
/// globally unique across the operation) and each is copied to its destination
/// offset. Returns a fresh RC-managed relation (rc=1); `src` is unchanged.
/// Mirrors `coddl_relation_project`, but at leaf granularity so a leaf that
/// moves into or out of a tuple sub-region is placed correctly.
///
/// # Safety
/// `src` must point to a `Relation` payload from `coddl_rc_alloc` whose header
/// carries `src_desc`; `src_desc`/`dst_desc` must outlive this call.
#[no_mangle]
pub unsafe extern "C" fn coddl_relation_restructure(
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

    let mut src_leaves: Vec<LeafCell> = Vec::new();
    flatten_leaves(src_desc, 0, &mut src_leaves);
    let mut dst_leaves: Vec<LeafCell> = Vec::new();
    flatten_leaves(dst_desc, 0, &mut dst_leaves);

    // Per-leaf byte move `(src_offset, dst_offset, width)`, matching by name.
    let mut moves: Vec<(usize, usize, usize)> = Vec::with_capacity(dst_leaves.len());
    for &(dname, dlen, doff, dwidth) in &dst_leaves {
        let dn = std::slice::from_raw_parts(dname, dlen);
        if let Some(&(_, _, soff, _)) = src_leaves
            .iter()
            .find(|&&(sname, slen, _, _)| std::slice::from_raw_parts(sname, slen) == dn)
        {
            moves.push((soff, doff, dwidth));
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
        for &(soff, doff, w) in &moves {
            std::ptr::copy_nonoverlapping(src_rec.add(soff), dst_rec.add(doff), w);
        }
    }

    // Retain each copied `Text` cell before sealing — `restructure` only moves
    // cells between flat and tuple-nested positions, so the leaf payloads (and
    // their pointers) are preserved and the new relation co-owns them.
    retain_text_cells(out, count, dst_desc);

    if dst_record_size == 0 {
        // All-empty result (e.g. wrap of everything into `Tuple {}`): seal can't
        // dedup zero-width records, so collapse to reltrue/relfalse by hand.
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

pub(crate) unsafe fn drop_relation_payload(payload: *mut u8, header: &CoddlRcHeader) {
    // Release each record's `Text` cells (recursing through tuple cells). Every
    // cell holds exactly one reference owned by this relation — taken at
    // production (retain-on-store for a literal, retain-on-copy for a relop, or
    // an rc=1 fresh cell from `extend`/SQLite) — so one release per cell slot
    // balances it. Immortal-literal cells see `rc == IMMORTAL_RC` and no-op.
    let desc = header.desc;
    if desc.is_null() {
        return;
    }
    let record_size = (*desc).record_size as usize;
    if record_size == 0 {
        return;
    }
    let count = header.length as usize;
    let attrs = std::slice::from_raw_parts((*desc).attrs, (*desc).attr_count as usize);
    for i in 0..count {
        release_record_text_cells(payload.add(i * record_size), attrs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rc::{coddl_rc_alloc, coddl_rc_release, CoddlKind, IMMORTAL_RC};

    /// Build an immortal-headed `Text` payload over `bytes` — exactly the shape
    /// the codegen backends emit for a string literal (a `CoddlRcHeader` with
    /// `rc = IMMORTAL_RC` ahead of the bytes). Returns the payload pointer.
    /// Now that the runtime RC-manages `Text` cells (retain-on-copy, drop
    /// walker, dedup release), a relation cell must point at a headered payload;
    /// a bare `&[u8]`/`Vec` pointer would make those calls read a bogus header.
    /// The backing block is leaked for the test's lifetime — immortal cells are
    /// never freed by the RC machinery, so this matches production behavior.
    ///
    /// # Safety
    /// The returned pointer is valid for the process lifetime (leaked).
    unsafe fn immortal_text(bytes: &[u8]) -> *const u8 {
        let total = HEADER_SIZE + bytes.len();
        let block = vec![0u8; total].into_boxed_slice();
        let raw = Box::leak(block).as_mut_ptr();
        std::ptr::write(
            raw as *mut CoddlRcHeader,
            CoddlRcHeader {
                rc: IMMORTAL_RC,
                desc: std::ptr::null(),
                kind: CoddlKind::Text as u32,
                length: bytes.len() as u32,
                capacity: bytes.len(),
            },
        );
        let payload = raw.add(HEADER_SIZE);
        if !bytes.is_empty() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), payload, bytes.len());
        }
        payload
    }

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
    fn text_concat_joins_bytes() {
        unsafe {
            let a = b"Hello, ";
            let b = b"world!";
            let out = coddl_text_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len());
            let n = a.len() + b.len();
            assert_eq!(std::slice::from_raw_parts(out, n), b"Hello, world!");
            // Release is possible (kind=Text frees the block); the *compiled
            // program* doesn't yet emit one — that's the documented leak.
            coddl_rc_release(out);
        }
    }

    #[test]
    fn text_concat_handles_empty_operands() {
        unsafe {
            let a = b"abc";
            let out = coddl_text_concat(a.as_ptr(), a.len(), std::ptr::null(), 0);
            assert_eq!(std::slice::from_raw_parts(out, a.len()), b"abc");
            coddl_rc_release(out);
            let empty = coddl_text_concat(std::ptr::null(), 0, std::ptr::null(), 0);
            coddl_rc_release(empty);
        }
    }

    #[test]
    fn char_to_text_and_utf8_len_agree_across_widths() {
        // 1/2/3/4-byte codepoints: 'A', 'é', '€', '😀'.
        for (cp, expect) in [
            ('A' as u32, "A".as_bytes()),
            ('é' as u32, "é".as_bytes()),
            ('€' as u32, "€".as_bytes()),
            ('😀' as u32, "😀".as_bytes()),
        ] {
            let len = coddl_utf8_len(cp);
            assert_eq!(len, expect.len(), "utf8_len for U+{cp:04X}");
            unsafe {
                let out = coddl_char_to_text(cp);
                assert_eq!(
                    std::slice::from_raw_parts(out, len),
                    expect,
                    "bytes for U+{cp:04X}"
                );
                coddl_rc_release(out);
            }
        }
    }

    #[test]
    fn int_to_text_formats_decimal() {
        for (n, expect) in [(0i64, "0"), (42, "42"), (-7, "-7"), (i64::MIN, "-9223372036854775808")] {
            unsafe {
                let mut len = 0usize;
                let out = coddl_int_to_text(n, &mut len);
                assert_eq!(std::slice::from_raw_parts(out, len), expect.as_bytes(), "int {n}");
                coddl_rc_release(out);
            }
        }
    }

    #[test]
    fn bool_to_text_formats_true_false() {
        unsafe {
            let mut len = 0usize;
            let t = coddl_bool_to_text(1, &mut len);
            assert_eq!(std::slice::from_raw_parts(t, len), b"true");
            coddl_rc_release(t);
            let f = coddl_bool_to_text(0, &mut len);
            assert_eq!(std::slice::from_raw_parts(f, len), b"false");
            coddl_rc_release(f);
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
            sub: std::ptr::null(),
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
    fn seal_dedups_text_by_content_not_pointer() {
        // Two records with equal Text content but DIFFERENT pointers (distinct
        // heap allocations) must dedup to one. A whole-record byte compare would
        // see the fat pointers differ and keep both — this is the hazard that
        // would make `union`'s concat+seal return a duplicate tuple.
        // Heading {name: Text}: name@0 (ptr@0, len@8); record_size 16.
        let attrs = [CoddlAttrDesc {
            name: b"name".as_ptr(),
            name_len: 4,
            kind: CoddlAttrKind::Text as u32,
            offset: 0,
            sub: std::ptr::null(),
        }];
        let desc = CoddlHeadingDesc {
            attr_count: 1,
            record_size: 16,
            attrs: attrs.as_ptr(),
        };
        let grace_a: Vec<u8> = b"Grace".to_vec();
        let grace_b: Vec<u8> = b"Grace".to_vec();
        assert_ne!(grace_a.as_ptr(), grace_b.as_ptr());
        unsafe {
            let payload = coddl_rc_alloc(
                2 * 16,
                2,
                CoddlKind::Relation as u32,
                &desc as *const CoddlHeadingDesc,
            );
            assert!(!payload.is_null());
            let write_row = |rec: *mut u8, s: &[u8]| {
                std::ptr::write(rec as *mut usize, immortal_text(s) as usize);
                std::ptr::write(rec.add(8) as *mut usize, s.len());
            };
            write_row(payload, &grace_a);
            write_row(payload.add(16), &grace_b);
            coddl_relation_seal(payload, &desc as *const CoddlHeadingDesc);
            let header = payload.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 1, "equal-content Text rows dedup to one");
            let (ptr, len) = read_text_cell(payload, 0);
            assert_eq!(std::slice::from_raw_parts(ptr, len), b"Grace");
            coddl_rc_release(payload);
        }
    }

    #[test]
    fn union_concats_and_dedups_cross_operand_text() {
        // lhs {(1,Ada),(2,grace_a)} ∪ rhs {(2,grace_b),(3,Zoe)} = {(1,Ada),(2,Grace),(3,Zoe)}.
        // The (2,Grace) tuple appears in both with DIFFERENT Text pointers; the
        // content-aware seal must collapse it to one row. Heading {id, name}:
        // id@0 (8), name@8 (ptr@8,len@16); record_size 24.
        let attrs = [
            CoddlAttrDesc {
                name: b"id".as_ptr(),
                name_len: 2,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"name".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 8,
                sub: std::ptr::null(),
            },
        ];
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 24,
            attrs: attrs.as_ptr(),
        };
        let ada: Vec<u8> = b"Ada".to_vec();
        let grace_a: Vec<u8> = b"Grace".to_vec();
        let grace_b: Vec<u8> = b"Grace".to_vec();
        let zoe: Vec<u8> = b"Zoe".to_vec();
        assert_ne!(grace_a.as_ptr(), grace_b.as_ptr());
        unsafe {
            let write_row = |rec: *mut u8, id: i64, s: &[u8]| {
                std::ptr::write(rec as *mut i64, id);
                std::ptr::write(rec.add(8) as *mut usize, immortal_text(s) as usize);
                std::ptr::write(rec.add(16) as *mut usize, s.len());
            };
            let lhs = coddl_rc_alloc(2 * 24, 2, CoddlKind::Relation as u32, &desc);
            write_row(lhs, 1, &ada);
            write_row(lhs.add(24), 2, &grace_a);
            let rhs = coddl_rc_alloc(2 * 24, 2, CoddlKind::Relation as u32, &desc);
            write_row(rhs, 2, &grace_b);
            write_row(rhs.add(24), 3, &zoe);

            let out = coddl_relation_union(lhs, rhs, &desc);
            assert!(!out.is_null());
            let len = (*(out.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
            assert_eq!(len, 3, "the shared (2, Grace) collapses to one row");

            // Collect the result as a set of (id, name).
            let mut got: Vec<(i64, Vec<u8>)> = (0..len)
                .map(|i| {
                    let rec = out.add(i * 24);
                    let id = std::ptr::read(rec as *const i64);
                    let (p, l) = read_text_cell(rec, 8);
                    (id, std::slice::from_raw_parts(p, l).to_vec())
                })
                .collect();
            got.sort();
            assert_eq!(
                got,
                vec![
                    (1, b"Ada".to_vec()),
                    (2, b"Grace".to_vec()),
                    (3, b"Zoe".to_vec()),
                ]
            );
            coddl_rc_release(out);
            coddl_rc_release(rhs);
            coddl_rc_release(lhs);
        }
    }

    #[test]
    fn minus_excludes_rhs_by_content_not_pointer() {
        // lhs {(1,Ada),(2,grace_a)} minus rhs {(2,grace_b),(3,Zoe)} = {(1,Ada)}.
        // (2,Grace) is in rhs with a DIFFERENT Text pointer; the content-aware
        // membership test must still exclude it. Heading {id, name}: id@0 (8),
        // name@8 (ptr@8,len@16); record_size 24.
        let attrs = [
            CoddlAttrDesc {
                name: b"id".as_ptr(),
                name_len: 2,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"name".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 8,
                sub: std::ptr::null(),
            },
        ];
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 24,
            attrs: attrs.as_ptr(),
        };
        let ada: Vec<u8> = b"Ada".to_vec();
        let grace_a: Vec<u8> = b"Grace".to_vec();
        let grace_b: Vec<u8> = b"Grace".to_vec();
        let zoe: Vec<u8> = b"Zoe".to_vec();
        assert_ne!(grace_a.as_ptr(), grace_b.as_ptr());
        unsafe {
            let write_row = |rec: *mut u8, id: i64, s: &[u8]| {
                std::ptr::write(rec as *mut i64, id);
                std::ptr::write(rec.add(8) as *mut usize, immortal_text(s) as usize);
                std::ptr::write(rec.add(16) as *mut usize, s.len());
            };
            let lhs = coddl_rc_alloc(2 * 24, 2, CoddlKind::Relation as u32, &desc);
            write_row(lhs, 1, &ada);
            write_row(lhs.add(24), 2, &grace_a);
            let rhs = coddl_rc_alloc(2 * 24, 2, CoddlKind::Relation as u32, &desc);
            write_row(rhs, 2, &grace_b);
            write_row(rhs.add(24), 3, &zoe);

            let out = coddl_relation_minus(lhs, rhs, &desc);
            assert!(!out.is_null());
            let len = (*(out.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
            assert_eq!(len, 1, "only (1, Ada) is in lhs but not rhs");
            let id = std::ptr::read(out as *const i64);
            let (p, l) = read_text_cell(out, 8);
            assert_eq!(id, 1);
            assert_eq!(std::slice::from_raw_parts(p, l), b"Ada");
            coddl_rc_release(out);
            coddl_rc_release(rhs);
            coddl_rc_release(lhs);
        }
    }

    #[test]
    fn tclose_computes_reachability_integer_chain() {
        // Edges {from, to} (both Integer): 1→2, 2→3. The transitive closure
        // adds 1→3, so {(1,2),(2,3),(1,3)}. Canonical heading order sorts
        // `from` < `to`: from@0 (8), to@8 (8); record_size 16. attrs[0]=from
        // (source), attrs[1]=to (target).
        let attrs = [
            CoddlAttrDesc {
                name: b"from".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"to".as_ptr(),
                name_len: 2,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
                sub: std::ptr::null(),
            },
        ];
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 16,
            attrs: attrs.as_ptr(),
        };
        unsafe {
            let write_edge = |rec: *mut u8, from: i64, to: i64| {
                std::ptr::write(rec as *mut i64, from);
                std::ptr::write(rec.add(8) as *mut i64, to);
            };
            let edges = coddl_rc_alloc(2 * 16, 2, CoddlKind::Relation as u32, &desc);
            write_edge(edges, 1, 2);
            write_edge(edges.add(16), 2, 3);

            let out = coddl_relation_tclose(edges, &desc);
            assert!(!out.is_null());
            let len = (*(out.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
            assert_eq!(len, 3, "closure of 1→2→3 adds 1→3");
            let mut got: Vec<(i64, i64)> = (0..len)
                .map(|i| {
                    let rec = out.add(i * 16);
                    (
                        std::ptr::read(rec as *const i64),
                        std::ptr::read(rec.add(8) as *const i64),
                    )
                })
                .collect();
            got.sort();
            assert_eq!(got, vec![(1, 2), (1, 3), (2, 3)]);
            coddl_rc_release(out);
            coddl_rc_release(edges);
        }
    }

    #[test]
    fn tclose_computes_reachability_text_keyed_by_content() {
        // A Text-keyed graph "a"→"b", "b"→"c" whose shared "b" nodes have
        // DISTINCT pointers (independent heap allocations). The closure must
        // add "a"→"c", which requires the cell match to compare Text by
        // CONTENT, not by the `(ptr, len)` fat pointer. Heading {from, to} Text:
        // from@0 (ptr@0,len@8), to@16 (ptr@16,len@24); record_size 32.
        let attrs = [
            CoddlAttrDesc {
                name: b"from".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"to".as_ptr(),
                name_len: 2,
                kind: CoddlAttrKind::Text as u32,
                offset: 16,
                sub: std::ptr::null(),
            },
        ];
        let desc = CoddlHeadingDesc {
            attr_count: 2,
            record_size: 32,
            attrs: attrs.as_ptr(),
        };
        let a: Vec<u8> = b"a".to_vec();
        let b_src: Vec<u8> = b"b".to_vec();
        let b_dst: Vec<u8> = b"b".to_vec();
        let c: Vec<u8> = b"c".to_vec();
        assert_ne!(b_src.as_ptr(), b_dst.as_ptr());
        unsafe {
            let write_edge = |rec: *mut u8, from: &[u8], to: &[u8]| {
                std::ptr::write(rec as *mut usize, immortal_text(from) as usize);
                std::ptr::write(rec.add(8) as *mut usize, from.len());
                std::ptr::write(rec.add(16) as *mut usize, immortal_text(to) as usize);
                std::ptr::write(rec.add(24) as *mut usize, to.len());
            };
            let edges = coddl_rc_alloc(2 * 32, 2, CoddlKind::Relation as u32, &desc);
            write_edge(edges, &a, &b_dst); // "a" → "b" (target uses b_dst)
            write_edge(edges.add(32), &b_src, &c); // "b" → "c" (source uses b_src)

            let out = coddl_relation_tclose(edges, &desc);
            assert!(!out.is_null());
            let len = (*(out.sub(HEADER_SIZE) as *const CoddlRcHeader)).length as usize;
            assert_eq!(len, 3, "closure adds \"a\"→\"c\" across distinct \"b\" pointers");
            let mut got: Vec<(Vec<u8>, Vec<u8>)> = (0..len)
                .map(|i| {
                    let rec = out.add(i * 32);
                    let (pf, lf) = read_text_cell(rec, 0);
                    let (pt, lt) = read_text_cell(rec, 16);
                    (
                        std::slice::from_raw_parts(pf, lf).to_vec(),
                        std::slice::from_raw_parts(pt, lt).to_vec(),
                    )
                })
                .collect();
            got.sort();
            assert_eq!(
                got,
                vec![
                    (b"a".to_vec(), b"b".to_vec()),
                    (b"a".to_vec(), b"c".to_vec()),
                    (b"b".to_vec(), b"c".to_vec()),
                ]
            );
            coddl_rc_release(out);
            coddl_rc_release(edges);
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
            sub: std::ptr::null(),
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
            sub: std::ptr::null(),
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
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"b".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
                sub: std::ptr::null(),
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
                    sub: std::ptr::null(),
                },
                CoddlAttrDesc {
                    name: b"name".as_ptr(),
                    name_len: 4,
                    kind: CoddlAttrKind::Text as u32,
                    offset: 8,
                    sub: std::ptr::null(),
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
                std::ptr::write(rec.add(8) as *mut usize, immortal_text(s) as usize);
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
            sub: std::ptr::null(),
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
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"b".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
                sub: std::ptr::null(),
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
            sub: std::ptr::null(),
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
            sub: std::ptr::null(),
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
        // {a, b} replace {z: a} → {b, z}: `z` gets `a`'s value, and the
        // canonical order flips (sorted by b,z instead of a,b) so seal re-sorts.
        let src_attrs = [
            CoddlAttrDesc {
                name: b"a".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 0,
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"b".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
                sub: std::ptr::null(),
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
                sub: std::ptr::null(),
            },
            CoddlAttrDesc {
                name: b"z".as_ptr(),
                name_len: 1,
                kind: CoddlAttrKind::Integer as u32,
                offset: 8,
                sub: std::ptr::null(),
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

    /// Helper used by `extend_widens_and_computes`: fills the widened record
    /// for source `{a, b}` → result `{a, b, c}` with `c = a + b`. Result order
    /// is canonical (a@0, b@8, c@16); the source layout is a@0, b@8.
    extern "C" fn extend_c_eq_a_plus_b(src: *const u8, dst: *mut u8) {
        unsafe {
            let a = std::ptr::read(src.add(0) as *const i64);
            let b = std::ptr::read(src.add(8) as *const i64);
            std::ptr::write(dst.add(0) as *mut i64, a);
            std::ptr::write(dst.add(8) as *mut i64, b);
            std::ptr::write(dst.add(16) as *mut i64, a + b);
        }
    }

    #[test]
    fn extend_widens_and_computes() {
        // {a, b} extend {c: a + b} → {a, b, c}. The helper fills each widened
        // record; the runtime allocates, loops, and re-seals.
        let src_attrs = [
            CoddlAttrDesc { name: b"a".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 0, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"b".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 8, sub: std::ptr::null() },
        ];
        let src_desc = CoddlHeadingDesc { attr_count: 2, record_size: 16, attrs: src_attrs.as_ptr() };
        // result {a, b, c}: a@0, b@8, c@16.
        let res_attrs = [
            CoddlAttrDesc { name: b"a".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 0, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"b".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 8, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"c".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 16, sub: std::ptr::null() },
        ];
        let res_desc = CoddlHeadingDesc { attr_count: 3, record_size: 24, attrs: res_attrs.as_ptr() };
        unsafe {
            let s = coddl_rc_alloc(2 * 16, 2, CoddlKind::Relation as u32, &src_desc);
            // sealed input: {a:1,b:2}, {a:3,b:1}
            std::ptr::write(s.add(0) as *mut i64, 1);
            std::ptr::write(s.add(8) as *mut i64, 2);
            std::ptr::write(s.add(16) as *mut i64, 3);
            std::ptr::write(s.add(24) as *mut i64, 1);

            let out = coddl_relation_extend(s, &src_desc, &res_desc, extend_c_eq_a_plus_b);
            assert!(!out.is_null());
            let header = out.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 2, "two distinct widened rows");
            let read = |row: usize, off: usize| std::ptr::read(out.add(row * 24 + off) as *const i64);
            // sorted by (a, b, c): {1, 2, 3} then {3, 1, 4}
            assert_eq!((read(0, 0), read(0, 8), read(0, 16)), (1, 2, 3));
            assert_eq!((read(1, 0), read(1, 8), read(1, 16)), (3, 1, 4));

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
            sub: std::ptr::null(),
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
            sub: std::ptr::null(),
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

    // ── inline nested-tuple cells ────────────────────────────────────

    /// Heading `{id: Integer, pt: Tuple{x: Integer, y: Integer}}` descriptor.
    /// Returns the (sub_attrs, sub_desc, top_attrs) so the caller keeps them
    /// alive; build the top `CoddlHeadingDesc` from `top_attrs`.
    fn nested_attrs() -> ([CoddlAttrDesc; 2], CoddlHeadingDesc, [CoddlAttrDesc; 2]) {
        // NOTE: sub_desc.attrs points at sub_attrs; the caller must keep the
        // returned tuple alive for as long as the descriptor is used.
        let sub_attrs = [
            CoddlAttrDesc { name: b"x".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 0, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"y".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 8, sub: std::ptr::null() },
        ];
        // sub_desc.attrs is filled in by the caller after the array settles.
        let sub_desc = CoddlHeadingDesc { attr_count: 2, record_size: 16, attrs: std::ptr::null() };
        let top_attrs = [
            CoddlAttrDesc { name: b"id".as_ptr(), name_len: 2, kind: CoddlAttrKind::Integer as u32, offset: 0, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"pt".as_ptr(), name_len: 2, kind: CoddlAttrKind::Tuple as u32, offset: 8, sub: std::ptr::null() },
        ];
        (sub_attrs, sub_desc, top_attrs)
    }

    #[test]
    fn print_cell_renders_nested_tuple() {
        let (sub_attrs, mut sub_desc, _top) = nested_attrs();
        sub_desc.attrs = sub_attrs.as_ptr();
        let pt_attr = CoddlAttrDesc {
            name: b"pt".as_ptr(),
            name_len: 2,
            kind: CoddlAttrKind::Tuple as u32,
            offset: 8,
            sub: &sub_desc as *const CoddlHeadingDesc,
        };
        // record: id@0 = 7, pt.x@8 = 1, pt.y@16 = 2 (24 bytes).
        let mut record = vec![0u8; 24];
        record[0..8].copy_from_slice(&7i64.to_ne_bytes());
        record[8..16].copy_from_slice(&1i64.to_ne_bytes());
        record[16..24].copy_from_slice(&2i64.to_ne_bytes());
        let mut buf: Vec<u8> = Vec::new();
        unsafe { print_cell(&mut buf, &pt_attr, &record, 0) };
        assert_eq!(buf, b"{x: 1, y: 2}");
    }

    #[test]
    fn seal_compares_full_tuple_cell_not_just_first_word() {
        // {id:1, pt:{x:1,y:2}} and {id:1, pt:{x:1,y:9}} differ ONLY in pt.y (the
        // tuple's 2nd word). A naive 8-byte cell compare would treat the pt cells
        // as equal and wrongly dedup; the recursive cmp must keep both records.
        let (sub_attrs, mut sub_desc, mut top_attrs) = nested_attrs();
        sub_desc.attrs = sub_attrs.as_ptr();
        top_attrs[1].sub = &sub_desc as *const CoddlHeadingDesc;
        let desc = CoddlHeadingDesc { attr_count: 2, record_size: 24, attrs: top_attrs.as_ptr() };
        unsafe {
            let payload = coddl_rc_alloc(2 * 24, 2, CoddlKind::Relation as u32, &desc);
            let write = |rec: usize, id: i64, x: i64, y: i64| {
                std::ptr::write(payload.add(rec * 24) as *mut i64, id);
                std::ptr::write(payload.add(rec * 24 + 8) as *mut i64, x);
                std::ptr::write(payload.add(rec * 24 + 16) as *mut i64, y);
            };
            write(0, 1, 1, 2);
            write(1, 1, 1, 9);
            coddl_relation_seal(payload, &desc);
            let header = payload.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 2, "records differing inside the tuple must not dedup");
            coddl_rc_release(payload);

            // Identical tuple cells → dedup to one.
            let payload2 = coddl_rc_alloc(2 * 24, 2, CoddlKind::Relation as u32, &desc);
            std::ptr::write(payload2.add(0) as *mut i64, 1);
            std::ptr::write(payload2.add(8) as *mut i64, 1);
            std::ptr::write(payload2.add(16) as *mut i64, 2);
            std::ptr::write(payload2.add(24) as *mut i64, 1);
            std::ptr::write(payload2.add(32) as *mut i64, 1);
            std::ptr::write(payload2.add(40) as *mut i64, 2);
            coddl_relation_seal(payload2, &desc);
            let header2 = payload2.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header2).length, 1, "identical tuple records must dedup");
            coddl_rc_release(payload2);
        }
    }

    #[test]
    fn restructure_wrap_then_unwrap_round_trips() {
        // flat {a@0, b@8, c@16} (size 24).
        let flat_attrs = [
            CoddlAttrDesc { name: b"a".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 0, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"b".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 8, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"c".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 16, sub: std::ptr::null() },
        ];
        let flat_desc = CoddlHeadingDesc { attr_count: 3, record_size: 24, attrs: flat_attrs.as_ptr() };
        // wrapped {c@0, t: Tuple{a@0, b@8}@8} (size 24) — name-sorted: c, t.
        let t_sub_attrs = [
            CoddlAttrDesc { name: b"a".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 0, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"b".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 8, sub: std::ptr::null() },
        ];
        let t_sub_desc = CoddlHeadingDesc { attr_count: 2, record_size: 16, attrs: t_sub_attrs.as_ptr() };
        let wrapped_attrs = [
            CoddlAttrDesc { name: b"c".as_ptr(), name_len: 1, kind: CoddlAttrKind::Integer as u32, offset: 0, sub: std::ptr::null() },
            CoddlAttrDesc { name: b"t".as_ptr(), name_len: 1, kind: CoddlAttrKind::Tuple as u32, offset: 8, sub: &t_sub_desc as *const CoddlHeadingDesc },
        ];
        let wrapped_desc = CoddlHeadingDesc { attr_count: 2, record_size: 24, attrs: wrapped_attrs.as_ptr() };
        unsafe {
            let flat = coddl_rc_alloc(24, 1, CoddlKind::Relation as u32, &flat_desc);
            std::ptr::write(flat.add(0) as *mut i64, 1); // a
            std::ptr::write(flat.add(8) as *mut i64, 2); // b
            std::ptr::write(flat.add(16) as *mut i64, 3); // c

            // wrap: leaves a→t.a (off 8), b→t.b (off 16), c→c (off 0).
            let wrapped = coddl_relation_restructure(flat, &flat_desc, &wrapped_desc);
            assert_eq!(std::ptr::read(wrapped.add(0) as *const i64), 3, "c at front");
            assert_eq!(std::ptr::read(wrapped.add(8) as *const i64), 1, "t.a");
            assert_eq!(std::ptr::read(wrapped.add(16) as *const i64), 2, "t.b");

            // unwrap: back to the flat layout.
            let back = coddl_relation_restructure(wrapped, &wrapped_desc, &flat_desc);
            assert_eq!(std::ptr::read(back.add(0) as *const i64), 1, "a");
            assert_eq!(std::ptr::read(back.add(8) as *const i64), 2, "b");
            assert_eq!(std::ptr::read(back.add(16) as *const i64), 3, "c");

            coddl_rc_release(flat);
            coddl_rc_release(wrapped);
            coddl_rc_release(back);
        }
    }

    // Store `{a, b}` Integer rows into a fresh (unsealed) relation — the caller
    // owns `desc` (and its backing attrs array), so the descriptor pointer stays
    // valid for the whole test. No seal: the test controls input row order, so a
    // stable sort's tie-breaking is observable.
    unsafe fn ab_src(rows: &[(i64, i64)], desc: &CoddlHeadingDesc) -> *mut u8 {
        let src = coddl_rc_alloc(16 * rows.len(), rows.len() as u32, CoddlKind::Relation as u32, desc);
        for (i, &(a, b)) in rows.iter().enumerate() {
            std::ptr::write(src.add(i * 16) as *mut i64, a);
            std::ptr::write(src.add(i * 16 + 8) as *mut i64, b);
        }
        src
    }

    unsafe fn ab_row(seq: *const u8, i: usize) -> (i64, i64) {
        (
            std::ptr::read(seq.add(i * 16) as *const i64),
            std::ptr::read(seq.add(i * 16 + 8) as *const i64),
        )
    }

    #[test]
    fn load_ordered_sorts_by_single_key_and_is_stable() {
        unsafe {
            // Rows chosen so the `a`-key has ties whose `b` order differs from
            // any sort — a stable sort must keep the two `a==1` (and two `a==2`)
            // rows in their input order.
            let (attrs, mut desc) = ab_desc();
            desc.attrs = attrs.as_ptr();
            let src = ab_src(&[(2, 1), (1, 2), (2, 0), (1, 5)], &desc);

            let asc = coddl_load_ordered(src, &desc, [0u32].as_ptr(), 1);
            let header = asc.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).kind, CoddlKind::Sequence as u32, "result is a Sequence");
            assert_eq!((*header).length, 4, "no dedup — every row kept");
            assert_eq!(
                [ab_row(asc, 0), ab_row(asc, 1), ab_row(asc, 2), ab_row(asc, 3)],
                [(1, 2), (1, 5), (2, 1), (2, 0)],
                "asc by a; ties keep input order",
            );

            // Descending flips the key groups but keeps each group's input order.
            let desc_seq = coddl_load_ordered(src, &desc, [0x8000_0000u32].as_ptr(), 1);
            assert_eq!(
                [ab_row(desc_seq, 0), ab_row(desc_seq, 1), ab_row(desc_seq, 2), ab_row(desc_seq, 3)],
                [(2, 1), (2, 0), (1, 2), (1, 5)],
            );

            coddl_rc_release(asc);
            coddl_rc_release(desc_seq);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn load_ordered_multi_key() {
        unsafe {
            // Order by `a` asc, then `b` desc — key indices [a=0, b=1|desc].
            let (attrs, mut desc) = ab_desc();
            desc.attrs = attrs.as_ptr();
            let src = ab_src(&[(1, 2), (1, 5), (2, 1), (2, 0)], &desc);
            let keys = [0u32, 1u32 | 0x8000_0000];
            let out = coddl_load_ordered(src, &desc, keys.as_ptr(), keys.len());
            assert_eq!(
                [ab_row(out, 0), ab_row(out, 1), ab_row(out, 2), ab_row(out, 3)],
                [(1, 5), (1, 2), (2, 1), (2, 0)],
            );
            coddl_rc_release(out);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn load_ordered_no_keys_preserves_input_order() {
        unsafe {
            let (attrs, mut desc) = ab_desc();
            desc.attrs = attrs.as_ptr();
            let src = ab_src(&[(2, 0), (1, 0), (3, 0)], &desc);
            let out = coddl_load_ordered(src, &desc, std::ptr::null(), 0);
            let header = out.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).length, 3);
            assert_eq!(
                [ab_row(out, 0).0, ab_row(out, 1).0, ab_row(out, 2).0],
                [2, 1, 3],
                "no `order` clause leaves input order untouched",
            );
            coddl_rc_release(out);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn load_ordered_sorts_text_by_content() {
        unsafe {
            let attrs = [CoddlAttrDesc {
                name: b"name".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 0,
                sub: std::ptr::null(),
            }];
            let desc = CoddlHeadingDesc {
                attr_count: 1,
                record_size: 16,
                attrs: attrs.as_ptr(),
            };
            let names: [&[u8]; 3] = [b"beta", b"alpha", b"gamma"];
            let src = coddl_rc_alloc(16 * 3, 3, CoddlKind::Relation as u32, &desc);
            for (i, n) in names.iter().enumerate() {
                std::ptr::write(src.add(i * 16) as *mut *const u8, immortal_text(n));
                std::ptr::write(src.add(i * 16 + 8) as *mut usize, n.len());
            }

            let out = coddl_load_ordered(src, &desc, [0u32].as_ptr(), 1);
            let read = |i: usize| -> Vec<u8> {
                let p = std::ptr::read(out.add(i * 16) as *const *const u8);
                let l = std::ptr::read(out.add(i * 16 + 8) as *const usize);
                std::slice::from_raw_parts(p, l).to_vec()
            };
            assert_eq!(read(0), b"alpha");
            assert_eq!(read(1), b"beta");
            assert_eq!(read(2), b"gamma");

            coddl_rc_release(out);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn load_ordered_empty_relation_yields_empty_sequence() {
        unsafe {
            let (attrs, mut desc) = ab_desc();
            desc.attrs = attrs.as_ptr();
            let src = ab_src(&[], &desc);
            let out = coddl_load_ordered(src, &desc, [0u32].as_ptr(), 1);
            assert!(!out.is_null());
            let header = out.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).kind, CoddlKind::Sequence as u32);
            assert_eq!((*header).length, 0);
            coddl_rc_release(out);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn relation_from_sequence_dedups_and_seals() {
        unsafe {
            // A Sequence with duplicate rows in arbitrary order (built via the
            // no-key `coddl_load_ordered`, which never dedups). Collecting it back
            // must sort + drop duplicates (RM Pro 1, 3): the reverse `load`.
            let (attrs, mut desc) = ab_desc();
            desc.attrs = attrs.as_ptr();
            let src = ab_src(&[(2, 1), (1, 2), (2, 1), (1, 2)], &desc);
            let seq = coddl_load_ordered(src, &desc, std::ptr::null(), 0);
            assert_eq!((*(seq.sub(HEADER_SIZE) as *const CoddlRcHeader)).length, 4);

            let rel = coddl_relation_from_sequence(seq, &desc);
            let header = rel.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).kind, CoddlKind::Relation as u32, "result is a Relation");
            assert_eq!((*header).length, 2, "duplicates dropped");
            assert_eq!(
                [ab_row(rel, 0), ab_row(rel, 1)],
                [(1, 2), (2, 1)],
                "sorted into the canonical order",
            );

            coddl_rc_release(rel);
            coddl_rc_release(seq);
            coddl_rc_release(src);
        }
    }

    #[test]
    fn relation_from_sequence_empty_yields_empty_relation() {
        unsafe {
            let (attrs, mut desc) = ab_desc();
            desc.attrs = attrs.as_ptr();
            let src = ab_src(&[], &desc);
            let seq = coddl_load_ordered(src, &desc, std::ptr::null(), 0);
            let rel = coddl_relation_from_sequence(seq, &desc);
            assert!(!rel.is_null());
            let header = rel.sub(HEADER_SIZE) as *const CoddlRcHeader;
            assert_eq!((*header).kind, CoddlKind::Relation as u32);
            assert_eq!((*header).length, 0);
            coddl_rc_release(rel);
            coddl_rc_release(seq);
            coddl_rc_release(src);
        }
    }
}

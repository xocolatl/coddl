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
//! [`coddl_relation_seal`] sorts records by byte-wise comparison
//! (total because the layout is canonical) then adjacent-dedups in
//! place by trimming the header's `length`. The sort is unspecified
//! beyond "total and deterministic" — backends rely on the same
//! sort so cross-backend stdout matches byte-for-byte.
//!
//! ## Printer
//!
//! [`coddl_write_relation`] prints one tuple per line as
//! `{name: value, name: value}\n`. Attributes appear in canonical
//! heading order (matching the descriptor). Tuple and Relation cells
//! print as `{...}` placeholders in Phase 19; the recursive printer
//! lands when nested compound types become a real workflow.

use std::io::Write;

use crate::rc::{CoddlRcHeader, HEADER_SIZE};

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

/// Sort + adjacent-dedup a relation's payload in place. Updates the
/// header's `length` to reflect dedup. After this returns, the
/// relation upholds RM Pro 3 (no duplicates) and presents records
/// in a total deterministic order.
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

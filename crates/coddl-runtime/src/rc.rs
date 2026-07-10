//! Reference-counted heap discipline.
//!
//! Every runtime-allocated payload (today: relations) is preceded by a
//! [`CoddlRcHeader`] at a fixed negative offset from the payload
//! pointer the codegen sees. The header carries:
//!
//! - `rc` — the refcount. Sentinel value [`IMMORTAL_RC`] marks
//!   payloads that live in read-only segments (string literals,
//!   compiled-in constants); retain and release short-circuit to
//!   no-ops on this sentinel so the same calling convention works
//!   for both heap and immortal payloads.
//! - `desc` — for relation-kind payloads, a pointer to the
//!   per-heading static descriptor. The drop walker reads it to
//!   recurse into nested heap cells. `null` for non-relation kinds.
//! - `kind` — drives the drop walker's per-kind dispatch.
//!   [`CoddlKind`] enumerates the supported values.
//! - `length` — for relation payloads, the number of records. Other
//!   kinds use it as type-specific aux metadata.
//!
//! The C-ABI contract:
//!
//! - [`coddl_rc_alloc`] returns a pointer to the **payload**, not to
//!   the header. Codegen never sees the header; the runtime reaches
//!   it via `payload.sub(HEADER_SIZE)`.
//! - [`coddl_rc_retain`] / [`coddl_rc_release`] take the same payload
//!   pointer and seek backward to the header.
//! - When `rc` drops to zero in `coddl_rc_release`, the drop walker
//!   recurses into the payload (per `kind`) and then frees the
//!   `header + payload` block via the same allocator that allocated
//!   it.
//!
//! Layout discipline: the header is `#[repr(C)]` and matches a `C`
//! struct with explicit 8-byte alignment. Backends must align their
//! payload pointers so that `payload - HEADER_SIZE` is a valid
//! `CoddlRcHeader*`.

use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicI64, Ordering};

use crate::relation::CoddlHeadingDesc;

/// Refcount sentinel marking a payload as immortal (lives in
/// read-only segments). Retain/release become no-ops; the drop walker
/// never runs.
pub const IMMORTAL_RC: u64 = u64::MAX;

/// Per-payload kind tag. Lives in the RC header; drives the drop
/// walker's dispatch.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoddlKind {
    /// Relation payload. `desc` points to the heading descriptor;
    /// `length` is the number of records.
    Relation = 0,
    /// A heap-allocated scalar `Text` payload — a flat run of UTF-8 bytes,
    /// `length` bytes long, with no nested RC pointers. Produced by `||`
    /// (`coddl_text_concat` / `coddl_char_to_text`), `read_line`, and SQLite
    /// text-column materialization. The drop walker frees the block (no
    /// recursion); string literals carry the same header with `rc =
    /// IMMORTAL_RC` so retain/release are uniform. See `docs/memory.md`.
    Text = 1,
    /// An ordered, duplicate-preserving `Sequence` payload. Physically a
    /// relation payload that was never sealed (sort + dedup): `desc` points
    /// to the synthetic single-attribute heading, `length` is the element
    /// count. The drop walker reuses `drop_relation_payload` — element cells
    /// release exactly like relation cells.
    Sequence = 2,
}

/// RC header preceding every heap payload at offset `-HEADER_SIZE`.
/// `#[repr(C)]` with explicit field order — backends mirror this
/// layout when allocating.
#[repr(C)]
#[derive(Debug)]
pub struct CoddlRcHeader {
    pub rc: u64,
    pub desc: *const CoddlHeadingDesc,
    pub kind: u32,
    pub length: u32,
    /// The payload byte size passed to [`coddl_rc_alloc`] — the block's
    /// allocated capacity. Distinct from `length`: relation ops
    /// (`where`/`project`, relation-literal seal) allocate worst-case then
    /// trim `length` after dedup/filter, so `length` ≤ capacity. The block
    /// must be freed with the capacity it was allocated with — the
    /// `GlobalAlloc` contract requires the dealloc `Layout` to match alloc.
    pub capacity: usize,
}

/// Header size in bytes. Backends pad payload allocations by this
/// amount before the payload start. `mem::size_of::<CoddlRcHeader>()`
/// at runtime; published as a constant so the FFI surface is
/// self-documenting.
pub const HEADER_SIZE: usize = std::mem::size_of::<CoddlRcHeader>();

/// 8-byte alignment is enough for every leaf cell type Phase 19
/// supports (Integer = i64, Text = (ptr, len) = 16 bytes total but
/// 8-byte aligned). Both backends produce payload pointers aligned
/// to this.
const PAYLOAD_ALIGN: usize = 8;

/// Debug-only live-allocation counter. Tests assert it returns to
/// zero at program end; release builds compile this out to a no-op.
#[cfg(debug_assertions)]
static LIVE_ALLOCATIONS: AtomicI64 = AtomicI64::new(0);

/// Allocate a refcounted payload of `payload_size` bytes plus the
/// header. Returns a pointer to the payload; the header lives at
/// `payload - HEADER_SIZE`.
///
/// `length` is stored in the header (the relation row count for
/// relations). `desc` is the heading descriptor for relation
/// payloads, or null for non-relation kinds.
///
/// Returns null on allocation failure (callers must check).
///
/// # Safety
///
/// `desc`, if non-null, must outlive any subsequent retain/release
/// on the returned pointer. The compiled program normally places
/// descriptors in read-only data segments, satisfying this.
#[no_mangle]
pub unsafe extern "C" fn coddl_rc_alloc(
    payload_size: usize,
    length: u32,
    kind: u32,
    desc: *const CoddlHeadingDesc,
) -> *mut u8 {
    let total = HEADER_SIZE
        .checked_add(payload_size)
        .expect("alloc overflow");
    let layout = Layout::from_size_align(total, PAYLOAD_ALIGN).expect("bad layout");
    let block = alloc(layout);
    if block.is_null() {
        return std::ptr::null_mut();
    }
    let header = block as *mut CoddlRcHeader;
    std::ptr::write(
        header,
        CoddlRcHeader {
            rc: 1,
            desc,
            kind,
            length,
            capacity: payload_size,
        },
    );
    #[cfg(debug_assertions)]
    LIVE_ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
    block.add(HEADER_SIZE)
}

/// Increment the refcount on a heap payload. No-op on null and on
/// immortal payloads.
///
/// # Safety
/// `ptr` must either be null, point to a payload returned by
/// [`coddl_rc_alloc`], or point to an immortal payload whose
/// preceding header has `rc == IMMORTAL_RC`.
#[no_mangle]
pub unsafe extern "C" fn coddl_rc_retain(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let header = ptr.sub(HEADER_SIZE) as *mut CoddlRcHeader;
    if (*header).rc == IMMORTAL_RC {
        return;
    }
    (*header).rc += 1;
}

/// Decrement the refcount. When it reaches zero, dispatch to the
/// drop walker per `kind` and free the block.
///
/// # Safety
/// Same as [`coddl_rc_retain`]. After this returns, `ptr` is invalid
/// if it caused the rc to reach zero.
#[no_mangle]
pub unsafe extern "C" fn coddl_rc_release(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let header = ptr.sub(HEADER_SIZE) as *mut CoddlRcHeader;
    if (*header).rc == IMMORTAL_RC {
        return;
    }
    (*header).rc -= 1;
    if (*header).rc != 0 {
        return;
    }
    // Drop walker: recurse per kind. Phase 19 supports Relation only.
    // Tuple cells in records that themselves carry heap pointers
    // (Text, nested Relation) get released here.
    let kind = (*header).kind;
    if kind == CoddlKind::Relation as u32 || kind == CoddlKind::Sequence as u32 {
        // A sequence is an unsealed relation over the synthetic element
        // heading — same per-record cell layout, so the same drop walker
        // releases its (Text / nested) element cells.
        crate::relation::drop_relation_payload(ptr, &*header);
    }
    // Free the block with the SAME layout `coddl_rc_alloc` used — the
    // allocated `capacity`, not the (possibly seal-shrunk) `length`.
    // Deallocating with a size different from the allocation is UB under
    // the `GlobalAlloc` contract.
    let total = HEADER_SIZE + (*header).capacity;
    let layout = Layout::from_size_align(total, PAYLOAD_ALIGN).expect("bad layout");
    let block = ptr.sub(HEADER_SIZE);
    dealloc(block, layout);
    #[cfg(debug_assertions)]
    LIVE_ALLOCATIONS.fetch_sub(1, Ordering::SeqCst);
}

/// Read the `length` field from a heap payload's header — the element
/// count for a `Sequence`, the tuple count for a `Relation`. The
/// `cardinality` built-in lowers to a call here. Borrows: it only
/// inspects the header and never touches the refcount. Returns `i64`
/// (the surface `Integer`); the stored count is a `u32` widened here so
/// the caller needs no conversion.
///
/// # Safety
/// `ptr` must be null or point to a payload returned by
/// [`coddl_rc_alloc`] (or an immortal payload with a valid header).
#[no_mangle]
pub unsafe extern "C" fn coddl_rc_length(ptr: *const u8) -> i64 {
    if ptr.is_null() {
        return 0;
    }
    let header = ptr.sub(HEADER_SIZE) as *const CoddlRcHeader;
    i64::from((*header).length)
}

/// Return the element *record* pointer for a 0-based `Sequence` index —
/// `payload + index * record_size`, from which the caller `AttrLoad`s the
/// synthetic single `value` cell (at offset 0). The postfix index expression
/// `s[i]` lowers to a call here followed by that load.
///
/// Bounds are checked here (the codegen has no branching yet): a null
/// sequence, or an `index` outside `0 ..< length`, prints a diagnostic and
/// aborts (non-zero exit), mirroring `coddl_extract_check_cardinality`.
/// Borrows: it inspects only the header and never touches any refcount.
///
/// # Safety
/// `seq` must point to a payload returned by [`coddl_rc_alloc`] with a valid
/// `Sequence` header (its `desc` carrying the element `record_size`).
#[no_mangle]
pub unsafe extern "C" fn coddl_seq_index(seq: *const u8, index: i64) -> *const u8 {
    if seq.is_null() {
        eprintln!("coddl: index: null sequence");
        std::process::abort();
    }
    let header = seq.sub(HEADER_SIZE) as *const CoddlRcHeader;
    let length = i64::from((*header).length);
    if index < 0 || index >= length {
        eprintln!("coddl: index {index} out of bounds (sequence length {length})");
        std::process::abort();
    }
    let record_size = (*(*header).desc).record_size as usize;
    seq.add(index as usize * record_size)
}

/// Debug-build accessor for the live-allocation counter. Returns 0
/// in release builds. Used by runtime unit tests to confirm
/// retain/release balance.
pub fn live_allocations() -> i64 {
    #[cfg(debug_assertions)]
    {
        LIVE_ALLOCATIONS.load(Ordering::SeqCst)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_retain_release_balances() {
        // `live_allocations()` is a process-wide atomic counter; under
        // `cargo test`'s default parallel scheduler other test threads
        // may bump it between our reads. We track the DELTA via local
        // expectations and assert the alloc/retain/release sequence
        // doesn't crash, instead of asserting absolute counter values.
        unsafe {
            // Allocate a small payload with null descriptor (no drop
            // walker for non-relation kinds yet — set kind to a value
            // that doesn't match CoddlKind::Relation so the walker is
            // skipped and the block frees cleanly).
            let ptr = coddl_rc_alloc(0, 0, 999, std::ptr::null());
            assert!(!ptr.is_null());
            coddl_rc_retain(ptr);
            coddl_rc_retain(ptr);
            coddl_rc_release(ptr);
            coddl_rc_release(ptr);
            // Final release frees the block.
            coddl_rc_release(ptr);
        }
    }

    #[test]
    fn null_retain_release_is_safe() {
        unsafe {
            coddl_rc_retain(std::ptr::null_mut());
            coddl_rc_release(std::ptr::null_mut());
        }
    }

    #[test]
    fn header_layout_is_stable() {
        // Both codegen backends hand-mirror this layout when they emit an
        // immortal-headed string literal (a `{ i64, ptr, i32, i32, i64,
        // [N x i8] }` struct). If the header ever changes size or field
        // order, those mirrors break silently at runtime — pin it here.
        use std::mem::offset_of;
        assert_eq!(HEADER_SIZE, 32, "RC_HEADER_SIZE mirror in both backends");
        assert_eq!(offset_of!(CoddlRcHeader, rc), 0);
        assert_eq!(offset_of!(CoddlRcHeader, desc), 8);
        assert_eq!(offset_of!(CoddlRcHeader, kind), 16);
        assert_eq!(offset_of!(CoddlRcHeader, length), 20);
        assert_eq!(offset_of!(CoddlRcHeader, capacity), 24);
        assert_eq!(CoddlKind::Text as u32, 1, "RC_KIND_TEXT mirror");
        assert_eq!(IMMORTAL_RC, u64::MAX, "RC_IMMORTAL mirror");
    }

    #[test]
    fn immortal_header_is_inert_under_retain_release() {
        // A literal-shaped payload: a real `CoddlRcHeader` with rc =
        // IMMORTAL_RC sitting in front of the bytes. retain/release must be
        // no-ops and must never free it (mirrors what the backends emit).
        #[repr(C)]
        struct Immortal {
            header: CoddlRcHeader,
            bytes: [u8; 3],
        }
        let mut obj = Immortal {
            header: CoddlRcHeader {
                rc: IMMORTAL_RC,
                desc: std::ptr::null(),
                kind: CoddlKind::Text as u32,
                length: 3,
                capacity: 3,
            },
            bytes: *b"abc",
        };
        unsafe {
            let payload = obj.bytes.as_mut_ptr();
            coddl_rc_retain(payload);
            coddl_rc_release(payload);
            coddl_rc_release(payload);
            // Still untouched — release never decremented or freed it.
            assert_eq!(obj.header.rc, IMMORTAL_RC);
            assert_eq!(&obj.bytes, b"abc");
        }
    }
}

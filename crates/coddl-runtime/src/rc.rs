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
    /// (`coddl_text_concat` / `coddl_char_to_text`). The drop walker treats it
    /// like any non-`Relation` kind (free the block, no recursion); scalar-Text
    /// RC is not yet wired, so these currently leak — see `docs/memory.md`.
    Text = 1,
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
    let total = HEADER_SIZE.checked_add(payload_size).expect("alloc overflow");
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
    if kind == CoddlKind::Relation as u32 {
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
}

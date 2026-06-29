//! Leak oracle for relation-cell `Text` reference counting.
//!
//! Exercises the runtime relational primitives over **heap-allocated** `Text`
//! cells (so each is tracked by the debug `LIVE_ALLOCATIONS` counter) and
//! asserts the global live-allocation count returns to its baseline after every
//! input and output is released — proving retain-on-copy, the relation drop
//! walker, and dedup-release stay balanced (no leak, no double-free).
//!
//! This is its own integration-test binary, so the process-global counter is
//! touched only by the single test below — no interleaving from parallel tests
//! in other binaries. In release builds `live_allocations()` is a no-op `0`, so
//! the asserts hold vacuously.

use coddl_runtime::rc::{coddl_rc_alloc, coddl_rc_release, live_allocations, CoddlKind};
use coddl_runtime::relation::{
    coddl_relation_minus, coddl_relation_project, coddl_relation_union, coddl_text_concat,
    CoddlAttrDesc, CoddlAttrKind, CoddlHeadingDesc,
};

/// Allocate a heap `Text` payload (kind=Text, rc=1) over `bytes` — a *tracked*
/// allocation (unlike an immortal literal), so leaks show up in the counter.
unsafe fn heap_text(bytes: &[u8]) -> *mut u8 {
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

/// Build an (unsealed) `{ id: Integer, name: Text }` relation — record_size 24:
/// id@0 (i64), name@8 (ptr@8, len@16) — with heap `Text` cells.
unsafe fn build(rows: &[(i64, &[u8])], desc: *const CoddlHeadingDesc) -> *mut u8 {
    let rec = (*desc).record_size as usize;
    let rel = coddl_rc_alloc(
        rec * rows.len(),
        rows.len() as u32,
        CoddlKind::Relation as u32,
        desc,
    );
    for (i, (id, name)) in rows.iter().enumerate() {
        let r = rel.add(i * rec);
        std::ptr::write(r as *mut i64, *id);
        std::ptr::write(r.add(8) as *mut usize, heap_text(name) as usize);
        std::ptr::write(r.add(16) as *mut usize, name.len());
    }
    rel
}

#[test]
fn relops_and_concat_over_heap_text_leave_no_live_allocations() {
    unsafe {
        let id_name = [
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
            attrs: id_name.as_ptr(),
        };

        let base = live_allocations();

        // union — concatenate + content-aware dedup. (2,"Grace") appears in both
        // operands with DISTINCT heap pointers, so dedup-release must free the
        // dropped duplicate's cell.
        {
            let lhs = build(&[(1, b"Ada"), (2, b"Grace")], &desc);
            let rhs = build(&[(2, b"Grace"), (3, b"Zoe")], &desc);
            let out = coddl_relation_union(lhs, rhs, &desc);
            coddl_rc_release(out);
            coddl_rc_release(rhs);
            coddl_rc_release(lhs);
        }
        assert_eq!(live_allocations(), base, "union over heap Text leaked");

        // minus — keep lhs rows not in rhs (filter, no seal).
        {
            let lhs = build(&[(1, b"Ada"), (2, b"Grace")], &desc);
            let rhs = build(&[(2, b"Grace")], &desc);
            let out = coddl_relation_minus(lhs, rhs, &desc);
            coddl_rc_release(out);
            coddl_rc_release(rhs);
            coddl_rc_release(lhs);
        }
        assert_eq!(live_allocations(), base, "minus over heap Text leaked");

        // project to { name } — drops `id`, may create duplicates that seal
        // dedups (two distinct "Grace" rows collapse, releasing one cell).
        {
            let name_only = [CoddlAttrDesc {
                name: b"name".as_ptr(),
                name_len: 4,
                kind: CoddlAttrKind::Text as u32,
                offset: 0,
                sub: std::ptr::null(),
            }];
            let dst_desc = CoddlHeadingDesc {
                attr_count: 1,
                record_size: 16,
                attrs: name_only.as_ptr(),
            };
            let src = build(&[(1, b"Grace"), (2, b"Grace"), (3, b"Zoe")], &desc);
            let out = coddl_relation_project(src, &desc, &dst_desc);
            coddl_rc_release(out);
            coddl_rc_release(src);
        }
        assert_eq!(live_allocations(), base, "project over heap Text leaked");

        // scalar `||` — a heap concat result, released.
        {
            let a = b"Hello, ";
            let b = b"world!";
            let r = coddl_text_concat(a.as_ptr(), a.len(), b.as_ptr(), b.len());
            coddl_rc_release(r);
        }
        assert_eq!(live_allocations(), base, "scalar concat leaked");
    }
}

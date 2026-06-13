# Coddl runtime

This document is the authoritative spec for `libcoddl_runtime` —
the C-ABI staticlib that Coddl binaries link against. It pins the
RC contract, the per-heading descriptor layout, the seal discipline,
and the canonical printer format. See `ARCHITECTURE.md §6` for the
broader runtime responsibilities; this document is the Phase 19+
data layer.

For the IR shapes that drive these calls, see `docs/procir.md`. For
the per-backend emission of descriptors, see `docs/codegen.md`'s
"Per-module heading descriptors" section.

## RC contract

Every runtime-allocated payload is preceded by a 24-byte
`CoddlRcHeader` at a fixed negative offset (`HEADER_SIZE`):

```c
struct CoddlRcHeader {
    uint64_t                    rc;       // refcount
    const CoddlHeadingDesc*     desc;     // descriptor for relations; null otherwise
    uint32_t                    kind;     // CoddlKind discriminant
    uint32_t                    length;   // tuples in a relation; aux for other kinds
};
```

Codegen sees only the payload pointer; the runtime reaches the header
via `payload.sub(HEADER_SIZE)`.

### Immortal sentinel

`IMMORTAL_RC = u64::MAX` marks payloads that live in read-only
segments (string literals today, future compile-time data). Retain
and release short-circuit to no-ops on this sentinel so the same
calling convention works for both heap-managed and immortal values.

### Public API

| Symbol                     | Signature                                          | Semantics                                                                                                                |
|----------------------------|----------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------|
| `coddl_rc_alloc`           | `(payload_size: u64, length: u32, kind: u32, desc: ptr) -> ptr` | Allocate `HEADER_SIZE + payload_size` bytes, write a fresh header with `rc = 1`, return the payload pointer.             |
| `coddl_rc_retain`          | `(ptr) -> ()`                                       | Increment `rc`. No-op on null and on immortal payloads.                                                                  |
| `coddl_rc_release`         | `(ptr) -> ()`                                       | Decrement `rc`. On zero: dispatch the drop walker by `kind`, then free the entire block (`header + payload`).            |
| `coddl_relation_seal`      | `(payload, desc) -> ()`                             | Sort the relation's records by byte-wise comparison, then adjacent-dedup in place; updates the header's `length`.        |
| `coddl_write_relation`     | `(payload, desc) -> ()`                             | Print the relation, one tuple per line, in canonical heading order. Empty relation writes zero bytes.                    |
| `coddl_relation_where`     | `(src, desc, pred_fn) -> ptr`                       | Restrict `src` by `pred_fn(record_ptr) != 0`. Returns a fresh RC-managed relation (rc=1) holding the matching rows in the input's original order. Worst-case alloc; header `length` trimmed to the actual count. No re-seal — filter preserves the input's sealed (sorted/dedup'd) order. |
| `coddl_extract_check_cardinality` | `(src, desc) -> ptr`                          | TTM RM Pre 10 cardinality check: if `src`'s header `length` is exactly 1, returns a pointer to the single record's bytes (which equals `src` itself, since records start at the payload base). Otherwise writes `"coddl: extract: expected exactly 1 tuple, got N"` to stderr and calls `std::process::abort()`. The caller reads each attribute via the descriptor before releasing the source — the lowering's "Extract then Release" order guarantees the buffer is live during attribute reads. |

The lowerer and both backends agree on the same set of symbol names
and signatures; the `BUILTIN_EXTERNS` table in `coddl-procir::lower`
maps the user-facing builtin (today: `write_relation`) to its
linkage name.

### Drop walker

When `coddl_rc_release` brings the refcount to zero, the runtime
dispatches on the header's `kind`. For `CoddlKind::Relation`, the
drop walker iterates each record, reads the descriptor for the
per-attribute kind, and recurses into any nested heap cells before
freeing the block.

Phase 19 has no nested heap cells in practice — relation records
hold scalars (Integer / Boolean) or pointers to immortal Text bytes
(string literals are placed in read-only data segments by codegen),
all of which are no-op-drop. The hook is in place so that future
phases (relation-of-relations, Tuple-of-Text-owners) slot in
without re-plumbing.

## Heading descriptors

Each unique `Heading` the typechecker reasons about gets one
descriptor emitted by each backend. The C struct layouts:

```c
struct CoddlAttrDesc {
    const uint8_t* name;       // not null-terminated; length in `name_len`
    uint32_t       name_len;
    uint32_t       kind;       // CoddlAttrKind discriminant
    uint32_t       offset;     // byte offset within a record
    // natural padding to multiple of pointer alignment
};

struct CoddlHeadingDesc {
    uint32_t                attr_count;
    uint32_t                record_size;  // bytes per record
    const CoddlAttrDesc*    attrs;        // attr_count entries, canonical order
};
```

`CoddlAttrKind` enumerates the supported cell types:

| Value | Variant   | Cell width (bytes) | Encoding                            |
|-------|-----------|--------------------|-------------------------------------|
| 0     | `Integer` | 8                  | `i64` host-endian                   |
| 1     | `Boolean` | 8                  | `i64`; 0 = false, 1 = true          |
| 2     | `Text`    | 16                 | `(ptr: *const u8, len: usize)`      |

Sub-word packing (Boolean → 1 bit, Byte → 1 byte) is deferred to a
later layout optimisation phase. Tuple-as-cell and Relation-as-cell
encodings are also reserved (kinds 10/11) but not yet emitted.

The `coddl-procir::layout` module owns the layout computation that
backends consume. Both backends and the runtime must agree on:

- Canonical (name-sorted) attribute order, matching
  `Heading::attrs()`.
- Per-cell width and host-endian encoding.
- Record stride (sum of cell widths; no per-record padding today).

If any of those drift, the runtime walks records by the wrong stride
or reads cells out of bounds. The hygiene gate doesn't catch this —
test discipline does. Phase 19's e2e suite is the smallest example
that exercises the full pipeline.

## Seal discipline

`coddl_relation_seal` enforces RM Pro 3 (no duplicates in a
relation) in two steps:

1. **Sort.** Records are sorted by byte-wise comparison of their
   record buffers. The order is unspecified beyond "total and
   deterministic" — the same source program produces the same byte
   order on every run and every backend, because the layout is
   canonical and every cell's encoding is host-endian fixed-width.
2. **Adjacent dedup.** Equal-byte adjacent records collapse; the
   header's `length` shrinks accordingly.

Byte-equality is the right equivalence for Phase 19's cell set:
Integer/Boolean cells are content-encoded directly; Text cells hold
`(ptr, len)` pairs that — for compile-time string literals —
deduplicate by content at codegen time, so equal-content strings
share their pointer. If runtime-allocated Text owners land later
(user-defined strings, computed concatenation), seal will need a
content-aware comparison; the current byte-wise sort would then
under-dedupe in safe ways (treat semantically-equal strings as
distinct) but never over-dedupe.

## Canonical printer

`coddl_write_relation` walks the sealed payload and writes one tuple
per line:

```
{a: 1}
{a: 2}
```

The format mirrors the surface tuple-literal syntax (Phase 18). Each
attribute appears in canonical heading order; the per-cell formatter
dispatches on `CoddlAttrKind`:

- `Integer` / `Boolean` — decimal / `true`/`false`.
- `Text` — `"..."` with the raw bytes inside.
- Tuple / Relation cells — print as `{...}` placeholder. The
  recursive printer lands when Phase 20+ workflows demand it.

Empty relation: zero bytes (no trailing newline). The byte-equality
e2e test relies on this — a non-empty header would diverge across
implementations.

## Debug instrumentation

`coddl-runtime::live_allocations()` returns the count of currently
allocated RC blocks. In `cfg(debug_assertions)` builds it tracks
every `coddl_rc_alloc` / `coddl_rc_release`; release builds return
0. The runtime unit tests assert this counter returns to zero after
allocate-retain-release cycles. The driver e2e tests run release
builds (where the counter is a no-op), so live-allocation leaks
need a debug-build runtime test to catch — that's the role of
`coddl-runtime::rc::tests::alloc_retain_release_balances`.

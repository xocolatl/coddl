//! Heading-canonical record layout.
//!
//! Computes per-attribute byte offsets and the total record size for
//! a given `Heading`. Both backends consume this when emitting the
//! per-heading `CoddlHeadingDesc` static and when writing tuple
//! payloads into a relation's record buffer.
//!
//! Phase 19 cell widths:
//!
//! | Surface `Type` | Width (bytes) | Encoding                       |
//! |----------------|---------------|--------------------------------|
//! | `Integer`      | 8             | i64 host-endian                |
//! | `Boolean`      | 8             | i64 (0 / 1); sub-word later    |
//! | `Character`    | 8             | codepoint zero-extended to i64 |
//! | `Approximate`  | 8             | IEEE-754 double (canonical bits)|
//! | `Rational`     | 16            | reduced `(i64 numer, i64 denom)` |
//! | `Text`         | 16            | (ptr: usize, len: usize)       |
//! | `Tuple H`      | Σ components  | inline sub-region (recursive)  |
//!
//! A `Tuple`-valued attribute is an inline nested cell: a contiguous
//! sub-region whose width is the sum of its components' widths, with
//! the sub-layout carried on [`AttrLayout::sub`] (0-based offsets).
//! The remaining surface types (`Binary`, `Byte`, nested `Relation`)
//! are reserved — hitting one in `record_layout` is a "future phase
//! will widen this" `unreachable!`; the typechecker keeps them out of
//! relation cells.
//!
//! See `docs/runtime.md` for the kind-tag → cell-encoding contract
//! the runtime relies on (and which this module must match).

use coddl_types::{Heading, Type};

/// Numeric kind tag matching the runtime's [`CoddlAttrKind`] (see
/// `coddl-runtime::relation`). Backends emit the same integers into
/// the static descriptor bytes.
pub mod kind_tag {
    pub const INTEGER: u32 = 0;
    pub const BOOLEAN: u32 = 1;
    pub const TEXT: u32 = 2;
    /// `Character` cell: the Unicode scalar value zero-extended into an
    /// 8-byte inline slot (SQL binds/stores it as an integer codepoint).
    pub const CHARACTER: u32 = 3;
    /// `Approximate` cell: an IEEE-754 double stored inline as its 8 bytes
    /// (canonical bits — see the lowerer's `canonical_approx_bits`).
    pub const APPROXIMATE: u32 = 4;
    /// `Rational` cell: a reduced `(numer, denom)` pair of `i64`s, 16 bytes
    /// (num @ 0, den @ 8). Canonical form ⇒ byte-compare is value-equality.
    pub const RATIONAL: u32 = 5;
    /// Inline nested-tuple cell: a contiguous sub-region; the descriptor
    /// attribute carries a pointer to the tuple's own heading descriptor.
    pub const TUPLE: u32 = 10;
    /// Relation-valued attribute cell: a single RC payload pointer (8 bytes),
    /// stored inline. The record co-owns it (retain-on-store); the drop walker
    /// releases it. Unlike `TUPLE`, it is *not* an inline sub-region — the
    /// relation's own header/descriptor drives its own drop, so no `sub`.
    pub const RELATION: u32 = 11;
}

/// One attribute's slot inside a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrLayout {
    pub name: String,
    pub kind: u32,
    pub offset: u32,
    pub width: u32,
    /// For a `Tuple`-valued attribute (`kind == kind_tag::TUPLE`): the layout
    /// of its inline sub-region, with offsets **0-based within the sub-region**
    /// (add this attr's `offset` to reach the record). `None` for scalar cells.
    pub sub: Option<RecordLayout>,
}

/// Full record layout for a heading: per-attribute slot information
/// in canonical (name-sorted) order, plus the total stride per record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordLayout {
    pub attrs: Vec<AttrLayout>,
    pub record_size: u32,
}

/// Width in bytes for a supported leaf type. Returns `Some(n)` for
/// Integer/Boolean/Character/Approximate/Rational/Text; `None` for types
/// the printer / drop walker doesn't yet handle.
pub fn cell_width(ty: &Type) -> Option<u32> {
    match ty {
        Type::Integer => Some(8),
        Type::Boolean => Some(8),
        // Character is an inline codepoint zero-extended into an 8-byte slot
        // (matching Integer/Boolean), so `cmp_cell`'s byte compare stays valid.
        Type::Character => Some(8),
        // Approximate is an inline IEEE-754 double (8 bytes), stored as its
        // canonical bits so `cmp_cell`'s byte compare is a proper equality.
        Type::Approximate => Some(8),
        // Rational is a reduced (numer, denom) pair of i64s — 16 bytes.
        Type::Rational => Some(16),
        Type::Text => Some(16),
        // Relation-valued attribute: one inline RC payload pointer.
        Type::Relation(_) => Some(8),
        // Inline nested-tuple cell: the sum of its components' widths,
        // recursively (`Tuple {}` → 0). `None` propagates if any component
        // is an as-yet-unsupported cell type.
        Type::Tuple(h) => {
            let mut total = 0u32;
            for (_, fty) in h.attrs() {
                total += cell_width(fty)?;
            }
            Some(total)
        }
        _ => None,
    }
}

/// A tuple whose flattened record is at least this many bytes is **boxed** —
/// passed and returned as a single RC pointer to a heap record — instead of
/// flattened into per-attribute ABI slots. Below it, tuples stay flattened
/// (zero heap, free field access). ~8 machine words, so typical 2–4-field
/// tuples flatten and only genuinely wide ones box. Purely a representation
/// knob — no observable semantic effect.
pub const TUPLE_BOX_THRESHOLD: u32 = 64;

/// Whether a tuple with `heading` is boxed (see [`TUPLE_BOX_THRESHOLD`]). A pure
/// function of the interned heading, so the lowerer and both backends agree by
/// construction. A heading whose width isn't layout-computable (an unsupported
/// cell type) is never boxed — it stays flattened, where its cells pass as
/// individual leaf operands.
pub fn tuple_is_boxed(heading: &Heading) -> bool {
    cell_width(&Type::Tuple(heading.clone())).is_some_and(|w| w >= TUPLE_BOX_THRESHOLD)
}

/// Map a surface `Type` to its [`kind_tag`] integer. Returns `None`
/// for types not yet supported in relation cells.
pub fn cell_kind(ty: &Type) -> Option<u32> {
    match ty {
        Type::Integer => Some(kind_tag::INTEGER),
        Type::Boolean => Some(kind_tag::BOOLEAN),
        Type::Character => Some(kind_tag::CHARACTER),
        Type::Approximate => Some(kind_tag::APPROXIMATE),
        Type::Rational => Some(kind_tag::RATIONAL),
        Type::Text => Some(kind_tag::TEXT),
        Type::Tuple(_) => Some(kind_tag::TUPLE),
        Type::Relation(_) => Some(kind_tag::RELATION),
        _ => None,
    }
}

/// Compute the layout for `heading`. A `Tuple`-valued attribute lays out as a
/// contiguous inline sub-region (recursively), carried on `AttrLayout.sub` with
/// 0-based offsets. Panics if any attribute uses a type the runtime layout
/// doesn't cover yet (Binary, Byte, nested Relation). The
/// typechecker doesn't reject those — they just don't reach
/// codegen inside a relation cell yet. When the cell layout widens, add cases
/// to `cell_width`/`cell_kind`.
pub fn record_layout(heading: &Heading) -> RecordLayout {
    let mut offset: u32 = 0;
    let mut attrs: Vec<AttrLayout> = Vec::with_capacity(heading.len());
    for (name, ty) in heading.attrs() {
        let kind =
            cell_kind(ty).unwrap_or_else(|| unreachable!("cell kind for {ty} not yet supported"));
        // A `Tuple` attribute is an inline nested cell: lay its components out
        // in a self-contained sub-region (0-based) and keep that sub-layout so
        // codegen can emit a nested descriptor and the runtime can recurse.
        let sub = match ty {
            Type::Tuple(h) => Some(record_layout(h)),
            _ => None,
        };
        let width = match &sub {
            Some(s) => s.record_size,
            None => cell_width(ty).unwrap_or_else(|| {
                unreachable!("cell type {ty} not yet supported in relation layout")
            }),
        };
        attrs.push(AttrLayout {
            name: name.clone(),
            kind,
            offset,
            width,
            sub,
        });
        offset += width;
    }
    RecordLayout {
        attrs,
        record_size: offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heading(pairs: &[(&str, Type)]) -> Heading {
        Heading::new(
            pairs
                .iter()
                .map(|(n, t)| (n.to_string(), t.clone()))
                .collect(),
        )
    }

    #[test]
    fn single_integer_attr_is_eight_bytes() {
        let h = heading(&[("a", Type::Integer)]);
        let l = record_layout(&h);
        assert_eq!(l.record_size, 8);
        assert_eq!(l.attrs.len(), 1);
        assert_eq!(l.attrs[0].name, "a");
        assert_eq!(l.attrs[0].offset, 0);
        assert_eq!(l.attrs[0].width, 8);
        assert_eq!(l.attrs[0].kind, kind_tag::INTEGER);
    }

    #[test]
    fn single_character_attr_is_eight_byte_codepoint_cell() {
        let h = heading(&[("c", Type::Character)]);
        let l = record_layout(&h);
        assert_eq!(l.record_size, 8);
        assert_eq!(l.attrs[0].name, "c");
        assert_eq!(l.attrs[0].offset, 0);
        assert_eq!(l.attrs[0].width, 8);
        assert_eq!(l.attrs[0].kind, kind_tag::CHARACTER);
    }

    #[test]
    fn single_approximate_attr_is_eight_byte_double_cell() {
        let h = heading(&[("x", Type::Approximate)]);
        let l = record_layout(&h);
        assert_eq!(l.record_size, 8);
        assert_eq!(l.attrs[0].name, "x");
        assert_eq!(l.attrs[0].offset, 0);
        assert_eq!(l.attrs[0].width, 8);
        assert_eq!(l.attrs[0].kind, kind_tag::APPROXIMATE);
    }

    #[test]
    fn single_rational_attr_is_sixteen_byte_cell() {
        let h = heading(&[("r", Type::Rational)]);
        let l = record_layout(&h);
        assert_eq!(l.record_size, 16);
        assert_eq!(l.attrs[0].name, "r");
        assert_eq!(l.attrs[0].offset, 0);
        assert_eq!(l.attrs[0].width, 16);
        assert_eq!(l.attrs[0].kind, kind_tag::RATIONAL);
    }

    #[test]
    fn integer_and_text_layout_canonical() {
        // Heading source order is reversed; canonical layout sorts
        // by name, so `a` (Integer) comes before `z` (Text).
        let h = heading(&[("z", Type::Text), ("a", Type::Integer)]);
        let l = record_layout(&h);
        assert_eq!(l.record_size, 24);
        let names: Vec<&str> = l.attrs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "z"]);
        assert_eq!(l.attrs[0].offset, 0);
        assert_eq!(l.attrs[0].width, 8);
        assert_eq!(l.attrs[1].offset, 8);
        assert_eq!(l.attrs[1].width, 16);
    }

    #[test]
    fn tuple_attr_lays_out_as_inline_sub_region() {
        // {id: Integer, pt: Tuple {x: Integer, y: Text}} — `id` (8) then `pt`
        // (a 24-byte sub-region: x at sub-offset 0, y at sub-offset 8). The
        // sub-layout's offsets are 0-based within the region.
        let pt = Type::Tuple(heading(&[("x", Type::Integer), ("y", Type::Text)]));
        let h = heading(&[("id", Type::Integer), ("pt", pt)]);
        let l = record_layout(&h);
        assert_eq!(l.record_size, 8 + 24);
        let names: Vec<&str> = l.attrs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["id", "pt"]);
        assert_eq!(l.attrs[0].offset, 0);
        let pt_attr = &l.attrs[1];
        assert_eq!(pt_attr.kind, kind_tag::TUPLE);
        assert_eq!(pt_attr.offset, 8);
        assert_eq!(pt_attr.width, 24);
        let sub = pt_attr.sub.as_ref().expect("tuple attr has a sub-layout");
        assert_eq!(sub.record_size, 24);
        assert_eq!(sub.attrs[0].name, "x");
        assert_eq!(sub.attrs[0].offset, 0);
        assert_eq!(sub.attrs[0].kind, kind_tag::INTEGER);
        assert_eq!(sub.attrs[1].name, "y");
        assert_eq!(sub.attrs[1].offset, 8);
        assert_eq!(sub.attrs[1].kind, kind_tag::TEXT);
    }

    #[test]
    fn empty_tuple_attr_is_zero_width() {
        let h = heading(&[("u", Type::Tuple(Heading::empty()))]);
        let l = record_layout(&h);
        assert_eq!(l.record_size, 0);
        assert_eq!(l.attrs[0].kind, kind_tag::TUPLE);
        assert_eq!(l.attrs[0].width, 0);
        assert_eq!(l.attrs[0].sub.as_ref().unwrap().record_size, 0);
    }
}

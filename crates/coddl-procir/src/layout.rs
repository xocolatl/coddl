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
//! | `Text`         | 16            | (ptr: usize, len: usize)       |
//!
//! Other Phase-15 surface types (`Rational`, `Approximate`,
//! `Character`, `Binary`, `Byte`, `Tuple`, `Relation`) are reserved.
//! Hitting one in `record_layout` today is a "future phase will
//! widen this" `unreachable!` — the typechecker keeps them out of
//! relation cells in Phase 19's tests.
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
    // Reserved (not yet emitted): TUPLE = 10, RELATION = 11.
}

/// One attribute's slot inside a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrLayout {
    pub name: String,
    pub kind: u32,
    pub offset: u32,
    pub width: u32,
}

/// Full record layout for a heading: per-attribute slot information
/// in canonical (name-sorted) order, plus the total stride per record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordLayout {
    pub attrs: Vec<AttrLayout>,
    pub record_size: u32,
}

/// Width in bytes for a Phase-19-supported leaf type. Returns
/// `Some(n)` for Integer/Boolean/Text; `None` for types Phase 19's
/// printer / drop walker doesn't yet handle.
pub fn cell_width(ty: &Type) -> Option<u32> {
    match ty {
        Type::Integer => Some(8),
        Type::Boolean => Some(8),
        Type::Text => Some(16),
        _ => None,
    }
}

/// Map a surface `Type` to its [`kind_tag`] integer. Returns `None`
/// for types not yet supported in relation cells.
pub fn cell_kind(ty: &Type) -> Option<u32> {
    match ty {
        Type::Integer => Some(kind_tag::INTEGER),
        Type::Boolean => Some(kind_tag::BOOLEAN),
        Type::Text => Some(kind_tag::TEXT),
        _ => None,
    }
}

/// Compute the layout for `heading`. Panics if any attribute uses a
/// type Phase 19's runtime layout doesn't cover (Rational, Approximate,
/// Character, Binary, Byte, nested Tuple, nested Relation). The
/// typechecker doesn't reject those — they just don't reach codegen
/// inside a relation cell in Phase 19's e2e program. When Phase 20+
/// widens the cell layout, add cases here.
pub fn record_layout(heading: &Heading) -> RecordLayout {
    let mut offset: u32 = 0;
    let mut attrs: Vec<AttrLayout> = Vec::with_capacity(heading.len());
    for (name, ty) in heading.attrs() {
        let width = cell_width(ty).unwrap_or_else(|| {
            unreachable!("cell type {ty} not yet supported in relation layout")
        });
        let kind = cell_kind(ty).unwrap_or_else(|| {
            unreachable!("cell kind for {ty} not yet supported")
        });
        attrs.push(AttrLayout {
            name: name.clone(),
            kind,
            offset,
            width,
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
}

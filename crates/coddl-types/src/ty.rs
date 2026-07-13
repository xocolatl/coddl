//! Type representation.
//!
//! A flat enum covering the eight built-in scalar types, the structural
//! `Tuple H` and `Relation H` type generators (sharing a [`Heading`]),
//! and an `Unknown` sentinel used during error recovery. `Sequence T`
//! joins the enum when its parsing and semantics arrive.

use std::fmt;

/// A canonical (name-sorted) heading: the structural shape shared by
/// `Tuple H` and `Relation H`. The constructor enforces the sort
/// invariant so equality reduces to a slice comparison and a lookup
/// reduces to a binary search.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Heading(Vec<(String, Type)>);

impl Heading {
    /// Build a heading from `(name, type)` pairs. The fields are
    /// sorted by name in place — calls with the same set of
    /// attributes produce equal `Heading`s regardless of source order.
    pub fn new(mut fields: Vec<(String, Type)>) -> Self {
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Self(fields)
    }

    /// The empty heading. Backs the unit type `Tuple {}`.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// All attribute pairs in canonical (name-sorted) order.
    pub fn attrs(&self) -> &[(String, Type)] {
        &self.0
    }

    /// Number of attributes in the heading.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True iff the heading has zero attributes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Look up an attribute's type by name. Returns `None` if the
    /// heading has no attribute with that name. O(log n) via the
    /// canonical sort.
    pub fn lookup(&self, name: &str) -> Option<&Type> {
        self.0
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()
            .map(|i| &self.0[i].1)
    }

    /// True iff `self` is structurally assignable to `other` — every
    /// attribute name matches and the corresponding types are
    /// individually assignable. `Unknown` types in the field set
    /// participate as wildcards, like at the top level.
    pub fn assignable_to(&self, other: &Heading) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        self.0
            .iter()
            .zip(other.0.iter())
            .all(|((an, at), (bn, bt))| an == bn && at.assignable_to(bt))
    }

    /// Attribute names present in both headings (the natural-join key), in
    /// `self`'s canonical order.
    pub fn shared_names(&self, other: &Heading) -> Vec<String> {
        self.0
            .iter()
            .filter(|(n, _)| other.lookup(n).is_some())
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// True iff the two headings share no attribute name.
    pub fn is_disjoint_from(&self, other: &Heading) -> bool {
        self.0.iter().all(|(n, _)| other.lookup(n).is_none())
    }

    /// The union of two headings — every attribute from both, shared names
    /// appearing once. `Err(name)` if a shared attribute has incompatible
    /// types on the two sides. Re-canonicalized (sorted).
    pub fn union(&self, other: &Heading) -> Result<Heading, String> {
        let mut merged: Vec<(String, Type)> = self.0.clone();
        for (n, t) in &other.0 {
            match self.lookup(n) {
                Some(existing) => {
                    if !existing.assignable_to(t) {
                        return Err(n.clone());
                    }
                }
                None => merged.push((n.clone(), t.clone())),
            }
        }
        Ok(Heading::new(merged))
    }
}

impl fmt::Display for Heading {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, (name, ty)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name}: {ty}")?;
        }
        f.write_str("}")
    }
}

/// A Coddl type as the typechecker reasons about it.
///
/// Derived equality is structural and exact — two `Tuple`s are equal
/// only when their headings literally coincide; two `Unknown`s compare
/// equal. For typechecker comparisons that should treat `Unknown` as
/// a wildcard, use [`Type::assignable_to`] instead.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Integer,
    Rational,
    Approximate,
    Text,
    Character,
    Binary,
    Byte,
    Boolean,
    /// The type of an `f"…"` format-string literal. Compile-time-only and
    /// non-storable: it never survives lowering, and there is **no**
    /// `Text -> FormatText` coercion (that absence is the firewall keeping
    /// runtime `Text` — e.g. `read_line` input — out of template slots). It
    /// may flow through a `let` binding — `let t = f"…"` — so a template can
    /// be written once and reused across `format` calls; provenance still
    /// traces to a compile-time literal. Unspellable as a type name (absent
    /// from `from_builtin_name`), so it can never be a relvar/tuple
    /// attribute. Only `format`'s `template` parameter accepts it. See
    /// `docs/typecheck.md`.
    FormatText,
    /// Structural tuple: a value whose shape is its heading. The empty
    /// `Tuple` is the unit type (`Tuple {}`).
    Tuple(Heading),
    /// Structural relation: a set of tuples all sharing one heading.
    Relation(Heading),
    /// Ordered, finite list of values of one element type — the
    /// procedural-side companion to `Relation` (position significant,
    /// duplicates allowed). The type generator `Sequence T`; its surface
    /// literal is `Sequence [ … ]`. The element type may be any type,
    /// including a nested `Sequence`.
    Sequence(Box<Type>),
    /// A user-defined **nominal** scalar type, identified by name (RM Pre 1:
    /// distinct scalar types are disjoint). Declared `type Name { comp: T };`
    /// with a possrep whose components live in the checker's nominal-scalar
    /// table; a single-possrep scalar erases to its component's representation
    /// at ProcIR (`RawRequestPath` is physically a `Text`). Two `Scalar`s are
    /// the same type iff they have the same name — a `Scalar("RawRequestPath")`
    /// is never assignable to `Text` (or to another scalar), even when its
    /// component is `Text`. See `docs/typecheck.md`.
    Scalar(String),
    /// Used wherever a type couldn't be resolved (unknown type name,
    /// unresolved callee, etc.). Compares equal to anything so the
    /// checker can keep walking without piling errors on top of
    /// errors.
    Unknown,
    /// The **bottom** type: the type of an expression or block that never
    /// yields a value because control leaves it first — a block containing a
    /// statement that diverges (a bare `return`, or a statement-position
    /// `if/else` both of whose arms return) or whose tail itself diverges.
    /// `Never` is assignable to *every* type (a diverging path can
    /// stand in wherever any value is expected) and unifies as the identity
    /// (`Never` with `T` is `T`), so a `return`-only `if` arm agrees with its
    /// value-producing sibling. It is **unspellable** — absent from
    /// [`Type::from_builtin_name`], produced only by divergent control flow —
    /// and never survives lowering (a divergent value is never materialized).
    /// Same compile-time-only spirit as [`Type::FormatText`]. See
    /// `docs/typecheck.md`.
    Never,
}

impl Type {
    /// The unit type `Tuple {}` — the implicit return type of every
    /// `oper` declared without an explicit return clause.
    pub fn unit() -> Self {
        Type::Tuple(Heading::empty())
    }

    /// Resolve a type-name lexeme to its built-in `Type`, or `None`
    /// for an unknown name. The typechecker calls this on every
    /// `TypeRef` and emits T0005 when it returns `None`.
    ///
    /// `Tuple` and `Relation` are type *generators* (they take a
    /// heading) so they do not appear as bare names; they are
    /// constructed by the typechecker from their syntactic form.
    pub fn from_builtin_name(name: &str) -> Option<Type> {
        Some(match name {
            "Integer" => Type::Integer,
            "Rational" => Type::Rational,
            "Approximate" => Type::Approximate,
            "Text" => Type::Text,
            "Character" => Type::Character,
            "Binary" => Type::Binary,
            "Byte" => Type::Byte,
            "Boolean" => Type::Boolean,
            _ => return None,
        })
    }

    /// True iff `self` and `other` are interchangeable. `Unknown`
    /// participates as a wildcard so a single resolution failure
    /// doesn't poison every downstream check.
    pub fn assignable_to(&self, other: &Type) -> bool {
        match (self, other) {
            (Type::Unknown, _) | (_, Type::Unknown) => true,
            // Bottom: a diverging path produces no value, so it satisfies any
            // expected type. (`Never` as a *target* accepts only `Never`, via
            // the `_ => false` fallthrough — nothing is coerced *to* bottom.)
            (Type::Never, _) => true,
            (Type::Integer, Type::Integer)
            | (Type::Rational, Type::Rational)
            | (Type::Approximate, Type::Approximate)
            | (Type::Text, Type::Text)
            | (Type::Character, Type::Character)
            | (Type::Binary, Type::Binary)
            | (Type::Byte, Type::Byte)
            | (Type::Boolean, Type::Boolean)
            // FormatText is assignable only to itself — deliberately no
            // `Text -> FormatText` arm (the format-string-injection firewall).
            | (Type::FormatText, Type::FormatText) => true,
            (Type::Tuple(a), Type::Tuple(b)) => a.assignable_to(b),
            (Type::Relation(a), Type::Relation(b)) => a.assignable_to(b),
            (Type::Sequence(a), Type::Sequence(b)) => a.assignable_to(b),
            // Nominal: same name = same type. Never bridges to the component
            // type (RM Pre 1 disjointness) — the `_ => false` below covers
            // `Scalar` vs `Text`/other scalars.
            (Type::Scalar(a), Type::Scalar(b)) => a == b,
            _ => false,
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Integer => f.write_str("Integer"),
            Type::Rational => f.write_str("Rational"),
            Type::Approximate => f.write_str("Approximate"),
            Type::Text => f.write_str("Text"),
            Type::Character => f.write_str("Character"),
            Type::Binary => f.write_str("Binary"),
            Type::Byte => f.write_str("Byte"),
            Type::Boolean => f.write_str("Boolean"),
            Type::FormatText => f.write_str("FormatText"),
            Type::Tuple(h) => write!(f, "Tuple {h}"),
            Type::Relation(h) => write!(f, "Relation {h}"),
            Type::Sequence(t) => write!(f, "Sequence {t}"),
            Type::Scalar(name) => f.write_str(name),
            Type::Unknown => f.write_str("<unknown>"),
            Type::Never => f.write_str("Never"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_is_empty_tuple() {
        let u = Type::unit();
        assert!(matches!(u, Type::Tuple(ref h) if h.is_empty()));
        assert_eq!(format!("{u}"), "Tuple {}");
    }

    #[test]
    fn builtin_names_round_trip() {
        for name in [
            "Integer",
            "Rational",
            "Approximate",
            "Text",
            "Character",
            "Binary",
            "Byte",
            "Boolean",
        ] {
            let t = Type::from_builtin_name(name).expect(name);
            assert_eq!(format!("{t}"), name);
        }
        assert!(Type::from_builtin_name("NotABuiltin").is_none());
    }

    #[test]
    fn assignable_unknown_is_wildcard() {
        assert!(Type::Unknown.assignable_to(&Type::Integer));
        assert!(Type::Integer.assignable_to(&Type::Unknown));
        assert!(Type::Unknown.assignable_to(&Type::Unknown));
    }

    #[test]
    fn assignable_scalar_mismatch_rejected() {
        assert!(!Type::Integer.assignable_to(&Type::Text));
        assert!(!Type::Boolean.assignable_to(&Type::Byte));
    }

    #[test]
    fn format_text_is_the_injection_firewall() {
        // FormatText is unspellable as a type name (can never be a
        // relvar/tuple attribute) …
        assert!(Type::from_builtin_name("FormatText").is_none());
        // … assignable only to itself …
        assert!(Type::FormatText.assignable_to(&Type::FormatText));
        // … and crucially NOT interchangeable with Text in either
        // direction (runtime Text can never become a template).
        assert!(!Type::Text.assignable_to(&Type::FormatText));
        assert!(!Type::FormatText.assignable_to(&Type::Text));
        assert_eq!(format!("{}", Type::FormatText), "FormatText");
    }

    #[test]
    fn assignable_tuples_match_structurally() {
        let a = Type::Tuple(Heading::new(vec![("x".into(), Type::Integer)]));
        let b = Type::Tuple(Heading::new(vec![("x".into(), Type::Integer)]));
        assert!(a.assignable_to(&b));

        let c = Type::Tuple(Heading::new(vec![("y".into(), Type::Integer)]));
        assert!(!a.assignable_to(&c));

        let d = Type::Tuple(Heading::new(vec![
            ("x".into(), Type::Integer),
            ("y".into(), Type::Text),
        ]));
        assert!(!a.assignable_to(&d));
    }

    #[test]
    fn heading_canonicalizes_field_order() {
        // The constructor sorts by name, so source-order variations
        // produce equal headings.
        let h1 = Heading::new(vec![("b".into(), Type::Text), ("a".into(), Type::Integer)]);
        let h2 = Heading::new(vec![("a".into(), Type::Integer), ("b".into(), Type::Text)]);
        assert_eq!(h1, h2);
        assert_eq!(h1.attrs()[0].0, "a");
        assert_eq!(h1.attrs()[1].0, "b");
    }

    #[test]
    fn heading_lookup_finds_attribute() {
        let h = Heading::new(vec![("x".into(), Type::Integer), ("y".into(), Type::Text)]);
        assert!(matches!(h.lookup("x"), Some(Type::Integer)));
        assert!(matches!(h.lookup("y"), Some(Type::Text)));
        assert!(h.lookup("z").is_none());
    }

    #[test]
    fn relation_assignable_compares_headings() {
        let r1 = Type::Relation(Heading::new(vec![("id".into(), Type::Integer)]));
        let r2 = Type::Relation(Heading::new(vec![("id".into(), Type::Integer)]));
        let r3 = Type::Relation(Heading::new(vec![("id".into(), Type::Text)]));
        assert!(r1.assignable_to(&r2));
        assert!(!r1.assignable_to(&r3));
    }

    #[test]
    fn relation_and_tuple_are_distinct_types() {
        let h = Heading::new(vec![("x".into(), Type::Integer)]);
        let t = Type::Tuple(h.clone());
        let r = Type::Relation(h);
        assert!(!t.assignable_to(&r));
        assert!(!r.assignable_to(&t));
    }

    #[test]
    fn relation_display_uses_canonical_form() {
        let r = Type::Relation(Heading::new(vec![
            ("message".into(), Type::Text),
            ("id".into(), Type::Integer),
        ]));
        assert_eq!(format!("{r}"), "Relation {id: Integer, message: Text}");
    }
}

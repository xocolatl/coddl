//! Type representation.
//!
//! A flat enum covering the eight built-in scalar types, the structural
//! `Tuple H` type generator, and an `Unknown` sentinel used during
//! error recovery. `Relation H` and `Sequence T` join the enum when
//! their parsing and semantics arrive.

use std::fmt;

/// A Coddl type as the typechecker reasons about it.
///
/// Equality is structural: two `Tuple`s are equal when their attribute
/// sets coincide (the constructor stores them sorted by name to make
/// this cheap). `Unknown` is a sentinel that compares equal to anything
/// so a single failure doesn't cascade into a hundred unrelated
/// downstream errors.
#[derive(Clone, Debug)]
pub enum Type {
    Integer,
    Rational,
    Approximate,
    Text,
    Character,
    Binary,
    Byte,
    Boolean,
    /// Structural tuple. Attribute pairs are stored sorted by name for
    /// canonical equality. The empty `Tuple` is the unit type
    /// (`Tuple {}`).
    Tuple(Vec<(String, Type)>),
    /// Used wherever a type couldn't be resolved (unknown type name,
    /// unresolved callee, etc.). Compares equal to anything so the
    /// checker can keep walking without piling errors on top of
    /// errors.
    Unknown,
}

impl Type {
    /// The unit type `Tuple {}` — the implicit return type of every
    /// `oper` declared without an explicit return clause.
    pub fn unit() -> Self {
        Type::Tuple(Vec::new())
    }

    /// Resolve a type-name lexeme to its built-in `Type`, or `None`
    /// for an unknown name. The typechecker calls this on every
    /// `TypeRef` and emits T0005 when it returns `None`.
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
            (Type::Integer, Type::Integer)
            | (Type::Rational, Type::Rational)
            | (Type::Approximate, Type::Approximate)
            | (Type::Text, Type::Text)
            | (Type::Character, Type::Character)
            | (Type::Binary, Type::Binary)
            | (Type::Byte, Type::Byte)
            | (Type::Boolean, Type::Boolean) => true,
            (Type::Tuple(a), Type::Tuple(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((an, at), (bn, bt))| an == bn && at.assignable_to(bt))
            }
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
            Type::Tuple(fields) => {
                f.write_str("Tuple {")?;
                for (i, (name, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {ty}")?;
                }
                f.write_str("}")
            }
            Type::Unknown => f.write_str("<unknown>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_is_empty_tuple() {
        let u = Type::unit();
        assert!(matches!(u, Type::Tuple(ref v) if v.is_empty()));
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
    fn assignable_tuples_match_structurally() {
        let a = Type::Tuple(vec![("x".into(), Type::Integer)]);
        let b = Type::Tuple(vec![("x".into(), Type::Integer)]);
        assert!(a.assignable_to(&b));

        let c = Type::Tuple(vec![("y".into(), Type::Integer)]);
        assert!(!a.assignable_to(&c));

        let d = Type::Tuple(vec![("x".into(), Type::Integer), ("y".into(), Type::Text)]);
        assert!(!a.assignable_to(&d));
    }
}

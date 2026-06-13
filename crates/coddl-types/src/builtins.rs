//! Built-in operator registry.
//!
//! A tiny table mapping operator names to their signatures. The
//! typechecker consults it whenever a `Call` expression's callee is a
//! `NameRef`. Today the only registered operator is `write_line`; more
//! arrive as the runtime grows.

use std::collections::HashMap;

use crate::ty::{Heading, Type};

/// What a built-in operator's parameter accepts.
///
/// Most params take a concrete `Type` and the call site's argument
/// must be assignable to it. `AnyRelation` is a polymorphism escape
/// hatch for `write_relation` (and future printer-style builtins):
/// any `Type::Relation(_)` matches regardless of heading. The
/// typechecker reports T0004 ("argument type mismatch") whenever the
/// supplied value doesn't fit the declared kind.
#[derive(Clone, Debug)]
pub enum ParamKind {
    /// Concrete type. Standard structural assignability check.
    Concrete(Type),
    /// Polymorphic over any `Relation H`. Used by `write_relation`.
    AnyRelation,
}

/// One built-in operator's declared signature.
///
/// `params` is the operator's heading, in source order; the typechecker
/// matches arguments by name, not position.
#[derive(Clone, Debug)]
pub struct OperSig {
    pub params: Vec<(&'static str, ParamKind)>,
    pub return_type: Type,
}

/// Registry of every built-in operator known to the typechecker.
pub struct Builtins {
    opers: HashMap<&'static str, OperSig>,
}

impl Builtins {
    /// Populate with the current built-in set.
    pub fn new() -> Self {
        let mut opers = HashMap::new();
        opers.insert(
            "write_line",
            OperSig {
                params: vec![("message", ParamKind::Concrete(Type::Text))],
                return_type: Type::unit(),
            },
        );
        // `write_relation { rel: Relation H }` — polymorphic; the
        // backend supplies a per-call-site heading descriptor.
        opers.insert(
            "write_relation",
            OperSig {
                params: vec![("rel", ParamKind::AnyRelation)],
                return_type: Type::unit(),
            },
        );
        Self { opers }
    }

    /// Look up an operator by name. Returns `None` for unknown names;
    /// the caller emits T0001 in that case.
    pub fn oper(&self, name: &str) -> Option<&OperSig> {
        self.opers.get(name)
    }
}

impl Default for Builtins {
    fn default() -> Self {
        Self::new()
    }
}

/// Sentinel `Heading` used when an error path needs a placeholder
/// `Type::Relation(_)` value. Lives in this module so the typechecker
/// and downstream consumers share one source.
pub fn empty_relation_heading() -> Heading {
    Heading::empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_line_is_registered() {
        let b = Builtins::new();
        let sig = b.oper("write_line").expect("write_line should exist");
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0].0, "message");
        assert!(matches!(sig.params[0].1, ParamKind::Concrete(Type::Text)));
        assert!(matches!(sig.return_type, Type::Tuple(ref h) if h.is_empty()));
    }

    #[test]
    fn write_relation_is_polymorphic() {
        let b = Builtins::new();
        let sig = b.oper("write_relation").expect("write_relation should exist");
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0].0, "rel");
        assert!(matches!(sig.params[0].1, ParamKind::AnyRelation));
        assert!(matches!(sig.return_type, Type::Tuple(ref h) if h.is_empty()));
    }

    #[test]
    fn unknown_oper_returns_none() {
        let b = Builtins::new();
        assert!(b.oper("write_lne").is_none());
    }
}

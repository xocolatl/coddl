//! Built-in operator registry.
//!
//! A tiny table mapping operator names to their signatures. The
//! typechecker consults it whenever a `Call` expression's callee is a
//! `NameRef`. The current set is the I/O builtins — `write_line`,
//! `write_relation`, and `read_line`; more arrive as the runtime grows.

use std::borrow::Cow;
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// Concrete type. Standard structural assignability check.
    Concrete(Type),
    /// Polymorphic over any `Relation H`. Used by `write_relation`.
    AnyRelation,
    /// Polymorphic over any `Sequence T`. Used by `cardinality`, which
    /// reads the element count regardless of element type (mirrors
    /// `AnyRelation`).
    AnySequence,
    /// Polymorphic over any `Tuple H`. Used by `format`'s `params`; the
    /// heading is captured at the call site (mirrors `AnyRelation`).
    AnyTuple,
}

/// Whether an operator is safe to call inside a `transaction [...]`.
///
/// Transactions must be replayable on serialization conflict, so any
/// callee that touches the outside world is forbidden inside one. The
/// registry marks each builtin explicitly; new builtins default to
/// `Pure` and must opt in to `SideEffecting`, so adding a printing
/// operator is a forcing function on the conformance check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purity {
    Pure,
    SideEffecting,
}

/// One operator's declared signature.
///
/// `params` is the operator's heading, in source order; the typechecker
/// matches arguments by name, not position. Param names are `Cow` so the
/// same signature type serves both built-ins (borrowed `&'static str`
/// literals, no allocation) and user-defined operators collected from the
/// AST (owned `String`s). User ops only ever use `ParamKind::Concrete`.
#[derive(Clone, Debug)]
pub struct OperSig {
    pub params: Vec<(Cow<'static, str>, ParamKind)>,
    pub return_type: Type,
    pub purity: Purity,
}

/// Registry of every built-in operator known to the typechecker.
///
/// A name maps to a *list* of signatures: most names have exactly one,
/// but overloaded names (`to_text`) carry several, resolved by argument
/// type at the call site (static dispatch — each underlying signature is
/// monomorphic, so RM Pre 8 is preserved). The intrinsic `format` is not
/// in this table; the checker special-cases it (it needs a cross-argument
/// placeholders-↔-heading check and has no runtime symbol).
pub struct Builtins {
    opers: HashMap<&'static str, Vec<OperSig>>,
}

impl Builtins {
    /// Populate with the current built-in set.
    pub fn new() -> Self {
        let mut b = Builtins {
            opers: HashMap::new(),
        };
        b.register(
            "write_line",
            OperSig {
                params: vec![("message".into(), ParamKind::Concrete(Type::Text))],
                return_type: Type::unit(),
                purity: Purity::SideEffecting,
            },
        );
        // `write_relation { rel: Relation H }` — polymorphic; the
        // backend supplies a per-call-site heading descriptor.
        b.register(
            "write_relation",
            OperSig {
                params: vec![("rel".into(), ParamKind::AnyRelation)],
                return_type: Type::unit(),
                purity: Purity::SideEffecting,
            },
        );
        // `read_line { prompt: Text } -> Text` — prints the prompt, reads
        // one line from stdin (newline stripped). Side-effecting: it
        // touches the outside world, so it's barred inside a transaction.
        b.register(
            "read_line",
            OperSig {
                params: vec![("prompt".into(), ParamKind::Concrete(Type::Text))],
                return_type: Type::Text,
                purity: Purity::SideEffecting,
            },
        );
        // `to_text { self: <scalar> } -> Text` — the overloaded conversion
        // string interpolation desugars to, and the first multi-signature
        // builtin. One monomorphic signature per scalar type; the checker
        // picks by the static type of `self`. `Text` is an identity and
        // `Character` reuses `CharToText`; `Integer` / `Boolean` carry their
        // own runtime conversions (`coddl_int_to_text` / `coddl_bool_to_text`).
        for self_ty in [Type::Text, Type::Character, Type::Integer, Type::Boolean] {
            b.register(
                "to_text",
                OperSig {
                    params: vec![("self".into(), ParamKind::Concrete(self_ty))],
                    return_type: Type::Text,
                    purity: Purity::Pure,
                },
            );
        }
        // `cardinality { self } -> Integer` — the element/tuple count, read
        // from the RC header's `length` field. Polymorphic over both
        // `Relation H` (the natural TTM `COUNT`) and `Sequence T`; the count
        // lives in the same header slot for both, so one runtime read serves
        // either. Pure — it only inspects the header.
        for self_kind in [ParamKind::AnyRelation, ParamKind::AnySequence] {
            b.register(
                "cardinality",
                OperSig {
                    params: vec![("self".into(), self_kind)],
                    return_type: Type::Integer,
                    purity: Purity::Pure,
                },
            );
        }
        b
    }

    fn register(&mut self, name: &'static str, sig: OperSig) {
        self.opers.entry(name).or_default().push(sig);
    }

    /// Every signature registered under `name`, in registration order.
    /// Empty slice for an unknown name (the caller emits T0001).
    pub fn candidates(&self, name: &str) -> &[OperSig] {
        self.opers.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Look up an operator that has exactly one signature. Returns `None`
    /// for an unknown name *or* an overloaded one — overloaded callers
    /// must use [`Builtins::candidates`]. Kept for the non-overloaded
    /// call path and for introspection.
    pub fn oper(&self, name: &str) -> Option<&OperSig> {
        match self.opers.get(name) {
            Some(sigs) if sigs.len() == 1 => Some(&sigs[0]),
            _ => None,
        }
    }

    /// Whether `name` is a built-in operator. Used by the typechecker to
    /// reject a user-defined `oper` that would shadow a built-in (every
    /// callee name must resolve to exactly one definition). `format` is an
    /// intrinsic handled outside the registry, so it is reported as known
    /// too — a user op may not redefine it either.
    pub fn is_known(&self, name: &str) -> bool {
        name == "format" || self.opers.contains_key(name)
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
        assert_eq!(sig.params[0].0.as_ref(), "message");
        assert!(matches!(sig.params[0].1, ParamKind::Concrete(Type::Text)));
        assert!(matches!(sig.return_type, Type::Tuple(ref h) if h.is_empty()));
        assert_eq!(sig.purity, Purity::SideEffecting);
    }

    #[test]
    fn write_relation_is_polymorphic() {
        let b = Builtins::new();
        let sig = b.oper("write_relation").expect("write_relation should exist");
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0].0.as_ref(), "rel");
        assert!(matches!(sig.params[0].1, ParamKind::AnyRelation));
        assert!(matches!(sig.return_type, Type::Tuple(ref h) if h.is_empty()));
        assert_eq!(sig.purity, Purity::SideEffecting);
    }

    #[test]
    fn read_line_returns_text() {
        let b = Builtins::new();
        let sig = b.oper("read_line").expect("read_line should exist");
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0].0.as_ref(), "prompt");
        assert!(matches!(sig.params[0].1, ParamKind::Concrete(Type::Text)));
        assert!(matches!(sig.return_type, Type::Text));
        assert_eq!(sig.purity, Purity::SideEffecting);
    }

    #[test]
    fn unknown_oper_returns_none() {
        let b = Builtins::new();
        assert!(b.oper("write_lne").is_none());
        assert!(b.candidates("write_lne").is_empty());
    }

    #[test]
    fn to_text_is_overloaded() {
        let b = Builtins::new();
        let sigs = b.candidates("to_text");
        assert_eq!(
            sigs.len(),
            4,
            "expected Text + Character + Integer + Boolean overloads"
        );
        // Overloaded names are not reachable via the single-sig `oper()`.
        assert!(b.oper("to_text").is_none());
        // Every overload takes one `self` param and returns Text.
        for sig in sigs {
            assert_eq!(sig.params.len(), 1);
            assert_eq!(sig.params[0].0.as_ref(), "self");
            assert!(matches!(sig.return_type, Type::Text));
            assert_eq!(sig.purity, Purity::Pure);
        }
        let self_types: Vec<_> = sigs
            .iter()
            .map(|s| match &s.params[0].1 {
                ParamKind::Concrete(t) => t.clone(),
                _ => panic!("to_text self should be Concrete"),
            })
            .collect();
        assert!(self_types.contains(&Type::Text));
        assert!(self_types.contains(&Type::Character));
        assert!(self_types.contains(&Type::Integer));
        assert!(self_types.contains(&Type::Boolean));
    }

    #[test]
    fn cardinality_is_overloaded() {
        let b = Builtins::new();
        let sigs = b.candidates("cardinality");
        assert_eq!(sigs.len(), 2, "expected Relation + Sequence overloads");
        // Overloaded names are not reachable via the single-sig `oper()`.
        assert!(b.oper("cardinality").is_none());
        for sig in sigs {
            assert_eq!(sig.params.len(), 1);
            assert_eq!(sig.params[0].0.as_ref(), "self");
            assert!(matches!(sig.return_type, Type::Integer));
            assert_eq!(sig.purity, Purity::Pure);
        }
        let kinds: Vec<_> = sigs.iter().map(|s| s.params[0].1.clone()).collect();
        assert!(kinds.contains(&ParamKind::AnyRelation));
        assert!(kinds.contains(&ParamKind::AnySequence));
    }
}

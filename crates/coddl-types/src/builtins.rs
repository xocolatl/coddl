//! Built-in operator registry.
//!
//! Maps operator names to their signatures; the typechecker consults it
//! whenever a `Call` expression's callee is a `NameRef`. Monomorphic
//! signatures are **loaded from the Coddl prelude** (`coddl::core`, embedded in
//! `coddl-stdlib` — see docs/prelude.md); the heading-polymorphic printers/counters,
//! which have no surface spelling yet, are registered here in Rust. The
//! prelude gives the *signature*; purity and the lowering strategy stay
//! compiler-side, keyed by name.

use std::borrow::Cow;
use std::collections::HashMap;

use coddl_diagnostics::FileId;
use coddl_syntax::ast::{AstNode, Item, Root};
use coddl_syntax::{parse, FileKind};

use crate::checker::resolve_type_ref_quiet;
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
    /// Polymorphic over any `Tuple H`. Used by `format`'s `args`; the
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
    opers: HashMap<String, Vec<OperSig>>,
}

impl Builtins {
    /// Populate the registry. The monomorphic operators are loaded from the
    /// Coddl prelude (`coddl::core` — the signature source of truth); the
    /// heading-polymorphic printers/counters, which have no surface spelling
    /// yet, are registered in Rust.
    pub fn new() -> Self {
        let mut b = Builtins {
            opers: HashMap::new(),
        };
        b.load_prelude();
        b.register_polymorphic();
        b
    }

    /// Load the monomorphic `builtin oper` signatures from the `coddl::core`
    /// source (resolved from the embedded stdlib). The prelude gives the
    /// *signature* (params + return); purity is compiler-side metadata keyed by
    /// name ([`prelude_purity`]) and the lowering strategy lives in the codegen
    /// crates. Only `builtin oper` items are consumed here; any other items
    /// (e.g. `type` aliases) are ignored.
    fn load_prelude(&mut self) {
        let core = coddl_stdlib::resolve(&coddl_stdlib::ModulePath::parse("coddl::core"))
            .expect("coddl::core is always embedded in coddl-stdlib");
        self.load_module(core.source);
    }

    /// Parse one stdlib module's source and register its `builtin oper`
    /// signatures. `coddl::core` is loaded eagerly at construction; the opt-in
    /// modules (`coddl::env`, …) are loaded on demand by the typechecker when a
    /// file `use`s them — registering an opt-in module's operators only when
    /// imported keeps its names out of a file's namespace until it asks for
    /// them (so a user may freely name an `oper environment` when `coddl::env`
    /// is not in scope). Only `builtin oper` items are consumed here; other
    /// items (e.g. `type` aliases) are handled by the checker.
    pub(crate) fn load_module(&mut self, source: &str) {
        let out = parse(source, FileId(0), FileKind::Cd);
        let Some(root) = Root::cast(out.tree) else {
            return;
        };
        for item in root.items() {
            let Item::OperDecl(decl) = item else { continue };
            if !decl.is_builtin() {
                continue;
            }
            let Some(name_tok) = decl.name() else { continue };
            let name = name_tok.text().to_string();

            let mut params: Vec<(Cow<'static, str>, ParamKind)> = Vec::new();
            if let Some(heading) = decl.heading() {
                for param in heading.params() {
                    let Some(pname) = param.name() else { continue };
                    let pty = param
                        .type_ref()
                        .map(|tr| resolve_type_ref_quiet(&tr))
                        .unwrap_or(Type::Unknown);
                    params.push((
                        Cow::Owned(pname.text().to_string()),
                        ParamKind::Concrete(pty),
                    ));
                }
            }
            let return_type = decl
                .return_type()
                .map(|tr| resolve_type_ref_quiet(&tr))
                .unwrap_or_else(Type::unit);

            let purity = prelude_purity(&name);
            self.register(name, OperSig { params, return_type, purity });
        }
    }

    /// Register the built-ins that can't be spelled in the prelude yet: the
    /// heading- and element-polymorphic printer/counter operators. Heading
    /// polymorphism has no surface syntax (see docs/risks.md), so these stay
    /// hand-written. (`format` is a checker intrinsic outside the registry.)
    fn register_polymorphic(&mut self) {
        // `write_relation { rel: Relation H }` — the backend supplies a
        // per-call-site heading descriptor.
        self.register(
            "write_relation".to_string(),
            OperSig {
                params: vec![("rel".into(), ParamKind::AnyRelation)],
                return_type: Type::unit(),
                purity: Purity::SideEffecting,
            },
        );
        // `cardinality { self } -> Integer` — the element/tuple count read from
        // the RC header's `length` field. Polymorphic over both `Relation H`
        // (the natural TTM `COUNT`) and `Sequence T`; the count lives in the
        // same header slot for both. Pure — it only inspects the header.
        for self_kind in [ParamKind::AnyRelation, ParamKind::AnySequence] {
            self.register(
                "cardinality".to_string(),
                OperSig {
                    params: vec![("self".into(), self_kind)],
                    return_type: Type::Integer,
                    purity: Purity::Pure,
                },
            );
        }
    }

    fn register(&mut self, name: String, sig: OperSig) {
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

/// Purity for a prelude-declared built-in. The prelude expresses only
/// signatures; purity — whether a call is legal inside a `transaction [...]`
/// (RM Pre 14 / OO Pre 4) — is compiler-side metadata keyed by name.
/// Everything the prelude declares is `Pure` except the two stdio operators.
fn prelude_purity(name: &str) -> Purity {
    match name {
        "write_line" | "read_line" => Purity::SideEffecting,
        _ => Purity::Pure,
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

    // The conversions are prelude-only (no hand-written Rust registration),
    // so these also prove the loader actually parses `coddl::core`.
    #[test]
    fn to_approximate_loaded_from_prelude() {
        let b = Builtins::new();
        let sig = b.oper("to_approximate").expect("to_approximate should exist");
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0].0.as_ref(), "self");
        assert!(matches!(sig.params[0].1, ParamKind::Concrete(Type::Rational)));
        assert!(matches!(sig.return_type, Type::Approximate));
        assert_eq!(sig.purity, Purity::Pure);
    }

    #[test]
    fn to_rational_loaded_from_prelude() {
        let b = Builtins::new();
        let sig = b.oper("to_rational").expect("to_rational should exist");
        assert!(matches!(sig.params[0].1, ParamKind::Concrete(Type::Integer)));
        assert!(matches!(sig.return_type, Type::Rational));
        assert_eq!(sig.purity, Purity::Pure);
    }
}

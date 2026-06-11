//! Built-in operator registry.
//!
//! A tiny table mapping operator names to their signatures. The
//! typechecker consults it whenever a `Call` expression's callee is a
//! `NameRef`. Today the only registered operator is `write_line`; more
//! arrive as the runtime grows.

use std::collections::HashMap;

use crate::ty::Type;

/// One built-in operator's declared signature.
///
/// `params` is the operator's heading, in source order; the typechecker
/// matches arguments by name, not position.
#[derive(Clone, Debug)]
pub struct OperSig {
    pub params: Vec<(&'static str, Type)>,
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
                params: vec![("message", Type::Text)],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_line_is_registered() {
        let b = Builtins::new();
        let sig = b.oper("write_line").expect("write_line should exist");
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0].0, "message");
        assert!(matches!(sig.params[0].1, Type::Text));
        assert!(matches!(sig.return_type, Type::Tuple(ref v) if v.is_empty()));
    }

    #[test]
    fn unknown_oper_returns_none() {
        let b = Builtins::new();
        assert!(b.oper("write_lne").is_none());
    }
}

//! The relvar table — the result of the typechecker's pre-pass over
//! every declared relvar in a file.
//!
//! Phase 15 populates the table from `.cd` (`public` / `private`) and
//! `.cddb` (`base` / `virtual`) declarations; Phase 16 cross-validates
//! tables produced from companion files; Phase 18+ exposes the entries
//! to operator-body name resolution.

use std::collections::HashMap;

use coddl_diagnostics::Span;
use coddl_stdlib::ModulePath;

use crate::ty::Heading;

/// Discriminates the relvar kinds. Each one belongs to exactly one
/// dialect; mixing `.cd` and `.cddb` kinds is T0014.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelvarKind {
    /// `.cd` — exposed to the catalog (planned for materialization).
    Public,
    /// `.cd` — internal to the program.
    Private,
    /// `.cd` — a stdlib relvar whose backing the runtime supplies via FFI
    /// (e.g. `coddl::env`'s `Environment`, backed by the OS environment).
    /// Registered only from an imported stdlib module; a `builtin relvar`
    /// in a user file is rejected (T0091).
    Builtin,
    /// `.cddb` — persistent catalog relvar.
    Base,
    /// `.cddb` — derived catalog view (a `Relation`-typed expression).
    Virtual,
}

impl RelvarKind {
    /// Surface-level keyword as it appears in source. Stable across
    /// dialects so diagnostics can quote it back to the user.
    pub fn keyword(self) -> &'static str {
        match self {
            RelvarKind::Public => "public",
            RelvarKind::Private => "private",
            RelvarKind::Builtin => "builtin",
            RelvarKind::Base => "base",
            RelvarKind::Virtual => "virtual",
        }
    }
}

/// Everything the typechecker (and downstream passes) need to know
/// about one declared relvar: its kind, heading, candidate keys, and
/// the span of its declaration for diagnostic anchoring.
#[derive(Clone, Debug)]
pub struct RelvarInfo {
    pub kind: RelvarKind,
    pub heading: Heading,
    /// One inner `Vec<String>` per declared candidate key. Multi-key
    /// declarations parse; v1 typechecks only the first key. Stored
    /// in source order; attribute names within a key are unsorted.
    pub keys: Vec<Vec<String>>,
    /// For a `RelvarKind::Builtin` relvar, the stdlib module that declared it.
    /// The relvar's qualified name (`<module>::<name>`) is the runtime handle
    /// its read/assign lowering dispatches on. `None` for every other kind.
    pub module: Option<ModulePath>,
    /// Source range of the declaration's name token, for downstream
    /// "declared here" notes.
    pub span: Span,
}

/// All relvars declared in one file, keyed by name. Lookups are case-
/// sensitive — that's the language's convention (per the CLAUDE.md
/// identifier-case rule).
#[derive(Default, Clone, Debug)]
pub struct RelvarTable {
    entries: HashMap<String, RelvarInfo>,
}

impl RelvarTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to insert a relvar by name. Returns `Err(existing_span)`
    /// if a relvar with the same name was already declared in this
    /// file — the caller emits T0012 against the duplicate.
    pub fn try_insert(&mut self, name: String, info: RelvarInfo) -> Result<(), Span> {
        if let Some(existing) = self.entries.get(&name) {
            return Err(existing.span);
        }
        self.entries.insert(name, info);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&RelvarInfo> {
        self.entries.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &RelvarInfo)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::Type;
    use coddl_diagnostics::{FileId, Span};

    fn span() -> Span {
        Span::new(FileId(0), 0, 1)
    }

    fn info(kind: RelvarKind) -> RelvarInfo {
        RelvarInfo {
            kind,
            heading: Heading::new(vec![("id".into(), Type::Integer)]),
            keys: vec![vec!["id".into()]],
            module: None,
            span: span(),
        }
    }

    #[test]
    fn insert_and_lookup() {
        let mut table = RelvarTable::new();
        table
            .try_insert("Greetings".into(), info(RelvarKind::Public))
            .unwrap();
        assert!(table.get("Greetings").is_some());
        assert!(table.get("Other").is_none());
    }

    #[test]
    fn duplicate_insert_returns_existing_span() {
        let mut table = RelvarTable::new();
        let first = info(RelvarKind::Public);
        let first_span = first.span;
        table.try_insert("X".into(), first).unwrap();
        let err = table.try_insert("X".into(), info(RelvarKind::Private));
        assert_eq!(err, Err(first_span));
    }

    #[test]
    fn kind_keyword_round_trips() {
        assert_eq!(RelvarKind::Public.keyword(), "public");
        assert_eq!(RelvarKind::Private.keyword(), "private");
        assert_eq!(RelvarKind::Base.keyword(), "base");
        assert_eq!(RelvarKind::Virtual.keyword(), "virtual");
    }
}

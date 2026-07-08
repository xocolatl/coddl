//! The Coddl standard library.
//!
//! Owns the *sources* of the built-in modules under the reserved `coddl::`
//! root, plus a resolver that maps a [`ModulePath`] to its source text. The
//! typechecker (`coddl-types`) depends on this crate to load `coddl::core`
//! (the always-in-scope prelude) and — once the module system lands — the
//! opt-in modules gated behind `use module`.
//!
//! The dependency arrow runs one way: `coddl-types → coddl-stdlib`. This crate
//! stays below the typechecker and hands back plain source text; it never
//! reaches up into type representation or diagnostics.
//!
//! ## Roots and providers
//!
//! `coddl::` is a closed, compiler-owned root: its module sources are embedded
//! into the compiler binary via `include_str!` (see [`EMBEDDED`]). Project-local
//! and third-party roots (resolved from the filesystem or a dependency
//! manifest) are future providers that plug into the same [`resolve`] seam;
//! only the embedded `coddl` provider is wired today.

use std::fmt;

/// A module path, e.g. `coddl::core`.
///
/// Segments are the `::`-separated identifiers, in order. Used both as a
/// resolver key and — in the typechecker — as the "owning module" tag on a
/// signature and the membership key of a file's active-module set, so it is
/// cheap to clone, compare, and hash.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ModulePath(Box<[String]>);

impl ModulePath {
    /// Build a path from its segments.
    pub fn new(segments: impl IntoIterator<Item = String>) -> Self {
        ModulePath(segments.into_iter().collect())
    }

    /// Parse a `::`-separated path (`"coddl::core"`). For embedded/known keys,
    /// not user input — the parser builds paths from CST tokens, not this.
    pub fn parse(s: &str) -> Self {
        ModulePath(s.split("::").map(str::to_string).collect())
    }

    /// The `::`-separated segments, in order.
    pub fn segments(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for ModulePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("::")?;
            }
            f.write_str(seg)?;
        }
        Ok(())
    }
}

/// A resolved module: its canonical path and its source text.
#[derive(Clone, Debug)]
pub struct ModuleSource {
    pub path: ModulePath,
    pub source: &'static str,
}

/// One embedded stdlib module: its canonical path and `include_str!`'d source.
struct Embedded {
    path: &'static str,
    source: &'static str,
}

/// The embedded `coddl` root. Every module under the reserved `coddl::`
/// namespace lives here, compiled into the binary so the toolchain stays
/// self-contained. Listed in load order.
const EMBEDDED: &[Embedded] = &[
    Embedded {
        path: "coddl::core",
        source: include_str!("../modules/coddl/core.cd"),
    },
    Embedded {
        path: "coddl::web",
        source: include_str!("../modules/coddl/web.cd"),
    },
    Embedded {
        path: "coddl::env",
        source: include_str!("../modules/coddl/env.cd"),
    },
];

/// Resolve a module path to its source, or `None` if no provider owns it.
///
/// Today the only provider is the embedded `coddl` root; filesystem and
/// manifest providers slot in here later.
pub fn resolve(path: &ModulePath) -> Option<ModuleSource> {
    let key = path.to_string();
    EMBEDDED
        .iter()
        .find(|m| m.path == key)
        .map(|m| ModuleSource {
            path: path.clone(),
            source: m.source,
        })
}

/// Every embedded stdlib module, in load order. The typechecker iterates this
/// to register each module's signatures, tagging them with their owning path.
pub fn stdlib_modules() -> impl Iterator<Item = ModuleSource> {
    EMBEDDED.iter().map(|m| ModuleSource {
        path: ModulePath::parse(m.path),
        source: m.source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_display_round_trips() {
        assert_eq!(ModulePath::parse("coddl::core").to_string(), "coddl::core");
        assert_eq!(
            ModulePath::parse("coddl::core").segments(),
            &["coddl".to_string(), "core".to_string()]
        );
    }

    #[test]
    fn core_resolves_to_nonempty_source() {
        let src = resolve(&ModulePath::parse("coddl::core"))
            .expect("coddl::core is always embedded");
        assert!(src.source.contains("builtin oper"));
    }

    #[test]
    fn unknown_module_does_not_resolve() {
        assert!(resolve(&ModulePath::parse("coddl::nope")).is_none());
    }

    #[test]
    fn stdlib_modules_includes_core() {
        let paths: Vec<String> = stdlib_modules().map(|m| m.path.to_string()).collect();
        assert!(paths.contains(&"coddl::core".to_string()));
    }
}

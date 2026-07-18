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
//! A root is a [`ModuleProvider`]: it maps a [`ModulePath`] to its source text,
//! or declines. `coddl::` is a closed, compiler-owned root whose module sources
//! are embedded into the compiler binary via `include_str!` (see [`EMBEDDED`]);
//! its provider is [`EmbeddedProvider`], which lives here. Project-local and
//! third-party roots (resolved from the filesystem or a dependency manifest) are
//! sibling providers assembled by the driver/plan layer — they need I/O and file
//! paths, which this below-the-typechecker crate deliberately has none of. They
//! plug in behind the same [`ModuleProvider`] trait; a [`ModuleSource`] carries
//! `Cow` text so a filesystem provider can hand back an owned `String` while the
//! embedded provider hands back a `'static` borrow.

use std::borrow::Cow;
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
///
/// The source is `Cow`-typed so a provider can hand back either a `'static`
/// borrow (the embedded `coddl::` root, whose sources are `include_str!`'d) or
/// owned text (a filesystem provider, which reads the file at resolve time).
#[derive(Clone, Debug)]
pub struct ModuleSource {
    pub path: ModulePath,
    source: Cow<'static, str>,
}

impl ModuleSource {
    /// Build a resolved module from its path and source text. The source may be
    /// a `'static` borrow or owned — `&'static str` and `String` both coerce.
    pub fn new(path: ModulePath, source: impl Into<Cow<'static, str>>) -> Self {
        ModuleSource {
            path,
            source: source.into(),
        }
    }

    /// The module's source text.
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// A source of module text under one root. The resolver consults an ordered
/// list of providers; the first to claim a [`ModulePath`] wins.
///
/// Object-safe, so heterogeneous roots — the embedded `coddl::` root, a
/// project-local filesystem root, a future dependency-manifest root — coexist
/// behind `dyn ModuleProvider`.
pub trait ModuleProvider {
    /// Resolve `path` to its source, or `None` if this provider does not own it.
    fn resolve(&self, path: &ModulePath) -> Option<ModuleSource>;
}

/// The embedded `coddl::` root: every stdlib module compiled into the binary
/// via `include_str!` (see [`EMBEDDED`]). Owns the reserved `coddl` first
/// segment and nothing else.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmbeddedProvider;

impl ModuleProvider for EmbeddedProvider {
    fn resolve(&self, path: &ModulePath) -> Option<ModuleSource> {
        let key = path.to_string();
        EMBEDDED
            .iter()
            .find(|m| m.path == key)
            .map(|m| ModuleSource::new(path.clone(), Cow::Borrowed(m.source)))
    }
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
    Embedded {
        path: "coddl::storage",
        source: include_str!("../modules/coddl/storage.cd"),
    },
    Embedded {
        path: "coddl::catalog",
        source: include_str!("../modules/coddl/catalog.cd"),
    },
];

/// Resolve a module path against the embedded `coddl::` root, or `None`.
///
/// A convenience over [`EmbeddedProvider`] for callers that only ever look up
/// stdlib modules (e.g. the typechecker loading `coddl::core`). Project-local
/// and manifest roots are separate [`ModuleProvider`]s the driver assembles.
pub fn resolve(path: &ModulePath) -> Option<ModuleSource> {
    EmbeddedProvider.resolve(path)
}

/// Every embedded stdlib module, in load order. The typechecker iterates this
/// to register each module's signatures, tagging them with their owning path.
pub fn stdlib_modules() -> impl Iterator<Item = ModuleSource> {
    EMBEDDED
        .iter()
        .map(|m| ModuleSource::new(ModulePath::parse(m.path), Cow::Borrowed(m.source)))
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
        let src =
            resolve(&ModulePath::parse("coddl::core")).expect("coddl::core is always embedded");
        assert!(src.source().contains("builtin oper"));
    }

    #[test]
    fn unknown_module_does_not_resolve() {
        assert!(resolve(&ModulePath::parse("coddl::nope")).is_none());
    }

    #[test]
    fn embedded_provider_owns_only_the_coddl_root() {
        // The embedded provider claims stdlib paths and declines everything
        // else — a non-`coddl` root belongs to a sibling provider (filesystem).
        let p = EmbeddedProvider;
        assert!(p.resolve(&ModulePath::parse("coddl::core")).is_some());
        assert!(p.resolve(&ModulePath::parse("mymod")).is_none());
    }

    #[test]
    fn module_source_carries_owned_text() {
        // A filesystem provider hands back owned text; the `Cow` widening must
        // accept a `String` and expose it through `source()` like a borrow.
        let owned = String::from("module m;\noper helper {} [ ]\n");
        let ms = ModuleSource::new(ModulePath::parse("m"), owned.clone());
        assert_eq!(ms.source(), owned);
    }

    #[test]
    fn stdlib_modules_includes_core() {
        let paths: Vec<String> = stdlib_modules().map(|m| m.path.to_string()).collect();
        assert!(paths.contains(&"coddl::core".to_string()));
    }
}

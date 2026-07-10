//! Userspace module resolution.
//!
//! Walks the `use module` imports reachable from an entry `.cd`, resolving each
//! **userspace** module (a non-`coddl` path) to a sibling `<leaf>.cd` file via
//! the project-local [`FsProvider`], validating the file/header contract, and
//! detecting import cycles. The result — a [`ModuleGraph`] of resolved modules
//! in dependency-first order — is what a later phase type-checks and lowers.
//!
//! `coddl::` imports are the embedded stdlib's concern (the typechecker's
//! `resolve_modules` handles them); this walk skips them.
//!
//! Diagnostics use zero-span [`crate::plain_error`]s whose messages name the
//! importing file, the module, and the expected path — consistent with the
//! other compilation-unit diagnostics (PL0012, PL0100). Precise per-file spans
//! arrive with the multi-file source map.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use coddl_diagnostics::{Diagnostic, FileId};
use coddl_stdlib::{ModulePath, ModuleProvider, ModuleSource};
use coddl_syntax::ast::{AstNode, Item, Root};
use coddl_syntax::cst::SyntaxNode;
use coddl_syntax::{parse, FileKind};

use crate::plain_error;
use crate::plan::FileHeaderKind;

/// The project-local filesystem module root.
///
/// Resolves a single-segment userspace leaf `foo` to a sibling `foo.cd` under
/// `base` (the importing file's directory) — the same by-convention resolution
/// as `database greetings;` → `greetings.cddb`, so a module path is a *logical
/// name, never a filesystem path in source*. Declines the reserved `coddl` root
/// (owned by the embedded provider) and, for v1, multi-segment paths (nested
/// userspace modules are a later provider enhancement).
pub struct FsProvider<'a> {
    base: PathBuf,
    overrides: &'a HashMap<PathBuf, String>,
}

impl<'a> FsProvider<'a> {
    /// Anchor a filesystem provider at `base`, consulting `overrides` (unsaved
    /// LSP buffers, keyed by exact path) before touching disk.
    pub fn new(base: impl Into<PathBuf>, overrides: &'a HashMap<PathBuf, String>) -> Self {
        FsProvider {
            base: base.into(),
            overrides,
        }
    }

    /// The sibling file a single-segment userspace leaf resolves to, or `None`
    /// when this provider does not own the path (reserved root / multi-segment).
    fn sibling_path(&self, path: &ModulePath) -> Option<PathBuf> {
        match path.segments() {
            [leaf] if leaf != "coddl" => Some(self.base.join(format!("{leaf}.cd"))),
            _ => None,
        }
    }
}

impl ModuleProvider for FsProvider<'_> {
    fn resolve(&self, path: &ModulePath) -> Option<ModuleSource> {
        let file = self.sibling_path(path)?;
        let source = match self.overrides.get(&file) {
            Some(s) => s.clone(),
            None => std::fs::read_to_string(&file).ok()?,
        };
        Some(ModuleSource::new(path.clone(), source))
    }
}

/// A resolved userspace module.
#[derive(Debug, Clone)]
pub struct ResolvedModule {
    /// The module's logical path (a single-segment leaf in v1).
    pub path: ModulePath,
    /// The file it resolved to.
    pub file: PathBuf,
    /// Its source text, read at resolve time.
    pub source: String,
}

/// The transitive userspace-module graph reachable from an entry `.cd`.
///
/// Nodes are resolved userspace modules; the entry `program`/`library` is not a
/// node (it cannot be imported). `modules` is in **dependency-first** order (a
/// module appears after every module it imports), so a consumer can process
/// each unit once its imports are already available. Empty when the entry file
/// imports no userspace modules — the common case for every existing example.
#[derive(Debug, Default, Clone)]
pub struct ModuleGraph {
    pub modules: Vec<ResolvedModule>,
}

/// Build and validate the userspace module graph rooted at `entry_tree`.
///
/// `base` is the entry file's directory; `entry_display` names it for messages;
/// `overrides` feeds unsaved buffers (the LSP path). Appends PL0016 (unresolved
/// import / unsupported path shape), PL0017 (header name ≠ file name), PL0018
/// (target is not a `module`), and PL0019 (import cycle) to `diags`.
pub(crate) fn resolve_module_graph(
    entry_tree: &SyntaxNode,
    entry_display: &str,
    base: &Path,
    overrides: &HashMap<PathBuf, String>,
    diags: &mut Vec<Diagnostic>,
) -> ModuleGraph {
    let provider = FsProvider::new(base, overrides);
    let mut graph = ModuleGraph::default();
    let mut state = WalkState {
        stack: Vec::new(),
        done: HashSet::new(),
    };
    walk(
        entry_tree,
        entry_display,
        &provider,
        &mut graph,
        &mut state,
        diags,
    );
    graph
}

/// DFS bookkeeping for the transitive walk.
struct WalkState {
    /// The active import path (module leaves), for cycle detection.
    stack: Vec<String>,
    /// Leaves already fully resolved into the graph (diamond dedup).
    done: HashSet<String>,
}

fn walk(
    tree: &SyntaxNode,
    importer_display: &str,
    provider: &FsProvider,
    graph: &mut ModuleGraph,
    state: &mut WalkState,
    diags: &mut Vec<Diagnostic>,
) {
    for path in use_module_paths(tree) {
        // The reserved `coddl` root is the embedded stdlib — the typechecker
        // resolves `coddl::*`, so the userspace graph skips it.
        if path.segments().first().map(String::as_str) == Some("coddl") {
            continue;
        }
        // v1 supports single-segment userspace leaves only.
        let [leaf] = path.segments() else {
            diags.push(plain_error(
                "PL0016",
                format!(
                    "cannot resolve `use module {path};` in {importer_display}: nested \
                     userspace module paths are not yet supported — use a single-segment name"
                ),
            ));
            continue;
        };
        let leaf = leaf.clone();

        // Cycle: the leaf is already on the active import path.
        if state.stack.iter().any(|s| *s == leaf) {
            let chain = state
                .stack
                .iter()
                .cloned()
                .chain([leaf.clone()])
                .collect::<Vec<_>>()
                .join(" → ");
            diags.push(plain_error(
                "PL0019",
                format!("import cycle among modules: {chain}"),
            ));
            continue;
        }
        // Already resolved via another path (diamond) — don't re-read.
        if state.done.contains(&leaf) {
            continue;
        }

        // Resolve the sibling file.
        let Some(ms) = provider.resolve(&path) else {
            let expected = provider
                .sibling_path(&path)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| format!("{leaf}.cd"));
            diags.push(plain_error(
                "PL0016",
                format!(
                    "cannot resolve `use module {leaf};` in {importer_display}: \
                     no readable module file at {expected}"
                ),
            ));
            continue;
        };

        let file = provider
            .sibling_path(&path)
            .expect("a resolved userspace path has a sibling file");
        let file_display = file.display().to_string();
        let parsed = parse(ms.source(), FileId(0), FileKind::Cd);
        let (kind, name) = read_header(&parsed.tree);

        // Must be a `module` — not a `program`/`library`, not headerless.
        if kind != Some(FileHeaderKind::Module) {
            let what = match kind {
                Some(FileHeaderKind::Program) => "a `program`",
                Some(FileHeaderKind::Library) => "a `library`",
                _ => "not a valid `module` (no `module` header)",
            };
            diags.push(plain_error(
                "PL0018",
                format!(
                    "`use module {leaf};` in {importer_display} targets {file_display}, but that \
                     is {what} — `use module` links `module` units only"
                ),
            ));
            continue;
        }

        // The header name must match the file name exactly — the guard against
        // case-folding filesystems (macOS/Windows), where `foo.cd` and `Foo.cd`
        // are the same file: the self-declared header is source-of-truth.
        if name.as_deref() != Some(leaf.as_str()) {
            let declared = name.unwrap_or_default();
            diags.push(plain_error(
                "PL0017",
                format!(
                    "module file {file_display} declares `module {declared};` but must declare \
                     `module {leaf};` — the header name must match the file name exactly"
                ),
            ));
            continue;
        }

        // A valid module: recurse into its own imports first (so dependencies
        // land in the graph before dependents), then record it.
        state.stack.push(leaf.clone());
        walk(&parsed.tree, &file_display, provider, graph, state, diags);
        state.stack.pop();
        state.done.insert(leaf);
        graph.modules.push(ResolvedModule {
            path,
            file,
            source: ms.source().to_string(),
        });
    }
}

/// The module paths named by every `use module …;` item in `tree`, in source
/// order. Malformed (empty) paths are skipped — the parser already reported them.
fn use_module_paths(tree: &SyntaxNode) -> Vec<ModulePath> {
    let Some(root) = Root::cast(tree.clone()) else {
        return Vec::new();
    };
    root.items()
        .filter_map(|item| {
            let Item::UseDecl(u) = item else {
                return None;
            };
            let segs: Vec<String> = u.segments().map(|t| t.text().to_string()).collect();
            (!segs.is_empty()).then(|| ModulePath::new(segs))
        })
        .collect()
}

/// Read a `.cd`'s file-header kind and declared name without emitting anything
/// (unlike the entry file's `validate_file_header`, which enforces the full
/// header rules). Reads the first `ProgramDecl`'s leading keyword and name.
fn read_header(tree: &SyntaxNode) -> (Option<FileHeaderKind>, Option<String>) {
    let Some(root) = Root::cast(tree.clone()) else {
        return (None, None);
    };
    for item in root.items() {
        if let Item::ProgramDecl(d) = item {
            let kind = match d.kind().map(|t| t.text().to_string()).as_deref() {
                Some("program") => Some(FileHeaderKind::Program),
                Some("library") => Some(FileHeaderKind::Library),
                Some("module") => Some(FileHeaderKind::Module),
                _ => None,
            };
            let name = d.name().map(|t| t.text().to_string());
            return (kind, name);
        }
    }
    (None, None)
}

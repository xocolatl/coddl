//! Project plan: discover a `.cd` entry point's companions and
//! cross-validate that the chain holds at every hand-off.
//!
//! Public surface: `discover_and_validate(cd_path) -> PlanOutput`
//! parses the `.cd`, reads its `database <name>;` binding, walks to
//! the same-directory `<name>.cddb` and `<name>.cdstore`, and emits
//! a [`Plan`] plus every diagnostic from per-file typechecking and
//! cross-file validation.
//!
//! Phase 16 supports identity mapping only (`Public X` → `Base X`).
//! `.cdmap` files are out of scope this phase; non-identity adapters
//! land in a later phase.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use coddl_diagnostics::{Diagnostic, FileId, Span};
use coddl_syntax::ast::{AstNode, DatabaseBinding, Root};
use coddl_syntax::ast_cddb::CddbRoot;
use coddl_syntax::{parse, FileKind};
use coddl_types::{check, check_program, CheckUnit, RelvarKind, RelvarTable};

mod catalog;
mod modules;
mod plan;
pub use catalog::{resolve_catalog, CatalogPlan, CatalogPlanOutput, ResolvedCatalogRelvar};
pub use modules::{ModuleGraph, ResolvedModule};
pub use plan::{BackendKind, FileHeaderKind, Plan, PlanOutput, ResolvedPublicRelvar, WritePolicy};

/// Discover the `.cd`'s companions and cross-validate the chain.
///
/// On success returns a [`Plan`] with `resolved` entries for every
/// public relvar. On hard discovery failure (cannot read `cd_path`,
/// no `database <name>;` binding when public relvars exist) the
/// plan is `None` and the diagnostics describe what's missing.
///
/// The function does file I/O via [`std::fs::read_to_string`] but
/// mutates nothing outside its return value.
pub fn discover_and_validate(cd_path: &Path) -> PlanOutput {
    discover_and_validate_with_overrides(cd_path, &HashMap::new())
}

/// Same as [`discover_and_validate`], but consults `overrides` before
/// touching the filesystem.
///
/// The LSP (Phase 17) uses this to feed unsaved buffer content into
/// the plan layer: each entry in `overrides` maps a canonicalized
/// path to the in-memory source the editor is showing. Paths not in
/// the map fall through to `std::fs::read_to_string`. The override
/// keys must exactly match the paths the plan layer constructs
/// (`<cd_path>`, `<dir>/<db>.cddb`, `<dir>/<db>.cdstore`) — there's
/// no path-normalization step beyond what the caller already did.
pub fn discover_and_validate_with_overrides(
    cd_path: &Path,
    overrides: &HashMap<PathBuf, String>,
) -> PlanOutput {
    let mut diags: Vec<Diagnostic> = Vec::new();

    let cd_source = match read_source_or_override(cd_path, overrides) {
        Ok(s) => s,
        Err(err) => {
            diags.push(plain_error(
                "PL0100",
                format!("cannot read {}: {err}", cd_path.display()),
            ));
            return PlanOutput {
                plan: None,
                diagnostics: diags,
                module_graph: ModuleGraph::default(),
            };
        }
    };

    // Parse the entry once to walk its `use module` edges; the full multi-unit
    // check (below) re-checks the entry with its imports in scope.
    let entry_parse = parse(&cd_source, FileId(0), FileKind::Cd);

    // Resolve the userspace module graph (`use module <leaf>;` imports → sibling
    // `.cd` files), validating the file/header contract and detecting cycles
    // (PL0016–PL0019). Runs for every entry file, independent of public relvars.
    let base = cd_path.parent().unwrap_or_else(|| Path::new("."));
    let entry_display = cd_path.display().to_string();
    let module_graph = modules::resolve_module_graph(
        &entry_parse.tree,
        &entry_display,
        base,
        overrides,
        &mut diags,
    );

    // Type-check the whole program — the entry plus every resolved userspace
    // module, dependency-first — so cross-module calls resolve and each module
    // body is checked. FileIds: entry = 0; the `.cddb`/`.cdstore` companions
    // reserve 1/2; modules take 3.. so every unit's diagnostics are distinct.
    let mut units: Vec<CheckUnit> = module_graph
        .modules
        .iter()
        .enumerate()
        .map(|(i, m)| CheckUnit {
            module: Some(m.path.clone()),
            source: &m.source,
            file: FileId((i + 3) as u32),
        })
        .collect();
    units.push(CheckUnit {
        module: None,
        source: &cd_source,
        file: FileId(0),
    });
    let program_out = check_program(&units);
    diags.extend(program_out.diagnostics.iter().cloned());
    let cd_check = program_out
        .entry
        .expect("check_program always returns the entry unit's output");

    // Compilation-unit header rules (PL0012–PL0015). Run unconditionally, before
    // the public-relvar branches, so every `.cd` entry point is validated.
    let header_kind = validate_file_header(&cd_check.tree, &mut diags);

    let program_name = extract_program_name(&cd_check.tree);
    let database_binding = find_database_binding(&cd_check.tree);
    let database_name = database_binding
        .as_ref()
        .and_then(|b| b.name().map(|t| t.text().to_string()));

    let has_public = cd_check
        .relvars
        .iter()
        .any(|(_, info)| info.kind == RelvarKind::Public);

    // No public relvars → no companions needed. Empty Plan, no PL
    // diagnostics. Standalone programs (today's Phase 8 path) stay
    // valid on this code path.
    if !has_public {
        return PlanOutput {
            plan: Some(Plan {
                header_kind,
                program_name,
                database_name,
                cd_relvars: cd_check.relvars,
                cddb_relvars: RelvarTable::new(),
                backend_kind: BackendKind::Unknown,
                resolved: Vec::new(),
                db_file_default: None,
                module_graph: module_graph.clone(),
            }),
            diagnostics: diags,
            module_graph,
        };
    }

    // Public relvars present → database binding required.
    let Some(database_name) = database_name else {
        let span = first_public_relvar_span(&cd_check.relvars);
        diags.push(Diagnostic::error(
            span,
            "PL0001",
            "program declares `public relvar`s but has no `database <name>;` binding",
        ));
        return PlanOutput {
            plan: Some(Plan {
                header_kind,
                program_name,
                database_name: None,
                cd_relvars: cd_check.relvars,
                cddb_relvars: RelvarTable::new(),
                backend_kind: BackendKind::Unknown,
                resolved: Vec::new(),
                db_file_default: None,
                module_graph: module_graph.clone(),
            }),
            diagnostics: diags,
            module_graph,
        };
    };

    let cddb_path = base.join(format!("{database_name}.cddb"));

    let cddb_source = match read_source_or_override(&cddb_path, overrides) {
        Ok(s) => Some(s),
        Err(_) => {
            diags.push(plain_error(
                "PL0002",
                format!("missing companion catalog: {}", cddb_path.display()),
            ));
            None
        }
    };

    let cddb_check = cddb_source
        .as_ref()
        .map(|s| check(s, FileId(1), FileKind::Cddb));
    if let Some(c) = &cddb_check {
        diags.extend(c.diagnostics.iter().cloned());
    }

    // Header consistency (PL0004 / PL0005).
    if let Some(c) = &cddb_check {
        if let Some(root) = CddbRoot::cast(c.tree.clone()) {
            if let Some(decl) = root.database() {
                if let Some(tok) = decl.name() {
                    let cddb_db_name = tok.text();
                    if cddb_db_name != database_name {
                        diags.push(Diagnostic::error(
                            token_span(FileId(1), &tok),
                            "PL0004",
                            format!(
                                "`{}` declares `database {cddb_db_name};` but `{}` binds `database {database_name};`",
                                cddb_path.display(),
                                cd_path.display(),
                            ),
                        ));
                    }
                }
            }
        }
    }

    let cddb_relvars = cddb_check
        .as_ref()
        .map(|c| c.relvars.clone())
        .unwrap_or_default();
    // The physical binding no longer comes from a `.cdstore`: table = relvar
    // name and column = attribute (identity — the mapping `coddl::storage`'s
    // design mandates), and backend + file are transitional defaults.
    // TODO(cdstore-loader): resolve backend + connection by querying the loaded
    // `coddl::storage` relations instead of defaulting.
    let backend_kind = BackendKind::Sqlite;
    let db_file_default = Some(canonicalize_against(
        &cddb_path,
        &format!("{database_name}.sqlite"),
    ));

    // Per-public-relvar resolution: identity match, heading
    // equivalence, store-binding lookup, column coverage.
    let mut resolved: Vec<ResolvedPublicRelvar> = Vec::new();
    for (app_name, info) in cd_check.relvars.iter() {
        if info.kind != RelvarKind::Public {
            continue;
        }

        // PL0006: public relvar must have a same-named catalog relvar.
        let Some(catalog) = cddb_relvars.get(app_name) else {
            diags.push(Diagnostic::error(
                info.span,
                "PL0006",
                format!("public relvar `{app_name}` has no matching catalog relvar"),
            ));
            continue;
        };

        // PL0007: heading equivalence on (name, type) set.
        if !info.heading.assignable_to(&catalog.heading) {
            diags.push(Diagnostic::error(
                info.span,
                "PL0007",
                format!(
                    "public relvar `{app_name}` heading {} doesn't match catalog heading {}",
                    info.heading, catalog.heading,
                ),
            ));
            continue;
        }

        // Identity physical mapping: table = relvar name, column = attribute
        // (heading-canonical, name-sorted for determinism).
        let table_name = app_name.to_string();
        let mut columns: Vec<(String, String)> = catalog
            .heading
            .attrs()
            .iter()
            .map(|(a, _)| (a.clone(), a.clone()))
            .collect();
        columns.sort_by(|a, b| a.0.cmp(&b.0));

        resolved.push(ResolvedPublicRelvar {
            app_name: app_name.to_string(),
            catalog_name: app_name.to_string(),
            heading: catalog.heading.clone(),
            table_name,
            columns,
            // The catalog is the truth about the database's keys.
            keys: catalog.keys.clone(),
            // A base catalog relvar maps 1:1 onto a SQL table and is
            // directly writable; a virtual (view) relvar stays read-only
            // until view-updating (`WriteThrough`) lands.
            write_policy: match catalog.kind {
                RelvarKind::Base => WritePolicy::ReadWrite,
                _ => WritePolicy::ReadOnly,
            },
        });
    }

    PlanOutput {
        plan: Some(Plan {
            header_kind,
            program_name,
            database_name: Some(database_name),
            cd_relvars: cd_check.relvars,
            cddb_relvars,
            backend_kind,
            resolved,
            db_file_default,
            module_graph: module_graph.clone(),
        }),
        diagnostics: diags,
        module_graph,
    }
}

/// Resolve `raw` against `base_path`'s parent directory and try to
/// canonicalize. Falls back to the path-joined form when canonicalize
/// fails (e.g., the file doesn't exist yet — the user may seed the DB
/// after build but before run). Always returns an absolute lexical
/// path so the binary is relocatable via `CODDL_<DB>_FILE` override.
pub(crate) fn canonicalize_against(base_path: &Path, raw: &str) -> String {
    let raw_path = Path::new(raw);
    let absolute = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        let parent = base_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        parent.join(raw_path)
    };
    match absolute.canonicalize() {
        Ok(p) => p.display().to_string(),
        Err(_) => absolute.display().to_string(),
    }
}

/// Validate the `.cd`'s mandatory file header and return its kind.
///
/// Compilation-unit rules — enforced here, not in `check()`, so the reusable
/// frontend stays lenient for the LSP's partial buffers and the unit-test
/// fragments that call `check()` directly:
///   * exactly one header, and it is the first item — else **PL0012** (missing
///     or not first) / **PL0013** (more than one);
///   * `program` declares an `oper main` — else **PL0014**;
///   * `library` / `module` declares no `oper main` — else **PL0015**.
///
/// Returns `None` only when no header is present at all.
fn validate_file_header(
    tree: &coddl_syntax::cst::SyntaxNode,
    diags: &mut Vec<Diagnostic>,
) -> Option<FileHeaderKind> {
    use coddl_syntax::ast::Item;

    let root = Root::cast(tree.clone())?;
    let items: Vec<Item> = root.items().collect();

    // Collect every header (all three kinds share the ProgramDecl node).
    let headers: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| match it {
            Item::ProgramDecl(d) => Some((i, d.clone())),
            _ => None,
        })
        .collect();

    let Some((first_idx, header)) = headers.first().cloned() else {
        diags.push(plain_error(
            "PL0012",
            "file has no `program`/`library`/`module` header — every `.cd` file must \
             open with one"
                .to_string(),
        ));
        return None;
    };

    // Header present but not the first item.
    if first_idx != 0 {
        diags.push(Diagnostic::error(
            header_span(&header),
            "PL0012",
            "the `program`/`library`/`module` header must be the first item in the file"
                .to_string(),
        ));
    }

    // More than one header.
    for (_, extra) in headers.iter().skip(1) {
        diags.push(Diagnostic::error(
            header_span(extra),
            "PL0013",
            "a `.cd` file declares exactly one `program`/`library`/`module` header".to_string(),
        ));
    }

    let kind = match header.kind().map(|t| t.text().to_string()).as_deref() {
        Some("program") => FileHeaderKind::Program,
        Some("library") => FileHeaderKind::Library,
        Some("module") => FileHeaderKind::Module,
        // Malformed header (parser already reported the missing keyword/name);
        // treat as a program so a stray `main` isn't double-diagnosed.
        _ => FileHeaderKind::Program,
    };

    let main_oper = items.iter().find_map(|it| match it {
        Item::OperDecl(o) if o.name().map(|t| t.text() == "main").unwrap_or(false) => Some(o),
        _ => None,
    });

    match kind {
        FileHeaderKind::Program if main_oper.is_none() => {
            diags.push(Diagnostic::error(
                header_span(&header),
                "PL0014",
                "a `program` must declare an `oper main` entry point".to_string(),
            ));
        }
        FileHeaderKind::Library | FileHeaderKind::Module if main_oper.is_some() => {
            let span = main_oper
                .and_then(|o| o.name())
                .map(|t| token_span(FileId(0), &t))
                .unwrap_or_else(|| header_span(&header));
            diags.push(Diagnostic::error(
                span,
                "PL0015",
                "a `library`/`module` must not declare an `oper main` — only a `program` \
                 has an entry point"
                    .to_string(),
            ));
        }
        _ => {}
    }

    Some(kind)
}

/// Span of a file header node (`program`/`library`/`module` …), in the `.cd`.
fn header_span(header: &coddl_syntax::ast::ProgramDecl) -> Span {
    let r = header.syntax().text_range();
    Span::new(FileId(0), r.start().into(), r.end().into())
}

fn extract_program_name(tree: &coddl_syntax::cst::SyntaxNode) -> String {
    let Some(root) = Root::cast(tree.clone()) else {
        return String::new();
    };
    for item in root.items() {
        if let coddl_syntax::ast::Item::ProgramDecl(p) = item {
            if let Some(name) = p.name() {
                return name.text().to_string();
            }
        }
    }
    String::new()
}

fn find_database_binding(tree: &coddl_syntax::cst::SyntaxNode) -> Option<DatabaseBinding> {
    let root = Root::cast(tree.clone())?;
    for item in root.items() {
        if let coddl_syntax::ast::Item::DatabaseBinding(b) = item {
            return Some(b);
        }
    }
    None
}

fn first_public_relvar_span(table: &RelvarTable) -> Span {
    table
        .iter()
        .find(|(_, info)| info.kind == RelvarKind::Public)
        .map(|(_, info)| info.span)
        .unwrap_or_else(|| Span::new(FileId(0), 0, 0))
}

pub(crate) fn token_span(file: FileId, token: &coddl_syntax::cst::SyntaxToken) -> Span {
    let r = token.text_range();
    Span::new(file, r.start().into(), r.end().into())
}

pub(crate) fn plain_error(code: &'static str, message: String) -> Diagnostic {
    Diagnostic::error(Span::new(FileId(0), 0, 0), code, message)
}

/// Read `path`'s source: from `overrides` if present (in-memory
/// buffer wins), else from disk. The override map keys must match
/// the paths the plan layer constructs verbatim.
pub(crate) fn read_source_or_override(
    path: &Path,
    overrides: &HashMap<PathBuf, String>,
) -> std::io::Result<String> {
    if let Some(s) = overrides.get(path) {
        return Ok(s.clone());
    }
    std::fs::read_to_string(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Write a `.cd` plus its `greetings.cddb` companion into a fresh tempdir
    /// and return the tempdir plus the `.cd` path. There is no `.cdstore` — the
    /// plan layer no longer reads one (identity mapping + defaults).
    fn write_project(cd: &str, cddb: Option<&str>) -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let cd_path = dir.path().join("app.cd");
        fs::write(&cd_path, cd).unwrap();
        if let Some(s) = cddb {
            fs::write(dir.path().join("greetings.cddb"), s).unwrap();
        }
        (dir, cd_path)
    }

    const CD_HELLO: &str = "\
program hello;
database greetings;
public relvar Greetings { id: Integer, message: Text } key { id };
oper main {} [ ]
";

    const CDDB_GREETINGS: &str = "\
database greetings;
base relvar Greetings { id: Integer, message: Text } key { id };
";

    fn codes(diags: &[Diagnostic]) -> Vec<&'static str> {
        diags.iter().map(|d| d.code).collect()
    }

    #[test]
    fn hello_world_db_resolves_cleanly() {
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS));
        let out = discover_and_validate(&cd);
        let pl: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code.starts_with("PL"))
            .map(|d| d.code)
            .collect();
        assert!(pl.is_empty(), "unexpected PL diagnostics: {pl:?}");

        let plan = out.plan.expect("plan");
        assert_eq!(plan.program_name, "hello");
        assert_eq!(plan.database_name.as_deref(), Some("greetings"));
        // Backend + file are transitional defaults (TODO cdstore-loader).
        assert_eq!(plan.backend_kind, BackendKind::Sqlite);
        assert_eq!(plan.resolved.len(), 1);
        let r = &plan.resolved[0];
        assert_eq!(r.app_name, "Greetings");
        assert_eq!(r.catalog_name, "Greetings");
        // Identity mapping: table = relvar name, column = same-named attribute.
        assert_eq!(r.table_name, "Greetings");
        // A base catalog relvar is directly writable.
        assert_eq!(r.write_policy, WritePolicy::ReadWrite);
        let col_attrs: Vec<&str> = r.columns.iter().map(|(a, _)| a.as_str()).collect();
        assert!(col_attrs.contains(&"id"));
        assert!(col_attrs.contains(&"message"));
        for (a, c) in &r.columns {
            assert_eq!(a, c, "column is the same-named attribute");
        }
    }

    #[test]
    fn no_public_relvars_empty_plan() {
        let cd = "program p;\noper main {} [];\n";
        let (_dir, cd_path) = write_project(cd, None);
        let out = discover_and_validate(&cd_path);
        let pl: Vec<_> = codes(&out.diagnostics)
            .into_iter()
            .filter(|c| c.starts_with("PL"))
            .collect();
        assert!(pl.is_empty(), "expected no PL diagnostics, got {pl:?}");
        let plan = out.plan.expect("plan");
        assert!(plan.resolved.is_empty());
        assert_eq!(plan.backend_kind, BackendKind::Unknown);
    }

    #[test]
    fn public_relvar_without_binding_emits_pl0001() {
        let cd = "\
program p;
public relvar X { a: Integer } key { a };
";
        let (_dir, cd_path) = write_project(cd, None);
        let out = discover_and_validate(&cd_path);
        assert!(codes(&out.diagnostics).contains(&"PL0001"));
    }

    #[test]
    fn missing_cddb_emits_pl0002() {
        let (_dir, cd) = write_project(CD_HELLO, None);
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0002"));
    }

    #[test]
    fn cddb_header_mismatch_emits_pl0004() {
        let bad_cddb = "\
database other;
base relvar Greetings { id: Integer, message: Text } key { id };
";
        let (_dir, cd) = write_project(CD_HELLO, Some(bad_cddb));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0004"));
    }

    #[test]
    fn missing_catalog_relvar_emits_pl0006() {
        let empty_cddb = "database greetings;\n";
        let (_dir, cd) = write_project(CD_HELLO, Some(empty_cddb));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0006"));
    }

    #[test]
    fn heading_mismatch_emits_pl0007() {
        let mismatched_cddb = "\
database greetings;
base relvar Greetings { id: Integer, message: Boolean } key { id };
";
        let (_dir, cd) = write_project(CD_HELLO, Some(mismatched_cddb));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0007"));
    }

    #[test]
    fn overrides_with_empty_map_matches_disk_only_behavior() {
        let (dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS));
        let baseline = discover_and_validate(&cd);
        let with_empty = discover_and_validate_with_overrides(&cd, &HashMap::new());
        // Same PL-code set (the per-file T-code diagnostics carry
        // identical spans / messages too, but we don't assert on
        // those here — codes are the contract).
        let base_codes: Vec<_> = codes(&baseline.diagnostics);
        let over_codes: Vec<_> = codes(&with_empty.diagnostics);
        assert_eq!(base_codes, over_codes);
        let _ = dir; // keep tempdir alive
    }

    #[test]
    fn override_for_cddb_wins_over_disk() {
        let (dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS));

        // First confirm the disk version validates clean.
        let clean = discover_and_validate(&cd);
        assert!(
            !codes(&clean.diagnostics)
                .iter()
                .any(|c| c.starts_with("PL")),
            "baseline should be clean"
        );

        // Inject an in-memory CDDB whose heading mismatches the .cd.
        // Disk file still has the matching shape, so we know the
        // PL0007 came from the override and not from disk.
        let bad_cddb = "\
database greetings;
base relvar Greetings { id: Integer, message: Boolean } key { id };
";
        let mut overrides = HashMap::new();
        overrides.insert(dir.path().join("greetings.cddb"), bad_cddb.to_string());
        let out = discover_and_validate_with_overrides(&cd, &overrides);
        assert!(codes(&out.diagnostics).contains(&"PL0007"));
    }

    #[test]
    fn sqlite_backed_cd_family_resolves_cleanly() {
        // Owns its source: author a `.cd` plus its `greetings.cddb` companion in
        // a tempdir, then discover + validate the family. No on-disk fixture
        // dependency and no `.cdstore` (identity mapping + defaults).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("app.cd"),
            "program hello_world_db;\n\
             database greetings;\n\
             public relvar Greetings { id: Integer, message: Text } key { id };\n\
             oper main {} [\n\
                 let g = transaction [ extract (Greetings where id = 1 project { message }) ];\n\
                 write_line { message: g.message };\n\
             ];\n",
        )
        .expect("write app.cd");
        std::fs::write(
            tmp.path().join("greetings.cddb"),
            "database greetings;\n\
             base relvar Greetings { id: Integer, message: Text } key { id };\n",
        )
        .expect("write greetings.cddb");

        let out = discover_and_validate(&tmp.path().join("app.cd"));
        let pl: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code.starts_with("PL"))
            .map(|d| d.code)
            .collect();
        assert!(pl.is_empty(), "unexpected PL diagnostics: {pl:?}");

        let plan = out.plan.expect("plan");
        assert_eq!(plan.database_name.as_deref(), Some("greetings"));
        assert_eq!(plan.backend_kind, BackendKind::Sqlite);
        assert_eq!(plan.resolved.len(), 1);
        assert_eq!(plan.resolved[0].app_name, "Greetings");
    }

    // ── File-kind header rules (PL0012–PL0015) ───────────────────────────

    /// PL-codes only, from a standalone `.cd` (no companions needed).
    fn pl_codes(cd: &str) -> Vec<&'static str> {
        let (_dir, cd_path) = write_project(cd, None);
        discover_and_validate(&cd_path)
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .filter(|c| c.starts_with("PL"))
            .collect()
    }

    #[test]
    fn clean_program_library_module_have_no_pl_and_right_kind() {
        for (cd, kind) in [
            ("program p;\noper main {} [ ]\n", FileHeaderKind::Program),
            ("library l;\noper handle {} [ ]\n", FileHeaderKind::Library),
            ("module m;\noper helper {} [ ]\n", FileHeaderKind::Module),
        ] {
            let (_dir, cd_path) = write_project(cd, None);
            let out = discover_and_validate(&cd_path);
            let pl: Vec<_> = out
                .diagnostics
                .iter()
                .filter(|d| d.code.starts_with("PL"))
                .map(|d| d.code)
                .collect();
            assert!(pl.is_empty(), "{cd:?}: unexpected {pl:?}");
            assert_eq!(out.plan.unwrap().header_kind, Some(kind), "{cd:?}");
        }
    }

    #[test]
    fn headerless_file_is_pl0012() {
        assert!(pl_codes("oper main {} [ ]\n").contains(&"PL0012"));
    }

    #[test]
    fn header_not_first_is_pl0012() {
        let codes = pl_codes("oper helper {} [ ]\nmodule m;\n");
        assert!(codes.contains(&"PL0012"), "{codes:?}");
    }

    #[test]
    fn two_headers_is_pl0013() {
        assert!(pl_codes("program p;\nlibrary l;\noper main {} [ ]\n").contains(&"PL0013"));
    }

    #[test]
    fn program_without_main_is_pl0014() {
        let codes = pl_codes("program p;\noper helper {} [ ]\n");
        assert!(codes.contains(&"PL0014"), "{codes:?}");
    }

    #[test]
    fn library_or_module_with_main_is_pl0015() {
        assert!(pl_codes("library l;\noper main {} [ ]\n").contains(&"PL0015"));
        assert!(pl_codes("module m;\noper main {} [ ]\n").contains(&"PL0015"));
    }

    // ── Userspace module resolution (PL0016–PL0019) ──────────────────────

    /// Write an entry `app.cd` plus sibling files (name → contents) into a
    /// fresh tempdir; return the dir (kept alive) and the entry path.
    fn write_unit(entry: &str, siblings: &[(&str, &str)]) -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let entry_path = dir.path().join("app.cd");
        fs::write(&entry_path, entry).unwrap();
        for (name, body) in siblings {
            fs::write(dir.path().join(name), body).unwrap();
        }
        (dir, entry_path)
    }

    /// PL-codes from resolving `cd_path`.
    fn pl_of(cd_path: &Path) -> Vec<&'static str> {
        discover_and_validate(cd_path)
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .filter(|c| c.starts_with("PL"))
            .collect()
    }

    #[test]
    fn userspace_import_resolves_cleanly() {
        let (_d, cd) = write_unit(
            "program app;\nuse module foo;\noper main {} [ ]\n",
            &[("foo.cd", "module foo;\noper helper {} [ ]\n")],
        );
        let out = discover_and_validate(&cd);
        let pl: Vec<_> = out
            .diagnostics
            .iter()
            .map(|d| d.code)
            .filter(|c| c.starts_with("PL"))
            .collect();
        assert!(pl.is_empty(), "unexpected {pl:?}");
        assert_eq!(out.module_graph.modules.len(), 1);
        assert_eq!(out.module_graph.modules[0].path.to_string(), "foo");
    }

    #[test]
    fn missing_module_file_is_pl0016() {
        let (_d, cd) = write_unit("program app;\nuse module gone;\noper main {} [ ]\n", &[]);
        assert!(pl_of(&cd).contains(&"PL0016"));
    }

    #[test]
    fn nested_userspace_path_is_pl0016() {
        // Multi-segment userspace paths (nested modules) are not yet supported.
        let (_d, cd) = write_unit("program app;\nuse module a::b;\noper main {} [ ]\n", &[]);
        assert!(pl_of(&cd).contains(&"PL0016"));
    }

    #[test]
    fn header_name_not_matching_file_is_pl0017() {
        let (_d, cd) = write_unit(
            "program app;\nuse module foo;\noper main {} [ ]\n",
            &[("foo.cd", "module bar;\noper helper {} [ ]\n")],
        );
        assert!(pl_of(&cd).contains(&"PL0017"));
    }

    #[test]
    fn case_mismatched_header_is_pl0017() {
        // The case-fold guard: on a case-insensitive filesystem `foo.cd` and
        // `Foo.cd` are the same file, so the header's case must match exactly.
        let (_d, cd) = write_unit(
            "program app;\nuse module foo;\noper main {} [ ]\n",
            &[("foo.cd", "module Foo;\noper helper {} [ ]\n")],
        );
        assert!(pl_of(&cd).contains(&"PL0017"));
    }

    #[test]
    fn importing_a_non_module_is_pl0018() {
        for target in [
            ("lib.cd", "library lib;\noper handle {} [ ]\n"),
            ("prog.cd", "program prog;\noper main {} [ ]\n"),
        ] {
            let leaf = target.0.strip_suffix(".cd").unwrap();
            let (_d, cd) = write_unit(
                &format!("program app;\nuse module {leaf};\noper main {{}} [ ]\n"),
                &[target],
            );
            assert!(pl_of(&cd).contains(&"PL0018"), "{}", target.0);
        }
    }

    #[test]
    fn import_cycle_is_pl0019() {
        let (_d, cd) = write_unit(
            "program app;\nuse module a;\noper main {} [ ]\n",
            &[
                ("a.cd", "module a;\nuse module b;\noper ha {} [ ]\n"),
                ("b.cd", "module b;\nuse module a;\noper hb {} [ ]\n"),
            ],
        );
        assert!(pl_of(&cd).contains(&"PL0019"));
    }

    #[test]
    fn transitive_graph_is_dependency_first() {
        // entry → a → b: `b` (a dependency) precedes `a` (its dependent).
        let (_d, cd) = write_unit(
            "program app;\nuse module a;\noper main {} [ ]\n",
            &[
                ("a.cd", "module a;\nuse module b;\noper ha {} [ ]\n"),
                ("b.cd", "module b;\noper hb {} [ ]\n"),
            ],
        );
        let out = discover_and_validate(&cd);
        assert!(pl_of(&cd).is_empty(), "{:?}", pl_of(&cd));
        let names: Vec<String> = out
            .module_graph
            .modules
            .iter()
            .map(|m| m.path.to_string())
            .collect();
        assert_eq!(names, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn diamond_imports_resolve_each_module_once() {
        let (_d, cd) = write_unit(
            "program app;\nuse module a;\nuse module b;\noper main {} [ ]\n",
            &[
                ("a.cd", "module a;\nuse module c;\noper ha {} [ ]\n"),
                ("b.cd", "module b;\nuse module c;\noper hb {} [ ]\n"),
                ("c.cd", "module c;\noper hc {} [ ]\n"),
            ],
        );
        let out = discover_and_validate(&cd);
        assert_eq!(out.module_graph.modules.len(), 3, "c must appear once");
        let cs = out
            .module_graph
            .modules
            .iter()
            .filter(|m| m.path.to_string() == "c")
            .count();
        assert_eq!(cs, 1);
    }

    #[test]
    fn coddl_import_is_not_a_userspace_module() {
        // A stdlib import is the checker's concern; the filesystem walk skips
        // it, so there is no PL0016 even with no `web.cd` sibling on disk.
        let (_d, cd) = write_unit(
            "program app;\nuse module coddl::web;\noper main {} [ ]\n",
            &[],
        );
        let out = discover_and_validate(&cd);
        let pl: Vec<_> = out
            .diagnostics
            .iter()
            .map(|d| d.code)
            .filter(|c| c.starts_with("PL"))
            .collect();
        assert!(pl.is_empty(), "unexpected {pl:?}");
        assert!(out.module_graph.modules.is_empty());
    }

    /// All diagnostic codes from resolving `cd_path` (T-codes and PL-codes).
    fn all_codes(cd_path: &Path) -> Vec<&'static str> {
        discover_and_validate(cd_path)
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn entry_call_to_imported_oper_resolves_through_discover() {
        // The full-program payoff at the plan layer: the entry's call to a
        // module's exported oper resolves (no T0001) because `discover_and_validate`
        // runs the multi-unit `check_program` over the entry + module.
        let (_d, cd) = write_unit(
            "program app;\nuse module greet;\noper main {} [ hello {}; ];\n",
            &[("greet.cd", "module greet;\noper hello {} [ ];\n")],
        );
        assert!(
            !all_codes(&cd).contains(&"T0001"),
            "imported `hello` must resolve: {:?}",
            all_codes(&cd)
        );
    }

    #[test]
    fn entry_call_without_import_is_t0001_through_discover() {
        // Same module on disk, but not imported → out of scope → unresolved.
        let (_d, cd) = write_unit(
            "program app;\noper main {} [ hello {}; ];\n",
            &[("greet.cd", "module greet;\noper hello {} [ ];\n")],
        );
        assert!(all_codes(&cd).contains(&"T0001"));
    }
}

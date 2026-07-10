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
use coddl_syntax::ast_cdstore::{
    BackendDecl, CdstoreRoot, CdstoreValue, ColumnsBlock, RelvarBinding,
};
use coddl_syntax::FileKind;
use coddl_types::{check, Heading, RelvarKind, RelvarTable};

mod modules;
mod plan;
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

    let cd_check = check(&cd_source, FileId(0), FileKind::Cd);
    diags.extend(cd_check.diagnostics.iter().cloned());

    // Compilation-unit header rules (PL0012–PL0015). Run unconditionally, before
    // the public-relvar branches, so every `.cd` entry point is validated.
    let header_kind = validate_file_header(&cd_check.tree, &mut diags);

    // Resolve the userspace module graph (`use module <leaf>;` imports → sibling
    // `.cd` files), validating the file/header contract and detecting cycles
    // (PL0016–PL0019). Runs for every entry file, independent of public relvars.
    let base = cd_path.parent().unwrap_or_else(|| Path::new("."));
    let entry_display = cd_path.display().to_string();
    let module_graph =
        modules::resolve_module_graph(&cd_check.tree, &entry_display, base, overrides, &mut diags);

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
            }),
            diagnostics: diags,
            module_graph,
        };
    };

    let cddb_path = base.join(format!("{database_name}.cddb"));
    let cdstore_path = base.join(format!("{database_name}.cdstore"));

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
    let cdstore_source = match read_source_or_override(&cdstore_path, overrides) {
        Ok(s) => Some(s),
        Err(_) => {
            diags.push(plain_error(
                "PL0003",
                format!("missing companion store: {}", cdstore_path.display()),
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
    let cdstore_check = cdstore_source
        .as_ref()
        .map(|s| check(s, FileId(2), FileKind::Cdstore));
    if let Some(c) = &cdstore_check {
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

    let cdstore_root = cdstore_check
        .as_ref()
        .and_then(|c| CdstoreRoot::cast(c.tree.clone()));
    if let Some(root) = &cdstore_root {
        if let Some(header) = root.header() {
            if let Some(tok) = header.database_name() {
                let cdstore_db_name = tok.text();
                if cdstore_db_name != database_name {
                    diags.push(Diagnostic::error(
                        token_span(FileId(2), &tok),
                        "PL0005",
                        format!(
                            "`{}` declares `store for {cdstore_db_name};` but `{}` binds `database {database_name};`",
                            cdstore_path.display(),
                            cd_path.display(),
                        ),
                    ));
                }
            }
        }
    }

    let cddb_relvars = cddb_check
        .as_ref()
        .map(|c| c.relvars.clone())
        .unwrap_or_default();
    let backend_kind = cdstore_root
        .as_ref()
        .and_then(|r| r.backend())
        .map(|b| classify_backend(&b, &mut diags))
        .unwrap_or(BackendKind::Unknown);
    let db_file_default = if matches!(backend_kind, BackendKind::Sqlite) {
        cdstore_root
            .as_ref()
            .and_then(|r| r.backend())
            .and_then(|b| extract_file_directive(&b))
            .map(|raw| canonicalize_against(&cdstore_path, &raw))
    } else {
        None
    };

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

        // PL0008: catalog relvar must have a store binding.
        let binding = cdstore_root
            .as_ref()
            .and_then(|root| find_binding(root, app_name));
        let Some(binding) = binding else {
            diags.push(Diagnostic::error(
                info.span,
                "PL0008",
                format!("catalog relvar `{app_name}` has no `.cdstore` binding"),
            ));
            continue;
        };

        let table_name = binding
            .table_name()
            .map(|t| unquote(t.text()))
            .unwrap_or_default();

        // PL0009 / PL0010: column coverage.
        let columns = collect_columns(&binding, &catalog.heading, &mut diags);

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
        }),
        diagnostics: diags,
        module_graph,
    }
}

/// Pull the `file: "..."` directive out of a `BackendDecl`. Returns the
/// raw (still-relative) lexeme; the caller canonicalizes against the
/// `.cdstore`'s parent directory.
fn extract_file_directive(decl: &BackendDecl) -> Option<String> {
    for field in decl.fields() {
        let name = field
            .name()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        if name != "file" {
            continue;
        }
        return match field.value() {
            Some(CdstoreValue::String(t)) => Some(unquote(t.text())),
            _ => None,
        };
    }
    None
}

/// Resolve `raw` against the `.cdstore`'s parent directory and try to
/// canonicalize. Falls back to the path-joined form when canonicalize
/// fails (e.g., the file doesn't exist yet — the user may seed the DB
/// after build but before run). Always returns an absolute lexical
/// path so the binary is relocatable via `CODDL_<DB>_FILE` override.
fn canonicalize_against(cdstore_path: &Path, raw: &str) -> String {
    let raw_path = Path::new(raw);
    let absolute = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        let parent = cdstore_path
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

fn classify_backend(decl: &BackendDecl, diags: &mut Vec<Diagnostic>) -> BackendKind {
    let Some(tok) = decl.kind() else {
        return BackendKind::Unknown;
    };
    let kind = tok.text();
    if kind == "sqlite" {
        BackendKind::Sqlite
    } else {
        diags.push(Diagnostic::error(
            token_span(FileId(2), &tok),
            "PL0011",
            format!("backend `{kind}` is not supported (v1 supports `sqlite` only)"),
        ));
        BackendKind::Other(kind.to_string())
    }
}

fn find_binding(root: &CdstoreRoot, name: &str) -> Option<RelvarBinding> {
    root.bindings()
        .find(|b| b.name().map(|t| t.text() == name).unwrap_or(false))
}

fn collect_columns(
    binding: &RelvarBinding,
    heading: &Heading,
    diags: &mut Vec<Diagnostic>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let block: Option<ColumnsBlock> = binding.columns_block();

    let mut seen_attrs: Vec<String> = Vec::new();
    if let Some(block) = &block {
        for field in block.fields() {
            let Some(name_tok) = field.name() else {
                continue;
            };
            let attr_name = name_tok.text().to_string();
            // Shorthand `id` ≡ `id: "id"` — the column name is the attribute
            // name. An explicit `attr: "col"` uses the quoted column string.
            let column_name = if field.is_shorthand() {
                attr_name.clone()
            } else {
                match field.value() {
                    Some(CdstoreValue::String(t)) => unquote(t.text()),
                    _ => String::new(),
                }
            };

            if heading.lookup(&attr_name).is_none() {
                diags.push(Diagnostic::error(
                    token_span(FileId(2), &name_tok),
                    "PL0010",
                    format!(
                        "column entry `{attr_name}` is not in the catalog heading {}",
                        heading,
                    ),
                ));
                continue;
            }
            seen_attrs.push(attr_name.clone());
            out.push((attr_name, column_name));
        }
    }

    // PL0009: every heading attribute must appear in the columns block.
    let binding_name = binding
        .name()
        .map(|t| t.text().to_string())
        .unwrap_or_default();
    for (attr, _) in heading.attrs() {
        if !seen_attrs.iter().any(|a| a == attr) {
            let span = binding
                .name()
                .map(|t| token_span(FileId(2), &t))
                .unwrap_or_else(|| Span::new(FileId(2), 0, 0));
            diags.push(Diagnostic::error(
                span,
                "PL0009",
                format!("binding for `{binding_name}` doesn't cover heading attribute `{attr}`",),
            ));
        }
    }
    out
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

fn token_span(file: FileId, token: &coddl_syntax::cst::SyntaxToken) -> Span {
    let r = token.text_range();
    Span::new(file, r.start().into(), r.end().into())
}

/// Strip surrounding double-quotes from a raw string-literal lexeme.
/// The lexer guarantees the token form: opening `"`, body, closing `"`.
fn unquote(s: &str) -> String {
    let trimmed = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    // The string-literal token preserves escapes verbatim today; the
    // caller (SQL emitter, runtime) decodes them on use. Phase 21's
    // SQLite layer should run the decoder before passing to sqlite3.
    trimmed.to_string()
}

pub(crate) fn plain_error(code: &'static str, message: String) -> Diagnostic {
    Diagnostic::error(Span::new(FileId(0), 0, 0), code, message)
}

/// Read `path`'s source: from `overrides` if present (in-memory
/// buffer wins), else from disk. The override map keys must match
/// the paths the plan layer constructs verbatim.
fn read_source_or_override(
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

    /// Write a triple of files into a fresh tempdir and return its
    /// path plus the path to the `.cd`. `db` is the database name
    /// used to construct the companion file names.
    fn write_project(cd: &str, cddb: Option<&str>, cdstore: Option<&str>) -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let cd_path = dir.path().join("app.cd");
        fs::write(&cd_path, cd).unwrap();
        if let Some(s) = cddb {
            fs::write(dir.path().join("greetings.cddb"), s).unwrap();
        }
        if let Some(s) = cdstore {
            fs::write(dir.path().join("greetings.cdstore"), s).unwrap();
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

    const CDSTORE_GREETINGS: &str = "\
store for greetings;
backend sqlite { file: \"greetings.sqlite\" };
relvar Greetings: table \"greetings\" {
    columns: { id: \"id\", message: \"message\" }
};
";

    fn codes(diags: &[Diagnostic]) -> Vec<&'static str> {
        diags.iter().map(|d| d.code).collect()
    }

    #[test]
    fn hello_world_db_resolves_cleanly() {
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(CDSTORE_GREETINGS));
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
        assert_eq!(plan.backend_kind, BackendKind::Sqlite);
        assert_eq!(plan.resolved.len(), 1);
        let r = &plan.resolved[0];
        assert_eq!(r.app_name, "Greetings");
        assert_eq!(r.catalog_name, "Greetings");
        assert_eq!(r.table_name, "greetings");
        // A base catalog relvar is directly writable.
        assert_eq!(r.write_policy, WritePolicy::ReadWrite);
        // Heading-canonical (sorted) order: id, message.
        let col_attrs: Vec<&str> = r.columns.iter().map(|(a, _)| a.as_str()).collect();
        assert!(col_attrs.contains(&"id"));
        assert!(col_attrs.contains(&"message"));
    }

    #[test]
    fn columns_shorthand_maps_each_attr_to_its_own_name() {
        // `columns: { id, message }` — the shorthand binds each attribute to a
        // column of the same name, exactly like the explicit `id: "id"` form.
        const CDSTORE_SHORTHAND: &str = "\
store for greetings;
backend sqlite { file: \"greetings.sqlite\" };
relvar Greetings: table \"greetings\" {
    columns: { id, message }
};
";
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(CDSTORE_SHORTHAND));
        let out = discover_and_validate(&cd);
        let pl: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code.starts_with("PL"))
            .map(|d| d.code)
            .collect();
        assert!(pl.is_empty(), "unexpected PL diagnostics: {pl:?}");

        let plan = out.plan.expect("plan");
        let r = &plan.resolved[0];
        let mut cols = r.columns.clone();
        cols.sort();
        assert_eq!(
            cols,
            vec![
                ("id".to_string(), "id".to_string()),
                ("message".to_string(), "message".to_string()),
            ],
            "shorthand maps each attribute to a same-named column"
        );
    }

    #[test]
    fn no_public_relvars_empty_plan() {
        let cd = "program p;\noper main {} [];\n";
        let (_dir, cd_path) = write_project(cd, None, None);
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
        let (_dir, cd_path) = write_project(cd, None, None);
        let out = discover_and_validate(&cd_path);
        assert!(codes(&out.diagnostics).contains(&"PL0001"));
    }

    #[test]
    fn missing_cddb_emits_pl0002() {
        let (_dir, cd) = write_project(CD_HELLO, None, Some(CDSTORE_GREETINGS));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0002"));
    }

    #[test]
    fn missing_cdstore_emits_pl0003() {
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), None);
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0003"));
    }

    #[test]
    fn cddb_header_mismatch_emits_pl0004() {
        let bad_cddb = "\
database other;
base relvar Greetings { id: Integer, message: Text } key { id };
";
        let (_dir, cd) = write_project(CD_HELLO, Some(bad_cddb), Some(CDSTORE_GREETINGS));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0004"));
    }

    #[test]
    fn cdstore_header_mismatch_emits_pl0005() {
        let bad_cdstore = "\
store for other;
backend sqlite { file: \"x.sqlite\" };
relvar Greetings: table \"greetings\" {
    columns: { id: \"id\", message: \"message\" }
};
";
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(bad_cdstore));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0005"));
    }

    #[test]
    fn missing_catalog_relvar_emits_pl0006() {
        let empty_cddb = "database greetings;\n";
        let (_dir, cd) = write_project(CD_HELLO, Some(empty_cddb), Some(CDSTORE_GREETINGS));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0006"));
    }

    #[test]
    fn heading_mismatch_emits_pl0007() {
        let mismatched_cddb = "\
database greetings;
base relvar Greetings { id: Integer, message: Boolean } key { id };
";
        let (_dir, cd) = write_project(CD_HELLO, Some(mismatched_cddb), Some(CDSTORE_GREETINGS));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0007"));
    }

    #[test]
    fn missing_store_binding_emits_pl0008() {
        let empty_cdstore = "\
store for greetings;
backend sqlite { file: \"x.sqlite\" };
";
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(empty_cdstore));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0008"));
    }

    #[test]
    fn missing_column_emits_pl0009() {
        let bad_cdstore = "\
store for greetings;
backend sqlite { file: \"x.sqlite\" };
relvar Greetings: table \"greetings\" {
    columns: { id: \"id\" }
};
";
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(bad_cdstore));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0009"));
    }

    #[test]
    fn extra_column_emits_pl0010() {
        let bad_cdstore = "\
store for greetings;
backend sqlite { file: \"x.sqlite\" };
relvar Greetings: table \"greetings\" {
    columns: { id: \"id\", message: \"message\", extra: \"foo\" }
};
";
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(bad_cdstore));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0010"));
    }

    #[test]
    fn unsupported_backend_emits_pl0011() {
        let pg_cdstore = "\
store for greetings;
backend postgres { dsn: \"postgres://x\" };
relvar Greetings: table \"greetings\" {
    columns: { id: \"id\", message: \"message\" }
};
";
        let (_dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(pg_cdstore));
        let out = discover_and_validate(&cd);
        assert!(codes(&out.diagnostics).contains(&"PL0011"));
        let plan = out.plan.unwrap();
        assert_eq!(
            plan.backend_kind,
            BackendKind::Other("postgres".to_string())
        );
    }

    #[test]
    fn overrides_with_empty_map_matches_disk_only_behavior() {
        let (dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(CDSTORE_GREETINGS));
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
        let (dir, cd) = write_project(CD_HELLO, Some(CDDB_GREETINGS), Some(CDSTORE_GREETINGS));

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
        // Owns its source: author a `.cd` plus its `greetings.cddb` /
        // `greetings.cdstore` companions in a tempdir, then discover + validate
        // the family. No on-disk fixture dependency.
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
        std::fs::write(
            tmp.path().join("greetings.cdstore"),
            "store for greetings;\n\
             backend sqlite { file: \"greetings.sqlite\" };\n\
             relvar Greetings: table \"greetings\" { columns: { id: \"id\", message: \"message\" } };\n",
        )
        .expect("write greetings.cdstore");

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
        let (_dir, cd_path) = write_project(cd, None, None);
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
            let (_dir, cd_path) = write_project(cd, None, None);
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
}

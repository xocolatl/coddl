//! Catalog-rooted resolution for `coddl provision`.
//!
//! [`crate::discover_and_validate`] resolves from a `.cd` program entry
//! point. Provision has no program — it starts from a `.cddb` catalog
//! and reconciles a physical store to it. [`resolve_catalog`] is the
//! parallel entry point: given a `.cddb` path it reads + typechecks the
//! catalog and resolves every **base** relvar to its physical table,
//! column map, candidate keys, and INIT value.
//!
//! The physical binding is identity — table = relvar name, column =
//! attribute — and the backend + connection are transitional defaults
//! (SQLite, `<db>.sqlite`). TODO(cdstore-loader): resolve them by
//! querying the loaded `coddl::storage` relations instead of defaulting.
//!
//! Diagnostic reuse: PL0100 (unreadable catalog) and PL0020 (a `.cddb`
//! with no `database <name>;` header) are the only cross-file codes here.

use std::collections::HashMap;
use std::path::Path;

use coddl_diagnostics::{Diagnostic, FileId};
use coddl_syntax::ast::{AstNode, Expr};
use coddl_syntax::ast_cddb::{CddbItem, CddbRoot};
use coddl_syntax::FileKind;
use coddl_types::{check, Heading, RelvarKind};

use crate::plan::BackendKind;
use crate::{canonicalize_against, plain_error};

/// FileId assigned to the `.cddb` catalog in the catalog-rooted flow.
const CDDB_FILE: FileId = FileId(0);

/// The output of one catalog resolution: the [`CatalogPlan`] (when the
/// catalog was readable and named a database) plus every diagnostic
/// from the `.cddb` typecheck, the `.cdstore` parse, and cross-file
/// validation.
#[derive(Debug)]
pub struct CatalogPlanOutput {
    pub plan: Option<CatalogPlan>,
    pub diagnostics: Vec<Diagnostic>,
}

/// A `.cddb` catalog resolved against its sibling `.cdstore`: what
/// `coddl provision` needs to create + seed each physical table.
///
/// Carries the AST INIT node per relvar rather than folded rows — the
/// constant-expression → rows evaluation lives up in `coddl-provision`,
/// above the neutral backend seam. That AST node is an `Rc`-backed
/// rowan handle, so this type is `!Send`; provision is a synchronous
/// CLI pass, and the program flow's `Plan` stays `Send` unaffected.
#[derive(Debug)]
pub struct CatalogPlan {
    /// The database name from the `.cddb`'s `database <name>;` header.
    /// Chunk 8 uppercases this to build the `CODDL_<DBNAME>_FILE` env key.
    pub database_name: String,
    pub backend_kind: BackendKind,
    /// Default database file, canonicalized against the `.cdstore`'s
    /// directory. `None` for non-SQLite backends or an absent `file:`.
    /// Provision applies the `CODDL_<DBNAME>_FILE` override over this.
    pub db_file_default: Option<String>,
    /// Every **base** catalog relvar, name-sorted. Virtual relvars are
    /// omitted (no physical table).
    pub relvars: Vec<ResolvedCatalogRelvar>,
}

/// One base catalog relvar resolved to its physical form.
#[derive(Debug)]
pub struct ResolvedCatalogRelvar {
    /// The relvar name (catalog side == physical side for v1 identity).
    pub name: String,
    /// The canonical heading (resolved types, name-sorted).
    pub heading: Heading,
    /// Declared candidate keys — one inner `Vec` per key, in source
    /// order. Provision materializes the first as the table's PRIMARY KEY.
    pub keys: Vec<Vec<String>>,
    /// The physical SQL table name from the `.cdstore` binding.
    pub table_name: String,
    /// `(heading_attr, sql_column)` in heading-canonical (sorted) order.
    pub columns: Vec<(String, String)>,
    /// The INIT value — the RHS of `<Name> := <expr>;` in the `.cddb`,
    /// or `None` when the relvar declares no INIT. Carried as the typed
    /// AST expression; `coddl-provision` evaluates it to seed rows.
    pub init: Option<Expr>,
}

/// Resolve a `.cddb` catalog and its sibling `.cdstore` into a
/// [`CatalogPlan`]. Does file I/O but mutates nothing outside its
/// return value.
///
/// On a hard failure — an unreadable `.cddb` (PL0100), or a `.cddb`
/// with no `database <name>;` header (PL0020) — the plan is `None` and
/// the diagnostics say why. Otherwise a plan is returned even when some
/// relvars fail to resolve: the failures are diagnostics and the caller
/// gates on their severity.
pub fn resolve_catalog(cddb_path: &Path) -> CatalogPlanOutput {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // Read + typecheck the catalog. Unreadable → PL0100, no plan.
    let cddb_source = match std::fs::read_to_string(cddb_path) {
        Ok(s) => s,
        Err(_) => {
            diags.push(plain_error(
                "PL0100",
                format!("cannot read catalog: {}", cddb_path.display()),
            ));
            return CatalogPlanOutput {
                plan: None,
                diagnostics: diags,
            };
        }
    };
    let cddb_check = check(&cddb_source, CDDB_FILE, FileKind::Cddb);
    diags.extend(cddb_check.diagnostics.iter().cloned());
    let cddb_root = CddbRoot::cast(cddb_check.tree.clone());

    // The database name is the catalog's identity: it names the sibling
    // store and keys the runtime env override. Absent → PL0020, no plan.
    let Some(database_name) = cddb_root
        .as_ref()
        .and_then(|r| r.database())
        .and_then(|d| d.name().map(|t| t.text().to_string()))
    else {
        diags.push(plain_error(
            "PL0020",
            format!(
                "catalog `{}` has no `database <name>;` header, so its `.cdstore` cannot be located",
                cddb_path.display(),
            ),
        ));
        return CatalogPlanOutput {
            plan: None,
            diagnostics: diags,
        };
    };

    // Physical binding no longer comes from a `.cdstore`: table = relvar name
    // and column = attribute (identity — the mapping `coddl::storage`'s design
    // mandates), and backend + file are transitional defaults.
    // TODO(cdstore-loader): resolve backend + connection by querying the loaded
    // `coddl::storage` relations instead of defaulting.
    let backend_kind = BackendKind::Sqlite;
    let db_file_default = Some(canonicalize_against(
        cddb_path,
        &format!("{database_name}.sqlite"),
    ));

    // INIT values live in the `.cddb` as `<Name> := <expr>;` items. The
    // typechecker validated them (Chunk 3) but doesn't retain the RHS,
    // so pull the expression nodes straight from the CST. Duplicate
    // INITs already errored at check time (T0104) — first binding wins.
    let mut init_map: HashMap<String, Expr> = HashMap::new();
    if let Some(root) = &cddb_root {
        for item in root.items() {
            if let CddbItem::RelvarInit(rv) = item {
                if let (Some(name), Some(rhs)) = (rv.name(), rv.rhs()) {
                    init_map.entry(name.text().to_string()).or_insert(rhs);
                }
            }
        }
    }

    // Resolve each base relvar to its physical form, name-sorted for
    // deterministic output. Identity mapping: table = relvar name, column =
    // attribute (heading-canonical, name-sorted).
    let mut relvars: Vec<ResolvedCatalogRelvar> = Vec::new();
    let mut base_relvars: Vec<_> = cddb_check
        .relvars
        .iter()
        .filter(|(_, info)| info.kind == RelvarKind::Base)
        .collect();
    base_relvars.sort_by(|a, b| a.0.cmp(b.0));

    for (name, info) in base_relvars {
        let mut columns: Vec<(String, String)> = info
            .heading
            .attrs()
            .iter()
            .map(|(a, _)| (a.clone(), a.clone()))
            .collect();
        columns.sort_by(|a, b| a.0.cmp(&b.0));
        relvars.push(ResolvedCatalogRelvar {
            name: name.to_string(),
            heading: info.heading.clone(),
            keys: info.keys.clone(),
            table_name: name.to_string(),
            columns,
            init: init_map.get(name).cloned(),
        });
    }

    CatalogPlanOutput {
        plan: Some(CatalogPlan {
            database_name,
            backend_kind,
            db_file_default,
            relvars,
        }),
        diagnostics: diags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Write a `<db>.cddb` into a fresh tempdir and return its path plus the
    /// `.cddb` path. `resolve_catalog` no longer reads a `.cdstore` — the
    /// physical binding is identity and backend/file are defaults.
    fn write_catalog(db: &str, cddb: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let cddb_path = dir.path().join(format!("{db}.cddb"));
        fs::write(&cddb_path, cddb).unwrap();
        (dir, cddb_path)
    }

    /// A three-relvar suppliers-shaped catalog. `S`/`P` carry INIT
    /// values (P exercises `Rational`); `SP` deliberately has none.
    const CDDB_SP: &str = "\
database sp;
base relvar S { sno: Text, sname: Text, status: Integer, city: Text } key { sno };
base relvar P { pno: Text, weight: Rational } key { pno };
base relvar SP { sno: Text, pno: Text, qty: Integer } key { sno, pno };

S := Relation {
    { sno: \"S1\", sname: \"Smith\", status: 20, city: \"London\" },
    { sno: \"S2\", sname: \"Jones\", status: 10, city: \"Paris\" },
};
P := Relation {
    { pno: \"P1\", weight: 12.0 },
};
";

    fn has(diags: &[Diagnostic], code: &str) -> bool {
        diags.iter().any(|d| d.code == code)
    }

    fn pl_codes(diags: &[Diagnostic]) -> Vec<&'static str> {
        diags
            .iter()
            .map(|d| d.code)
            .filter(|c| c.starts_with("PL"))
            .collect()
    }

    #[test]
    fn clean_catalog_resolves() {
        let (_dir, cddb) = write_catalog("sp", CDDB_SP);
        let out = resolve_catalog(&cddb);

        let pl = pl_codes(&out.diagnostics);
        assert!(pl.is_empty(), "unexpected PL diagnostics: {pl:?}");

        let plan = out.plan.expect("plan");
        assert_eq!(plan.database_name, "sp");
        // Backend + file are transitional defaults (TODO cdstore-loader).
        assert_eq!(plan.backend_kind, BackendKind::Sqlite);
        assert!(
            plan.db_file_default
                .as_deref()
                .is_some_and(|p| p.ends_with("sp.sqlite")),
            "db_file_default = {:?}",
            plan.db_file_default,
        );

        // Base relvars only, name-sorted: P, S, SP.
        let names: Vec<&str> = plan.relvars.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["P", "S", "SP"]);

        let s = plan.relvars.iter().find(|r| r.name == "S").unwrap();
        // Identity mapping: table = relvar name, column = attribute.
        assert_eq!(s.table_name, "S");
        assert_eq!(s.keys, vec![vec!["sno".to_string()]]);
        let s_cols: Vec<(&str, &str)> = s
            .columns
            .iter()
            .map(|(a, c)| (a.as_str(), c.as_str()))
            .collect();
        assert_eq!(
            s_cols,
            vec![
                ("city", "city"),
                ("sname", "sname"),
                ("sno", "sno"),
                ("status", "status"),
            ]
        );
        assert!(s.init.is_some(), "S has an INIT value");

        let sp = plan.relvars.iter().find(|r| r.name == "SP").unwrap();
        assert_eq!(sp.table_name, "SP");
        assert_eq!(sp.keys, vec![vec!["sno".to_string(), "pno".to_string()]]);
        assert!(sp.init.is_none(), "SP declares no INIT");

        // The Rational-typed relvar still resolves and keeps its INIT.
        let p = plan.relvars.iter().find(|r| r.name == "P").unwrap();
        assert!(p.init.is_some());
    }

    #[test]
    fn missing_database_header_is_pl0020_no_plan() {
        let cddb = "base relvar S { sno: Text } key { sno };\n";
        let (_dir, path) = write_catalog("sp", cddb);
        let out = resolve_catalog(&path);
        assert!(has(&out.diagnostics, "PL0020"));
        assert!(out.plan.is_none());
    }

    #[test]
    fn unreadable_catalog_is_pl0100_no_plan() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.cddb");
        let out = resolve_catalog(&missing);
        assert!(has(&out.diagnostics, "PL0100"));
        assert!(out.plan.is_none());
    }
}

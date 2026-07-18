//! `coddl provision` — reconcile a database to the state its `.cddb` catalog
//! declares.
//!
//! This crate is the orchestration layer that sees *both* the relational middle
//! and the storage bottom, exactly as `coddl-runtime` does. It takes a resolved
//! [`CatalogPlan`](coddl_plan::CatalogPlan) (from `coddl-plan`), folds it into
//! the neutral `Schema` + `Row` shapes the SQLite executor consumes, and drives
//! [`coddl_backend_sqlite::provision`] — mapping the outcome to diagnostics.
//!
//! Two folds bridge the gap the self-hosting seam keeps open (the executor never
//! sees a `Heading`/`Type`/`Expr`):
//!
//! - **`Heading → Schema`** — each attribute's [`Type`] maps to a neutral
//!   [`ColKind`]; the first candidate key's attributes translate to their
//!   physical columns and name-sort into the `PRIMARY KEY`.
//! - **INIT `Expr → Row`s** — each `S := Relation { … };` cell is a constant
//!   expression, *evaluated* (not merely decoded) by the shared constant-folder
//!   [`coddl_consteval::fold_const_scalar`] — the very folder `coddl-procir`
//!   uses for module `let`s — then converted to a storage [`Value`].
//!
//! Every fold-time failure is a diagnostic in the `PV####` namespace and leaves
//! the database untouched; only a fully-folded catalog reaches the one-transaction
//! executor. Nothing here is committed unless the whole reconcile succeeds.

use std::collections::HashMap;
use std::path::Path;

use coddl_backend_sqlite::{provision, ProvisionError, ProvisionTable, SchemaDiff};
use coddl_diagnostics::{Diagnostic, FileId, Severity, Span};
use coddl_plan::{resolve_catalog, BackendKind, ResolvedCatalogRelvar};
use coddl_relir::Literal;
use coddl_sqlemit::{ColKind, Column, Row, Schema, Value};
use coddl_syntax::ast::Expr;

pub use coddl_backend_sqlite::{Report, TableReport};

/// The `.cddb` catalog is `FileId(0)` in the catalog-rooted flow (its sibling
/// `.cdstore` is `FileId(1)`) — the same numbering `coddl_plan::resolve_catalog`
/// uses, so INIT spans point back into the catalog source.
const CDDB_FILE: FileId = FileId(0);

/// The result of one provisioning run: the executor's [`Report`] when the whole
/// reconcile succeeded, plus every diagnostic (resolution, folding, execution).
/// A caller (the `coddl provision` CLI) prints the diagnostics and picks an exit
/// code from their severities.
#[derive(Debug)]
pub struct ProvisionOutcome {
    pub report: Option<Report>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Provision the database declared by `cddb_path`: resolve the catalog, fold each
/// base relvar to a schema + seed rows, resolve the target file the same way the
/// runtime does, and reconcile in one transaction. The database is touched only
/// if resolution and every fold succeed.
pub fn provision_catalog(cddb_path: &Path) -> ProvisionOutcome {
    let out = resolve_catalog(cddb_path);
    let mut diags = out.diagnostics;

    // A catalog that doesn't typecheck (or couldn't be resolved) never reaches
    // the database — folding a broken catalog would be meaningless.
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return ProvisionOutcome {
            report: None,
            diagnostics: diags,
        };
    }
    let Some(plan) = out.plan else {
        return ProvisionOutcome {
            report: None,
            diagnostics: diags,
        };
    };

    if plan.backend_kind != BackendKind::Sqlite {
        diags.push(plain(
            "PV0007",
            format!(
                "database `{}` uses backend {:?}; `coddl provision` supports SQLite only in v1",
                plan.database_name, plan.backend_kind
            ),
        ));
        return ProvisionOutcome {
            report: None,
            diagnostics: diags,
        };
    }

    // Fold every base relvar (name-sorted by `resolve_catalog`). All fold-time
    // validation is pre-SQL: a failure aborts before the database is opened.
    let mut tables: Vec<ProvisionTable> = Vec::with_capacity(plan.relvars.len());
    let mut fold_ok = true;
    for relvar in &plan.relvars {
        let Some(schema) = build_schema(relvar, &mut diags) else {
            fold_ok = false;
            continue;
        };
        let Some(rows) = build_rows(relvar, &schema, &mut diags) else {
            fold_ok = false;
            continue;
        };
        tables.push(ProvisionTable { schema, rows });
    }
    if !fold_ok {
        return ProvisionOutcome {
            report: None,
            diagnostics: diags,
        };
    }

    // Resolve the target file exactly as the compiled runtime will: the
    // `CODDL_<DBNAME>_FILE` env override, else the `.cdstore` baked default.
    let env_key = format!("CODDL_{}_FILE", plan.database_name.to_ascii_uppercase());
    let Some(path) = std::env::var(&env_key)
        .ok()
        .or_else(|| plan.db_file_default.clone())
    else {
        diags.push(plain(
            "PV0006",
            format!(
                "cannot resolve a database file for `{}`: neither `{}` is set nor does the \
                 `.cdstore` declare a `file:` default",
                plan.database_name, env_key
            ),
        ));
        return ProvisionOutcome {
            report: None,
            diagnostics: diags,
        };
    };

    match provision(&path, &tables) {
        Ok(report) => ProvisionOutcome {
            report: Some(report),
            diagnostics: diags,
        },
        Err(e) => {
            push_provision_error(&mut diags, e);
            ProvisionOutcome {
                report: None,
                diagnostics: diags,
            }
        }
    }
}

/// Fold a relvar's heading + key into a neutral [`Schema`]. Columns follow the
/// heading-canonical (attribute-name-sorted) order `resolve_catalog` supplies;
/// the PRIMARY KEY is the first candidate key's columns, name-sorted.
fn build_schema(relvar: &ResolvedCatalogRelvar, diags: &mut Vec<Diagnostic>) -> Option<Schema> {
    let mut columns: Vec<Column> = Vec::with_capacity(relvar.columns.len());
    let mut ok = true;

    for (attr, sql_col) in &relvar.columns {
        match relvar.heading.lookup(attr).and_then(type_to_colkind) {
            Some(kind) => columns.push(Column {
                name: sql_col.clone(),
                // Conceptual totality (RM Pro 4). `emit_ddl` applies the one
                // physical exception (the Approximate NaN channel).
                not_null: true,
                kind,
            }),
            None => {
                let ty = relvar.heading.lookup(attr);
                diags.push(plain(
                    "PV0001",
                    format!(
                        "relvar `{}`: attribute `{}` has type `{}`, which has no provisionable \
                         column type (only Integer, Text, Boolean, Rational, Approximate, and \
                         Character can be seeded)",
                        relvar.name,
                        attr,
                        ty.map(|t| format!("{t:?}"))
                            .unwrap_or_else(|| "?".to_string()),
                    ),
                ));
                ok = false;
            }
        }
    }

    // Key attributes → physical columns, name-sorted (a key is a set — RM Pro 1).
    let colmap: HashMap<&str, &str> = relvar
        .columns
        .iter()
        .map(|(a, c)| (a.as_str(), c.as_str()))
        .collect();
    let pk = match relvar.keys.first() {
        Some(key) => {
            let mut cols: Vec<String> = Vec::with_capacity(key.len());
            for attr in key {
                match colmap.get(attr.as_str()) {
                    Some(c) => cols.push((*c).to_string()),
                    None => {
                        // Defensive: a resolved base relvar's key attributes are
                        // always bound columns.
                        diags.push(plain(
                            "PV0001",
                            format!(
                                "relvar `{}`: key attribute `{}` has no column binding",
                                relvar.name, attr
                            ),
                        ));
                        ok = false;
                    }
                }
            }
            cols.sort();
            cols
        }
        None => {
            diags.push(plain(
                "PV0001",
                format!(
                    "relvar `{}` has no candidate key to materialize as a PRIMARY KEY",
                    relvar.name
                ),
            ));
            ok = false;
            Vec::new()
        }
    };

    ok.then(|| Schema {
        table: relvar.table_name.clone(),
        columns,
        pk,
    })
}

/// Evaluate a relvar's INIT relation literal into seed [`Row`]s, in
/// `schema.columns` (heading-canonical) order. Deduplicates exact-duplicate
/// tuples (a relation is a set — RM Pro 2) and rejects two tuples that share a
/// key but differ in a non-key attribute. Returns an empty vec when the relvar
/// declares no INIT (it truncates to empty). `None` on any fold error.
fn build_rows(
    relvar: &ResolvedCatalogRelvar,
    schema: &Schema,
    diags: &mut Vec<Diagnostic>,
) -> Option<Vec<Row>> {
    let Some(init) = &relvar.init else {
        return Some(Vec::new());
    };
    let Expr::RelationLit(rel) = init else {
        diags.push(Diagnostic::error(
            span_of(init),
            "PV0002",
            format!(
                "relvar `{}`: INIT value must be a relation literal `Relation {{ … }}` to be \
                 seeded; other constant relation forms are not yet evaluable",
                relvar.name
            ),
        ));
        return None;
    };

    // Each built row is carried with its source-tuple span, so a later
    // key-collision points at the offending tuple.
    let mut built: Vec<(Row, Span)> = Vec::new();
    let mut ok = true;

    for element in rel.elements() {
        let tuple_span = span_of(&element);
        let Expr::TupleLit(tuple) = &element else {
            diags.push(Diagnostic::error(
                tuple_span,
                "PV0002",
                format!(
                    "relvar `{}`: each INIT element must be a tuple literal",
                    relvar.name
                ),
            ));
            ok = false;
            continue;
        };

        let fields: HashMap<String, Expr> = tuple
            .fields()
            .filter_map(|f| Some((f.name()?.text().to_string(), f.value()?)))
            .collect();

        // Build the row in schema/heading order; each column carries its
        // attribute name (relvar.columns) for the tuple-field lookup.
        let mut row: Row = Vec::with_capacity(schema.columns.len());
        let mut row_ok = true;
        for ((attr, _), col) in relvar.columns.iter().zip(&schema.columns) {
            let Some(value_expr) = fields.get(attr) else {
                // The typechecker (T0106) guarantees the heading matches; this
                // is a defensive guard, not an expected path.
                diags.push(Diagnostic::error(
                    tuple_span,
                    "PV0003",
                    format!(
                        "relvar `{}`: INIT tuple is missing attribute `{}`",
                        relvar.name, attr
                    ),
                ));
                row_ok = false;
                continue;
            };
            match coddl_consteval::fold_const_scalar(value_expr, &|_| None) {
                Ok(Some(lit)) => match coerce(lit, col.kind) {
                    Some(v) => row.push(v),
                    None => {
                        diags.push(Diagnostic::error(
                            span_of(value_expr),
                            "PV0003",
                            format!(
                                "relvar `{}`: INIT value for `{}` does not match its column type",
                                relvar.name, attr
                            ),
                        ));
                        row_ok = false;
                    }
                },
                Ok(None) => {
                    diags.push(Diagnostic::error(
                        span_of(value_expr),
                        "PV0003",
                        format!(
                            "relvar `{}`: INIT value for `{}` is not a constant scalar; only \
                             constant expressions can be seeded",
                            relvar.name, attr
                        ),
                    ));
                    row_ok = false;
                }
                Err(msg) => {
                    diags.push(Diagnostic::error(
                        span_of(value_expr),
                        "PV0004",
                        format!(
                            "relvar `{}`: evaluating the INIT value for `{}` failed: {}",
                            relvar.name, attr, msg
                        ),
                    ));
                    row_ok = false;
                }
            }
        }

        if row_ok {
            built.push((row, tuple_span));
        } else {
            ok = false;
        }
    }
    if !ok {
        return None;
    }

    // Dedup + key-uniqueness. Key positions are the row indices of the PK columns.
    let key_positions: Vec<usize> = schema
        .pk
        .iter()
        .filter_map(|k| schema.columns.iter().position(|c| &c.name == k))
        .collect();

    let mut seen: HashMap<Vec<Value>, Row> = HashMap::new();
    let mut rows: Vec<Row> = Vec::new();
    for (row, span) in built {
        let key: Vec<Value> = key_positions.iter().map(|&i| row[i].clone()).collect();
        match seen.get(&key) {
            // Exact-duplicate tuple: a relation is a set, so silently coalesce.
            Some(prev) if *prev == row => {}
            Some(_) => {
                diags.push(Diagnostic::error(
                    span,
                    "PV0005",
                    format!(
                        "relvar `{}`: two INIT tuples share the key {} but differ in a non-key \
                         attribute",
                        relvar.name,
                        render_key(&schema.pk, &key),
                    ),
                ));
                ok = false;
            }
            None => {
                seen.insert(key, row.clone());
                rows.push(row);
            }
        }
    }

    ok.then_some(rows)
}

/// Map a scalar [`Type`](coddl_types::Type) to its neutral [`ColKind`], or `None`
/// for a type that can't back a seedable column (`Binary`/`Byte` are
/// literal-less; tuples/relations/sequences/user scalars aren't flat columns).
fn type_to_colkind(ty: &coddl_types::Type) -> Option<ColKind> {
    use coddl_types::Type;
    match ty {
        Type::Integer => Some(ColKind::Integer),
        Type::Text => Some(ColKind::Text),
        Type::Boolean => Some(ColKind::Boolean),
        Type::Rational => Some(ColKind::Rational),
        Type::Approximate => Some(ColKind::Approximate),
        Type::Character => Some(ColKind::Character),
        _ => None,
    }
}

/// Convert a folded scalar [`Literal`] to a storage [`Value`] for its column,
/// widening an `Integer` literal to `n/1` in a `Rational` column (the Chunk-3
/// INIT tolerance). `None` on a kind/literal mismatch the typechecker should
/// have already rejected.
fn coerce(lit: Literal, kind: ColKind) -> Option<Value> {
    match (kind, lit) {
        (ColKind::Rational, Literal::Integer(n)) => Some(Value::Rational(n, 1)),
        (ColKind::Integer, Literal::Integer(n)) => Some(Value::Integer(n)),
        (ColKind::Text, Literal::Text(s)) => Some(Value::Text(s)),
        (ColKind::Boolean, Literal::Boolean(b)) => Some(Value::Boolean(b)),
        (ColKind::Rational, Literal::Rational(n, d)) => Some(Value::Rational(n, d)),
        (ColKind::Approximate, Literal::Approximate(bits)) => Some(Value::Approximate(bits)),
        (ColKind::Character, Literal::Character(cp)) => Some(Value::Character(cp)),
        _ => None,
    }
}

/// Turn an executor [`ProvisionError`] into one or more diagnostics. A schema
/// mismatch attaches a `with_related` note per differing column / key.
fn push_provision_error(diags: &mut Vec<Diagnostic>, e: ProvisionError) {
    match e {
        ProvisionError::Open(msg) => {
            diags.push(plain("PV0010", format!("cannot open database: {msg}")))
        }
        ProvisionError::Sql(msg) => diags.push(plain(
            "PV0010",
            format!("database error during provisioning: {msg}"),
        )),
        ProvisionError::NotATable { table, actual_type } => diags.push(plain(
            "PV0009",
            format!(
                "`{table}` exists as a {actual_type}, not a table; provision never alters foreign \
                 objects"
            ),
        )),
        ProvisionError::SchemaMismatch { table, diff } => {
            let d = plain(
                "PV0008",
                format!(
                    "table `{table}` does not match the catalog; provision does not migrate — \
                     reconcile or drop it by hand"
                ),
            );
            diags.push(attach_diff_notes(d, &diff));
        }
    }
}

fn attach_diff_notes(mut d: Diagnostic, diff: &SchemaDiff) -> Diagnostic {
    let sp = Span::synthetic(CDDB_FILE);
    for c in &diff.missing_columns {
        d = d.with_related(sp, format!("column `{}` is declared but absent", c.name));
    }
    for c in &diff.extra_columns {
        d = d.with_related(sp, format!("column `{c}` is in the table but not declared"));
    }
    for (col, exp, act) in &diff.kind_mismatches {
        d = d.with_related(
            sp,
            format!("column `{col}`: catalog expects `{exp}`, table has `{act}`"),
        );
    }
    for (col, exp, act) in &diff.not_null_mismatches {
        d = d.with_related(
            sp,
            format!("column `{col}`: NOT NULL expected {exp}, table has {act}"),
        );
    }
    if let Some((exp, act)) = &diff.pk_mismatch {
        d = d.with_related(
            sp,
            format!("primary key: catalog expects {exp:?}, table has {act:?}"),
        );
    }
    d
}

/// A diagnostic with no meaningful byte range (execution / resolution errors).
fn plain(code: &'static str, message: String) -> Diagnostic {
    Diagnostic::error(Span::synthetic(CDDB_FILE), code, message)
}

/// The catalog-file span of an AST node, for INIT-cell diagnostics.
fn span_of(expr: &Expr) -> Span {
    let r = expr.syntax().text_range();
    Span::new(CDDB_FILE, r.start().into(), r.end().into())
}

/// Render a key tuple as `col = value, …` for a collision diagnostic.
fn render_key(pk: &[String], key: &[Value]) -> String {
    pk.iter()
        .zip(key)
        .map(|(name, v)| format!("{name} = {}", render_value(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_value(v: &Value) -> String {
    match v {
        Value::Integer(n) => n.to_string(),
        Value::Text(s) => format!("{s:?}"),
        Value::Boolean(b) => b.to_string(),
        Value::Rational(n, d) => format!("{n}/{d}"),
        Value::Approximate(bits) => f64::from_bits(*bits).to_string(),
        Value::Character(cp) => char::from_u32(*cp)
            .map(|c| format!("{c:?}"))
            .unwrap_or_else(|| format!("U+{cp:04X}")),
    }
}

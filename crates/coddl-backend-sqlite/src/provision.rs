//! Provision executor.
//!
//! [`provision`] reconciles a SQLite database to the state a catalog declares,
//! in one transaction: create-or-verify each base table, then truncate +
//! replenish it to its INIT rows. It is **not** a migrator — a table that exists
//! but doesn't match its declared [`Schema`] is a rollback + error, never a
//! drop-recreate (that destructive/evolving story is the future `migrate`
//! command, which reuses [`diff_table`]'s neutral [`SchemaDiff`]).
//!
//! This layer sits on the permanent-Rust side of the self-hosting seam
//! (`docs/principles.md`): it takes ONLY the neutral `coddl-sqlemit` vocabulary
//! (`Schema` / `Column` / `ColKind` / `Value` / `Row`) plus its own report and
//! error types — never a `coddl-types` `Heading` or a `coddl-plan`
//! `CatalogPlan`. The `Type → ColKind` and `RelationLit → Row` folds live up in
//! `coddl-provision` (the crate that sees both the relational middle and this
//! storage bottom); this executor only ever sees a finished `Schema` + its rows.
//!
//! See `docs/storage.md` ("Provision executor and schema diff").

use std::collections::{BTreeMap, BTreeSet};

use coddl_sqlemit::{quote_ident, Backend, ColKind, Column, Row, Schema};
use rusqlite::{params_from_iter, Connection, OpenFlags, OptionalExtension};

use super::{value_to_sqlite, SqliteBackend, SQLITE_TYPE_MAP};

/// Conservative ceiling on bind variables per INSERT statement — under SQLite's
/// historical `SQLITE_MAX_VARIABLE_NUMBER` floor (999). Seed batches are sized so
/// `rows × arity` stays below it. Mirrors the runtime's write path.
const INSERT_PARAM_BUDGET: usize = 900;

/// One relvar's physical schema paired with its INIT rows. The row cells are
/// **positional** in the schema's heading-canonical (name-sorted) column order —
/// the same order `schema.columns` is in — so cell `i` binds to
/// `schema.columns[i]`. The `coddl-provision` fold (Chunk 8) is what guarantees
/// that alignment; this executor trusts it and length-checks each row.
pub struct ProvisionTable {
    pub schema: Schema,
    pub rows: Vec<Row>,
}

/// What a [`provision`] run did, table by table, in the order supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub tables: Vec<TableReport>,
}

/// The outcome for one reconciled table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableReport {
    pub table: String,
    /// `true` if this run issued the `CREATE TABLE`; `false` if it verified an
    /// already-matching table.
    pub created: bool,
    /// The number of INIT rows re-seeded (after truncation).
    pub rows_inserted: usize,
}

/// A neutral, policy-free description of how an existing table differs from a
/// declared [`Schema`]. [`provision`]'s policy is "any non-empty diff ⇒ rollback
/// + error"; the future `migrate` will consume the same value to emit ALTERs.
///
/// v1 records only the `PRAGMA table_info`-visible surface — see the
/// Boolean/Integer blind spot on [`diff_table`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaDiff {
    /// Declared columns absent from the table.
    pub missing_columns: Vec<Column>,
    /// Column names present in the table but not declared.
    pub extra_columns: Vec<String>,
    /// `(column, expected_sql_type, actual_sql_type)` for a declared-vs-actual
    /// type-keyword mismatch on a shared column.
    pub kind_mismatches: Vec<(String, String, String)>,
    /// `(column, expected_not_null, actual_not_null)` for a NOT NULL mismatch on
    /// a shared column.
    pub not_null_mismatches: Vec<(String, bool, bool)>,
    /// `(expected_pk, actual_pk)`, each name-sorted, when the key column *sets*
    /// differ (a key is a set — compared order-independently).
    pub pk_mismatch: Option<(Vec<String>, Vec<String>)>,
}

impl SchemaDiff {
    /// A table matches its declared schema iff the diff is empty.
    pub fn is_empty(&self) -> bool {
        self.missing_columns.is_empty()
            && self.extra_columns.is_empty()
            && self.kind_mismatches.is_empty()
            && self.not_null_mismatches.is_empty()
            && self.pk_mismatch.is_none()
    }
}

/// Why a [`provision`] run failed. Every variant leaves the database unchanged:
/// an `Open` error never began a transaction, and every later error triggers a
/// ROLLBACK before returning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionError {
    /// The database file could not be opened (read-write + create).
    Open(String),
    /// A statement (introspection, DDL, DELETE, or INSERT) failed.
    Sql(String),
    /// A managed table exists but does not match its declared [`Schema`]. The
    /// diff is boxed so a mismatch (the rare, large variant) doesn't widen every
    /// `Result<_, ProvisionError>` the executor threads by value.
    SchemaMismatch {
        table: String,
        diff: Box<SchemaDiff>,
    },
    /// A managed name resolves to a non-table object (view, index, …). Provision
    /// never touches foreign objects, so this is an error, never a drop.
    NotATable { table: String, actual_type: String },
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvisionError::Open(e) => write!(f, "cannot open database: {e}"),
            ProvisionError::Sql(e) => write!(f, "SQL error: {e}"),
            ProvisionError::SchemaMismatch { table, .. } => {
                write!(f, "table `{table}` does not match its declared schema")
            }
            ProvisionError::NotATable { table, actual_type } => {
                write!(f, "`{table}` is a {actual_type}, not a table")
            }
        }
    }
}

impl std::error::Error for ProvisionError {}

/// Reconcile the SQLite database at `db_path` to the state `tables` declares, in
/// one transaction. `tables` should be name-sorted by the caller
/// (`coddl-provision`) for deterministic output; this executor processes them in
/// the order given. See the module docs / `docs/storage.md` for the algorithm.
pub fn provision(db_path: &str, tables: &[ProvisionTable]) -> Result<Report, ProvisionError> {
    // Read-write, creating the file if absent (unlike the read path's read-only
    // open). NO_MUTEX: provision owns this connection for the whole reconcile.
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| ProvisionError::Open(e.to_string()))?;

    conn.execute_batch("BEGIN")
        .map_err(|e| ProvisionError::Sql(e.to_string()))?;

    // SQLite's DDL + DML are transactional and `sqlite_master`/`PRAGMA` reads
    // don't force an implicit commit, so the whole reconcile — CREATE + DELETE +
    // INSERT — is atomic under this one BEGIN. On any error we ROLLBACK (leaving
    // the database byte-identical) and surface the original error.
    match reconcile(&conn, tables) {
        Ok(report) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| ProvisionError::Sql(e.to_string()))?;
            Ok(report)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// The two-pass reconcile, run inside the caller's open transaction.
fn reconcile(conn: &Connection, tables: &[ProvisionTable]) -> Result<Report, ProvisionError> {
    // Pass 1 — reconcile each table's schema: create if absent, verify if
    // present, reject a non-table collision.
    let mut created: Vec<bool> = Vec::with_capacity(tables.len());
    for t in tables {
        match object_type(conn, &t.schema.table)? {
            None => {
                for stmt in SqliteBackend.emit_ddl(&t.schema) {
                    conn.execute(&stmt.text, [])
                        .map_err(|e| ProvisionError::Sql(e.to_string()))?;
                }
                created.push(true);
            }
            Some(ref ty) if ty == "table" => {
                let diff = diff_table(conn, &t.schema)?;
                if !diff.is_empty() {
                    return Err(ProvisionError::SchemaMismatch {
                        table: t.schema.table.clone(),
                        diff: Box::new(diff),
                    });
                }
                created.push(false);
            }
            Some(other) => {
                return Err(ProvisionError::NotATable {
                    table: t.schema.table.clone(),
                    actual_type: other,
                });
            }
        }
    }

    // Pass 2 — truncate + replenish each table to its INIT rows. No FK-ordering
    // hazard in v1 (the `.cddb` declares no foreign keys); if FKs arrive this
    // must become an all-tables-DELETE pass (child→parent) before any INSERT
    // pass (parent→child), since a per-table truncate+fill would trip a
    // cross-table constraint.
    let mut reports = Vec::with_capacity(tables.len());
    for (t, created) in tables.iter().zip(created) {
        let rows_inserted = replenish(conn, t)?;
        reports.push(TableReport {
            table: t.schema.table.clone(),
            created,
            rows_inserted,
        });
    }

    Ok(Report { tables: reports })
}

/// The `sqlite_master.type` of the object named `name` (`"table"`, `"view"`,
/// `"index"`, `"trigger"`), or `None` if no such object exists.
fn object_type(conn: &Connection, name: &str) -> Result<Option<String>, ProvisionError> {
    conn.query_row(
        "SELECT type FROM sqlite_master WHERE name = ?1",
        [name],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|e| ProvisionError::Sql(e.to_string()))
}

/// Compare an existing table against a declared [`Schema`], returning a neutral
/// [`SchemaDiff`] (empty ⇔ they match). Policy-free: it never decides what to do
/// about a difference — that's the caller's job (`provision` rolls back;
/// `migrate` will ALTER). The comparison oracle is exactly what `emit_ddl`
/// produced: the declared SQL type keyword (`type_map().sql_type`), the NOT NULL
/// rule (every total column except `Approximate`), and the key column set.
///
/// **v1 blind spot:** `PRAGMA table_info` cannot see a `CHECK` constraint, so a
/// `Boolean` column (`INTEGER CHECK (c IN (0, 1))`) is indistinguishable from an
/// `Integer` column — both report declared type `INTEGER`, `notnull = 1`. Such a
/// pair diffs *clean*. Accepted for v1; a `sqlite_master.sql`-based comparison is
/// deferred to `migrate`.
pub fn diff_table(conn: &Connection, schema: &Schema) -> Result<SchemaDiff, ProvisionError> {
    // The table name can't be a bound parameter, so quote-interpolate it (values
    // elsewhere are always bound, never interpolated).
    let sql = format!("PRAGMA table_info({})", quote_ident(&schema.table));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ProvisionError::Sql(e.to_string()))?;
    // Per column: (name, declared type, notnull flag, pk ordinal — 0 = not a key
    // column, else its 1-based position within the PK).
    let actual: Vec<(String, String, bool, i64)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|e| ProvisionError::Sql(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| ProvisionError::Sql(e.to_string()))?;

    let mut diff = SchemaDiff::default();

    let actual_by_name: BTreeMap<&str, (&str, bool)> = actual
        .iter()
        .map(|(name, ty, nn, _pk)| (name.as_str(), (ty.as_str(), *nn)))
        .collect();
    let declared_names: BTreeSet<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();

    // Declared columns: missing, or type / NOT NULL mismatched.
    for col in &schema.columns {
        match actual_by_name.get(col.name.as_str()) {
            None => diff.missing_columns.push(col.clone()),
            Some(&(actual_ty, actual_nn)) => {
                let expected_ty = SQLITE_TYPE_MAP.sql_type(col.kind);
                if actual_ty != expected_ty {
                    diff.kind_mismatches.push((
                        col.name.clone(),
                        expected_ty.to_string(),
                        actual_ty.to_string(),
                    ));
                }
                let expected_nn = col.not_null && col.kind != ColKind::Approximate;
                if actual_nn != expected_nn {
                    diff.not_null_mismatches
                        .push((col.name.clone(), expected_nn, actual_nn));
                }
            }
        }
    }

    // Table columns not in the schema.
    for (name, _ty, _nn, _pk) in &actual {
        if !declared_names.contains(name.as_str()) {
            diff.extra_columns.push(name.clone());
        }
    }

    // Key column *sets*, order-independent (BTreeSet iteration is name-sorted).
    let actual_pk: BTreeSet<&str> = actual
        .iter()
        .filter(|(_, _, _, pk)| *pk != 0)
        .map(|(name, _, _, _)| name.as_str())
        .collect();
    let expected_pk: BTreeSet<&str> = schema.pk.iter().map(|s| s.as_str()).collect();
    if actual_pk != expected_pk {
        diff.pk_mismatch = Some((
            expected_pk.iter().map(|s| s.to_string()).collect(),
            actual_pk.iter().map(|s| s.to_string()).collect(),
        ));
    }

    Ok(diff)
}

/// Truncate then re-seed one table to its INIT rows, returning the row count.
/// Cells bind through the shared `value_to_sqlite`, and inserts batch under
/// [`INSERT_PARAM_BUDGET`] so a wide relation never blows SQLite's bind-variable
/// ceiling.
fn replenish(conn: &Connection, table: &ProvisionTable) -> Result<usize, ProvisionError> {
    let schema = &table.schema;
    let quoted_table = quote_ident(&schema.table);

    conn.execute(&format!("DELETE FROM {quoted_table}"), [])
        .map_err(|e| ProvisionError::Sql(e.to_string()))?;

    if table.rows.is_empty() {
        return Ok(0);
    }

    // A storage-backed base relvar always has ≥1 column (its non-empty key lives
    // among them), so arity ≥ 1 here — the division and `chunks` below are safe.
    let arity = schema.columns.len();
    debug_assert!(arity >= 1, "a base relvar has at least its key columns");

    // Flatten rows to one bind vector in column order, validating arity.
    let mut cells: Vec<rusqlite::types::Value> = Vec::with_capacity(table.rows.len() * arity);
    for row in &table.rows {
        if row.len() != arity {
            return Err(ProvisionError::Sql(format!(
                "seed row for `{}` has {} cells, expected {arity}",
                schema.table,
                row.len()
            )));
        }
        cells.extend(row.iter().map(value_to_sqlite));
    }

    let col_list = schema
        .columns
        .iter()
        .map(|c| quote_ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ");

    let batch_rows = (INSERT_PARAM_BUDGET / arity).max(1);
    for batch in cells.chunks(batch_rows * arity) {
        let n_groups = batch.len() / arity;
        let sql = format!(
            "INSERT INTO {quoted_table} ({col_list}) VALUES {}",
            values_groups(arity, n_groups)
        );
        let mut stmt = conn
            .prepare_cached(&sql)
            .map_err(|e| ProvisionError::Sql(e.to_string()))?;
        stmt.execute(params_from_iter(batch.iter()))
            .map_err(|e| ProvisionError::Sql(e.to_string()))?;
    }

    Ok(table.rows.len())
}

/// Render `n_groups` numbered `(?1, ?2, …)` VALUES row-groups of `arity` cells
/// each, numbering from 1. A local twin of the runtime's helper; a seed INSERT
/// has no compile-time scalar placeholders to compose with, so the base is 0.
fn values_groups(arity: usize, n_groups: usize) -> String {
    let mut idx = 0usize;
    let mut groups = Vec::with_capacity(n_groups);
    for _ in 0..n_groups {
        let cells: Vec<String> = (0..arity)
            .map(|_| {
                idx += 1;
                format!("?{idx}")
            })
            .collect();
        groups.push(format!("({})", cells.join(", ")));
    }
    groups.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_sqlemit::Value;

    fn col(name: &str, kind: ColKind) -> Column {
        Column {
            name: name.to_string(),
            kind,
            not_null: true,
        }
    }

    // The classic suppliers relvar: single Text key, columns heading-sorted.
    fn schema_s() -> Schema {
        Schema {
            table: "s".to_string(),
            columns: vec![
                col("city", ColKind::Text),
                col("sname", ColKind::Text),
                col("sno", ColKind::Text),
                col("status", ColKind::Integer),
            ],
            pk: vec!["sno".to_string()],
        }
    }

    // A supplier row, cells in heading-sorted order (city, sname, sno, status).
    fn supplier(city: &str, sname: &str, sno: &str, status: i64) -> Row {
        vec![
            Value::Text(city.to_string()),
            Value::Text(sname.to_string()),
            Value::Text(sno.to_string()),
            Value::Integer(status),
        ]
    }

    fn tmp_db() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite").to_string_lossy().into_owned();
        (dir, path)
    }

    #[test]
    fn creates_and_seeds_tables_from_scratch() {
        let (_dir, path) = tmp_db();
        let s = ProvisionTable {
            schema: schema_s(),
            rows: vec![
                supplier("London", "Smith", "S1", 20),
                supplier("Paris", "Jones", "S2", 10),
            ],
        };

        let report = provision(&path, &[s]).unwrap();
        assert_eq!(
            report.tables,
            vec![TableReport {
                table: "s".to_string(),
                created: true,
                rows_inserted: 2,
            }]
        );

        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM s", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let city: String = conn
            .query_row("SELECT city FROM s WHERE sno = 'S1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(city, "London");
    }

    #[test]
    fn re_provision_is_idempotent() {
        let (_dir, path) = tmp_db();
        let make = || ProvisionTable {
            schema: schema_s(),
            rows: vec![supplier("London", "Smith", "S1", 20)],
        };

        let first = provision(&path, &[make()]).unwrap();
        assert!(first.tables[0].created);

        // Second run verifies the existing table (created = false) and re-seeds
        // via truncate + refill, so the row count stays 1, not doubled.
        let second = provision(&path, &[make()]).unwrap();
        assert!(!second.tables[0].created);
        assert_eq!(second.tables[0].rows_inserted, 1);

        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM s", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn mismatch_rolls_back_leaving_bytes_identical() {
        let (_dir, path) = tmp_db();
        // Pre-seed `s` with `status` as TEXT — the schema declares it INTEGER.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"CREATE TABLE "s" ("city" TEXT NOT NULL, "sname" TEXT NOT NULL, "sno" TEXT NOT NULL, "status" TEXT NOT NULL, PRIMARY KEY ("sno"));
                   INSERT INTO "s" VALUES ('London', 'Smith', 'S1', 'twenty');"#,
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        let t = ProvisionTable {
            schema: schema_s(),
            rows: vec![supplier("Paris", "Jones", "S2", 10)],
        };
        match provision(&path, &[t]).unwrap_err() {
            ProvisionError::SchemaMismatch { table, diff } => {
                assert_eq!(table, "s");
                assert_eq!(
                    diff.kind_mismatches,
                    vec![(
                        "status".to_string(),
                        "INTEGER".to_string(),
                        "TEXT".to_string()
                    )]
                );
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "a rolled-back provision must leave the DB byte-identical"
        );
    }

    #[test]
    fn view_named_like_a_table_is_an_error_and_untouched() {
        let (_dir, path) = tmp_db();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"CREATE TABLE backing (x INTEGER);
                   CREATE VIEW "s" AS SELECT x FROM backing;"#,
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        let t = ProvisionTable {
            schema: schema_s(),
            rows: vec![],
        };
        match provision(&path, &[t]).unwrap_err() {
            ProvisionError::NotATable { table, actual_type } => {
                assert_eq!(table, "s");
                assert_eq!(actual_type, "view");
            }
            other => panic!("expected NotATable, got {other:?}"),
        }

        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "a name collision must never drop the object");
    }

    #[test]
    fn table_with_no_init_rows_is_created_empty() {
        let (_dir, path) = tmp_db();
        let t = ProvisionTable {
            schema: schema_s(),
            rows: vec![],
        };
        let report = provision(&path, &[t]).unwrap();
        assert_eq!(
            report.tables,
            vec![TableReport {
                table: "s".to_string(),
                created: true,
                rows_inserted: 0,
            }]
        );

        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM s", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn batched_insert_crosses_param_budget() {
        let (_dir, path) = tmp_db();
        // 1000 rows × arity 1 exceeds INSERT_PARAM_BUDGET (900), forcing >1 batch.
        let schema = Schema {
            table: "n".to_string(),
            columns: vec![col("id", ColKind::Integer)],
            pk: vec!["id".to_string()],
        };
        let rows: Vec<Row> = (0..1000).map(|i| vec![Value::Integer(i)]).collect();
        let report = provision(&path, &[ProvisionTable { schema, rows }]).unwrap();
        assert_eq!(report.tables[0].rows_inserted, 1000);

        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM n", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1000);
        let max: i64 = conn
            .query_row("SELECT MAX(id) FROM n", [], |r| r.get(0))
            .unwrap();
        assert_eq!(max, 999);
    }

    #[test]
    fn diff_table_detects_each_difference() {
        let (_dir, path) = tmp_db();
        let conn = Connection::open(&path).unwrap();
        for stmt in SqliteBackend.emit_ddl(&schema_s()) {
            conn.execute(&stmt.text, []).unwrap();
        }

        // Matching schema ⇒ empty diff.
        assert!(diff_table(&conn, &schema_s()).unwrap().is_empty());

        // Wrong declared type on `status` (Text vs the table's INTEGER).
        let mut wrong_type = schema_s();
        wrong_type
            .columns
            .iter_mut()
            .find(|c| c.name == "status")
            .unwrap()
            .kind = ColKind::Text;
        let d = diff_table(&conn, &wrong_type).unwrap();
        assert_eq!(
            d.kind_mismatches,
            vec![(
                "status".to_string(),
                "TEXT".to_string(),
                "INTEGER".to_string()
            )]
        );
        assert!(d.missing_columns.is_empty() && d.extra_columns.is_empty());

        // Declared column absent from the table.
        let mut missing = schema_s();
        missing.columns.push(col("region", ColKind::Text));
        let d = diff_table(&conn, &missing).unwrap();
        assert_eq!(d.missing_columns.len(), 1);
        assert_eq!(d.missing_columns[0].name, "region");

        // Table column not in the schema.
        let mut fewer = schema_s();
        fewer.columns.retain(|c| c.name != "city");
        let d = diff_table(&conn, &fewer).unwrap();
        assert_eq!(d.extra_columns, vec!["city".to_string()]);

        // Wrong key column set.
        let mut wrong_pk = schema_s();
        wrong_pk.pk = vec!["city".to_string()];
        let d = diff_table(&conn, &wrong_pk).unwrap();
        assert_eq!(
            d.pk_mismatch,
            Some((vec!["city".to_string()], vec!["sno".to_string()]))
        );
    }

    #[test]
    fn boolean_and_integer_are_introspection_indistinguishable() {
        // v1 blind spot (documented): PRAGMA table_info can't see the Boolean
        // `CHECK (c IN (0,1))`, so an INTEGER column and a Boolean column report
        // the same `type = INTEGER`, `notnull = 1` and diff clean.
        let (_dir, path) = tmp_db();
        let conn = Connection::open(&path).unwrap();
        let integer_schema = Schema {
            table: "flags".to_string(),
            columns: vec![col("id", ColKind::Integer), col("on", ColKind::Integer)],
            pk: vec!["id".to_string()],
        };
        for stmt in SqliteBackend.emit_ddl(&integer_schema) {
            conn.execute(&stmt.text, []).unwrap();
        }

        let boolean_schema = Schema {
            table: "flags".to_string(),
            columns: vec![col("id", ColKind::Integer), col("on", ColKind::Boolean)],
            pk: vec!["id".to_string()],
        };
        assert!(
            diff_table(&conn, &boolean_schema).unwrap().is_empty(),
            "v1 blind spot: Boolean ≈ Integer under PRAGMA table_info"
        );
    }
}

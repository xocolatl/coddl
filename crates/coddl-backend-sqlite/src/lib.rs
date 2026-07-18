//! SQLite backend.
//!
//! Implements `coddl_sqlemit::Backend` and `coddl_sqlemit::Conn` using
//! `rusqlite`. SQLite-specific quirks (BOOLEAN as `INTEGER CHECK (col IN (0, 1))`,
//! `CAST` on INSERT to dodge affinity coercion) belong here, not in
//! `coddl-sqlemit`. See `docs/storage.md` and `docs/sqlemit.md`.
//!
//! Today only the read path is wired: open read-only, prepare an emitted
//! `SELECT`, bind positional params, and step rows into backend-neutral
//! [`Value`] cells. Marshalling those cells into Coddl's canonical record
//! layout is the runtime's job, so this crate stays free of any RC / FFI
//! knowledge and depends only on `coddl-sqlemit`.

use coddl_sqlemit::{
    quote_ident, Backend, BackendError, ColKind, Conn, Dialect, Dsn, Result, Row, Schema,
    SqlString, StmtId, TypeMap, Value,
};
use rusqlite::{params_from_iter, Connection, OpenFlags};

/// The SQLite `Type ↔ SQL` keyword map: what `PRAGMA table_info` reports for
/// each column kind. The structural quirks a keyword can't carry — the Boolean
/// `CHECK (col IN (0, 1))` and the `Approximate` NaN-channel nullability — are
/// handled in `emit_ddl`, not here. `Character` binds as its integer codepoint
/// and `Rational` as canonical `TEXT "n/d"`, so both reuse existing keywords.
static SQLITE_TYPE_MAP: TypeMap = TypeMap {
    integer: "INTEGER",
    text: "TEXT",
    boolean: "INTEGER",
    rational: "TEXT",
    approximate: "REAL",
    character: "INTEGER",
};

/// The SQLite backend — the pure half. Stateless: emission goes through the
/// shared `coddl_sqlemit::emit_select` (the default trait method), and the
/// dialect is fixed.
pub struct SqliteBackend;

impl Backend for SqliteBackend {
    type Conn = SqliteConn;

    fn dialect(&self) -> Dialect {
        Dialect::SQLite
    }

    fn type_map(&self) -> &TypeMap {
        &SQLITE_TYPE_MAP
    }

    /// Render a base relvar's `Schema` to a single SQLite `CREATE TABLE`.
    ///
    /// Column defs use the shared `type_map` keyword, then this method adds the
    /// SQLite-specific structure (`docs/sqlemit.md`): every total column is
    /// `NOT NULL` (RM Pro 4) **except** an `Approximate`, which stays nullable so
    /// SQLite can encode the `NaN` value as `NULL` (`value_to_sqlite` — a
    /// `NOT NULL` `REAL` column would reject a NaN store); a `Boolean` gets a
    /// `CHECK (col IN (0, 1))` since SQLite has no boolean type; the key is a
    /// **table-level** `PRIMARY KEY` (never inline `INTEGER PRIMARY KEY`, whose
    /// rowid-alias accepts NULL). Columns render in the order the `Schema`
    /// supplies (heading-sorted); PK columns are name-sorted by the fold.
    fn emit_ddl(&self, schema: &Schema) -> Vec<SqlString> {
        let type_map = self.type_map();
        let mut col_defs: Vec<String> = Vec::with_capacity(schema.columns.len());
        for col in &schema.columns {
            let quoted = quote_ident(&col.name);
            let mut def = format!("{quoted} {}", type_map.sql_type(col.kind));
            // The Approximate NaN channel is the one place a total column is
            // physically nullable (SQLite can't store NaN, so NULL encodes it).
            if col.not_null && col.kind != ColKind::Approximate {
                def.push_str(" NOT NULL");
            }
            if col.kind == ColKind::Boolean {
                def.push_str(&format!(" CHECK ({quoted} IN (0, 1))"));
            }
            col_defs.push(def);
        }
        let pk = schema
            .pk
            .iter()
            .map(|k| quote_ident(k))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "CREATE TABLE {} ({}, PRIMARY KEY ({pk}))",
            quote_ident(&schema.table),
            col_defs.join(", "),
        );
        vec![SqlString {
            text,
            param_count: 0,
        }]
    }

    fn open(&self, dsn: &Dsn) -> Result<Self::Conn> {
        // Read-only: hand-edits to the file between materialization and reads
        // can't corrupt an in-memory snapshot; NO_MUTEX since the runtime
        // already serializes access to the connection.
        let conn = Connection::open_with_flags(
            &dsn.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| BackendError::Connect(e.to_string()))?;
        Ok(SqliteConn {
            conn,
            statements: Vec::new(),
        })
    }
}

/// A live SQLite connection. Prepared SQL is held by text and re-prepared
/// through rusqlite's internal statement cache (`prepare_cached`) on each
/// step — that sidesteps the borrow lifetimes of a stored `Statement` while
/// still avoiding re-compilation.
pub struct SqliteConn {
    conn: Connection,
    statements: Vec<String>,
}

impl Conn for SqliteConn {
    fn prepare(&mut self, sql: &SqlString) -> Result<StmtId> {
        let id = StmtId(self.statements.len() as u32);
        self.statements.push(sql.text.clone());
        Ok(id)
    }

    fn bind_and_step(&mut self, id: StmtId, params: &[Value]) -> Result<Vec<Row>> {
        let sql = self
            .statements
            .get(id.0 as usize)
            .ok_or_else(|| BackendError::Other(format!("unknown statement id {}", id.0)))?
            .clone();

        let mut stmt = self
            .conn
            .prepare_cached(&sql)
            .map_err(|e| BackendError::Prepare(e.to_string()))?;
        let column_count = stmt.column_count();

        let bindings: Vec<rusqlite::types::Value> = params.iter().map(value_to_sqlite).collect();
        let mut rows = stmt
            .query(params_from_iter(bindings.iter()))
            .map_err(|e| BackendError::Step(e.to_string()))?;

        let mut out: Vec<Row> = Vec::new();
        while let Some(row) = rows.next().map_err(|e| BackendError::Step(e.to_string()))? {
            let mut cells: Row = Vec::with_capacity(column_count);
            for i in 0..column_count {
                cells.push(cell_to_value(row, i)?);
            }
            out.push(cells);
        }
        Ok(out)
    }
}

/// Lower a storage `Value` to a rusqlite bind value. `Boolean` binds as the
/// integer 0/1 SQLite stores it as; `Character` as its integer codepoint;
/// `Approximate` as a REAL from its canonical bits.
fn value_to_sqlite(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Integer(n) => Sql::Integer(*n),
        Value::Text(s) => Sql::Text(s.clone()),
        Value::Character(cp) => Sql::Integer(*cp as i64),
        // SQLite encodes the NaN value as NULL (it can't store NaN); finite/±Inf
        // binds as REAL. The reverse of `cell_to_value`'s Null/Real handling.
        Value::Approximate(bits) => {
            let v = f64::from_bits(*bits);
            if v.is_nan() {
                Sql::Null
            } else {
                Sql::Real(v)
            }
        }
        // A Rational binds as its canonical `"n/d"` text (no native exact-rational
        // type on SQLite); canonical form ⇒ text-`=` is value-`=`.
        Value::Rational(n, d) => Sql::Text(format!("{n}/{d}")),
        Value::Boolean(b) => Sql::Integer(*b as i64),
    }
}

/// Read one result cell by its SQLite storage class. A `Boolean` is stored as
/// an integer, so it surfaces here as `Value::Integer`; the runtime refines it
/// to `Boolean` using the result heading. `NULL` is a hard error — Coddl
/// relvars have no nulls (RM Pro 4).
fn cell_to_value(row: &rusqlite::Row<'_>, i: usize) -> Result<Value> {
    use rusqlite::types::ValueRef;
    match row
        .get_ref(i)
        .map_err(|e| BackendError::Step(e.to_string()))?
    {
        ValueRef::Integer(n) => Ok(Value::Integer(n)),
        // A REAL surfaces as `Approximate` (canonical bits); the runtime refines
        // it against the result heading, same as Integer→Boolean/Character.
        ValueRef::Real(f) => {
            let bits = if f.is_nan() {
                f64::NAN.to_bits()
            } else if f == 0.0 {
                0
            } else {
                f.to_bits()
            };
            Ok(Value::Approximate(bits))
        }
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(|s| Value::Text(s.to_string()))
            .map_err(|e| BackendError::Step(format!("non-UTF-8 text cell: {e}"))),
        // A NULL in an `Approximate` column is the encoding of `NaN` and is
        // decoded there by the heading-aware runtime path (`marshal_rows`). This
        // storage-class-only path has no heading to distinguish that from a
        // genuine (forbidden) missing value, so it treats a bare NULL as a
        // violation (RM Pro 4: no nulls).
        ValueRef::Null => Err(BackendError::Step(
            "unexpected NULL cell (RM Pro 4: no nulls)".to_string(),
        )),
        other => Err(BackendError::Step(format!(
            "unsupported cell storage class: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_sqlemit::Column;

    fn col(name: &str, kind: ColKind) -> Column {
        Column {
            name: name.to_string(),
            kind,
            not_null: true,
        }
    }

    fn ddl(schema: &Schema) -> String {
        let stmts = SqliteBackend.emit_ddl(schema);
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].param_count, 0);
        stmts[0].text.clone()
    }

    // Golden 1 — suppliers: single key, Text + Integer. Columns render
    // heading-sorted (city, sname, sno, status).
    #[test]
    fn emit_ddl_suppliers_single_key() {
        let schema = Schema {
            table: "s".to_string(),
            columns: vec![
                col("city", ColKind::Text),
                col("sname", ColKind::Text),
                col("sno", ColKind::Text),
                col("status", ColKind::Integer),
            ],
            pk: vec!["sno".to_string()],
        };
        assert_eq!(
            ddl(&schema),
            r#"CREATE TABLE "s" ("city" TEXT NOT NULL, "sname" TEXT NOT NULL, "sno" TEXT NOT NULL, "status" INTEGER NOT NULL, PRIMARY KEY ("sno"))"#
        );
    }

    // Golden 2 — parts: a Rational column stores as TEXT "n/d".
    #[test]
    fn emit_ddl_parts_rational_stores_as_text() {
        let schema = Schema {
            table: "p".to_string(),
            columns: vec![
                col("city", ColKind::Text),
                col("color", ColKind::Text),
                col("pname", ColKind::Text),
                col("pno", ColKind::Text),
                col("weight", ColKind::Rational),
            ],
            pk: vec!["pno".to_string()],
        };
        assert_eq!(
            ddl(&schema),
            r#"CREATE TABLE "p" ("city" TEXT NOT NULL, "color" TEXT NOT NULL, "pname" TEXT NOT NULL, "pno" TEXT NOT NULL, "weight" TEXT NOT NULL, PRIMARY KEY ("pno"))"#
        );
    }

    // Golden 3 — shipments: composite key. PK columns are name-sorted
    // (pno, sno) regardless of how the catalog declared `key { sno, pno }`.
    #[test]
    fn emit_ddl_shipments_composite_key() {
        let schema = Schema {
            table: "sp".to_string(),
            columns: vec![
                col("pno", ColKind::Text),
                col("qty", ColKind::Integer),
                col("sno", ColKind::Text),
            ],
            pk: vec!["pno".to_string(), "sno".to_string()],
        };
        assert_eq!(
            ddl(&schema),
            r#"CREATE TABLE "sp" ("pno" TEXT NOT NULL, "qty" INTEGER NOT NULL, "sno" TEXT NOT NULL, PRIMARY KEY ("pno", "sno"))"#
        );
    }

    // Golden 4 — Boolean gets the SQLite 0/1 CHECK on the INTEGER column.
    #[test]
    fn emit_ddl_boolean_gets_check_constraint() {
        let schema = Schema {
            table: "users".to_string(),
            columns: vec![col("active", ColKind::Boolean), col("id", ColKind::Integer)],
            pk: vec!["id".to_string()],
        };
        assert_eq!(
            ddl(&schema),
            r#"CREATE TABLE "users" ("active" INTEGER NOT NULL CHECK ("active" IN (0, 1)), "id" INTEGER NOT NULL, PRIMARY KEY ("id"))"#
        );
    }

    // Golden 5 — an Approximate column is REAL and stays *nullable* (the NaN
    // channel: SQLite encodes NaN as NULL, so NOT NULL would reject it); a
    // Character column is a NOT NULL INTEGER codepoint.
    #[test]
    fn emit_ddl_approximate_is_nullable_character_is_integer() {
        let schema = Schema {
            table: "readings".to_string(),
            columns: vec![
                col("grade", ColKind::Character),
                col("id", ColKind::Integer),
                col("score", ColKind::Approximate),
            ],
            pk: vec!["id".to_string()],
        };
        assert_eq!(
            ddl(&schema),
            r#"CREATE TABLE "readings" ("grade" INTEGER NOT NULL, "id" INTEGER NOT NULL, "score" REAL, PRIMARY KEY ("id"))"#
        );
    }

    // Execution check — the three suppliers-and-parts schemas must be valid,
    // executable SQLite, and PRAGMA table_info must report exactly the columns,
    // declared types, NOT NULL flags, and PK membership emit_ddl intended.
    #[test]
    fn emit_ddl_produces_executable_sqlite() {
        let schemas = [
            Schema {
                table: "s".to_string(),
                columns: vec![
                    col("city", ColKind::Text),
                    col("sname", ColKind::Text),
                    col("sno", ColKind::Text),
                    col("status", ColKind::Integer),
                ],
                pk: vec!["sno".to_string()],
            },
            Schema {
                table: "p".to_string(),
                columns: vec![
                    col("pno", ColKind::Text),
                    col("weight", ColKind::Rational),
                    col("score", ColKind::Approximate),
                ],
                pk: vec!["pno".to_string()],
            },
            Schema {
                table: "sp".to_string(),
                columns: vec![
                    col("pno", ColKind::Text),
                    col("qty", ColKind::Integer),
                    col("sno", ColKind::Text),
                ],
                pk: vec!["pno".to_string(), "sno".to_string()],
            },
        ];

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        for schema in &schemas {
            for stmt in SqliteBackend.emit_ddl(schema) {
                conn.execute(&stmt.text, []).unwrap();
            }
        }

        // Verify `sp`'s introspection: composite PK, name/type/notnull per column.
        let mut pragma = conn.prepare(r#"PRAGMA table_info("sp")"#).unwrap();
        // (cid, name, type, notnull, dflt_value, pk)
        let rows: Vec<(String, String, i64, i64)> = pragma
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                ("pno".to_string(), "TEXT".to_string(), 1, 1),
                ("qty".to_string(), "INTEGER".to_string(), 1, 0),
                ("sno".to_string(), "TEXT".to_string(), 1, 2),
            ]
        );

        // Verify `p`'s Approximate column is physically nullable (notnull = 0).
        let mut pragma_p = conn.prepare(r#"PRAGMA table_info("p")"#).unwrap();
        let score_notnull: i64 = pragma_p
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .find(|(name, _)| name == "score")
            .map(|(_, notnull)| notnull)
            .unwrap();
        assert_eq!(score_notnull, 0);
    }

    #[test]
    fn approximate_store_encoding_nan_to_null_finite_to_real() {
        use rusqlite::types::Value as Sql;
        // Finite / ±Inf bind as REAL; the NaN *value* encodes as SQL NULL
        // (SQLite can't store NaN). `marshal_rows` reverses this on retrieval.
        assert_eq!(
            value_to_sqlite(&Value::Approximate(1.5f64.to_bits())),
            Sql::Real(1.5)
        );
        assert_eq!(
            value_to_sqlite(&Value::Approximate(f64::INFINITY.to_bits())),
            Sql::Real(f64::INFINITY)
        );
        assert_eq!(
            value_to_sqlite(&Value::Approximate(f64::NAN.to_bits())),
            Sql::Null
        );
    }

    /// Seed a one-row db, then run the exact SELECT `coddl-sqlemit` emits for
    /// `Greetings where id = 1`, bind `1`, and check the row comes back.
    #[test]
    fn executes_an_emitted_select_against_a_seeded_db() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        {
            let seed = Connection::open(&path).unwrap();
            seed.execute_batch(
                "CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL);
                 INSERT INTO greetings (id, message) VALUES (1, 'hello world');",
            )
            .unwrap();
        }

        let sql = SqlString {
            text: r#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?1"#.to_string(),
            param_count: 1,
        };

        let backend = SqliteBackend;
        let mut conn = backend.open(&Dsn { path }).unwrap();
        let stmt = conn.prepare(&sql).unwrap();
        let rows = conn.bind_and_step(stmt, &[Value::Integer(1)]).unwrap();

        // Cells in SELECT-list order: id, message.
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(1),
                Value::Text("hello world".to_string())
            ]]
        );
    }

    #[test]
    fn no_match_returns_no_rows() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        {
            let seed = Connection::open(&path).unwrap();
            seed.execute_batch(
                "CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL);
                 INSERT INTO greetings (id, message) VALUES (1, 'hello world');",
            )
            .unwrap();
        }

        let sql = SqlString {
            text: r#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?1"#.to_string(),
            param_count: 1,
        };
        let backend = SqliteBackend;
        let mut conn = backend.open(&Dsn { path }).unwrap();
        let stmt = conn.prepare(&sql).unwrap();
        let rows = conn.bind_and_step(stmt, &[Value::Integer(99)]).unwrap();
        assert!(rows.is_empty());
    }
}

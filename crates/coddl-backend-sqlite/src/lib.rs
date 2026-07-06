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
    Backend, BackendError, Conn, Dialect, Dsn, Result, Row, SqlString, StmtId, Value,
};
use rusqlite::{params_from_iter, Connection, OpenFlags};

/// The SQLite backend — the pure half. Stateless: emission goes through the
/// shared `coddl_sqlemit::emit_select` (the default trait method), and the
/// dialect is fixed.
pub struct SqliteBackend;

impl Backend for SqliteBackend {
    type Conn = SqliteConn;

    fn dialect(&self) -> Dialect {
        Dialect::SQLite
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
/// integer 0/1 SQLite stores it as; `Character` as its integer codepoint.
fn value_to_sqlite(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Integer(n) => Sql::Integer(*n),
        Value::Text(s) => Sql::Text(s.clone()),
        Value::Character(cp) => Sql::Integer(*cp as i64),
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
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(|s| Value::Text(s.to_string()))
            .map_err(|e| BackendError::Step(format!("non-UTF-8 text cell: {e}"))),
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
            text: r#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?"#.to_string(),
            param_count: 1,
        };

        let backend = SqliteBackend;
        let mut conn = backend.open(&Dsn { path }).unwrap();
        let stmt = conn.prepare(&sql).unwrap();
        let rows = conn.bind_and_step(stmt, &[Value::Integer(1)]).unwrap();

        // Cells in SELECT-list order: id, message.
        assert_eq!(
            rows,
            vec![vec![Value::Integer(1), Value::Text("hello world".to_string())]]
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
            text: r#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?"#.to_string(),
            param_count: 1,
        };
        let backend = SqliteBackend;
        let mut conn = backend.open(&Dsn { path }).unwrap();
        let stmt = conn.prepare(&sql).unwrap();
        let rows = conn.bind_and_step(stmt, &[Value::Integer(99)]).unwrap();
        assert!(rows.is_empty());
    }
}

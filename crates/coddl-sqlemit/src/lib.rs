//! RelIR → SQL emission and the storage-backend traits.
//!
//! The traits in this crate are implemented by `coddl-backend-sqlite`
//! and `coddl-backend-postgres`. SQL emission follows the mandatory
//! rules table in `docs/sqlemit.md` — `SELECT DISTINCT` everywhere,
//! never `NULL`/`NULLABLE`/`IS NULL`/outer joins, explicit `BEGIN`/`COMMIT`,
//! enumerate columns in deterministic order, etc.
//!
//! This crate is shared between the compiler and the runtime — the SQL
//! emitter must be callable from a `staticlib` linked into user binaries
//! (for plans built at runtime; see `docs/runtime.md` "Reaching the engines").

use std::fmt;

use coddl_relir::{Heading, Literal, Predicate, RelExpr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    SQLite,
    Postgres,
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dialect::SQLite => f.write_str("sqlite"),
            Dialect::Postgres => f.write_str("postgres"),
        }
    }
}

/// A piece of SQL with its parameter slots already enumerated.
///
/// Parameters are positional in the emitted text (`?` for SQLite,
/// `$1`, `$2` for Postgres) but the higher-level compiler tracks them
/// by name; the binding step (Conn::bind_and_step) takes them in
/// declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlString {
    pub text: String,
    pub param_count: u32,
}

/// A stable identifier for one emitted query, derived from the dialect
/// and the SQL text. Identical text under the same dialect yields the
/// same id, so the runtime can cache the prepared statement by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlanId(pub u64);

/// The full result of emitting one relvar-rooted relational expression:
/// the SQL text, the bind values in positional order, the heading of the
/// rows it returns, and the cache key.
#[derive(Clone, Debug)]
pub struct SqlQuery {
    pub sql: SqlString,
    pub params: Vec<Literal>,
    pub result_heading: Heading,
    pub plan_id: PlanId,
}

/// Identifier for a prepared statement cached by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StmtId(pub u32);

/// Identifier for a backend-side temp relation that an in-memory relation
/// was shipped into (see `docs/sqlemit.md` "Sending in-memory relations
/// back into SQL").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TempRelRef(pub u32);

/// Opaque shapes still used only by the effectful `Conn`/`Backend`
/// signatures; they gain real definitions when the backend is wired up.
pub struct Schema;
pub struct TypeMap;
pub struct Tuple;
pub struct Value;
pub struct Dsn;
pub struct RowIter<'a> {
    _phantom: std::marker::PhantomData<&'a ()>,
}

#[derive(Debug)]
pub enum BackendError {
    Connect(String),
    Prepare(String),
    Step(String),
    Other(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Connect(m) => write!(f, "connect: {m}"),
            BackendError::Prepare(m) => write!(f, "prepare: {m}"),
            BackendError::Step(m) => write!(f, "step: {m}"),
            BackendError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for BackendError {}

pub type Result<T> = std::result::Result<T, BackendError>;

/// Emit one relvar-rooted relational expression as a backend `SELECT`.
///
/// The expression must bottom out in a `RelvarRef`, optionally wrapped in
/// `Restrict` (→ `WHERE`) and/or `Project` (→ a narrowed column list). The
/// column list is exactly the expression's heading, so an author-written
/// projection narrows the `SELECT` faithfully — there is no implicit
/// column pruning. `SELECT DISTINCT` is always emitted (RM Pro 3).
pub fn emit_select(expr: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    // Descend to the relvar leaf, collecting restriction predicates (outermost
    // first) along the way.
    let mut preds: Vec<&Predicate> = Vec::new();
    let leaf = relvar_leaf(expr, &mut preds)?;
    let (table_name, columns) = match leaf {
        RelExpr::RelvarRef {
            table_name,
            columns,
            ..
        } => (table_name, columns.as_slice()),
        // relvar_leaf only ever returns a RelvarRef.
        _ => return Err(BackendError::Other("expected a relvar leaf".to_string())),
    };

    // SELECT list = the (projection-aware) result heading, in canonical order.
    let heading = expr.heading();
    let mut select_cols = Vec::with_capacity(heading.attrs().len());
    for (attr, _ty) in heading.attrs() {
        select_cols.push(quote_ident(column_for(columns, attr)?));
    }
    let mut text = format!(
        "SELECT DISTINCT {} FROM {}",
        select_cols.join(", "),
        quote_ident(table_name),
    );

    // WHERE = the conjunction of the collected attribute-equals-literal tests.
    let mut params: Vec<Literal> = Vec::new();
    if !preds.is_empty() {
        let mut conjuncts = Vec::with_capacity(preds.len());
        for pred in &preds {
            match pred {
                Predicate::AttrEq { attr, value } => {
                    let placeholder = match dialect {
                        Dialect::SQLite => "?".to_string(),
                        Dialect::Postgres => format!("${}", params.len() + 1),
                    };
                    conjuncts.push(format!("{} = {placeholder}", quote_ident(column_for(columns, attr)?)));
                    params.push(value.clone());
                }
            }
        }
        text.push_str(" WHERE ");
        text.push_str(&conjuncts.join(" AND "));
    }

    let plan_id = PlanId(fnv1a(dialect, &text));
    Ok(SqlQuery {
        sql: SqlString {
            param_count: params.len() as u32,
            text,
        },
        params,
        result_heading: heading,
        plan_id,
    })
}

/// Walk down `Restrict`/`Project` to the underlying `RelvarRef`, pushing each
/// `Restrict`'s predicate into `preds`.
fn relvar_leaf<'a>(expr: &'a RelExpr, preds: &mut Vec<&'a Predicate>) -> Result<&'a RelExpr> {
    match expr {
        RelExpr::RelvarRef { .. } => Ok(expr),
        RelExpr::Restrict { input, pred } => {
            preds.push(pred);
            relvar_leaf(input, preds)
        }
        RelExpr::Project { input, .. } => relvar_leaf(input, preds),
    }
}

/// The SQL column an attribute maps to, per the relvar's `(attr, column)` map.
fn column_for<'a>(columns: &'a [(String, String)], attr: &str) -> Result<&'a str> {
    columns
        .iter()
        .find(|(a, _)| a == attr)
        .map(|(_, c)| c.as_str())
        .ok_or_else(|| BackendError::Other(format!("no column mapping for attribute `{attr}`")))
}

/// Double-quote a SQL identifier, doubling any embedded quote.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// FNV-1a over the dialect label and the SQL text — a deterministic, text-stable
/// plan id without pulling in a hashing dependency.
fn fnv1a(dialect: Dialect, text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let label = dialect.to_string();
    for b in label.bytes().chain(std::iter::once(0u8)).chain(text.bytes()) {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Backend = SQL-emission half (pure) + factory for connections.
///
/// The compiler picks one backend at build time via a Cargo feature on
/// the runtime crate. If passing backends around as values gets clumsy
/// with the associated-type form, the planned escape hatch (§5) is a
/// `dyn`-friendly `BackendOps` record-of-fn-pointers; decide once the
/// second backend exists.
pub trait Backend {
    type Conn: Conn;

    fn dialect(&self) -> Dialect;

    /// Render a relvar-rooted expression to this backend's SQL. The default
    /// delegates to the shared [`emit_select`] with the backend's dialect;
    /// a backend only overrides this for dialect quirks the shared emitter
    /// can't express.
    fn emit_select(&self, expr: &RelExpr) -> Result<SqlQuery> {
        emit_select(expr, self.dialect())
    }

    fn emit_ddl(&self, schema: &Schema) -> Vec<SqlString>;
    fn type_map(&self) -> &TypeMap;
    fn open(&self, dsn: &Dsn) -> Result<Self::Conn>;
}

/// Conn = effectful side: own a live connection, prepare/cache statements,
/// step rows, ship in-memory relations to temp tables.
pub trait Conn {
    fn prepare(&mut self, sql: &SqlString) -> Result<StmtId>;
    fn bind_and_step<'a>(&'a mut self, id: StmtId, params: &[Value]) -> Result<RowIter<'a>>;
    fn materialize_temp(&mut self, heading: &Heading, rows: &[Tuple]) -> Result<TempRelRef>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_relir::{Heading, Type};

    fn greetings_heading() -> Heading {
        Heading::new(vec![
            ("id".to_string(), Type::Integer),
            ("message".to_string(), Type::Text),
        ])
    }

    fn greetings() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Greetings".to_string(),
            database: "greetings".to_string(),
            heading: greetings_heading(),
            table_name: "greetings".to_string(),
            columns: vec![
                ("id".to_string(), "id".to_string()),
                ("message".to_string(), "message".to_string()),
            ],
        }
    }

    fn where_id_1(input: RelExpr) -> RelExpr {
        RelExpr::Restrict {
            input: Box::new(input),
            pred: Predicate::AttrEq {
                attr: "id".to_string(),
                value: Literal::Integer(1),
            },
        }
    }

    #[test]
    fn restrict_emits_select_distinct_with_where() {
        let q = emit_select(&where_id_1(greetings()), Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Literal::Integer(1)]);
        assert_eq!(q.result_heading, greetings_heading());
    }

    #[test]
    fn bare_relvar_emits_select_distinct_no_where() {
        let q = emit_select(&greetings(), Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "id", "message" FROM "greetings""#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
    }

    #[test]
    fn author_projection_narrows_the_select_list() {
        // project { message } (Greetings where id = 1) — author-written, emitted faithfully.
        let expr = RelExpr::Project {
            input: Box::new(where_id_1(greetings())),
            keep: vec!["message".to_string()],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(
            q.result_heading,
            Heading::new(vec![("message".to_string(), Type::Text)])
        );
    }

    #[test]
    fn postgres_uses_dollar_placeholders() {
        let q = emit_select(&where_id_1(greetings()), Dialect::Postgres).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = $1"#
        );
    }

    #[test]
    fn plan_id_is_stable_and_text_sensitive() {
        let a = emit_select(&where_id_1(greetings()), Dialect::SQLite).unwrap();
        let b = emit_select(&where_id_1(greetings()), Dialect::SQLite).unwrap();
        assert_eq!(a.plan_id, b.plan_id);
        let bare = emit_select(&greetings(), Dialect::SQLite).unwrap();
        assert_ne!(a.plan_id, bare.plan_id);
    }
}

//! RelIR → SQL emission and the storage-backend traits.
//!
//! The traits in this crate are implemented by `coddl-backend-sqlite`
//! and `coddl-backend-postgres`. SQL emission follows the mandatory
//! rules table in `docs/sqlemit.md` — the result is always a set
//! (`SELECT DISTINCT` unless a surviving key / cardinality bound proves it
//! redundant; see `RelExpr::needs_distinct`), never
//! `NULL`/`NULLABLE`/`IS NULL`/outer joins, explicit `BEGIN`/`COMMIT`,
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
    pub params: Vec<Value>,
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

/// Opaque shapes still used only by the not-yet-implemented DDL / temp-table
/// paths; they gain real definitions when those land.
pub struct Schema;
pub struct TypeMap;
pub struct Tuple;

/// A scalar value crossing the storage boundary — a bind parameter or a
/// result cell. The storage layer owns this type rather than reusing the
/// relational IR's `Literal`, so the backend crates never depend on
/// `coddl-relir` (see `docs/principles.md` "Toward self-hosting").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Integer(i64),
    Text(String),
    Boolean(bool),
}

impl From<Literal> for Value {
    fn from(lit: Literal) -> Self {
        match lit {
            Literal::Integer(n) => Value::Integer(n),
            Literal::Text(s) => Value::Text(s),
            Literal::Boolean(b) => Value::Boolean(b),
        }
    }
}

/// One result row: its cells in SELECT-list (heading-canonical) order.
pub type Row = Vec<Value>;

/// A connection target. For SQLite this is a database file path.
#[derive(Clone, Debug)]
pub struct Dsn {
    pub path: String,
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
    // Resolve the tree to its physical table and output `(attr, column)` map —
    // renames remap the attr side, projects narrow it — collecting each
    // restriction as a resolved `(column, value)` conjunct along the way.
    let mut wheres: Vec<(String, Literal)> = Vec::new();
    let (from_clause, output_cols) = resolve(expr, &mut wheres)?;

    // SELECT list = the result heading in canonical order. Each attribute is
    // its physical column, aliased `AS` the attribute when the names differ —
    // a `rename`, or any attr/column mismatch — so the rename is pushed to SQL
    // (the result columns are named by the Coddl attributes).
    let heading = expr.heading();
    let mut select_cols = Vec::with_capacity(heading.attrs().len());
    for (attr, _ty) in heading.attrs() {
        let col = column_for(&output_cols, attr)?;
        if col == attr.as_str() {
            select_cols.push(quote_ident(col));
        } else {
            select_cols.push(format!("{} AS {}", quote_ident(col), quote_ident(attr)));
        }
    }
    // A nullary projection (`project {}`) has an empty heading, and SQL has no
    // zero-column SELECT. Emit the constant `1`: `SELECT DISTINCT 1 …` returns
    // exactly one row when any tuple matches and zero rows when none do, which
    // the runtime marshals against the empty (0-attribute) descriptor as
    // `reltrue` / `relfalse`. The `1` column is never read.
    let select_list = if select_cols.is_empty() {
        "1".to_string()
    } else {
        select_cols.join(", ")
    };
    // `DISTINCT` only when the result isn't provably already a set — a
    // surviving candidate key or a cardinality-≤-1 restriction makes it
    // redundant (RM Pro 3 is still upheld; we only drop a proven no-op).
    let distinct = if expr.needs_distinct() { "DISTINCT " } else { "" };
    let mut text = format!("SELECT {distinct}{select_list} FROM {from_clause}");

    // WHERE = the conjunction of the collected (column, literal) tests. Each
    // predicate was resolved to its physical column at its own level in the
    // tree (so a `where` above a `rename` resolves through the rename).
    let mut params: Vec<Value> = Vec::new();
    if !wheres.is_empty() {
        let mut conjuncts = Vec::with_capacity(wheres.len());
        for (col, value) in &wheres {
            let placeholder = match dialect {
                Dialect::SQLite => "?".to_string(),
                Dialect::Postgres => format!("${}", params.len() + 1),
            };
            conjuncts.push(format!("{} = {placeholder}", quote_ident(col)));
            params.push(Value::from(value.clone()));
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

/// Post-order walk returning the node's **FROM expression** (a quoted table,
/// or a composed `… INNER JOIN …`) and its output `(attribute, sql_column)`
/// map. `Rename` remaps the attribute side; `Project` narrows it; `Restrict`
/// resolves its predicate against the map **at its own level** (so a `where`
/// above a `rename` resolves through the rename) and pushes the
/// `(column, value)` onto `wheres`; `And` joins two FROM expressions.
fn resolve(
    expr: &RelExpr,
    wheres: &mut Vec<(String, Literal)>,
) -> Result<(String, Vec<(String, String)>)> {
    match expr {
        RelExpr::RelvarRef {
            table_name,
            columns,
            ..
        } => Ok((quote_ident(table_name), columns.clone())),
        RelExpr::Restrict { input, pred } => {
            let (from, cols) = resolve(input, wheres)?;
            let Predicate::AttrEq { attr, value } = pred;
            let col = column_for(&cols, attr)?.to_string();
            wheres.push((col, value.clone()));
            Ok((from, cols))
        }
        RelExpr::Project { input, keep } => {
            let (from, cols) = resolve(input, wheres)?;
            let kept = cols.into_iter().filter(|(a, _)| keep.contains(a)).collect();
            Ok((from, kept))
        }
        RelExpr::Rename { input, renames } => {
            let (from, cols) = resolve(input, wheres)?;
            let renamed = cols
                .into_iter()
                .map(|(a, c)| {
                    let new = renames
                        .iter()
                        .find(|(old, _)| *old == a)
                        .map(|(_, new)| new.clone())
                        .unwrap_or(a);
                    (new, c)
                })
                .collect();
            Ok((from, renamed))
        }
        RelExpr::And { lhs, rhs } => {
            let (lhs_from, lhs_cols) = resolve(lhs, wheres)?;
            let (rhs_from, rhs_cols) = resolve(rhs, wheres)?;
            // The shared columns. `join` has ≥1 (typechecked); `times` has 0
            // (disjoint headings, typechecked). Assumes a shared attribute maps
            // to the same column name on both sides.
            let using: Vec<String> = lhs_cols
                .iter()
                .filter(|(a, _)| rhs_cols.iter().any(|(b, _)| b == a))
                .map(|(_, c)| quote_ident(c))
                .collect();
            // ≥1 shared column → `INNER JOIN … USING (…)` naming the shared
            // columns (coalesced once). Never SQL `NATURAL JOIN`: the join key
            // comes from the catalog, not the live schema, so schema drift
            // fails loud instead of returning silently-wrong rows — see
            // docs/sqlemit.md "`USING` over `NATURAL JOIN`". Zero shared columns
            // (`times`) → a `CROSS JOIN`: `USING ()` is invalid SQL. Inner/cross
            // only — no outer joins (RM Pro 4).
            let from = if using.is_empty() {
                format!("{lhs_from} CROSS JOIN {rhs_from}")
            } else {
                format!(
                    "{lhs_from} INNER JOIN {rhs_from} USING ({})",
                    using.join(", ")
                )
            };
            // Merged map: every LHS column plus the RHS columns not shared.
            let mut merged = lhs_cols.clone();
            for (a, c) in rhs_cols {
                if !lhs_cols.iter().any(|(la, _)| la == &a) {
                    merged.push((a, c));
                }
            }
            Ok((from, merged))
        }
        // Never reached: a materialized leaf fails the cut's `RelvarRooted`
        // gate before SQL emission. Defensive only.
        RelExpr::MaterializedRelvar { name, .. } => Err(BackendError::Other(format!(
            "materialized relvar `{name}` reached SQL emission (should be in-process)"
        ))),
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

    fn open(&self, dsn: &Dsn) -> Result<Self::Conn>;

    /// DDL emission and the type map serve write/schema paths not yet wired;
    /// only the read path exists today.
    fn emit_ddl(&self, _schema: &Schema) -> Vec<SqlString> {
        unimplemented!("emit_ddl is not implemented yet")
    }
    fn type_map(&self) -> &TypeMap {
        unimplemented!("type_map is not implemented yet")
    }
}

/// Conn = effectful side: own a live connection, prepare/cache statements,
/// step rows, ship in-memory relations to temp tables.
pub trait Conn {
    fn prepare(&mut self, sql: &SqlString) -> Result<StmtId>;

    /// Bind positional params and step every row eagerly into owned cells.
    /// (v1 reads are small and the runtime materializes anyway, so a streaming
    /// cursor isn't worth the borrow gymnastics yet.)
    fn bind_and_step(&mut self, id: StmtId, params: &[Value]) -> Result<Vec<Row>>;

    /// Shipping in-memory relations into a backend temp table is not yet wired.
    fn materialize_temp(&mut self, _heading: &Heading, _rows: &[Tuple]) -> Result<TempRelRef> {
        unimplemented!("materialize_temp is not implemented yet")
    }
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
            keys: vec![vec!["id".to_string()]],
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
    fn restrict_on_key_drops_distinct() {
        // Full heading keeps the key `id`, so rows are already unique —
        // `DISTINCT` is provably redundant and elided.
        let q = emit_select(&where_id_1(greetings()), Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Integer(1)]);
        assert_eq!(q.result_heading, greetings_heading());
    }

    #[test]
    fn restrict_on_text_binds_a_text_param() {
        // `Greetings where message = "hello world"` pushes the Text literal as
        // a bound parameter; the surviving key `id` keeps the result a set, so
        // no `DISTINCT`.
        let expr = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrEq {
                attr: "message".to_string(),
                value: Literal::Text("hello world".to_string()),
            },
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "message" FROM "greetings" WHERE "message" = ?"#
        );
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Text("hello world".to_string())]);
    }

    #[test]
    fn bare_relvar_drops_distinct() {
        // A full relvar read keeps its key → already a set → no `DISTINCT`.
        let q = emit_select(&greetings(), Dialect::SQLite).unwrap();
        assert_eq!(q.sql.text, r#"SELECT "id", "message" FROM "greetings""#);
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
    }

    fn employees() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Employees".to_string(),
            database: "staffing".to_string(),
            heading: Heading::new(vec![
                ("emp_id".to_string(), Type::Integer),
                ("emp_name".to_string(), Type::Text),
                ("dept_id".to_string(), Type::Integer),
            ]),
            table_name: "employees".to_string(),
            columns: vec![
                ("emp_id".to_string(), "emp_id".to_string()),
                ("emp_name".to_string(), "emp_name".to_string()),
                ("dept_id".to_string(), "dept_id".to_string()),
            ],
            keys: vec![vec!["emp_id".to_string()]],
        }
    }

    fn departments() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Departments".to_string(),
            database: "staffing".to_string(),
            heading: Heading::new(vec![
                ("dept_id".to_string(), Type::Integer),
                ("dept_name".to_string(), Type::Text),
            ]),
            table_name: "departments".to_string(),
            columns: vec![
                ("dept_id".to_string(), "dept_id".to_string()),
                ("dept_name".to_string(), "dept_name".to_string()),
            ],
            keys: vec![vec!["dept_id".to_string()]],
        }
    }

    #[test]
    fn and_emits_inner_join_using_the_shared_column() {
        // `Employees join Departments` → RelExpr::And. Natural join on the one
        // shared attribute `dept_id`, emitted as `INNER JOIN ... USING`. The
        // SELECT list is the union heading in canonical (sorted) order; the join
        // drops both keys so `DISTINCT` stays (RM Pro 3).
        let expr = RelExpr::And {
            lhs: Box::new(employees()),
            rhs: Box::new(departments()),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "dept_id", "dept_name", "emp_id", "emp_name" FROM "employees" INNER JOIN "departments" USING ("dept_id")"#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("dept_id".to_string(), Type::Integer),
                ("dept_name".to_string(), Type::Text),
                ("emp_id".to_string(), Type::Integer),
                ("emp_name".to_string(), Type::Text),
            ])
        );
    }

    fn job_titles() -> RelExpr {
        RelExpr::RelvarRef {
            name: "JobTitles".to_string(),
            database: "staffing".to_string(),
            heading: Heading::new(vec![("title".to_string(), Type::Text)]),
            table_name: "job_titles".to_string(),
            columns: vec![("title".to_string(), "title".to_string())],
            keys: vec![vec!["title".to_string()]],
        }
    }

    fn locations() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Locations".to_string(),
            database: "staffing".to_string(),
            heading: Heading::new(vec![("location".to_string(), Type::Text)]),
            table_name: "locations".to_string(),
            columns: vec![("location".to_string(), "location".to_string())],
            keys: vec![vec!["location".to_string()]],
        }
    }

    #[test]
    fn and_with_disjoint_headings_emits_cross_join() {
        // `JobTitles times Locations` → RelExpr::And with no shared column.
        // `USING ()` is invalid SQL, so a disjoint product emits `CROSS JOIN`.
        // SELECT list is the union heading in canonical (sorted) order
        // (`location` before `title`); both single-attr keys are dropped, so
        // `DISTINCT` stays (RM Pro 3).
        let expr = RelExpr::And {
            lhs: Box::new(job_titles()),
            rhs: Box::new(locations()),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "location", "title" FROM "job_titles" CROSS JOIN "locations""#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("location".to_string(), Type::Text),
                ("title".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn compose_emits_join_with_shared_columns_dropped() {
        // `Employees compose Departments` → Project{And} keeping the non-shared
        // attributes. The And builds the natural join on `dept_id`; the Project
        // narrows the SELECT to drop `dept_id`. Result heading in canonical
        // (sorted) order; the dropped key means `DISTINCT` stays (RM Pro 3).
        let expr = RelExpr::Project {
            input: Box::new(RelExpr::And {
                lhs: Box::new(employees()),
                rhs: Box::new(departments()),
            }),
            keep: vec![
                "dept_name".to_string(),
                "emp_id".to_string(),
                "emp_name".to_string(),
            ],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "dept_name", "emp_id", "emp_name" FROM "employees" INNER JOIN "departments" USING ("dept_id")"#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("dept_name".to_string(), Type::Text),
                ("emp_id".to_string(), Type::Integer),
                ("emp_name".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn restrict_then_project_over_a_join() {
        // `(Employees join Departments) where dept_name = "Engineering"
        //  project { emp_name, dept_name }` — a join feeding `where` then
        // `project`. The Restrict resolves `dept_name` against the join's merged
        // column map and pushes it as a bound `WHERE`; the Project narrows the
        // SELECT. The join drops both keys, so `DISTINCT` stays.
        let expr = RelExpr::Project {
            input: Box::new(RelExpr::Restrict {
                input: Box::new(RelExpr::And {
                    lhs: Box::new(employees()),
                    rhs: Box::new(departments()),
                }),
                pred: Predicate::AttrEq {
                    attr: "dept_name".to_string(),
                    value: Literal::Text("Engineering".to_string()),
                },
            }),
            keep: vec!["dept_name".to_string(), "emp_name".to_string()],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "dept_name", "emp_name" FROM "employees" INNER JOIN "departments" USING ("dept_id") WHERE "dept_name" = ?"#
        );
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Text("Engineering".to_string())]);
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("dept_name".to_string(), Type::Text),
                ("emp_name".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn author_projection_narrows_the_select_list() {
        // (Greetings where id = 1) project {message}: the key filter bounds
        // cardinality to ≤ 1, so the projection can't dup → no `DISTINCT`.
        let expr = RelExpr::Project {
            input: Box::new(where_id_1(greetings())),
            keep: vec!["message".to_string()],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(
            q.result_heading,
            Heading::new(vec![("message".to_string(), Type::Text)])
        );
    }

    #[test]
    fn projection_dropping_the_key_keeps_distinct() {
        // Greetings project {message} — no filter, key dropped, cardinality
        // unbounded: the projection may create duplicates, so `DISTINCT` stays.
        let expr = RelExpr::Project {
            input: Box::new(greetings()),
            keep: vec!["message".to_string()],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "message" FROM "greetings""#
        );
    }

    #[test]
    fn nullary_projection_selects_the_constant_one() {
        // `(Greetings where id = 1) project {}` → empty heading. SQL has no
        // zero-column SELECT, so emit the constant `1`. The key filter bounds
        // cardinality to ≤ 1, so `DISTINCT` is elided; the row count (0/1) is
        // marshalled as relfalse/reltrue.
        let expr = RelExpr::Project {
            input: Box::new(where_id_1(greetings())),
            keep: vec![],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(q.sql.text, r#"SELECT 1 FROM "greetings" WHERE "id" = ?"#);
        assert_eq!(q.result_heading, Heading::new(vec![]));
    }

    #[test]
    fn nullary_projection_no_filter_keeps_distinct() {
        // Greetings project {} — unbounded, so `SELECT DISTINCT 1` collapses
        // any matching rows to one (reltrue) / none (relfalse).
        let expr = RelExpr::Project {
            input: Box::new(greetings()),
            keep: vec![],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(q.sql.text, r#"SELECT DISTINCT 1 FROM "greetings""#);
    }

    #[test]
    fn postgres_uses_dollar_placeholders() {
        let q = emit_select(&where_id_1(greetings()), Dialect::Postgres).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = $1"#
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

    #[test]
    fn renamed_read_aliases_columns() {
        // (Greetings where id = 1) rename {id: identifier, message: msg} —
        // pushed via `AS`; key `id` renamed to `identifier` still elides DISTINCT.
        let expr = RelExpr::Rename {
            input: Box::new(where_id_1(greetings())),
            renames: vec![
                ("id".to_string(), "identifier".to_string()),
                ("message".to_string(), "msg".to_string()),
            ],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id" AS "identifier", "message" AS "msg" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("identifier".to_string(), Type::Integer),
                ("msg".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn where_above_rename_resolves_through_it() {
        // (Greetings rename {id: identifier}) where identifier = 1 — the
        // predicate references the renamed name; it resolves to column "id".
        let renamed = RelExpr::Rename {
            input: Box::new(greetings()),
            renames: vec![("id".to_string(), "identifier".to_string())],
        };
        let expr = RelExpr::Restrict {
            input: Box::new(renamed),
            pred: Predicate::AttrEq {
                attr: "identifier".to_string(),
                value: Literal::Integer(1),
            },
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id" AS "identifier", "message" FROM "greetings" WHERE "id" = ?"#
        );
    }
}

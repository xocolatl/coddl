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

use coddl_relir::{CmpOp, Heading, Literal, Predicate, RelExpr, ScalarBinOp, ScalarExpr, Type};

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

/// CTE names for the `WITH RECURSIVE` transitive-closure emission: `coddl_tc`
/// is the recursive closure relation; `coddl_tc_op` is the non-recursive CTE
/// that holds the operand (edge set) once. The `coddl_tc*` prefix keeps them
/// clear of user tables — a clash needs a table bound to literally that SQL
/// name.
const TCLOSE_RESULT_CTE: &str = "coddl_tc";
const TCLOSE_OPERAND_CTE: &str = "coddl_tc_op";

/// Emit one relvar-rooted relational expression as a backend `SELECT`.
///
/// The expression bottoms out in a `RelvarRef`, optionally wrapped in
/// `Restrict` (→ `WHERE`), `Project` (→ a narrowed column list), `Rename`, and
/// `And` (→ `INNER JOIN`/`CROSS JOIN`). A root `Or` (surface `union`) emits a
/// set-op query `(<lhs>) UNION (<rhs>)`; a root `TClose` (surface `tclose`)
/// emits a `WITH RECURSIVE` query. The column list is exactly the expression's
/// heading, so an author-written projection narrows the `SELECT` faithfully.
/// `DISTINCT` is emitted unless the result is provably a set.
pub fn emit_select(expr: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    // Root transitive closure → a `WITH RECURSIVE` query. Handled here at the
    // true statement root, NOT in `emit_select_offset`: a `WITH`-prefixed query
    // cannot be a compound-`UNION`/`EXCEPT` operand (invalid SQL, and SQLite
    // rejects parenthesizing it), so a `TClose` reached as a set-op operand
    // goes through `emit_select_offset` → `resolve`'s Err arm, declines the
    // push, and decomposes in-process instead.
    if let RelExpr::TClose { input } = expr {
        return emit_tclose(input, dialect);
    }
    emit_select_offset(expr, dialect, 0)
}

/// Emit a root `TClose` (surface `tclose`) as a backend `WITH RECURSIVE` query.
///
/// The operand is a binary relation of two same-typed attributes `a` (canonical
/// `attrs[0]`, the source) and `b` (`attrs[1]`, the target) — the typechecker
/// guarantees this shape. It is defined **once** as a non-recursive CTE so its
/// bind parameters appear once; the recursive closure CTE references that CTE
/// for both its base and recursive members, computing
/// `R_{i+1} = R_i ∪ (R_i ∘ E)` which converges to `⋃_{k≥1} Eᵏ` (the closure).
/// Closure is direction-agnostic, so the result heading equals the operand
/// heading and the final `SELECT DISTINCT "a", "b"` marshals against it
/// unchanged.
fn emit_tclose(input: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    let heading = input.heading();
    let attrs = heading.attrs();
    // Defensive: a non-binary operand can't be closed. The typechecker rejects
    // it (T0041), so this only guards against a malformed RelIR.
    if attrs.len() != 2 {
        return Err(BackendError::Other(
            "tclose operand is not a binary relation".to_string(),
        ));
    }
    let a = quote_ident(&attrs[0].0); // source column (canonical order)
    let b = quote_ident(&attrs[1].0); // target column
    // Operand SELECT — its two columns are aliased to the attribute names a, b.
    // Emitted via `emit_select_offset` (not `emit_select`): the operand is never
    // itself a root `TClose` (a nested `(R tclose) tclose` declines through
    // `resolve` and decomposes in-process), and its placeholders start at the
    // statement's `$1`.
    let op = emit_select_offset(input, dialect, 0)?;
    let src = &op.sql.text;
    let result = TCLOSE_RESULT_CTE;
    let edges = TCLOSE_OPERAND_CTE;
    let text = format!(
        "WITH RECURSIVE {edges}({a}, {b}) AS ({src}), \
         {result}({a}, {b}) AS (\
         SELECT {a}, {b} FROM {edges} \
         UNION \
         SELECT {result}.{a}, {edges}.{b} FROM {result} JOIN {edges} ON {result}.{b} = {edges}.{a}) \
         SELECT DISTINCT {a}, {b} FROM {result}"
    );
    let plan_id = PlanId(fnv1a(dialect, &text));
    Ok(SqlQuery {
        sql: SqlString {
            param_count: op.sql.param_count,
            text,
        },
        params: op.params,
        result_heading: heading,
        plan_id,
    })
}

/// Emit a surgical `DELETE` for the subtrahend of a recognized relational
/// assignment (see [`emit_assignment`]). Not a public entry point — it is the
/// shared body of both DELETE-family arms.
///
/// `expr` is the subtrahend — a `RelvarRef`, optionally wrapped in `Restrict`
/// layers (the `where` predicate). It must bottom out in a single base relvar:
/// a composed FROM (a `Project`/`Rename`/`And`/set-op) is not a surgical DELETE
/// target, so this declines. The predicate reuses `resolve`'s
/// `(column, literal)` collection, so the WHERE is identical to the one a
/// `SELECT … WHERE` would emit for the same restriction. A bare `RelvarRef`
/// (the self-truncate arm) emits no WHERE.
fn emit_delete(expr: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    if delete_base_table(expr).is_none() {
        return Err(BackendError::Other(
            "delete operand does not resolve to a single base relvar".to_string(),
        ));
    }
    let mut wheres: Vec<(String, CmpOp, Literal)> = Vec::new();
    let (from_clause, _cols) = resolve(expr, &mut wheres)?;
    let mut params: Vec<Value> = Vec::new();
    let where_sql = render_where_clause(&wheres, dialect, 0, &mut params);
    let text = format!("DELETE FROM {from_clause}{where_sql}");
    let plan_id = PlanId(fnv1a(dialect, &text));
    Ok(SqlQuery {
        sql: SqlString {
            param_count: params.len() as u32,
            text,
        },
        params,
        // Unused for DML (no rows returned), but the operand heading is the
        // honest descriptor of what was matched.
        result_heading: expr.heading(),
        plan_id,
    })
}

/// Emit surgical DML for a relational assignment `t := <rhs>` to a public base
/// relvar, by recognizing the RHS [`RelExpr`] shape. `target` is the assignment
/// LHS lowered to its `RelvarRef`; `rhs` is the lowered RHS. Relational
/// assignment is the write primitive (RM Pre 21); the surgical equivalent is
/// shape-recognition on the RHS rather than a hydrate-mutate-writeback.
///
/// The recognized shapes all have the target relvar as one operand:
/// - `t := t minus <subtrahend>` where the subtrahend bottoms out in `t` (a
///   `where`-restriction, or the bare relvar) → delete exactly its rows via
///   [`emit_delete`]: `DELETE FROM t WHERE p…`, or a whole-table `DELETE FROM t`
///   for the self-subtraction `t minus t`;
/// - `t := t minus X` where `X` is any other pushable, same-heading relation →
///   an anti-join `DELETE FROM t WHERE EXISTS (… X … AND t.col = X.attr …)`;
/// - `t := t union e` where `e` is a pushable, same-heading relation → an
///   idempotent `INSERT INTO t … SELECT … FROM e WHERE NOT EXISTS (…)`, inserting
///   every tuple of `e` not already in `t` (union is commutative, so `t` may be
///   either operand);
/// - `t := (t where ¬p) union ((t where p) «substitute»)` (or a bare substitute
///   over `t`, update-all) → `UPDATE t SET … [WHERE p]` (see [`emit_update`]).
///
/// Any other shape is declined with `Err`.
pub fn emit_assignment(target: &RelExpr, rhs: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    let RelExpr::RelvarRef { table_name: t, .. } = target else {
        return Err(BackendError::Other(
            "assignment target does not resolve to a base relvar".to_string(),
        ));
    };
    let is_target = |e: &RelExpr| matches!(e, RelExpr::RelvarRef { table_name, .. } if table_name == t);
    match rhs {
        RelExpr::Minus { lhs, rhs: subtrahend } if is_target(lhs) => {
            if delete_base_table(subtrahend) == Some(t.as_str()) {
                // Subtrahend is the target relvar, optionally `where`-restricted:
                // delete exactly its rows (or every row, for `t minus t`).
                emit_delete(subtrahend, dialect)
            } else {
                // Subtrahend is some other same-heading relation: anti-join.
                emit_anti_join_delete(target, subtrahend, dialect)
            }
        }
        // `t := t union e` — union is commutative, so the target may be the left
        // or right operand; `e` is the other one.
        RelExpr::Or { lhs, rhs } if is_target(lhs) => {
            emit_idempotent_insert(target, rhs, dialect)
        }
        RelExpr::Or { lhs, rhs } if is_target(rhs) => {
            emit_idempotent_insert(target, lhs, dialect)
        }
        // Otherwise try the UPDATE shape: a `union` of the unchanged rows and the
        // substituted matching rows (`t := (t where ¬p) union ((t where p)
        // «substitute»)`), or a bare substitute over `t` (update-all). Anything
        // `emit_update` doesn't recognize declines with `Err`.
        _ => emit_update(target, rhs, dialect),
    }
}

/// Emit the idempotent `INSERT` for `t := t union e` where `e` is a pushable,
/// same-heading relation → insert every tuple of `e` not already in `t`. Set
/// union keeps `t` a set, so the `NOT EXISTS` makes re-inserting an identical
/// tuple a no-op; a tuple sharing a key but differing elsewhere is *not* skipped,
/// so `t`'s `PRIMARY KEY` rejects it — the Golden Rule (RM Pre 23): a
/// key-violating update fails rather than silently dropping the tuple. Tuple
/// equality on all attributes (RM Pre 8), no outer join (RM Pro 4) — the insert
/// mirror of [`emit_anti_join_delete`].
///
/// `e` renders via [`emit_select`] as a derived table with attribute-named
/// columns; the INSERT column list and the `NOT EXISTS` correlation use `t`'s
/// physical columns (canonical order). A non-pushable `e` (e.g. an in-memory
/// `MaterializedRelvar` or a relation literal) makes `emit_select` `Err`, which
/// propagates so the assignment declines (the in-memory path ships those rows at
/// runtime instead).
fn emit_idempotent_insert(target: &RelExpr, e: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    let RelExpr::RelvarRef {
        table_name,
        columns,
        ..
    } = target
    else {
        return Err(BackendError::Other(
            "insert target is not a base relvar".to_string(),
        ));
    };
    let src = emit_select(e, dialect)?;
    let insert_cols: Vec<String> = columns.iter().map(|(_, phys)| quote_ident(phys)).collect();
    let select_cols: Vec<String> = columns
        .iter()
        .map(|(attr, _)| format!("coddl_src.{}", quote_ident(attr)))
        .collect();
    let conjuncts: Vec<String> = columns
        .iter()
        .map(|(attr, phys)| {
            format!(
                "{}.{} = coddl_src.{}",
                quote_ident(table_name),
                quote_ident(phys),
                quote_ident(attr)
            )
        })
        .collect();
    let text = format!(
        "INSERT INTO {} ({}) SELECT {} FROM ({}) AS coddl_src WHERE NOT EXISTS (SELECT 1 FROM {} WHERE {})",
        quote_ident(table_name),
        insert_cols.join(", "),
        select_cols.join(", "),
        src.sql.text,
        quote_ident(table_name),
        conjuncts.join(" AND ")
    );
    let plan_id = PlanId(fnv1a(dialect, &text));
    Ok(SqlQuery {
        sql: SqlString {
            param_count: src.params.len() as u32,
            text,
        },
        params: src.params,
        result_heading: target.heading(),
        plan_id,
    })
}

/// Peel a **substitute** chain — `Rename?( Project?( Extend( inner ) ) )`, the
/// heading-preserving shape `R replace { c: e }` desugars to — into its `inner`
/// relation and the `(target_attr, value)` assignments. Each `Extend` value is
/// paired with its `Rename` target (`rename temp → target` × `extend temp = e`
/// → `target ← e`); an un-renamed extend keeps its own name. `None` when there's
/// no `Extend` (e.g. a plain `Restrict` — the unchanged-rows operand of an
/// UPDATE union). Mirrors the peel in [`emit_select_offset`].
fn peel_substitute(expr: &RelExpr) -> Option<(&RelExpr, Vec<(&str, &ScalarExpr)>)> {
    let mut n = expr;
    let mut renames: &[(String, String)] = &[];
    if let RelExpr::Rename { input, renames: r } = n {
        renames = r;
        n = input;
    }
    if let RelExpr::Project { input, .. } = n {
        n = input;
    }
    let RelExpr::Extend { input, extends } = n else {
        return None;
    };
    let sets = extends
        .iter()
        .map(|(name, _ty, scalar)| {
            let target = renames
                .iter()
                .find(|(old, _)| old == name)
                .map(|(_, new)| new.as_str())
                .unwrap_or(name.as_str());
            (target, scalar)
        })
        .collect();
    Some((input, sets))
}

/// Emit a surgical `UPDATE` for the TTM update expansion
/// `t := (t where ¬p) union ((t where p) «substitute»)` (or a bare substitute
/// over `t`, update-all). The substitute is the heading-preserving
/// `Extend → Project(all but targets) → Rename` chain (what `replace` produces
/// when the value reads the attribute it sets). Emits `UPDATE t SET c = e, … [WHERE p]`.
///
/// The "unchanged rows" operand must be the exact complement `t where ¬p` — same
/// attribute and value, the negated operator ([`CmpOp::negate`]) — over the same
/// `t`; otherwise this isn't an update and declines. The SET values render via
/// [`render_scalar`] (constants inline, like `extend`); the `WHERE` predicate's
/// literal is the one bound parameter.
fn emit_update(target: &RelExpr, rhs: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    let RelExpr::RelvarRef { table_name, .. } = target else {
        return Err(BackendError::Other(
            "update target is not a base relvar".to_string(),
        ));
    };
    let t = table_name.as_str();

    // The substitute's `inner` (`Restrict(t, p)` with a WHERE, or bare
    // `RelvarRef(t)` for update-all) and its SET assignments.
    let (inner, sets): (&RelExpr, Vec<(&str, &ScalarExpr)>) = match rhs {
        RelExpr::Or { lhs, rhs: r } => {
            // Exactly one operand is the substitute (changed rows); the other is
            // the unchanged-rows complement `t where ¬p`.
            let (sub, complement) = match (peel_substitute(lhs), peel_substitute(r)) {
                (Some(sub), None) => (sub, r.as_ref()),
                (None, Some(sub)) => (sub, lhs.as_ref()),
                _ => {
                    return Err(BackendError::Other(
                        "unrecognized assignment RHS shape (not a recognized update union)"
                            .to_string(),
                    ))
                }
            };
            let (inner, sets) = sub;
            // Changed rows must be `Restrict(t, p)`; complement `Restrict(t, ¬p)`.
            let RelExpr::Restrict { input: si, pred: p } = inner else {
                return Err(BackendError::Other(
                    "update: changed-rows operand is not a restriction".to_string(),
                ));
            };
            let RelExpr::Restrict {
                input: ci,
                pred: q,
            } = complement
            else {
                return Err(BackendError::Other(
                    "update: unchanged-rows operand is not a restriction".to_string(),
                ));
            };
            if delete_base_table(si) != Some(t) || delete_base_table(ci) != Some(t) {
                return Err(BackendError::Other(
                    "update: operands are not rooted in the target relvar".to_string(),
                ));
            }
            // `q` must be `¬p`: same attribute and value, negated operator.
            let Predicate::AttrCmp {
                attr: pa,
                op: po,
                value: pv,
            } = p;
            let Predicate::AttrCmp {
                attr: qa,
                op: qo,
                value: qv,
            } = q;
            if pa != qa || pv != qv || *qo != po.negate() {
                return Err(BackendError::Other(
                    "update: the union operand is not the complement of the changed rows"
                        .to_string(),
                ));
            }
            (inner, sets)
        }
        // Bare substitute over `t` → update every row (no WHERE).
        _ => {
            let (inner, sets) = peel_substitute(rhs).ok_or_else(|| {
                BackendError::Other(
                    "unrecognized assignment RHS shape (not a minus/union/update over the target)"
                        .to_string(),
                )
            })?;
            if delete_base_table(inner) != Some(t) {
                return Err(BackendError::Other(
                    "update: substitute is not rooted in the target relvar".to_string(),
                ));
            }
            (inner, sets)
        }
    };

    // Resolve the inner to its table + column map, collecting the WHERE predicate
    // (`p`, or none for update-all).
    let mut wheres: Vec<(String, CmpOp, Literal)> = Vec::new();
    let (from_clause, cols) = resolve(inner, &mut wheres)?;

    let mut set_sql = Vec::with_capacity(sets.len());
    for &(target_attr, scalar) in &sets {
        set_sql.push(format!(
            "{} = {}",
            quote_ident(column_for(&cols, target_attr)?),
            render_scalar(scalar, &cols)?
        ));
    }
    if set_sql.is_empty() {
        return Err(BackendError::Other("update: empty SET list".to_string()));
    }

    let mut params: Vec<Value> = Vec::new();
    let where_sql = render_where_clause(&wheres, dialect, 0, &mut params);
    let text = format!("UPDATE {from_clause} SET {}{where_sql}", set_sql.join(", "));
    let plan_id = PlanId(fnv1a(dialect, &text));
    Ok(SqlQuery {
        sql: SqlString {
            param_count: params.len() as u32,
            text,
        },
        params,
        result_heading: target.heading(),
        plan_id,
    })
}

/// Placeholder the runtime replaces with a batch of `VALUES` row-groups in an
/// insert template (see [`emit_insert_template`]). A token that cannot occur in
/// real SQL, so the substitution is unambiguous.
pub const INSERT_ROWS_MARKER: &str = "__CODDL_ROW_VALUES__";

/// Emit the **insert template** for the in-memory `union` path: `t := t union
/// <in-memory e>`, where `e`'s rows are shipped from the process at runtime
/// rather than pushed as SQL (a relation literal, or a private relvar). The
/// template carries the [`INSERT_ROWS_MARKER`] inside `(VALUES …)`, which the
/// runtime expands to one `(?,…)` group per source row (in batches); the
/// idempotent `NOT EXISTS` merge and the column projection are fixed here at
/// compile time. Same set / Golden-Rule semantics as [`emit_idempotent_insert`]
/// — only the row source differs (a bound `VALUES` list vs. a pushed sub-SELECT).
///
/// The `(VALUES …) AS v` derived table exposes positional columns `column1…N`
/// on both SQLite and Postgres; the projection and correlation reference those.
pub fn emit_insert_template(target: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    let RelExpr::RelvarRef {
        table_name,
        columns,
        ..
    } = target
    else {
        return Err(BackendError::Other(
            "insert target is not a base relvar".to_string(),
        ));
    };
    let insert_cols: Vec<String> = columns.iter().map(|(_, phys)| quote_ident(phys)).collect();
    let select_cols: Vec<String> = (1..=columns.len()).map(|i| format!("v.column{i}")).collect();
    let conjuncts: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, (_, phys))| {
            format!(
                "{}.{} = v.column{}",
                quote_ident(table_name),
                quote_ident(phys),
                i + 1
            )
        })
        .collect();
    let text = format!(
        "INSERT INTO {} ({}) SELECT {} FROM (VALUES {}) AS v WHERE NOT EXISTS (SELECT 1 FROM {} WHERE {})",
        quote_ident(table_name),
        insert_cols.join(", "),
        select_cols.join(", "),
        INSERT_ROWS_MARKER,
        quote_ident(table_name),
        conjuncts.join(" AND ")
    );
    let plan_id = PlanId(fnv1a(dialect, &text));
    Ok(SqlQuery {
        sql: SqlString {
            param_count: 0,
            text,
        },
        params: Vec::new(),
        result_heading: target.heading(),
        plan_id,
    })
}

/// Emit the anti-join `DELETE` for `t := t minus X` where `X` is a pushable,
/// same-heading relation that is *not* rooted in `t` (the
/// `t := t minus other_relvar` shape). Deletes every tuple of `t` that also
/// appears in `X`. Tuple equality is on **all** attributes (RM Pre 8) and there
/// are no nulls (RM Pro 4), so the correlation compares every column with `=`
/// inside an `EXISTS` — never an outer join.
///
/// `X` is rendered by [`emit_select`] as a derived table whose output columns
/// are the Coddl attribute names; the target side correlates its own physical
/// columns. A non-pushable `X` (e.g. an in-memory `MaterializedRelvar`) makes
/// `emit_select` `Err`, which propagates so the assignment declines.
fn emit_anti_join_delete(target: &RelExpr, x: &RelExpr, dialect: Dialect) -> Result<SqlQuery> {
    let RelExpr::RelvarRef {
        table_name,
        columns,
        ..
    } = target
    else {
        return Err(BackendError::Other(
            "anti-join delete target is not a base relvar".to_string(),
        ));
    };
    let sub = emit_select(x, dialect)?;
    // Correlate every attribute: the target's physical column against the
    // derived table's attribute-named column. `columns` is `(attr, phys)` in
    // canonical (name-sorted) order, so the conjunction is deterministic.
    let conjuncts: Vec<String> = columns
        .iter()
        .map(|(attr, phys)| {
            format!(
                "{}.{} = coddl_anti.{}",
                quote_ident(table_name),
                quote_ident(phys),
                quote_ident(attr)
            )
        })
        .collect();
    let text = format!(
        "DELETE FROM {} WHERE EXISTS (SELECT 1 FROM ({}) AS coddl_anti WHERE {})",
        quote_ident(table_name),
        sub.sql.text,
        conjuncts.join(" AND ")
    );
    let plan_id = PlanId(fnv1a(dialect, &text));
    Ok(SqlQuery {
        sql: SqlString {
            param_count: sub.params.len() as u32,
            text,
        },
        params: sub.params,
        result_heading: target.heading(),
        plan_id,
    })
}

/// The base relvar a DELETE subtrahend targets — `RelvarRef`, optionally under
/// `Restrict` layers. `None` for any other shape (`Project`/`Rename`/`And`/
/// set-op), which is not a surgical single-table DELETE.
fn delete_base_table(expr: &RelExpr) -> Option<&str> {
    match expr {
        RelExpr::RelvarRef { table_name, .. } => Some(table_name),
        RelExpr::Restrict { input, .. } => delete_base_table(input),
        _ => None,
    }
}

/// `emit_select` with a bind-parameter start offset, threaded so a set-op's
/// right operand numbers its Postgres `$N` placeholders after the left's. The
/// public entry passes `0`.
fn emit_select_offset(expr: &RelExpr, dialect: Dialect, param_offset: u32) -> Result<SqlQuery> {
    // Root set-op: each operand emits as a full sub-SELECT, combined with a bare
    // compound operator (`UNION` for `Or`, `EXCEPT` for `Minus`; set semantics,
    // never `… ALL`). Params concatenate (lhs then rhs); the rhs's placeholders
    // start after the lhs's. CORRESPONDING is free — both sides emit
    // canonical-sorted SELECT lists over identical headings (typechecked), so
    // columns align by position. A set-op nested *under* a relational op never
    // reaches here: `resolve` errs on `Or`/`Minus`, so `cut::try_push` declines
    // and it runs in-process.
    let set_op = match expr {
        RelExpr::Or { lhs, rhs } => Some(("UNION", lhs, rhs)),
        RelExpr::Minus { lhs, rhs } => Some(("EXCEPT", lhs, rhs)),
        _ => None,
    };
    if let Some((op, lhs, rhs)) = set_op {
        let l = emit_select_offset(lhs, dialect, param_offset)?;
        let r = emit_select_offset(rhs, dialect, param_offset + l.sql.param_count)?;
        // Unparenthesized compound SELECT: `… UNION/EXCEPT …`. SQLite rejects
        // parenthesized operands in a compound query (`(SELECT …) UNION …` is a
        // syntax error); the bare form is valid in both SQLite and Postgres and
        // is associative, so nested root set-ops chain correctly.
        let text = format!("{} {op} {}", l.sql.text, r.sql.text);
        let mut params = l.params;
        params.extend(r.params);
        let plan_id = PlanId(fnv1a(dialect, &text));
        return Ok(SqlQuery {
            sql: SqlString {
                param_count: params.len() as u32,
                text,
            },
            params,
            // `UNION`: identical operand headings (typechecked). `EXCEPT`: the
            // result is the lhs's rows. Either way the lhs heading is correct.
            result_heading: l.result_heading,
            plan_id,
        });
    }

    // A computed-column chain is handled here (like root set-ops / `tclose`):
    // peel a root `Rename?` over `Project?` over `Extend`, resolve the Extend's
    // input, render each computed attribute, then replay the project/rename
    // wrappers onto the column map. This is exactly the shape a general
    // `replace` desugars to (`extend → project all-but → rename`) plus a bare
    // root `extend`. `resolve` never sees the `Extend` — a *genuinely* nested
    // extend (under `Restrict`/`And`/…) is NOT peeled, so its `resolve` arm
    // still errs and the push declines.
    let mut core = expr;
    let mut peeled_rename: Option<&[(String, String)]> = None;
    let mut peeled_keep: Option<&[String]> = None;
    let mut extends: &[(String, coddl_relir::Type, ScalarExpr)] = &[];
    {
        let mut n = expr;
        let mut rename = None;
        let mut keep = None;
        if let RelExpr::Rename { input, renames } = n {
            rename = Some(renames.as_slice());
            n = input;
        }
        if let RelExpr::Project { input, keep: k } = n {
            keep = Some(k.as_slice());
            n = input;
        }
        // Commit the peel only if the chain bottoms out in an Extend; otherwise
        // leave `core = expr` so `resolve` handles it (and declines a nested
        // extend) exactly as before.
        if let RelExpr::Extend { input, extends: e } = n {
            peeled_rename = rename;
            peeled_keep = keep;
            extends = e.as_slice();
            core = input;
        }
    }

    // Resolve the core to its physical table and output `(attr, column)` map —
    // renames remap the attr side, projects narrow it — collecting each
    // restriction as a resolved `(column, value)` conjunct along the way.
    let mut wheres: Vec<(String, CmpOp, Literal)> = Vec::new();
    let (from_clause, mut output_cols) = resolve(core, &mut wheres)?;

    // Computed columns: each extend value rendered to SQL against the resolved
    // column map (`attr` → its rendered SQL expression).
    let mut computed: Vec<(String, String)> = Vec::with_capacity(extends.len());
    for (name, _ty, value) in extends {
        computed.push((name.clone(), render_scalar(value, &output_cols)?));
    }
    // Replay the peeled `project all-but` (retain kept attrs) and `rename`
    // (remap attr keys) onto both physical and computed columns.
    if let Some(keep) = peeled_keep {
        output_cols.retain(|(a, _)| keep.iter().any(|k| k == a));
        computed.retain(|(a, _)| keep.iter().any(|k| k == a));
    }
    if let Some(renames) = peeled_rename {
        let remap = |a: &str| {
            renames
                .iter()
                .find(|(old, _)| old == a)
                .map(|(_, new)| new.clone())
                .unwrap_or_else(|| a.to_string())
        };
        for (a, _) in output_cols.iter_mut() {
            *a = remap(a);
        }
        for (a, _) in computed.iter_mut() {
            *a = remap(a);
        }
    }

    // SELECT list = the result heading in canonical order. Each attribute is
    // either a computed extend value (`(<expr>) AS "c"`) or a physical column,
    // aliased `AS` the attribute when the names differ — a `rename`, or any
    // attr/column mismatch — so the rename is pushed to SQL (the result columns
    // are named by the Coddl attributes).
    let heading = expr.heading();
    let mut select_cols = Vec::with_capacity(heading.attrs().len());
    for (attr, ty) in heading.attrs() {
        if let Some((_, sql)) = computed.iter().find(|(a, _)| a == attr) {
            select_cols.push(format!("{sql} AS {}", quote_ident(attr)));
        } else {
            // A `Tuple`-valued attribute (from `wrap`) flattens to its leaf
            // columns — the SQL has no composite column; the nesting lives in
            // the result descriptor, which the runtime reconstructs. Depth-first
            // name-sorted, matching `record_layout`'s leaf order (the runtime
            // maps result columns to record cells by position).
            push_leaf_cols(attr, ty, &output_cols, &mut select_cols)?;
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
    text.push_str(&render_where_clause(&wheres, dialect, param_offset, &mut params));

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
    wheres: &mut Vec<(String, CmpOp, Literal)>,
) -> Result<(String, Vec<(String, String)>)> {
    match expr {
        RelExpr::RelvarRef {
            table_name,
            columns,
            ..
        } => Ok((quote_ident(table_name), columns.clone())),
        RelExpr::Restrict { input, pred } => {
            let (from, cols) = resolve(input, wheres)?;
            let Predicate::AttrCmp { attr, op, value } = pred;
            let col = column_for(&cols, attr)?.to_string();
            wheres.push((col, *op, value.clone()));
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
        // A set-op nested under a relational op (e.g. `(A union B) where p`).
        // `resolve` produces a FROM clause, but a `UNION` isn't a table
        // reference; rather than emit a subquery (out of scope — see
        // sqlemit.md), err so `cut::try_push` declines and the whole expression
        // runs in-process. A *root* `Or` is handled in `emit_select_offset`
        // before `resolve` is ever called.
        RelExpr::Or { .. } | RelExpr::Minus { .. } => Err(BackendError::Other(
            "set operation nested under a relational operator does not push to SQL".to_string(),
        )),
        // A `TClose` reached *here* (via `resolve`) is non-root — nested under a
        // relational op (`(R tclose) where p`), or a set-op operand
        // (`(R tclose) union S`). A `WITH RECURSIVE` query can't be a `FROM`
        // table-expression or a compound operand, so err: the whole push
        // declines and the expression decomposes in-process (each closure
        // pushes its own `WITH RECURSIVE`, the surrounding op runs in process).
        // A *root* `TClose` never reaches here — `emit_select` emits its
        // `WITH RECURSIVE` before delegating to `emit_select_offset`/`resolve`.
        RelExpr::TClose { .. } => Err(BackendError::Other(
            "transitive closure (tclose) nested under another operator does not push to SQL"
                .to_string(),
        )),
        // A *root* `Extend` is peeled in `emit_select_offset` before `resolve`
        // is called; reaching here means it's nested under another relational
        // operator. A computed column isn't a table reference, so decline: the
        // whole push declines and the expression runs in-process.
        RelExpr::Extend { .. } => Err(BackendError::Other(
            "extend nested under another operator does not push to SQL".to_string(),
        )),
        // wrap/unwrap restructure the heading only — the underlying SQL columns
        // are the flat leaf columns, unchanged. Pass the operand's (from, cols)
        // through; `emit_select` reads the restructured (nested) heading from
        // `expr.heading()` and flattens its Tuple attrs to leaf columns, and the
        // result descriptor carries the nesting for the runtime to reconstruct.
        RelExpr::Wrap { input, .. } | RelExpr::Unwrap { input, .. } => resolve(input, wheres),
        // Never reached: a materialized leaf fails the cut's `RelvarRooted`
        // gate before SQL emission. Defensive only.
        RelExpr::MaterializedRelvar { name, .. } => Err(BackendError::Other(format!(
            "materialized relvar `{name}` reached SQL emission (should be in-process)"
        ))),
    }
}

/// Render a [`ScalarExpr`] (an `extend` value) to a SQL expression, resolving
/// each attribute reference to its quoted physical column via `cols`. Integer
/// `/` is SQLite/Postgres integer division (`5 / 2 = 2`); `||` is SQL string
/// concatenation. Literals are inlined (no bind params): an `extend`'s value
/// is part of the SELECT list, which precedes the WHERE clause, so inlining
/// keeps the positional `?`/`$n` numbering of the restrict params intact.
fn render_scalar(e: &ScalarExpr, cols: &[(String, String)]) -> Result<String> {
    Ok(match e {
        ScalarExpr::Attr(name) => quote_ident(column_for(cols, name)?),
        ScalarExpr::Int(n) => n.to_string(),
        ScalarExpr::Str(s) => format!("'{}'", s.replace('\'', "''")),
        ScalarExpr::Char(cp) => {
            let c = char::from_u32(*cp).unwrap_or('\u{FFFD}');
            format!("'{}'", c.to_string().replace('\'', "''"))
        }
        ScalarExpr::Bin { op, lhs, rhs } => {
            let sym = match op {
                ScalarBinOp::Add => "+",
                ScalarBinOp::Sub => "-",
                ScalarBinOp::Mul => "*",
                ScalarBinOp::Div => "/",
                ScalarBinOp::Concat => "||",
            };
            format!(
                "({} {sym} {})",
                render_scalar(lhs, cols)?,
                render_scalar(rhs, cols)?
            )
        }
    })
}

/// The SQL column an attribute maps to, per the relvar's `(attr, column)` map.
fn column_for<'a>(columns: &'a [(String, String)], attr: &str) -> Result<&'a str> {
    columns
        .iter()
        .find(|(a, _)| a == attr)
        .map(|(_, c)| c.as_str())
        .ok_or_else(|| BackendError::Other(format!("no column mapping for attribute `{attr}`")))
}

/// Push the SELECT column(s) for one result attribute. A scalar/`Text` attribute
/// emits one column (bare `"col"`, or `"col" AS "attr"` when they differ — a
/// rename). A `Tuple`-valued attribute (from `wrap`) has no SQL column of its
/// own; it flattens to its components' leaf columns, recursing depth-first in
/// the sub-heading's canonical (name-sorted) order. This matches `record_layout`'s
/// leaf order, so the runtime's positional column→cell mapping reconstructs the
/// inline nested cell.
/// Render a `WHERE col <op> ? AND …` clause (leading space included) from the
/// collected `(column, op, literal)` comparison tests, appending each bind value
/// to `params`. Each test renders its own comparison operator (`=`, `<>`, `<`,
/// `<=`, `>`, `>=`). `param_offset` is the Postgres `$N` base (SQLite uses `?`
/// and ignores it). Returns the empty string when there are no tests — so a
/// caller can unconditionally append the result. Shared by `emit_select_offset`
/// (SELECT) and `emit_delete` (DELETE); the conjunction shape is identical.
fn render_where_clause(
    wheres: &[(String, CmpOp, Literal)],
    dialect: Dialect,
    param_offset: u32,
    params: &mut Vec<Value>,
) -> String {
    if wheres.is_empty() {
        return String::new();
    }
    let mut conjuncts = Vec::with_capacity(wheres.len());
    for (col, op, value) in wheres {
        let placeholder = match dialect {
            Dialect::SQLite => "?".to_string(),
            Dialect::Postgres => format!("${}", param_offset as usize + params.len() + 1),
        };
        conjuncts.push(format!("{} {} {placeholder}", quote_ident(col), op.sql()));
        params.push(Value::from(value.clone()));
    }
    format!(" WHERE {}", conjuncts.join(" AND "))
}

fn push_leaf_cols(
    attr: &str,
    ty: &Type,
    columns: &[(String, String)],
    out: &mut Vec<String>,
) -> Result<()> {
    match ty {
        Type::Tuple(sub) => {
            for (name, sub_ty) in sub.attrs() {
                push_leaf_cols(name, sub_ty, columns, out)?;
            }
            Ok(())
        }
        _ => {
            let col = column_for(columns, attr)?;
            if col == attr {
                out.push(quote_ident(col));
            } else {
                out.push(format!("{} AS {}", quote_ident(col), quote_ident(attr)));
            }
            Ok(())
        }
    }
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
            pred: Predicate::AttrCmp {
                op: CmpOp::Eq,
                attr: "id".to_string(),
                value: Literal::Integer(1),
            },
        }
    }

    /// `Greetings where <attr> <op> <value>`.
    fn restrict_cmp(attr: &str, op: CmpOp, value: Literal) -> RelExpr {
        RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrCmp {
                attr: attr.to_string(),
                op,
                value,
            },
        }
    }

    #[test]
    fn restrict_comparison_ops_render_their_symbols() {
        // Every scalar comparison pushes; the WHERE renders its own operator.
        // A full-heading read keeps the key `id`, so the result is already a set
        // (no `DISTINCT`) regardless of the operator.
        for (op, sym) in [
            (CmpOp::Ne, "<>"),
            (CmpOp::Lt, "<"),
            (CmpOp::LtEq, "<="),
            (CmpOp::Gt, ">"),
            (CmpOp::GtEq, ">="),
        ] {
            let q = emit_select(&restrict_cmp("id", op, Literal::Integer(3)), Dialect::SQLite)
                .unwrap();
            assert_eq!(
                q.sql.text,
                format!(r#"SELECT "id", "message" FROM "greetings" WHERE "id" {sym} ?"#),
                "op {op:?}"
            );
            assert_eq!(q.params, vec![Value::Integer(3)], "op {op:?}");
        }
    }

    #[test]
    fn restrict_ne_uses_postgres_placeholder() {
        let q = emit_select(&restrict_cmp("id", CmpOp::Ne, Literal::Integer(1)), Dialect::Postgres)
            .unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" <> $1"#
        );
    }

    #[test]
    fn delete_with_ne_predicate_renders() {
        // The WHERE renderer is shared, so surgical DELETE gets `<>` for free.
        let q = emit_delete(&restrict_cmp("id", CmpOp::Ne, Literal::Integer(1)), Dialect::SQLite)
            .unwrap();
        assert_eq!(q.sql.text, r#"DELETE FROM "greetings" WHERE "id" <> ?"#);
    }

    #[test]
    fn ordering_keeps_distinct_when_projection_drops_the_key() {
        // Project away the key `id`: `=` pins the key (card ≤ 1) so the
        // projection is still a set (no DISTINCT); `<>`/`<`/`>` bound nothing, so
        // DISTINCT is required.
        let project_message = |r: RelExpr| RelExpr::Project {
            input: Box::new(r),
            keep: vec!["message".to_string()],
        };
        let eq = emit_select(
            &project_message(restrict_cmp("id", CmpOp::Eq, Literal::Integer(1))),
            Dialect::SQLite,
        )
        .unwrap();
        assert_eq!(
            eq.sql.text,
            r#"SELECT "message" FROM "greetings" WHERE "id" = ?"#
        );
        let lt = emit_select(
            &project_message(restrict_cmp("id", CmpOp::Lt, Literal::Integer(3))),
            Dialect::SQLite,
        )
        .unwrap();
        assert_eq!(
            lt.sql.text,
            r#"SELECT DISTINCT "message" FROM "greetings" WHERE "id" < ?"#
        );
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
            pred: Predicate::AttrCmp { op: CmpOp::Eq,
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

    /// `t := t minus <subtrahend>` builder — the shape `emit_assignment`
    /// recognizes. `minuend` is always the bare target relvar.
    fn minus(minuend: RelExpr, subtrahend: RelExpr) -> RelExpr {
        RelExpr::Minus {
            lhs: Box::new(minuend),
            rhs: Box::new(subtrahend),
        }
    }

    #[test]
    fn assignment_minus_where_emits_surgical_delete_sqlite() {
        // `Greetings := Greetings minus (Greetings where id = 1)` → a single-row
        // DELETE; the WHERE is identical to the one `SELECT … WHERE` builds for
        // the same restriction.
        let rhs = minus(greetings(), where_id_1(greetings()));
        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert_eq!(q.sql.text, r#"DELETE FROM "greetings" WHERE "id" = ?"#);
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Integer(1)]);
    }

    #[test]
    fn assignment_minus_where_uses_postgres_placeholder() {
        let rhs = minus(greetings(), where_id_1(greetings()));
        let q = emit_assignment(&greetings(), &rhs, Dialect::Postgres).unwrap();
        assert_eq!(q.sql.text, r#"DELETE FROM "greetings" WHERE "id" = $1"#);
        assert_eq!(q.sql.param_count, 1);
    }

    #[test]
    fn assignment_self_subtraction_truncates() {
        // `Greetings := Greetings minus Greetings` → DELETE every row.
        let rhs = minus(greetings(), greetings());
        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert_eq!(q.sql.text, r#"DELETE FROM "greetings""#);
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
    }

    #[test]
    fn assignment_declines_unrecognized_rhs() {
        // Recognized shapes are `minus`/`union` with the target as an operand; a
        // `join` (`And`) RHS is neither, so it declines.
        let rhs = RelExpr::And {
            lhs: Box::new(greetings()),
            rhs: Box::new(employees()),
        };
        assert!(emit_assignment(&greetings(), &rhs, Dialect::SQLite).is_err());
    }

    /// A second relvar with the *same* heading as `greetings`, bound to table
    /// `stale` — the right operand of an anti-join `minus`.
    fn stale() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Stale".to_string(),
            database: "greetings".to_string(),
            heading: greetings_heading(),
            table_name: "stale".to_string(),
            columns: vec![
                ("id".to_string(), "id".to_string()),
                ("message".to_string(), "message".to_string()),
            ],
            keys: vec![vec!["id".to_string()]],
        }
    }

    #[test]
    fn assignment_minus_other_relvar_emits_anti_join() {
        // `Greetings := Greetings minus Stale` deletes every Greetings tuple that
        // also appears in Stale — correlated on *all* attributes (RM Pre 8), via
        // EXISTS (RM Pro 4: no outer join).
        let rhs = minus(greetings(), stale());
        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"DELETE FROM "greetings" WHERE EXISTS (SELECT 1 FROM (SELECT "id", "message" FROM "stale") AS coddl_anti WHERE "greetings"."id" = coddl_anti."id" AND "greetings"."message" = coddl_anti."message")"#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
    }

    #[test]
    fn assignment_anti_join_against_restricted_relvar_binds_param() {
        // The right operand can be any pushable same-heading relation; a
        // restricted one threads its bind param through the derived table.
        let x = RelExpr::Restrict {
            input: Box::new(stale()),
            pred: Predicate::AttrCmp { op: CmpOp::Eq,
                attr: "id".to_string(),
                value: Literal::Integer(2),
            },
        };
        let rhs = minus(greetings(), x);

        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"DELETE FROM "greetings" WHERE EXISTS (SELECT 1 FROM (SELECT "id", "message" FROM "stale" WHERE "id" = ?) AS coddl_anti WHERE "greetings"."id" = coddl_anti."id" AND "greetings"."message" = coddl_anti."message")"#
        );
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Integer(2)]);

        // Postgres numbers the derived table's placeholder from $1 (nothing
        // precedes it in the DELETE).
        let pg = emit_assignment(&greetings(), &rhs, Dialect::Postgres).unwrap();
        assert!(
            pg.sql.text.contains(r#"WHERE "id" = $1"#),
            "expected $1 placeholder, got: {}",
            pg.sql.text
        );
    }

    #[test]
    fn assignment_anti_join_declines_non_pushable_operand() {
        // An in-memory (private) relation has no SQL source, so the anti-join
        // can't be pushed — the assignment declines.
        let priv_rel = RelExpr::MaterializedRelvar {
            name: "Local".to_string(),
            heading: greetings_heading(),
        };
        let rhs = minus(greetings(), priv_rel);
        assert!(emit_assignment(&greetings(), &rhs, Dialect::SQLite).is_err());
    }

    /// A second relvar with the same heading as `greetings`, bound to table
    /// `new_arrivals` — the right operand of a `union` insert.
    fn new_arrivals() -> RelExpr {
        RelExpr::RelvarRef {
            name: "NewArrivals".to_string(),
            database: "greetings".to_string(),
            heading: greetings_heading(),
            table_name: "new_arrivals".to_string(),
            columns: vec![
                ("id".to_string(), "id".to_string()),
                ("message".to_string(), "message".to_string()),
            ],
            keys: vec![vec!["id".to_string()]],
        }
    }

    fn union(a: RelExpr, b: RelExpr) -> RelExpr {
        RelExpr::Or {
            lhs: Box::new(a),
            rhs: Box::new(b),
        }
    }

    #[test]
    fn assignment_union_other_relvar_emits_idempotent_insert() {
        // `Greetings := Greetings union NewArrivals` inserts every NewArrivals
        // tuple not already in Greetings — NOT EXISTS on all attributes keeps it
        // a set (identical tuple is a no-op; a key-clash hits the PK and errors).
        let rhs = union(greetings(), new_arrivals());
        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"INSERT INTO "greetings" ("id", "message") SELECT coddl_src."id", coddl_src."message" FROM (SELECT "id", "message" FROM "new_arrivals") AS coddl_src WHERE NOT EXISTS (SELECT 1 FROM "greetings" WHERE "greetings"."id" = coddl_src."id" AND "greetings"."message" = coddl_src."message")"#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
    }

    #[test]
    fn assignment_union_is_commutative_target_on_right() {
        // Union is commutative, so `Greetings := NewArrivals union Greetings`
        // recognizes identically (target is the right operand).
        let rhs = union(new_arrivals(), greetings());
        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert!(
            q.sql.text.starts_with(r#"INSERT INTO "greetings" ("id", "message")"#),
            "got: {}",
            q.sql.text
        );
    }

    #[test]
    fn assignment_union_restricted_relvar_binds_param() {
        let x = RelExpr::Restrict {
            input: Box::new(new_arrivals()),
            pred: Predicate::AttrCmp { op: CmpOp::Eq,
                attr: "id".to_string(),
                value: Literal::Integer(5),
            },
        };
        let rhs = union(greetings(), x);
        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert!(q.sql.text.contains(r#"WHERE "id" = ?) AS coddl_src"#), "got: {}", q.sql.text);
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Integer(5)]);
    }

    #[test]
    fn assignment_union_declines_non_pushable_operand() {
        // A relation literal / private relation isn't pushable — declines here
        // (the in-memory path ships those rows at runtime).
        let priv_rel = RelExpr::MaterializedRelvar {
            name: "Local".to_string(),
            heading: greetings_heading(),
        };
        let rhs = union(greetings(), priv_rel);
        assert!(emit_assignment(&greetings(), &rhs, Dialect::SQLite).is_err());
    }

    #[test]
    fn insert_template_marks_the_values_rows() {
        // The in-memory `union` template: a fixed idempotent merge whose VALUES
        // rows the runtime substitutes for the marker, projecting/correlating the
        // derived table's positional columns.
        let q = emit_insert_template(&greetings(), Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"INSERT INTO "greetings" ("id", "message") SELECT v.column1, v.column2 FROM (VALUES __CODDL_ROW_VALUES__) AS v WHERE NOT EXISTS (SELECT 1 FROM "greetings" WHERE "greetings"."id" = v.column1 AND "greetings"."message" = v.column2)"#
        );
        assert!(q.sql.text.contains(INSERT_ROWS_MARKER));
    }

    /// `(inner) replace { message: message || "!" }` desugar shape — the
    /// heading-preserving substitute chain `Rename(Project(Extend(inner)))` that
    /// sets `message` in place (extend a temp, drop `message`, rename temp back).
    fn substitute_message_bang(inner: RelExpr) -> RelExpr {
        let tmp = "__coddl_replace_tmp_message".to_string();
        RelExpr::Rename {
            input: Box::new(RelExpr::Project {
                input: Box::new(RelExpr::Extend {
                    input: Box::new(inner),
                    extends: vec![(
                        tmp.clone(),
                        Type::Text,
                        ScalarExpr::Bin {
                            op: ScalarBinOp::Concat,
                            lhs: Box::new(ScalarExpr::Attr("message".to_string())),
                            rhs: Box::new(ScalarExpr::Str("!".to_string())),
                        },
                    )],
                }),
                keep: vec!["id".to_string(), tmp.clone()],
            }),
            renames: vec![(tmp, "message".to_string())],
        }
    }

    #[test]
    fn assignment_update_with_where_emits_update() {
        // `Greetings := (Greetings where id <> 1) union ((Greetings where id = 1)
        // replace { message: message || "!" })` → an UPDATE of the matching row.
        let rhs = union(
            restrict_cmp("id", CmpOp::Ne, Literal::Integer(1)),
            substitute_message_bang(restrict_cmp("id", CmpOp::Eq, Literal::Integer(1))),
        );
        let q = emit_assignment(&greetings(), &rhs, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"UPDATE "greetings" SET "message" = ("message" || '!') WHERE "id" = ?"#
        );
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Integer(1)]);
    }

    #[test]
    fn assignment_update_with_where_uses_postgres_placeholder() {
        let rhs = union(
            restrict_cmp("id", CmpOp::Ne, Literal::Integer(1)),
            substitute_message_bang(restrict_cmp("id", CmpOp::Eq, Literal::Integer(1))),
        );
        let q = emit_assignment(&greetings(), &rhs, Dialect::Postgres).unwrap();
        assert_eq!(
            q.sql.text,
            r#"UPDATE "greetings" SET "message" = ("message" || '!') WHERE "id" = $1"#
        );
    }

    #[test]
    fn assignment_update_all_has_no_where() {
        // A bare substitute over the relvar (no complement/union) updates every
        // row — `Greetings := Greetings replace { message: message || "!" }`.
        let q = emit_assignment(&greetings(), &substitute_message_bang(greetings()), Dialect::SQLite)
            .unwrap();
        assert_eq!(
            q.sql.text,
            r#"UPDATE "greetings" SET "message" = ("message" || '!')"#
        );
        assert!(q.params.is_empty());
    }

    #[test]
    fn assignment_update_declines_non_complementary_union() {
        // The "unchanged" operand must be the exact complement: `id < 1` is not
        // `¬(id = 1)` (that's `id <> 1`), so this is not a recognized update.
        let rhs = union(
            restrict_cmp("id", CmpOp::Lt, Literal::Integer(1)),
            substitute_message_bang(restrict_cmp("id", CmpOp::Eq, Literal::Integer(1))),
        );
        assert!(emit_assignment(&greetings(), &rhs, Dialect::SQLite).is_err());
    }

    #[test]
    fn assignment_declines_non_relvar_target() {
        // A composed target (not a single base table) can never be a surgical
        // write target.
        let target = RelExpr::And {
            lhs: Box::new(greetings()),
            rhs: Box::new(employees()),
        };
        let rhs = minus(greetings(), where_id_1(greetings()));
        assert!(emit_assignment(&target, &rhs, Dialect::SQLite).is_err());
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

    fn morning() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Morning".to_string(),
            database: "shifts".to_string(),
            heading: Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("name".to_string(), Type::Text),
            ]),
            table_name: "morning".to_string(),
            columns: vec![
                ("id".to_string(), "id".to_string()),
                ("name".to_string(), "name".to_string()),
            ],
            keys: vec![vec!["id".to_string()]],
        }
    }

    fn evening() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Evening".to_string(),
            database: "shifts".to_string(),
            heading: Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("name".to_string(), Type::Text),
            ]),
            table_name: "evening".to_string(),
            columns: vec![
                ("id".to_string(), "id".to_string()),
                ("name".to_string(), "name".to_string()),
            ],
            keys: vec![vec!["id".to_string()]],
        }
    }

    #[test]
    fn intersect_emits_inner_join_using_all_columns() {
        // `Morning intersect Evening` → RelExpr::And on identical headings, so
        // every column is shared and the join key is all of them, emitted as
        // `INNER JOIN ... USING ("id", "name")`. DISTINCT stays — the join drops
        // both keys (RM Pro 3), like `and_emits_inner_join_using_the_shared_column`.
        let expr = RelExpr::And {
            lhs: Box::new(morning()),
            rhs: Box::new(evening()),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "id", "name" FROM "morning" INNER JOIN "evening" USING ("id", "name")"#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("name".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn union_emits_union_of_selects() {
        // `Morning union Evening` → RelExpr::Or. Each operand emits a full
        // SELECT, combined with bare UNION (set semantics, never UNION ALL). No
        // DISTINCT on the operands — each keeps key `id` — and UNION dedups.
        let expr = RelExpr::Or {
            lhs: Box::new(morning()),
            rhs: Box::new(evening()),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            // Unparenthesized — SQLite rejects parens around compound operands.
            r#"SELECT "id", "name" FROM "morning" UNION SELECT "id", "name" FROM "evening""#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("name".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn union_postgres_offsets_rhs_params() {
        // Postgres `$N` placeholders are statement-global, so the rhs operand's
        // params number after the lhs's: lhs `$1`, rhs `$2`. (SQLite's `?` is
        // positional and needs no offset.)
        let expr = RelExpr::Or {
            lhs: Box::new(where_id_1(morning())),
            rhs: Box::new(where_id_1(evening())),
        };
        let q = emit_select(&expr, Dialect::Postgres).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "name" FROM "morning" WHERE "id" = $1 UNION SELECT "id", "name" FROM "evening" WHERE "id" = $2"#
        );
        assert_eq!(q.sql.param_count, 2);
    }

    #[test]
    fn minus_emits_except_of_selects() {
        // `Morning minus Evening` → RelExpr::Minus. Each operand emits a full
        // SELECT, combined with bare EXCEPT (set difference, dedups). No DISTINCT
        // on the operands — each keeps key `id`.
        let expr = RelExpr::Minus {
            lhs: Box::new(morning()),
            rhs: Box::new(evening()),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "name" FROM "morning" EXCEPT SELECT "id", "name" FROM "evening""#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("name".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn minus_with_where_on_left_pushes_the_param() {
        // `(Morning where id = 1) minus Evening` — the lhs WHERE param precedes
        // the (param-free) rhs; one bind on the left.
        let expr = RelExpr::Minus {
            lhs: Box::new(where_id_1(morning())),
            rhs: Box::new(evening()),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "name" FROM "morning" WHERE "id" = ? EXCEPT SELECT "id", "name" FROM "evening""#
        );
        assert_eq!(q.sql.param_count, 1);
    }

    // ── tclose (WITH RECURSIVE) ──────────────────────────────────────────

    /// A binary same-typed graph relvar `{ from: Integer, to: Integer }` keyed on
    /// both endpoints — the canonical `tclose` operand. (`from` < `to`, so
    /// `attrs[0]` = `from` = source, `attrs[1]` = `to` = target.)
    fn edges() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Edges".to_string(),
            database: "tclose".to_string(),
            heading: Heading::new(vec![
                ("from".to_string(), Type::Integer),
                ("to".to_string(), Type::Integer),
            ]),
            table_name: "edges".to_string(),
            columns: vec![
                ("from".to_string(), "from".to_string()),
                ("to".to_string(), "to".to_string()),
            ],
            keys: vec![vec!["from".to_string(), "to".to_string()]],
        }
    }

    /// A wider bill-of-materials `{ major, minor, qty }` keyed on `{ major, minor }`
    /// — the operand of the brace form `Contains tclose { major, minor }`, which
    /// projects to the two key columns before closing.
    fn contains() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Contains".to_string(),
            database: "tclose".to_string(),
            heading: Heading::new(vec![
                ("major".to_string(), Type::Integer),
                ("minor".to_string(), Type::Integer),
                ("qty".to_string(), Type::Integer),
            ]),
            table_name: "contains".to_string(),
            columns: vec![
                ("major".to_string(), "major".to_string()),
                ("minor".to_string(), "minor".to_string()),
                ("qty".to_string(), "qty".to_string()),
            ],
            keys: vec![vec!["major".to_string(), "minor".to_string()]],
        }
    }

    #[test]
    fn tclose_emits_with_recursive() {
        // `Edges tclose` → a two-CTE `WITH RECURSIVE`: the operand once
        // (`coddl_tc_op`), then the recursive closure (`coddl_tc`) composing on
        // `to = from`. Direction-agnostic: result heading == operand heading.
        let expr = RelExpr::TClose {
            input: Box::new(edges()),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"WITH RECURSIVE coddl_tc_op("from", "to") AS (SELECT "from", "to" FROM "edges"), coddl_tc("from", "to") AS (SELECT "from", "to" FROM coddl_tc_op UNION SELECT coddl_tc."from", coddl_tc_op."to" FROM coddl_tc JOIN coddl_tc_op ON coddl_tc."to" = coddl_tc_op."from") SELECT DISTINCT "from", "to" FROM coddl_tc"#
        );
        assert_eq!(q.sql.param_count, 0);
        assert!(q.params.is_empty());
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("from".to_string(), Type::Integer),
                ("to".to_string(), Type::Integer),
            ])
        );
    }

    #[test]
    fn tclose_braced_operand_projects_first() {
        // `Contains tclose { major, minor }` → TClose over a Project that narrows
        // to the two columns; the operand CTE is the projected SELECT.
        let expr = RelExpr::TClose {
            input: Box::new(RelExpr::Project {
                input: Box::new(contains()),
                keep: vec!["major".to_string(), "minor".to_string()],
            }),
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"WITH RECURSIVE coddl_tc_op("major", "minor") AS (SELECT "major", "minor" FROM "contains"), coddl_tc("major", "minor") AS (SELECT "major", "minor" FROM coddl_tc_op UNION SELECT coddl_tc."major", coddl_tc_op."minor" FROM coddl_tc JOIN coddl_tc_op ON coddl_tc."minor" = coddl_tc_op."major") SELECT DISTINCT "major", "minor" FROM coddl_tc"#
        );
        assert_eq!(q.sql.param_count, 0);
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("major".to_string(), Type::Integer),
                ("minor".to_string(), Type::Integer),
            ])
        );
    }

    #[test]
    fn tclose_with_where_param_appears_once() {
        // `(Edges where from = 1) tclose` — the restricted operand is defined in a
        // single CTE, so its bind parameter appears exactly once (no duplication),
        // both as SQLite `?` and Postgres `$1`.
        let restricted = RelExpr::Restrict {
            input: Box::new(edges()),
            pred: Predicate::AttrCmp { op: CmpOp::Eq,
                attr: "from".to_string(),
                value: Literal::Integer(1),
            },
        };
        let expr = RelExpr::TClose {
            input: Box::new(restricted),
        };

        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(q.sql.param_count, 1);
        assert_eq!(q.params, vec![Value::Integer(1)]);
        assert_eq!(
            q.sql.text.matches('?').count(),
            1,
            "the operand CTE holds the only `?`: {}",
            q.sql.text
        );
        assert!(q
            .sql
            .text
            .starts_with(r#"WITH RECURSIVE coddl_tc_op("from", "to") AS (SELECT "from", "to" FROM "edges" WHERE "from" = ?)"#));

        let pg = emit_select(&expr, Dialect::Postgres).unwrap();
        assert_eq!(pg.sql.param_count, 1);
        assert!(
            pg.sql.text.contains(r#"WHERE "from" = $1"#) && !pg.sql.text.contains("$2"),
            "the operand appears once, so only `$1`: {}",
            pg.sql.text
        );
    }

    #[test]
    fn tclose_union_operand_does_not_push() {
        // `(Edges tclose) union (Edges tclose)` — a `WITH RECURSIVE` query can't
        // be a compound-`UNION` operand, so the whole push must DECLINE (the
        // operand `TClose` reaches `resolve` via `emit_select_offset` and errs).
        // It then decomposes in-process (each closure pushes separately).
        let expr = RelExpr::Or {
            lhs: Box::new(RelExpr::TClose {
                input: Box::new(edges()),
            }),
            rhs: Box::new(RelExpr::TClose {
                input: Box::new(edges()),
            }),
        };
        assert!(
            emit_select(&expr, Dialect::SQLite).is_err(),
            "a tclose as a set-op operand must not push as one query"
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
                pred: Predicate::AttrCmp { op: CmpOp::Eq,
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
        // (Greetings where id = 1) replace {identifier: id, msg: message} —
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
    fn extend_renders_computed_column_as_alias() {
        use coddl_relir::{ScalarBinOp, ScalarExpr};
        // Greetings extend { c: id * id, d: id + 10 } — two computed columns;
        // the key `id` survives so no DISTINCT. Result heading is canonically
        // sorted: c, d, id, message.
        let expr = RelExpr::Extend {
            input: Box::new(greetings()),
            extends: vec![
                (
                    "c".to_string(),
                    Type::Integer,
                    ScalarExpr::Bin {
                        op: ScalarBinOp::Mul,
                        lhs: Box::new(ScalarExpr::Attr("id".to_string())),
                        rhs: Box::new(ScalarExpr::Attr("id".to_string())),
                    },
                ),
                (
                    "d".to_string(),
                    Type::Integer,
                    ScalarExpr::Bin {
                        op: ScalarBinOp::Add,
                        lhs: Box::new(ScalarExpr::Attr("id".to_string())),
                        rhs: Box::new(ScalarExpr::Int(10)),
                    },
                ),
            ],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT ("id" * "id") AS "c", ("id" + 10) AS "d", "id", "message" FROM "greetings""#
        );
        assert_eq!(
            q.result_heading,
            Heading::new(vec![
                ("c".to_string(), Type::Integer),
                ("d".to_string(), Type::Integer),
                ("id".to_string(), Type::Integer),
                ("message".to_string(), Type::Text),
            ])
        );
    }

    #[test]
    fn extend_nested_under_restrict_declines_push() {
        use coddl_relir::{ScalarBinOp, ScalarExpr};
        // A restrict *over* an extend isn't a root extend, so it declines the
        // push (runs in-process) rather than emitting wrong SQL.
        let extended = RelExpr::Extend {
            input: Box::new(greetings()),
            extends: vec![(
                "c".to_string(),
                Type::Integer,
                ScalarExpr::Bin {
                    op: ScalarBinOp::Add,
                    lhs: Box::new(ScalarExpr::Attr("id".to_string())),
                    rhs: Box::new(ScalarExpr::Int(1)),
                },
            )],
        };
        let expr = where_id_1(extended);
        assert!(emit_select(&expr, Dialect::SQLite).is_err());
    }

    #[test]
    fn replace_collapse_desugar_pushes_computed_column() {
        use coddl_relir::{ScalarBinOp, ScalarExpr};
        // `Greetings replace { c: id * id }` desugars to
        // Project all-but {id} over Extend {c: id*id}: the computed column
        // pushes and `id` (consumed) is absent. No temp/rename (c ∉ heading).
        let extended = RelExpr::Extend {
            input: Box::new(greetings()),
            extends: vec![(
                "c".to_string(),
                Type::Integer,
                ScalarExpr::Bin {
                    op: ScalarBinOp::Mul,
                    lhs: Box::new(ScalarExpr::Attr("id".to_string())),
                    rhs: Box::new(ScalarExpr::Attr("id".to_string())),
                },
            )],
        };
        let expr = RelExpr::Project {
            input: Box::new(extended),
            keep: vec!["message".to_string(), "c".to_string()],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT ("id" * "id") AS "c", "message" FROM "greetings""#
        );
    }

    #[test]
    fn replace_in_place_desugar_pushes_via_temp_rename() {
        use coddl_relir::{ScalarBinOp, ScalarExpr};
        // `Greetings replace { id: id + 1 }` desugars to Rename {__t → id} over
        // Project all-but {id} over Extend {__t: id+1}: the computed column is
        // aliased back to `id`.
        let extended = RelExpr::Extend {
            input: Box::new(greetings()),
            extends: vec![(
                "__t".to_string(),
                Type::Integer,
                ScalarExpr::Bin {
                    op: ScalarBinOp::Add,
                    lhs: Box::new(ScalarExpr::Attr("id".to_string())),
                    rhs: Box::new(ScalarExpr::Int(1)),
                },
            )],
        };
        let projected = RelExpr::Project {
            input: Box::new(extended),
            keep: vec!["message".to_string(), "__t".to_string()],
        };
        let expr = RelExpr::Rename {
            input: Box::new(projected),
            renames: vec![("__t".to_string(), "id".to_string())],
        };
        let q = emit_select(&expr, Dialect::SQLite).unwrap();
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT ("id" + 1) AS "id", "message" FROM "greetings""#
        );
    }

    #[test]
    fn where_above_rename_resolves_through_it() {
        // (Greetings replace {identifier: id}) where identifier = 1 — the
        // predicate references the renamed name; it resolves to column "id".
        let renamed = RelExpr::Rename {
            input: Box::new(greetings()),
            renames: vec![("id".to_string(), "identifier".to_string())],
        };
        let expr = RelExpr::Restrict {
            input: Box::new(renamed),
            pred: Predicate::AttrCmp { op: CmpOp::Eq,
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

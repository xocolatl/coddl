//! The SQL pushdown cut: decide whether a relational subtree can be served by
//! the backend and, if so, bake its SQL.
//!
//! The gate is origin-driven, not a cost model. A `RelvarRooted` tree (every
//! leaf a public relvar) pushes when SQL emission succeeds. A `Mixed` tree —
//! public relvars combined with in-process relation values — **also pushes**:
//! the lowerer has already collapsed every materialized subtree to a
//! `RelExpr::RelParam` leaf, and emission renders each as a VALUES-backed
//! derived table whose rows the runtime binds. This is the settled
//! mixed-origin rule (docs/relir.md "The cut"): always ship the local relation
//! *up* into SQL, never pull the relvar *down* into memory — the local side
//! is bounded (a request path, a literal, a private relvar) while the relvar
//! side is unbounded. A fully `Materialized` tree stays in-process (there may
//! not even be a database). The seam where a real cost model lands later is
//! here; until then ship-up is unconditional.

use coddl_relir::{RelExpr, StorageOrigin};
use coddl_sqlemit::{emit_select_ordered, Dialect, SqlQuery};

/// Try to push `expr` to the backend.
///
/// Returns the baked [`SqlQuery`] when the subtree touches a relvar
/// (`RelvarRooted` or `Mixed`) and emits cleanly, else `None` — the caller
/// then lowers `expr` via the in-process path. A `Mixed` query's
/// `rel_params` name the shipped relation slots.
pub fn try_push(expr: &RelExpr, dialect: Dialect) -> Option<SqlQuery> {
    try_push_ordered(expr, dialect, &[])
}

/// Try to push `expr` with a trailing `ORDER BY` for the `load … order [ … ]`
/// boundary. `order` is the sort keys as `(attribute-name, is_descending)`
/// pairs. Returns the ordered [`SqlQuery`] when the subtree touches a relvar
/// and the order attaches cleanly; `None` otherwise (fully materialized, or a
/// root set-op / semijoin / `tclose` that can't carry a trailing `ORDER BY`
/// in v1) — the caller then sorts in-process. An empty `order` is exactly
/// [`try_push`].
pub fn try_push_ordered(
    expr: &RelExpr,
    dialect: Dialect,
    order: &[(String, bool)],
) -> Option<SqlQuery> {
    if expr.origin() == StorageOrigin::Materialized {
        return None;
    }
    emit_select_ordered(expr, dialect, order).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_relir::{CmpOp, Heading, Literal, Predicate, RestrictValue, Type};

    fn greetings() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Greetings".to_string(),
            database: "greetings".to_string(),
            heading: Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("message".to_string(), Type::Text),
            ]),
            table_name: "greetings".to_string(),
            columns: vec![
                ("id".to_string(), "id".to_string()),
                ("message".to_string(), "message".to_string()),
            ],
            keys: vec![vec!["id".to_string()]],
        }
    }

    #[test]
    fn pushes_relvar_rooted_restrict() {
        let expr = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrCmp {
                attr: "id".to_string(),
                op: CmpOp::Eq,
                value: RestrictValue::Lit(Literal::Integer(1)),
            },
        };
        let q = try_push(&expr, Dialect::SQLite).expect("relvar-rooted subtree pushes");
        assert_eq!(
            q.sql.text,
            // Full heading keeps key `id` → already a set → no DISTINCT.
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?1"#
        );
        assert_eq!(q.sql.param_count, 1);
    }

    #[test]
    fn pushes_relvar_rooted_tclose() {
        // A binary same-typed relvar `{ a: Integer, b: Integer }` closed: a
        // relvar-rooted `TClose` passes the origin gate and emits a
        // `WITH RECURSIVE` query (the deferred half of `tclose`, now landed).
        let edges = RelExpr::RelvarRef {
            name: "Edges".to_string(),
            database: "graph".to_string(),
            heading: Heading::new(vec![
                ("a".to_string(), Type::Integer),
                ("b".to_string(), Type::Integer),
            ]),
            table_name: "edges".to_string(),
            columns: vec![
                ("a".to_string(), "a".to_string()),
                ("b".to_string(), "b".to_string()),
            ],
            keys: vec![vec!["a".to_string(), "b".to_string()]],
        };
        let expr = RelExpr::TClose {
            input: Box::new(edges),
        };
        let q = try_push(&expr, Dialect::SQLite).expect("relvar-rooted tclose pushes");
        assert!(
            q.sql.text.starts_with("WITH RECURSIVE "),
            "tclose pushes as a recursive CTE: {}",
            q.sql.text
        );
        assert!(q
            .sql
            .text
            .contains(r#"JOIN coddl_tc_op ON coddl_tc."b" = coddl_tc_op."a""#));
    }

    #[test]
    fn ordered_push_appends_order_by() {
        // `load … from (Greetings where id = 1) order [asc message]` pushes the
        // order as a trailing ORDER BY on the relvar-rooted restrict.
        let expr = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrCmp {
                attr: "id".to_string(),
                op: CmpOp::Eq,
                value: RestrictValue::Lit(Literal::Integer(1)),
            },
        };
        let q = try_push_ordered(&expr, Dialect::SQLite, &[("message".to_string(), false)])
            .expect("relvar-rooted ordered load pushes");
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?1 ORDER BY "message""#
        );
    }

    #[test]
    fn ordered_push_declines_setop_root() {
        // A root `union` can't carry a trailing ORDER BY in v1 → decline, so the
        // caller sorts in-process. Unordered, the same union still pushes.
        let union = RelExpr::Or {
            lhs: Box::new(greetings()),
            rhs: Box::new(greetings()),
        };
        assert!(try_push_ordered(&union, Dialect::SQLite, &[("id".to_string(), false)]).is_none());
        assert!(try_push(&union, Dialect::SQLite).is_some());
    }
}

//! The SQL pushdown cut: decide whether a relvar-rooted relational subtree
//! can be served by the backend and, if so, bake its SQL.
//!
//! v1 is a trivial gate, not a cost model: push when every leaf is a public
//! relvar (`StorageOrigin::RelvarRooted`) and SQL emission succeeds; otherwise
//! fall back to the in-process materialize path so nothing regresses. This is
//! the seam where a real cost model lands later.

use coddl_relir::{RelExpr, StorageOrigin};
use coddl_sqlemit::{emit_select, Dialect, SqlQuery};

/// Try to push `expr` to the backend.
///
/// Returns the baked [`SqlQuery`] when the subtree is relvar-rooted and emits
/// cleanly, else `None` — the caller then lowers `expr` via the legacy
/// in-process path.
pub fn try_push(expr: &RelExpr, dialect: Dialect) -> Option<SqlQuery> {
    if expr.origin() != StorageOrigin::RelvarRooted {
        return None;
    }
    emit_select(expr, dialect).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_relir::{CmpOp, Heading, Literal, Predicate, Type};

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
                value: Literal::Integer(1),
            },
        };
        let q = try_push(&expr, Dialect::SQLite).expect("relvar-rooted subtree pushes");
        assert_eq!(
            q.sql.text,
            // Full heading keeps key `id` → already a set → no DISTINCT.
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?"#
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
        assert!(q.sql.text.contains(r#"JOIN coddl_tc_op ON coddl_tc."b" = coddl_tc_op."a""#));
    }
}

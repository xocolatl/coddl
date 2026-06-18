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
    use coddl_relir::{Heading, Literal, Predicate, Type};

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
        }
    }

    #[test]
    fn pushes_relvar_rooted_restrict() {
        let expr = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrEq {
                attr: "id".to_string(),
                value: Literal::Integer(1),
            },
        };
        let q = try_push(&expr, Dialect::SQLite).expect("relvar-rooted subtree pushes");
        assert_eq!(
            q.sql.text,
            r#"SELECT DISTINCT "id", "message" FROM "greetings" WHERE "id" = ?"#
        );
        assert_eq!(q.sql.param_count, 1);
    }
}

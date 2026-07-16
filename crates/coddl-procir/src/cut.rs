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
use coddl_sqlemit::{emit_select, emit_select_ordered, Dialect, ParamSource, SqlQuery};

/// A pushed subtree's baked plan(s): the general query, plus — for a root
/// `matching` whose rhs is the plan's only shipped relation — the
/// cardinality-1 sibling the runtime dispatches to when that relation holds
/// exactly one row (`L matching {t}` ≡ `L where shared = t.shared and …`).
/// The sibling's binds are the general plan's scalars followed by the row's
/// cells (`RestrictValue::SlotCell` placeholders); cardinality is runtime
/// knowledge, SQL text is compile-time, so both shapes are baked and the
/// force point picks.
pub struct PushedPlan {
    pub query: SqlQuery,
    /// `(sibling plan, dispatch slot)` — the slot whose runtime cardinality
    /// selects the sibling.
    pub card1_alt: Option<(SqlQuery, usize)>,
}

/// Try to push `expr` to the backend.
///
/// Returns the baked [`PushedPlan`] when the subtree touches a relvar
/// (`RelvarRooted` or `Mixed`) and emits cleanly, else `None` — the caller
/// then lowers `expr` via the in-process path. A `Mixed` query's
/// `rel_params` name the shipped relation slots.
pub fn try_push(expr: &RelExpr, dialect: Dialect) -> Option<PushedPlan> {
    try_push_ordered(expr, dialect, &[])
}

/// Try to push `expr` with a trailing `ORDER BY` for the `load … order [ … ]`
/// boundary. `order` is the sort keys as `(attribute-name, is_descending)`
/// pairs. Returns the ordered [`PushedPlan`] when the subtree touches a relvar
/// and the order attaches cleanly; `None` otherwise (fully materialized, or a
/// root set-op / semijoin / `tclose` that can't carry a trailing `ORDER BY`
/// in v1) — the caller then sorts in-process. An empty `order` is exactly
/// [`try_push`].
pub fn try_push_ordered(
    expr: &RelExpr,
    dialect: Dialect,
    order: &[(String, bool)],
) -> Option<PushedPlan> {
    if expr.origin() == StorageOrigin::Materialized {
        return None;
    }
    let query = emit_select_ordered(expr, dialect, order).ok()?;
    // A semijoin root can't carry a trailing ORDER BY (the push above would
    // have declined), so a successful ordered push never qualifies — the
    // shape check inside the rewrite handles that without a special case.
    let card1_alt = card1_alt_of(expr, &query, dialect);
    Some(PushedPlan { query, card1_alt })
}

/// Bake the cardinality-1 sibling of a qualifying pushed query (see
/// [`PushedPlan::card1_alt`]), or `None` when the root shape doesn't qualify
/// or the sibling doesn't emit.
fn card1_alt_of(expr: &RelExpr, general: &SqlQuery, dialect: Dialect) -> Option<(SqlQuery, usize)> {
    let (specialized, slot) = expr.card1_semijoin_specialization()?;
    let alt = emit_select(&specialized, dialect).ok()?;
    // The rewrite requires the dispatch slot to be the tree's only rel param,
    // so the sibling carries no markers of its own.
    debug_assert!(
        alt.rel_params.is_empty(),
        "card-1 sibling still carries rel-param markers"
    );
    // The numbering invariant the runtime dispatch relies on: the sibling's
    // binds are exactly the general plan's scalars (same lhs, same resolve
    // order → same `?1..?k`) followed by only slot cells (`?k+1..?k+m`).
    debug_assert!(
        alt.params.len() > general.params.len()
            && alt.params[..general.params.len()] == general.params[..]
            && alt.params[general.params.len()..]
                .iter()
                .all(|p| matches!(p, ParamSource::SlotCell { .. })),
        "card-1 sibling's binds must be the general plan's scalars plus slot cells"
    );
    Some((alt, slot))
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
        let q = try_push(&expr, Dialect::SQLite)
            .expect("relvar-rooted subtree pushes")
            .query;
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
        let q = try_push(&expr, Dialect::SQLite)
            .expect("relvar-rooted tclose pushes")
            .query;
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
            .expect("relvar-rooted ordered load pushes")
            .query;
        assert_eq!(
            q.sql.text,
            r#"SELECT "id", "message" FROM "greetings" WHERE "id" = ?1 ORDER BY "message""#
        );
    }

    #[test]
    fn mixed_matching_root_bakes_the_card1_sibling() {
        // `Greetings matching <local {id}> project { message }` — the general
        // plan is the EXISTS+VALUES form; the sibling is the plain keyed
        // lookup the runtime fires when the shipped relation holds one row.
        let expr = RelExpr::Project {
            input: Box::new(RelExpr::Semijoin {
                lhs: Box::new(greetings()),
                rhs: Box::new(RelExpr::RelParam {
                    slot: 0,
                    heading: Heading::new(vec![("id".to_string(), Type::Integer)]),
                }),
                negated: false,
            }),
            keep: vec!["message".to_string()],
        };
        let plan = try_push(&expr, Dialect::SQLite).expect("mixed matching pushes");
        assert!(plan.query.sql.text.contains("WHERE EXISTS"));
        let (alt, dispatch_slot) = plan.card1_alt.expect("card-1 sibling baked");
        assert_eq!(dispatch_slot, 0);
        assert_eq!(
            alt.sql.text,
            r#"SELECT "message" FROM "greetings" WHERE "id" = ?1"#
        );
        // The antijoin gets no sibling (cardinality-1 `not matching` is a
        // disjunction — B1's territory).
        let anti = RelExpr::Semijoin {
            lhs: Box::new(greetings()),
            rhs: Box::new(RelExpr::RelParam {
                slot: 0,
                heading: Heading::new(vec![("id".to_string(), Type::Integer)]),
            }),
            negated: true,
        };
        let plan = try_push(&anti, Dialect::SQLite).expect("mixed antijoin pushes");
        assert!(plan.card1_alt.is_none());
    }

    #[test]
    fn stacked_semijoins_push_general_only() {
        // Two semijoins in one query never bake a sibling, for two distinct
        // reasons — but the general nested-EXISTS plan pushes either way.
        let mentions = RelExpr::RelvarRef {
            name: "Mentions".to_string(),
            database: "greetings".to_string(),
            heading: Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("topic".to_string(), Type::Text),
            ]),
            table_name: "mentions".to_string(),
            columns: vec![
                ("id".to_string(), "id".to_string()),
                ("topic".to_string(), "topic".to_string()),
            ],
            keys: vec![vec!["id".to_string(), "topic".to_string()]],
        };
        let id_param = |slot| RelExpr::RelParam {
            slot,
            heading: Heading::new(vec![("id".to_string(), Type::Integer)]),
        };
        // A relvar-rooted semijoin in the lhs: the card-1 rewrite fires on
        // shape (rhs is the only rel param) but the rewritten tree is a
        // `Restrict` over a nested `Semijoin`, which doesn't emit — the
        // sibling declines gracefully and only the general plan is baked.
        let inner_relvar = RelExpr::Semijoin {
            lhs: Box::new(RelExpr::Semijoin {
                lhs: Box::new(greetings()),
                rhs: Box::new(mentions),
                negated: false,
            }),
            rhs: Box::new(id_param(0)),
            negated: false,
        };
        let plan = try_push(&inner_relvar, Dialect::SQLite).expect("nested semijoin pushes");
        assert_eq!(plan.query.sql.text.matches("WHERE EXISTS").count(), 2);
        assert!(plan.card1_alt.is_none());
        // Two shipped slots: the v1 single-slot gate declines the rewrite
        // outright; both markers ride the general plan.
        let two_slots = RelExpr::Semijoin {
            lhs: Box::new(RelExpr::Semijoin {
                lhs: Box::new(greetings()),
                rhs: Box::new(id_param(0)),
                negated: false,
            }),
            rhs: Box::new(id_param(1)),
            negated: false,
        };
        let plan = try_push(&two_slots, Dialect::SQLite).expect("two-slot semijoin pushes");
        assert!(
            plan.query.sql.text.contains("__CODDL_REL_0__")
                && plan.query.sql.text.contains("__CODDL_REL_1__")
        );
        assert!(plan.card1_alt.is_none());
    }

    #[test]
    fn pushes_relvar_rooted_otherwise_as_one_plan() {
        // `Greetings otherwise Greetings` (both arms relvar-rooted) passes the
        // origin gate and emits the compound CTE + UNION ALL + NOT EXISTS form
        // as ONE ordinary plan — no card-1 sibling (the rewrite is
        // semijoin-root-only), no gate skips, nothing shipped.
        let expr = RelExpr::Otherwise {
            primary: Box::new(RelExpr::Restrict {
                input: Box::new(greetings()),
                pred: Predicate::AttrCmp {
                    attr: "id".to_string(),
                    op: CmpOp::Eq,
                    value: RestrictValue::Lit(Literal::Integer(1)),
                },
            }),
            fallback: Box::new(greetings()),
        };
        let plan = try_push(&expr, Dialect::SQLite).expect("relvar-rooted otherwise pushes");
        assert!(plan.query.sql.text.starts_with("WITH coddl_ow_p"));
        assert!(plan.query.sql.text.contains("UNION ALL"));
        assert!(plan.query.sql.text.contains("NOT EXISTS"));
        assert!(plan.card1_alt.is_none());
        assert!(plan.query.gate_params.is_empty());
        assert!(plan.query.rel_params.is_empty());
        // The ordered form declines (a trailing ORDER BY can't attach to the
        // compound) — the caller sorts in-process.
        assert!(try_push_ordered(&expr, Dialect::SQLite, &[("id".to_string(), false)]).is_none());
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

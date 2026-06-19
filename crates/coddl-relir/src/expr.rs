//! The RelIR expression tree: a relvar-rooted leaf plus the nodes needed to
//! restrict, project, and rename it.
//!
//! This is the minimal set that represents reading a public relvar, filtering
//! it, narrowing it to a subset of attributes, and renaming attributes.
//! `Restrict` (surface `where`) and `Project` are sugar that will desugar onto
//! the Algebra A core (`Project` onto the REMOVE primitive); the `Rename` node
//! realizes the core RENAME directly. The remaining A-core operators (AND, OR,
//! NOT, TCLOSE) and the rest of the sugar layer grow here later.

use coddl_types::{Heading, Type};

/// Where a (sub)expression's data is ultimately rooted.
///
/// A flag that drives the SQL-vs-in-process cut: it records *whether* a
/// subtree can be pushed to a backend, not which engine. The identity needed
/// to group pushable leaves (the logical database) lives on the leaves
/// themselves; the concrete backend and its SQL dialect are resolved at the
/// storage boundary, never here — RelIR is backend-agnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageOrigin {
    /// Every leaf is a public relvar — a candidate for SQL pushdown.
    RelvarRooted,
    /// A relation literal or private relvar — evaluated in-process.
    Materialized,
    /// A mix of both — the cut inserts a materialization boundary.
    Mixed,
}

/// A relational-algebra expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelExpr {
    /// A public relvar read — the relvar-rooted leaf. Carries everything SQL
    /// emission and the cut need: the heading, the physical table and its
    /// attribute→column mapping, and the logical database the relvar lives
    /// in. The cut groups leaves by `database`; the backend kind and dialect
    /// are resolved elsewhere, so they deliberately do not appear here.
    RelvarRef {
        /// The relvar's application-level name.
        name: String,
        /// The logical database handle the relvar is rooted in.
        database: String,
        /// The relation's heading.
        heading: Heading,
        /// The physical SQL table name.
        table_name: String,
        /// Attribute→column mapping in heading-canonical (sorted) order:
        /// `(attribute_name, sql_column_name)`.
        columns: Vec<(String, String)>,
        /// The relvar's declared candidate keys — one inner `Vec` per key,
        /// each the key's attribute-name set. Used to prove a result is
        /// already duplicate-free so an emitted `SELECT` can drop `DISTINCT`
        /// (see [`RelExpr::needs_distinct`]).
        keys: Vec<Vec<String>>,
    },
    /// Restrict by a predicate (surface `where`). Heading-preserving.
    ///
    /// Sugar: in Algebra A a restriction is a natural join against a constant
    /// relation that encodes the predicate (operators-as-relations). The
    /// desugaring onto the A core arrives with the A-core nodes; for now this
    /// is a node in its own right.
    Restrict {
        input: Box<RelExpr>,
        pred: Predicate,
    },
    /// Project onto a subset of attributes — the project-away (REMOVE)
    /// direction of the algebra. Narrows the heading to `keep`.
    Project {
        input: Box<RelExpr>,
        /// Attribute names to retain.
        keep: Vec<String>,
    },
    /// Rename attributes (surface `rename`). Heading-cardinality-preserving;
    /// values unchanged, names swapped. The heading re-canonicalizes (sorts)
    /// under the new names.
    Rename {
        input: Box<RelExpr>,
        /// `(old, new)` pairs. A bijection: no `old` repeats and no `new`
        /// collides with a surviving attribute (the typechecker enforces it).
        renames: Vec<(String, String)>,
    },
}

/// Apply a rename map to one attribute name: the renamed name if `name` is a
/// rename source, else `name` unchanged.
fn apply_rename(renames: &[(String, String)], name: &str) -> String {
    renames
        .iter()
        .find(|(old, _)| old == name)
        .map(|(_, new)| new.clone())
        .unwrap_or_else(|| name.to_string())
}

/// A restriction predicate. Currently a single attribute-equals-literal test;
/// this grows to comparisons, conjunction/disjunction, and attribute-vs-
/// attribute tests as the surface `where` support grows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predicate {
    /// `<attr> = <literal>`.
    AttrEq { attr: String, value: Literal },
}

/// A scalar literal usable in a predicate. Grows alongside the scalar types
/// the predicate language accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    Integer(i64),
    Text(String),
    Boolean(bool),
}

impl RelExpr {
    /// The heading of the relation this expression produces.
    ///
    /// `RelvarRef` yields its declared heading; `Restrict` preserves its
    /// input's; `Project` narrows its input's to the retained attributes.
    pub fn heading(&self) -> Heading {
        match self {
            RelExpr::RelvarRef { heading, .. } => heading.clone(),
            RelExpr::Restrict { input, .. } => input.heading(),
            RelExpr::Project { input, keep } => {
                let kept: Vec<(String, Type)> = input
                    .heading()
                    .attrs()
                    .iter()
                    .filter(|(name, _)| keep.contains(name))
                    .cloned()
                    .collect();
                Heading::new(kept)
            }
            RelExpr::Rename { input, renames } => {
                // Remap names; `Heading::new` re-canonicalizes (re-sorts).
                let remapped: Vec<(String, Type)> = input
                    .heading()
                    .attrs()
                    .iter()
                    .map(|(name, ty)| (apply_rename(renames, name), ty.clone()))
                    .collect();
                Heading::new(remapped)
            }
        }
    }

    /// Where this expression's data is rooted — the input to the SQL cut.
    ///
    /// `RelvarRef` is relvar-rooted; the unary operators inherit their input's
    /// origin, since restricting or projecting does not change what the data
    /// is rooted in.
    pub fn origin(&self) -> StorageOrigin {
        match self {
            RelExpr::RelvarRef { .. } => StorageOrigin::RelvarRooted,
            RelExpr::Restrict { input, .. } => input.origin(),
            RelExpr::Project { input, .. } => input.origin(),
            RelExpr::Rename { input, .. } => input.origin(),
        }
    }

    /// The candidate keys whose attributes all survive into this expression's
    /// heading. A surviving key guarantees row-uniqueness on the (possibly
    /// projected) heading, so the emitted `SELECT` need not be `DISTINCT`.
    ///
    /// `RelvarRef` yields its declared keys; `Restrict` preserves them
    /// (filtering a set is a set); `Project` keeps only keys whose attributes
    /// are all retained.
    pub fn surviving_keys(&self) -> Vec<Vec<String>> {
        match self {
            RelExpr::RelvarRef { keys, .. } => keys.clone(),
            RelExpr::Restrict { input, .. } => input.surviving_keys(),
            RelExpr::Project { input, keep } => input
                .surviving_keys()
                .into_iter()
                .filter(|k| k.iter().all(|a| keep.contains(a)))
                .collect(),
            RelExpr::Rename { input, renames } => input
                .surviving_keys()
                .into_iter()
                .map(|k| k.iter().map(|a| apply_rename(renames, a)).collect())
                .collect(),
        }
    }

    /// True when the expression provably has at most one tuple — so any
    /// projection of it is duplicate-free regardless of which attributes
    /// survive.
    ///
    /// A `Restrict` that pins every attribute of some candidate key to a
    /// constant bounds cardinality to ≤ 1. v1 restrictions are a single
    /// `AttrEq`, so this holds iff the pinned attribute is itself a candidate
    /// key of the input.
    pub fn card_le_one(&self) -> bool {
        match self {
            RelExpr::RelvarRef { .. } => false,
            RelExpr::Restrict { input, pred } => {
                input.card_le_one() || {
                    let Predicate::AttrEq { attr, .. } = pred;
                    input
                        .surviving_keys()
                        .iter()
                        .any(|k| k.len() == 1 && &k[0] == attr)
                }
            }
            RelExpr::Project { input, .. } => input.card_le_one(),
            RelExpr::Rename { input, .. } => input.card_le_one(),
        }
    }

    /// Whether the emitted `SELECT` must be `DISTINCT` to honor RM Pro 3.
    ///
    /// It need not be when the result is *provably* already a set: the input
    /// has ≤ 1 tuple, or a candidate key survives into the heading (no two
    /// distinct rows can collide on the kept columns). Otherwise a projection
    /// may collapse distinct rows into duplicates and `DISTINCT` is required.
    /// Conservative: an unknown/keyless leaf keeps `DISTINCT`.
    pub fn needs_distinct(&self) -> bool {
        !(self.card_le_one() || !self.surviving_keys().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn id_eq_1() -> Predicate {
        Predicate::AttrEq {
            attr: "id".to_string(),
            value: Literal::Integer(1),
        }
    }

    #[test]
    fn relvar_ref_reports_its_heading_and_relvar_rooted_origin() {
        let r = greetings();
        assert_eq!(r.heading(), greetings_heading());
        assert_eq!(r.origin(), StorageOrigin::RelvarRooted);
    }

    #[test]
    fn restrict_preserves_heading_and_origin() {
        let r = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: id_eq_1(),
        };
        assert_eq!(r.heading(), greetings_heading());
        assert_eq!(r.origin(), StorageOrigin::RelvarRooted);
    }

    #[test]
    fn project_narrows_heading_and_keeps_origin() {
        let r = RelExpr::Project {
            input: Box::new(greetings()),
            keep: vec!["message".to_string()],
        };
        assert_eq!(
            r.heading(),
            Heading::new(vec![("message".to_string(), Type::Text)])
        );
        assert_eq!(r.origin(), StorageOrigin::RelvarRooted);
    }

    #[test]
    fn project_over_restrict_propagates_through_both() {
        // project { message } (Greetings where id = 1)
        let r = RelExpr::Project {
            input: Box::new(RelExpr::Restrict {
                input: Box::new(greetings()),
                pred: id_eq_1(),
            }),
            keep: vec!["message".to_string()],
        };
        assert_eq!(
            r.heading(),
            Heading::new(vec![("message".to_string(), Type::Text)])
        );
        assert_eq!(r.origin(), StorageOrigin::RelvarRooted);
    }

    // ── DISTINCT-elision analyses ─────────────────────────────────────

    #[test]
    fn bare_relvar_keeps_its_key_so_no_distinct() {
        let r = greetings();
        assert_eq!(r.surviving_keys(), vec![vec!["id".to_string()]]);
        assert!(!r.card_le_one());
        assert!(!r.needs_distinct(), "a full relvar read keeps its key");
    }

    #[test]
    fn key_equality_restriction_bounds_cardinality() {
        // Greetings where id = 1 — id is the key, so ≤ 1 tuple.
        let r = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: id_eq_1(),
        };
        assert!(r.card_le_one());
        assert!(!r.needs_distinct());
    }

    #[test]
    fn projection_keeping_the_key_needs_no_distinct() {
        let r = RelExpr::Project {
            input: Box::new(greetings()),
            keep: vec!["id".to_string()],
        };
        assert_eq!(r.surviving_keys(), vec![vec!["id".to_string()]]);
        assert!(!r.needs_distinct());
    }

    #[test]
    fn projection_dropping_key_unbounded_needs_distinct() {
        // Greetings project {message} — key gone, cardinality unbounded.
        let r = RelExpr::Project {
            input: Box::new(greetings()),
            keep: vec!["message".to_string()],
        };
        assert!(r.surviving_keys().is_empty());
        assert!(!r.card_le_one());
        assert!(r.needs_distinct(), "dropping the key may create duplicates");
    }

    #[test]
    fn projection_dropping_key_but_card_bounded_needs_no_distinct() {
        // (Greetings where id = 1) project {message} — key gone but ≤ 1 tuple.
        let r = RelExpr::Project {
            input: Box::new(RelExpr::Restrict {
                input: Box::new(greetings()),
                pred: id_eq_1(),
            }),
            keep: vec!["message".to_string()],
        };
        assert!(r.surviving_keys().is_empty());
        assert!(r.card_le_one());
        assert!(!r.needs_distinct());
    }

    // ── rename ────────────────────────────────────────────────────────

    fn renamed() -> RelExpr {
        // Greetings rename {id: identifier, message: msg}
        RelExpr::Rename {
            input: Box::new(greetings()),
            renames: vec![
                ("id".to_string(), "identifier".to_string()),
                ("message".to_string(), "msg".to_string()),
            ],
        }
    }

    #[test]
    fn rename_remaps_and_recanonicalizes_heading() {
        let r = renamed();
        assert_eq!(
            r.heading(),
            // re-sorted under the new names: identifier < msg
            Heading::new(vec![
                ("identifier".to_string(), Type::Integer),
                ("msg".to_string(), Type::Text),
            ])
        );
        assert_eq!(r.origin(), StorageOrigin::RelvarRooted);
    }

    #[test]
    fn rename_carries_keys_through_so_no_distinct() {
        let r = renamed();
        // key `id` becomes `identifier`; it still survives → no DISTINCT.
        assert_eq!(r.surviving_keys(), vec![vec!["identifier".to_string()]]);
        assert!(!r.needs_distinct());
    }

    #[test]
    fn rename_over_key_restriction_keeps_card_bound() {
        // (Greetings where id = 1) rename {message: msg} — still ≤ 1 tuple.
        let r = RelExpr::Rename {
            input: Box::new(RelExpr::Restrict {
                input: Box::new(greetings()),
                pred: id_eq_1(),
            }),
            renames: vec![("message".to_string(), "msg".to_string())],
        };
        assert!(r.card_le_one());
        assert!(!r.needs_distinct());
    }
}

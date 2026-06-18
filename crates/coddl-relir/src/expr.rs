//! The RelIR expression tree: a relvar-rooted leaf plus the sugar nodes
//! needed to restrict and project it.
//!
//! This is the minimal set that represents reading a public relvar, filtering
//! it, and narrowing it to a subset of attributes. The Algebra A core (AND,
//! OR, NOT, REMOVE, RENAME, TCLOSE) and the rest of the sugar layer grow here
//! later; `Restrict` and `Project` are sugar that will desugar onto that core.

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
        }
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
}

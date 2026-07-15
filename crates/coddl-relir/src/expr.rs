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
    /// Extend (surface `extend`) — add each new attribute bound to a computed
    /// scalar expression, keeping every operand attribute. Heading-growing;
    /// the result re-canonicalizes (sorts) with the new attributes mixed in.
    /// Each entry carries the new attribute name, its (typechecked) result
    /// type, and the scalar expression that computes it. Origin/keys inherit
    /// from the input (a scalar extend doesn't change rooting, and adds a
    /// functionally-determined column that can't break a surviving key). A
    /// relvar-rooted operand pushes to SQL as `SELECT …, (<expr>) AS <c> …`.
    Extend {
        input: Box<RelExpr>,
        /// `(new_name, result_type, value)` triples, in source order.
        extends: Vec<(String, Type, ScalarExpr)>,
    },
    /// Natural join (surface `join`) — the Algebra-A `AND` core node. The
    /// result heading is the union of the operands' headings (shared
    /// attributes appear once, with matching types the typechecker enforces).
    /// Both operands relvar-rooted → pushes to SQL; both materialized →
    /// in-process; mixed → a materialization boundary.
    And {
        lhs: Box<RelExpr>,
        rhs: Box<RelExpr>,
    },
    /// Set union (surface `union`) — the Algebra-A `OR` core node, restricted to
    /// identical operand headings (the typechecker enforces it; Coddl has no
    /// nulls, so no heading-agnostic union). The result heading is that shared
    /// heading. Both operands relvar-rooted → pushes to SQL as `… UNION …`; both
    /// materialized → in-process; mixed → a materialization boundary.
    Or {
        lhs: Box<RelExpr>,
        rhs: Box<RelExpr>,
    },
    /// Set difference (surface `minus`) — the Algebra-A `AND NOT` core, restricted
    /// to identical operand headings (typechecked). The result is the `lhs` tuples
    /// not in `rhs`, so its heading is `lhs`'s (= `rhs`'s). Both operands
    /// relvar-rooted → pushes to SQL as `… EXCEPT …`; both materialized →
    /// in-process; mixed → a materialization boundary.
    Minus {
        lhs: Box<RelExpr>,
        rhs: Box<RelExpr>,
    },
    /// Semijoin (surface `matching`, `negated == false`) / antijoin (surface `not
    /// matching`, `negated == true`) — filter `lhs` to the tuples that have
    /// (semijoin) / lack (antijoin) a match in `rhs` on the shared attributes.
    /// The result is a subset of `lhs`, so its heading is `lhs`'s. A **sugar
    /// node**: in Algebra A this is `(lhs AND rhs)` projected back onto `lhs`
    /// (semijoin) or `lhs` minus that (antijoin), but keeping it explicit lets
    /// the SQL emitter push it as the idiomatic `WHERE [NOT] EXISTS` correlated
    /// subquery (no join row-multiplication, no `DISTINCT`/`EXCEPT`) instead of
    /// reconstructing the semijoin from an `And`+`Project`. In-process it expands
    /// to join+project(+minus). Origin combines like the other binary nodes: both
    /// relvar-rooted → pushes; both materialized → in-process; mixed → boundary.
    Semijoin {
        lhs: Box<RelExpr>,
        rhs: Box<RelExpr>,
        negated: bool,
    },
    /// Transitive closure (surface `tclose`) — the Algebra-A `◄TCLOSE►` core,
    /// the one genuinely irreducible operator (transitive closure is not
    /// first-order expressible, so it cannot be built from finite AND/OR/NOT
    /// composition). **Unary.** The operand is a binary relation of two
    /// identically-typed attributes (typechecked); the result heading is
    /// unchanged — closure is direction-agnostic, so the result is the same
    /// relation regardless of which attribute is treated as source. v1 has no
    /// SQL emission (a `WITH RECURSIVE` push is a deferred follow-up), so a
    /// relvar-rooted `tclose` fetches its operand via SQL then closes
    /// in-process.
    TClose { input: Box<RelExpr> },
    /// Wrap (surface `wrap`) — group attributes into tuple-valued attributes.
    /// **Unary.** Each `(new, components)` removes the component attributes from
    /// the top level and adds `new : Tuple(components)`. Cardinality- and
    /// data-preserving (a leaf-cell re-layout). v1 has no SQL emission (a
    /// flat-column push is a deferred follow-up), so a relvar-rooted `wrap`
    /// fetches its operand via SQL then restructures in-process.
    Wrap {
        input: Box<RelExpr>,
        /// `(new_name, components_heading)` pairs, in source order.
        wraps: Vec<(String, Heading)>,
    },
    /// Unwrap (surface `unwrap`) — expand tuple-valued attributes back to their
    /// components, lifted to top level. **Unary.** The inverse of `Wrap`; same
    /// data-preserving re-layout and same SQL-deferral.
    Unwrap {
        input: Box<RelExpr>,
        /// The tuple-valued attribute names to expand.
        names: Vec<String>,
    },
    /// Group (surface `group` — TTM GROUP). **Unary.** Each `(new, components)`
    /// consumes the component attributes into `new : Relation(components)`; the
    /// attributes named in NO pair survive and partition the operand (one
    /// result tuple per distinct survivor combination — the survivor set is a
    /// candidate key of the result). Cardinality-**changing** (unlike `Wrap`),
    /// so it never pushes to SQL: a relation-valued cell has no flat-column
    /// form, and `resolve` declines it — a relvar-rooted `group` fetches its
    /// operand via SQL then nests in-process (`coddl_relation_group`).
    Group {
        input: Box<RelExpr>,
        /// `(new_name, components_heading)` pairs, in source order.
        groups: Vec<(String, Heading)>,
    },
    /// Ungroup (surface `ungroup` — TTM UNGROUP). **Unary.** Unnests the named
    /// relation-valued attributes: one result tuple per combination of an outer
    /// tuple and one tuple from each named RVA (an empty RVA contributes
    /// nothing). Cardinality-changing like `TClose`; same SQL decline as
    /// `Group` — in-process via `coddl_relation_ungroup`.
    Ungroup {
        input: Box<RelExpr>,
        /// The relation-valued attribute names to unnest.
        names: Vec<String>,
    },
    /// An in-memory (`private`) relvar read — the materialized counterpart of
    /// the relvar-rooted `RelvarRef` leaf. No SQL source, so any subtree
    /// containing it is `Materialized` and lowers in-process.
    MaterializedRelvar { name: String, heading: Heading },
    /// A relation-valued **bind parameter** — an in-process relation value
    /// shipped into the backend at query time as a `VALUES`-backed derived
    /// table (see `coddl-sqlemit`). The relation analogue of
    /// [`RestrictValue::Param`]: the free-variable identity is `slot`, an index
    /// into the per-build slot table the lowerer owns (each slot holds the AST
    /// subexpression whose lowered value binds at run time), so this IR stays
    /// independent of the lowerer and the backend exactly as `Param(name)`
    /// does. Origin is `Materialized`: a `RelParam` beside a relvar-rooted
    /// sibling makes the tree `Mixed`, which the cut pushes by shipping the
    /// slot's rows *up* into SQL — never by pulling the relvar down into
    /// memory (the settled mixed-origin rule, `docs/relir.md` "The cut").
    RelParam { slot: usize, heading: Heading },
}

/// Combine the storage origins of a binary node's two operands (`And` / `Or`):
/// both relvar-rooted → pushable; both materialized → in-process; otherwise a
/// materialization boundary (`Mixed`).
fn combine_origin(lhs: StorageOrigin, rhs: StorageOrigin) -> StorageOrigin {
    match (lhs, rhs) {
        (StorageOrigin::RelvarRooted, StorageOrigin::RelvarRooted) => StorageOrigin::RelvarRooted,
        (StorageOrigin::Materialized, StorageOrigin::Materialized) => StorageOrigin::Materialized,
        _ => StorageOrigin::Mixed,
    }
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

/// Render a restriction predicate for `RelExpr::render` (e.g. `id <> 1`;
/// a gate renders as `when <value>` — it has no attribute side).
fn render_predicate(pred: &Predicate) -> String {
    match pred {
        Predicate::AttrCmp { attr, op, value } => {
            format!("{attr} {} {}", op.sql(), render_value(value))
        }
        Predicate::Gate(value) => format!("when {}", render_value(value)),
    }
}

/// Render a restriction value for `RelExpr::render`. A literal renders
/// transparently (`render_literal`); a bound parameter renders as `:name` — the
/// placeholder form the explain output shows for a runtime-bound value.
fn render_value(value: &RestrictValue) -> String {
    match value {
        RestrictValue::Lit(lit) => render_literal(lit),
        RestrictValue::Param(name) => format!(":{name}"),
        RestrictValue::SlotCell { slot, cell } => format!(":rel{slot}[{cell}]"),
    }
}

/// Render a scalar literal for `RelExpr::render`. `Text` is quoted so the
/// rendered predicate is unambiguous; `Integer`/`Boolean` print bare;
/// `Character` prints as its codepoint (matching the integer it binds as).
fn render_literal(lit: &Literal) -> String {
    match lit {
        Literal::Integer(n) => n.to_string(),
        Literal::Text(s) => format!("{s:?}"),
        Literal::Character(cp) => cp.to_string(),
        Literal::Approximate(bits) => format!("{:e}", f64::from_bits(*bits)),
        Literal::Rational(n, d) => format!("{n}/{d}"),
        Literal::Boolean(b) => b.to_string(),
    }
}

/// A restriction predicate: a single `<attr> <cmp> <value>` test, or a
/// tuple-independent gate. This grows to conjunction/disjunction and
/// attribute-vs-attribute tests as the surface `where` support grows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predicate {
    /// `<attr> <op> <value>`.
    AttrCmp {
        attr: String,
        op: CmpOp,
        value: RestrictValue,
    },
    /// A tuple-independent Boolean gate (surface `when` — `R times ⟨c⟩` with
    /// the condition lifted to reltrue/relfalse). Reads no attribute: every
    /// tuple survives or none does. As a `Restrict` conjunct it inherits the
    /// restrict machinery wholesale — keys pass through, absorption holds
    /// (gate-of-empty is empty) — and SQL emission renders it `<placeholder>
    /// = 1` (never a bare truthy integer, which is SQLite-only). Note it
    /// counts as "filtered" to `contains_restrict`, muting the S1 pull guard
    /// for gated-relvar shapes — sound, because those shapes push.
    Gate(RestrictValue),
}

/// The right-hand side of a pushable restriction: either a compile-time
/// literal, or a **bound parameter** identified by the surface name of an
/// in-scope local/parameter whose runtime value binds at query time. The name
/// is a backend-agnostic free-variable identity — deliberately *not* a ProcIR
/// value id or an AST node, so this IR stays independent of the lowerer. Both
/// forms render to a `?`/`$n` placeholder in SQL (see `coddl-sqlemit`); the
/// lowerer resolves a `Param` name to the local's already-lowered value when it
/// emits the query's bind arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestrictValue {
    Lit(Literal),
    Param(String),
    /// A cell of a relation-valued parameter's **single row** — the bind form
    /// of the cardinality-1 semijoin specialization
    /// ([`RelExpr::card1_semijoin_specialization`]). `slot` names the
    /// [`RelExpr::RelParam`] whose runtime cardinality drives the dispatch;
    /// `cell` indexes its heading in canonical (sorted) order. Renders to an
    /// ordinary numbered placeholder; the *runtime* fills it from the shipped
    /// row at the force point when it selects the specialized sibling plan —
    /// the lowerer never sees it as a bind argument.
    SlotCell {
        slot: usize,
        cell: usize,
    },
}

/// A scalar comparison operator in a pushable restriction. Equality (`Eq`/`Ne`)
/// applies to Integer/Text/Boolean; the ordering ops are Integer-only (enforced
/// by the typechecker, not here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl CmpOp {
    /// The SQL spelling — identical across SQLite and Postgres.
    pub fn sql(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::LtEq => "<=",
            CmpOp::Gt => ">",
            CmpOp::GtEq => ">=",
        }
    }

    /// The operator with its operands swapped — `a OP b` ≡ `b flip(OP) a`. Used
    /// when the attribute is the right operand (`5 < id` ⇒ `id > 5`). Equality
    /// is symmetric (`Eq`/`Ne` map to themselves).
    pub fn flip(self) -> CmpOp {
        match self {
            CmpOp::Eq => CmpOp::Eq,
            CmpOp::Ne => CmpOp::Ne,
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::LtEq => CmpOp::GtEq,
            CmpOp::GtEq => CmpOp::LtEq,
        }
    }

    /// The logical negation — `NOT (a OP b)` ≡ `a negate(OP) b`. With no nulls
    /// (RM Pro 4, two-valued logic) the negation is total, so it's used to test
    /// that an UPDATE's "unchanged rows" operand is the exact complement
    /// `R where ¬p` of its "changed rows" `R where p`. Distinct from `flip`
    /// (which swaps operands): `negate(Lt)` is `GtEq`, not `Gt`.
    pub fn negate(self) -> CmpOp {
        match self {
            CmpOp::Eq => CmpOp::Ne,
            CmpOp::Ne => CmpOp::Eq,
            CmpOp::Lt => CmpOp::GtEq,
            CmpOp::GtEq => CmpOp::Lt,
            CmpOp::Gt => CmpOp::LtEq,
            CmpOp::LtEq => CmpOp::Gt,
        }
    }
}

/// A scalar literal usable in a predicate. Grows alongside the scalar types
/// the predicate language accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    Integer(i64),
    Text(String),
    /// A `Character` literal as its Unicode scalar value. Binds/stores as an
    /// integer codepoint in SQL (SQLite has no character type).
    Character(u32),
    /// An `Approximate` value as its **canonical** IEEE-754 double bit pattern
    /// (`u64`, not `f64`, so the enum stays `Eq`). Binds/stores as SQL `REAL`.
    Approximate(u64),
    /// A bounded `Rational` as its reduced `(numer, denom)` pair. Binds/stores
    /// as canonical SQL `TEXT "n/d"`.
    Rational(i64, i64),
    Boolean(bool),
}

/// A scalar expression computed per tuple — the value of an `extend`'s new
/// attribute. Self-contained (it does not reuse [`Literal`]) so the surface
/// scalar grammar can grow here without touching the predicate/`Value` paths.
/// v1 covers attribute references, Integer/Text/Character literals, and the
/// arithmetic/concatenation binary operators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScalarExpr {
    /// An operand-attribute reference.
    Attr(String),
    /// An `Integer` literal.
    Int(i64),
    /// A `Text` literal (already-decoded UTF-8).
    Str(String),
    /// A `Character` literal as its Unicode scalar value.
    Char(u32),
    /// A binary operator over two scalar sub-expressions.
    Bin {
        op: ScalarBinOp,
        lhs: Box<ScalarExpr>,
        rhs: Box<ScalarExpr>,
    },
}

/// The binary scalar operators an `extend` value may use: arithmetic
/// (`Integer × Integer → Integer`, `Div` truncating) and concatenation
/// (`(Text|Character) × (Text|Character) → Text`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Concat,
}

/// Render a scalar expression for `RelExpr::render` (the `coddl explain`
/// human view) — e.g. `(unit_cents * qty)`. Not SQL; the SQL form lives in
/// `coddl-sqlemit`.
fn render_scalar(e: &ScalarExpr) -> String {
    match e {
        ScalarExpr::Attr(name) => name.clone(),
        ScalarExpr::Int(n) => n.to_string(),
        ScalarExpr::Str(s) => format!("{s:?}"),
        ScalarExpr::Char(cp) => match char::from_u32(*cp) {
            Some(c) => format!("'{}'", c.escape_default()),
            None => format!("'\\u{{{cp:x}}}'"),
        },
        ScalarExpr::Bin { op, lhs, rhs } => {
            let sym = match op {
                ScalarBinOp::Add => "+",
                ScalarBinOp::Sub => "-",
                ScalarBinOp::Mul => "*",
                ScalarBinOp::Div => "/",
                ScalarBinOp::Concat => "||",
            };
            format!("({} {sym} {})", render_scalar(lhs), render_scalar(rhs))
        }
    }
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
            RelExpr::And { lhs, rhs } => lhs
                .heading()
                .union(&rhs.heading())
                .expect("typechecked join has compatible shared attributes"),
            // Identical operand headings (typechecked) — either is the result.
            RelExpr::Or { lhs, .. } => lhs.heading(),
            // The result is a subset of `lhs`, so its heading is `lhs`'s.
            RelExpr::Minus { lhs, .. } => lhs.heading(),
            // Semijoin/antijoin filter `lhs`, so the result heading is `lhs`'s.
            RelExpr::Semijoin { lhs, .. } => lhs.heading(),
            // Closure preserves the (binary) operand heading.
            RelExpr::TClose { input } => input.heading(),
            // Input attributes plus each computed `(name, type)`; `Heading::new`
            // re-canonicalizes (re-sorts) with the new attributes mixed in.
            RelExpr::Extend { input, extends } => {
                let mut attrs: Vec<(String, Type)> = input.heading().attrs().to_vec();
                attrs.extend(
                    extends
                        .iter()
                        .map(|(name, ty, _)| (name.clone(), ty.clone())),
                );
                Heading::new(attrs)
            }
            // Survivors (attributes not consumed by any wrap) plus each new
            // `Tuple(components)`; `Heading::new` re-canonicalizes.
            RelExpr::Wrap { input, wraps } => {
                let consumed: std::collections::HashSet<&str> = wraps
                    .iter()
                    .flat_map(|(_, h)| h.attrs().iter().map(|(n, _)| n.as_str()))
                    .collect();
                let mut attrs: Vec<(String, Type)> = input
                    .heading()
                    .attrs()
                    .iter()
                    .filter(|(n, _)| !consumed.contains(n.as_str()))
                    .cloned()
                    .collect();
                attrs.extend(
                    wraps
                        .iter()
                        .map(|(new, h)| (new.clone(), Type::Tuple(h.clone()))),
                );
                Heading::new(attrs)
            }
            // Survivors (attributes not unwrapped) plus each unwrapped tuple's
            // components lifted to top level.
            RelExpr::Unwrap { input, names } => {
                let in_heading = input.heading();
                let mut attrs: Vec<(String, Type)> = in_heading
                    .attrs()
                    .iter()
                    .filter(|(n, _)| !names.contains(n))
                    .cloned()
                    .collect();
                for name in names {
                    if let Some(Type::Tuple(sub)) = in_heading.lookup(name) {
                        attrs.extend(sub.attrs().iter().cloned());
                    }
                }
                Heading::new(attrs)
            }
            // Survivors (attributes not consumed by any pair) plus each new
            // `Relation(components)`; `Heading::new` re-canonicalizes. Same
            // shape as `Wrap` — only the added attribute's type differs.
            RelExpr::Group { input, groups } => {
                let consumed: std::collections::HashSet<&str> = groups
                    .iter()
                    .flat_map(|(_, h)| h.attrs().iter().map(|(n, _)| n.as_str()))
                    .collect();
                let mut attrs: Vec<(String, Type)> = input
                    .heading()
                    .attrs()
                    .iter()
                    .filter(|(n, _)| !consumed.contains(n.as_str()))
                    .cloned()
                    .collect();
                attrs.extend(
                    groups
                        .iter()
                        .map(|(new, h)| (new.clone(), Type::Relation(h.clone()))),
                );
                Heading::new(attrs)
            }
            // Survivors (attributes not ungrouped) plus each ungrouped
            // relation's attributes lifted to top level.
            RelExpr::Ungroup { input, names } => {
                let in_heading = input.heading();
                let mut attrs: Vec<(String, Type)> = in_heading
                    .attrs()
                    .iter()
                    .filter(|(n, _)| !names.contains(n))
                    .cloned()
                    .collect();
                for name in names {
                    if let Some(Type::Relation(sub)) = in_heading.lookup(name) {
                        attrs.extend(sub.attrs().iter().cloned());
                    }
                }
                Heading::new(attrs)
            }
            RelExpr::MaterializedRelvar { heading, .. } => heading.clone(),
            RelExpr::RelParam { heading, .. } => heading.clone(),
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
            // Binary nodes combine operand origins: both pushable → pushable,
            // both materialized → in-process, else a materialization boundary.
            RelExpr::And { lhs, rhs } => combine_origin(lhs.origin(), rhs.origin()),
            RelExpr::Or { lhs, rhs } => combine_origin(lhs.origin(), rhs.origin()),
            RelExpr::Minus { lhs, rhs } => combine_origin(lhs.origin(), rhs.origin()),
            RelExpr::Semijoin { lhs, rhs, .. } => combine_origin(lhs.origin(), rhs.origin()),
            // A unary node inherits its input's origin (like Restrict/Project).
            // Note v1 has no `tclose` SQL emission, so a relvar-rooted closure
            // still declines the push (sqlemit errs) and runs in-process — the
            // operand fetch alone pushes.
            RelExpr::TClose { input } => input.origin(),
            // A scalar extend doesn't change what the data is rooted in.
            RelExpr::Extend { input, .. } => input.origin(),
            // Unary re-layouts inherit their input's origin (like TClose); v1 has
            // no `wrap`/`unwrap` SQL emission, so a relvar-rooted one declines
            // the push (sqlemit errs) and restructures in-process.
            RelExpr::Wrap { input, .. } => input.origin(),
            RelExpr::Unwrap { input, .. } => input.origin(),
            // Group/ungroup inherit like TClose. They NEVER push (a
            // relation-valued cell has no flat-column SQL form — `resolve`
            // declines), so a relvar-rooted one fetches its operand via SQL
            // then nests/unnests in-process.
            RelExpr::Group { input, .. } => input.origin(),
            RelExpr::Ungroup { input, .. } => input.origin(),
            RelExpr::MaterializedRelvar { .. } => StorageOrigin::Materialized,
            // A relation value lives in the process; what makes it *pushable*
            // anyway is the rel-param shipping, decided at the cut, not here.
            RelExpr::RelParam { .. } => StorageOrigin::Materialized,
        }
    }

    /// Render this expression as an indented, multi-line RelIR tree for human
    /// inspection (the `coddl explain` subcommand). No trailing newline.
    ///
    /// Honest naming: this is the **as-lowered RelIR** — the Algebra-A core
    /// (`And`) plus the sugar nodes (`Restrict`/`Project`/`Rename`) that are
    /// not yet reduced to the minimal A primitives (the operators-as-relations
    /// desugaring `docs/relir.md` never materializes). It is not "optimized"
    /// (there is no optimizer) and not minimal Algebra A.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out, 0);
        out.truncate(out.trim_end().len());
        out
    }

    fn render_into(&self, out: &mut String, depth: usize) {
        use std::fmt::Write as _;
        let pad = "  ".repeat(depth);
        match self {
            RelExpr::RelvarRef {
                name,
                database,
                table_name,
                ..
            } => {
                let _ = writeln!(
                    out,
                    "{pad}RelvarRef {name} {{ db: {database}, table: {table_name} }}"
                );
            }
            RelExpr::MaterializedRelvar { name, .. } => {
                let _ = writeln!(out, "{pad}MaterializedRelvar {name}");
            }
            RelExpr::Restrict { input, pred } => {
                let _ = writeln!(out, "{pad}Restrict {{ {} }}", render_predicate(pred));
                input.render_into(out, depth + 1);
            }
            RelExpr::Project { input, keep } => {
                let _ = writeln!(out, "{pad}Project {{ keep: {} }}", keep.join(", "));
                input.render_into(out, depth + 1);
            }
            RelExpr::Rename { input, renames } => {
                let pairs = renames
                    .iter()
                    .map(|(old, new)| format!("{old} -> {new}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "{pad}Rename {{ {pairs} }}");
                input.render_into(out, depth + 1);
            }
            RelExpr::And { lhs, rhs } => {
                let _ = writeln!(out, "{pad}And");
                lhs.render_into(out, depth + 1);
                rhs.render_into(out, depth + 1);
            }
            RelExpr::Or { lhs, rhs } => {
                let _ = writeln!(out, "{pad}Or");
                lhs.render_into(out, depth + 1);
                rhs.render_into(out, depth + 1);
            }
            RelExpr::Minus { lhs, rhs } => {
                let _ = writeln!(out, "{pad}Minus");
                lhs.render_into(out, depth + 1);
                rhs.render_into(out, depth + 1);
            }
            RelExpr::Semijoin { lhs, rhs, negated } => {
                let _ = writeln!(
                    out,
                    "{pad}{}",
                    if *negated { "Antijoin" } else { "Semijoin" }
                );
                lhs.render_into(out, depth + 1);
                rhs.render_into(out, depth + 1);
            }
            RelExpr::TClose { input } => {
                let _ = writeln!(out, "{pad}TClose");
                input.render_into(out, depth + 1);
            }
            RelExpr::Extend { input, extends } => {
                let pairs = extends
                    .iter()
                    .map(|(name, _ty, e)| format!("{name} = {}", render_scalar(e)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "{pad}Extend {{ {pairs} }}");
                input.render_into(out, depth + 1);
            }
            RelExpr::Wrap { input, wraps } => {
                let pairs = wraps
                    .iter()
                    .map(|(new, h)| {
                        let comps = h
                            .attrs()
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{new}: {{ {comps} }}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "{pad}Wrap {{ {pairs} }}");
                input.render_into(out, depth + 1);
            }
            RelExpr::Unwrap { input, names } => {
                let _ = writeln!(out, "{pad}Unwrap {{ {} }}", names.join(", "));
                input.render_into(out, depth + 1);
            }
            RelExpr::Group { input, groups } => {
                let pairs = groups
                    .iter()
                    .map(|(new, h)| {
                        let comps = h
                            .attrs()
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{new}: {{ {comps} }}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "{pad}Group {{ {pairs} }}");
                input.render_into(out, depth + 1);
            }
            RelExpr::Ungroup { input, names } => {
                let _ = writeln!(out, "{pad}Ungroup {{ {} }}", names.join(", "));
                input.render_into(out, depth + 1);
            }
            RelExpr::RelParam { slot, heading } => {
                let attrs = heading
                    .attrs()
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "{pad}RelParam #{slot} {{ {attrs} }}");
            }
        }
    }

    /// The surviving keys whose attributes all appear in this expression's
    /// heading. A surviving key guarantees row-uniqueness on the (possibly
    /// projected) heading, so the emitted `SELECT` need not be `DISTINCT`.
    ///
    /// `RelvarRef` yields its declared keys; `Restrict`/`Semijoin`/`Minus`
    /// preserve them (each returns a subset of its input); `Project` keeps only
    /// keys whose attributes are all retained; `And` (join) *derives* keys via
    /// the cover and composite rules below. Derived entries may be **superkeys**
    /// rather than minimal candidate keys — sound for every consumer, since the
    /// only property used downstream is uniqueness.
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
            // A natural join of two sets is itself a set. Two rules derive its
            // surviving keys (see docs/sqlemit.md "`DISTINCT` elision"):
            //  - **Cover:** if the join attributes `J = shared` contain a
            //    candidate key of one operand, each row of the *other* operand
            //    matches ≤ 1 row here, so the other operand's keys survive. This
            //    is what lets `compose` (`Project(And)` over a FK→PK join) drop
            //    `DISTINCT`: the surviving single-side key outlives the
            //    projection that removes `J`.
            //  - **Composite:** `k_lhs ∪ k_rhs` is always a superkey (handles a
            //    bare `join`/`times` — including disjoint `times`, where `J` is
            //    empty and the cover rule can't fire).
            // Entries may be derived superkeys, not minimal candidate keys —
            // sound for every consumer (`needs_distinct` only checks non-empty,
            // `card_le_one` only matches genuine length-1 keys, `Project` filters
            // by kept attrs).
            RelExpr::And { lhs, rhs } => {
                let shared = lhs.heading().shared_names(&rhs.heading());
                let covers = |key: &[String]| key.iter().all(|a| shared.contains(a));
                let lhs_keys = lhs.surviving_keys();
                let rhs_keys = rhs.surviving_keys();
                let mut out: Vec<Vec<String>> = Vec::new();
                let add = |k: Vec<String>, out: &mut Vec<Vec<String>>| {
                    if !out.contains(&k) {
                        out.push(k);
                    }
                };
                // Cover rule (symmetric): a key of one side survives when the
                // join attributes cover a key of the *other* side.
                if rhs_keys.iter().any(|k| covers(k)) {
                    for k in &lhs_keys {
                        add(k.clone(), &mut out);
                    }
                }
                if lhs_keys.iter().any(|k| covers(k)) {
                    for k in &rhs_keys {
                        add(k.clone(), &mut out);
                    }
                }
                // Composite rule: `k_lhs ∪ k_rhs` is always a superkey.
                for kl in &lhs_keys {
                    for kr in &rhs_keys {
                        let mut composite = kl.clone();
                        for a in kr {
                            if !composite.contains(a) {
                                composite.push(a.clone());
                            }
                        }
                        add(composite, &mut out);
                    }
                }
                out
            }
            // `union` is keyless in general — two same-heading operands can hold
            // rows that agree on any candidate key without a disjointness proof.
            // (SQL `UNION` dedups itself anyway, so the operand SELECTs need no
            // `DISTINCT` regardless.)
            RelExpr::Or { .. } => Vec::new(),
            // `minus` returns a subset of `lhs` (the `lhs` rows not in `rhs`), so
            // every surviving `lhs` key still identifies a result row — mirrors
            // `Semijoin`.
            RelExpr::Minus { lhs, .. } => lhs.surviving_keys(),
            // Semijoin/antijoin filter `lhs` without changing its heading, so
            // every `lhs` candidate key still uniquely identifies a result tuple
            // — the result is already a set, no `DISTINCT` needed.
            RelExpr::Semijoin { lhs, .. } => lhs.surviving_keys(),
            // Closure introduces new tuples, so the operand's keys need not
            // survive; conservatively keyless.
            RelExpr::TClose { .. } => Vec::new(),
            // Extend adds a column functionally determined by existing ones
            // and removes nothing, so every input key still uniquely
            // identifies a row.
            RelExpr::Extend { input, .. } => input.surviving_keys(),
            // Conservative: wrap/unwrap restructure the heading (attributes a
            // key names may now be nested or lifted), so key tracking through
            // them is deferred; keyless.
            RelExpr::Wrap { .. } => Vec::new(),
            RelExpr::Unwrap { .. } => Vec::new(),
            // Group yields exactly one tuple per distinct survivor combination,
            // so the survivor set is a genuine candidate key — the empty key
            // for a group that consumes every attribute is exactly right
            // (cardinality ≤ 1, like a nullary rel-param). Any input key made
            // of survivors also survives: it pins one input tuple, hence one
            // survivor combination.
            RelExpr::Group { input, groups } => {
                let consumed: std::collections::HashSet<&str> = groups
                    .iter()
                    .flat_map(|(_, h)| h.attrs().iter().map(|(n, _)| n.as_str()))
                    .collect();
                let survivors: Vec<String> = input
                    .heading()
                    .attrs()
                    .iter()
                    .filter(|(n, _)| !consumed.contains(n.as_str()))
                    .map(|(n, _)| n.clone())
                    .collect();
                let mut out = vec![survivors.clone()];
                for k in input.surviving_keys() {
                    if k.iter().all(|a| survivors.contains(a)) && !out.contains(&k) {
                        out.push(k);
                    }
                }
                out
            }
            // The DBC7 superkey shape (TTM ch. 6): an input key `k` made of
            // survivors pins one input tuple; that tuple's fan-out rows are
            // distinguished by the lifted attributes — so `k ∪ lifted` is a
            // superkey of the result (superkeys are sound here, see above).
            // No survivor-contained input key → keyless (unnesting can
            // produce duplicates; the runtime seals).
            RelExpr::Ungroup { input, names } => {
                let in_heading = input.heading();
                let mut lifted: Vec<String> = Vec::new();
                for name in names {
                    if let Some(Type::Relation(sub)) = in_heading.lookup(name) {
                        lifted.extend(sub.attrs().iter().map(|(n, _)| n.clone()));
                    }
                }
                input
                    .surviving_keys()
                    .into_iter()
                    .filter(|k| k.iter().all(|a| !names.contains(a)))
                    .map(|mut k| {
                        for a in &lifted {
                            if !k.contains(a) {
                                k.push(a.clone());
                            }
                        }
                        k
                    })
                    .collect()
            }
            RelExpr::MaterializedRelvar { .. } => Vec::new(),
            // Every relation value is a set (RM Pro 3 — the in-process seal
            // enforces it), so the full heading is a trivially-true superkey.
            // Sound for every consumer: `needs_distinct` only checks non-empty,
            // `Project` drops the key the moment any attribute is projected
            // away, and for a *nullary* rel-param the empty key is exactly
            // right (a nullary relation holds at most one tuple).
            RelExpr::RelParam { heading, .. } => {
                vec![heading.attrs().iter().map(|(n, _)| n.clone()).collect()]
            }
        }
    }

    /// True when the expression provably has at most one tuple — so any
    /// projection of it is duplicate-free regardless of which attributes
    /// survive.
    ///
    /// A `Restrict` that pins every attribute of some candidate key to a
    /// constant bounds cardinality to ≤ 1. v1 restrictions are a single
    /// `AttrCmp`, and only an **equality** (`=`) pins a value, so this holds iff
    /// the test is `Eq` and the pinned attribute is itself a candidate key of
    /// the input (a `<>`/`<`/`>` test bounds nothing).
    pub fn card_le_one(&self) -> bool {
        match self {
            RelExpr::RelvarRef { .. } => false,
            RelExpr::Restrict { input, pred } => {
                input.card_le_one() || {
                    // Only equality *pins* an attribute to a single value; a
                    // range/exclusion (`<>`, `<`, `>`, …) bounds nothing, and
                    // a gate reads no attribute at all.
                    match pred {
                        Predicate::AttrCmp { attr, op, .. } => {
                            *op == CmpOp::Eq
                                && input
                                    .surviving_keys()
                                    .iter()
                                    .any(|k| k.len() == 1 && &k[0] == attr)
                        }
                        Predicate::Gate(_) => false,
                    }
                }
            }
            RelExpr::Project { input, .. } => input.card_le_one(),
            RelExpr::Rename { input, .. } => input.card_le_one(),
            // A join has ≤ 1 tuple when both operands do (0 or 1 matching pair).
            RelExpr::And { lhs, rhs } => lhs.card_le_one() && rhs.card_le_one(),
            RelExpr::Or { .. } => false,
            RelExpr::Minus { .. } => false,
            // A subset of `lhs`: if `lhs` has ≤ 1 tuple, so does the filtered result.
            RelExpr::Semijoin { lhs, .. } => lhs.card_le_one(),
            RelExpr::TClose { .. } => false,
            // Extend is cardinality-preserving (one input tuple → one output).
            RelExpr::Extend { input, .. } => input.card_le_one(),
            // wrap/unwrap are cardinality-preserving (one tuple → one tuple).
            RelExpr::Wrap { input, .. } => input.card_le_one(),
            RelExpr::Unwrap { input, .. } => input.card_le_one(),
            // Group never *increases* cardinality (one tuple per distinct
            // survivor combination), so ≤ 1 input tuple → ≤ 1 output; and a
            // group that consumes every attribute has at most one output
            // tuple regardless of input size.
            RelExpr::Group { input, groups } => {
                input.card_le_one() || {
                    let consumed: std::collections::HashSet<&str> = groups
                        .iter()
                        .flat_map(|(_, h)| h.attrs().iter().map(|(n, _)| n.as_str()))
                        .collect();
                    input
                        .heading()
                        .attrs()
                        .iter()
                        .all(|(n, _)| consumed.contains(n.as_str()))
                }
            }
            // Unnesting multiplies cardinality (like TClose introduces tuples).
            RelExpr::Ungroup { .. } => false,
            RelExpr::MaterializedRelvar { .. } => false,
            RelExpr::RelParam { .. } => false,
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

    /// Whether any `RelvarRef` leaf in this tree reads the physical `table`. Used
    /// by the assignment lowerer to tell a *self-referential* `R := <… R …>`
    /// (which must be surgical, e.g. `R := R where p`) from an independent
    /// `R := X` (which can be a truncate-and-refill replace-all). A private
    /// `MaterializedRelvar` has no SQL table, so it never matches.
    pub fn references_table(&self, table: &str) -> bool {
        match self {
            RelExpr::RelvarRef { table_name, .. } => table_name == table,
            RelExpr::Restrict { input, .. }
            | RelExpr::Project { input, .. }
            | RelExpr::Rename { input, .. }
            | RelExpr::Extend { input, .. }
            | RelExpr::TClose { input }
            | RelExpr::Wrap { input, .. }
            | RelExpr::Unwrap { input, .. }
            | RelExpr::Group { input, .. }
            | RelExpr::Ungroup { input, .. } => input.references_table(table),
            RelExpr::And { lhs, rhs }
            | RelExpr::Or { lhs, rhs }
            | RelExpr::Minus { lhs, rhs }
            | RelExpr::Semijoin { lhs, rhs, .. } => {
                lhs.references_table(table) || rhs.references_table(table)
            }
            RelExpr::MaterializedRelvar { .. } => false,
            // A relation value carries rows, not a table reference.
            RelExpr::RelParam { .. } => false,
        }
    }

    /// Number of [`RelExpr::RelParam`] leaves in this tree.
    pub fn rel_param_count(&self) -> usize {
        match self {
            RelExpr::RelParam { .. } => 1,
            RelExpr::Restrict { input, .. }
            | RelExpr::Project { input, .. }
            | RelExpr::Rename { input, .. }
            | RelExpr::Extend { input, .. }
            | RelExpr::TClose { input }
            | RelExpr::Wrap { input, .. }
            | RelExpr::Unwrap { input, .. }
            | RelExpr::Group { input, .. }
            | RelExpr::Ungroup { input, .. } => input.rel_param_count(),
            RelExpr::And { lhs, rhs }
            | RelExpr::Or { lhs, rhs }
            | RelExpr::Minus { lhs, rhs }
            | RelExpr::Semijoin { lhs, rhs, .. } => lhs.rel_param_count() + rhs.rel_param_count(),
            RelExpr::RelvarRef { .. } | RelExpr::MaterializedRelvar { .. } => 0,
        }
    }

    /// The cardinality-1 specialization of a root semijoin over a
    /// relation-valued parameter: `L matching {t}` degenerates the existence
    /// test into an equality conjunction on the shared attributes —
    /// `L where shared₁ = t.shared₁ and …`. Returns the rewritten tree (the
    /// semijoin replaced by a [`RelExpr::Restrict`] chain whose values are
    /// [`RestrictValue::SlotCell`] cells of the rhs slot's single row, one
    /// conjunct per shared attribute in canonical order, under the same peeled
    /// projection if any) and the dispatch slot, or `None` when the shape
    /// doesn't qualify.
    ///
    /// The emitter bakes *both* plans; the runtime force point — which is
    /// already holding the shipped relation — picks the specialized one when
    /// the slot has exactly one row (see `coddl_query`). `DISTINCT` elision on
    /// the rewritten tree is not a special case: a pinned key hits the
    /// existing [`RelExpr::card_le_one`] proof.
    ///
    /// v1 limits: `matching` only (`not matching` at cardinality 1 negates a
    /// conjunction — a disjunction, which the predicate surface can't push
    /// yet), and the rhs must be the tree's **only** rel param (a sibling plan
    /// with leftover slots would need sparse slot numbering and marker
    /// expansion of its own).
    pub fn card1_semijoin_specialization(&self) -> Option<(RelExpr, usize)> {
        // Root shape: `Semijoin { negated: false }`, optionally under the
        // peeled root `Project` the semijoin emission recognizes.
        let (lhs, rhs, keep) = match self {
            RelExpr::Semijoin {
                lhs,
                rhs,
                negated: false,
            } => (lhs, rhs, None),
            RelExpr::Project { input, keep } => match input.as_ref() {
                RelExpr::Semijoin {
                    lhs,
                    rhs,
                    negated: false,
                } => (lhs, rhs, Some(keep.clone())),
                _ => return None,
            },
            _ => return None,
        };
        let RelExpr::RelParam { slot, heading } = rhs.as_ref() else {
            return None;
        };
        if self.rel_param_count() != 1 {
            return None;
        }
        // The rhs is narrowed to exactly the shared attributes at build time
        // (`build_rel_binary`); a wider rhs would leave cells that don't map
        // to equality conjuncts, so require the narrowed form.
        let shared = lhs.heading().shared_names(heading);
        if shared.len() != heading.len() {
            return None;
        }
        // One equality conjunct per shared attribute, cell indices in the
        // heading's canonical order — the same order the runtime decodes the
        // shipped row's cells in, which is what lets it bind them positionally
        // after the scalar params.
        let mut rewritten = (**lhs).clone();
        for (cell, (attr, _)) in heading.attrs().iter().enumerate() {
            rewritten = RelExpr::Restrict {
                input: Box::new(rewritten),
                pred: Predicate::AttrCmp {
                    attr: attr.clone(),
                    op: CmpOp::Eq,
                    value: RestrictValue::SlotCell { slot: *slot, cell },
                },
            };
        }
        if let Some(keep) = keep {
            rewritten = RelExpr::Project {
                input: Box::new(rewritten),
                keep,
            };
        }
        Some((rewritten, *slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmpop_negate_is_logical_complement() {
        // `negate` is logical NOT (for the UPDATE complement), distinct from
        // `flip` (operand swap): negate(Lt) is GtEq, flip(Lt) is Gt.
        assert_eq!(CmpOp::Eq.negate(), CmpOp::Ne);
        assert_eq!(CmpOp::Ne.negate(), CmpOp::Eq);
        assert_eq!(CmpOp::Lt.negate(), CmpOp::GtEq);
        assert_eq!(CmpOp::GtEq.negate(), CmpOp::Lt);
        assert_eq!(CmpOp::Gt.negate(), CmpOp::LtEq);
        assert_eq!(CmpOp::LtEq.negate(), CmpOp::Gt);
        // Negation is an involution.
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::LtEq,
            CmpOp::Gt,
            CmpOp::GtEq,
        ] {
            assert_eq!(op.negate().negate(), op);
        }
    }

    #[test]
    fn references_table_walks_to_the_relvar_leaf() {
        // The bare leaf, and through `Restrict`/`Or`.
        assert!(greetings().references_table("greetings"));
        assert!(!greetings().references_table("other"));
        let restricted = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: id_eq_1(),
        };
        assert!(restricted.references_table("greetings"));
        // A private relvar has no SQL table — never matches.
        let private = RelExpr::MaterializedRelvar {
            name: "Local".to_string(),
            heading: greetings_heading(),
        };
        assert!(!private.references_table("greetings"));
        // One branch of an `Or` references it.
        let mixed = RelExpr::Or {
            lhs: Box::new(private),
            rhs: Box::new(restricted),
        };
        assert!(mixed.references_table("greetings"));
    }

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
        Predicate::AttrCmp {
            attr: "id".to_string(),
            op: CmpOp::Eq,
            value: RestrictValue::Lit(Literal::Integer(1)),
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

    // ── render (coddl explain) ────────────────────────────────────────

    #[test]
    fn render_indents_the_tree_outermost_first() {
        // project { message } (Greetings where id = 1)
        let r = RelExpr::Project {
            input: Box::new(RelExpr::Restrict {
                input: Box::new(greetings()),
                pred: id_eq_1(),
            }),
            keep: vec!["message".to_string()],
        };
        assert_eq!(
            r.render(),
            "Project { keep: message }\n  \
             Restrict { id = 1 }\n    \
             RelvarRef Greetings { db: greetings, table: greetings }"
        );
    }

    #[test]
    fn render_quotes_text_predicate_literals() {
        let r = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrCmp {
                attr: "message".to_string(),
                op: CmpOp::Eq,
                value: RestrictValue::Lit(Literal::Text("hi".to_string())),
            },
        };
        assert!(
            r.render().contains(r#"Restrict { message = "hi" }"#),
            "text literals render quoted: {}",
            r.render()
        );
    }

    #[test]
    fn render_shows_bound_param_as_placeholder() {
        // A restriction against a bound local renders its value as `:name` — the
        // explain-output placeholder for a runtime-bound value.
        let r = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrCmp {
                attr: "message".to_string(),
                op: CmpOp::Eq,
                value: RestrictValue::Param("wanted".to_string()),
            },
        };
        assert!(
            r.render().contains("Restrict { message = :wanted }"),
            "bound params render as :name — {}",
            r.render()
        );
    }

    #[test]
    fn key_equality_against_a_param_still_bounds_cardinality() {
        // `Greetings where id = :p` — a key equality pins ≤ 1 tuple regardless of
        // whether the value is a literal or a runtime-bound parameter.
        let r = RelExpr::Restrict {
            input: Box::new(greetings()),
            pred: Predicate::AttrCmp {
                attr: "id".to_string(),
                op: CmpOp::Eq,
                value: RestrictValue::Param("p".to_string()),
            },
        };
        assert!(r.card_le_one());
        assert!(!r.needs_distinct());
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
    fn non_equality_on_key_does_not_bound_cardinality() {
        // `id <> 1` (or any range op) excludes/ranges over the key but pins no
        // single value — so it does NOT bound cardinality. The difference only
        // shows once a projection drops the key: then `<>` needs `DISTINCT`
        // whereas `=` does not.
        let ne = |op| RelExpr::Project {
            input: Box::new(RelExpr::Restrict {
                input: Box::new(greetings()),
                pred: Predicate::AttrCmp {
                    attr: "id".to_string(),
                    op,
                    value: RestrictValue::Lit(Literal::Integer(1)),
                },
            }),
            keep: vec!["message".to_string()],
        };
        assert!(!ne(CmpOp::Ne).card_le_one());
        assert!(ne(CmpOp::Ne).needs_distinct());
        assert!(!ne(CmpOp::Lt).card_le_one());
        assert!(ne(CmpOp::Gt).needs_distinct());
        // Equality on the key still bounds it even after the projection.
        assert!(ne(CmpOp::Eq).card_le_one());
        assert!(!ne(CmpOp::Eq).needs_distinct());
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

    // ── key inference through joins (cover + composite rules) ──────────

    fn orders() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Orders".to_string(),
            database: "shop".to_string(),
            heading: Heading::new(vec![
                ("order_id".to_string(), Type::Integer),
                ("cust_id".to_string(), Type::Integer),
            ]),
            table_name: "orders".to_string(),
            columns: vec![
                ("order_id".to_string(), "order_id".to_string()),
                ("cust_id".to_string(), "cust_id".to_string()),
            ],
            keys: vec![vec!["order_id".to_string()]],
        }
    }

    fn customers() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Customers".to_string(),
            database: "shop".to_string(),
            heading: Heading::new(vec![
                ("cust_id".to_string(), Type::Integer),
                ("cname".to_string(), Type::Text),
            ]),
            table_name: "customers".to_string(),
            columns: vec![
                ("cust_id".to_string(), "cust_id".to_string()),
                ("cname".to_string(), "cname".to_string()),
            ],
            keys: vec![vec!["cust_id".to_string()]],
        }
    }

    fn sizes() -> RelExpr {
        RelExpr::RelvarRef {
            name: "Sizes".to_string(),
            database: "greetings".to_string(),
            heading: Heading::new(vec![("size".to_string(), Type::Text)]),
            table_name: "sizes".to_string(),
            columns: vec![("size".to_string(), "size".to_string())],
            keys: vec![vec!["size".to_string()]],
        }
    }

    #[test]
    fn join_covering_a_key_lets_the_other_side_key_survive() {
        // Orders join Customers on cust_id. The join key {cust_id} covers
        // Customers' key, so each Order matches ≤ 1 Customer and Orders' key
        // {order_id} survives — the bare join is already a set, no DISTINCT.
        let j = RelExpr::And {
            lhs: Box::new(orders()),
            rhs: Box::new(customers()),
        };
        assert!(
            j.surviving_keys().contains(&vec!["order_id".to_string()]),
            "orders' key survives the FK->PK join: {:?}",
            j.surviving_keys()
        );
        assert!(!j.needs_distinct());
    }

    #[test]
    fn compose_over_fk_pk_join_elides_distinct() {
        // Orders compose Customers = Project(And, keep = {order_id, cname}),
        // dropping the shared cust_id. {order_id} survives the projection, so the
        // result is provably unique: no DISTINCT. (The case that motivated this.)
        let compose = RelExpr::Project {
            input: Box::new(RelExpr::And {
                lhs: Box::new(orders()),
                rhs: Box::new(customers()),
            }),
            keep: vec!["order_id".to_string(), "cname".to_string()],
        };
        assert_eq!(compose.surviving_keys(), vec![vec!["order_id".to_string()]]);
        assert!(!compose.needs_distinct());
    }

    #[test]
    fn join_projected_below_every_key_keeps_distinct() {
        // Project the compose down to just {cname}: order_id (the only surviving
        // key) is gone, so two orders for one customer collapse — DISTINCT stays.
        // The soundness guard that the derived keys don't over-elide.
        let r = RelExpr::Project {
            input: Box::new(RelExpr::And {
                lhs: Box::new(orders()),
                rhs: Box::new(customers()),
            }),
            keep: vec!["cname".to_string()],
        };
        assert!(r.surviving_keys().is_empty());
        assert!(
            r.needs_distinct(),
            "dropping every key may create duplicates"
        );
    }

    #[test]
    fn times_composite_key_elides_distinct() {
        // Greetings times Sizes (disjoint headings): the cover rule can't fire
        // (empty join key), but the composite {id, size} keys the Cartesian
        // product, so a bare times needs no DISTINCT.
        let t = RelExpr::And {
            lhs: Box::new(greetings()),
            rhs: Box::new(sizes()),
        };
        assert!(!t.surviving_keys().is_empty());
        assert!(!t.needs_distinct());
    }

    #[test]
    fn minus_preserves_lhs_keys() {
        // R minus S returns a subset of R, so R's key survives — no DISTINCT.
        let m = RelExpr::Minus {
            lhs: Box::new(greetings()),
            rhs: Box::new(greetings()),
        };
        assert_eq!(m.surviving_keys(), vec![vec!["id".to_string()]]);
        assert!(!m.needs_distinct());
    }

    #[test]
    fn union_stays_keyless() {
        // Two same-heading operands can share key values, so `union` is keyless —
        // conservative (and SQL `UNION` dedups itself anyway).
        let u = RelExpr::Or {
            lhs: Box::new(greetings()),
            rhs: Box::new(greetings()),
        };
        assert!(u.surviving_keys().is_empty());
        assert!(u.needs_distinct());
    }

    // ── rename ────────────────────────────────────────────────────────

    fn renamed() -> RelExpr {
        // Greetings replace {identifier: id, msg: message} — the surface
        // `replace` (bare-ref case) maps to this `Rename` node; the `(old, new)`
        // tuples are (source, target), unchanged by the surface direction.
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

    // ── tclose ─────────────────────────────────────────────────────────

    fn edges() -> RelExpr {
        // A binary same-typed graph relvar {from: Integer, to: Integer}.
        RelExpr::RelvarRef {
            name: "Edges".to_string(),
            database: "graph".to_string(),
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

    #[test]
    fn tclose_preserves_heading_and_origin() {
        let r = RelExpr::TClose {
            input: Box::new(edges()),
        };
        assert_eq!(r.heading(), edges().heading());
        assert_eq!(r.origin(), StorageOrigin::RelvarRooted);
        // Closure introduces tuples → conservatively keyless → needs DISTINCT.
        assert!(r.surviving_keys().is_empty());
        assert!(!r.card_le_one());
    }

    #[test]
    fn tclose_renders_one_indented_child() {
        let r = RelExpr::TClose {
            input: Box::new(edges()),
        };
        assert_eq!(
            r.render(),
            "TClose\n  RelvarRef Edges { db: graph, table: edges }"
        );
    }

    #[test]
    fn rename_over_key_restriction_keeps_card_bound() {
        // (Greetings where id = 1) replace {msg: message} — still ≤ 1 tuple.
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

    // ── extend ─────────────────────────────────────────────────────────

    fn doubled() -> RelExpr {
        // Greetings extend { twice: id * id } — adds an Integer column.
        RelExpr::Extend {
            input: Box::new(greetings()),
            extends: vec![(
                "twice".to_string(),
                Type::Integer,
                ScalarExpr::Bin {
                    op: ScalarBinOp::Mul,
                    lhs: Box::new(ScalarExpr::Attr("id".to_string())),
                    rhs: Box::new(ScalarExpr::Attr("id".to_string())),
                },
            )],
        }
    }

    #[test]
    fn extend_adds_attribute_keeps_origin_and_keys() {
        let r = doubled();
        assert_eq!(
            r.heading(),
            Heading::new(vec![
                ("id".to_string(), Type::Integer),
                ("message".to_string(), Type::Text),
                ("twice".to_string(), Type::Integer),
            ])
        );
        assert_eq!(r.origin(), StorageOrigin::RelvarRooted);
        // The key `id` survives (extend removes nothing) → no DISTINCT.
        assert_eq!(r.surviving_keys(), vec![vec!["id".to_string()]]);
        assert!(!r.needs_distinct());
    }

    #[test]
    fn extend_renders_expr_and_one_child() {
        assert_eq!(
            doubled().render(),
            "Extend { twice = (id * id) }\n  \
             RelvarRef Greetings { db: greetings, table: greetings }"
        );
    }

    // ── relation-valued parameters (RelParam) ──────────────────────────

    fn path_param() -> RelExpr {
        // A request-derived local relation shipped as slot 0 — e.g. the wiki's
        // `req.path where ordinality = 1 rename { slug: segment }`.
        RelExpr::RelParam {
            slot: 0,
            heading: Heading::new(vec![
                ("ordinality".to_string(), Type::Integer),
                ("slug".to_string(), Type::Text),
            ]),
        }
    }

    #[test]
    fn rel_param_is_materialized_with_its_carried_heading() {
        let p = path_param();
        assert_eq!(p.origin(), StorageOrigin::Materialized);
        assert_eq!(
            p.heading(),
            Heading::new(vec![
                ("ordinality".to_string(), Type::Integer),
                ("slug".to_string(), Type::Text),
            ])
        );
        assert!(!p.card_le_one());
        assert!(!p.references_table("pages"));
    }

    #[test]
    fn rel_param_full_heading_is_a_key_so_no_distinct() {
        // A relation value is a set, so its whole heading is a superkey: the
        // bare parameter (and anything that keeps every attribute) needs no
        // DISTINCT; projecting any attribute away drops the key.
        let p = path_param();
        assert_eq!(
            p.surviving_keys(),
            vec![vec!["ordinality".to_string(), "slug".to_string()]]
        );
        assert!(!p.needs_distinct());
        let projected = RelExpr::Project {
            input: Box::new(p),
            keep: vec!["slug".to_string()],
        };
        assert!(projected.surviving_keys().is_empty());
        assert!(projected.needs_distinct());
    }

    #[test]
    fn relvar_matching_rel_param_is_mixed() {
        // The wiki shape: a public relvar semijoined against a shipped local.
        let sj = RelExpr::Semijoin {
            lhs: Box::new(greetings()),
            rhs: Box::new(path_param()),
            negated: false,
        };
        assert_eq!(sj.origin(), StorageOrigin::Mixed);
        // The semijoin result heading is the lhs's, and the lhs keys survive.
        assert_eq!(sj.heading(), greetings_heading());
        assert!(!sj.needs_distinct());
    }

    #[test]
    fn rel_param_renders_slot_and_attrs_as_a_leaf() {
        assert_eq!(path_param().render(), "RelParam #0 { ordinality, slug }");
        let sj = RelExpr::Semijoin {
            lhs: Box::new(greetings()),
            rhs: Box::new(path_param()),
            negated: false,
        };
        assert_eq!(
            sj.render(),
            "Semijoin\n  \
             RelvarRef Greetings { db: greetings, table: greetings }\n  \
             RelParam #0 { ordinality, slug }"
        );
    }

    #[test]
    fn nullary_rel_param_empty_key_bounds_it() {
        // A nullary relation value (reltrue/relfalse) holds ≤ 1 tuple; the
        // empty key expresses exactly that, so no DISTINCT.
        let p = RelExpr::RelParam {
            slot: 0,
            heading: Heading::new(vec![]),
        };
        assert_eq!(p.surviving_keys(), vec![Vec::<String>::new()]);
        assert!(!p.needs_distinct());
    }

    /// A rel param already narrowed to the shared attribute `id` — the shape
    /// `build_rel_binary` produces for a semijoin rhs after A1 narrowing.
    fn id_param() -> RelExpr {
        RelExpr::RelParam {
            slot: 0,
            heading: Heading::new(vec![("id".to_string(), Type::Integer)]),
        }
    }

    #[test]
    fn card1_specialization_rewrites_projected_matching_to_a_restrict_chain() {
        // The wiki shape: `Pages matching (…) project { … }` — a peeled root
        // projection over a semijoin whose rhs is the only rel param.
        let expr = RelExpr::Project {
            input: Box::new(RelExpr::Semijoin {
                lhs: Box::new(greetings()),
                rhs: Box::new(id_param()),
                negated: false,
            }),
            keep: vec!["message".to_string()],
        };
        let (rewritten, slot) = expr.card1_semijoin_specialization().unwrap();
        assert_eq!(slot, 0);
        assert_eq!(
            rewritten,
            RelExpr::Project {
                input: Box::new(RelExpr::Restrict {
                    input: Box::new(greetings()),
                    pred: Predicate::AttrCmp {
                        attr: "id".to_string(),
                        op: CmpOp::Eq,
                        value: RestrictValue::SlotCell { slot: 0, cell: 0 },
                    },
                }),
                keep: vec!["message".to_string()],
            }
        );
        // The pinned key drives DISTINCT elision with no new machinery: the
        // equality is on `greetings`' key, so cardinality is provably ≤ 1.
        assert!(rewritten.card_le_one());
        assert!(!rewritten.needs_distinct());
    }

    #[test]
    fn card1_specialization_handles_a_bare_semijoin_and_multi_attr_keys() {
        // No peeled projection; two shared attributes → one equality conjunct
        // per attribute, cells numbered in canonical heading order.
        let both = RelExpr::RelParam {
            slot: 0,
            heading: greetings_heading(),
        };
        let expr = RelExpr::Semijoin {
            lhs: Box::new(greetings()),
            rhs: Box::new(both),
            negated: false,
        };
        let (rewritten, slot) = expr.card1_semijoin_specialization().unwrap();
        assert_eq!(slot, 0);
        let RelExpr::Restrict { input, pred } = &rewritten else {
            panic!("expected the outer conjunct, got {rewritten:?}");
        };
        // Canonical order: `id` (cell 0) innermost, `message` (cell 1) outer.
        assert_eq!(
            *pred,
            Predicate::AttrCmp {
                attr: "message".to_string(),
                op: CmpOp::Eq,
                value: RestrictValue::SlotCell { slot: 0, cell: 1 },
            }
        );
        let RelExpr::Restrict { input, pred } = input.as_ref() else {
            panic!("expected the inner conjunct");
        };
        assert_eq!(
            *pred,
            Predicate::AttrCmp {
                attr: "id".to_string(),
                op: CmpOp::Eq,
                value: RestrictValue::SlotCell { slot: 0, cell: 0 },
            }
        );
        assert_eq!(**input, greetings());
    }

    #[test]
    fn card1_specialization_declines_out_of_scope_shapes() {
        // Antijoin: cardinality-1 `not matching` negates the conjunction — a
        // disjunction the predicate surface can't push yet (B1).
        let anti = RelExpr::Semijoin {
            lhs: Box::new(greetings()),
            rhs: Box::new(id_param()),
            negated: true,
        };
        assert_eq!(anti.card1_semijoin_specialization(), None);
        // Structural rhs: no shipped slot to dispatch on.
        let structural = RelExpr::Semijoin {
            lhs: Box::new(greetings()),
            rhs: Box::new(RelExpr::Project {
                input: Box::new(greetings()),
                keep: vec!["id".to_string()],
            }),
            negated: false,
        };
        assert_eq!(structural.card1_semijoin_specialization(), None);
        // A rhs wider than the shared attributes (not the narrowed form):
        // `ordinality` has no `greetings` counterpart to pin.
        let wide = RelExpr::Semijoin {
            lhs: Box::new(greetings()),
            rhs: Box::new(path_param()),
            negated: false,
        };
        assert_eq!(wide.card1_semijoin_specialization(), None);
        // A second rel param elsewhere in the tree: the sibling plan would
        // inherit its marker — out of v1 scope.
        let two_slots = RelExpr::Semijoin {
            lhs: Box::new(RelExpr::And {
                lhs: Box::new(greetings()),
                rhs: Box::new(RelExpr::RelParam {
                    slot: 0,
                    heading: Heading::new(vec![("id".to_string(), Type::Integer)]),
                }),
            }),
            rhs: Box::new(RelExpr::RelParam {
                slot: 1,
                heading: Heading::new(vec![("id".to_string(), Type::Integer)]),
            }),
            negated: false,
        };
        assert_eq!(two_slots.card1_semijoin_specialization(), None);
    }
}

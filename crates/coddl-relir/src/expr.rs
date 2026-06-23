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
    TClose {
        input: Box<RelExpr>,
    },
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
    /// An in-memory (`private`) relvar read — the materialized counterpart of
    /// the relvar-rooted `RelvarRef` leaf. No SQL source, so any subtree
    /// containing it is `Materialized` and lowers in-process.
    MaterializedRelvar {
        name: String,
        heading: Heading,
    },
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

/// Render a restriction predicate for `RelExpr::render` (e.g. `id <> 1`).
fn render_predicate(pred: &Predicate) -> String {
    match pred {
        Predicate::AttrCmp { attr, op, value } => {
            format!("{attr} {} {}", op.sql(), render_literal(value))
        }
    }
}

/// Render a scalar literal for `RelExpr::render`. `Text` is quoted so the
/// rendered predicate is unambiguous; `Integer`/`Boolean` print bare.
fn render_literal(lit: &Literal) -> String {
    match lit {
        Literal::Integer(n) => n.to_string(),
        Literal::Text(s) => format!("{s:?}"),
        Literal::Boolean(b) => b.to_string(),
    }
}

/// A restriction predicate: a single `<attr> <cmp> <literal>` test. This grows
/// to conjunction/disjunction and attribute-vs-attribute tests as the surface
/// `where` support grows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predicate {
    /// `<attr> <op> <literal>`.
    AttrCmp {
        attr: String,
        op: CmpOp,
        value: Literal,
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
            // Closure preserves the (binary) operand heading.
            RelExpr::TClose { input } => input.heading(),
            // Input attributes plus each computed `(name, type)`; `Heading::new`
            // re-canonicalizes (re-sorts) with the new attributes mixed in.
            RelExpr::Extend { input, extends } => {
                let mut attrs: Vec<(String, Type)> = input.heading().attrs().to_vec();
                attrs.extend(extends.iter().map(|(name, ty, _)| (name.clone(), ty.clone())));
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
            RelExpr::MaterializedRelvar { heading, .. } => heading.clone(),
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
            RelExpr::MaterializedRelvar { .. } => StorageOrigin::Materialized,
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
                let _ = writeln!(out, "{pad}RelvarRef {name} {{ db: {database}, table: {table_name} }}");
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
            // Conservative: join / union key-inference is deferred. (A `union`
            // pushes as a SQL `UNION`, which dedups itself, so no DISTINCT is
            // needed on the operand SELECTs anyway.)
            RelExpr::And { .. } => Vec::new(),
            RelExpr::Or { .. } => Vec::new(),
            // Conservative: `minus` preserves `lhs`'s keys (the result is a
            // subset of `lhs`), but lhs-key preservation is deferred.
            RelExpr::Minus { .. } => Vec::new(),
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
            RelExpr::MaterializedRelvar { .. } => Vec::new(),
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
                    // range/exclusion (`<>`, `<`, `>`, …) bounds nothing.
                    let Predicate::AttrCmp { attr, op, .. } = pred;
                    *op == CmpOp::Eq
                        && input
                            .surviving_keys()
                            .iter()
                            .any(|k| k.len() == 1 && &k[0] == attr)
                }
            }
            RelExpr::Project { input, .. } => input.card_le_one(),
            RelExpr::Rename { input, .. } => input.card_le_one(),
            RelExpr::And { .. } => false,
            RelExpr::Or { .. } => false,
            RelExpr::Minus { .. } => false,
            RelExpr::TClose { .. } => false,
            // Extend is cardinality-preserving (one input tuple → one output).
            RelExpr::Extend { input, .. } => input.card_le_one(),
            // wrap/unwrap are cardinality-preserving (one tuple → one tuple).
            RelExpr::Wrap { input, .. } => input.card_le_one(),
            RelExpr::Unwrap { input, .. } => input.card_le_one(),
            RelExpr::MaterializedRelvar { .. } => false,
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
                value: Literal::Text("hi".to_string()),
            },
        };
        assert!(
            r.render().contains(r#"Restrict { message = "hi" }"#),
            "text literals render quoted: {}",
            r.render()
        );
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
                    value: Literal::Integer(1),
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
}

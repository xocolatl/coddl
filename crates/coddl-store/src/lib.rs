//! Compile-time evaluation of a `.cdstore` document into `coddl::storage`.
//!
//! A `.cdstore` is DML into the storage meta-catalog: a bare sequence of
//! statements (`insert Backends Relation { … };`, `ConnDefault := ConnDefault
//! union Relation { … };`) that describe how each database is reached. The old
//! `.cdstore` never ran — the compiler read it as compile-time data. The new one
//! works the same way, only relationally: the compiler **evaluates** the DML at
//! compile time, applying each insert/assignment to build the `coddl::storage`
//! relation values it later queries. Nothing here emits a binary or runs at
//! runtime.
//!
//! The evaluation is the relational sibling of `coddl-provision`'s INIT fold: it
//! walks `Relation { … }` literals through the shared constant-folder
//! [`coddl_consteval::fold_const_scalar`], and combines them with the relvar's
//! current value under `union` / `minus`. It reuses the typechecker
//! ([`coddl_types::check`] over [`FileKind::Cdstore`]) — a document that doesn't
//! typecheck is never evaluated. Evaluation errors live in the `SE####`
//! namespace and leave the catalog partial.

use std::collections::BTreeMap;

use coddl_consteval::fold_const_scalar;
use coddl_diagnostics::{Diagnostic, FileId, Severity, Span};
use coddl_relir::Literal;
use coddl_syntax::ast::{AstNode, BinaryOp, Expr, Stmt};
use coddl_syntax::ast_cdstore::CdstoreRoot;
use coddl_syntax::cst::SyntaxNode;
use coddl_syntax::FileKind;
use coddl_types::RelvarTable;

/// One tuple of a storage relation: its `(attribute, value)` pairs, kept sorted
/// by attribute name so two tuples with the same fields compare equal regardless
/// of source order (a heading is unordered — RM Pro 1).
pub type Tuple = Vec<(String, Literal)>;

/// A storage relation value — a *set* of [`Tuple`]s (deduplicated; RM Pro 2).
/// Small, so membership/dedup are linear; no `Ord`/`Hash` on [`Literal`] needed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Relation {
    pub tuples: Vec<Tuple>,
}

/// The evaluated `coddl::storage` meta-catalog: each storage relvar the document
/// wrote to, mapped to its relation value. Name-sorted (`BTreeMap`) for
/// deterministic output.
#[derive(Clone, Debug, Default)]
pub struct StorageCatalog {
    pub relvars: BTreeMap<String, Relation>,
}

/// The result of evaluating a `.cdstore`: the storage catalog it produces plus
/// every diagnostic (parse, typecheck, and evaluation). The catalog is complete
/// only when `diagnostics` carries no error.
pub struct CdstoreEval {
    pub catalog: StorageCatalog,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse, typecheck, and compile-time-evaluate a `.cdstore` document's DML into
/// the `coddl::storage` relation values. A document that fails to typecheck is
/// not evaluated — its values would be meaningless — so its errors are returned
/// with an empty catalog.
pub fn evaluate_cdstore(source: &str, file: FileId) -> CdstoreEval {
    let check = coddl_types::check(source, file, FileKind::Cdstore);
    let mut diagnostics = check.diagnostics;

    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return CdstoreEval {
            catalog: StorageCatalog::default(),
            diagnostics,
        };
    }

    let Some(root) = CdstoreRoot::cast(check.tree.clone()) else {
        return CdstoreEval {
            catalog: StorageCatalog::default(),
            diagnostics,
        };
    };

    let mut ev = Evaluator {
        relvars: &check.relvars,
        file,
        catalog: StorageCatalog::default(),
        diagnostics: Vec::new(),
    };
    for stmt in root.stmts() {
        ev.apply_stmt(&stmt);
    }

    diagnostics.append(&mut ev.diagnostics);
    CdstoreEval {
        catalog: ev.catalog,
        diagnostics,
    }
}

struct Evaluator<'a> {
    /// The storage relvars in scope (headings + candidate keys), from the check.
    relvars: &'a RelvarTable,
    file: FileId,
    catalog: StorageCatalog,
    diagnostics: Vec<Diagnostic>,
}

impl Evaluator<'_> {
    /// Apply one statement to the catalog. `insert`/`:=`/`truncate` are the DML a
    /// `.cdstore` uses; `delete`/`update` are not yet evaluated (SE0004) and any
    /// non-DML statement is rejected (SE0005) — hard errors, never silent skips.
    fn apply_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Insert(ins) => {
                let (Some(target), Some(source)) = (ins.target(), ins.source()) else {
                    return;
                };
                let Some(name) = relvar_name(&target) else {
                    return;
                };
                let Some(added) = self.eval_rel(&source) else {
                    return;
                };
                let mut current = self.catalog.relvars.remove(&name).unwrap_or_default();
                union_into(&mut current, added);
                self.check_key(&name, &current, stmt.syntax());
                self.catalog.relvars.insert(name, current);
            }
            Stmt::Assign(a) => {
                let (Some(target), Some(value)) = (a.target(), a.value()) else {
                    return;
                };
                let Some(name) = relvar_name(&target) else {
                    return;
                };
                let Some(rel) = self.eval_rel(&value) else {
                    return;
                };
                self.check_key(&name, &rel, stmt.syntax());
                self.catalog.relvars.insert(name, rel);
            }
            Stmt::Truncate(t) => {
                let Some(op) = t.operand() else { return };
                let Some(name) = relvar_name(&op) else { return };
                self.catalog.relvars.insert(name, Relation::default());
            }
            Stmt::Delete(_) | Stmt::Update(_) => {
                self.error(
                    stmt.syntax(),
                    "SE0004",
                    "`delete` / `update` in a `.cdstore` are not yet evaluated at compile time",
                );
            }
            other => {
                self.error(
                    other.syntax(),
                    "SE0005",
                    "a `.cdstore` may only contain DML into `coddl::storage` relvars",
                );
            }
        }
    }

    /// Evaluate a relational expression to its [`Relation`] value. Covers the
    /// forms a `.cdstore`'s DML uses: a bare relvar name (its current value), a
    /// `Relation { … }` literal, and `union` / `minus` over them. Any other
    /// operator is SE0001.
    fn eval_rel(&mut self, expr: &Expr) -> Option<Relation> {
        match expr {
            Expr::NameRef(n) => {
                let name = n.ident()?.text().to_string();
                // A relvar not yet written to reads as empty (RM: an unpopulated
                // relvar is the empty relation, never undefined).
                Some(self.catalog.relvars.get(&name).cloned().unwrap_or_default())
            }
            Expr::RelationLit(rel) => {
                let mut out = Relation::default();
                let mut ok = true;
                for element in rel.elements() {
                    match self.eval_tuple(&element) {
                        Some(t) => push_unique(&mut out.tuples, t),
                        None => ok = false,
                    }
                }
                ok.then_some(out)
            }
            Expr::Binary(b) => {
                let op = b.op_kind()?;
                let l = self.eval_rel(&b.lhs()?)?;
                let r = self.eval_rel(&b.rhs()?)?;
                match op {
                    BinaryOp::Union => {
                        let mut out = l;
                        union_into(&mut out, r);
                        Some(out)
                    }
                    BinaryOp::Minus => Some(minus(l, r)),
                    _ => {
                        self.error(
                            expr.syntax(),
                            "SE0001",
                            format!(
                                "`{op:?}` is not a relational operator evaluated in a `.cdstore` \
                                 (only `union` / `minus` over storage relvars and `Relation {{ … }}` \
                                 literals)"
                            ),
                        );
                        None
                    }
                }
            }
            _ => {
                self.error(
                    expr.syntax(),
                    "SE0001",
                    "unsupported relational expression in a `.cdstore`",
                );
                None
            }
        }
    }

    /// Evaluate one relation element into a [`Tuple`] — a `{ a: 1, b: 2 }` tuple
    /// literal whose cells are constant scalars, folded via
    /// [`fold_const_scalar`]. Pairs are sorted by attribute name for set
    /// equality. `None` (with a diagnostic) on any non-tuple element or
    /// non-constant / failed cell.
    fn eval_tuple(&mut self, element: &Expr) -> Option<Tuple> {
        let Expr::TupleLit(tuple) = element else {
            self.error(
                element.syntax(),
                "SE0002",
                "each `.cdstore` relation element must be a tuple literal `{ … }`",
            );
            return None;
        };
        let mut pairs: Tuple = Vec::new();
        let mut ok = true;
        for field in tuple.fields() {
            let (Some(name_tok), Some(value_expr)) = (field.name(), field.value()) else {
                ok = false;
                continue;
            };
            let attr = name_tok.text().to_string();
            match fold_const_scalar(&value_expr, &|_| None) {
                Ok(Some(lit)) => pairs.push((attr, lit)),
                Ok(None) => {
                    self.error(
                        value_expr.syntax(),
                        "SE0002",
                        format!("value for `{attr}` is not a constant scalar"),
                    );
                    ok = false;
                }
                Err(msg) => {
                    self.error(
                        value_expr.syntax(),
                        "SE0003",
                        format!("evaluating the value for `{attr}` failed: {msg}"),
                    );
                    ok = false;
                }
            }
        }
        if !ok {
            return None;
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Some(pairs)
    }

    /// Enforce the relvar's first candidate key: two tuples that share the key
    /// but differ in a non-key attribute are a **compiler error** (SE0006).
    /// Exact-duplicate tuples were already coalesced (a relation is a set), so
    /// any surviving key collision is a genuine conflict.
    fn check_key(&mut self, name: &str, rel: &Relation, at: &SyntaxNode) {
        let Some(key) = self.relvars.get(name).and_then(|i| i.keys.first()) else {
            return;
        };
        let mut seen: Vec<Vec<Literal>> = Vec::new();
        for t in &rel.tuples {
            let kv: Vec<Literal> = key
                .iter()
                .filter_map(|k| t.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()))
                .collect();
            if kv.len() != key.len() {
                continue; // a tuple missing a key attribute — the checker's job
            }
            if seen.contains(&kv) {
                self.error(
                    at,
                    "SE0006",
                    format!(
                        "relvar `{name}`: two tuples share the key {{ {} }} but differ in a \
                         non-key attribute",
                        key.join(", ")
                    ),
                );
                return;
            }
            seen.push(kv);
        }
    }

    fn error(&mut self, node: &SyntaxNode, code: &'static str, message: impl Into<String>) {
        let r = node.text_range();
        let span = Span::new(self.file, r.start().into(), r.end().into());
        self.diagnostics
            .push(Diagnostic::error(span, code, message));
    }
}

/// The bare relvar name of a DML target (`insert R …`, `R := …`), or `None` if
/// the target is not a name reference (the checker already rejected that, T0033).
fn relvar_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::NameRef(n) => n.ident().map(|t| t.text().to_string()),
        _ => None,
    }
}

/// Append `t` to `tuples` only if not already present (set semantics).
fn push_unique(tuples: &mut Vec<Tuple>, t: Tuple) {
    if !tuples.contains(&t) {
        tuples.push(t);
    }
}

/// Union `src` into `dst` in place (set union).
fn union_into(dst: &mut Relation, src: Relation) {
    for t in src.tuples {
        push_unique(&mut dst.tuples, t);
    }
}

/// Set difference `l − r`.
fn minus(mut l: Relation, r: Relation) -> Relation {
    l.tuples.retain(|t| !r.tuples.contains(t));
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(src: &str) -> CdstoreEval {
        evaluate_cdstore(src, FileId(0))
    }

    /// A relation's tuples as sorted `Vec<Vec<(attr, text)>>` for easy asserts —
    /// every greetings value is Text.
    fn text_rows(rel: &Relation) -> Vec<Vec<(String, String)>> {
        let mut rows: Vec<Vec<(String, String)>> = rel
            .tuples
            .iter()
            .map(|t| {
                t.iter()
                    .map(|(a, v)| {
                        let s = match v {
                            Literal::Text(s) => s.clone(),
                            other => format!("{other:?}"),
                        };
                        (a.clone(), s)
                    })
                    .collect()
            })
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn greetings_cdstore_evaluates_to_storage_relvars() {
        let src = "insert Backends Relation { { database: \"greetings\", backend: \"sqlite\" }, };\n\
                   insert ConnEnv Relation { { database: \"greetings\", backend: \"sqlite\", field: \"file\", env_var: \"HELLO_WORLD_SQLITE_PATH\" }, };\n\
                   ConnDefault := ConnDefault union Relation { { database: \"greetings\", backend: \"sqlite\", field: \"file\", value: \"greetings.sqlite\" }, };\n";
        let out = eval(src);
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );

        let backends = out.catalog.relvars.get("Backends").expect("Backends");
        assert_eq!(
            text_rows(backends),
            vec![vec![
                ("backend".into(), "sqlite".into()),
                ("database".into(), "greetings".into()),
            ]]
        );

        let conn_env = out.catalog.relvars.get("ConnEnv").expect("ConnEnv");
        assert_eq!(
            text_rows(conn_env),
            vec![vec![
                ("backend".into(), "sqlite".into()),
                ("database".into(), "greetings".into()),
                ("env_var".into(), "HELLO_WORLD_SQLITE_PATH".into()),
                ("field".into(), "file".into()),
            ]]
        );

        let conn_default = out.catalog.relvars.get("ConnDefault").expect("ConnDefault");
        assert_eq!(
            text_rows(conn_default),
            vec![vec![
                ("backend".into(), "sqlite".into()),
                ("database".into(), "greetings".into()),
                ("field".into(), "file".into()),
                ("value".into(), "greetings.sqlite".into()),
            ]]
        );
    }

    #[test]
    fn duplicate_insert_coalesces_as_a_set() {
        // The same tuple inserted twice is one tuple (a relation is a set).
        let src = "insert Backends Relation { { database: \"g\", backend: \"sqlite\" }, };\n\
                   insert Backends Relation { { database: \"g\", backend: \"sqlite\" }, };\n";
        let out = eval(src);
        assert!(
            out.diagnostics.is_empty(),
            "diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(out.catalog.relvars["Backends"].tuples.len(), 1);
    }

    #[test]
    fn key_conflict_is_se0006() {
        // Backends key is { database, backend }; two rows share it but the
        // relation-literal heading is identical, so a differing non-key attr is
        // needed — Backends is all-key, so use ConnDefault (key excludes value).
        let src = "insert ConnDefault Relation { { database: \"g\", backend: \"sqlite\", field: \"file\", value: \"a.sqlite\" }, };\n\
                   insert ConnDefault Relation { { database: \"g\", backend: \"sqlite\", field: \"file\", value: \"b.sqlite\" }, };\n";
        let out = eval(src);
        assert!(
            out.diagnostics.iter().any(|d| d.code == "SE0006"),
            "expected SE0006, got: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn typecheck_error_short_circuits_evaluation() {
        // A heading mismatch (T0034) means the DML never evaluates.
        let src = "insert Backends Relation { { database: \"g\", backend: \"sqlite\", bogus: \"x\" }, };\n";
        let out = eval(src);
        assert!(out.diagnostics.iter().any(|d| d.code == "T0034"));
        assert!(
            out.catalog.relvars.is_empty(),
            "catalog must stay empty on a type error"
        );
    }
}

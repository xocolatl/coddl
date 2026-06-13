//! AST → ProcIR lowering pass.
//!
//! `lower(source, file)` runs the lexer, parser, typechecker, and (if
//! the result is diagnostic-free) walks the AST into a `Module`. The
//! walk mirrors the typechecker's structure: every `check_<x>` in
//! `coddl-types::checker` has a sibling `lower_<x>` here.
//!
//! Lowering is defined to be infallible on a clean typecheck. Cases
//! that aren't reachable on diagnostic-free input (an unresolved
//! callee, a malformed call) hit `unreachable!()`; tests cover the
//! reachable ones. The `L####` namespace is reserved for the future.

use std::collections::{HashMap, HashSet};

use coddl_diagnostics::{Diagnostic, FileId, Severity, Span};
use coddl_syntax::ast::{
    AstNode, BinaryExpr, BinaryOp, Block, BoolLit, CallExpr, Expr, ExprStmt, FieldAccess, Item,
    LetStmt, Literal, NameRef, NamedArg, OperDecl, ProgramDecl, RelationLit, Root, Stmt,
    TransactionExpr, TupleLit, UnaryExpr, UnaryOp,
};
use coddl_syntax::SyntaxKind;
use coddl_types::{check, Heading, Type};

use crate::ir::{
    BasicBlock, BlockId, Const, Function, HeadingId, Inst, Module, ProcType, ScalarOp, Terminator,
    ValueId,
};

/// Surface name → C-ABI linkage name for each runtime extern. The
/// table is short by design; every entry corresponds to a built-in
/// operator the typechecker already knows.
const BUILTIN_EXTERNS: &[BuiltinExtern] = &[BuiltinExtern {
    surface: "write_line",
    linkage: "coddl_write_line",
    params: &[("message", ProcType::Text)],
    return_type: ProcType::Unit,
}];

struct BuiltinExtern {
    surface: &'static str,
    linkage: &'static str,
    params: &'static [(&'static str, ProcType)],
    return_type: ProcType,
}

/// The output of one `lower` pass. `module` is `None` iff any error
/// diagnostic was emitted upstream — the lowering pass refuses to
/// shape an IR for a program that didn't typecheck cleanly.
#[derive(Debug)]
pub struct LowerOutput {
    pub module: Option<Module>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Tokenize, parse, type-check, and lower `source` to ProcIR.
/// Lowering is `.cd`-only — `.cddb`, `.cdmap`, and `.cdstore` describe
/// storage shape that the typechecker and the (Phase 16) plan layer
/// consume; they have no procedural lowering.
pub fn lower(source: &str, file: FileId) -> LowerOutput {
    let check_out = check(source, file, coddl_syntax::FileKind::Cd);
    let has_errors = check_out
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error);
    if has_errors {
        return LowerOutput {
            module: None,
            diagnostics: check_out.diagnostics,
        };
    }
    let root = Root::cast(check_out.tree).expect("parser always returns a Root");
    let mut lowerer = Lowerer::new(file);
    let module = lowerer.lower_root(&root);
    // Merge in any diagnostics the lowerer itself emitted (e.g.
    // T0022 for captures in `where` predicates). If the lowerer
    // emitted error-severity diagnostics, the IR is unsafe to
    // codegen — return no module.
    let mut diagnostics = check_out.diagnostics;
    diagnostics.extend(lowerer.diagnostics);
    let lower_errored = diagnostics.iter().any(|d| d.severity == Severity::Error);
    LowerOutput {
        module: if lower_errored { None } else { Some(module) },
        diagnostics,
    }
}

struct Lowerer {
    program_name: String,
    functions: Vec<Function>,
    seen_externs: HashSet<&'static str>,
    /// Per-module interner: maps each unique `Heading` to a
    /// `HeadingId`. Phase 19 backends emit one static descriptor per
    /// entry; `ProcType::Relation(HeadingId)` and `Inst::RelationLit`
    /// reference these by id. Order is push-only (id == index).
    headings: Vec<Heading>,
    heading_ids: HashMap<Heading, HeadingId>,
    /// Source file for diagnostic spans the lowerer itself emits
    /// (e.g. T0022 captures).
    file: FileId,
    /// Lowering-time diagnostics. Merged into `LowerOutput::diagnostics`
    /// at the end of `lower()`.
    diagnostics: Vec<Diagnostic>,
    /// Counter for synthesized predicate function names
    /// (`__coddl_where_<n>`). Per-module; never reset.
    next_where: u32,
    // Per-function state, reset on each `lower_oper_decl`.
    next_value: u32,
    next_block: u32,
    insts: Vec<Inst>,
    /// Stack of binding scopes. The outermost layer is the function's
    /// parameter scope; each `transaction [...]` block pushes a new
    /// layer; `let` statements insert into the topmost layer. Each
    /// entry stores the binding's `ValueId` and its `ProcType` so
    /// later walks (tuple construction, field access) know the static
    /// shape of a `NameRef` lookup result.
    locals: Vec<HashMap<String, (ValueId, ProcType)>>,
    /// Type of every SSA value defined in the current function. Built
    /// up as each `Inst` is emitted; consulted by lowerings that need
    /// the static type of a base expression (notably field access on
    /// a let-bound tuple, where the heading lives in the source
    /// `ValueId`'s `ProcType::Tuple`).
    value_types: HashMap<ValueId, ProcType>,
    /// When lowering a `where`-predicate body, this holds a snapshot
    /// of the enclosing function's `locals` so the NameRef walk can
    /// detect captures (Phase 20 deferral, T0022). `None` outside any
    /// predicate body.
    outer_locals_for_capture: Option<Vec<HashMap<String, (ValueId, ProcType)>>>,
}

impl Lowerer {
    fn new(file: FileId) -> Self {
        Self {
            program_name: String::new(),
            functions: Vec::new(),
            seen_externs: HashSet::new(),
            headings: Vec::new(),
            heading_ids: HashMap::new(),
            file,
            diagnostics: Vec::new(),
            next_where: 0,
            next_value: 0,
            next_block: 0,
            insts: Vec::new(),
            locals: vec![HashMap::new()],
            value_types: HashMap::new(),
            outer_locals_for_capture: None,
        }
    }

    /// Compute a `Span` for an AST node — used when the lowerer emits
    /// a diagnostic against a specific subtree (e.g. T0022 against
    /// the offending `NameRef`).
    fn node_span(&self, node: &coddl_syntax::cst::SyntaxNode) -> Span {
        let r = node.text_range();
        Span::new(self.file, r.start().into(), r.end().into())
    }

    /// Intern a heading: return its existing `HeadingId` or push a new
    /// one. Stable order; backends iterate `Module::headings` in this
    /// order when emitting descriptors.
    fn intern_heading(&mut self, h: &Heading) -> HeadingId {
        if let Some(id) = self.heading_ids.get(h) {
            return *id;
        }
        let id = HeadingId(self.headings.len() as u32);
        self.headings.push(h.clone());
        self.heading_ids.insert(h.clone(), id);
        id
    }

    fn push_local_scope(&mut self) {
        self.locals.push(HashMap::new());
    }

    fn pop_local_scope(&mut self) {
        self.locals.pop();
    }

    fn bind_local(&mut self, name: String, v: ValueId, ty: ProcType) {
        self.locals
            .last_mut()
            .expect("scope stack empty")
            .insert(name, (v, ty));
    }

    fn lookup_local(&self, name: &str) -> Option<(ValueId, ProcType)> {
        self.locals
            .iter()
            .rev()
            .find_map(|l| l.get(name).cloned())
    }

    fn fresh_value(&mut self) -> ValueId {
        let v = ValueId(self.next_value);
        self.next_value += 1;
        v
    }

    fn fresh_block(&mut self) -> BlockId {
        let b = BlockId(self.next_block);
        self.next_block += 1;
        b
    }

    fn reset_function_state(&mut self) {
        self.next_value = 0;
        self.next_block = 0;
        self.insts.clear();
        self.locals.clear();
        self.locals.push(HashMap::new());
        self.value_types.clear();
    }

    /// Look up an SSA value's static type. Diagnostic-free input always
    /// has a recorded type for every consumed value; an unbound id is
    /// an internal lowerer bug.
    fn value_type(&self, v: ValueId) -> ProcType {
        self.value_types
            .get(&v)
            .cloned()
            .unwrap_or_else(|| unreachable!("unbound ValueId {v}"))
    }

    /// Bind a freshly-defined SSA value to its `ProcType`. Every
    /// instruction-emission helper goes through this so `value_types`
    /// stays in sync without per-call-site duplication.
    fn record_type(&mut self, v: ValueId, ty: ProcType) {
        self.value_types.insert(v, ty);
    }

    /// True iff `ty` describes a heap-managed value that needs RC
    /// retain/release. Phase 19: relations only. Future phases add
    /// sequences and possibly text owners.
    fn is_heap_managed(ty: &ProcType) -> bool {
        matches!(ty, ProcType::Relation(_))
    }

    /// Emit `Inst::Release` for every heap-managed binding in the
    /// topmost local scope, in unspecified (HashMap) order. Called
    /// before popping a scope (transaction exit) and at function
    /// epilogue, before any terminator or runtime-shutdown call.
    fn release_top_scope_heap_locals(&mut self) {
        let releases: Vec<ValueId> = self
            .locals
            .last()
            .expect("scope stack empty")
            .values()
            .filter(|(_, ty)| Self::is_heap_managed(ty))
            .map(|(v, _)| *v)
            .collect();
        for v in releases {
            self.insts.push(Inst::Release { src: v });
        }
    }

    fn lookup_extern(&self, surface: &str) -> Option<&'static BuiltinExtern> {
        BUILTIN_EXTERNS.iter().find(|e| e.surface == surface)
    }

    fn ensure_extern(&mut self, ext: &'static BuiltinExtern) {
        if !self.seen_externs.insert(ext.surface) {
            return;
        }
        self.functions.push(Function {
            name: ext.surface.to_string(),
            linkage_name: ext.linkage.to_string(),
            params: ext
                .params
                .iter()
                .map(|(n, t)| ((*n).to_string(), t.clone()))
                .collect(),
            return_type: ext.return_type.clone(),
            blocks: Vec::new(),
        });
    }

    /// Register a runtime entry-point extern whose name *is* its
    /// linkage symbol (`coddl_runtime_init`, `coddl_runtime_shutdown`).
    /// Used by `main`'s init/shutdown wrapping; the synthetic extern
    /// participates in the same `seen_externs` deduplication as the
    /// builtin-mapped externs.
    fn ensure_runtime_extern(&mut self, linkage: &'static str) {
        if !self.seen_externs.insert(linkage) {
            return;
        }
        self.functions.push(Function {
            name: linkage.to_string(),
            linkage_name: linkage.to_string(),
            params: Vec::new(),
            return_type: ProcType::Integer,
            blocks: Vec::new(),
        });
    }

    // ── Walks ────────────────────────────────────────────────────────

    fn lower_root(&mut self, root: &Root) -> Module {
        for item in root.items() {
            match item {
                Item::ProgramDecl(p) => self.lower_program_decl(&p),
                Item::DatabaseBinding(_) => {
                    // The binding is a parse-time label today; runtime
                    // wiring lands with Phase 21's SQLite materialization.
                }
                Item::PublicRelvarDecl(_)
                | Item::PrivateRelvarDecl(_)
                | Item::BaseRelvarDecl(_)
                | Item::VirtualRelvarDecl(_) => {
                    // Relvar declarations don't lower yet — they
                    // describe storage shape that the typechecker
                    // collects into the relvar table (Phase 15).
                    // Storage init lands in Phase 21.
                }
                Item::OperDecl(o) => {
                    let func = self.lower_oper_decl(&o);
                    self.functions.push(func);
                }
            }
        }
        Module {
            program_name: std::mem::take(&mut self.program_name),
            functions: std::mem::take(&mut self.functions),
            headings: std::mem::take(&mut self.headings),
        }
    }

    fn lower_program_decl(&mut self, decl: &ProgramDecl) {
        if let Some(name_tok) = decl.name() {
            self.program_name = name_tok.text().to_string();
        }
    }

    fn lower_oper_decl(&mut self, decl: &OperDecl) -> Function {
        self.reset_function_state();

        let name = decl
            .name()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        // Defined functions: surface name is the linkage name for now.
        // Adding name mangling — for overloading or module-scoped
        // symbols — slots in here once it arrives.
        let linkage_name = name.clone();

        let mut params: Vec<(String, ProcType)> = Vec::new();
        if let Some(heading) = decl.heading() {
            for param in heading.params() {
                let pname = param
                    .name()
                    .map(|t| t.text().to_string())
                    .unwrap_or_default();
                let pty = param
                    .type_ref()
                    .and_then(|tr| tr.name())
                    .map(|t| proc_type_from_builtin_name(t.text()))
                    .unwrap_or(ProcType::Unit);
                params.push((pname, pty));
            }
        }

        let is_main = name == "main";

        // Resolve the declared return type. Default = Unit. Main is
        // treated as Unit at the IR level (the backends special-case
        // `ret i32 0`); the typechecker rejects a declared non-Unit
        // return on `main` with T0011, so this assignment is safe.
        let declared_return = decl
            .return_type()
            .and_then(|tr| tr.name())
            .map(|t| proc_type_from_builtin_name(t.text()))
            .unwrap_or(ProcType::Unit);

        let block_id = self.fresh_block();

        // The compiled program's startup must call the runtime before
        // touching any other extern (ARCHITECTURE.md §6). Today the
        // stubs are no-ops, but wiring it now means future runtime
        // work — DB connection pool, prepared-statement cache,
        // arena setup — slots in without a codegen change.
        if is_main {
            self.ensure_runtime_extern("coddl_runtime_init");
            let dst = self.fresh_value();
            self.record_type(dst, ProcType::Integer);
            self.insts.push(Inst::Call {
                dst: Some(dst),
                callee: "coddl_runtime_init".to_string(),
                args: Vec::new(),
                return_type: ProcType::Integer,
            });
        }

        let body_value = decl.body().map(|body| self.lower_block(&body));

        // Release every heap-typed function-scope local before either
        // the runtime-shutdown call (main) or the terminator (others).
        // Phase 19 doesn't yet return heap values from functions, so
        // we can release everything in the function scope here.
        self.release_top_scope_heap_locals();

        if is_main {
            self.ensure_runtime_extern("coddl_runtime_shutdown");
            let dst = self.fresh_value();
            self.record_type(dst, ProcType::Integer);
            self.insts.push(Inst::Call {
                dst: Some(dst),
                callee: "coddl_runtime_shutdown".to_string(),
                args: Vec::new(),
                return_type: ProcType::Integer,
            });
        }

        // Non-main opers with a non-Unit declared return carry their
        // body's tail-expression value out via `Return(Some(v))`.
        // Main + Unit-returning opers use `Return(None)`; the backend
        // special-cases main as `ret i32 0`.
        let terminator = if !is_main && !matches!(declared_return, ProcType::Unit) {
            match body_value {
                Some(v) => Terminator::Return(Some(v)),
                None => Terminator::Return(None),
            }
        } else {
            Terminator::Return(None)
        };

        let block = BasicBlock {
            id: block_id,
            insts: std::mem::take(&mut self.insts),
            terminator,
        };

        Function {
            name,
            linkage_name,
            params,
            return_type: declared_return,
            blocks: vec![block],
        }
    }

    /// Lower a block. Returns the block's value — the tail
    /// expression's `ValueId` if there is one, otherwise a fresh
    /// placeholder representing Unit (never consumed downstream).
    fn lower_block(&mut self, block: &Block) -> ValueId {
        for stmt in block.statements() {
            match stmt {
                Stmt::Let(l) => self.lower_let_stmt(&l),
                Stmt::ExprStmt(e) => self.lower_expr_stmt(&e),
            }
        }
        match block.tail_expr() {
            Some(expr) => self.lower_expr(&expr),
            None => {
                let v = self.fresh_value();
                self.record_type(v, ProcType::Unit);
                v
            }
        }
    }

    fn lower_let_stmt(&mut self, stmt: &LetStmt) {
        // RHS expression always lowers first; the binding name then
        // adopts its `ValueId`. Missing name (parser recovery) is
        // dropped silently — the diagnostic-free invariant means
        // we'd never reach lowering with one.
        let value_expr = match stmt.value() {
            Some(v) => v,
            None => return,
        };
        // If the RHS is a NameRef to an existing heap-typed binding,
        // the new let creates a second owner of the same value —
        // emit a retain so the refcount reflects both bindings. Pure
        // `RelationLit` RHS produces a fresh allocation already at
        // rc=1, so no retain is needed for that path.
        let rhs_is_existing_name = matches!(value_expr, Expr::NameRef(_));
        let id = self.lower_expr(&value_expr);
        let ty = self.value_type(id);
        if rhs_is_existing_name && Self::is_heap_managed(&ty) {
            self.insts.push(Inst::Retain { src: id });
        }
        if let Some(name_tok) = stmt.name() {
            self.bind_local(name_tok.text().to_string(), id, ty);
        }
    }

    fn lower_expr_stmt(&mut self, stmt: &ExprStmt) {
        if let Some(expr) = stmt.expr() {
            let _ = self.lower_expr(&expr);
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> ValueId {
        match expr {
            Expr::Literal(lit) => self.lower_literal(lit),
            Expr::Call(call) => self.lower_call(call),
            Expr::Transaction(t) => self.lower_transaction_expr(t),
            Expr::TupleLit(t) => self.lower_tuple_lit(t),
            Expr::RelationLit(r) => self.lower_relation_lit(r),
            Expr::FieldAccess(f) => self.lower_field_access(f),
            Expr::BoolLit(b) => self.lower_bool_lit(b),
            Expr::Binary(b) => self.lower_binary_expr(b),
            Expr::Unary(u) => self.lower_unary_expr(u),
            Expr::NameRef(n) => self.lower_name_ref(n),
        }
    }

    /// Lower a unary prefix expression. Phase 21 handles `Extract`:
    /// emit `Inst::Extract` with the source's heading id. If the
    /// source isn't bound to any local (i.e., it's a temporary —
    /// e.g., a freshly-allocated `R where p`), emit `Inst::Release`
    /// after extract so the heap payload is freed.
    fn lower_unary_expr(&mut self, ue: &UnaryExpr) -> ValueId {
        let op = ue.op_kind().expect("typechecked unary expr has an op");
        match op {
            UnaryOp::Extract => {
                let operand_expr = ue
                    .operand()
                    .expect("typechecked extract has an operand");
                let src = self.lower_expr(&operand_expr);
                let heading_id = match self.value_type(src) {
                    ProcType::Relation(id) => id,
                    other => unreachable!(
                        "extract on non-relation `{other}` survived typecheck"
                    ),
                };
                let heading = self.headings[heading_id.0 as usize].clone();
                let dst = self.fresh_value();
                self.record_type(dst, ProcType::Tuple(heading));
                self.insts.push(Inst::Extract {
                    dst,
                    src,
                    heading_id,
                });
                // If the source ValueId isn't owned by any local, it's
                // a temporary — release its heap payload now that
                // extract has copied its content into scalar fields.
                // Bound sources (e.g., `extract r` where `r` is a let
                // binding) stay live; releasing here would
                // double-free at the next use.
                let is_owned = self
                    .locals
                    .iter()
                    .any(|layer| layer.values().any(|(vid, _)| *vid == src));
                if !is_owned {
                    self.insts.push(Inst::Release { src });
                }
                dst
            }
        }
    }

    /// Resolve a `NameRef`. The active `locals` scope is consulted
    /// first. When inside a `where` predicate
    /// (`outer_locals_for_capture` is `Some`), a miss in the active
    /// scope checks the saved outer scope; a hit there is a capture,
    /// which Phase 20 deferred (T0022). Names that resolve nowhere
    /// fall through to a Unit placeholder ValueId — diagnostic-free
    /// input doesn't reach this branch in practice.
    fn lower_name_ref(&mut self, n: &NameRef) -> ValueId {
        if let Some(name_tok) = n.ident() {
            let name = name_tok.text();
            if let Some((v, _ty)) = self.lookup_local(name) {
                return v;
            }
            if let Some(outer) = &self.outer_locals_for_capture {
                let captured = outer.iter().rev().any(|l| l.contains_key(name));
                if captured {
                    self.diagnostics.push(Diagnostic::error(
                        self.node_span(n.syntax()),
                        "T0022",
                        format!(
                            "identifier `{name}` is captured from an outer scope; \
                             `where`-predicate captures are not yet supported"
                        ),
                    ));
                }
            }
        }
        let v = self.fresh_value();
        self.record_type(v, ProcType::Unit);
        v
    }

    fn lower_bool_lit(&mut self, b: &BoolLit) -> ValueId {
        let value = b.value().unwrap_or(false);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Boolean);
        self.insts.push(Inst::Const {
            dst,
            value: Const::Boolean(value),
            ty: ProcType::Boolean,
        });
        dst
    }

    /// Lower a binary infix expression. Dispatches on the parsed
    /// op kind. `Where` is the relational case (synthesizes a
    /// predicate helper Function); everything else is a scalar
    /// `Inst::ScalarOp`.
    fn lower_binary_expr(&mut self, bin: &BinaryExpr) -> ValueId {
        let op = bin.op_kind().expect("typechecked binary expr has an op");
        if matches!(op, BinaryOp::Where) {
            return self.lower_where_expr(bin);
        }
        let scalar_op = match op {
            BinaryOp::Eq => ScalarOp::Eq,
            BinaryOp::NotEq => ScalarOp::NotEq,
            BinaryOp::Lt => ScalarOp::Lt,
            BinaryOp::Gt => ScalarOp::Gt,
            BinaryOp::LtEq => ScalarOp::LtEq,
            BinaryOp::GtEq => ScalarOp::GtEq,
            BinaryOp::And => ScalarOp::And,
            BinaryOp::Or => ScalarOp::Or,
            BinaryOp::Where => unreachable!("handled above"),
        };
        let lhs = bin
            .lhs()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.fresh_value());
        let rhs = bin
            .rhs()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.fresh_value());
        let operand_type = self.value_type(lhs);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Boolean);
        self.insts.push(Inst::ScalarOp {
            dst,
            op: scalar_op,
            operand_type,
            lhs,
            rhs,
        });
        dst
    }

    /// Lower `R where pred`: synthesize a helper function
    /// `__coddl_where_<n>` that takes a record pointer and returns
    /// Boolean, populate its body by lowering the predicate against
    /// a scope whose only entries are the heading's attributes
    /// (pre-loaded via `Inst::AttrLoad`), then emit `Inst::Where` in
    /// the enclosing function.
    fn lower_where_expr(&mut self, bin: &BinaryExpr) -> ValueId {
        // 1. Lower the relation operand in the enclosing function's
        //    scope.
        let src = bin
            .lhs()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked where has a relation lhs");
        let heading_id = match self.value_type(src) {
            ProcType::Relation(id) => id,
            other => unreachable!("where on non-relation `{other}` survived typecheck"),
        };
        let heading = self.headings[heading_id.0 as usize].clone();
        let layout = crate::layout::record_layout(&heading);

        // 2. Mint a fresh predicate function name.
        let pred_name = format!("__coddl_where_{}", self.next_where);
        self.next_where += 1;

        // 3. Snapshot the enclosing function's per-function state,
        //    install a fresh state for the predicate, and stash the
        //    outer locals on `outer_locals_for_capture` so the
        //    predicate's NameRef walk can detect captures.
        let saved_next_value = std::mem::replace(&mut self.next_value, 0);
        let saved_next_block = std::mem::replace(&mut self.next_block, 0);
        let saved_insts = std::mem::take(&mut self.insts);
        let saved_locals = std::mem::replace(&mut self.locals, vec![HashMap::new()]);
        let saved_value_types = std::mem::take(&mut self.value_types);
        self.outer_locals_for_capture = Some(saved_locals.clone());

        // 4. Build the predicate body. The function has a single
        //    parameter `record_ptr: Pointer`. Pre-emit `AttrLoad` for
        //    each heading attribute at function entry; bind each in
        //    the predicate scope under its source-level name.
        let block_id = self.fresh_block();
        let record_ptr = self.fresh_value();
        self.record_type(record_ptr, ProcType::Pointer);
        for attr in &layout.attrs {
            let attr_type = proc_type_from_kind(attr.kind);
            let dst = self.fresh_value();
            self.record_type(dst, attr_type.clone());
            self.insts.push(Inst::AttrLoad {
                dst,
                src: record_ptr,
                offset: attr.offset,
                attr_type: attr_type.clone(),
            });
            self.bind_local(attr.name.clone(), dst, attr_type);
        }

        // 5. Lower the predicate body.
        let pred_value = bin
            .rhs()
            .map(|e| self.lower_expr(&e))
            .expect("typechecked where has a predicate rhs");

        // 6. Close the predicate function.
        let block = BasicBlock {
            id: block_id,
            insts: std::mem::take(&mut self.insts),
            terminator: Terminator::Return(Some(pred_value)),
        };
        self.functions.push(Function {
            name: pred_name.clone(),
            linkage_name: pred_name.clone(),
            params: vec![("record_ptr".to_string(), ProcType::Pointer)],
            return_type: ProcType::Boolean,
            blocks: vec![block],
        });

        // 7. Restore the enclosing function's state.
        self.next_value = saved_next_value;
        self.next_block = saved_next_block;
        self.insts = saved_insts;
        self.locals = saved_locals;
        self.value_types = saved_value_types;
        self.outer_locals_for_capture = None;

        // 8. Emit Inst::Where in the enclosing function.
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        self.insts.push(Inst::Where {
            dst,
            src,
            predicate_linkage: pred_name,
            heading_id,
        });
        dst
    }

    /// Lower a `{a: e1, b: e2, …}` tuple literal. Each field's
    /// expression lowers in source order; the resulting
    /// `(name, ValueId)` pairs are reordered to canonical (name-sorted)
    /// heading order in the emitted `Inst::TupleLit`. The heading
    /// itself is built from the per-field static types — which the
    /// typechecker already enforces match the surface declaration.
    fn lower_tuple_lit(&mut self, tup: &TupleLit) -> ValueId {
        let mut field_pairs: Vec<(String, ValueId, ProcType)> = Vec::new();
        for field in tup.fields() {
            let name_tok = match field.name() {
                Some(t) => t,
                None => continue,
            };
            let value_expr = match field.value() {
                Some(v) => v,
                None => continue,
            };
            let id = self.lower_expr(&value_expr);
            let ty = self.value_type(id);
            field_pairs.push((name_tok.text().to_string(), id, ty));
        }
        // Canonical order — `Heading::new` will sort the type-level
        // pairs identically; emitting the SSA fields in the same order
        // means backends can iterate the heading and the fields in
        // lockstep without re-sorting.
        field_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let heading = Heading::new(
            field_pairs
                .iter()
                .map(|(n, _, ty)| (n.clone(), type_from_proc(ty)))
                .collect(),
        );
        let fields: Vec<(String, ValueId)> = field_pairs
            .into_iter()
            .map(|(n, v, _)| (n, v))
            .collect();
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Tuple(heading.clone()));
        self.insts.push(Inst::TupleLit {
            dst,
            fields,
            heading,
        });
        dst
    }

    /// Lower `<expr>.<field>`. The base's `ProcType` must be a
    /// `Tuple(H)` after typecheck; the field's `ProcType` is derived
    /// from `H`'s entry for the named attribute via `proc_type_from_type`.
    fn lower_field_access(&mut self, fa: &FieldAccess) -> ValueId {
        let base_expr = fa.base().expect("typechecked field-access has a base");
        let src = self.lower_expr(&base_expr);
        let src_ty = self.value_type(src);
        let heading = match &src_ty {
            ProcType::Tuple(h) => h.clone(),
            other => unreachable!("field access on non-tuple `{other}` survived typecheck"),
        };
        let field_name = fa
            .field()
            .expect("typechecked field-access has a field token")
            .text()
            .to_string();
        let field_type = heading
            .lookup(&field_name)
            .map(proc_type_from_type)
            .unwrap_or_else(|| {
                unreachable!("unknown field `{field_name}` survived typecheck")
            });
        let dst = self.fresh_value();
        self.record_type(dst, field_type.clone());
        self.insts.push(Inst::TupleField {
            dst,
            src,
            field_name,
            field_type,
        });
        dst
    }

    fn lower_transaction_expr(&mut self, txn: &TransactionExpr) -> ValueId {
        // The transaction wrapper is transparent for now — push a
        // scope, lower the body, pop. The body's value flows out.
        // When real transaction semantics arrive, this is where
        // synthetic begin/commit/rollback calls slot in.
        self.push_local_scope();
        let value = match txn.body() {
            Some(b) => self.lower_block(&b),
            None => self.fresh_value(),
        };
        // Release every heap-typed local in this transaction scope
        // before popping. The body's tail value (if heap-typed) is
        // not currently a use case Phase 19 exercises — relations
        // don't yet escape transactions as return values.
        self.release_top_scope_heap_locals();
        self.pop_local_scope();
        value
    }

    /// Lower a `Relation { <tuple-lit>, … }` literal. Each nested
    /// `TupleLit` lowers to its Phase-18 `Inst::TupleLit`; the
    /// resulting `ValueId`s become operands of an
    /// `Inst::RelationLit { dst, tuples, heading_id }`. The heading
    /// is the first tuple's; we intern it so backends emit at most
    /// one static descriptor per unique heading. Empty literals are
    /// kept out by the typechecker (T0018); reaching here with zero
    /// tuples is an internal bug.
    fn lower_relation_lit(&mut self, rel: &RelationLit) -> ValueId {
        let tuples: Vec<TupleLit> = rel.tuples().collect();
        assert!(
            !tuples.is_empty(),
            "empty relation literal survived typecheck (T0018)"
        );
        let mut tuple_values: Vec<ValueId> = Vec::with_capacity(tuples.len());
        let mut heading: Option<Heading> = None;
        for tup in &tuples {
            let v = self.lower_tuple_lit(tup);
            tuple_values.push(v);
            if heading.is_none() {
                if let ProcType::Tuple(h) = self.value_type(v) {
                    heading = Some(h);
                }
            }
        }
        let heading = heading.expect("typechecked tuple has a heading");
        let heading_id = self.intern_heading(&heading);
        let dst = self.fresh_value();
        self.record_type(dst, ProcType::Relation(heading_id));
        self.insts.push(Inst::RelationLit {
            dst,
            tuples: tuple_values,
            heading_id,
        });
        dst
    }

    fn lower_literal(&mut self, lit: &Literal) -> ValueId {
        let token = lit.token().expect("typechecked literal has a token");
        let (value, ty) = match token.kind() {
            SyntaxKind::STRING_LIT => (
                Const::Text(decode_string_literal(token.text())),
                ProcType::Text,
            ),
            SyntaxKind::INTEGER_LIT => {
                let n = parse_integer_literal(token.text());
                (Const::Integer(n), ProcType::Integer)
            }
            // CHAR_LIT, RATIONAL_LIT, APPROXIMATE_LIT land here as the
            // language exercises them. The typechecker already accepts
            // them; lowering catches up when the runtime grows to
            // consume them.
            other => unreachable!("literal kind {other:?} not yet lowered"),
        };
        let dst = self.fresh_value();
        self.record_type(dst, ty.clone());
        self.insts.push(Inst::Const { dst, value, ty });
        dst
    }

    fn lower_call(&mut self, call: &CallExpr) -> ValueId {
        let callee_name = match call.callee() {
            Some(Expr::NameRef(n)) => n.ident().map(|t| t.text().to_string()),
            _ => None,
        };
        let surface = callee_name.expect("typechecked call has a NameRef callee");

        // Polymorphic-Relation builtins are lowered to specialized
        // ProcIR ops carrying their argument's `HeadingId`. The
        // backends look the descriptor up in `Module::headings` to
        // emit the per-call-site descriptor pointer.
        if surface == "write_relation" {
            return self.lower_write_relation_call(call);
        }

        let ext = self
            .lookup_extern(&surface)
            .unwrap_or_else(|| unreachable!("unknown callee `{surface}` survived typecheck"));
        let linkage = ext.linkage.to_string();
        let return_type = ext.return_type.clone();

        // Lower each argument in the order the operator declared its
        // parameters; the typechecker has guaranteed every declared
        // parameter is supplied exactly once.
        let arg_list = call.args().expect("typechecked call has an arg list");
        let supplied: Vec<NamedArg> = arg_list.args().collect();
        let mut arg_values: Vec<ValueId> = Vec::with_capacity(ext.params.len());
        for (pname, _) in ext.params {
            let arg = supplied
                .iter()
                .find(|a| a.name().map(|t| t.text().to_string()).as_deref() == Some(*pname))
                .unwrap_or_else(|| unreachable!("missing arg `{pname}` survived typecheck"));
            let value_expr = arg.value().expect("typechecked named arg has a value");
            arg_values.push(self.lower_expr(&value_expr));
        }

        self.ensure_extern(ext);

        let dst = if matches!(return_type, ProcType::Unit) {
            None
        } else {
            let v = self.fresh_value();
            self.record_type(v, return_type.clone());
            Some(v)
        };
        self.insts.push(Inst::Call {
            dst,
            callee: linkage,
            args: arg_values,
            return_type,
        });
        // For Unit-returning calls there is no real SSA value; return a
        // fresh id so the surrounding expression machinery has a place
        // to plug in once it grows real consumers. Today nothing reads
        // it.
        dst.unwrap_or_else(|| {
            let v = self.fresh_value();
            self.record_type(v, ProcType::Unit);
            v
        })
    }

    /// Lower `write_relation { rel: <expr> }` to `Inst::WriteRelation`.
    /// The `rel` argument's static type is `ProcType::Relation(id)`;
    /// we pull the id off via `value_type` and embed it in the
    /// instruction so the backend doesn't need value-type tracking.
    /// `write_relation` returns Unit; the surrounding expression
    /// machinery gets a placeholder ValueId.
    fn lower_write_relation_call(&mut self, call: &CallExpr) -> ValueId {
        let arg_list = call.args().expect("typechecked call has an arg list");
        let rel_arg = arg_list
            .args()
            .find(|a| a.name().map(|t| t.text().to_string()).as_deref() == Some("rel"))
            .expect("typechecked write_relation has a `rel` arg");
        let rel_expr = rel_arg.value().expect("rel arg has a value expression");
        let rel = self.lower_expr(&rel_expr);
        let heading_id = match self.value_type(rel) {
            ProcType::Relation(id) => id,
            other => unreachable!(
                "write_relation got non-relation arg type `{other}` past typecheck"
            ),
        };
        self.insts.push(Inst::WriteRelation { rel, heading_id });
        let v = self.fresh_value();
        self.record_type(v, ProcType::Unit);
        v
    }
}

/// Convert a `record_layout` attribute kind tag to its machine-level
/// `ProcType`. Mirrors the runtime's `CoddlAttrKind`. Used by the
/// predicate-function synthesis to type the per-attribute `AttrLoad`
/// SSA values.
fn proc_type_from_kind(kind: u32) -> ProcType {
    use crate::layout::kind_tag;
    match kind {
        k if k == kind_tag::INTEGER => ProcType::Integer,
        k if k == kind_tag::BOOLEAN => ProcType::Boolean,
        k if k == kind_tag::TEXT => ProcType::Text,
        other => unreachable!("unsupported attr kind {other} in predicate"),
    }
}

/// Convert a surface `Type` (as it appears in a `Heading`) to its
/// machine-level `ProcType`. The mapping is total over the types the
/// Convert a surface `Type` (as it appears in a `Heading`) to its
/// machine-level `ProcType`. Pure on scalar / tuple cases. The
/// `Relation` case is handled by `Lowerer::proc_type_from_type`
/// (which needs the heading interner); the free function below is
/// the simple total mapping for non-relation cells. Phase 19's tuple
/// cells don't yet carry relations, so this path is fine.
fn proc_type_from_type(ty: &Type) -> ProcType {
    match ty {
        Type::Integer => ProcType::Integer,
        Type::Rational => ProcType::Rational,
        Type::Approximate => ProcType::Approximate,
        Type::Text => ProcType::Text,
        Type::Character => ProcType::Character,
        Type::Binary => ProcType::Binary,
        Type::Byte => ProcType::Byte,
        Type::Boolean => ProcType::Boolean,
        Type::Tuple(h) => ProcType::Tuple(h.clone()),
        Type::Relation(_) => {
            unreachable!(
                "Type::Relation inside a non-relation context — use Lowerer::proc_type_from_type"
            )
        }
        Type::Unknown => unreachable!("Type::Unknown survived typecheck"),
    }
}

/// Recover a surface `Type` for a `ProcType` we previously derived
/// from one. Used by tuple-literal lowering to round-trip
/// per-field types through `Heading::new`. The mapping is exact for
/// scalar and tuple cells; relation cells need the lowerer's
/// heading table to recover the surface heading, so this free
/// function rejects them — Phase 19's tuple-literal walk doesn't
/// yet need to thread `ProcType::Relation` back through `Type`.
fn type_from_proc(pt: &ProcType) -> Type {
    match pt {
        ProcType::Integer => Type::Integer,
        ProcType::Rational => Type::Rational,
        ProcType::Approximate => Type::Approximate,
        ProcType::Text => Type::Text,
        ProcType::Character => Type::Character,
        ProcType::Binary => Type::Binary,
        ProcType::Byte => Type::Byte,
        ProcType::Boolean => Type::Boolean,
        ProcType::Unit => Type::unit(),
        ProcType::Tuple(h) => Type::Tuple(h.clone()),
        ProcType::Pointer => {
            unreachable!("Pointer ProcType in a tuple field — not reachable in Phase 19")
        }
        ProcType::Relation(_) => {
            unreachable!(
                "ProcType::Relation in a tuple cell — needs heading interner; not reachable in Phase 19"
            )
        }
    }
}

fn proc_type_from_builtin_name(name: &str) -> ProcType {
    match name {
        "Integer" => ProcType::Integer,
        "Rational" => ProcType::Rational,
        "Approximate" => ProcType::Approximate,
        "Text" => ProcType::Text,
        "Character" => ProcType::Character,
        "Binary" => ProcType::Binary,
        "Byte" => ProcType::Byte,
        "Boolean" => ProcType::Boolean,
        // Typechecker already rejected anything else via T0005; the
        // diagnostic-free invariant means we never get here.
        other => unreachable!("unknown type `{other}` survived typecheck"),
    }
}

/// Decode the body of a `STRING_LIT` token (with surrounding `"`s) to
/// raw UTF-8 bytes. Recognizes the escape set spelled out in
/// `docs/grammar.md`: `\n`, `\r`, `\t`, `\"`, `\\`, `\u{...}`.
fn decode_string_literal(text: &str) -> Vec<u8> {
    let inner = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    let mut out = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let Some(esc) = chars.next() else { break };
        match esc {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '"' => out.push(b'"'),
            '\\' => out.push(b'\\'),
            'u' => {
                // `\u{XXXX}` — the lexer already validated the form.
                if chars.next() != Some('{') {
                    break;
                }
                let mut hex = String::new();
                for h in chars.by_ref() {
                    if h == '}' {
                        break;
                    }
                    hex.push(h);
                }
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            _ => unreachable!("unknown escape `\\{esc}` survived lexing"),
        }
    }
    out
}

/// Parse an `INTEGER_LIT` lexeme into its `i64` value. Handles the
/// four bases the lexer recognizes (`0x`, `0b`, `0o`, `0d`) plus the
/// default decimal form. Underscores between digits are stripped.
fn parse_integer_literal(text: &str) -> i64 {
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    let (radix, digits) = if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        (16, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        (2, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        (8, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0d")
        .or_else(|| cleaned.strip_prefix("0D"))
    {
        (10, rest)
    } else {
        (10, cleaned.as_str())
    };
    i64::from_str_radix(digits, radix).expect("lexer validated the digits")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_WORLD: &str = "program hello_world;\n\
                               \n\
                               oper main {}\n\
                               [\n\
                                   write_line{message: \"Hello, world!\"};\n\
                               ];\n";

    fn lower_ok(src: &str) -> Module {
        let out = lower(src, FileId(0));
        assert!(
            out.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            out.diagnostics
        );
        out.module
            .expect("module should be produced on clean check")
    }

    #[test]
    fn hello_world_lowers_to_four_functions() {
        // `main` plus three runtime externs: write_line for the user
        // call, init + shutdown for the auto-wrapped startup
        // housekeeping ARCHITECTURE.md §6 requires.
        let m = lower_ok(HELLO_WORLD);
        let names: Vec<_> = m.functions.iter().map(|f| f.name.as_str()).collect();
        for needed in [
            "main",
            "write_line",
            "coddl_runtime_init",
            "coddl_runtime_shutdown",
        ] {
            assert!(names.contains(&needed), "expected {needed} in {names:?}");
        }
        assert_eq!(m.functions.len(), 4);
    }

    #[test]
    fn hello_world_main_body_is_init_const_call_shutdown() {
        let m = lower_ok(HELLO_WORLD);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(main.blocks.len(), 1);
        let block = &main.blocks[0];
        assert_eq!(block.insts.len(), 4);

        // 1. init wrapper call.
        match &block.insts[0] {
            Inst::Call {
                callee,
                args,
                return_type: ProcType::Integer,
                ..
            } => {
                assert_eq!(callee, "coddl_runtime_init");
                assert!(args.is_empty());
            }
            other => panic!("expected init Call, got {other:?}"),
        }

        // 2. string constant.
        match &block.insts[1] {
            Inst::Const {
                value: Const::Text(bytes),
                ty: ProcType::Text,
                ..
            } => assert_eq!(bytes, b"Hello, world!"),
            other => panic!("expected Const Text, got {other:?}"),
        }

        // 3. write_line call.
        match &block.insts[2] {
            Inst::Call {
                dst: None,
                callee,
                args,
                return_type: ProcType::Unit,
            } => {
                assert_eq!(callee, "coddl_write_line");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected write_line Call, got {other:?}"),
        }

        // 4. shutdown wrapper call.
        match &block.insts[3] {
            Inst::Call {
                callee,
                args,
                return_type: ProcType::Integer,
                ..
            } => {
                assert_eq!(callee, "coddl_runtime_shutdown");
                assert!(args.is_empty());
            }
            other => panic!("expected shutdown Call, got {other:?}"),
        }
        assert!(matches!(block.terminator, Terminator::Return(None)));
    }

    #[test]
    fn let_binding_threaded_into_write_line_call() {
        // The let-bound `msg` becomes the same ValueId the call site
        // uses for `message`. The const + call instructions are the
        // body's payload (sandwiched between init/shutdown).
        let src = "oper main {} [ let msg = \"hi\"; write_line{message: msg}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        // Find the Const Text and the call to coddl_write_line.
        let const_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::Const {
                    dst,
                    value: Const::Text(bytes),
                    ..
                } if bytes == b"hi" => Some(*dst),
                _ => None,
            })
            .expect("Const Text \"hi\" present");
        let call_arg = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if callee == "coddl_write_line" => {
                    Some(args.first().copied())
                }
                _ => None,
            })
            .expect("write_line call present")
            .expect("write_line call has an arg");
        assert_eq!(
            call_arg, const_dst,
            "let binding should thread its ValueId to the call site"
        );
    }

    #[test]
    fn transaction_tail_expression_value_flows_out() {
        // `transaction [ "ok" ]` as the RHS of a let: the let's bound
        // ValueId is the same one Const Text "ok" produces. The
        // following write_line call references it.
        let src = "oper main {} [ let ok = transaction [ \"ok\" ]; write_line{message: ok}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;

        let ok_const_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::Const {
                    dst,
                    value: Const::Text(bytes),
                    ..
                } if bytes == b"ok" => Some(*dst),
                _ => None,
            })
            .expect("Const Text \"ok\" present");
        let call_arg = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if callee == "coddl_write_line" => {
                    Some(args.first().copied())
                }
                _ => None,
            })
            .expect("write_line call present")
            .expect("write_line call has an arg");
        assert_eq!(call_arg, ok_const_dst);
    }

    #[test]
    fn hello_world_extern_has_no_blocks() {
        let m = lower_ok(HELLO_WORLD);
        let ext = m.functions.iter().find(|f| f.name == "write_line").unwrap();
        assert!(ext.is_extern());
        assert_eq!(ext.linkage_name, "coddl_write_line");
        assert_eq!(ext.params.len(), 1);
        assert_eq!(ext.params[0].0, "message");
        assert_eq!(ext.params[0].1, ProcType::Text);
        assert_eq!(ext.return_type, ProcType::Unit);
    }

    #[test]
    fn string_literal_decodes_escapes() {
        let src = "oper main {} [ write_line{message: \"a\\nb\"}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let block = &main.blocks[0];
        // `main`'s body is wrapped by init/shutdown; the user's
        // string constant lives between them.
        let text_const = block
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::Const {
                    value: Const::Text(bytes),
                    ..
                } => Some(bytes.as_slice()),
                _ => None,
            })
            .expect("expected a Const Text in main");
        assert_eq!(text_const, b"a\nb");
    }

    #[test]
    fn program_name_carried_through() {
        let src = "program greet; oper main {} [];";
        let m = lower_ok(src);
        assert_eq!(m.program_name, "greet");
    }

    #[test]
    fn multiple_opers_lower_independently() {
        let src = "oper foo {} []; oper bar {} [];";
        let m = lower_ok(src);
        let defined: Vec<&str> = m
            .functions
            .iter()
            .filter(|f| !f.is_extern())
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(defined, vec!["foo", "bar"]);
    }

    #[test]
    fn typecheck_error_returns_none_module() {
        let src = "oper main {} [ write_lne{message: \"x\"}; ];";
        let out = lower(src, FileId(0));
        assert!(out.module.is_none());
        assert!(out.diagnostics.iter().any(|d| d.code == "T0001"));
    }

    #[test]
    fn call_to_write_line_uses_coddl_prefix_in_linkage_name() {
        // Among `main`'s calls, exactly one routes a Text argument —
        // that's the write_line site. Its callee must be the linkage
        // name, not the surface name.
        let m = lower_ok(HELLO_WORLD);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let call = main
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if !args.is_empty() => Some(callee.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(call, "coddl_write_line");
    }

    #[test]
    fn non_main_oper_with_return_type_emits_typed_return() {
        // Integer chosen because both backends already handle scalar
        // returns. Text returns are blocked on the C ABI return-pair
        // codegen — a future phase.
        let src = "oper compute {} -> Integer [ 42 ]; oper main {} [];";
        let m = lower_ok(src);
        let compute = m.functions.iter().find(|f| f.name == "compute").unwrap();
        assert_eq!(compute.return_type, ProcType::Integer);
        assert_eq!(compute.blocks.len(), 1);
        match &compute.blocks[0].terminator {
            Terminator::Return(Some(_)) => {}
            other => panic!("expected Return(Some(_)), got {other:?}"),
        }
    }

    #[test]
    fn oper_without_explicit_return_stays_unit() {
        let src = "oper noop {} [];";
        let m = lower_ok(src);
        let noop = m.functions.iter().find(|f| f.name == "noop").unwrap();
        assert_eq!(noop.return_type, ProcType::Unit);
        assert!(matches!(
            noop.blocks[0].terminator,
            Terminator::Return(None)
        ));
    }

    #[test]
    fn integer_literal_decodes_decimal_and_hex() {
        assert_eq!(parse_integer_literal("42"), 42);
        assert_eq!(parse_integer_literal("0x2a"), 42);
        assert_eq!(parse_integer_literal("0b101010"), 42);
        assert_eq!(parse_integer_literal("0o52"), 42);
        assert_eq!(parse_integer_literal("1_000"), 1000);
    }

    // ── Tuple lit + field access (Phase 18) ──────────────────────────

    #[test]
    fn tuple_let_field_access_threaded_through_call() {
        // The tuple's `message` field becomes a TupleField project;
        // its value flows into write_line's `message` argument.
        let src = "oper main {} [ \
                   let t = {message: \"hi\"}; \
                   write_line{message: t.message}; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;

        // Find the TupleLit instruction.
        let tuple_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleLit { dst, fields, .. } => {
                    assert_eq!(fields.len(), 1);
                    assert_eq!(fields[0].0, "message");
                    Some(*dst)
                }
                _ => None,
            })
            .expect("TupleLit emitted");

        // Find the TupleField projecting `message` from the tuple.
        let field_dst = insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleField {
                    dst,
                    src,
                    field_name,
                    field_type,
                } if *src == tuple_dst && field_name == "message" => {
                    assert_eq!(*field_type, ProcType::Text);
                    Some(*dst)
                }
                _ => None,
            })
            .expect("TupleField emitted");

        // Find the write_line call and confirm it consumes the field's
        // ValueId as its argument.
        let arg = insts
            .iter()
            .find_map(|i| match i {
                Inst::Call { callee, args, .. } if callee == "coddl_write_line" => {
                    Some(args.first().copied())
                }
                _ => None,
            })
            .expect("write_line call present")
            .expect("write_line call has an arg");
        assert_eq!(arg, field_dst);
    }

    #[test]
    fn empty_tuple_lit_emits_inst_with_empty_heading() {
        // `{}` in expression position must lower to an Inst::TupleLit
        // with no fields and an empty heading.
        let src = "oper main {} [ let _u = {}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let inst = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleLit { fields, heading, .. } => Some((fields.clone(), heading.clone())),
                _ => None,
            })
            .expect("TupleLit emitted");
        assert!(inst.0.is_empty());
        assert!(inst.1.is_empty());
    }

    #[test]
    fn tuple_fields_emitted_in_canonical_order() {
        // Source order is reversed alphabetically; the emitted
        // Inst::TupleLit's fields list and heading must both be sorted.
        let src = "oper main {} [ let _t = {z: 1, a: 2}; ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let (fields, heading) = main.blocks[0]
            .insts
            .iter()
            .find_map(|i| match i {
                Inst::TupleLit { fields, heading, .. } => Some((fields.clone(), heading.clone())),
                _ => None,
            })
            .expect("TupleLit emitted");
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "z"]);
        let attr_names: Vec<&str> = heading.attrs().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(attr_names, vec!["a", "z"]);
    }

    // ── Where + predicate synthesis (Phase 20) ───────────────────────

    #[test]
    fn where_synthesizes_predicate_function_and_emits_inst_where() {
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let s = r where a = 2; \
                   ];";
        let m = lower_ok(src);
        // Predicate helper function exists.
        let pred = m
            .functions
            .iter()
            .find(|f| f.name.starts_with("__coddl_where_"))
            .expect("synthesized predicate function");
        assert_eq!(pred.params.len(), 1);
        assert_eq!(pred.return_type, ProcType::Boolean);
        // Predicate body contains an AttrLoad + ScalarOp.
        let pred_insts = &pred.blocks[0].insts;
        assert!(
            pred_insts.iter().any(|i| matches!(i, Inst::AttrLoad { .. })),
            "predicate should AttrLoad heading attrs"
        );
        assert!(
            pred_insts.iter().any(|i| matches!(i, Inst::ScalarOp { .. })),
            "predicate body should ScalarOp"
        );
        // Main contains an Inst::Where.
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let main_insts = &main.blocks[0].insts;
        assert!(
            main_insts.iter().any(|i| matches!(i, Inst::Where { .. })),
            "main should emit Inst::Where"
        );
    }

    #[test]
    fn capture_in_where_predicate_diagnoses_t0022() {
        // `n` is bound in the enclosing scope, not in the heading. The
        // lowerer must emit T0022 because Phase 20 deferred captures.
        let src = "oper main {} [ \
                   let n = 5; \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let s = r where a = n; \
                   ];";
        let out = lower(src, FileId(0));
        assert!(
            out.diagnostics.iter().any(|d| d.code == "T0022"),
            "expected T0022, got {:?}",
            out.diagnostics
        );
        assert!(out.module.is_none(), "module should be None on T0022");
    }

    // ── extract (Phase 21) ───────────────────────────────────────────

    #[test]
    fn extract_on_temporary_emits_inst_then_release() {
        // The `r where a = 2` is a fresh allocation — extract should
        // emit Inst::Extract followed by Inst::Release(src).
        let src = "oper main {} [ \
                   let r = Relation { {a: 1}, {a: 2} }; \
                   let t = extract (r where a = 2); \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        let extract_idx = insts
            .iter()
            .position(|i| matches!(i, Inst::Extract { .. }))
            .expect("Inst::Extract emitted");
        // The next instruction must be a Release of the extract's
        // source (the temporary from the `where`).
        let extract_src = match &insts[extract_idx] {
            Inst::Extract { src, .. } => *src,
            _ => unreachable!(),
        };
        let next = &insts[extract_idx + 1];
        match next {
            Inst::Release { src } => assert_eq!(*src, extract_src),
            other => panic!("expected Release after Extract, got {other:?}"),
        }
    }

    #[test]
    fn extract_on_let_bound_does_not_release_source() {
        // When the source is a let-bound name, the scope owns the
        // refcount — extract should NOT emit an immediate Release
        // (that would double-free at scope exit).
        let src = "oper main {} [ \
                   let r = Relation { {a: 1} }; \
                   let t = extract r; \
                   ];";
        let m = lower_ok(src);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let insts = &main.blocks[0].insts;
        let extract_idx = insts
            .iter()
            .position(|i| matches!(i, Inst::Extract { .. }))
            .unwrap();
        let extract_src = match &insts[extract_idx] {
            Inst::Extract { src, .. } => *src,
            _ => unreachable!(),
        };
        // No Release of `extract_src` should appear between the
        // Extract and the function epilogue's scope-exit release.
        // There IS exactly one Release of `r`'s ValueId — but it
        // sits at the function epilogue (after the second
        // RelationLit's let-stmt finishes), not immediately after
        // the Extract. Verify there's exactly one Release for the
        // source.
        let count = insts
            .iter()
            .filter(|i| matches!(i, Inst::Release { src } if *src == extract_src))
            .count();
        assert_eq!(count, 1, "let-bound source should see exactly one Release (at scope exit)");
    }
}

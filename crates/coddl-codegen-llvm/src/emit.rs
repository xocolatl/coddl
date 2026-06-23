//! ProcIR → LLVM IR text emission.
//!
//! Walks a `Module` and produces clang-compatible LLVM IR text. Opaque
//! pointers (`ptr`) throughout — works on LLVM 15+. No target triple
//! is written; `clang` picks the host triple. `Text` values are
//! decomposed at the C-call boundary into a `(ptr, i64)` pair.
//!
//! See `docs/codegen.md` for the spec.

use std::collections::HashMap;
use std::fmt::Write as _;

use coddl_procir::{
    record_layout, BasicBlock, Codegen, Const, Function, HeadingId, Inst, Module, ProcType,
    RecordLayout, ScalarOp, Terminator, Type, ValueId,
};

use crate::error::LlvmEmitError;

pub struct LlvmBackend;

impl Default for LlvmBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LlvmBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Codegen for LlvmBackend {
    type Output = String;
    type Error = LlvmEmitError;

    fn emit(&mut self, module: &Module) -> Result<String, LlvmEmitError> {
        let mut emitter = Emitter::default();
        emitter.emit_module(module)?;
        Ok(emitter.finish())
    }
}

/// Per-value representation kept during the walk. `Text` values are
/// two LLVM operands even though ProcIR sees them as one logical
/// value; `Tuple` values are a compile-time grouping over per-field
/// `ValueRepr`s, which flatten recursively at ABI boundaries.
#[derive(Debug, Clone)]
enum ValueRepr {
    Scalar {
        ty: String,
        op: String,
    },
    Text {
        ptr_op: String,
        len_op: String,
    },
    /// Heading-canonical, name-sorted (matches the ProcIR
    /// `Inst::TupleLit::heading`'s `attrs()` order).
    Tuple {
        fields: Vec<(String, ValueRepr)>,
    },
}

impl ValueRepr {
    /// Append the LLVM operand spelling(s) for this value to a list
    /// of call-site operands. Scalars contribute one entry; Text two
    /// (ptr, i64); Tuples flatten recursively in canonical order.
    fn push_call_operands(&self, out: &mut Vec<String>) {
        match self {
            ValueRepr::Scalar { ty, op } => out.push(format!("{ty} {op}")),
            ValueRepr::Text { ptr_op, len_op } => {
                out.push(format!("ptr {ptr_op}"));
                out.push(format!("i64 {len_op}"));
            }
            ValueRepr::Tuple { fields } => {
                for (_, f) in fields {
                    f.push_call_operands(out);
                }
            }
        }
    }
}

#[derive(Default)]
struct Emitter {
    /// Function definitions and extern declarations.
    body: String,
    /// File-scope global constants (string literals etc.), accumulated
    /// during function walks. Spliced into the final output between
    /// the extern declarations and the first defined function.
    globals: String,
    /// Per-function value map. Cleared at the start of each function.
    values: HashMap<ValueId, ValueRepr>,
    /// Counter for unique string-constant names (`@.str.0`, …).
    next_str: u32,
    /// Per-module record layout cache. Populated once in
    /// `emit_module` so `Inst::RelationLit` can look up the
    /// heading's layout by id without recomputing.
    heading_layouts: Vec<coddl_procir::RecordLayout>,
    /// Per-module public-relvar lookup: surface name → column count.
    /// The three new Inst arms (RelvarSlotInit / RelvarSlotRelease /
    /// RelvarRead) reference this to size the column-pointer / column-
    /// length arrays passed to `coddl_sqlite_relvar_init`.
    public_relvar_columns: HashMap<String, usize>,
    /// Symbol → payload byte length for every `private constant [N x i8]`
    /// we emit. `lower_relvar_slot_init` reads these to pass the
    /// right (ptr, len) pair to `coddl_resolve_op_field` and
    /// `coddl_sqlite_relvar_init`.
    byte_const_lens: HashMap<String, usize>,
    /// Per-module pushed-plan metadata: plan id → (param count, result
    /// heading id). `Inst::RegisterPlan` reads these to call
    /// `coddl_register_plan` with the right bind count and descriptor.
    plan_meta: HashMap<u32, (u32, u32)>,
}

impl Emitter {
    fn emit_module(&mut self, module: &Module) -> Result<(), LlvmEmitError> {
        writeln!(self.body, "; ModuleID = '{}'", module.program_name).unwrap();
        writeln!(self.body).unwrap();

        for func in module.functions.iter().filter(|f| f.is_extern()) {
            self.emit_extern(func)?;
        }
        // If the module touches any relation-shaped instruction we
        // need the runtime extern declarations even when no user
        // surface call references them directly. Declare them
        // unconditionally if the headings table is non-empty —
        // empty == no relations were ever interned, so no rc/seal/
        // write_relation symbol will be referenced.
        if !module.headings.is_empty() {
            self.emit_runtime_rc_externs();
        }
        // Scalar `Text` concatenation externs are needed whenever `||` appears,
        // independent of any relation machinery (a concat program may touch no
        // relations at all). Unused declares are harmless, so emit them
        // unconditionally.
        self.emit_scalar_text_externs();
        if !module.public_relvars.is_empty() {
            self.emit_runtime_relvar_externs();
        }
        if !module.plans.is_empty() {
            self.emit_runtime_plan_externs();
        }
        if module.functions.iter().any(Function::is_extern)
            || !module.headings.is_empty()
            || !module.public_relvars.is_empty()
        {
            writeln!(self.body).unwrap();
        }

        // Per-module heading descriptors. One block per unique
        // `Heading` in `Module::headings`. Each block has three
        // globals: per-attr name strings, the attribute array, and
        // the descriptor struct itself. Cache the layouts so
        // `Inst::RelationLit` can look them up by id later.
        self.heading_layouts = module
            .headings
            .iter()
            .map(|h| record_layout(h))
            .collect();
        for (i, heading) in module.headings.iter().enumerate() {
            self.emit_heading_descriptor(HeadingId(i as u32), heading)?;
        }
        if !module.headings.is_empty() {
            writeln!(self.body).unwrap();
        }

        // Per-public-relvar globals: the slot pointer + string
        // constants the runtime resolver needs (relvar name, env-var
        // name, default path, table name, per-column names + a column-
        // pointer / column-length array). All `Linkage::Local`-style
        // private constants; the slot starts as `null` and is written
        // by `coddl_sqlite_relvar_init` in `main`'s prologue.
        for relvar in &module.public_relvars {
            self.emit_public_relvar_globals(relvar, module)?;
            self.public_relvar_columns
                .insert(relvar.name.clone(), relvar.columns.len());
        }
        if !module.public_relvars.is_empty() {
            writeln!(self.body).unwrap();
        }

        // Per-plan globals (SQL text + db name) and the database-level
        // resolver strings used by `RegisterPlan` / `RegisterDatabase`.
        if !module.plans.is_empty() {
            self.emit_plan_globals(module);
        }

        let mut first_defined = true;
        for func in module.functions.iter().filter(|f| !f.is_extern()) {
            if !first_defined {
                writeln!(self.body).unwrap();
            }
            first_defined = false;
            self.emit_function(func)?;
        }

        Ok(())
    }

    /// Declare the scalar `Text` concatenation runtime symbols (`||` and the
    /// `Character`→`Text` normalization). These are needed independent of any
    /// relation machinery, so they are declared unconditionally. (`coddl_text_eq`
    /// stays with the relation externs since it predates this and is only used
    /// from contexts that already pull those in.)
    fn emit_scalar_text_externs(&mut self) {
        // Text concatenation `||`: (a_ptr, a_len, b_ptr, b_len) -> payload ptr
        // (length is `a_len + b_len`, recomputed at the call site).
        writeln!(
            self.body,
            "declare ptr @coddl_text_concat(ptr, i64, ptr, i64)"
        )
        .unwrap();
        // Character → Text: (codepoint) -> payload ptr; paired with
        // `coddl_utf8_len` for the length.
        writeln!(self.body, "declare ptr @coddl_char_to_text(i32)").unwrap();
        writeln!(self.body, "declare i64 @coddl_utf8_len(i32)").unwrap();
    }

    /// Declare the runtime symbols that `Inst::RelationLit`,
    /// `Inst::Retain`, `Inst::Release`, and `Inst::WriteRelation`
    /// call directly. The compiler injects these regardless of
    /// whether the user wrote `write_relation` — the relation
    /// machinery (alloc/seal) is always needed once a `RELATION_LIT`
    /// is present.
    fn emit_runtime_rc_externs(&mut self) {
        writeln!(
            self.body,
            "declare ptr @coddl_rc_alloc(i64, i32, i32, ptr)"
        )
        .unwrap();
        writeln!(self.body, "declare void @coddl_rc_retain(ptr)").unwrap();
        writeln!(self.body, "declare void @coddl_rc_release(ptr)").unwrap();
        writeln!(self.body, "declare void @coddl_relation_seal(ptr, ptr)").unwrap();
        writeln!(self.body, "declare void @coddl_write_relation(ptr, ptr)").unwrap();
        // Private-relvar in-memory slots: empty-init a slot, and store (move)
        // a relation into a slot.
        writeln!(
            self.body,
            "declare void @coddl_relvar_slot_init_empty(ptr, ptr)"
        )
        .unwrap();
        writeln!(self.body, "declare void @coddl_relvar_slot_store(ptr, ptr)").unwrap();
        // Phase 20 `where`: takes (src, desc, pred_fn) and returns
        // a fresh relation pointer (rc=1).
        writeln!(self.body, "declare ptr @coddl_relation_where(ptr, ptr, ptr)").unwrap();
        // `extend`: (src, src_desc, result_desc, helper_fn) -> rc=1 ptr.
        writeln!(
            self.body,
            "declare ptr @coddl_relation_extend(ptr, ptr, ptr, ptr)"
        )
        .unwrap();
        // `project`: takes (src, src_desc, result_desc) and returns a
        // fresh narrowed + sealed relation pointer (rc=1).
        writeln!(self.body, "declare ptr @coddl_relation_project(ptr, ptr, ptr)").unwrap();
        writeln!(self.body, "declare ptr @coddl_relation_restructure(ptr, ptr, ptr)").unwrap();
        // `join`: (lhs, lhs_desc, rhs, rhs_desc, result_desc) -> rc=1 ptr.
        writeln!(
            self.body,
            "declare ptr @coddl_relation_join(ptr, ptr, ptr, ptr, ptr)"
        )
        .unwrap();
        // `union`: (lhs, rhs, desc) -> rc=1 ptr. Identical headings ⇒ one desc.
        writeln!(self.body, "declare ptr @coddl_relation_union(ptr, ptr, ptr)").unwrap();
        // `minus`: (lhs, rhs, desc) -> rc=1 ptr. Identical headings ⇒ one desc.
        writeln!(self.body, "declare ptr @coddl_relation_minus(ptr, ptr, ptr)").unwrap();
        // `tclose`: (src, desc) -> rc=1 ptr. Result heading == operand heading
        // ⇒ one desc for both.
        writeln!(self.body, "declare ptr @coddl_relation_tclose(ptr, ptr)").unwrap();
        // `rename`: (src, src_desc, result_desc, perm, perm_count) -> rc=1 ptr.
        writeln!(
            self.body,
            "declare ptr @coddl_relation_rename(ptr, ptr, ptr, ptr, i64)"
        )
        .unwrap();
        // Text byte-equality: (a_ptr, a_len, b_ptr, b_len) -> i8 (1 = equal).
        writeln!(
            self.body,
            "declare i8 @coddl_text_eq(ptr, i64, ptr, i64)"
        )
        .unwrap();
        // Phase 21 `extract`: takes (src, desc) and returns a record
        // pointer (the relation's payload, which IS the first record
        // when length==1). Aborts on length != 1.
        writeln!(
            self.body,
            "declare ptr @coddl_extract_check_cardinality(ptr, ptr)"
        )
        .unwrap();
    }

    /// Declare the Phase 22 runtime externs the public-relvar
    /// machinery uses. Always emitted when the module has any public
    /// relvar; never otherwise. Transaction externs aren't included
    /// here — the lowerer registers `coddl_begin_tx` / `coddl_commit_tx`
    /// / `coddl_rollback_tx` through its Function table so they reach
    /// `emit_extern` like `coddl_runtime_init` does. Adding them here
    /// too would emit a conflicting `declare`.
    fn emit_runtime_relvar_externs(&mut self) {
        // (relvar_name, relvar_name_len, db_path, db_path_len,
        //  table, table_len, columns, column_lens, column_count,
        //  desc, slot) -> i32
        writeln!(
            self.body,
            "declare i32 @coddl_sqlite_relvar_init(ptr, i64, ptr, i64, ptr, i64, ptr, ptr, i32, ptr, ptr)"
        )
        .unwrap();
        writeln!(
            self.body,
            "declare ptr @coddl_resolve_op_field(ptr, i64, ptr, i64, ptr)"
        )
        .unwrap();
    }

    /// Declare the SQL-pushdown runtime externs: database/plan registration
    /// (program prologue), `coddl_query` (the lazy read force point), and
    /// `coddl_exec` (the DML force point). Emitted only when the module pushed
    /// at least one plan.
    fn emit_runtime_plan_externs(&mut self) {
        writeln!(
            self.body,
            "declare i32 @coddl_register_database(ptr, i64, ptr, i64)"
        )
        .unwrap();
        writeln!(
            self.body,
            "declare i32 @coddl_register_plan(i32, ptr, i64, ptr, i64, i32, ptr)"
        )
        .unwrap();
        writeln!(self.body, "declare ptr @coddl_query(i32, ptr, i64)").unwrap();
        writeln!(self.body, "declare i32 @coddl_exec(i32, ptr, i64)").unwrap();
        writeln!(self.body, "declare i32 @coddl_exec_insert(i32, ptr, ptr)").unwrap();
    }

    /// Emit the static byte constants the pushdown prologue references: the
    /// database name + its env-var key + baked default path (shared by every
    /// plan), and per-plan the SQL text and database name. Also records each
    /// plan's bind count + result heading id in `plan_meta`.
    fn emit_plan_globals(&mut self, module: &Module) {
        let db_name = module.db_name.as_deref().unwrap_or("");
        let default_path = module.db_path_default.as_deref().unwrap_or("");
        let env_name = format!("CODDL_{}_FILE", db_name.to_ascii_uppercase());
        self.emit_byte_constant("@.db.name", db_name.as_bytes());
        self.emit_byte_constant("@.db.env_name", env_name.as_bytes());
        self.emit_byte_constant("@.db.default_path", default_path.as_bytes());
        for p in &module.plans {
            self.emit_byte_constant(&format!("@.plan.{}.sql", p.plan_id), p.sql.as_bytes());
            self.emit_byte_constant(&format!("@.plan.{}.db_name", p.plan_id), p.db_name.as_bytes());
            self.plan_meta
                .insert(p.plan_id, (p.param_count, p.result_heading_id.0));
        }
    }

    /// Emit the per-relvar slot + companion string constants the
    /// materializer needs. Layout choices follow the runtime's C ABI:
    /// each column name is its own private constant; the column-pointer
    /// and column-length arrays index those.
    fn emit_public_relvar_globals(
        &mut self,
        relvar: &coddl_procir::PublicRelvarBinding,
        module: &Module,
    ) -> Result<(), LlvmEmitError> {
        let name = &relvar.name;
        let db_name = module.db_name.as_deref().unwrap_or("");
        let default_path = module.db_path_default.as_deref().unwrap_or("");
        let env_name = format!("CODDL_{}_FILE", db_name.to_ascii_uppercase());

        // Slot global — initialized to null; written by the runtime
        // at startup. The lowerer emits `RelvarRead` against this
        // global.
        writeln!(
            self.globals,
            "@{name}_slot = private unnamed_addr global ptr null",
        )
        .unwrap();

        // String constants for relvar name, env name, default path,
        // table name. UTF-8 byte arrays — runtime takes (ptr, len)
        // and never relies on null termination.
        self.emit_byte_constant(&format!("@{name}.relvar_name"), name.as_bytes());
        self.emit_byte_constant(&format!("@{name}.env_name"), env_name.as_bytes());
        self.emit_byte_constant(&format!("@{name}.default_path"), default_path.as_bytes());
        self.emit_byte_constant(&format!("@{name}.table_name"), relvar.table_name.as_bytes());
        for (i, (_, col)) in relvar.columns.iter().enumerate() {
            self.emit_byte_constant(&format!("@{name}.col{i}.name"), col.as_bytes());
        }
        // Pointer array (one ptr per column) and length array (one
        // i64 per column).
        write!(
            self.globals,
            "@{name}.col_ptrs = private unnamed_addr constant [{} x ptr] [",
            relvar.columns.len()
        )
        .unwrap();
        for (i, _) in relvar.columns.iter().enumerate() {
            if i > 0 {
                self.globals.push_str(", ");
            }
            write!(self.globals, "ptr @{name}.col{i}.name").unwrap();
        }
        writeln!(self.globals, "]").unwrap();
        write!(
            self.globals,
            "@{name}.col_lens = private unnamed_addr constant [{} x i64] [",
            relvar.columns.len()
        )
        .unwrap();
        for (i, (_, col)) in relvar.columns.iter().enumerate() {
            if i > 0 {
                self.globals.push_str(", ");
            }
            write!(self.globals, "i64 {}", col.as_bytes().len()).unwrap();
        }
        writeln!(self.globals, "]").unwrap();
        Ok(())
    }

    /// Emit the globals that describe one heading, keyed by `HeadingId`:
    /// `@.attrname.<id>.<i>`, `@.attrs.<id>`, `@.heading.<id>`. Instructions
    /// reference `@.heading.<id>`. Recurses into nested-tuple sub-layouts.
    fn emit_heading_descriptor(
        &mut self,
        id: HeadingId,
        heading: &coddl_procir::Heading,
    ) -> Result<(), LlvmEmitError> {
        let layout = record_layout(heading);
        self.emit_layout_descriptor(&id.0.to_string(), &layout);
        Ok(())
    }

    /// Emit the globals describing one record layout under symbol `base`
    /// (`@.attrname.<base>.<i>`, `@.attrs.<base>`, `@.heading.<base>`), recursing
    /// into a `Tuple` attr's sub-layout under `<base>.<i>`. Layout matches
    /// `coddl_runtime::{CoddlHeadingDesc, CoddlAttrDesc}`; the attr struct's
    /// trailing `ptr` is `sub` — the nested descriptor for a Tuple cell, else null.
    fn emit_layout_descriptor(&mut self, base: &str, layout: &RecordLayout) {
        // Nested sub-descriptors first (the parent attrs array references them).
        for (i, attr) in layout.attrs.iter().enumerate() {
            if let Some(sub) = &attr.sub {
                self.emit_layout_descriptor(&format!("{base}.{i}"), sub);
            }
        }
        // Per-attribute name strings.
        for (i, attr) in layout.attrs.iter().enumerate() {
            let name_bytes = attr.name.as_bytes();
            writeln!(
                self.globals,
                "@.attrname.{}.{} = private unnamed_addr constant [{} x i8] c\"{}\"",
                base,
                i,
                name_bytes.len(),
                escape_ir_bytes(name_bytes),
            )
            .unwrap();
        }
        // Attribute array. Each element matches `CoddlAttrDesc`:
        // { ptr name, i32 name_len, i32 kind, i32 offset, ptr sub }. Natural
        // padding on the host puts `sub` at offset 24 (64-bit); LLVM matches.
        write!(
            self.globals,
            "@.attrs.{} = private unnamed_addr constant [{} x {{ ptr, i32, i32, i32, ptr }}] [",
            base,
            layout.attrs.len()
        )
        .unwrap();
        for (i, attr) in layout.attrs.iter().enumerate() {
            if i > 0 {
                self.globals.push_str(", ");
            }
            let name_len = attr.name.as_bytes().len();
            // `sub` points at the nested descriptor for a Tuple cell, else null.
            let sub = if attr.sub.is_some() {
                format!("ptr @.heading.{base}.{i}")
            } else {
                "ptr null".to_string()
            };
            write!(
                self.globals,
                "{{ ptr, i32, i32, i32, ptr }} {{ ptr @.attrname.{}.{}, i32 {}, i32 {}, i32 {}, {} }}",
                base, i, name_len, attr.kind, attr.offset, sub,
            )
            .unwrap();
        }
        writeln!(self.globals, "]").unwrap();
        // The descriptor struct. Matches `CoddlHeadingDesc`:
        // { i32 attr_count, i32 record_size, ptr attrs }.
        writeln!(
            self.globals,
            "@.heading.{} = private unnamed_addr constant {{ i32, i32, ptr }} {{ i32 {}, i32 {}, ptr @.attrs.{} }}",
            base,
            layout.attrs.len(),
            layout.record_size,
            base,
        )
        .unwrap();
    }

    fn finish(self) -> String {
        // Splice globals between the extern declarations and the
        // first defined function. The marker is the first `define`
        // line; if the module has none, append at end.
        if self.globals.is_empty() {
            return self.body;
        }
        match self.body.find("define ") {
            Some(idx) => {
                let mut out = String::with_capacity(self.body.len() + self.globals.len() + 1);
                out.push_str(&self.body[..idx]);
                out.push_str(&self.globals);
                out.push('\n');
                out.push_str(&self.body[idx..]);
                out
            }
            None => {
                let mut out = self.body;
                out.push_str(&self.globals);
                out
            }
        }
    }

    fn emit_extern(&mut self, func: &Function) -> Result<(), LlvmEmitError> {
        let mut params: Vec<String> = Vec::new();
        for (_, pty) in &func.params {
            push_param_types(&mut params, pty);
        }
        writeln!(
            self.body,
            "declare {ret} @{linkage}({args})",
            ret = llvm_return_type(&func.return_type),
            linkage = func.linkage_name,
            args = params.join(", "),
        )
        .unwrap();
        Ok(())
    }

    fn emit_function(&mut self, func: &Function) -> Result<(), LlvmEmitError> {
        // C convention says `int main(void)`. ProcIR may declare
        // `Unit` but the linker expects `i32`. Special-case the name
        // `main` regardless of its surface return.
        let is_main = func.name == "main";
        let ret_ty = if is_main {
            "i32".to_string()
        } else {
            llvm_return_type(&func.return_type)
        };

        let mut params: Vec<String> = Vec::new();
        for (pname, pty) in &func.params {
            push_param_decl(&mut params, pname, pty);
        }
        self.values.clear();

        // Seed `self.values` with the function's parameters. The
        // lowerer's convention is that the first N fresh ValueIds in
        // each function map 1:1 to the function's params (in the
        // declared order). Phase 20's predicate helpers exercise this
        // — record_ptr is param 0, allocated as ValueId(0) in
        // `lower_where_expr`. The seeding matches the SSA names
        // `push_param_decl` writes into the signature.
        for (i, (pname, pty)) in func.params.iter().enumerate() {
            let vid = ValueId(i as u32);
            match pty {
                ProcType::Text | ProcType::Binary => {
                    self.values.insert(
                        vid,
                        ValueRepr::Text {
                            ptr_op: format!("%{pname}.ptr"),
                            len_op: format!("%{pname}.len"),
                        },
                    );
                }
                ProcType::Tuple(_) => {
                    return Err(LlvmEmitError::UnsupportedInst(
                        "Tuple-typed parameters not yet supported in defined functions".into(),
                    ));
                }
                other => {
                    let ty = llvm_value_type(other).to_string();
                    self.values.insert(
                        vid,
                        ValueRepr::Scalar {
                            ty,
                            op: format!("%{pname}"),
                        },
                    );
                }
            }
        }

        writeln!(
            self.body,
            "define {ret_ty} @{linkage}({args}) {{",
            linkage = func.linkage_name,
            args = params.join(", "),
        )
        .unwrap();

        for block in &func.blocks {
            self.emit_block(block, is_main, &func.return_type)?;
        }

        writeln!(self.body, "}}").unwrap();
        Ok(())
    }

    fn emit_block(
        &mut self,
        block: &BasicBlock,
        is_main: bool,
        return_type: &ProcType,
    ) -> Result<(), LlvmEmitError> {
        writeln!(self.body, "{}:", block.id).unwrap();
        for inst in &block.insts {
            self.emit_inst(inst)?;
        }
        self.emit_terminator(&block.terminator, is_main, return_type)?;
        Ok(())
    }

    fn emit_inst(&mut self, inst: &Inst) -> Result<(), LlvmEmitError> {
        match inst {
            Inst::Const {
                dst,
                value: Const::Text(bytes),
                ty: ProcType::Text,
            } => {
                self.lower_const_text(*dst, bytes);
                Ok(())
            }
            Inst::Const {
                dst,
                value: Const::Integer(n),
                ty: ProcType::Integer,
            } => {
                self.values.insert(
                    *dst,
                    ValueRepr::Scalar {
                        ty: "i64".to_string(),
                        op: format!("{n}"),
                    },
                );
                Ok(())
            }
            Inst::Const {
                dst,
                value: Const::Character(cp),
                ty: ProcType::Character,
            } => {
                // A Character is an inline `i32` codepoint.
                self.values.insert(
                    *dst,
                    ValueRepr::Scalar {
                        ty: "i32".to_string(),
                        op: format!("{cp}"),
                    },
                );
                Ok(())
            }
            Inst::Const {
                dst,
                value: Const::Boolean(b),
                ty: ProcType::Boolean,
            } => {
                // Boolean SSA is `i1` in LLVM. Widening to i8 at the
                // C-ABI boundary happens at return sites (predicate
                // functions).
                self.values.insert(
                    *dst,
                    ValueRepr::Scalar {
                        ty: "i1".to_string(),
                        op: (if *b { "1" } else { "0" }).to_string(),
                    },
                );
                Ok(())
            }
            Inst::Const { value, ty, .. } => Err(LlvmEmitError::UnsupportedInst(format!(
                "Const {value:?} of type {ty:?}"
            ))),
            Inst::Call {
                dst,
                callee,
                args,
                return_type,
            } => self.lower_call(*dst, callee, args, return_type),
            Inst::TupleLit { dst, fields, .. } => {
                // Pure compile-time grouping — no LLVM op emitted.
                let mut repr_fields: Vec<(String, ValueRepr)> =
                    Vec::with_capacity(fields.len());
                for (name, v) in fields {
                    let repr = self
                        .values
                        .get(v)
                        .ok_or_else(|| {
                            LlvmEmitError::UnsupportedInst(format!(
                                "undefined tuple field value {v:?}"
                            ))
                        })?
                        .clone();
                    repr_fields.push((name.clone(), repr));
                }
                self.values
                    .insert(*dst, ValueRepr::Tuple { fields: repr_fields });
                Ok(())
            }
            Inst::TupleField {
                dst,
                src,
                field_name,
                ..
            } => {
                // Project a single attribute out of the source tuple's
                // ValueRepr — also a pure compile-time operation.
                let src_repr = self.values.get(src).ok_or_else(|| {
                    LlvmEmitError::UnsupportedInst(format!("undefined tuple source {src:?}"))
                })?;
                let field_repr = match src_repr {
                    ValueRepr::Tuple { fields } => fields
                        .iter()
                        .find(|(n, _)| n == field_name)
                        .map(|(_, r)| r.clone())
                        .ok_or_else(|| {
                            LlvmEmitError::UnsupportedInst(format!(
                                "tuple {src:?} has no field `{field_name}`"
                            ))
                        })?,
                    other => {
                        return Err(LlvmEmitError::UnsupportedInst(format!(
                            "field access on non-tuple value: {other:?}"
                        )));
                    }
                };
                self.values.insert(*dst, field_repr);
                Ok(())
            }
            Inst::RelationLit {
                dst,
                tuples,
                heading_id,
            } => self.lower_relation_lit(*dst, tuples, *heading_id),
            Inst::Retain { src } => {
                let op = self.scalar_op(src)?;
                writeln!(self.body, "    call void @coddl_rc_retain(ptr {op})").unwrap();
                Ok(())
            }
            Inst::Release { src } => {
                let op = self.scalar_op(src)?;
                writeln!(self.body, "    call void @coddl_rc_release(ptr {op})").unwrap();
                Ok(())
            }
            Inst::WriteRelation { rel, heading_id } => {
                let op = self.scalar_op(rel)?;
                writeln!(
                    self.body,
                    "    call void @coddl_write_relation(ptr {op}, ptr @.heading.{})",
                    heading_id.0,
                )
                .unwrap();
                Ok(())
            }
            Inst::ScalarOp {
                dst,
                op,
                operand_type,
                lhs,
                rhs,
            } => self.lower_scalar_op(*dst, *op, operand_type, lhs, rhs),
            Inst::CharToText { dst, src } => self.lower_char_to_text(*dst, src),
            Inst::AttrLoad {
                dst,
                src,
                offset,
                attr_type,
            } => self.lower_attr_load(*dst, src, *offset, attr_type),
            Inst::AttrStore {
                record,
                offset,
                value,
                attr_type: _,
            } => {
                let base = self.scalar_op(record)?;
                let repr = self
                    .values
                    .get(value)
                    .ok_or_else(|| {
                        LlvmEmitError::UnsupportedInst(format!("undefined value {value:?} in AttrStore"))
                    })?
                    .clone();
                // The extend/where store path is scalar/Text only (no sub-layout).
                self.emit_attr_store(&base, *offset as usize, &repr, None)
            }
            Inst::Where {
                dst,
                src,
                predicate_linkage,
                heading_id,
            } => self.lower_where_inst(*dst, src, predicate_linkage, *heading_id),
            Inst::Extend {
                dst,
                src,
                helper_linkage,
                src_heading_id,
                result_heading_id,
            } => self.lower_extend_inst(
                *dst,
                src,
                helper_linkage,
                *src_heading_id,
                *result_heading_id,
            ),
            Inst::Project {
                dst,
                src,
                src_heading_id,
                result_heading_id,
            } => self.lower_project_inst(*dst, src, *src_heading_id, *result_heading_id),
            Inst::Restructure {
                dst,
                src,
                src_heading_id,
                result_heading_id,
            } => self.lower_restructure_inst(*dst, src, *src_heading_id, *result_heading_id),
            Inst::Rename {
                dst,
                src,
                src_heading_id,
                result_heading_id,
                perm,
            } => self.lower_rename_inst(*dst, src, *src_heading_id, *result_heading_id, perm),
            Inst::Join {
                dst,
                lhs,
                rhs,
                lhs_heading_id,
                rhs_heading_id,
                result_heading_id,
            } => self.lower_join_inst(
                *dst,
                lhs,
                rhs,
                *lhs_heading_id,
                *rhs_heading_id,
                *result_heading_id,
            ),
            Inst::Union {
                dst,
                lhs,
                rhs,
                heading_id,
            } => self.lower_union_inst(*dst, lhs, rhs, *heading_id),
            Inst::Minus {
                dst,
                lhs,
                rhs,
                heading_id,
            } => self.lower_minus_inst(*dst, lhs, rhs, *heading_id),
            Inst::TClose {
                dst,
                src,
                heading_id,
            } => self.lower_tclose_inst(*dst, src, *heading_id),
            Inst::Extract {
                dst,
                src,
                heading_id,
            } => self.lower_extract_inst(*dst, src, *heading_id),
            Inst::RelvarSlotInit { name, heading_id } => {
                self.lower_relvar_slot_init(name, *heading_id)
            }
            Inst::RelvarSlotRelease { name } => self.lower_relvar_slot_release(name),
            Inst::RelvarRead {
                dst,
                name,
                heading_id,
            } => self.lower_relvar_read(*dst, name, *heading_id),
            Inst::PrivateRelvarSlotInit { name, heading_id } => {
                self.lower_private_relvar_slot_init(name, *heading_id)
            }
            Inst::RelvarSlotStore { name, value } => self.lower_relvar_slot_store(name, value),
            Inst::RegisterDatabase => self.lower_register_database(),
            Inst::RegisterPlan { plan_id } => self.lower_register_plan(*plan_id),
            Inst::Query {
                dst,
                plan_id,
                params,
                heading_id,
            } => self.lower_query(*dst, *plan_id, params, *heading_id),
            Inst::Dml { plan_id, params } => self.lower_dml(*plan_id, params),
            Inst::InsertFrom {
                plan_id,
                src,
                heading_id,
            } => {
                // Ship `src`'s in-memory rows into the target via the registered
                // insert template — pass the relation pointer + its heading
                // descriptor (like `coddl_write_relation`) plus the plan id. The
                // returned status is discarded (the runtime aborts on failure).
                let op = self.scalar_op(src)?;
                let status = format!("%v_insert_status.{}", self.next_str);
                self.next_str += 1;
                writeln!(
                    self.body,
                    "    {status} = call i32 @coddl_exec_insert(i32 {plan_id}, ptr {op}, ptr @.heading.{})",
                    heading_id.0,
                )
                .unwrap();
                Ok(())
            }
        }
    }

    /// Resolve the database file (env override → baked default) and register
    /// the logical database so `coddl_query` can find its connection path.
    fn lower_register_database(&mut self) -> Result<(), LlvmEmitError> {
        let len_slot = "%v_db_resolve_len";
        writeln!(self.body, "    {len_slot} = alloca i64, align 8").unwrap();
        let resolved = "%v_db_resolved";
        writeln!(
            self.body,
            "    {resolved} = call ptr @coddl_resolve_op_field(ptr @.db.env_name, i64 {env_len}, ptr @.db.default_path, i64 {def_len}, ptr {len_slot})",
            env_len = self.const_byte_len("@.db.env_name"),
            def_len = self.const_byte_len("@.db.default_path"),
        )
        .unwrap();
        let resolved_len = "%v_db_resolved_len";
        writeln!(self.body, "    {resolved_len} = load i64, ptr {len_slot}").unwrap();
        writeln!(
            self.body,
            "    call i32 @coddl_register_database(ptr @.db.name, i64 {name_len}, ptr {resolved}, i64 {resolved_len})",
            name_len = self.const_byte_len("@.db.name"),
        )
        .unwrap();
        Ok(())
    }

    /// Register one baked plan: SQL text + database name + bind count + the
    /// result heading descriptor, keyed by the dense plan id.
    fn lower_register_plan(&mut self, plan_id: u32) -> Result<(), LlvmEmitError> {
        let (param_count, result_heading_id) = *self.plan_meta.get(&plan_id).ok_or_else(|| {
            LlvmEmitError::UnsupportedInst(format!("RegisterPlan references unknown plan {plan_id}"))
        })?;
        writeln!(
            self.body,
            "    call i32 @coddl_register_plan(i32 {plan_id}, ptr @.plan.{plan_id}.db_name, i64 {db_len}, ptr @.plan.{plan_id}.sql, i64 {sql_len}, i32 {param_count}, ptr @.heading.{hid})",
            db_len = self.const_byte_len(&format!("@.plan.{plan_id}.db_name")),
            sql_len = self.const_byte_len(&format!("@.plan.{plan_id}.sql")),
            hid = result_heading_id,
        )
        .unwrap();
        Ok(())
    }

    /// Build the `CoddlParam` array on the stack for a `query`/`exec` call and
    /// return the `ptr` argument string (`ptr null` when there are no params).
    /// `CoddlParam` is `{ i64 i, ptr, i64 len, i32 kind }` (32-byte stride; the
    /// runtime owns this layout). Shared by `lower_query` and `lower_dml`.
    fn build_coddl_param_array(
        &mut self,
        params: &[(ValueId, ProcType)],
    ) -> Result<String, LlvmEmitError> {
        let n = params.len();
        if n == 0 {
            return Ok("ptr null".to_string());
        }
        let arr = format!("%qparams.{}", self.next_str);
        self.next_str += 1;
        writeln!(
            self.body,
            "    {arr} = alloca [{n} x {{ i64, ptr, i64, i32 }}], align 8"
        )
        .unwrap();
        for (i, (vid, ty)) in params.iter().enumerate() {
            let base = i * 32;
            let kind = kind_tag_for(ty)?;
            match ty {
                ProcType::Integer => {
                    let op = self.scalar_op(vid)?;
                    let slot = self.gep_byte(&arr, base);
                    writeln!(self.body, "    store i64 {op}, ptr {slot}").unwrap();
                }
                ProcType::Boolean => {
                    let op = self.scalar_op(vid)?;
                    let z = format!("%qb.{}", self.next_str);
                    self.next_str += 1;
                    writeln!(self.body, "    {z} = zext i1 {op} to i64").unwrap();
                    let slot = self.gep_byte(&arr, base);
                    writeln!(self.body, "    store i64 {z}, ptr {slot}").unwrap();
                }
                ProcType::Text => {
                    let (ptr_op, len_op) = self.text_ops(vid)?;
                    let ptr_slot = self.gep_byte(&arr, base + 8);
                    let len_slot = self.gep_byte(&arr, base + 16);
                    writeln!(self.body, "    store ptr {ptr_op}, ptr {ptr_slot}").unwrap();
                    writeln!(self.body, "    store i64 {len_op}, ptr {len_slot}").unwrap();
                }
                other => {
                    return Err(LlvmEmitError::UnsupportedInst(format!(
                        "query bind param of type {other:?} not supported"
                    )));
                }
            }
            let kind_slot = self.gep_byte(&arr, base + 24);
            writeln!(self.body, "    store i32 {kind}, ptr {kind_slot}").unwrap();
        }
        Ok(format!("ptr {arr}"))
    }

    /// Execute a registered plan: build the `CoddlParam` array on the stack,
    /// call `coddl_query`, and bind the returned relation pointer to `dst`.
    fn lower_query(
        &mut self,
        dst: ValueId,
        plan_id: u32,
        params: &[(ValueId, ProcType)],
        _heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let n = params.len();
        let dst_name = format!("%v{}", dst.0);
        let params_arg = self.build_coddl_param_array(params)?;
        writeln!(
            self.body,
            "    {dst_name} = call ptr @coddl_query(i32 {plan_id}, {params_arg}, i64 {n})"
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: dst_name,
            },
        );
        Ok(())
    }

    /// Execute a registered DML plan for effect: build the `CoddlParam` array
    /// and call `coddl_exec`. No result is bound (DML returns no rows); the
    /// `CoddlStatus` is discarded (the runtime aborts on a hard failure).
    fn lower_dml(
        &mut self,
        plan_id: u32,
        params: &[(ValueId, ProcType)],
    ) -> Result<(), LlvmEmitError> {
        let n = params.len();
        let params_arg = self.build_coddl_param_array(params)?;
        let status = format!("%v_dml_status.{}", self.next_str);
        self.next_str += 1;
        writeln!(
            self.body,
            "    {status} = call i32 @coddl_exec(i32 {plan_id}, {params_arg}, i64 {n})"
        )
        .unwrap();
        Ok(())
    }

    /// Read the `(ptr, len)` operand pair for a `Text`-typed SSA value.
    fn text_ops(&self, v: &ValueId) -> Result<(String, String), LlvmEmitError> {
        match self.values.get(v) {
            Some(ValueRepr::Text { ptr_op, len_op }) => Ok((ptr_op.clone(), len_op.clone())),
            other => Err(LlvmEmitError::UnsupportedInst(format!(
                "expected Text value, got {other:?}"
            ))),
        }
    }

    /// Emit the materialization call for one public relvar. Resolves
    /// `CODDL_<DB>_FILE` via `coddl_resolve_op_field` first, then calls
    /// `coddl_sqlite_relvar_init` with the (name, env-resolved path,
    /// table, column arrays, descriptor, slot) bundle.
    fn lower_relvar_slot_init(
        &mut self,
        name: &str,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let col_count = *self
            .public_relvar_columns
            .get(name)
            .ok_or_else(|| {
                LlvmEmitError::UnsupportedInst(format!(
                    "RelvarSlotInit references unknown relvar `{name}`"
                ))
            })?;
        let env_name_len = format!("CODDL_{}_FILE", name.to_ascii_uppercase());
        let _ = env_name_len; // length lookup unused — env_name was already stored as a global byte
        // Resolve the env-var override first; the runtime returns
        // (ptr, len_out) — len_out is written into a stack slot we
        // alloca right here.
        let resolved_len_slot = format!("%v_{name}_resolve_len");
        writeln!(
            self.body,
            "    {resolved_len_slot} = alloca i64, align 8"
        )
        .unwrap();
        let resolved_ptr = format!("%v_{name}_resolved");
        writeln!(
            self.body,
            "    {resolved_ptr} = call ptr @coddl_resolve_op_field(ptr @{name}.env_name, i64 {env_len}, ptr @{name}.default_path, i64 {def_len}, ptr {resolved_len_slot})",
            env_len = self.const_byte_len(&format!("@{name}.env_name")),
            def_len = self.const_byte_len(&format!("@{name}.default_path")),
        )
        .unwrap();
        let resolved_len = format!("%v_{name}_resolved_len");
        writeln!(
            self.body,
            "    {resolved_len} = load i64, ptr {resolved_len_slot}"
        )
        .unwrap();
        // Materialize. The runtime stores the RC pointer in the slot
        // and registers it for shutdown release.
        let status = format!("%v_{name}_init_status");
        writeln!(
            self.body,
            "    {status} = call i32 @coddl_sqlite_relvar_init(ptr @{name}.relvar_name, i64 {relvar_len}, ptr {resolved_ptr}, i64 {resolved_len}, ptr @{name}.table_name, i64 {table_len}, ptr @{name}.col_ptrs, ptr @{name}.col_lens, i32 {col_count}, ptr @.heading.{hid}, ptr @{name}_slot)",
            relvar_len = self.const_byte_len(&format!("@{name}.relvar_name")),
            table_len = self.const_byte_len(&format!("@{name}.table_name")),
            hid = heading_id.0,
        )
        .unwrap();
        Ok(())
    }

    fn lower_relvar_slot_release(&mut self, name: &str) -> Result<(), LlvmEmitError> {
        let v = format!("%v_{name}_release_load");
        writeln!(self.body, "    {v} = load ptr, ptr @{name}_slot").unwrap();
        writeln!(self.body, "    call void @coddl_rc_release(ptr {v})").unwrap();
        Ok(())
    }

    /// Init an in-memory `private` relvar's slot with an empty relation. Emits
    /// the slot global (shared with `RelvarRead` / store / release) and the
    /// empty-init call. No SQL source, unlike `lower_relvar_slot_init`.
    fn lower_private_relvar_slot_init(
        &mut self,
        name: &str,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        writeln!(
            self.globals,
            "@{name}_slot = private unnamed_addr global ptr null",
        )
        .unwrap();
        writeln!(
            self.body,
            "    call void @coddl_relvar_slot_init_empty(ptr @.heading.{}, ptr @{name}_slot)",
            heading_id.0,
        )
        .unwrap();
        Ok(())
    }

    /// Store a relation value into a relvar's slot (relational assignment).
    /// Move semantics — the runtime releases the slot's old value and takes
    /// ownership of `value`.
    fn lower_relvar_slot_store(
        &mut self,
        name: &str,
        value: &ValueId,
    ) -> Result<(), LlvmEmitError> {
        let op = self.scalar_op(value)?;
        writeln!(
            self.body,
            "    call void @coddl_relvar_slot_store(ptr {op}, ptr @{name}_slot)",
        )
        .unwrap();
        Ok(())
    }

    fn lower_relvar_read(
        &mut self,
        dst: ValueId,
        name: &str,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let _ = heading_id; // descriptor lookup not needed at read site
        let dst_name = format!("%v{}", dst.0);
        writeln!(self.body, "    {dst_name} = load ptr, ptr @{name}_slot").unwrap();
        writeln!(self.body, "    call void @coddl_rc_retain(ptr {dst_name})").unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: dst_name,
            },
        );
        Ok(())
    }

    /// Look up a public byte-constant global's payload length. Each
    /// `emit_byte_constant` writes a `private unnamed_addr constant
    /// [N x i8]` whose `N` we remember in `byte_const_lens`.
    fn const_byte_len(&self, sym: &str) -> usize {
        *self
            .byte_const_lens
            .get(sym)
            .expect("byte constant length tracked at emit time")
    }

    /// Emit a `private unnamed_addr constant [N x i8] c"..."` global
    /// and record its byte length. Used for the per-relvar string
    /// payloads `coddl_sqlite_relvar_init` reads. Empty payloads are
    /// emitted as a zero-length array so the linker has a definite
    /// symbol — the runtime treats len == 0 the same as a null body.
    fn emit_byte_constant(&mut self, sym: &str, bytes: &[u8]) {
        writeln!(
            self.globals,
            "{sym} = private unnamed_addr constant [{n} x i8] c\"{escaped}\"",
            n = bytes.len(),
            escaped = escape_ir_bytes(bytes),
        )
        .unwrap();
        self.byte_const_lens.insert(sym.to_string(), bytes.len());
    }

    /// Emit a comparison or logical op on scalar SSA values. Result
    /// type is always `i1`.
    fn lower_scalar_op(
        &mut self,
        dst: ValueId,
        op: ScalarOp,
        operand_type: &ProcType,
        lhs: &ValueId,
        rhs: &ValueId,
    ) -> Result<(), LlvmEmitError> {
        // Concatenation: `Text × Text → Text`. The lowerer has already
        // normalized any `Character` operand to Text, so both operands are
        // `(ptr, len)` pairs. The runtime returns the payload pointer; the
        // result length is `lhs_len + rhs_len` (it can't return a fat pointer).
        if matches!(op, ScalarOp::Concat) {
            let (lhs_ptr, lhs_len) = self.text_ops(lhs)?;
            let (rhs_ptr, rhs_len) = self.text_ops(rhs)?;
            let ptr_name = format!("%v{}.ptr", dst.0);
            let len_name = format!("%v{}.len", dst.0);
            writeln!(
                self.body,
                "    {ptr_name} = call ptr @coddl_text_concat(ptr {lhs_ptr}, i64 {lhs_len}, ptr {rhs_ptr}, i64 {rhs_len})"
            )
            .unwrap();
            writeln!(self.body, "    {len_name} = add i64 {lhs_len}, {rhs_len}").unwrap();
            self.values.insert(
                dst,
                ValueRepr::Text {
                    ptr_op: ptr_name,
                    len_op: len_name,
                },
            );
            return Ok(());
        }
        // Text operands aren't inline scalars — a Text cell/value is a
        // `(ptr, len)` pair, so `=`/`<>` route through the runtime's
        // byte comparison instead of `icmp`. (The typechecker only admits
        // `Eq`/`NotEq` on Text; ordering stays Integer-only.)
        if matches!(operand_type, ProcType::Text) {
            let (lhs_ptr, lhs_len) = self.text_ops(lhs)?;
            let (rhs_ptr, rhs_len) = self.text_ops(rhs)?;
            let raw = format!("%v{}.txt", dst.0);
            writeln!(
                self.body,
                "    {raw} = call i8 @coddl_text_eq(ptr {lhs_ptr}, i64 {lhs_len}, ptr {rhs_ptr}, i64 {rhs_len})"
            )
            .unwrap();
            // `coddl_text_eq` returns 1 when equal: `Eq` is `raw != 0`,
            // `NotEq` is `raw == 0`.
            let cmp = match op {
                ScalarOp::Eq => "ne",
                ScalarOp::NotEq => "eq",
                other => {
                    return Err(LlvmEmitError::UnsupportedInst(format!(
                        "operator {other:?} not supported on Text"
                    )))
                }
            };
            let dst_name = format!("%v{}", dst.0);
            writeln!(self.body, "    {dst_name} = icmp {cmp} i8 {raw}, 0").unwrap();
            self.values.insert(
                dst,
                ValueRepr::Scalar {
                    ty: "i1".to_string(),
                    op: dst_name,
                },
            );
            return Ok(());
        }
        let lhs_op = self.scalar_op(lhs)?;
        let rhs_op = self.scalar_op(rhs)?;
        let dst_name = format!("%v{}", dst.0);
        let operand_ty = llvm_value_type(operand_type);
        match op {
            ScalarOp::And | ScalarOp::Or => {
                let instr = if matches!(op, ScalarOp::And) { "and" } else { "or" };
                writeln!(self.body, "    {dst_name} = {instr} i1 {lhs_op}, {rhs_op}").unwrap();
                self.values.insert(
                    dst,
                    ValueRepr::Scalar {
                        ty: "i1".to_string(),
                        op: dst_name,
                    },
                );
            }
            ScalarOp::Add | ScalarOp::Sub | ScalarOp::Mul | ScalarOp::Div => {
                // `Integer × Integer → Integer`; `sdiv` truncates toward zero.
                let instr = match op {
                    ScalarOp::Add => "add",
                    ScalarOp::Sub => "sub",
                    ScalarOp::Mul => "mul",
                    ScalarOp::Div => "sdiv",
                    _ => unreachable!(),
                };
                writeln!(
                    self.body,
                    "    {dst_name} = {instr} {operand_ty} {lhs_op}, {rhs_op}"
                )
                .unwrap();
                self.values.insert(
                    dst,
                    ValueRepr::Scalar {
                        ty: operand_ty.to_string(),
                        op: dst_name,
                    },
                );
            }
            ScalarOp::Eq
            | ScalarOp::NotEq
            | ScalarOp::Lt
            | ScalarOp::Gt
            | ScalarOp::LtEq
            | ScalarOp::GtEq => {
                let pred = match op {
                    ScalarOp::Eq => "eq",
                    ScalarOp::NotEq => "ne",
                    ScalarOp::Lt => "slt",
                    ScalarOp::Gt => "sgt",
                    ScalarOp::LtEq => "sle",
                    ScalarOp::GtEq => "sge",
                    _ => unreachable!(),
                };
                writeln!(
                    self.body,
                    "    {dst_name} = icmp {pred} {operand_ty} {lhs_op}, {rhs_op}"
                )
                .unwrap();
                self.values.insert(
                    dst,
                    ValueRepr::Scalar {
                        ty: "i1".to_string(),
                        op: dst_name,
                    },
                );
            }
            ScalarOp::Concat => unreachable!("Concat handled before the inline-scalar path"),
        }
        Ok(())
    }

    /// Convert a `Character` (inline `i32` codepoint) to a `Text` `(ptr, len)`
    /// value: the runtime's `coddl_char_to_text` gives the payload pointer and
    /// `coddl_utf8_len` the byte length. Used to normalize a `Character`
    /// operand of `||`.
    fn lower_char_to_text(&mut self, dst: ValueId, src: &ValueId) -> Result<(), LlvmEmitError> {
        let cp = self.scalar_op(src)?;
        let ptr_name = format!("%v{}.ptr", dst.0);
        let len_name = format!("%v{}.len", dst.0);
        writeln!(
            self.body,
            "    {ptr_name} = call ptr @coddl_char_to_text(i32 {cp})"
        )
        .unwrap();
        writeln!(
            self.body,
            "    {len_name} = call i64 @coddl_utf8_len(i32 {cp})"
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Text {
                ptr_op: ptr_name,
                len_op: len_name,
            },
        );
        Ok(())
    }

    /// Read one attribute from a record pointer at the static byte
    /// offset. Phase 20 cells are Integer/Boolean (i64 in memory) and
    /// Text (a 16-byte `(ptr, len)` pair). Boolean cells round-trip
    /// through `i64 → trunc i1` so the SSA value type matches
    /// `llvm_value_type(Boolean)`.
    fn lower_attr_load(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        offset: u32,
        attr_type: &ProcType,
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        match attr_type {
            ProcType::Integer => {
                let slot = self.gep_byte(&src_op, offset as usize);
                let name = format!("%v{}", dst.0);
                writeln!(self.body, "    {name} = load i64, ptr {slot}").unwrap();
                self.values.insert(
                    dst,
                    ValueRepr::Scalar {
                        ty: "i64".to_string(),
                        op: name,
                    },
                );
                Ok(())
            }
            ProcType::Boolean => {
                let slot = self.gep_byte(&src_op, offset as usize);
                // The relation cell encodes Boolean as i64; pull the
                // raw 64-bit slot and truncate to i1.
                let raw = format!("%v{}.raw", dst.0);
                writeln!(self.body, "    {raw} = load i64, ptr {slot}").unwrap();
                let name = format!("%v{}", dst.0);
                writeln!(self.body, "    {name} = trunc i64 {raw} to i1").unwrap();
                self.values.insert(
                    dst,
                    ValueRepr::Scalar {
                        ty: "i1".to_string(),
                        op: name,
                    },
                );
                Ok(())
            }
            ProcType::Text => {
                let ptr_slot = self.gep_byte(&src_op, offset as usize);
                let len_slot = self.gep_byte(&src_op, offset as usize + 8);
                let ptr_name = format!("%v{}.ptr", dst.0);
                let len_name = format!("%v{}.len", dst.0);
                writeln!(self.body, "    {ptr_name} = load ptr, ptr {ptr_slot}").unwrap();
                writeln!(self.body, "    {len_name} = load i64, ptr {len_slot}").unwrap();
                self.values.insert(
                    dst,
                    ValueRepr::Text {
                        ptr_op: ptr_name,
                        len_op: len_name,
                    },
                );
                Ok(())
            }
            other => Err(LlvmEmitError::UnsupportedInst(format!(
                "AttrLoad of type {other:?} not yet supported"
            ))),
        }
    }

    /// Emit `call ptr @coddl_relation_where(src, &desc, &pred)`.
    fn lower_where_inst(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        predicate_linkage: &str,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_where(ptr {src_op}, ptr @.heading.{}, ptr @{predicate_linkage})",
            heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    fn lower_extend_inst(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        helper_linkage: &str,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_extend(ptr {src_op}, ptr @.heading.{}, ptr @.heading.{}, ptr @{helper_linkage})",
            src_heading_id.0, result_heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    /// Emit `call ptr @coddl_relation_project(src, &src_desc, &result_desc)`.
    /// The runtime narrows each record to the kept attributes and re-seals;
    /// `dst` carries the result (narrowed) heading.
    fn lower_project_inst(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_project(ptr {src_op}, ptr @.heading.{}, ptr @.heading.{})",
            src_heading_id.0, result_heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    /// Emit `call ptr @coddl_relation_restructure(src, &src_desc, &result_desc)`
    /// (surface `wrap`/`unwrap`). The runtime permutes each record's leaf cells
    /// into the destination layout and re-seals; `dst` carries the restructured
    /// heading.
    fn lower_restructure_inst(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_restructure(ptr {src_op}, ptr @.heading.{}, ptr @.heading.{})",
            src_heading_id.0, result_heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    /// Emit `Inst::Rename`: a static `u32` permutation array, then
    /// `call ptr @coddl_relation_rename(src, &src_desc, &result_desc, perm, n)`.
    /// The runtime permutes each record into the renamed layout and re-seals.
    fn lower_join_inst(
        &mut self,
        dst: ValueId,
        lhs: &ValueId,
        rhs: &ValueId,
        lhs_heading_id: HeadingId,
        rhs_heading_id: HeadingId,
        result_heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let lhs_op = self.scalar_op(lhs)?;
        let rhs_op = self.scalar_op(rhs)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_join(ptr {lhs_op}, ptr @.heading.{}, ptr {rhs_op}, ptr @.heading.{}, ptr @.heading.{})",
            lhs_heading_id.0, rhs_heading_id.0, result_heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    /// Emit `Inst::Union`: `call ptr @coddl_relation_union(lhs, rhs, &desc)`.
    /// Identical headings ⇒ one descriptor for both operands and the result.
    fn lower_union_inst(
        &mut self,
        dst: ValueId,
        lhs: &ValueId,
        rhs: &ValueId,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let lhs_op = self.scalar_op(lhs)?;
        let rhs_op = self.scalar_op(rhs)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_union(ptr {lhs_op}, ptr {rhs_op}, ptr @.heading.{})",
            heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    /// Emit `Inst::Minus`: `call ptr @coddl_relation_minus(lhs, rhs, &desc)`.
    /// Identical headings ⇒ one descriptor for both operands and the result.
    fn lower_minus_inst(
        &mut self,
        dst: ValueId,
        lhs: &ValueId,
        rhs: &ValueId,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let lhs_op = self.scalar_op(lhs)?;
        let rhs_op = self.scalar_op(rhs)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_minus(ptr {lhs_op}, ptr {rhs_op}, ptr @.heading.{})",
            heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    /// Emit `Inst::TClose`: `call ptr @coddl_relation_tclose(src, &desc)`.
    /// Result heading == operand heading ⇒ one descriptor for both.
    fn lower_tclose_inst(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_tclose(ptr {src_op}, ptr @.heading.{})",
            heading_id.0,
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    fn lower_rename_inst(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        src_heading_id: HeadingId,
        result_heading_id: HeadingId,
        perm: &[u32],
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        // The permutation, baked as read-only data under a module-unique name.
        let perm_id = self.next_str;
        self.next_str += 1;
        write!(
            self.globals,
            "@.perm.{} = private unnamed_addr constant [{} x i32] [",
            perm_id,
            perm.len()
        )
        .unwrap();
        for (i, p) in perm.iter().enumerate() {
            if i > 0 {
                self.globals.push_str(", ");
            }
            write!(self.globals, "i32 {p}").unwrap();
        }
        writeln!(self.globals, "]").unwrap();
        let name = format!("%v{}", dst.0);
        writeln!(
            self.body,
            "    {name} = call ptr @coddl_relation_rename(ptr {src_op}, ptr @.heading.{}, ptr @.heading.{}, ptr @.perm.{}, i64 {})",
            src_heading_id.0, result_heading_id.0, perm_id, perm.len(),
        )
        .unwrap();
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: name,
            },
        );
        Ok(())
    }

    /// Emit `Inst::Extract`. Calls the cardinality-checking runtime
    /// extern (which aborts on cardinality != 1), then reads each
    /// heading attribute from the returned record pointer via the
    /// same byte-offset GEP+load shape `Inst::AttrLoad` uses. The
    /// resulting per-field SSA values bundle into a
    /// `ValueRepr::Tuple` so downstream field-access machinery
    /// (Phase 18) reads from this tuple directly.
    fn lower_extract_inst(
        &mut self,
        dst: ValueId,
        src: &ValueId,
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let src_op = self.scalar_op(src)?;
        let layout = self
            .heading_layouts
            .get(heading_id.0 as usize)
            .ok_or_else(|| {
                LlvmEmitError::UnsupportedInst(format!(
                    "unknown heading_id {} in Extract",
                    heading_id.0
                ))
            })?
            .clone();
        // Call the cardinality check + record-ptr extern.
        let record_ptr = format!("%v{}.rec", dst.0);
        writeln!(
            self.body,
            "    {record_ptr} = call ptr @coddl_extract_check_cardinality(ptr {src_op}, ptr @.heading.{})",
            heading_id.0,
        )
        .unwrap();
        // For each attribute, read the value into a fresh SSA
        // ValueRepr; bundle into a Tuple.
        let mut fields: Vec<(String, ValueRepr)> = Vec::with_capacity(layout.attrs.len());
        for (i, attr) in layout.attrs.iter().enumerate() {
            let attr_type = proc_type_from_kind_llvm(attr.kind);
            let repr = self.read_attr_repr(
                &record_ptr,
                attr.offset as usize,
                &attr_type,
                &format!("v{}.f{i}", dst.0),
            )?;
            fields.push((attr.name.clone(), repr));
        }
        self.values.insert(dst, ValueRepr::Tuple { fields });
        Ok(())
    }

    /// Read one attribute from a record pointer (in the relation's
    /// payload) at the static byte offset, returning the
    /// `ValueRepr` for the field. Mirrors `lower_attr_load`'s logic
    /// but produces a `ValueRepr` directly instead of inserting into
    /// `self.values` — used by `Inst::Extract` to build the tuple
    /// without minting separate per-field ValueIds.
    fn read_attr_repr(
        &mut self,
        base: &str,
        byte_offset: usize,
        attr_type: &ProcType,
        name_hint: &str,
    ) -> Result<ValueRepr, LlvmEmitError> {
        match attr_type {
            ProcType::Integer => {
                let slot = self.gep_byte(base, byte_offset);
                let name = format!("%{name_hint}");
                writeln!(self.body, "    {name} = load i64, ptr {slot}").unwrap();
                Ok(ValueRepr::Scalar {
                    ty: "i64".to_string(),
                    op: name,
                })
            }
            ProcType::Boolean => {
                let slot = self.gep_byte(base, byte_offset);
                let raw = format!("%{name_hint}.raw");
                writeln!(self.body, "    {raw} = load i64, ptr {slot}").unwrap();
                let name = format!("%{name_hint}");
                writeln!(self.body, "    {name} = trunc i64 {raw} to i1").unwrap();
                Ok(ValueRepr::Scalar {
                    ty: "i1".to_string(),
                    op: name,
                })
            }
            ProcType::Text => {
                let ptr_slot = self.gep_byte(base, byte_offset);
                let len_slot = self.gep_byte(base, byte_offset + 8);
                let ptr_name = format!("%{name_hint}.ptr");
                let len_name = format!("%{name_hint}.len");
                writeln!(self.body, "    {ptr_name} = load ptr, ptr {ptr_slot}").unwrap();
                writeln!(self.body, "    {len_name} = load i64, ptr {len_slot}").unwrap();
                Ok(ValueRepr::Text {
                    ptr_op: ptr_name,
                    len_op: len_name,
                })
            }
            other => Err(LlvmEmitError::UnsupportedInst(format!(
                "Extract attribute of type {other:?} not yet supported"
            ))),
        }
    }

    /// Read out a `Scalar { ty: ptr, op }` for an RC-managed pointer
    /// value. Used by Retain / Release / WriteRelation, all of which
    /// take the relation pointer as their first operand.
    fn scalar_op(&self, v: &ValueId) -> Result<String, LlvmEmitError> {
        let repr = self
            .values
            .get(v)
            .ok_or_else(|| LlvmEmitError::UnsupportedInst(format!("undefined value {v:?}")))?;
        match repr {
            ValueRepr::Scalar { op, .. } => Ok(op.clone()),
            other => Err(LlvmEmitError::UnsupportedInst(format!(
                "expected scalar pointer, got {other:?}"
            ))),
        }
    }

    /// Lower `Inst::RelationLit` to a sequence of LLVM ops:
    ///
    /// 1. `call ptr @coddl_rc_alloc(record_size * count, count,
    ///                              kind=0, @.heading.<id>)`
    /// 2. For each tuple, in source order: compute the i-th record's
    ///    address (`getelementptr`) and store each attribute's
    ///    flattened operands at the right offset.
    /// 3. `call void @coddl_relation_seal(ptr, @.heading.<id>)`.
    ///
    /// The destination `ValueRepr` is the relation pointer as a
    /// `Scalar { ty: "ptr", op: "%vN" }`.
    fn lower_relation_lit(
        &mut self,
        dst: ValueId,
        tuples: &[ValueId],
        heading_id: HeadingId,
    ) -> Result<(), LlvmEmitError> {
        let layout = self
            .heading_layouts
            .get(heading_id.0 as usize)
            .ok_or_else(|| {
                LlvmEmitError::UnsupportedInst(format!(
                    "unknown heading_id {} in RelationLit",
                    heading_id.0
                ))
            })?
            .clone();
        let count = tuples.len();
        let record_size = layout.record_size as usize;
        let payload_bytes = record_size * count;
        let dst_name = format!("%v{}", dst.0);
        // 1. Allocate.
        writeln!(
            self.body,
            "    {dst_name} = call ptr @coddl_rc_alloc(i64 {payload_bytes}, i32 {count}, i32 0, ptr @.heading.{})",
            heading_id.0,
        )
        .unwrap();
        // 2. Write each record's bytes. For each tuple's field, get
        //    the ValueRepr (already flattened) and store into the
        //    correct (record + attribute) byte offset.
        for (record_idx, tuple_vid) in tuples.iter().enumerate() {
            let tuple_repr = self.values.get(tuple_vid).cloned().ok_or_else(|| {
                LlvmEmitError::UnsupportedInst(format!(
                    "undefined tuple value {tuple_vid:?} in RelationLit"
                ))
            })?;
            let tuple_fields = match &tuple_repr {
                ValueRepr::Tuple { fields } => fields,
                other => {
                    return Err(LlvmEmitError::UnsupportedInst(format!(
                        "RelationLit operand is not a Tuple: {other:?}"
                    )));
                }
            };
            for attr in &layout.attrs {
                let field_repr = tuple_fields
                    .iter()
                    .find(|(n, _)| n == &attr.name)
                    .map(|(_, r)| r)
                    .ok_or_else(|| {
                        LlvmEmitError::UnsupportedInst(format!(
                            "tuple missing attribute `{}` for relation layout",
                            attr.name
                        ))
                    })?;
                let byte_offset = record_idx * record_size + attr.offset as usize;
                self.emit_attr_store(&dst_name, byte_offset, field_repr, attr.sub.as_ref())?;
            }
        }
        // 3. Seal.
        writeln!(
            self.body,
            "    call void @coddl_relation_seal(ptr {dst_name}, ptr @.heading.{})",
            heading_id.0,
        )
        .unwrap();
        // 4. Record the dst's ValueRepr.
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "ptr".to_string(),
                op: dst_name,
            },
        );
        Ok(())
    }

    /// Store one attribute's flattened operands into the relation's
    /// payload at `byte_offset`. Integer/Boolean: one i64 store.
    /// Text: two stores (ptr, i64) at byte_offset and byte_offset+8.
    fn emit_attr_store(
        &mut self,
        base: &str,
        byte_offset: usize,
        repr: &ValueRepr,
        sub: Option<&RecordLayout>,
    ) -> Result<(), LlvmEmitError> {
        match repr {
            ValueRepr::Scalar { ty, op } if ty == "i64" => {
                let slot = self.gep_byte(base, byte_offset);
                writeln!(self.body, "    store i64 {op}, ptr {slot}").unwrap();
                Ok(())
            }
            ValueRepr::Scalar { ty, .. } => Err(LlvmEmitError::UnsupportedInst(format!(
                "scalar of type `{ty}` not yet stored into relation cell"
            ))),
            ValueRepr::Text { ptr_op, len_op } => {
                let slot_ptr = self.gep_byte(base, byte_offset);
                let slot_len = self.gep_byte(base, byte_offset + 8);
                writeln!(self.body, "    store ptr {ptr_op}, ptr {slot_ptr}").unwrap();
                writeln!(self.body, "    store i64 {len_op}, ptr {slot_len}").unwrap();
                Ok(())
            }
            // Inline nested-tuple cell: store each component into the sub-region
            // at `byte_offset + sub_attr.offset`, recursing (the sub-layout gives
            // offsets; the `ValueRepr::Tuple` gives values, both name-canonical).
            ValueRepr::Tuple { fields } => {
                let sub = sub.ok_or_else(|| {
                    LlvmEmitError::UnsupportedInst(
                        "tuple cell store without a sub-layout".into(),
                    )
                })?;
                for sub_attr in &sub.attrs {
                    let field = fields
                        .iter()
                        .find(|(n, _)| n == &sub_attr.name)
                        .map(|(_, r)| r.clone())
                        .ok_or_else(|| {
                            LlvmEmitError::UnsupportedInst(format!(
                                "tuple value missing field `{}` for cell layout",
                                sub_attr.name
                            ))
                        })?;
                    self.emit_attr_store(
                        base,
                        byte_offset + sub_attr.offset as usize,
                        &field,
                        sub_attr.sub.as_ref(),
                    )?;
                }
                Ok(())
            }
        }
    }

    /// Emit a `getelementptr` for `base + byte_offset` and return the
    /// fresh SSA name holding the resulting pointer. Used inside
    /// `Inst::RelationLit` to compute per-attribute slot addresses.
    fn gep_byte(&mut self, base: &str, byte_offset: usize) -> String {
        let name = format!("%gep.{}", self.next_str);
        self.next_str += 1;
        writeln!(
            self.body,
            "    {name} = getelementptr inbounds i8, ptr {base}, i64 {byte_offset}",
        )
        .unwrap();
        name
    }

    fn lower_const_text(&mut self, dst: ValueId, bytes: &[u8]) {
        let name = format!("@.str.{}", self.next_str);
        self.next_str += 1;
        let len = bytes.len();

        writeln!(
            self.globals,
            "{name} = private unnamed_addr constant [{len} x i8] c\"{}\"",
            escape_ir_bytes(bytes),
        )
        .unwrap();

        self.values.insert(
            dst,
            ValueRepr::Text {
                ptr_op: name,
                len_op: format!("{len}"),
            },
        );
    }

    fn lower_call(
        &mut self,
        dst: Option<ValueId>,
        callee: &str,
        args: &[ValueId],
        return_type: &ProcType,
    ) -> Result<(), LlvmEmitError> {
        let mut call_args: Vec<String> = Vec::new();
        for arg in args {
            let repr = self
                .values
                .get(arg)
                .ok_or_else(|| LlvmEmitError::UnsupportedInst(format!("undefined value {arg:?}")))?
                .clone();
            repr.push_call_operands(&mut call_args);
        }

        let ret_ty = llvm_return_type(return_type);
        let dst_prefix = match dst {
            Some(v) if !matches!(return_type, ProcType::Unit) => {
                let name = format!("%v{}", v.0);
                self.values.insert(
                    v,
                    ValueRepr::Scalar {
                        ty: ret_ty.clone(),
                        op: name.clone(),
                    },
                );
                format!("{name} = ")
            }
            _ => String::new(),
        };

        writeln!(
            self.body,
            "    {dst_prefix}call {ret_ty} @{callee}({args})",
            args = call_args.join(", "),
        )
        .unwrap();
        Ok(())
    }

    fn emit_terminator(
        &mut self,
        term: &Terminator,
        is_main: bool,
        return_type: &ProcType,
    ) -> Result<(), LlvmEmitError> {
        match term {
            Terminator::Return(None) if is_main => {
                writeln!(self.body, "    ret i32 0").unwrap();
            }
            Terminator::Return(None) => {
                writeln!(self.body, "    ret void").unwrap();
            }
            Terminator::Return(Some(v)) => {
                let repr = self
                    .values
                    .get(v)
                    .ok_or_else(|| {
                        LlvmEmitError::UnsupportedInst(format!("undefined return value {v:?}"))
                    })?
                    .clone();
                match repr {
                    ValueRepr::Scalar { ty, op } => {
                        // Predicate functions declare `i8` at the C
                        // ABI for Boolean returns, but the SSA value
                        // type is `i1`. Insert a `zext` so the IR
                        // type-checks.
                        if matches!(return_type, ProcType::Boolean) && ty == "i1" {
                            let widened = format!("{op}.b");
                            writeln!(
                                self.body,
                                "    {widened} = zext i1 {op} to i8"
                            )
                            .unwrap();
                            writeln!(self.body, "    ret i8 {widened}").unwrap();
                        } else {
                            writeln!(self.body, "    ret {ty} {op}").unwrap();
                        }
                    }
                    ValueRepr::Text { .. } => {
                        return Err(LlvmEmitError::UnsupportedInst(
                            "returning Text by value not yet supported".into(),
                        ));
                    }
                    ValueRepr::Tuple { .. } => {
                        return Err(LlvmEmitError::UnsupportedInst(
                            "returning Tuple by value not yet supported".into(),
                        ));
                    }
                }
            }
            Terminator::Unreachable => {
                writeln!(self.body, "    unreachable").unwrap();
            }
        }
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Map a `record_layout` attribute kind (numeric tag) to its
/// `ProcType`. Mirrors `coddl_procir::layout::kind_tag` constants;
/// kept here so the LLVM backend doesn't have to depend on the
/// internal `coddl_procir::lower` helper.
fn proc_type_from_kind_llvm(kind: u32) -> ProcType {
    use coddl_procir::kind_tag;
    match kind {
        k if k == kind_tag::INTEGER => ProcType::Integer,
        k if k == kind_tag::BOOLEAN => ProcType::Boolean,
        k if k == kind_tag::TEXT => ProcType::Text,
        other => unreachable!("unsupported attr kind {other} in Extract"),
    }
}

/// Map a scalar `ProcType` to its `CoddlAttrKind` tag for a `CoddlParam`.
fn kind_tag_for(ty: &ProcType) -> Result<u32, LlvmEmitError> {
    use coddl_procir::kind_tag;
    match ty {
        ProcType::Integer => Ok(kind_tag::INTEGER),
        ProcType::Boolean => Ok(kind_tag::BOOLEAN),
        ProcType::Text => Ok(kind_tag::TEXT),
        other => Err(LlvmEmitError::UnsupportedInst(format!(
            "query param of type {other:?} has no CoddlParam kind"
        ))),
    }
}

fn llvm_return_type(ty: &ProcType) -> String {
    match ty {
        ProcType::Unit => "void".to_string(),
        ProcType::Tuple(h) if h.is_empty() => "void".to_string(),
        // Booleans cross the C ABI as `i8` (matches Rust's `bool`
        // repr); inside an LLVM function the SSA is `i1` and gets
        // zext'd at the return site.
        ProcType::Boolean => "i8".to_string(),
        other => llvm_value_type(other).to_string(),
    }
}

fn llvm_value_type(ty: &ProcType) -> &'static str {
    match ty {
        ProcType::Integer => "i64",
        ProcType::Rational => "i64",
        ProcType::Approximate => "double",
        ProcType::Text => "ptr",
        ProcType::Character => "i32",
        ProcType::Binary => "ptr",
        ProcType::Byte => "i8",
        ProcType::Boolean => "i1",
        ProcType::Unit => "void",
        ProcType::Pointer => "ptr",
        // Relations cross the ABI as a single payload pointer; the
        // heading lives in static data, reached via the descriptor.
        ProcType::Relation(_) => "ptr",
        // Non-flattened tuple uses are limited to multi-attribute
        // returns, which need return-pair codegen and aren't on Phase
        // 18's path. Empty tuples lower to `void` via
        // `llvm_return_type` before reaching this branch.
        ProcType::Tuple(_) => unreachable!(
            "Tuple ProcType must be flattened at ABI boundaries; bare Tuple seen in scalar context"
        ),
    }
}

/// Recursively flatten a `ProcType` into the LLVM IR types it occupies
/// at an ABI boundary. Text/Binary expand to `(ptr, i64)`; Tuple
/// expands per-attribute in canonical heading order, nested tuples
/// recursively. Empty Tuple contributes zero entries.
fn push_param_types(out: &mut Vec<String>, ty: &ProcType) {
    match ty {
        ProcType::Text | ProcType::Binary => {
            out.push("ptr".to_string());
            out.push("i64".to_string());
        }
        ProcType::Tuple(heading) => {
            for (_, attr_ty) in heading.attrs() {
                push_param_types(out, &proc_type_from_attr(attr_ty));
            }
        }
        other => out.push(llvm_value_type(other).to_string()),
    }
}

/// Same recursion as [`push_param_types`], but emits `<ty> %<name>`
/// fragments for use in a `define` line. Each leaf attribute gets a
/// unique LLVM SSA name derived from the surface field-path
/// (`%user.address.zip.ptr`, etc.).
fn push_param_decl(out: &mut Vec<String>, name: &str, ty: &ProcType) {
    match ty {
        ProcType::Text | ProcType::Binary => {
            out.push(format!("ptr %{name}.ptr"));
            out.push(format!("i64 %{name}.len"));
        }
        ProcType::Tuple(heading) => {
            for (attr_name, attr_ty) in heading.attrs() {
                let sub_name = format!("{name}.{attr_name}");
                push_param_decl(out, &sub_name, &proc_type_from_attr(attr_ty));
            }
        }
        other => out.push(format!("{ty} %{name}", ty = llvm_value_type(other))),
    }
}

/// Heading attributes carry surface `Type`s; backends reason in
/// `ProcType`. This is the same mapping the lowerer uses; centralized
/// here so the codegen helpers don't need to depend on the lowerer
/// module.
fn proc_type_from_attr(ty: &Type) -> ProcType {
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
        Type::Relation(_) => ProcType::Pointer,
        Type::Unknown => unreachable!("Type::Unknown reached codegen"),
    }
}

fn escape_ir_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 4);
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\22"),
            b'\\' => out.push_str("\\5C"),
            0x20..=0x7e => out.push(b as char),
            _ => write!(out, "\\{b:02X}").unwrap(),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_diagnostics::FileId;
    use coddl_procir::lower;

    const HELLO_WORLD: &str = "program hello_world;\n\
                               \n\
                               oper main {}\n\
                               [\n\
                                   write_line{message: \"Hello, world!\"};\n\
                               ];\n";

    fn emit_ok(src: &str) -> String {
        let out = lower(src, FileId(0));
        let module = out.module.expect("typechecked");
        let mut backend = LlvmBackend::new();
        backend.emit(&module).expect("emit ok")
    }

    #[test]
    fn hello_world_ir_declares_extern() {
        let ir = emit_ok(HELLO_WORLD);
        assert!(
            ir.contains("declare void @coddl_write_line(ptr, i64)"),
            "no extern declaration in:\n{ir}"
        );
    }

    #[test]
    fn hello_world_ir_defines_main_returning_i32() {
        let ir = emit_ok(HELLO_WORLD);
        assert!(
            ir.contains("define i32 @main()"),
            "main signature missing:\n{ir}"
        );
        assert!(ir.contains("    ret i32 0"), "main return missing:\n{ir}");
    }

    #[test]
    fn hello_world_ir_contains_string_constant() {
        let ir = emit_ok(HELLO_WORLD);
        assert!(
            ir.contains("@.str.0 = private unnamed_addr constant [13 x i8] c\"Hello, world!\""),
            "string constant missing:\n{ir}"
        );
    }

    #[test]
    fn hello_world_ir_call_passes_ptr_and_len() {
        let ir = emit_ok(HELLO_WORLD);
        assert!(
            ir.contains("call void @coddl_write_line(ptr @.str.0, i64 13)"),
            "call site malformed:\n{ir}"
        );
    }

    #[test]
    fn proctype_to_llvm_type_covers_built_in_scalars() {
        for ty in [
            ProcType::Integer,
            ProcType::Rational,
            ProcType::Approximate,
            ProcType::Text,
            ProcType::Character,
            ProcType::Binary,
            ProcType::Byte,
            ProcType::Boolean,
            ProcType::Pointer,
        ] {
            assert!(!llvm_value_type(&ty).is_empty(), "{ty:?}");
        }
        assert_eq!(llvm_return_type(&ProcType::Unit), "void");
    }

    // ── Tuple ABI flattening (Phase 18) ──────────────────────────────

    #[test]
    fn tuple_let_field_access_flattens_call_to_one_text_pair() {
        // A let-bound tuple has its field projected and passed to
        // write_line. At the LLVM call site this should reduce to the
        // same `(ptr @.str.0, i64 N)` pair as the direct literal —
        // tuples are pure compile-time grouping.
        let src = "oper main {} [ \
                   let t = {message: \"hi\"}; \
                   write_line{message: t.message}; \
                   ];";
        let ir = emit_ok(src);
        assert!(
            ir.contains("call void @coddl_write_line(ptr @.str.0, i64 2)"),
            "expected flattened call site, got:\n{ir}"
        );
    }

    #[test]
    fn empty_tuple_param_decl_contributes_zero_operands() {
        // Direct unit test of the flattening helper: an empty tuple
        // parameter must declare zero operands.
        let mut params = Vec::new();
        push_param_types(
            &mut params,
            &ProcType::Tuple(coddl_procir::Heading::empty()),
        );
        assert!(params.is_empty());
    }

    #[test]
    fn nested_tuple_param_flattens_recursively() {
        // A tuple { inner: Tuple { ptr: Text } } as a parameter
        // expands into the same (ptr, i64) pair as a bare Text.
        let heading = coddl_procir::Heading::new(vec![(
            "inner".to_string(),
            Type::Tuple(coddl_procir::Heading::new(vec![(
                "msg".to_string(),
                Type::Text,
            )])),
        )]);
        let mut params = Vec::new();
        push_param_types(&mut params, &ProcType::Tuple(heading));
        assert_eq!(params, vec!["ptr".to_string(), "i64".to_string()]);
    }
}

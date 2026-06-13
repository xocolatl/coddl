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
    ScalarOp, Terminator, Type, ValueId,
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
        if !module.public_relvars.is_empty() {
            self.emit_runtime_relvar_externs();
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
        // Phase 20 `where`: takes (src, desc, pred_fn) and returns
        // a fresh relation pointer (rc=1).
        writeln!(self.body, "declare ptr @coddl_relation_where(ptr, ptr, ptr)").unwrap();
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

    /// Emit the three globals that describe one heading: a per-attr
    /// name string each (`@.attrname.<id>.<i>`), the attribute array
    /// (`@.attrs.<id>`), and the descriptor struct (`@.heading.<id>`).
    /// Layout matches `coddl_runtime::CoddlHeadingDesc` /
    /// `CoddlAttrDesc`.
    fn emit_heading_descriptor(
        &mut self,
        id: HeadingId,
        heading: &coddl_procir::Heading,
    ) -> Result<(), LlvmEmitError> {
        let layout = record_layout(heading);
        // Per-attribute name strings.
        for (i, attr) in layout.attrs.iter().enumerate() {
            let name_bytes = attr.name.as_bytes();
            writeln!(
                self.globals,
                "@.attrname.{}.{} = private unnamed_addr constant [{} x i8] c\"{}\"",
                id.0,
                i,
                name_bytes.len(),
                escape_ir_bytes(name_bytes),
            )
            .unwrap();
        }
        // Attribute array. Each element matches `CoddlAttrDesc`:
        // { ptr, i32, i32, i32 } — name, name_len, kind, offset.
        // Natural padding on the host adds 4 bytes after the last
        // i32; LLVM struct layout matches.
        write!(
            self.globals,
            "@.attrs.{} = private unnamed_addr constant [{} x {{ ptr, i32, i32, i32 }}] [",
            id.0,
            layout.attrs.len()
        )
        .unwrap();
        for (i, attr) in layout.attrs.iter().enumerate() {
            if i > 0 {
                self.globals.push_str(", ");
            }
            let name_len = attr.name.as_bytes().len();
            write!(
                self.globals,
                "{{ ptr, i32, i32, i32 }} {{ ptr @.attrname.{}.{}, i32 {}, i32 {}, i32 {} }}",
                id.0, i, name_len, attr.kind, attr.offset,
            )
            .unwrap();
        }
        writeln!(self.globals, "]").unwrap();
        // The descriptor struct. Matches `CoddlHeadingDesc`:
        // { i32 attr_count, i32 record_size, ptr attrs }.
        writeln!(
            self.globals,
            "@.heading.{} = private unnamed_addr constant {{ i32, i32, ptr }} {{ i32 {}, i32 {}, ptr @.attrs.{} }}",
            id.0,
            layout.attrs.len(),
            layout.record_size,
            id.0,
        )
        .unwrap();
        Ok(())
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
            Inst::AttrLoad {
                dst,
                src,
                offset,
                attr_type,
            } => self.lower_attr_load(*dst, src, *offset, attr_type),
            Inst::Where {
                dst,
                src,
                predicate_linkage,
                heading_id,
            } => self.lower_where_inst(*dst, src, predicate_linkage, *heading_id),
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
        let lhs_op = self.scalar_op(lhs)?;
        let rhs_op = self.scalar_op(rhs)?;
        let dst_name = format!("%v{}", dst.0);
        let operand_ty = llvm_value_type(operand_type);
        match op {
            ScalarOp::And | ScalarOp::Or => {
                let instr = if matches!(op, ScalarOp::And) { "and" } else { "or" };
                writeln!(
                    self.body,
                    "    {dst_name} = {instr} i1 {lhs_op}, {rhs_op}"
                )
                .unwrap();
            }
            _ => {
                let pred = match op {
                    ScalarOp::Eq => "eq",
                    ScalarOp::NotEq => "ne",
                    ScalarOp::Lt => "slt",
                    ScalarOp::Gt => "sgt",
                    ScalarOp::LtEq => "sle",
                    ScalarOp::GtEq => "sge",
                    ScalarOp::And | ScalarOp::Or => unreachable!(),
                };
                writeln!(
                    self.body,
                    "    {dst_name} = icmp {pred} {operand_ty} {lhs_op}, {rhs_op}"
                )
                .unwrap();
            }
        }
        self.values.insert(
            dst,
            ValueRepr::Scalar {
                ty: "i1".to_string(),
                op: dst_name,
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
                self.emit_attr_store(&dst_name, byte_offset, field_repr)?;
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
            ValueRepr::Tuple { .. } => Err(LlvmEmitError::UnsupportedInst(
                "nested Tuple cells not yet supported in relation records".into(),
            )),
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

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
    Terminator, Type, ValueId,
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
        if module.functions.iter().any(Function::is_extern) || !module.headings.is_empty() {
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

        writeln!(
            self.body,
            "define {ret_ty} @{linkage}({args}) {{",
            linkage = func.linkage_name,
            args = params.join(", "),
        )
        .unwrap();

        for block in &func.blocks {
            self.emit_block(block, is_main)?;
        }

        writeln!(self.body, "}}").unwrap();
        Ok(())
    }

    fn emit_block(&mut self, block: &BasicBlock, is_main: bool) -> Result<(), LlvmEmitError> {
        writeln!(self.body, "{}:", block.id).unwrap();
        for inst in &block.insts {
            self.emit_inst(inst)?;
        }
        self.emit_terminator(&block.terminator, is_main)?;
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

    fn emit_terminator(&mut self, term: &Terminator, is_main: bool) -> Result<(), LlvmEmitError> {
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
                        writeln!(self.body, "    ret {ty} {op}").unwrap();
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

fn llvm_return_type(ty: &ProcType) -> String {
    match ty {
        ProcType::Unit => "void".to_string(),
        ProcType::Tuple(h) if h.is_empty() => "void".to_string(),
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

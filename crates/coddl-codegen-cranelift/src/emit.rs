//! ProcIR → Cranelift object emission.
//!
//! Walks a `Module` and produces a native object file as `Vec<u8>`. Uses
//! `cranelift-native` for the host ISA and `cranelift-object` for the
//! object writer. `Text` values are decomposed at the C-call boundary
//! into a `(ptr, i64)` pair — same ABI as the LLVM backend.
//!
//! See `docs/codegen.md` for the spec.

use std::collections::HashMap;

use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Value as CrValue};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module as ClModule};
use cranelift_object::{ObjectBuilder, ObjectModule};

use coddl_procir::{
    record_layout, BasicBlock, Codegen, Const, Function, HeadingId, Inst, Module, ProcType,
    RecordLayout, ScalarOp, Terminator, Type, ValueId,
};

use crate::error::CraneliftEmitError;

pub struct CraneliftBackend;

impl CraneliftBackend {
    pub fn new() -> Result<Self, CraneliftEmitError> {
        // Probe the host ISA at construction so a missing-target
        // environment fails fast rather than at emit time.
        cranelift_native::builder().map_err(|e| CraneliftEmitError::IsaSetup(e.to_string()))?;
        Ok(Self)
    }
}

impl Codegen for CraneliftBackend {
    type Output = Vec<u8>;
    type Error = CraneliftEmitError;

    fn emit(&mut self, module: &Module) -> Result<Vec<u8>, CraneliftEmitError> {
        let isa_builder =
            cranelift_native::builder().map_err(|e| CraneliftEmitError::IsaSetup(e.to_string()))?;
        let mut flag_builder = settings::builder();
        // PIC is required for linking on Mach-O (and good practice
        // everywhere else): without it, the generated code uses
        // absolute relocations that modern linkers reject inside
        // text sections.
        flag_builder
            .set("is_pic", "true")
            .map_err(|e| CraneliftEmitError::IsaSetup(e.to_string()))?;
        let flags = settings::Flags::new(flag_builder);
        let isa = isa_builder
            .finish(flags)
            .map_err(|e| CraneliftEmitError::IsaSetup(e.to_string()))?;

        let builder = ObjectBuilder::new(
            isa,
            module.program_name.as_bytes().to_vec(),
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        let mut obj = ObjectModule::new(builder);

        let mut funcs: HashMap<String, FuncId> = HashMap::new();

        // Declare every function (extern + defined) so call sites can
        // resolve regardless of source order.
        for func in &module.functions {
            let sig = cranelift_signature(&mut obj, func);
            let linkage = if func.is_extern() {
                Linkage::Import
            } else {
                Linkage::Export
            };
            let id = obj
                .declare_function(&func.linkage_name, linkage, &sig)
                .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
            funcs.insert(func.linkage_name.clone(), id);
        }

        // Declare runtime RC + relation externs whenever the module
        // touches any relation-shaped instruction. The headings table
        // being non-empty is the trigger: an empty table means no
        // RelationLit ever interned a heading, so no rc/seal/write
        // symbol will be referenced from emitted code.
        if !module.headings.is_empty() {
            declare_runtime_rc_externs(&mut obj, &mut funcs)?;
        }

        // Per-module heading descriptors. One DataId block per unique
        // heading; each carries its attribute array and the
        // descriptor struct itself. Layouts are cached so the
        // instruction walk can look them up without recomputing.
        let mut layouts: Vec<RecordLayout> = Vec::with_capacity(module.headings.len());
        let mut heading_desc_ids: Vec<DataId> = Vec::with_capacity(module.headings.len());
        let ptr_bytes = obj.target_config().pointer_bytes() as usize;
        for (i, heading) in module.headings.iter().enumerate() {
            let layout = record_layout(heading);
            let desc_id = emit_heading_descriptor(&mut obj, HeadingId(i as u32), &layout, ptr_bytes)?;
            layouts.push(layout);
            heading_desc_ids.push(desc_id);
        }

        // Define every non-extern function.
        let mut next_data: u32 = 0;
        for func in module.functions.iter().filter(|f| !f.is_extern()) {
            let funcid = funcs[&func.linkage_name];
            emit_function(
                &mut obj,
                func,
                funcid,
                &funcs,
                &layouts,
                &heading_desc_ids,
                &mut next_data,
            )?;
        }

        let product = obj.finish();
        product
            .emit()
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))
    }
}

/// Declare the runtime RC + relation extern symbols. Mirrors the
/// LLVM backend's `emit_runtime_rc_externs`. Linkage is `Import`
/// since these resolve to the staticlib at link time.
fn declare_runtime_rc_externs(
    obj: &mut ObjectModule,
    funcs: &mut HashMap<String, FuncId>,
) -> Result<(), CraneliftEmitError> {
    let ptr_ty = obj.target_config().pointer_type();

    // coddl_rc_alloc(payload_size: i64, length: i32, kind: i32,
    //                desc: ptr) -> ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_rc_alloc", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_rc_alloc".into(), id);
    }
    // coddl_rc_retain(ptr) -> ()
    // coddl_rc_release(ptr) -> ()
    for name in ["coddl_rc_retain", "coddl_rc_release"] {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function(name, Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert(name.into(), id);
    }
    // coddl_relation_seal(ptr, ptr) -> ()
    // coddl_write_relation(ptr, ptr) -> ()
    for name in ["coddl_relation_seal", "coddl_write_relation"] {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function(name, Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert(name.into(), id);
    }
    // coddl_relation_where(src: ptr, desc: ptr, pred_fn: ptr) -> ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_relation_where", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_relation_where".into(), id);
    }
    Ok(())
}

/// Emit the per-heading descriptor data: per-attribute name bytes,
/// the attribute array, and the descriptor struct itself. Layout
/// matches `coddl_runtime::CoddlHeadingDesc` / `CoddlAttrDesc`.
///
/// CoddlAttrDesc bytes (24 on 64-bit, 16 on 32-bit):
///   ptr name            (ptr_bytes)
///   u32 name_len        (4)
///   u32 kind            (4)
///   u32 offset          (4)
///   u32 _pad            (4 on 64-bit for natural alignment)
/// CoddlHeadingDesc bytes (16 on 64-bit, 12 on 32-bit):
///   u32 attr_count      (4)
///   u32 record_size     (4)
///   ptr attrs           (ptr_bytes)
fn emit_heading_descriptor(
    obj: &mut ObjectModule,
    id: HeadingId,
    layout: &RecordLayout,
    ptr_bytes: usize,
) -> Result<DataId, CraneliftEmitError> {
    // Per-attribute name byte arrays.
    let mut name_ids: Vec<DataId> = Vec::with_capacity(layout.attrs.len());
    for (i, attr) in layout.attrs.iter().enumerate() {
        let sym = format!(".attrname.{}.{}", id.0, i);
        let nid = obj
            .declare_data(&sym, Linkage::Local, false, false)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        let mut dd = DataDescription::new();
        dd.define(attr.name.as_bytes().to_vec().into_boxed_slice());
        obj.define_data(nid, &dd)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        name_ids.push(nid);
    }

    // Attribute array. We compute element stride to match the host's
    // natural alignment for the struct (ptr_bytes-aligned).
    let attr_stride = if ptr_bytes == 8 { 24 } else { 16 };
    let attrs_sym = format!(".attrs.{}", id.0);
    let attrs_id = obj
        .declare_data(&attrs_sym, Linkage::Local, false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let attrs_bytes_len = attr_stride * layout.attrs.len();
    let mut attrs_dd = DataDescription::new();
    attrs_dd.define(vec![0u8; attrs_bytes_len].into_boxed_slice());
    // Relocate the name pointer field of each entry to its
    // corresponding name DataId.
    for (i, _attr) in layout.attrs.iter().enumerate() {
        let name_gv = obj.declare_data_in_data(name_ids[i], &mut attrs_dd);
        let offset_in_attrs = (i * attr_stride) as u32;
        attrs_dd.write_data_addr(offset_in_attrs, name_gv, 0);
        // Fill the u32 fields directly into the byte buffer.
        let name_bytes_len = layout.attrs[i].name.as_bytes().len() as u32;
        let kind = layout.attrs[i].kind;
        let off = layout.attrs[i].offset;
        // Offsets relative to attr base: ptr_bytes, ptr_bytes+4, ptr_bytes+8.
        attrs_write_u32(&mut attrs_dd, offset_in_attrs as usize + ptr_bytes, name_bytes_len);
        attrs_write_u32(&mut attrs_dd, offset_in_attrs as usize + ptr_bytes + 4, kind);
        attrs_write_u32(&mut attrs_dd, offset_in_attrs as usize + ptr_bytes + 8, off);
    }
    obj.define_data(attrs_id, &attrs_dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;

    // The descriptor struct itself.
    let desc_size = 8 + ptr_bytes; // u32 attr_count, u32 record_size, ptr attrs
    let desc_sym = format!(".heading.{}", id.0);
    let desc_id = obj
        .declare_data(&desc_sym, Linkage::Local, false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let mut desc_dd = DataDescription::new();
    desc_dd.define(vec![0u8; desc_size].into_boxed_slice());
    attrs_write_u32(&mut desc_dd, 0, layout.attrs.len() as u32);
    attrs_write_u32(&mut desc_dd, 4, layout.record_size);
    let attrs_gv = obj.declare_data_in_data(attrs_id, &mut desc_dd);
    desc_dd.write_data_addr(8, attrs_gv, 0);
    obj.define_data(desc_id, &desc_dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;

    Ok(desc_id)
}

/// Patch four bytes of `dd`'s data buffer with `val` (host-endian).
/// `DataDescription` exposes its initialized bytes as a mutable slice
/// via `init` after a `define`, but we mutate the slice directly via
/// `set_segment_section` semantics: easier path is to keep a local
/// vector and re-define, but that resets pointer relocations. Pull
/// off the slice via unsafe reach into private fields is not on the
/// table — instead we keep a parallel pre-computed bytes buffer that
/// we re-define. Caller uses this only before any relocation writes;
/// for the descriptor data above we order operations so relocations
/// happen after the u32 fills.
fn attrs_write_u32(dd: &mut DataDescription, offset: usize, val: u32) {
    if let cranelift_module::Init::Bytes { ref mut contents } = dd.init {
        let bytes = val.to_ne_bytes();
        contents[offset..offset + 4].copy_from_slice(&bytes);
    }
}

/// Build the Cranelift signature for a Coddl function. `main` is
/// special-cased to return `i32` (C convention) regardless of ProcIR's
/// `Unit` declaration.
fn cranelift_signature(
    obj: &mut ObjectModule,
    func: &Function,
) -> cranelift_codegen::ir::Signature {
    let mut sig = obj.make_signature();
    let ptr_ty = obj.target_config().pointer_type();
    for (_, pty) in &func.params {
        push_param_types(&mut sig.params, pty, ptr_ty);
    }
    if func.name == "main" {
        sig.returns.push(AbiParam::new(types::I32));
    } else {
        push_return_types(&mut sig.returns, &func.return_type, ptr_ty);
    }
    sig
}

/// Recursively flatten a `ProcType` into the Cranelift `AbiParam`
/// entries it occupies at an ABI boundary. Mirrors the LLVM backend:
/// Text/Binary expand to `(ptr, i64)`; Tuple expands per attribute in
/// canonical heading order, nested tuples recursively; empty Tuple
/// contributes zero entries.
fn push_param_types(
    out: &mut Vec<AbiParam>,
    ty: &ProcType,
    ptr_ty: cranelift_codegen::ir::Type,
) {
    match ty {
        ProcType::Text | ProcType::Binary => {
            out.push(AbiParam::new(ptr_ty));
            out.push(AbiParam::new(types::I64));
        }
        ProcType::Unit => {} // no value at the ABI level
        ProcType::Tuple(heading) => {
            for (_, attr_ty) in heading.attrs() {
                push_param_types(out, &proc_type_from_attr(attr_ty), ptr_ty);
            }
        }
        other => out.push(AbiParam::new(cranelift_value_type(other, ptr_ty))),
    }
}

fn push_return_types(
    out: &mut Vec<AbiParam>,
    ty: &ProcType,
    ptr_ty: cranelift_codegen::ir::Type,
) {
    match ty {
        ProcType::Unit => {}
        ProcType::Tuple(heading) if heading.is_empty() => {}
        other => out.push(AbiParam::new(cranelift_value_type(other, ptr_ty))),
    }
}

fn cranelift_value_type(
    ty: &ProcType,
    ptr_ty: cranelift_codegen::ir::Type,
) -> cranelift_codegen::ir::Type {
    match ty {
        ProcType::Integer => types::I64,
        ProcType::Rational => types::I64,
        ProcType::Approximate => types::F64,
        ProcType::Text | ProcType::Binary | ProcType::Pointer => ptr_ty,
        ProcType::Character => types::I32,
        ProcType::Byte => types::I8,
        ProcType::Boolean => types::I8,
        ProcType::Unit => types::I8, // unused; caller filters Unit out
        ProcType::Relation(_) => ptr_ty,
        ProcType::Tuple(_) => unreachable!(
            "Tuple ProcType must be flattened at ABI boundaries; bare Tuple seen in scalar context"
        ),
    }
}

/// Heading attributes carry surface `Type`s; backends reason in
/// `ProcType`. Centralized here so the codegen helpers stay in this
/// file.
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

#[derive(Debug, Clone)]
enum ValueRepr {
    Scalar(CrValue),
    Text {
        ptr: CrValue,
        len: CrValue,
    },
    /// Compile-time grouping over per-field `ValueRepr`s, in canonical
    /// heading order. Flattens recursively into leaf operands at ABI
    /// boundaries.
    Tuple {
        fields: Vec<(String, ValueRepr)>,
    },
}

impl ValueRepr {
    /// Append the leaf Cranelift values for this representation to a
    /// call-site argument vector. Scalars contribute one entry; Text
    /// two (ptr, len); Tuples flatten recursively.
    fn push_call_operands(&self, out: &mut Vec<CrValue>) {
        match self {
            ValueRepr::Scalar(v) => out.push(*v),
            ValueRepr::Text { ptr, len } => {
                out.push(*ptr);
                out.push(*len);
            }
            ValueRepr::Tuple { fields } => {
                for (_, f) in fields {
                    f.push_call_operands(out);
                }
            }
        }
    }
}

fn emit_function(
    obj: &mut ObjectModule,
    func: &Function,
    funcid: FuncId,
    funcs: &HashMap<String, FuncId>,
    heading_layouts: &[RecordLayout],
    heading_desc_ids: &[DataId],
    next_data: &mut u32,
) -> Result<(), CraneliftEmitError> {
    let mut ctx = obj.make_context();
    ctx.func.signature = cranelift_signature(obj, func);
    let mut fb_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);

    let is_main = func.name == "main";

    // One Cranelift block per ProcIR block. Pre-create so block-to-
    // block control flow (when it lands) has the targets it needs.
    let mut block_map: HashMap<coddl_procir::BlockId, cranelift_codegen::ir::Block> =
        HashMap::new();
    for procir_block in &func.blocks {
        let cl_block = builder.create_block();
        block_map.insert(procir_block.id, cl_block);
    }

    // Walk each ProcIR block.
    let mut values: HashMap<ValueId, ValueRepr> = HashMap::new();

    // Seed the entry block's params from the function signature. The
    // lowerer's convention is that the first N fresh ValueIds map
    // 1:1 to the declared params in source order. For Text params,
    // each contributes two consecutive ABI slots (ptr, len) which
    // combine into one `ValueRepr::Text`.
    if let Some(entry_proc_block) = func.blocks.first() {
        let entry_cl = block_map[&entry_proc_block.id];
        builder.append_block_params_for_function_params(entry_cl);
        let bps: Vec<CrValue> = builder.block_params(entry_cl).to_vec();
        let mut idx = 0usize;
        for (i, (_pname, pty)) in func.params.iter().enumerate() {
            let vid = ValueId(i as u32);
            match pty {
                ProcType::Text | ProcType::Binary => {
                    let ptr = bps[idx];
                    let len = bps[idx + 1];
                    values.insert(vid, ValueRepr::Text { ptr, len });
                    idx += 2;
                }
                ProcType::Tuple(_) => {
                    return Err(CraneliftEmitError::UnsupportedInst(
                        "Tuple-typed parameters not yet supported in defined functions".into(),
                    ));
                }
                _ => {
                    values.insert(vid, ValueRepr::Scalar(bps[idx]));
                    idx += 1;
                }
            }
        }
    }
    for procir_block in &func.blocks {
        let cl_block = block_map[&procir_block.id];
        builder.switch_to_block(cl_block);
        emit_block(
            obj,
            &mut builder,
            procir_block,
            funcs,
            heading_layouts,
            heading_desc_ids,
            &mut values,
            next_data,
            is_main,
        )?;
        builder.seal_block(cl_block);
    }

    builder.finalize();

    obj.define_function(funcid, &mut ctx)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    obj.clear_context(&mut ctx);
    Ok(())
}

fn emit_block(
    obj: &mut ObjectModule,
    builder: &mut FunctionBuilder<'_>,
    block: &BasicBlock,
    funcs: &HashMap<String, FuncId>,
    heading_layouts: &[RecordLayout],
    heading_desc_ids: &[DataId],
    values: &mut HashMap<ValueId, ValueRepr>,
    next_data: &mut u32,
    is_main: bool,
) -> Result<(), CraneliftEmitError> {
    for inst in &block.insts {
        emit_inst(
            obj,
            builder,
            inst,
            funcs,
            heading_layouts,
            heading_desc_ids,
            values,
            next_data,
        )?;
    }
    emit_terminator(builder, &block.terminator, values, is_main)?;
    Ok(())
}

fn emit_inst(
    obj: &mut ObjectModule,
    builder: &mut FunctionBuilder<'_>,
    inst: &Inst,
    funcs: &HashMap<String, FuncId>,
    heading_layouts: &[RecordLayout],
    heading_desc_ids: &[DataId],
    values: &mut HashMap<ValueId, ValueRepr>,
    next_data: &mut u32,
) -> Result<(), CraneliftEmitError> {
    match inst {
        Inst::Const {
            dst,
            value: Const::Text(bytes),
            ty: ProcType::Text,
        } => {
            let name = format!(".str.{}", *next_data);
            *next_data += 1;
            let data_id = obj
                .declare_data(&name, Linkage::Local, false, false)
                .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
            let mut data_desc = DataDescription::new();
            data_desc.define(bytes.clone().into_boxed_slice());
            obj.define_data(data_id, &data_desc)
                .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
            let local_data = obj.declare_data_in_func(data_id, builder.func);
            let ptr_ty = obj.target_config().pointer_type();
            let ptr = builder.ins().symbol_value(ptr_ty, local_data);
            let len = builder.ins().iconst(types::I64, bytes.len() as i64);
            values.insert(*dst, ValueRepr::Text { ptr, len });
            Ok(())
        }
        Inst::Const {
            dst,
            value: Const::Integer(n),
            ty: ProcType::Integer,
        } => {
            let v = builder.ins().iconst(types::I64, *n);
            values.insert(*dst, ValueRepr::Scalar(v));
            Ok(())
        }
        Inst::Const {
            dst,
            value: Const::Boolean(b),
            ty: ProcType::Boolean,
        } => {
            // Cranelift Booleans are I8; iconst is the simplest path.
            let v = builder.ins().iconst(types::I8, if *b { 1 } else { 0 });
            values.insert(*dst, ValueRepr::Scalar(v));
            Ok(())
        }
        Inst::Const { value, ty, .. } => Err(CraneliftEmitError::UnsupportedInst(format!(
            "Const {value:?} of type {ty:?}"
        ))),
        Inst::Call {
            dst,
            callee,
            args,
            return_type,
        } => {
            let callee_id = funcs.get(callee).copied().ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!("unresolved callee {callee}"))
            })?;
            let local_callee = obj.declare_func_in_func(callee_id, builder.func);

            let mut call_args: Vec<CrValue> = Vec::with_capacity(args.len() * 2);
            for arg in args {
                let repr = values.get(arg).cloned().ok_or_else(|| {
                    CraneliftEmitError::UnsupportedInst(format!("undefined value {arg:?}"))
                })?;
                repr.push_call_operands(&mut call_args);
            }

            let call = builder.ins().call(local_callee, &call_args);
            if let Some(dst) = dst {
                if !matches!(return_type, ProcType::Unit) {
                    let results = builder.inst_results(call);
                    if let Some(&v) = results.first() {
                        values.insert(*dst, ValueRepr::Scalar(v));
                    }
                }
            }
            Ok(())
        }
        Inst::TupleLit { dst, fields, .. } => {
            // Pure compile-time grouping — no Cranelift op emitted.
            let mut repr_fields: Vec<(String, ValueRepr)> = Vec::with_capacity(fields.len());
            for (name, v) in fields {
                let repr = values.get(v).cloned().ok_or_else(|| {
                    CraneliftEmitError::UnsupportedInst(format!(
                        "undefined tuple field value {v:?}"
                    ))
                })?;
                repr_fields.push((name.clone(), repr));
            }
            values.insert(*dst, ValueRepr::Tuple { fields: repr_fields });
            Ok(())
        }
        Inst::TupleField {
            dst,
            src,
            field_name,
            ..
        } => {
            // Pure compile-time projection.
            let src_repr = values.get(src).cloned().ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!("undefined tuple source {src:?}"))
            })?;
            let field_repr = match src_repr {
                ValueRepr::Tuple { fields } => fields
                    .into_iter()
                    .find(|(n, _)| n == field_name)
                    .map(|(_, r)| r)
                    .ok_or_else(|| {
                        CraneliftEmitError::UnsupportedInst(format!(
                            "tuple {src:?} has no field `{field_name}`"
                        ))
                    })?,
                other => {
                    return Err(CraneliftEmitError::UnsupportedInst(format!(
                        "field access on non-tuple value: {other:?}"
                    )));
                }
            };
            values.insert(*dst, field_repr);
            Ok(())
        }
        Inst::RelationLit {
            dst,
            tuples,
            heading_id,
        } => {
            let _ = next_data; // descriptors are pre-emitted
            let layout = heading_layouts.get(heading_id.0 as usize).ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!(
                    "unknown heading_id {} in RelationLit",
                    heading_id.0
                ))
            })?;
            let desc_id = heading_desc_ids[heading_id.0 as usize];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let ptr_ty = obj.target_config().pointer_type();
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);

            // call coddl_rc_alloc(payload_size, count, kind=0, desc).
            let alloc_id = funcs["coddl_rc_alloc"];
            let alloc_local = obj.declare_func_in_func(alloc_id, builder.func);
            let count = tuples.len() as i64;
            let payload_size = (layout.record_size as i64) * count;
            let size_val = builder.ins().iconst(types::I64, payload_size);
            let count_val = builder.ins().iconst(types::I32, count);
            let kind_val = builder.ins().iconst(types::I32, 0); // CoddlKind::Relation
            let call = builder.ins().call(
                alloc_local,
                &[size_val, count_val, kind_val, desc_val],
            );
            let payload = builder.inst_results(call)[0];

            // Store each tuple into its record slot.
            for (record_idx, tuple_vid) in tuples.iter().enumerate() {
                let tuple_repr = values.get(tuple_vid).cloned().ok_or_else(|| {
                    CraneliftEmitError::UnsupportedInst(format!(
                        "undefined tuple value {tuple_vid:?} in RelationLit"
                    ))
                })?;
                let fields = match &tuple_repr {
                    ValueRepr::Tuple { fields } => fields.clone(),
                    other => {
                        return Err(CraneliftEmitError::UnsupportedInst(format!(
                            "RelationLit operand is not a Tuple: {other:?}"
                        )));
                    }
                };
                for attr in &layout.attrs {
                    let field_repr = fields
                        .iter()
                        .find(|(n, _)| n == &attr.name)
                        .map(|(_, r)| r.clone())
                        .ok_or_else(|| {
                            CraneliftEmitError::UnsupportedInst(format!(
                                "tuple missing attribute `{}` for relation layout",
                                attr.name
                            ))
                        })?;
                    let byte_offset =
                        record_idx as i32 * layout.record_size as i32 + attr.offset as i32;
                    store_attr(builder, payload, byte_offset, &field_repr)?;
                }
            }

            // call coddl_relation_seal(payload, desc).
            let seal_id = funcs["coddl_relation_seal"];
            let seal_local = obj.declare_func_in_func(seal_id, builder.func);
            builder.ins().call(seal_local, &[payload, desc_val]);

            values.insert(*dst, ValueRepr::Scalar(payload));
            Ok(())
        }
        Inst::Retain { src } => {
            let v = scalar_value(values, src)?;
            let id = funcs["coddl_rc_retain"];
            let local = obj.declare_func_in_func(id, builder.func);
            builder.ins().call(local, &[v]);
            Ok(())
        }
        Inst::Release { src } => {
            let v = scalar_value(values, src)?;
            let id = funcs["coddl_rc_release"];
            let local = obj.declare_func_in_func(id, builder.func);
            builder.ins().call(local, &[v]);
            Ok(())
        }
        Inst::WriteRelation { rel, heading_id } => {
            let v = scalar_value(values, rel)?;
            let desc_id = heading_desc_ids[heading_id.0 as usize];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let ptr_ty = obj.target_config().pointer_type();
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let id = funcs["coddl_write_relation"];
            let local = obj.declare_func_in_func(id, builder.func);
            builder.ins().call(local, &[v, desc_val]);
            Ok(())
        }
        Inst::ScalarOp {
            dst,
            op,
            operand_type,
            lhs,
            rhs,
        } => {
            let lhs_v = scalar_value(values, lhs)?;
            let rhs_v = scalar_value(values, rhs)?;
            let result = match op {
                ScalarOp::And => builder.ins().band(lhs_v, rhs_v),
                ScalarOp::Or => builder.ins().bor(lhs_v, rhs_v),
                _ => {
                    use cranelift_codegen::ir::condcodes::IntCC;
                    let cc = match op {
                        ScalarOp::Eq => IntCC::Equal,
                        ScalarOp::NotEq => IntCC::NotEqual,
                        ScalarOp::Lt => IntCC::SignedLessThan,
                        ScalarOp::Gt => IntCC::SignedGreaterThan,
                        ScalarOp::LtEq => IntCC::SignedLessThanOrEqual,
                        ScalarOp::GtEq => IntCC::SignedGreaterThanOrEqual,
                        _ => unreachable!(),
                    };
                    let cmp = builder.ins().icmp(cc, lhs_v, rhs_v);
                    // Cranelift `icmp` returns an I8 already on the
                    // boolean lane; ensure it matches the
                    // `cranelift_value_type(Boolean) = I8`
                    // expectation by avoiding unnecessary
                    // conversions. (As of Cranelift's current API the
                    // result is already i8-equivalent.)
                    let _ = operand_type; // kept for backend symmetry
                    cmp
                }
            };
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::AttrLoad {
            dst,
            src,
            offset,
            attr_type,
        } => {
            let src_v = scalar_value(values, src)?;
            let flags = MemFlags::trusted();
            match attr_type {
                ProcType::Integer => {
                    let v =
                        builder.ins().load(types::I64, flags, src_v, *offset as i32);
                    values.insert(*dst, ValueRepr::Scalar(v));
                    Ok(())
                }
                ProcType::Boolean => {
                    // Record cell stores Boolean as 8 bytes; reduce to
                    // the I8 boolean SSA repr.
                    let raw =
                        builder.ins().load(types::I64, flags, src_v, *offset as i32);
                    let v = builder.ins().ireduce(types::I8, raw);
                    values.insert(*dst, ValueRepr::Scalar(v));
                    Ok(())
                }
                ProcType::Text => {
                    let ptr_ty = obj.target_config().pointer_type();
                    let ptr = builder.ins().load(ptr_ty, flags, src_v, *offset as i32);
                    let len = builder
                        .ins()
                        .load(types::I64, flags, src_v, *offset as i32 + 8);
                    values.insert(*dst, ValueRepr::Text { ptr, len });
                    Ok(())
                }
                other => Err(CraneliftEmitError::UnsupportedInst(format!(
                    "AttrLoad of type {other:?} not yet supported"
                ))),
            }
        }
        Inst::Where {
            dst,
            src,
            predicate_linkage,
            heading_id,
        } => {
            let src_v = scalar_value(values, src)?;
            let desc_id = heading_desc_ids[heading_id.0 as usize];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let ptr_ty = obj.target_config().pointer_type();
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let pred_id = *funcs.get(predicate_linkage).ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!(
                    "unresolved predicate {predicate_linkage}"
                ))
            })?;
            let pred_ref = obj.declare_func_in_func(pred_id, builder.func);
            let pred_addr = builder.ins().func_addr(ptr_ty, pred_ref);
            let where_id = funcs["coddl_relation_where"];
            let where_local = obj.declare_func_in_func(where_id, builder.func);
            let call =
                builder
                    .ins()
                    .call(where_local, &[src_v, desc_val, pred_addr]);
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
    }
}

/// Extract a `Scalar(v)` from the values map for an RC-managed
/// pointer. Used by Retain / Release / WriteRelation.
fn scalar_value(
    values: &HashMap<ValueId, ValueRepr>,
    v: &ValueId,
) -> Result<CrValue, CraneliftEmitError> {
    match values.get(v) {
        Some(ValueRepr::Scalar(val)) => Ok(*val),
        _ => Err(CraneliftEmitError::UnsupportedInst(format!(
            "expected scalar pointer at {v:?}"
        ))),
    }
}

/// Store one attribute's flattened operands into the relation's
/// payload at `byte_offset` (relative to `payload`'s base).
fn store_attr(
    builder: &mut FunctionBuilder<'_>,
    payload: CrValue,
    byte_offset: i32,
    repr: &ValueRepr,
) -> Result<(), CraneliftEmitError> {
    let flags = MemFlags::trusted();
    match repr {
        ValueRepr::Scalar(v) => {
            // Phase 19 supports Integer / Boolean (both 8-byte) as
            // relation cells. Future widths land here.
            builder.ins().store(flags, *v, payload, byte_offset);
            Ok(())
        }
        ValueRepr::Text { ptr, len } => {
            builder.ins().store(flags, *ptr, payload, byte_offset);
            builder.ins().store(flags, *len, payload, byte_offset + 8);
            Ok(())
        }
        ValueRepr::Tuple { .. } => Err(CraneliftEmitError::UnsupportedInst(
            "nested Tuple cells not yet supported in relation records".into(),
        )),
    }
}

fn emit_terminator(
    builder: &mut FunctionBuilder<'_>,
    term: &Terminator,
    values: &HashMap<ValueId, ValueRepr>,
    is_main: bool,
) -> Result<(), CraneliftEmitError> {
    match term {
        Terminator::Return(None) if is_main => {
            let zero = builder.ins().iconst(types::I32, 0);
            builder.ins().return_(&[zero]);
        }
        Terminator::Return(None) => {
            builder.ins().return_(&[]);
        }
        Terminator::Return(Some(v)) => match values.get(v) {
            Some(ValueRepr::Scalar(val)) => {
                builder.ins().return_(&[*val]);
            }
            Some(ValueRepr::Tuple { fields }) if fields.is_empty() => {
                builder.ins().return_(&[]);
            }
            Some(ValueRepr::Text { .. })
            | Some(ValueRepr::Tuple { .. })
            | None => {
                return Err(CraneliftEmitError::UnsupportedInst(format!(
                    "returning {v:?} unsupported"
                )));
            }
        },
        Terminator::Unreachable => {
            builder
                .ins()
                .trap(cranelift_codegen::ir::TrapCode::user(1).unwrap());
        }
    }
    Ok(())
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

    fn emit_ok(src: &str) -> Vec<u8> {
        let out = lower(src, FileId(0));
        let module = out.module.expect("typechecked");
        let mut backend = CraneliftBackend::new().expect("ISA setup");
        backend.emit(&module).expect("emit ok")
    }

    #[test]
    fn hello_world_object_has_main_symbol() {
        use object::{Object, ObjectSymbol};
        let bytes = emit_ok(HELLO_WORLD);
        let obj = object::File::parse(&*bytes).expect("parse object");
        let names: Vec<String> = obj
            .symbols()
            .filter_map(|s| s.name().ok().map(|n| n.to_string()))
            .collect();
        // Mach-O prefixes user symbols with `_`; ELF doesn't. Accept
        // both.
        assert!(
            names.iter().any(|n| n == "main" || n == "_main"),
            "no main symbol in {names:?}"
        );
    }

    #[test]
    fn hello_world_object_imports_coddl_write_line() {
        use object::{Object, ObjectSymbol};
        let bytes = emit_ok(HELLO_WORLD);
        let obj = object::File::parse(&*bytes).expect("parse object");
        let imports: Vec<String> = obj
            .symbols()
            .filter(|s| s.is_undefined())
            .filter_map(|s| s.name().ok().map(|n| n.to_string()))
            .collect();
        assert!(
            imports
                .iter()
                .any(|n| n == "coddl_write_line" || n == "_coddl_write_line"),
            "coddl_write_line not imported; imports: {imports:?}"
        );
    }

    #[test]
    fn hello_world_object_has_text_data() {
        // The literal bytes "Hello, world!" should appear somewhere
        // in the produced object. Cranelift puts them in a read-only
        // data section (`.rodata` on ELF, `__const` on Mach-O).
        let bytes = emit_ok(HELLO_WORLD);
        let needle = b"Hello, world!";
        let found = bytes.windows(needle.len()).any(|w| w == needle);
        assert!(found, "string bytes not present in object");
    }

    #[test]
    fn proctype_to_cranelift_type_covers_built_in_scalars() {
        let ptr_ty = types::I64;
        // Exhaustive match guards against silent ProcType additions.
        for ty in [
            ProcType::Integer,
            ProcType::Rational,
            ProcType::Approximate,
            ProcType::Text,
            ProcType::Character,
            ProcType::Binary,
            ProcType::Byte,
            ProcType::Boolean,
            ProcType::Unit,
            ProcType::Pointer,
        ] {
            let _ = cranelift_value_type(&ty, ptr_ty);
        }
    }

    // ── Tuple ABI flattening (Phase 18) ──────────────────────────────

    #[test]
    fn tuple_let_object_imports_coddl_write_line() {
        // A tuple-let program still imports coddl_write_line; the
        // tuple machinery should not affect symbol resolution.
        use object::{Object, ObjectSymbol};
        let src = "oper main {} [ \
                   let t = {message: \"hi\"}; \
                   write_line{message: t.message}; \
                   ];";
        let bytes = emit_ok(src);
        let obj = object::File::parse(&*bytes).expect("parse object");
        let imports: Vec<String> = obj
            .symbols()
            .filter(|s| s.is_undefined())
            .filter_map(|s| s.name().ok().map(|n| n.to_string()))
            .collect();
        assert!(
            imports
                .iter()
                .any(|n| n == "coddl_write_line" || n == "_coddl_write_line"),
            "coddl_write_line not imported; imports: {imports:?}"
        );
    }

    #[test]
    fn empty_tuple_param_decl_contributes_zero_operands() {
        let mut params = Vec::new();
        push_param_types(
            &mut params,
            &ProcType::Tuple(coddl_procir::Heading::empty()),
            types::I64,
        );
        assert!(params.is_empty());
    }
}

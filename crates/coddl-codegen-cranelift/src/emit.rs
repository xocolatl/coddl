//! ProcIR → Cranelift object emission.
//!
//! Walks a `Module` and produces a native object file as `Vec<u8>`. Uses
//! `cranelift-native` for the host ISA and `cranelift-object` for the
//! object writer. `Text` values are decomposed at the C-call boundary
//! into a `(ptr, i64)` pair — same ABI as the LLVM backend.
//!
//! See `docs/codegen.md` for the spec.

use std::collections::HashMap;

use cranelift_codegen::ir::{types, AbiParam, InstBuilder, Value as CrValue};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, FuncId, Linkage, Module as ClModule};
use cranelift_object::{ObjectBuilder, ObjectModule};

use coddl_procir::{
    BasicBlock, Codegen, Const, Function, Inst, Module, ProcType, Terminator, ValueId,
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

        // Define every non-extern function.
        let mut next_data: u32 = 0;
        for func in module.functions.iter().filter(|f| !f.is_extern()) {
            let funcid = funcs[&func.linkage_name];
            emit_function(&mut obj, func, funcid, &funcs, &mut next_data)?;
        }

        let product = obj.finish();
        product
            .emit()
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))
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
        push_param_types(&mut sig.params, *pty, ptr_ty);
    }
    if func.name == "main" {
        sig.returns.push(AbiParam::new(types::I32));
    } else {
        push_return_types(&mut sig.returns, func.return_type, ptr_ty);
    }
    sig
}

fn push_param_types(out: &mut Vec<AbiParam>, ty: ProcType, ptr_ty: cranelift_codegen::ir::Type) {
    match ty {
        ProcType::Text | ProcType::Binary => {
            out.push(AbiParam::new(ptr_ty));
            out.push(AbiParam::new(types::I64));
        }
        ProcType::Unit => {} // no value at the ABI level
        other => out.push(AbiParam::new(cranelift_value_type(other, ptr_ty))),
    }
}

fn push_return_types(out: &mut Vec<AbiParam>, ty: ProcType, ptr_ty: cranelift_codegen::ir::Type) {
    match ty {
        ProcType::Unit => {}
        other => out.push(AbiParam::new(cranelift_value_type(other, ptr_ty))),
    }
}

fn cranelift_value_type(
    ty: ProcType,
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
    }
}

#[derive(Debug, Clone, Copy)]
enum ValueRepr {
    Scalar(CrValue),
    Text { ptr: CrValue, len: CrValue },
}

fn emit_function(
    obj: &mut ObjectModule,
    func: &Function,
    funcid: FuncId,
    funcs: &HashMap<String, FuncId>,
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
    for procir_block in &func.blocks {
        let cl_block = block_map[&procir_block.id];
        builder.switch_to_block(cl_block);
        emit_block(
            obj,
            &mut builder,
            procir_block,
            funcs,
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
    values: &mut HashMap<ValueId, ValueRepr>,
    next_data: &mut u32,
    is_main: bool,
) -> Result<(), CraneliftEmitError> {
    for inst in &block.insts {
        emit_inst(obj, builder, inst, funcs, values, next_data)?;
    }
    emit_terminator(builder, &block.terminator, values, is_main)?;
    Ok(())
}

fn emit_inst(
    obj: &mut ObjectModule,
    builder: &mut FunctionBuilder<'_>,
    inst: &Inst,
    funcs: &HashMap<String, FuncId>,
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
                let repr = values.get(arg).copied().ok_or_else(|| {
                    CraneliftEmitError::UnsupportedInst(format!("undefined value {arg:?}"))
                })?;
                match repr {
                    ValueRepr::Scalar(v) => call_args.push(v),
                    ValueRepr::Text { ptr, len } => {
                        call_args.push(ptr);
                        call_args.push(len);
                    }
                }
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
            Some(ValueRepr::Text { .. }) | None => {
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
            let _ = cranelift_value_type(ty, ptr_ty);
        }
    }
}

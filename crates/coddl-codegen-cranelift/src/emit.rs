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
    record_layout, BasicBlock, Codegen, Const, Function, HeadingId, Inst, Module, PlanEntry,
    ProcType, RecordLayout, ScalarOp, Terminator, Type, ValueId,
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
        // Scalar `Text` concatenation (`||`) externs are needed independent of
        // any relation machinery — a concat-only program touches no headings.
        declare_scalar_text_externs(&mut obj, &mut funcs)?;
        if !module.public_relvars.is_empty() {
            declare_runtime_relvar_externs(&mut obj, &mut funcs)?;
        }
        if !module.plans.is_empty() {
            declare_runtime_plan_externs(&mut obj, &mut funcs)?;
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

        // Per-public-relvar data: slot global + name / env / path /
        // table / column strings + pointer-and-length arrays. Each
        // RelvarSlotInit / RelvarRead / RelvarSlotRelease looks up by
        // surface name.
        let mut relvar_data: HashMap<String, RelvarDataIds> = HashMap::new();
        let db_name = module.db_name.clone().unwrap_or_default();
        let db_default = module.db_path_default.clone().unwrap_or_default();
        for relvar in &module.public_relvars {
            let ids = emit_public_relvar_data(&mut obj, relvar, &db_name, &db_default, ptr_bytes)?;
            relvar_data.insert(relvar.name.clone(), ids);
        }

        // Per-private-relvar writable slot (in-memory; filled by assignment,
        // empty-initialized in `main`'s prologue). Just the slot global — no
        // SQL strings.
        let mut private_relvar_slots: HashMap<String, DataId> = HashMap::new();
        for (name, _heading_id) in &module.private_relvar_slots {
            let slot = obj
                .declare_data(&format!("{name}_slot"), Linkage::Local, true, false)
                .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
            let mut slot_dd = DataDescription::new();
            slot_dd.set_align(ptr_bytes as u64);
            slot_dd.define(vec![0u8; ptr_bytes].into_boxed_slice());
            obj.define_data(slot, &slot_dd)
                .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
            private_relvar_slots.insert(name.clone(), slot);
        }

        // Per-plan + database data symbols for the pushdown prologue.
        let db_data: Option<DbDataIds> = if module.plans.is_empty() {
            None
        } else {
            Some(emit_db_data(&mut obj, &db_name, &db_default)?)
        };
        let mut plan_data: HashMap<u32, PlanDataIds> = HashMap::new();
        for p in &module.plans {
            plan_data.insert(p.plan_id, emit_plan_data(&mut obj, p)?);
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
                &relvar_data,
                &private_relvar_slots,
                &plan_data,
                db_data.as_ref(),
                &mut next_data,
            )?;
        }

        let product = obj.finish();
        product
            .emit()
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))
    }
}

/// Declare the scalar `Text` concatenation externs (`||` and the
/// `Character`→`Text` normalization). Needed independent of any relation
/// machinery, so declared unconditionally. Mirrors the LLVM backend's
/// `emit_scalar_text_externs`.
fn declare_scalar_text_externs(
    obj: &mut ObjectModule,
    funcs: &mut HashMap<String, FuncId>,
) -> Result<(), CraneliftEmitError> {
    let ptr_ty = obj.target_config().pointer_type();
    // coddl_text_concat(a_ptr, a_len, b_ptr, b_len) -> payload ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_text_concat", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_text_concat".into(), id);
    }
    // coddl_char_to_text(codepoint: i32) -> payload ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_char_to_text", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_char_to_text".into(), id);
    }
    // coddl_utf8_len(codepoint: i32) -> i64
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        sig.returns.push(AbiParam::new(types::I64));
        let id = obj
            .declare_function("coddl_utf8_len", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_utf8_len".into(), id);
    }
    Ok(())
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
    // coddl_relation_project(src: ptr, src_desc: ptr, result_desc: ptr) -> ptr
    // coddl_relation_restructure(src: ptr, src_desc: ptr, result_desc: ptr) -> ptr
    for name in [
        "coddl_relation_where",
        "coddl_relation_project",
        "coddl_relation_restructure",
    ] {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function(name, Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert(name.into(), id);
    }
    // coddl_relation_rename(src, src_desc, result_desc, perm: ptr, perm_count: usize) -> ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty)); // usize perm_count
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_relation_rename", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_relation_rename".into(), id);
    }
    // coddl_extract_check_cardinality(src: ptr, desc: ptr) -> ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_extract_check_cardinality", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_extract_check_cardinality".into(), id);
    }
    // coddl_text_eq(a_ptr: ptr, a_len: i64, b_ptr: ptr, b_len: i64) -> i8
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I8));
        let id = obj
            .declare_function("coddl_text_eq", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_text_eq".into(), id);
    }
    // coddl_relation_extend(src, src_desc, result_desc, helper_fn) -> ptr
    {
        let mut sig = obj.make_signature();
        for _ in 0..4 {
            sig.params.push(AbiParam::new(ptr_ty));
        }
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_relation_extend", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_relation_extend".into(), id);
    }
    // coddl_relation_join(lhs, lhs_desc, rhs, rhs_desc, result_desc) -> ptr
    {
        let mut sig = obj.make_signature();
        for _ in 0..5 {
            sig.params.push(AbiParam::new(ptr_ty));
        }
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_relation_join", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_relation_join".into(), id);
    }
    // coddl_relation_union(lhs, rhs, desc) -> ptr. Identical headings ⇒ one desc.
    {
        let mut sig = obj.make_signature();
        for _ in 0..3 {
            sig.params.push(AbiParam::new(ptr_ty));
        }
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_relation_union", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_relation_union".into(), id);
    }
    // coddl_relation_minus(lhs, rhs, desc) -> ptr. Identical headings ⇒ one desc.
    {
        let mut sig = obj.make_signature();
        for _ in 0..3 {
            sig.params.push(AbiParam::new(ptr_ty));
        }
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_relation_minus", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_relation_minus".into(), id);
    }
    // coddl_relation_tclose(src, desc) -> ptr. Result heading == operand
    // heading ⇒ one desc.
    {
        let mut sig = obj.make_signature();
        for _ in 0..2 {
            sig.params.push(AbiParam::new(ptr_ty));
        }
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_relation_tclose", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_relation_tclose".into(), id);
    }
    // Private-relvar in-memory slots:
    //   coddl_relvar_slot_init_empty(desc: ptr, slot: ptr) -> ()
    //   coddl_relvar_slot_store(value: ptr, slot: ptr) -> ()
    for name in ["coddl_relvar_slot_init_empty", "coddl_relvar_slot_store"] {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function(name, Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert(name.into(), id);
    }
    Ok(())
}

/// Phase 22 runtime externs: SQLite materialization, transaction
/// brackets, and the env-var resolver. Mirrors the LLVM backend's
/// `emit_runtime_relvar_externs`.
fn declare_runtime_relvar_externs(
    obj: &mut ObjectModule,
    funcs: &mut HashMap<String, FuncId>,
) -> Result<(), CraneliftEmitError> {
    let ptr_ty = obj.target_config().pointer_type();
    // coddl_sqlite_relvar_init(
    //   relvar_name: ptr, relvar_name_len: i64,
    //   db_path: ptr, db_path_len: i64,
    //   table: ptr, table_len: i64,
    //   columns: ptr, column_lens: ptr, column_count: i32,
    //   desc: ptr, slot: ptr) -> i32
    {
        let mut sig = obj.make_signature();
        for _ in 0..3 {
            sig.params.push(AbiParam::new(ptr_ty));
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(types::I32));
        let id = obj
            .declare_function("coddl_sqlite_relvar_init", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_sqlite_relvar_init".into(), id);
    }
    // Transaction externs (begin/commit/rollback) are not declared
    // here. The lowerer registers them through its `ensure_runtime_extern`
    // path so they go through the same Inst::Call → Function-table
    // route as `coddl_runtime_init` / `coddl_runtime_shutdown`. The
    // Function table records their signature as `() -> Integer` —
    // wider than the runtime's actual `CoddlStatus` (i32) but harmless
    // because nothing reads the result. Pre-declaring them here too
    // would conflict with the lowerer's declaration.
    // coddl_resolve_op_field(env_name: ptr, env_len: i64,
    //                       default: ptr, default_len: i64,
    //                       out_len: ptr) -> ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_resolve_op_field", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_resolve_op_field".into(), id);
    }
    Ok(())
}

/// SQL-pushdown runtime externs: database/plan registration (program
/// prologue), `coddl_query` (the lazy read force point), and `coddl_exec` (the
/// DML force point). Mirrors the LLVM backend's `emit_runtime_plan_externs`.
fn declare_runtime_plan_externs(
    obj: &mut ObjectModule,
    funcs: &mut HashMap<String, FuncId>,
) -> Result<(), CraneliftEmitError> {
    let ptr_ty = obj.target_config().pointer_type();
    // coddl_register_database(name: ptr, name_len: i64, path: ptr,
    //                         path_len: i64) -> i32
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));
        let id = obj
            .declare_function("coddl_register_database", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_register_database".into(), id);
    }
    // coddl_register_plan(plan_id: i32, db_name: ptr, db_name_len: i64,
    //                     sql: ptr, sql_len: i64, param_count: i32,
    //                     desc: ptr) -> i32
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(types::I32));
        let id = obj
            .declare_function("coddl_register_plan", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_register_plan".into(), id);
    }
    // coddl_query(plan_id: i32, params: ptr, n: i64) -> ptr
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(ptr_ty));
        let id = obj
            .declare_function("coddl_query", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_query".into(), id);
    }
    // coddl_exec(plan_id: i32, params: ptr, n: i64) -> i32 (CoddlStatus)
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));
        let id = obj
            .declare_function("coddl_exec", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_exec".into(), id);
    }
    // coddl_exec_insert(plan_id: i32, src: ptr, desc: ptr) -> i32 (CoddlStatus)
    {
        let mut sig = obj.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.params.push(AbiParam::new(ptr_ty));
        sig.returns.push(AbiParam::new(types::I32));
        let id = obj
            .declare_function("coddl_exec_insert", Linkage::Import, &sig)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        funcs.insert("coddl_exec_insert".into(), id);
    }
    Ok(())
}

/// Database-level data symbols shared by every plan: the database name,
/// its `CODDL_<DB>_FILE` env-var key, and the baked default path.
struct DbDataIds {
    name: DataId,
    name_len: i64,
    env_name: DataId,
    env_name_len: i64,
    default_path: DataId,
    default_path_len: i64,
}

/// Per-plan data symbols: the SQL text and the logical database name.
struct PlanDataIds {
    db_name: DataId,
    db_name_len: i64,
    sql: DataId,
    sql_len: i64,
    param_count: i32,
    /// Index into the module's `heading_desc_ids` for the result heading.
    result_heading_id: usize,
}

/// Emit the database-level byte constants for `RegisterDatabase`.
fn emit_db_data(
    obj: &mut ObjectModule,
    db_name: &str,
    db_default: &str,
) -> Result<DbDataIds, CraneliftEmitError> {
    let env_name = format!("CODDL_{}_FILE", db_name.to_ascii_uppercase());
    let name = declare_byte_constant(obj, ".db.name", db_name.as_bytes())?;
    let env = declare_byte_constant(obj, ".db.env_name", env_name.as_bytes())?;
    let default = declare_byte_constant(obj, ".db.default_path", db_default.as_bytes())?;
    Ok(DbDataIds {
        name,
        name_len: db_name.len() as i64,
        env_name: env,
        env_name_len: env_name.len() as i64,
        default_path: default,
        default_path_len: db_default.len() as i64,
    })
}

/// Emit one plan's SQL + db-name byte constants.
fn emit_plan_data(
    obj: &mut ObjectModule,
    p: &PlanEntry,
) -> Result<PlanDataIds, CraneliftEmitError> {
    let sql = declare_byte_constant(obj, &format!(".plan.{}.sql", p.plan_id), p.sql.as_bytes())?;
    let dbn =
        declare_byte_constant(obj, &format!(".plan.{}.db_name", p.plan_id), p.db_name.as_bytes())?;
    Ok(PlanDataIds {
        db_name: dbn,
        db_name_len: p.db_name.len() as i64,
        sql,
        sql_len: p.sql.len() as i64,
        param_count: p.param_count as i32,
        result_heading_id: p.result_heading_id.0 as usize,
    })
}

/// Per-public-relvar DataIds and string lengths. The three new Inst
/// arms (`RelvarSlotInit`, `RelvarSlotRelease`, `RelvarRead`) look up
/// by surface name and reach these symbols via
/// `ObjectModule::declare_data_in_func`.
struct RelvarDataIds {
    slot: DataId,
    relvar_name: DataId,
    relvar_name_len: i64,
    env_name: DataId,
    env_name_len: i64,
    default_path: DataId,
    default_path_len: i64,
    table_name: DataId,
    table_name_len: i64,
    col_ptrs: DataId,
    col_lens: DataId,
    col_count: i32,
}

/// Emit every data symbol one public relvar needs:
/// - `<name>_slot` — writable pointer slot (initialized null).
/// - `<name>.relvar_name` / `<name>.env_name` / `<name>.default_path`
///   / `<name>.table_name` — UTF-8 byte arrays.
/// - `<name>.col<i>.name` — per-column UTF-8 byte arrays.
/// - `<name>.col_ptrs` — pointer-per-column array; each pointer
///   relocates to its respective name.
/// - `<name>.col_lens` — i64-per-column length array.
fn emit_public_relvar_data(
    obj: &mut ObjectModule,
    relvar: &coddl_procir::PublicRelvarBinding,
    db_name: &str,
    db_default: &str,
    ptr_bytes: usize,
) -> Result<RelvarDataIds, CraneliftEmitError> {
    let name = &relvar.name;
    let env_name = format!("CODDL_{}_FILE", db_name.to_ascii_uppercase());

    // Writable slot — pointer-sized, zero-initialized. `Linkage::Local`
    // + `writable=true`. Aligned to the pointer size so the runtime's
    // `*mut *mut u8` write into it is well-defined.
    let slot = obj
        .declare_data(&format!("{name}_slot"), Linkage::Local, true, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let mut slot_dd = DataDescription::new();
    slot_dd.set_align(ptr_bytes as u64);
    slot_dd.define(vec![0u8; ptr_bytes].into_boxed_slice());
    obj.define_data(slot, &slot_dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;

    let relvar_name_bytes = name.as_bytes();
    let relvar_name = declare_byte_constant(obj, &format!("{name}.relvar_name"), relvar_name_bytes)?;
    let env_name_bytes = env_name.as_bytes();
    let env_id = declare_byte_constant(obj, &format!("{name}.env_name"), env_name_bytes)?;
    let default_bytes = db_default.as_bytes();
    let default_id = declare_byte_constant(obj, &format!("{name}.default_path"), default_bytes)?;
    let table_bytes = relvar.table_name.as_bytes();
    let table_id = declare_byte_constant(obj, &format!("{name}.table_name"), table_bytes)?;

    // Per-column name byte arrays.
    let mut col_ids: Vec<DataId> = Vec::with_capacity(relvar.columns.len());
    for (i, (_, col)) in relvar.columns.iter().enumerate() {
        let id = declare_byte_constant(obj, &format!("{name}.col{i}.name"), col.as_bytes())?;
        col_ids.push(id);
    }
    // Pointer-per-column array. Aligned to ptr_bytes so the runtime's
    // `*const *const u8` indexed loads succeed.
    let col_ptrs = obj
        .declare_data(&format!("{name}.col_ptrs"), Linkage::Local, false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let ptrs_bytes_len = ptr_bytes * relvar.columns.len();
    let mut ptrs_dd = DataDescription::new();
    ptrs_dd.set_align(ptr_bytes as u64);
    ptrs_dd.define(vec![0u8; ptrs_bytes_len].into_boxed_slice());
    for (i, &cid) in col_ids.iter().enumerate() {
        let gv = obj.declare_data_in_data(cid, &mut ptrs_dd);
        ptrs_dd.write_data_addr((i * ptr_bytes) as u32, gv, 0);
    }
    obj.define_data(col_ptrs, &ptrs_dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;

    // i64-per-column length array. Aligned to 8 so the runtime's
    // `*const usize` indexed loads succeed.
    let col_lens = obj
        .declare_data(&format!("{name}.col_lens"), Linkage::Local, false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let lens_bytes_len = 8 * relvar.columns.len();
    let mut lens_dd = DataDescription::new();
    lens_dd.set_align(8);
    let mut lens_bytes = vec![0u8; lens_bytes_len];
    for (i, (_, col)) in relvar.columns.iter().enumerate() {
        let n = col.as_bytes().len() as u64;
        let off = i * 8;
        lens_bytes[off..off + 8].copy_from_slice(&n.to_ne_bytes());
    }
    lens_dd.define(lens_bytes.into_boxed_slice());
    obj.define_data(col_lens, &lens_dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;

    Ok(RelvarDataIds {
        slot,
        relvar_name,
        relvar_name_len: relvar_name_bytes.len() as i64,
        env_name: env_id,
        env_name_len: env_name_bytes.len() as i64,
        default_path: default_id,
        default_path_len: default_bytes.len() as i64,
        table_name: table_id,
        table_name_len: table_bytes.len() as i64,
        col_ptrs,
        col_lens,
        col_count: relvar.columns.len() as i32,
    })
}

/// Declare + define a Local byte-array data symbol. Helper for the
/// per-relvar string payloads.
fn declare_byte_constant(
    obj: &mut ObjectModule,
    sym: &str,
    bytes: &[u8],
) -> Result<DataId, CraneliftEmitError> {
    let id = obj
        .declare_data(sym, Linkage::Local, false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let mut dd = DataDescription::new();
    if bytes.is_empty() {
        // DataDescription rejects zero-length payloads; emit a single
        // zero byte and the runtime treats len == 0 as empty.
        dd.define(vec![0u8].into_boxed_slice());
    } else {
        dd.define(bytes.to_vec().into_boxed_slice());
    }
    obj.define_data(id, &dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    Ok(id)
}

/// Declare + define an anonymous read-only `u32` array (little-endian) — the
/// rename permutation. Anonymous so each `Inst::Rename` gets a fresh symbol
/// with no name-collision bookkeeping.
fn declare_perm_data(obj: &mut ObjectModule, perm: &[u32]) -> Result<DataId, CraneliftEmitError> {
    let id = obj
        .declare_anonymous_data(false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let mut bytes: Vec<u8> = Vec::with_capacity(perm.len() * 4);
    for p in perm {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    let mut dd = DataDescription::new();
    dd.set_align(4); // the runtime reads this as `*const u32`
    if bytes.is_empty() {
        // DataDescription rejects zero-length payloads; the runtime treats
        // perm_count == 0 as empty.
        dd.define(vec![0u8].into_boxed_slice());
    } else {
        dd.define(bytes.into_boxed_slice());
    }
    obj.define_data(id, &dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    Ok(id)
}

/// Emit the per-heading descriptor data keyed by `HeadingId`, recursing into
/// nested-tuple sub-layouts. Returns the top descriptor's `DataId`.
fn emit_heading_descriptor(
    obj: &mut ObjectModule,
    id: HeadingId,
    layout: &RecordLayout,
    ptr_bytes: usize,
) -> Result<DataId, CraneliftEmitError> {
    emit_layout_descriptor(obj, &id.0.to_string(), layout, ptr_bytes)
}

/// Emit the descriptor data for one record layout under symbol `base`:
/// per-attribute name bytes (`.attrname.<base>.<i>`), the attribute array
/// (`.attrs.<base>`), and the descriptor struct (`.heading.<base>`). A `Tuple`
/// attr recurses under `<base>.<i>`, and its descriptor `DataId` is relocated
/// into the parent attr's `sub` field. Layout matches
/// `coddl_runtime::CoddlHeadingDesc` / `CoddlAttrDesc`.
///
/// CoddlAttrDesc bytes (32 on 64-bit, 20 on 32-bit):
///   ptr name            (ptr_bytes)
///   u32 name_len        (4)
///   u32 kind            (4)
///   u32 offset          (4)
///   u32 _pad            (4 on 64-bit for natural alignment)
///   ptr sub             (ptr_bytes; null for scalar cells)
/// CoddlHeadingDesc bytes (16 on 64-bit, 12 on 32-bit):
///   u32 attr_count      (4)
///   u32 record_size     (4)
///   ptr attrs           (ptr_bytes)
fn emit_layout_descriptor(
    obj: &mut ObjectModule,
    base: &str,
    layout: &RecordLayout,
    ptr_bytes: usize,
) -> Result<DataId, CraneliftEmitError> {
    // Nested sub-descriptors first (Tuple attrs); the parent attr's `sub` field
    // relocates to these. `None` for scalar cells.
    let mut sub_ids: Vec<Option<DataId>> = Vec::with_capacity(layout.attrs.len());
    for (i, attr) in layout.attrs.iter().enumerate() {
        match &attr.sub {
            Some(sub) => {
                let sid = emit_layout_descriptor(obj, &format!("{base}.{i}"), sub, ptr_bytes)?;
                sub_ids.push(Some(sid));
            }
            None => sub_ids.push(None),
        }
    }

    // Per-attribute name byte arrays.
    let mut name_ids: Vec<DataId> = Vec::with_capacity(layout.attrs.len());
    for (i, attr) in layout.attrs.iter().enumerate() {
        let sym = format!(".attrname.{}.{}", base, i);
        let nid = obj
            .declare_data(&sym, Linkage::Local, false, false)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        let mut dd = DataDescription::new();
        dd.define(attr.name.as_bytes().to_vec().into_boxed_slice());
        obj.define_data(nid, &dd)
            .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
        name_ids.push(nid);
    }

    // Attribute array. The `sub` pointer sits after the (padded) u32 block —
    // i.e. at the old stride offset; the new stride adds one pointer.
    let sub_off = if ptr_bytes == 8 { 24 } else { 16 };
    let attr_stride = sub_off + ptr_bytes;
    let attrs_sym = format!(".attrs.{}", base);
    let attrs_id = obj
        .declare_data(&attrs_sym, Linkage::Local, false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let attrs_bytes_len = attr_stride * layout.attrs.len();
    let mut attrs_dd = DataDescription::new();
    attrs_dd.set_align(ptr_bytes as u64);
    attrs_dd.define(vec![0u8; attrs_bytes_len].into_boxed_slice());
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
        // `sub` pointer for a Tuple cell; scalars leave it null (zeroed).
        if let Some(sid) = sub_ids[i] {
            let sub_gv = obj.declare_data_in_data(sid, &mut attrs_dd);
            attrs_dd.write_data_addr(offset_in_attrs + sub_off as u32, sub_gv, 0);
        }
    }
    obj.define_data(attrs_id, &attrs_dd)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;

    // The descriptor struct itself.
    let desc_size = 8 + ptr_bytes; // u32 attr_count, u32 record_size, ptr attrs
    let desc_sym = format!(".heading.{}", base);
    let desc_id = obj
        .declare_data(&desc_sym, Linkage::Local, false, false)
        .map_err(|e| CraneliftEmitError::ModuleError(e.to_string()))?;
    let mut desc_dd = DataDescription::new();
    desc_dd.set_align(ptr_bytes as u64);
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

#[allow(clippy::too_many_arguments)]
fn emit_function(
    obj: &mut ObjectModule,
    func: &Function,
    funcid: FuncId,
    funcs: &HashMap<String, FuncId>,
    heading_layouts: &[RecordLayout],
    heading_desc_ids: &[DataId],
    relvar_data: &HashMap<String, RelvarDataIds>,
    private_relvar_slots: &HashMap<String, DataId>,
    plan_data: &HashMap<u32, PlanDataIds>,
    db_data: Option<&DbDataIds>,
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
            relvar_data,
            private_relvar_slots,
            plan_data,
            db_data,
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

#[allow(clippy::too_many_arguments)]
fn emit_block(
    obj: &mut ObjectModule,
    builder: &mut FunctionBuilder<'_>,
    block: &BasicBlock,
    funcs: &HashMap<String, FuncId>,
    heading_layouts: &[RecordLayout],
    heading_desc_ids: &[DataId],
    relvar_data: &HashMap<String, RelvarDataIds>,
    private_relvar_slots: &HashMap<String, DataId>,
    plan_data: &HashMap<u32, PlanDataIds>,
    db_data: Option<&DbDataIds>,
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
            relvar_data,
            private_relvar_slots,
            plan_data,
            db_data,
            values,
            next_data,
        )?;
    }
    emit_terminator(builder, &block.terminator, values, is_main)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_inst(
    obj: &mut ObjectModule,
    builder: &mut FunctionBuilder<'_>,
    inst: &Inst,
    funcs: &HashMap<String, FuncId>,
    heading_layouts: &[RecordLayout],
    heading_desc_ids: &[DataId],
    relvar_data: &HashMap<String, RelvarDataIds>,
    private_relvar_slots: &HashMap<String, DataId>,
    plan_data: &HashMap<u32, PlanDataIds>,
    db_data: Option<&DbDataIds>,
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
            value: Const::Character(cp),
            ty: ProcType::Character,
        } => {
            // A Character is an inline I32 codepoint.
            let v = builder.ins().iconst(types::I32, *cp as i64);
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
                    store_attr(builder, payload, byte_offset, &field_repr, attr.sub.as_ref())?;
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
            // Concatenation: `Text × Text → Text`. The lowerer normalized any
            // `Character` operand to Text, so both operands are (ptr, len).
            // The runtime returns the payload pointer; the result length is
            // `lhs_len + rhs_len` (no fat-pointer return).
            if matches!(op, ScalarOp::Concat) {
                let (lhs_ptr, lhs_len) = text_value(values, lhs)?;
                let (rhs_ptr, rhs_len) = text_value(values, rhs)?;
                let concat_id = funcs["coddl_text_concat"];
                let concat_local = obj.declare_func_in_func(concat_id, builder.func);
                let call = builder
                    .ins()
                    .call(concat_local, &[lhs_ptr, lhs_len, rhs_ptr, rhs_len]);
                let ptr = builder.inst_results(call)[0];
                let len = builder.ins().iadd(lhs_len, rhs_len);
                values.insert(*dst, ValueRepr::Text { ptr, len });
                return Ok(());
            }
            // Text operands are (ptr, len) pairs, not inline scalars, so
            // `=`/`<>` call the runtime byte comparison instead of `icmp`.
            // (Only Eq/NotEq reach here on Text; ordering is Integer-only.)
            if matches!(operand_type, ProcType::Text) {
                use cranelift_codegen::ir::condcodes::IntCC;
                let (lhs_ptr, lhs_len) = text_value(values, lhs)?;
                let (rhs_ptr, rhs_len) = text_value(values, rhs)?;
                let eq_id = funcs["coddl_text_eq"];
                let eq_local = obj.declare_func_in_func(eq_id, builder.func);
                let call = builder
                    .ins()
                    .call(eq_local, &[lhs_ptr, lhs_len, rhs_ptr, rhs_len]);
                let raw = builder.inst_results(call)[0];
                // `coddl_text_eq` returns 1 when equal: `Eq` is `raw != 0`,
                // `NotEq` is `raw == 0`.
                let cc = match op {
                    ScalarOp::Eq => IntCC::NotEqual,
                    ScalarOp::NotEq => IntCC::Equal,
                    other => {
                        return Err(CraneliftEmitError::UnsupportedInst(format!(
                            "operator {other:?} not supported on Text"
                        )))
                    }
                };
                let result = builder.ins().icmp_imm(cc, raw, 0);
                values.insert(*dst, ValueRepr::Scalar(result));
                return Ok(());
            }
            let lhs_v = scalar_value(values, lhs)?;
            let rhs_v = scalar_value(values, rhs)?;
            let result = match op {
                ScalarOp::And => builder.ins().band(lhs_v, rhs_v),
                ScalarOp::Or => builder.ins().bor(lhs_v, rhs_v),
                // `Integer × Integer → Integer`; `sdiv` truncates toward zero.
                ScalarOp::Add => builder.ins().iadd(lhs_v, rhs_v),
                ScalarOp::Sub => builder.ins().isub(lhs_v, rhs_v),
                ScalarOp::Mul => builder.ins().imul(lhs_v, rhs_v),
                ScalarOp::Div => builder.ins().sdiv(lhs_v, rhs_v),
                ScalarOp::Eq
                | ScalarOp::NotEq
                | ScalarOp::Lt
                | ScalarOp::Gt
                | ScalarOp::LtEq
                | ScalarOp::GtEq => {
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
                    // Cranelift `icmp` already yields an I8 on the boolean
                    // lane, matching `cranelift_value_type(Boolean) = I8`.
                    builder.ins().icmp(cc, lhs_v, rhs_v)
                }
                ScalarOp::Concat => {
                    unreachable!("Concat handled before the inline-scalar path")
                }
            };
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::CharToText { dst, src } => {
            // Normalize a `Character` (inline I32 codepoint) to a Text
            // `(ptr, len)`: `coddl_char_to_text` gives the payload pointer and
            // `coddl_utf8_len` the byte length.
            let cp = scalar_value(values, src)?;
            let to_text = obj.declare_func_in_func(funcs["coddl_char_to_text"], builder.func);
            let ptr_call = builder.ins().call(to_text, &[cp]);
            let ptr = builder.inst_results(ptr_call)[0];
            let len_fn = obj.declare_func_in_func(funcs["coddl_utf8_len"], builder.func);
            let len_call = builder.ins().call(len_fn, &[cp]);
            let len = builder.inst_results(len_call)[0];
            values.insert(*dst, ValueRepr::Text { ptr, len });
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
        Inst::AttrStore {
            record,
            offset,
            value,
            attr_type: _,
        } => {
            let payload = scalar_value(values, record)?;
            let repr = values.get(value).cloned().ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!("undefined value {value:?} in AttrStore"))
            })?;
            // The extend/where store path is scalar/Text only (no sub-layout).
            store_attr(builder, payload, *offset as i32, &repr, None)
        }
        Inst::Extend {
            dst,
            src,
            helper_linkage,
            src_heading_id,
            result_heading_id,
        } => {
            let src_v = scalar_value(values, src)?;
            let ptr_ty = obj.target_config().pointer_type();
            let src_desc_gv =
                obj.declare_data_in_func(heading_desc_ids[src_heading_id.0 as usize], builder.func);
            let src_desc_val = builder.ins().symbol_value(ptr_ty, src_desc_gv);
            let res_desc_gv = obj
                .declare_data_in_func(heading_desc_ids[result_heading_id.0 as usize], builder.func);
            let res_desc_val = builder.ins().symbol_value(ptr_ty, res_desc_gv);
            let helper_id = *funcs.get(helper_linkage).ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!(
                    "unresolved extend helper {helper_linkage}"
                ))
            })?;
            let helper_ref = obj.declare_func_in_func(helper_id, builder.func);
            let helper_addr = builder.ins().func_addr(ptr_ty, helper_ref);
            let extend_id = funcs["coddl_relation_extend"];
            let extend_local = obj.declare_func_in_func(extend_id, builder.func);
            let call = builder.ins().call(
                extend_local,
                &[src_v, src_desc_val, res_desc_val, helper_addr],
            );
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Project {
            dst,
            src,
            src_heading_id,
            result_heading_id,
        } => {
            let src_v = scalar_value(values, src)?;
            let ptr_ty = obj.target_config().pointer_type();
            let src_desc_id = heading_desc_ids[src_heading_id.0 as usize];
            let src_desc_gv = obj.declare_data_in_func(src_desc_id, builder.func);
            let src_desc_val = builder.ins().symbol_value(ptr_ty, src_desc_gv);
            let res_desc_id = heading_desc_ids[result_heading_id.0 as usize];
            let res_desc_gv = obj.declare_data_in_func(res_desc_id, builder.func);
            let res_desc_val = builder.ins().symbol_value(ptr_ty, res_desc_gv);
            let project_id = funcs["coddl_relation_project"];
            let project_local = obj.declare_func_in_func(project_id, builder.func);
            let call =
                builder
                    .ins()
                    .call(project_local, &[src_v, src_desc_val, res_desc_val]);
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Restructure {
            dst,
            src,
            src_heading_id,
            result_heading_id,
        } => {
            let src_v = scalar_value(values, src)?;
            let ptr_ty = obj.target_config().pointer_type();
            let src_desc_id = heading_desc_ids[src_heading_id.0 as usize];
            let src_desc_gv = obj.declare_data_in_func(src_desc_id, builder.func);
            let src_desc_val = builder.ins().symbol_value(ptr_ty, src_desc_gv);
            let res_desc_id = heading_desc_ids[result_heading_id.0 as usize];
            let res_desc_gv = obj.declare_data_in_func(res_desc_id, builder.func);
            let res_desc_val = builder.ins().symbol_value(ptr_ty, res_desc_gv);
            let restructure_id = funcs["coddl_relation_restructure"];
            let restructure_local = obj.declare_func_in_func(restructure_id, builder.func);
            let call =
                builder
                    .ins()
                    .call(restructure_local, &[src_v, src_desc_val, res_desc_val]);
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Join {
            dst,
            lhs,
            rhs,
            lhs_heading_id,
            rhs_heading_id,
            result_heading_id,
        } => {
            let lhs_v = scalar_value(values, lhs)?;
            let rhs_v = scalar_value(values, rhs)?;
            let ptr_ty = obj.target_config().pointer_type();
            let lhs_desc_gv = obj
                .declare_data_in_func(heading_desc_ids[lhs_heading_id.0 as usize], builder.func);
            let lhs_desc_val = builder.ins().symbol_value(ptr_ty, lhs_desc_gv);
            let rhs_desc_gv = obj
                .declare_data_in_func(heading_desc_ids[rhs_heading_id.0 as usize], builder.func);
            let rhs_desc_val = builder.ins().symbol_value(ptr_ty, rhs_desc_gv);
            let res_desc_gv = obj.declare_data_in_func(
                heading_desc_ids[result_heading_id.0 as usize],
                builder.func,
            );
            let res_desc_val = builder.ins().symbol_value(ptr_ty, res_desc_gv);
            let join_id = funcs["coddl_relation_join"];
            let join_local = obj.declare_func_in_func(join_id, builder.func);
            let call = builder.ins().call(
                join_local,
                &[lhs_v, lhs_desc_val, rhs_v, rhs_desc_val, res_desc_val],
            );
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Union {
            dst,
            lhs,
            rhs,
            heading_id,
        } => {
            let lhs_v = scalar_value(values, lhs)?;
            let rhs_v = scalar_value(values, rhs)?;
            let ptr_ty = obj.target_config().pointer_type();
            let desc_gv =
                obj.declare_data_in_func(heading_desc_ids[heading_id.0 as usize], builder.func);
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let union_id = funcs["coddl_relation_union"];
            let union_local = obj.declare_func_in_func(union_id, builder.func);
            let call = builder.ins().call(union_local, &[lhs_v, rhs_v, desc_val]);
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Minus {
            dst,
            lhs,
            rhs,
            heading_id,
        } => {
            let lhs_v = scalar_value(values, lhs)?;
            let rhs_v = scalar_value(values, rhs)?;
            let ptr_ty = obj.target_config().pointer_type();
            let desc_gv =
                obj.declare_data_in_func(heading_desc_ids[heading_id.0 as usize], builder.func);
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let minus_id = funcs["coddl_relation_minus"];
            let minus_local = obj.declare_func_in_func(minus_id, builder.func);
            let call = builder.ins().call(minus_local, &[lhs_v, rhs_v, desc_val]);
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::TClose {
            dst,
            src,
            heading_id,
        } => {
            let src_v = scalar_value(values, src)?;
            let ptr_ty = obj.target_config().pointer_type();
            let desc_gv =
                obj.declare_data_in_func(heading_desc_ids[heading_id.0 as usize], builder.func);
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let tclose_id = funcs["coddl_relation_tclose"];
            let tclose_local = obj.declare_func_in_func(tclose_id, builder.func);
            let call = builder.ins().call(tclose_local, &[src_v, desc_val]);
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Rename {
            dst,
            src,
            src_heading_id,
            result_heading_id,
            perm,
        } => {
            let src_v = scalar_value(values, src)?;
            let ptr_ty = obj.target_config().pointer_type();
            let src_desc_id = heading_desc_ids[src_heading_id.0 as usize];
            let src_desc_gv = obj.declare_data_in_func(src_desc_id, builder.func);
            let src_desc_val = builder.ins().symbol_value(ptr_ty, src_desc_gv);
            let res_desc_id = heading_desc_ids[result_heading_id.0 as usize];
            let res_desc_gv = obj.declare_data_in_func(res_desc_id, builder.func);
            let res_desc_val = builder.ins().symbol_value(ptr_ty, res_desc_gv);
            let perm_id = declare_perm_data(obj, perm)?;
            let perm_gv = obj.declare_data_in_func(perm_id, builder.func);
            let perm_val = builder.ins().symbol_value(ptr_ty, perm_gv);
            let count_val = builder.ins().iconst(ptr_ty, perm.len() as i64);
            let rename_id = funcs["coddl_relation_rename"];
            let rename_local = obj.declare_func_in_func(rename_id, builder.func);
            let call = builder.ins().call(
                rename_local,
                &[src_v, src_desc_val, res_desc_val, perm_val, count_val],
            );
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Extract {
            dst,
            src,
            heading_id,
        } => {
            let src_v = scalar_value(values, src)?;
            let layout = &heading_layouts[heading_id.0 as usize];
            let desc_id = heading_desc_ids[heading_id.0 as usize];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let ptr_ty = obj.target_config().pointer_type();
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            // Call the cardinality-check extern; it returns the
            // record pointer (or aborts).
            let extract_id = funcs["coddl_extract_check_cardinality"];
            let extract_local = obj.declare_func_in_func(extract_id, builder.func);
            let call = builder.ins().call(extract_local, &[src_v, desc_val]);
            let record_ptr = builder.inst_results(call)[0];
            // Read each attribute from the record pointer; bundle
            // into a ValueRepr::Tuple.
            let flags = MemFlags::trusted();
            let mut fields: Vec<(String, ValueRepr)> = Vec::with_capacity(layout.attrs.len());
            for attr in &layout.attrs {
                let attr_type = proc_type_from_kind_cl(attr.kind);
                let offset = attr.offset as i32;
                let repr = match attr_type {
                    ProcType::Integer => {
                        let v = builder.ins().load(types::I64, flags, record_ptr, offset);
                        ValueRepr::Scalar(v)
                    }
                    ProcType::Boolean => {
                        let raw =
                            builder.ins().load(types::I64, flags, record_ptr, offset);
                        let v = builder.ins().ireduce(types::I8, raw);
                        ValueRepr::Scalar(v)
                    }
                    ProcType::Text => {
                        let ptr = builder.ins().load(ptr_ty, flags, record_ptr, offset);
                        let len = builder
                            .ins()
                            .load(types::I64, flags, record_ptr, offset + 8);
                        ValueRepr::Text { ptr, len }
                    }
                    other => {
                        return Err(CraneliftEmitError::UnsupportedInst(format!(
                            "Extract attribute of type {other:?} not yet supported"
                        )));
                    }
                };
                fields.push((attr.name.clone(), repr));
            }
            values.insert(*dst, ValueRepr::Tuple { fields });
            Ok(())
        }
        Inst::RelvarSlotInit { name, heading_id } => {
            let ids = relvar_data.get(name).ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!(
                    "RelvarSlotInit references unknown relvar `{name}`"
                ))
            })?;
            let ptr_ty = obj.target_config().pointer_type();
            // 1. Resolve the env-var override → path (ptr, len).
            //    `coddl_resolve_op_field` writes len into an alloca'd
            //    i64 slot we set up first.
            let slot_len = builder.create_sized_stack_slot(
                cranelift_codegen::ir::StackSlotData::new(
                    cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ),
            );
            let len_addr = builder.ins().stack_addr(ptr_ty, slot_len, 0);
            let resolve_id = funcs["coddl_resolve_op_field"];
            let resolve_local = obj.declare_func_in_func(resolve_id, builder.func);
            let env_gv = obj.declare_data_in_func(ids.env_name, builder.func);
            let env_addr = builder.ins().symbol_value(ptr_ty, env_gv);
            let env_len = builder.ins().iconst(types::I64, ids.env_name_len);
            let default_gv = obj.declare_data_in_func(ids.default_path, builder.func);
            let default_addr = builder.ins().symbol_value(ptr_ty, default_gv);
            let default_len = builder.ins().iconst(types::I64, ids.default_path_len);
            let call = builder.ins().call(
                resolve_local,
                &[env_addr, env_len, default_addr, default_len, len_addr],
            );
            let resolved_ptr = builder.inst_results(call)[0];
            let resolved_len =
                builder.ins().load(types::I64, MemFlags::trusted(), len_addr, 0);

            // 2. Call coddl_sqlite_relvar_init.
            let relvar_name_gv = obj.declare_data_in_func(ids.relvar_name, builder.func);
            let relvar_name_addr = builder.ins().symbol_value(ptr_ty, relvar_name_gv);
            let relvar_name_len = builder.ins().iconst(types::I64, ids.relvar_name_len);
            let table_gv = obj.declare_data_in_func(ids.table_name, builder.func);
            let table_addr = builder.ins().symbol_value(ptr_ty, table_gv);
            let table_len = builder.ins().iconst(types::I64, ids.table_name_len);
            let col_ptrs_gv = obj.declare_data_in_func(ids.col_ptrs, builder.func);
            let col_ptrs_addr = builder.ins().symbol_value(ptr_ty, col_ptrs_gv);
            let col_lens_gv = obj.declare_data_in_func(ids.col_lens, builder.func);
            let col_lens_addr = builder.ins().symbol_value(ptr_ty, col_lens_gv);
            let col_count_val = builder.ins().iconst(types::I32, ids.col_count as i64);
            let desc_id = heading_desc_ids[heading_id.0 as usize];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let slot_gv = obj.declare_data_in_func(ids.slot, builder.func);
            let slot_addr = builder.ins().symbol_value(ptr_ty, slot_gv);
            let init_id = funcs["coddl_sqlite_relvar_init"];
            let init_local = obj.declare_func_in_func(init_id, builder.func);
            builder.ins().call(
                init_local,
                &[
                    relvar_name_addr,
                    relvar_name_len,
                    resolved_ptr,
                    resolved_len,
                    table_addr,
                    table_len,
                    col_ptrs_addr,
                    col_lens_addr,
                    col_count_val,
                    desc_val,
                    slot_addr,
                ],
            );
            Ok(())
        }
        Inst::RelvarSlotRelease { name } => {
            let slot = relvar_data
                .get(name)
                .map(|ids| ids.slot)
                .or_else(|| private_relvar_slots.get(name).copied())
                .ok_or_else(|| {
                    CraneliftEmitError::UnsupportedInst(format!(
                        "RelvarSlotRelease references unknown relvar `{name}`"
                    ))
                })?;
            let ptr_ty = obj.target_config().pointer_type();
            let slot_gv = obj.declare_data_in_func(slot, builder.func);
            let slot_addr = builder.ins().symbol_value(ptr_ty, slot_gv);
            let payload = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), slot_addr, 0);
            let release_id = funcs["coddl_rc_release"];
            let release_local = obj.declare_func_in_func(release_id, builder.func);
            builder.ins().call(release_local, &[payload]);
            Ok(())
        }
        Inst::RelvarRead {
            dst,
            name,
            heading_id: _,
        } => {
            let slot = relvar_data
                .get(name)
                .map(|ids| ids.slot)
                .or_else(|| private_relvar_slots.get(name).copied())
                .ok_or_else(|| {
                    CraneliftEmitError::UnsupportedInst(format!(
                        "RelvarRead references unknown relvar `{name}`"
                    ))
                })?;
            let ptr_ty = obj.target_config().pointer_type();
            let slot_gv = obj.declare_data_in_func(slot, builder.func);
            let slot_addr = builder.ins().symbol_value(ptr_ty, slot_gv);
            let payload = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), slot_addr, 0);
            let retain_id = funcs["coddl_rc_retain"];
            let retain_local = obj.declare_func_in_func(retain_id, builder.func);
            builder.ins().call(retain_local, &[payload]);
            values.insert(*dst, ValueRepr::Scalar(payload));
            Ok(())
        }
        Inst::PrivateRelvarSlotInit { name, heading_id } => {
            let slot = private_relvar_slots.get(name).copied().ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!(
                    "PrivateRelvarSlotInit references unknown relvar `{name}`"
                ))
            })?;
            let ptr_ty = obj.target_config().pointer_type();
            let desc_id = heading_desc_ids[heading_id.0 as usize];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let slot_gv = obj.declare_data_in_func(slot, builder.func);
            let slot_addr = builder.ins().symbol_value(ptr_ty, slot_gv);
            let init_id = funcs["coddl_relvar_slot_init_empty"];
            let init_local = obj.declare_func_in_func(init_id, builder.func);
            builder.ins().call(init_local, &[desc_val, slot_addr]);
            Ok(())
        }
        Inst::RelvarSlotStore { name, value } => {
            let slot = private_relvar_slots
                .get(name)
                .copied()
                .or_else(|| relvar_data.get(name).map(|ids| ids.slot))
                .ok_or_else(|| {
                    CraneliftEmitError::UnsupportedInst(format!(
                        "RelvarSlotStore references unknown relvar `{name}`"
                    ))
                })?;
            let value_v = scalar_value(values, value)?;
            let ptr_ty = obj.target_config().pointer_type();
            let slot_gv = obj.declare_data_in_func(slot, builder.func);
            let slot_addr = builder.ins().symbol_value(ptr_ty, slot_gv);
            let store_id = funcs["coddl_relvar_slot_store"];
            let store_local = obj.declare_func_in_func(store_id, builder.func);
            builder.ins().call(store_local, &[value_v, slot_addr]);
            Ok(())
        }
        Inst::RegisterDatabase => {
            let db = db_data.ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(
                    "RegisterDatabase with no database data".into(),
                )
            })?;
            let ptr_ty = obj.target_config().pointer_type();
            // Resolve the env override → path; len is written into a stack slot.
            let len_slot = builder.create_sized_stack_slot(
                cranelift_codegen::ir::StackSlotData::new(
                    cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ),
            );
            let len_addr = builder.ins().stack_addr(ptr_ty, len_slot, 0);
            let resolve_local =
                obj.declare_func_in_func(funcs["coddl_resolve_op_field"], builder.func);
            let env_gv = obj.declare_data_in_func(db.env_name, builder.func);
            let env_addr = builder.ins().symbol_value(ptr_ty, env_gv);
            let env_len = builder.ins().iconst(types::I64, db.env_name_len);
            let default_gv = obj.declare_data_in_func(db.default_path, builder.func);
            let default_addr = builder.ins().symbol_value(ptr_ty, default_gv);
            let default_len = builder.ins().iconst(types::I64, db.default_path_len);
            let call = builder.ins().call(
                resolve_local,
                &[env_addr, env_len, default_addr, default_len, len_addr],
            );
            let resolved_ptr = builder.inst_results(call)[0];
            let resolved_len = builder.ins().load(types::I64, MemFlags::trusted(), len_addr, 0);
            // Register the database.
            let name_gv = obj.declare_data_in_func(db.name, builder.func);
            let name_addr = builder.ins().symbol_value(ptr_ty, name_gv);
            let name_len = builder.ins().iconst(types::I64, db.name_len);
            let reg_local =
                obj.declare_func_in_func(funcs["coddl_register_database"], builder.func);
            builder
                .ins()
                .call(reg_local, &[name_addr, name_len, resolved_ptr, resolved_len]);
            Ok(())
        }
        Inst::RegisterPlan { plan_id } => {
            let p = plan_data.get(plan_id).ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst(format!(
                    "RegisterPlan references unknown plan {plan_id}"
                ))
            })?;
            let ptr_ty = obj.target_config().pointer_type();
            let plan_id_v = builder.ins().iconst(types::I32, *plan_id as i64);
            let dbname_gv = obj.declare_data_in_func(p.db_name, builder.func);
            let dbname_addr = builder.ins().symbol_value(ptr_ty, dbname_gv);
            let dbname_len = builder.ins().iconst(types::I64, p.db_name_len);
            let sql_gv = obj.declare_data_in_func(p.sql, builder.func);
            let sql_addr = builder.ins().symbol_value(ptr_ty, sql_gv);
            let sql_len = builder.ins().iconst(types::I64, p.sql_len);
            let pcount = builder.ins().iconst(types::I32, p.param_count as i64);
            let desc_id = heading_desc_ids[p.result_heading_id];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let desc_addr = builder.ins().symbol_value(ptr_ty, desc_gv);
            let reg_local =
                obj.declare_func_in_func(funcs["coddl_register_plan"], builder.func);
            builder.ins().call(
                reg_local,
                &[
                    plan_id_v,
                    dbname_addr,
                    dbname_len,
                    sql_addr,
                    sql_len,
                    pcount,
                    desc_addr,
                ],
            );
            Ok(())
        }
        Inst::Query {
            dst,
            plan_id,
            params,
            heading_id: _,
        } => {
            let params_arg = build_coddl_param_array_cl(obj, builder, values, params)?;
            let plan_id_v = builder.ins().iconst(types::I32, *plan_id as i64);
            let n_v = builder.ins().iconst(types::I64, params.len() as i64);
            let query_local = obj.declare_func_in_func(funcs["coddl_query"], builder.func);
            let call = builder
                .ins()
                .call(query_local, &[plan_id_v, params_arg, n_v]);
            let result = builder.inst_results(call)[0];
            values.insert(*dst, ValueRepr::Scalar(result));
            Ok(())
        }
        Inst::Dml { plan_id, params } => {
            // Fire a registered DML plan for effect — same param marshaling as a
            // query, but `coddl_exec` returns a status that is discarded (the
            // runtime aborts on a hard failure).
            let params_arg = build_coddl_param_array_cl(obj, builder, values, params)?;
            let plan_id_v = builder.ins().iconst(types::I32, *plan_id as i64);
            let n_v = builder.ins().iconst(types::I64, params.len() as i64);
            let exec_local = obj.declare_func_in_func(funcs["coddl_exec"], builder.func);
            builder.ins().call(exec_local, &[plan_id_v, params_arg, n_v]);
            Ok(())
        }
        Inst::InsertFrom {
            plan_id,
            src,
            heading_id,
        } => {
            // Ship `src`'s in-memory rows into the target via the insert template
            // — pass the relation pointer + its heading descriptor (like
            // `coddl_write_relation`) plus the plan id; the status is discarded.
            let ptr_ty = obj.target_config().pointer_type();
            let src_v = scalar_value(values, src)?;
            let desc_id = heading_desc_ids[heading_id.0 as usize];
            let desc_gv = obj.declare_data_in_func(desc_id, builder.func);
            let desc_val = builder.ins().symbol_value(ptr_ty, desc_gv);
            let plan_id_v = builder.ins().iconst(types::I32, *plan_id as i64);
            let local = obj.declare_func_in_func(funcs["coddl_exec_insert"], builder.func);
            builder.ins().call(local, &[plan_id_v, src_v, desc_val]);
            Ok(())
        }
    }
}

/// Build the `CoddlParam` array on the stack for a `query`/`exec` call —
/// `{ i64 i, ptr, i64 len, i32 kind }`, 32-byte stride — and return the pointer
/// argument value (a null pointer when there are no params). Shared by the
/// `Query` and `Dml` instruction arms.
fn build_coddl_param_array_cl(
    obj: &ObjectModule,
    builder: &mut FunctionBuilder<'_>,
    values: &HashMap<ValueId, ValueRepr>,
    params: &[(ValueId, ProcType)],
) -> Result<CrValue, CraneliftEmitError> {
    let ptr_ty = obj.target_config().pointer_type();
    let n = params.len();
    if n == 0 {
        return Ok(builder.ins().iconst(ptr_ty, 0));
    }
    let slot = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
        cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
        (n * 32) as u32,
        3,
    ));
    for (i, (vid, ty)) in params.iter().enumerate() {
        let base = (i * 32) as i32;
        let kind = kind_tag_for_cl(ty)?;
        match ty {
            ProcType::Integer => {
                let v = scalar_value(values, vid)?;
                builder.ins().stack_store(v, slot, base);
            }
            ProcType::Boolean => {
                let v = scalar_value(values, vid)?; // I8
                let ext = builder.ins().uextend(types::I64, v);
                builder.ins().stack_store(ext, slot, base);
            }
            ProcType::Text => {
                let (ptr, len) = text_value(values, vid)?;
                builder.ins().stack_store(ptr, slot, base + 8);
                builder.ins().stack_store(len, slot, base + 16);
            }
            other => {
                return Err(CraneliftEmitError::UnsupportedInst(format!(
                    "query bind param of type {other:?} not supported"
                )));
            }
        }
        let kind_v = builder.ins().iconst(types::I32, kind as i64);
        builder.ins().stack_store(kind_v, slot, base + 24);
    }
    Ok(builder.ins().stack_addr(ptr_ty, slot, 0))
}

/// Map a `record_layout` attribute kind to its `ProcType`. Same
/// shape as the LLVM backend's `proc_type_from_kind_llvm`.
fn proc_type_from_kind_cl(kind: u32) -> ProcType {
    use coddl_procir::kind_tag;
    match kind {
        k if k == kind_tag::INTEGER => ProcType::Integer,
        k if k == kind_tag::BOOLEAN => ProcType::Boolean,
        k if k == kind_tag::TEXT => ProcType::Text,
        other => unreachable!("unsupported attr kind {other} in Extract"),
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

/// Extract the `(ptr, len)` pair of a `Text`-typed SSA value. Used when
/// binding a Text query parameter into a `CoddlParam`.
fn text_value(
    values: &HashMap<ValueId, ValueRepr>,
    v: &ValueId,
) -> Result<(CrValue, CrValue), CraneliftEmitError> {
    match values.get(v) {
        Some(ValueRepr::Text { ptr, len }) => Ok((*ptr, *len)),
        _ => Err(CraneliftEmitError::UnsupportedInst(format!(
            "expected Text value at {v:?}"
        ))),
    }
}

/// Map a scalar `ProcType` to its `CoddlAttrKind` tag for a `CoddlParam`.
fn kind_tag_for_cl(ty: &ProcType) -> Result<u32, CraneliftEmitError> {
    use coddl_procir::kind_tag;
    match ty {
        ProcType::Integer => Ok(kind_tag::INTEGER),
        ProcType::Boolean => Ok(kind_tag::BOOLEAN),
        ProcType::Text => Ok(kind_tag::TEXT),
        other => Err(CraneliftEmitError::UnsupportedInst(format!(
            "query param of type {other:?} has no CoddlParam kind"
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
    sub: Option<&RecordLayout>,
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
        // Inline nested-tuple cell: store each component into the sub-region at
        // `byte_offset + sub_attr.offset`, recursing (sub-layout gives offsets,
        // the `ValueRepr::Tuple` gives values, both name-canonical).
        ValueRepr::Tuple { fields } => {
            let sub = sub.ok_or_else(|| {
                CraneliftEmitError::UnsupportedInst("tuple cell store without a sub-layout".into())
            })?;
            for sub_attr in &sub.attrs {
                let field = fields
                    .iter()
                    .find(|(n, _)| n == &sub_attr.name)
                    .map(|(_, r)| r.clone())
                    .ok_or_else(|| {
                        CraneliftEmitError::UnsupportedInst(format!(
                            "tuple value missing field `{}` for cell layout",
                            sub_attr.name
                        ))
                    })?;
                store_attr(
                    builder,
                    payload,
                    byte_offset + sub_attr.offset as i32,
                    &field,
                    sub_attr.sub.as_ref(),
                )?;
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

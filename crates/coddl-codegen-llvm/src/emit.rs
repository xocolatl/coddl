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
    BasicBlock, Codegen, Const, Function, Inst, Module, ProcType, Terminator, ValueId,
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
/// value.
#[derive(Debug, Clone)]
enum ValueRepr {
    Scalar { ty: String, op: String },
    Text { ptr_op: String, len_op: String },
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
}

impl Emitter {
    fn emit_module(&mut self, module: &Module) -> Result<(), LlvmEmitError> {
        writeln!(self.body, "; ModuleID = '{}'", module.program_name).unwrap();
        writeln!(self.body).unwrap();

        for func in module.functions.iter().filter(|f| f.is_extern()) {
            self.emit_extern(func)?;
        }
        if module.functions.iter().any(Function::is_extern) {
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
            push_param_types(&mut params, *pty);
        }
        writeln!(
            self.body,
            "declare {ret} @{linkage}({args})",
            ret = llvm_return_type(func.return_type),
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
            llvm_return_type(func.return_type)
        };

        let mut params: Vec<String> = Vec::new();
        for (pname, pty) in &func.params {
            push_param_decl(&mut params, pname, *pty);
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
            } => self.lower_call(*dst, callee, args, *return_type),
        }
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
        return_type: ProcType,
    ) -> Result<(), LlvmEmitError> {
        let mut call_args: Vec<String> = Vec::new();
        for arg in args {
            let repr = self
                .values
                .get(arg)
                .ok_or_else(|| LlvmEmitError::UnsupportedInst(format!("undefined value {arg:?}")))?
                .clone();
            match repr {
                ValueRepr::Scalar { ty, op } => call_args.push(format!("{ty} {op}")),
                ValueRepr::Text { ptr_op, len_op } => {
                    call_args.push(format!("ptr {ptr_op}"));
                    call_args.push(format!("i64 {len_op}"));
                }
            }
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

fn llvm_return_type(ty: ProcType) -> String {
    match ty {
        ProcType::Unit => "void".to_string(),
        other => llvm_value_type(other).to_string(),
    }
}

fn llvm_value_type(ty: ProcType) -> &'static str {
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
    }
}

fn push_param_types(out: &mut Vec<String>, ty: ProcType) {
    match ty {
        ProcType::Text | ProcType::Binary => {
            out.push("ptr".to_string());
            out.push("i64".to_string());
        }
        other => out.push(llvm_value_type(other).to_string()),
    }
}

fn push_param_decl(out: &mut Vec<String>, name: &str, ty: ProcType) {
    match ty {
        ProcType::Text | ProcType::Binary => {
            out.push(format!("ptr %{name}.ptr"));
            out.push(format!("i64 %{name}.len"));
        }
        other => out.push(format!("{ty} %{name}", ty = llvm_value_type(other))),
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
            assert!(!llvm_value_type(ty).is_empty(), "{ty:?}");
        }
        assert_eq!(llvm_return_type(ProcType::Unit), "void");
    }
}

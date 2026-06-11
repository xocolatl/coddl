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

use std::collections::HashSet;

use coddl_diagnostics::{Diagnostic, FileId, Severity};
use coddl_syntax::ast::{
    AstNode, Block, CallExpr, Expr, ExprStmt, Item, Literal, NamedArg, OperDecl, ProgramDecl, Root,
    Stmt,
};
use coddl_syntax::SyntaxKind;
use coddl_types::check;

use crate::ir::{
    BasicBlock, BlockId, Const, Function, Inst, Module, ProcType, Terminator, ValueId,
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
pub fn lower(source: &str, file: FileId) -> LowerOutput {
    let check_out = check(source, file);
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
    let mut lowerer = Lowerer::new();
    let module = lowerer.lower_root(&root);
    LowerOutput {
        module: Some(module),
        diagnostics: check_out.diagnostics,
    }
}

struct Lowerer {
    program_name: String,
    functions: Vec<Function>,
    seen_externs: HashSet<&'static str>,
    // Per-function state, reset on each `lower_oper_decl`.
    next_value: u32,
    next_block: u32,
    insts: Vec<Inst>,
}

impl Lowerer {
    fn new() -> Self {
        Self {
            program_name: String::new(),
            functions: Vec::new(),
            seen_externs: HashSet::new(),
            next_value: 0,
            next_block: 0,
            insts: Vec::new(),
        }
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
                .map(|(n, t)| ((*n).to_string(), *t))
                .collect(),
            return_type: ext.return_type,
            blocks: Vec::new(),
        });
    }

    // ── Walks ────────────────────────────────────────────────────────

    fn lower_root(&mut self, root: &Root) -> Module {
        for item in root.items() {
            match item {
                Item::ProgramDecl(p) => self.lower_program_decl(&p),
                Item::OperDecl(o) => {
                    let func = self.lower_oper_decl(&o);
                    self.functions.push(func);
                }
            }
        }
        Module {
            program_name: std::mem::take(&mut self.program_name),
            functions: std::mem::take(&mut self.functions),
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

        let block_id = self.fresh_block();
        if let Some(body) = decl.body() {
            self.lower_block(&body);
        }

        let block = BasicBlock {
            id: block_id,
            insts: std::mem::take(&mut self.insts),
            terminator: Terminator::Return(None),
        };

        Function {
            name,
            linkage_name,
            params,
            return_type: ProcType::Unit,
            blocks: vec![block],
        }
    }

    fn lower_block(&mut self, block: &Block) {
        for stmt in block.statements() {
            match stmt {
                Stmt::ExprStmt(e) => self.lower_expr_stmt(&e),
            }
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
            Expr::NameRef(_) => {
                // No value-level bindings yet — the typechecker accepts
                // parameter references but no construct in the current
                // language consumes the resulting value. When `let` /
                // `mut` / argument forwarding land, this gains a real
                // case.
                self.fresh_value()
            }
        }
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
        self.insts.push(Inst::Const { dst, value, ty });
        dst
    }

    fn lower_call(&mut self, call: &CallExpr) -> ValueId {
        let callee_name = match call.callee() {
            Some(Expr::NameRef(n)) => n.ident().map(|t| t.text().to_string()),
            _ => None,
        };
        let surface = callee_name.expect("typechecked call has a NameRef callee");
        let ext = self
            .lookup_extern(&surface)
            .unwrap_or_else(|| unreachable!("unknown callee `{surface}` survived typecheck"));
        let linkage = ext.linkage.to_string();
        let return_type = ext.return_type;

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
            Some(self.fresh_value())
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
        dst.unwrap_or_else(|| self.fresh_value())
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
    fn hello_world_lowers_to_two_functions() {
        let m = lower_ok(HELLO_WORLD);
        let names: Vec<_> = m.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.contains(&"main") && names.contains(&"write_line"),
            "expected main + write_line in {names:?}"
        );
        assert_eq!(m.functions.len(), 2);
    }

    #[test]
    fn hello_world_main_body_is_const_call_return() {
        let m = lower_ok(HELLO_WORLD);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(main.blocks.len(), 1);
        let block = &main.blocks[0];
        assert_eq!(block.insts.len(), 2);
        match &block.insts[0] {
            Inst::Const {
                value: Const::Text(bytes),
                ty: ProcType::Text,
                ..
            } => assert_eq!(bytes, b"Hello, world!"),
            other => panic!("expected Const Text, got {other:?}"),
        }
        match &block.insts[1] {
            Inst::Call {
                dst: None,
                callee,
                args,
                return_type: ProcType::Unit,
            } => {
                assert_eq!(callee, "coddl_write_line");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
        assert!(matches!(block.terminator, Terminator::Return(None)));
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
        match &block.insts[0] {
            Inst::Const {
                value: Const::Text(bytes),
                ..
            } => assert_eq!(bytes, b"a\nb"),
            other => panic!("expected Const Text, got {other:?}"),
        }
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
        let m = lower_ok(HELLO_WORLD);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let call = main
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find_map(|i| match i {
                Inst::Call { callee, .. } => Some(callee.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(call, "coddl_write_line");
    }

    #[test]
    fn integer_literal_decodes_decimal_and_hex() {
        assert_eq!(parse_integer_literal("42"), 42);
        assert_eq!(parse_integer_literal("0x2a"), 42);
        assert_eq!(parse_integer_literal("0b101010"), 42);
        assert_eq!(parse_integer_literal("0o52"), 42);
        assert_eq!(parse_integer_literal("1_000"), 1000);
    }
}

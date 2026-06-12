//! ProcIR data types.
//!
//! SSA-shaped, backend-agnostic. A `Module` carries one `Function` per
//! `oper` decl plus a synthetic extern `Function` for each runtime
//! symbol referenced. Each `Function` is a sequence of `BasicBlock`s;
//! each `BasicBlock` is a list of `Inst` plus a `Terminator`. Every
//! instruction defines at most one SSA value.
//!
//! See `docs/procir.md` for the spec and `ARCHITECTURE.md §4` for the
//! design rationale.

use std::fmt;

pub use coddl_types::{Heading, Type};

/// One compilation unit.
#[derive(Clone, Debug)]
pub struct Module {
    /// From `program <name>;`. Empty if the source had no program decl.
    pub program_name: String,
    pub functions: Vec<Function>,
}

/// A function — either a defined one (non-empty `blocks`) or an extern
/// declaration (`blocks.is_empty()`).
#[derive(Clone, Debug)]
pub struct Function {
    /// User-visible Coddl name. For an extern, this matches the
    /// surface symbol the user wrote (`write_line`).
    pub name: String,
    /// C-ABI symbol the backend emits. For `main`, `"main"`; for an
    /// extern, the declared `coddl_*` name. The lowering pass sets
    /// this explicitly so backends never have to derive it.
    pub linkage_name: String,
    pub params: Vec<(String, ProcType)>,
    pub return_type: ProcType,
    /// Empty for an extern declaration.
    pub blocks: Vec<BasicBlock>,
}

impl Function {
    pub fn is_extern(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct BasicBlock {
    pub id: BlockId,
    pub insts: Vec<Inst>,
    pub terminator: Terminator,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BlockId(pub u32);

/// An SSA value name. Rendered `%0`, `%1`, … in the `Display` form.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ValueId(pub u32);

#[derive(Clone, Debug)]
pub enum Inst {
    /// Materialize a compile-time constant.
    Const {
        dst: ValueId,
        value: Const,
        ty: ProcType,
    },
    /// Call a function by linkage name. `dst` is `None` when the
    /// callee returns `Unit`.
    Call {
        dst: Option<ValueId>,
        callee: String,
        args: Vec<ValueId>,
        return_type: ProcType,
    },
    /// Build a tuple value from its fields, in canonical heading
    /// order. The `heading` is the type-level shape; `fields` holds
    /// the SSA values, one per attribute, paired with the attribute
    /// name. Tuples are pure value types — no heap, no RC. Backends
    /// represent them as a compile-time grouping over the field SSA
    /// values; at ABI boundaries the fields flatten into their
    /// component scalar operands (the same shape as `Text → (ptr,
    /// len)`, recursive for nested tuples).
    TupleLit {
        dst: ValueId,
        fields: Vec<(String, ValueId)>,
        heading: Heading,
    },
    /// Project a single field out of a tuple. `field_type` is the
    /// attribute's ProcType (carried so backends needn't re-lookup
    /// through the heading). Like `TupleLit`, this is a compile-time
    /// projection — no runtime work.
    TupleField {
        dst: ValueId,
        src: ValueId,
        field_name: String,
        field_type: ProcType,
    },
}

#[derive(Clone, Debug)]
pub enum Terminator {
    Return(Option<ValueId>),
    /// Reserved for control-flow paths the typechecker has ruled out
    /// (e.g. a divergent branch). Not produced by hello-world.
    Unreachable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Const {
    Integer(i64),
    /// String literal payload as UTF-8 bytes (escapes already decoded).
    Text(Vec<u8>),
    /// The `Tuple {}` value — produced where the source had `{}` or
    /// an implicit unit return.
    Unit,
}

/// Machine-level type. Not the surface `Type` from `coddl-types` —
/// `Relation` and `Sequence` become runtime handles (`Pointer`).
/// `Tuple(H)` carries the same heading the typechecker reasoned about;
/// at ABI boundaries each attribute flattens into its component
/// scalar operands (nested tuples recursively). Every built-in scalar
/// gets a variant from day one so backends can pattern-match
/// exhaustively.
///
/// Not `Copy` — the `Tuple` variant carries a heap-backed heading.
/// Clone is cheap relative to typical compile-time data sizes; runtime
/// never touches `ProcType` values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcType {
    Integer,
    Rational,
    Approximate,
    Text,
    Character,
    Binary,
    Byte,
    Boolean,
    Unit,
    Pointer,
    Tuple(Heading),
}

// ── Display ──────────────────────────────────────────────────────────

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block_{}", self.0)
    }
}

impl fmt::Display for ProcType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcType::Integer => f.write_str("Integer"),
            ProcType::Rational => f.write_str("Rational"),
            ProcType::Approximate => f.write_str("Approximate"),
            ProcType::Text => f.write_str("Text"),
            ProcType::Character => f.write_str("Character"),
            ProcType::Binary => f.write_str("Binary"),
            ProcType::Byte => f.write_str("Byte"),
            ProcType::Boolean => f.write_str("Boolean"),
            ProcType::Unit => f.write_str("Unit"),
            ProcType::Pointer => f.write_str("Pointer"),
            ProcType::Tuple(h) => write!(f, "Tuple {h}"),
        }
    }
}

impl fmt::Display for Const {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Const::Integer(n) => write!(f, "{n}"),
            Const::Text(bytes) => {
                f.write_str("\"")?;
                for &b in bytes {
                    match b {
                        b'\n' => f.write_str("\\n")?,
                        b'\r' => f.write_str("\\r")?,
                        b'\t' => f.write_str("\\t")?,
                        b'"' => f.write_str("\\\"")?,
                        b'\\' => f.write_str("\\\\")?,
                        0x20..=0x7e => write!(f, "{}", b as char)?,
                        _ => write!(f, "\\x{b:02x}")?,
                    }
                }
                f.write_str("\"")
            }
            Const::Unit => f.write_str("{}"),
        }
    }
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inst::Const { dst, value, ty } => write!(f, "{dst} = const {ty} {value}"),
            Inst::Call {
                dst,
                callee,
                args,
                return_type: _,
            } => {
                if let Some(d) = dst {
                    write!(f, "{d} = ")?;
                }
                write!(f, "call {callee}(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(")")
            }
            Inst::TupleLit { dst, fields, .. } => {
                write!(f, "{dst} = tuple_lit {{")?;
                for (i, (name, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {v}")?;
                }
                f.write_str("}")
            }
            Inst::TupleField {
                dst,
                src,
                field_name,
                ..
            } => write!(f, "{dst} = field {src}.{field_name}"),
        }
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::Return(None) => f.write_str("return"),
            Terminator::Return(Some(v)) => write!(f, "return {v}"),
            Terminator::Unreachable => f.write_str("unreachable"),
        }
    }
}

impl fmt::Display for BasicBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "    {}:", self.id)?;
        for inst in &self.insts {
            writeln!(f, "        {inst}")?;
        }
        write!(f, "        {}", self.terminator)
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_extern() {
            f.write_str("  extern fn ")?;
        } else {
            f.write_str("  fn ")?;
        }
        // For an extern, the linkage name *is* the visible identity.
        // For a defined function the surface name is what reads
        // naturally — debugging text, not the linker symbol.
        if self.is_extern() {
            write!(f, "{}", self.linkage_name)?;
        } else {
            write!(f, "{}", self.name)?;
        }
        f.write_str("(")?;
        for (i, (pname, pty)) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{pname}: {pty}")?;
        }
        write!(f, ") -> {}", self.return_type)?;
        if self.is_extern() {
            return Ok(());
        }
        f.write_str(" {\n")?;
        for block in &self.blocks {
            writeln!(f, "{block}")?;
        }
        f.write_str("  }")
    }
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "module {} {{", self.program_name)?;
        for func in &self.functions {
            writeln!(f, "{func}")?;
        }
        f.write_str("}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extern_write_line() -> Function {
        Function {
            name: "write_line".to_string(),
            linkage_name: "coddl_write_line".to_string(),
            params: vec![("message".to_string(), ProcType::Text)],
            return_type: ProcType::Unit,
            blocks: Vec::new(),
        }
    }

    fn defined_main() -> Function {
        Function {
            name: "main".to_string(),
            linkage_name: "main".to_string(),
            params: Vec::new(),
            return_type: ProcType::Unit,
            blocks: vec![BasicBlock {
                id: BlockId(0),
                insts: vec![
                    Inst::Const {
                        dst: ValueId(0),
                        value: Const::Text(b"Hello, world!".to_vec()),
                        ty: ProcType::Text,
                    },
                    Inst::Call {
                        dst: None,
                        callee: "coddl_write_line".to_string(),
                        args: vec![ValueId(0)],
                        return_type: ProcType::Unit,
                    },
                ],
                terminator: Terminator::Return(None),
            }],
        }
    }

    #[test]
    fn module_display_round_trips_simple_extern() {
        let m = Module {
            program_name: "hello_world".to_string(),
            functions: vec![extern_write_line()],
        };
        let text = format!("{m}");
        assert!(text.starts_with("module hello_world {"));
        assert!(text.contains("extern fn coddl_write_line(message: Text) -> Unit"));
        assert!(text.ends_with("}"));
    }

    #[test]
    fn module_display_includes_basic_block_label() {
        let m = Module {
            program_name: "hello_world".to_string(),
            functions: vec![extern_write_line(), defined_main()],
        };
        let text = format!("{m}");
        assert!(text.contains("block_0:"), "no block label in:\n{text}");
        assert!(text.contains("%0 = const Text \"Hello, world!\""));
        assert!(text.contains("call coddl_write_line(%0)"));
        assert!(text.contains("return"));
    }

    #[test]
    fn value_id_renders_with_percent_prefix() {
        assert_eq!(ValueId(0).to_string(), "%0");
        assert_eq!(ValueId(42).to_string(), "%42");
    }

    #[test]
    fn proctype_display_covers_all_variants() {
        // Match force: if a variant is added without a Display arm,
        // this match becomes non-exhaustive and the test stops
        // compiling.
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
            ProcType::Tuple(Heading::empty()),
        ] {
            let s = ty.to_string();
            assert!(!s.is_empty());
            assert!(s.chars().next().unwrap().is_ascii_uppercase());
        }
    }

    #[test]
    fn inst_display_call_with_args() {
        let inst = Inst::Call {
            dst: Some(ValueId(2)),
            callee: "do_thing".to_string(),
            args: vec![ValueId(0), ValueId(1)],
            return_type: ProcType::Integer,
        };
        assert_eq!(inst.to_string(), "%2 = call do_thing(%0, %1)");

        let void_call = Inst::Call {
            dst: None,
            callee: "noop".to_string(),
            args: Vec::new(),
            return_type: ProcType::Unit,
        };
        assert_eq!(void_call.to_string(), "call noop()");
    }
}

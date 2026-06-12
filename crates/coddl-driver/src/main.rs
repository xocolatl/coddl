//! `coddl` — the command-line driver.

use std::io::{self, Read, Write};
use std::process::ExitCode;

use coddl_diagnostics::FileId;
use coddl_syntax::{SyntaxElement, SyntaxNode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("coddl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("lex") => cmd_lex(&args[2..]),
        Some("parse") => cmd_parse(&args[2..]),
        Some("check") => cmd_check(&args[2..]),
        Some("lower") => cmd_lower(&args[2..]),
        Some("emit-llvm") => cmd_emit_llvm(&args[2..]),
        Some("emit-obj") => cmd_emit_obj(&args[2..]),
        Some("fmt") => cmd_fmt(&args[2..]),
        _ => {
            eprintln!("usage: coddl <subcommand> [args]");
            eprintln!();
            eprintln!("subcommands:");
            eprintln!("  lex <file>           run the lexer on <file> (or stdin if -)");
            eprintln!("  parse <file>         parse <file> and dump the syntax tree");
            eprintln!("  check <file>         typecheck <file> (or stdin if -)");
            eprintln!("  lower <file>         lower <file> to ProcIR and dump it");
            eprintln!("  emit-llvm <file>     emit LLVM IR text for <file>");
            eprintln!("  emit-obj <file>      emit a native object file via Cranelift");
            eprintln!("                       [-o <path>] writes to <path> (default stdout)");
            eprintln!("  fmt <file>           run the formatter on <file> (or stdin if -)");
            eprintln!("  --version            print version");
            ExitCode::from(2)
        }
    }
}

fn read_input(args: &[String], cmd: &str) -> Option<String> {
    match args.first().map(String::as_str) {
        Some("-") | None => {
            let mut buf = String::new();
            if let Err(err) = io::stdin().read_to_string(&mut buf) {
                eprintln!("coddl {cmd}: read stdin: {err}");
                return None;
            }
            Some(buf)
        }
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(err) => {
                eprintln!("coddl {cmd}: read {path}: {err}");
                None
            }
        },
    }
}

fn cmd_lex(args: &[String]) -> ExitCode {
    let Some(source) = read_input(args, "lex") else {
        return ExitCode::from(1);
    };

    let out = coddl_syntax::lex(&source, FileId(0));

    let stdout = io::stdout();
    let mut w = stdout.lock();

    for tok in &out.tokens {
        let lexeme = &source[tok.span.start as usize..tok.span.end as usize];
        // Compact debug-style display: kind padded, byte range, lexeme.
        let _ = writeln!(
            w,
            "{:<16} {:>5}..{:<5} {}",
            format!("{:?}", tok.kind),
            tok.span.start,
            tok.span.end,
            DisplayLexeme(lexeme),
        );
    }

    if !out.diagnostics.is_empty() {
        for d in &out.diagnostics {
            eprintln!(
                "{}: {} [{}] at {}..{}",
                d.severity, d.message, d.code, d.span.start, d.span.end
            );
        }
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Compact display for a token lexeme — escapes control chars and clips
/// to one line so the token table stays scannable.
struct DisplayLexeme<'a>(&'a str);

impl std::fmt::Display for DisplayLexeme<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write as _;
        f.write_str("\"")?;
        for c in self.0.chars() {
            match c {
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                c if c.is_control() => write!(f, "\\u{{{:x}}}", c as u32)?,
                c => f.write_char(c)?,
            }
        }
        f.write_str("\"")
    }
}

fn cmd_parse(args: &[String]) -> ExitCode {
    let Some(source) = read_input(args, "parse") else {
        return ExitCode::from(1);
    };

    let out = coddl_syntax::parse(&source, FileId(0));

    let stdout = io::stdout();
    let mut w = stdout.lock();
    dump_node(&mut w, &out.tree, &source, 0);

    if !out.diagnostics.is_empty() {
        for d in &out.diagnostics {
            eprintln!(
                "{}: {} [{}] at {}..{}",
                d.severity, d.message, d.code, d.span.start, d.span.end
            );
        }
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Pretty-print a syntax tree in the rust-analyzer style:
/// `KIND@start..end "lexeme"` for tokens, recursively indented for nodes.
fn dump_node(w: &mut impl Write, node: &SyntaxNode, source: &str, indent: usize) {
    let range = node.text_range();
    let _ = writeln!(
        w,
        "{:indent$}{:?}@{}..{}",
        "",
        node.kind(),
        usize::from(range.start()),
        usize::from(range.end()),
        indent = indent * 2,
    );
    for child in node.children_with_tokens() {
        match child {
            SyntaxElement::Node(n) => dump_node(w, &n, source, indent + 1),
            SyntaxElement::Token(t) => {
                let r = t.text_range();
                let lexeme = &source[usize::from(r.start())..usize::from(r.end())];
                let _ = writeln!(
                    w,
                    "{:indent$}{:?}@{}..{} {}",
                    "",
                    t.kind(),
                    usize::from(r.start()),
                    usize::from(r.end()),
                    DisplayLexeme(lexeme),
                    indent = (indent + 1) * 2,
                );
            }
        }
    }
}

fn cmd_check(args: &[String]) -> ExitCode {
    let Some(source) = read_input(args, "check") else {
        return ExitCode::from(1);
    };

    let out = coddl_types::check(&source, FileId(0));

    if out.diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        for d in &out.diagnostics {
            eprintln!(
                "{}: {} [{}] at {}..{}",
                d.severity, d.message, d.code, d.span.start, d.span.end
            );
        }
        ExitCode::from(1)
    }
}

fn cmd_lower(args: &[String]) -> ExitCode {
    let Some(source) = read_input(args, "lower") else {
        return ExitCode::from(1);
    };

    let out = coddl_procir::lower(&source, FileId(0));

    if let Some(module) = &out.module {
        let stdout = io::stdout();
        let mut w = stdout.lock();
        let _ = writeln!(w, "{module}");
    }

    if out.diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        for d in &out.diagnostics {
            eprintln!(
                "{}: {} [{}] at {}..{}",
                d.severity, d.message, d.code, d.span.start, d.span.end
            );
        }
        ExitCode::from(1)
    }
}

fn cmd_emit_llvm(args: &[String]) -> ExitCode {
    let Some(source) = read_input(args, "emit-llvm") else {
        return ExitCode::from(1);
    };

    let lower_out = coddl_procir::lower(&source, FileId(0));
    for d in &lower_out.diagnostics {
        eprintln!(
            "{}: {} [{}] at {}..{}",
            d.severity, d.message, d.code, d.span.start, d.span.end
        );
    }
    let Some(module) = lower_out.module else {
        return ExitCode::from(1);
    };

    let mut backend = coddl_codegen_llvm::LlvmBackend::new();
    use coddl_procir::Codegen as _;
    match backend.emit(&module) {
        Ok(text) => {
            let stdout = io::stdout();
            let mut w = stdout.lock();
            if w.write_all(text.as_bytes()).is_err() {
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("coddl emit-llvm: {err}");
            ExitCode::from(1)
        }
    }
}

fn cmd_emit_obj(args: &[String]) -> ExitCode {
    // Parse an optional `-o <path>` from `args`. The remaining positional
    // is the input file (or stdin via `-`).
    let mut out_path: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                if i + 1 >= args.len() {
                    eprintln!("coddl emit-obj: `-o` requires a path argument");
                    return ExitCode::from(2);
                }
                out_path = Some(args[i + 1].clone());
                i += 2;
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    let Some(source) = read_input(&positional, "emit-obj") else {
        return ExitCode::from(1);
    };

    let lower_out = coddl_procir::lower(&source, FileId(0));
    for d in &lower_out.diagnostics {
        eprintln!(
            "{}: {} [{}] at {}..{}",
            d.severity, d.message, d.code, d.span.start, d.span.end
        );
    }
    let Some(module) = lower_out.module else {
        return ExitCode::from(1);
    };

    let mut backend = match coddl_codegen_cranelift::CraneliftBackend::new() {
        Ok(b) => b,
        Err(err) => {
            eprintln!("coddl emit-obj: {err}");
            return ExitCode::from(1);
        }
    };

    use coddl_procir::Codegen as _;
    let bytes = match backend.emit(&module) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("coddl emit-obj: {err}");
            return ExitCode::from(1);
        }
    };

    match out_path {
        Some(path) => {
            if let Err(err) = std::fs::write(&path, &bytes) {
                eprintln!("coddl emit-obj: write {path}: {err}");
                return ExitCode::from(1);
            }
        }
        None => {
            let stdout = io::stdout();
            let mut w = stdout.lock();
            if w.write_all(&bytes).is_err() {
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

fn cmd_fmt(args: &[String]) -> ExitCode {
    let Some(source) = read_input(args, "fmt") else {
        return ExitCode::from(1);
    };

    let opts = coddl_fmt::FormatOptions::default();
    let out = coddl_fmt::format(&source, &opts);
    if io::stdout().write_all(out.text.as_bytes()).is_err() {
        return ExitCode::from(1);
    }
    if out.diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        for d in &out.diagnostics {
            eprintln!("{}: {} [{}]", d.severity, d.message, d.code);
        }
        ExitCode::from(1)
    }
}

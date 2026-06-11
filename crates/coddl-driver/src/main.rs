//! `coddl` — the command-line driver.
//!
//! Subcommands implemented today:
//! - `lex <file>`  — run the lexer on a file and print the token stream.
//! - `fmt <file>`  — run the formatter (stub passthrough until rules land).
//! - `--version`   — print the build version.
//!
//! Planned: `parse`, `check`, `compile`, `run`, `repl`.

use std::io::{self, Read, Write};
use std::process::ExitCode;

use coddl_diagnostics::FileId;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("coddl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("lex") => cmd_lex(&args[2..]),
        Some("fmt") => cmd_fmt(&args[2..]),
        _ => {
            eprintln!("coddl: skeleton driver");
            eprintln!();
            eprintln!("usage: coddl <subcommand> [args]");
            eprintln!();
            eprintln!("subcommands:");
            eprintln!("  lex <file>     run the lexer on <file> (or stdin if -)");
            eprintln!("  fmt <file>     run the formatter on <file> (or stdin if -)");
            eprintln!("  --version      print version");
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

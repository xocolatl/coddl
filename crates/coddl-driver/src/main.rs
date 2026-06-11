//! `coddl` — the command-line driver.
//!
//! Subcommands (planned): `compile`, `run`, `repl`, `fmt`.
//! For now most are placeholders; `fmt` is wired through to `coddl-fmt`
//! end-to-end so the formatter library can be exercised before the
//! parser/CST land.

use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("coddl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("fmt") => cmd_fmt(&args[2..]),
        _ => {
            eprintln!("coddl: skeleton driver — subcommands not yet implemented");
            eprintln!("usage: coddl [--version | fmt [-]]");
            eprintln!("args: {:?}", &args[1..]);
            ExitCode::from(2)
        }
    }
}

fn cmd_fmt(args: &[String]) -> ExitCode {
    let opts = coddl_fmt::FormatOptions::default();

    let source = match args.first().map(String::as_str) {
        Some("-") | None => {
            let mut buf = String::new();
            if let Err(err) = io::stdin().read_to_string(&mut buf) {
                eprintln!("coddl fmt: read stdin: {err}");
                return ExitCode::from(1);
            }
            buf
        }
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("coddl fmt: read {path}: {err}");
                return ExitCode::from(1);
            }
        },
    };

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

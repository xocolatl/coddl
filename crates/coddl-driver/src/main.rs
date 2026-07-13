//! `coddl` — the command-line driver.

mod link;
mod runtime;

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use coddl_diagnostics::{Diagnostic, FileId, Severity};
use coddl_syntax::{FileKind, SyntaxElement, SyntaxNode};

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
        Some("plan") => cmd_plan(&args[2..]),
        Some("lower") => cmd_lower(&args[2..]),
        Some("explain") => cmd_explain(&args[2..]),
        Some("emit-llvm") => cmd_emit_llvm(&args[2..]),
        Some("emit-obj") => cmd_emit_obj(&args[2..]),
        Some("compile") => cmd_compile(&args[2..]),
        Some("run") => cmd_run(&args[2..]),
        Some("fmt") => cmd_fmt(&args[2..]),
        _ => {
            eprintln!("usage: coddl <subcommand> [args]");
            eprintln!();
            eprintln!("subcommands:");
            eprintln!("  lex <file>           run the lexer on <file> (or stdin if -)");
            eprintln!("  parse <file>         parse <file> and dump the syntax tree");
            eprintln!("  check <file>         typecheck <file> (or stdin if -);");
            eprintln!("                       cross-validates companions when <file>.cd");
            eprintln!("                       declares public relvars");
            eprintln!("  plan <file>          discover .cd companions, validate the chain,");
            eprintln!("                       dump the resolved Plan");
            eprintln!("  lower <file>         lower <file> to ProcIR and dump it");
            eprintln!("  explain <file>       dump the as-lowered RelIR for each");
            eprintln!("                       relational expression pushed to SQL");
            eprintln!("  emit-llvm <file>     emit LLVM IR text for <file>");
            eprintln!("  emit-obj <file>      emit a native object file via Cranelift");
            eprintln!("                       [-o <path>] writes to <path> (default stdout)");
            eprintln!("  compile <file>       compile <file> to a native binary");
            eprintln!("                       [--backend=llvm|cranelift] (default llvm)");
            eprintln!("                       [-o <path>] (default <basename> in CWD)");
            eprintln!("  run <file>           compile + run <file>, propagating exit code");
            eprintln!("                       [--backend=llvm|cranelift] (default cranelift)");
            eprintln!("  fmt <file>           run the formatter on <file> (or stdin if -)");
            eprintln!("                       [--check] verify formatting (exit 1 if not),");
            eprintln!("                       [--write] reformat the file in place");
            eprintln!("  --version            print version");
            ExitCode::from(2)
        }
    }
}

/// Read source from `args` (stdin or a file path) and decide which
/// dialect it belongs to. Stdin and unrecognized extensions default to
/// [`FileKind::Cd`]; the caller can choose to reject this with
/// [`require_cd`] when the downstream pipeline doesn't yet support
/// dialect input.
fn read_input(args: &[String], cmd: &str) -> Option<(String, FileKind)> {
    match args.first().map(String::as_str) {
        Some("-") | None => {
            let mut buf = String::new();
            if let Err(err) = io::stdin().read_to_string(&mut buf) {
                eprintln!("coddl {cmd}: read stdin: {err}");
                return None;
            }
            Some((buf, FileKind::Cd))
        }
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => {
                let kind = FileKind::from_path(Path::new(path)).unwrap_or(FileKind::Cd);
                Some((s, kind))
            }
            Err(err) => {
                eprintln!("coddl {cmd}: read {path}: {err}");
                None
            }
        },
    }
}

/// Reject input that isn't `.cd`. Used by every subcommand whose
/// downstream pipeline (typecheck / lower / emit / compile / run /
/// fmt) is `.cd`-only today; the dialect-aware pipeline lands in
/// later phases.
fn require_cd(kind: FileKind, cmd: &str) -> Result<(), ExitCode> {
    if kind == FileKind::Cd {
        Ok(())
    } else {
        eprintln!(
            "coddl {cmd}: only accepts .cd files today; \
             .{ext} pipeline support lands in later phases",
            ext = kind.extension(),
        );
        Err(ExitCode::from(2))
    }
}

/// `run` / `compile` produce an executable, so the input must be a `program`.
/// A `library` / `module` is a valid compilation unit but has no entry point,
/// so reject it with a usage error (exit 2) that points at `emit-obj`. A
/// missing/malformed header falls through — it surfaces as a PL0012 error in
/// the plan pass, which bails with exit 1.
fn require_program(source: &str, cmd: &str) -> Result<(), ExitCode> {
    use coddl_syntax::ast::{AstNode, Item, Root};
    let parsed = coddl_syntax::parse(source, FileId(0), FileKind::Cd);
    let header_kind = Root::cast(parsed.tree).and_then(|r| {
        r.items().find_map(|it| match it {
            Item::ProgramDecl(d) => d.kind().map(|t| t.text().to_string()),
            _ => None,
        })
    });
    if let Some(k) = header_kind.as_deref() {
        if k == "library" || k == "module" {
            eprintln!(
                "coddl {cmd}: a `{k}` is not executable; only a `program` can be run or \
                 compiled. Use `coddl emit-obj` to produce an object for a foreign host.",
            );
            return Err(ExitCode::from(2));
        }
    }
    Ok(())
}

fn cmd_lex(args: &[String]) -> ExitCode {
    let Some((source, _kind)) = read_input(args, "lex") else {
        return ExitCode::from(1);
    };

    // The lexer is dialect-agnostic — no FileKind plumbing needed.
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
    let Some((source, kind)) = read_input(args, "parse") else {
        return ExitCode::from(1);
    };

    let out = coddl_syntax::parse(&source, FileId(0), kind);

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
    let Some((source, kind)) = read_input(args, "check") else {
        return ExitCode::from(1);
    };

    // For a `.cd` **file path**, the plan pass is authoritative: it runs the
    // full multi-unit check (the entry plus every imported module, so
    // cross-module calls resolve), the compilation-unit header rules
    // (PL0012–PL0015), the module-resolution rules (PL0016–PL0019), and
    // companion cross-validation. Use its diagnostics wholesale. Stdin and
    // non-`.cd` inputs have no path to anchor discovery, so they fall back to a
    // single-file check (which can't see `use module` imports).
    let (diagnostics, source_map): (Vec<Diagnostic>, Vec<PathBuf>) =
        match (kind, args.first().filter(|s| s.as_str() != "-")) {
            (FileKind::Cd, Some(path)) => {
                let out = coddl_plan::discover_and_validate(Path::new(path));
                let map = build_source_map(Path::new(path), &out.module_graph);
                (out.diagnostics, map)
            }
            _ => (
                coddl_types::check(&source, FileId(0), kind).diagnostics,
                Vec::new(),
            ),
        };

    if diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        print_diagnostics(&diagnostics, &source_map);
        ExitCode::from(1)
    }
}

fn cmd_plan(args: &[String]) -> ExitCode {
    let path = match args.first().map(String::as_str) {
        Some("-") | None => {
            eprintln!("coddl plan: requires a `.cd` file path (stdin is unsupported)");
            return ExitCode::from(2);
        }
        Some(p) => PathBuf::from(p),
    };

    let kind = FileKind::from_path(&path).unwrap_or(FileKind::Cd);
    if let Err(code) = require_cd(kind, "plan") {
        return code;
    }

    let out = coddl_plan::discover_and_validate(&path);

    if let Some(plan) = &out.plan {
        let stdout = io::stdout();
        let mut w = stdout.lock();
        let _ = writeln!(w, "program: {}", plan.program_name);
        let _ = writeln!(
            w,
            "database: {}",
            plan.database_name.as_deref().unwrap_or("(none)")
        );
        let _ = writeln!(w, "backend: {:?}", plan.backend_kind);
        let _ = writeln!(w, "resolved ({}):", plan.resolved.len());
        for r in &plan.resolved {
            let _ = writeln!(
                w,
                "  {} → {} (table {:?}, {:?})",
                r.app_name, r.catalog_name, r.table_name, r.write_policy
            );
            for (attr, col) in &r.columns {
                let _ = writeln!(w, "    {attr}: {col:?}");
            }
        }
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
        // Any error severity = exit 1.
        let has_error = out
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error);
        if has_error {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        }
    }
}

fn cmd_lower(args: &[String]) -> ExitCode {
    let Some((source, kind)) = read_input(args, "lower") else {
        return ExitCode::from(1);
    };
    if let Err(code) = require_cd(kind, "lower") {
        return code;
    }
    let plan = discover_plan_for_input(args);
    let out = coddl_procir::lower_with_plan(&source, FileId(0), plan.as_ref());

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

/// `coddl explain <file>` — dump the as-lowered RelIR for each relational
/// expression the cut pushes to SQL, paired with the SQL it became. This is the
/// logical (RelIR) view of the program's queries, not an optimized plan: there
/// is no optimizer yet, and the tree is the hybrid A-core + sugar form, not
/// minimal Algebra A. In-process relational evaluation is not covered.
fn cmd_explain(args: &[String]) -> ExitCode {
    let Some((source, kind)) = read_input(args, "explain") else {
        return ExitCode::from(1);
    };
    if let Err(code) = require_cd(kind, "explain") {
        return code;
    }
    let plan = discover_plan_for_input(args);
    let out = coddl_procir::explain_with_plan(&source, FileId(0), plan.as_ref());

    // `module` is `Some` exactly when the program is free of error-severity
    // diagnostics (same gate `lower` uses); only then is the RelIR meaningful.
    if out.module.is_some() {
        let stdout = io::stdout();
        let mut w = stdout.lock();
        if out.relir.is_empty() {
            let _ = writeln!(w, "no relational expressions were pushed to SQL");
        } else {
            let _ = writeln!(
                w,
                "as-lowered RelIR for {} relational expression(s) pushed to SQL:",
                out.relir.len()
            );
            for (i, entry) in out.relir.iter().enumerate() {
                let _ = writeln!(w);
                let _ = writeln!(w, "query {}:", i + 1);
                let _ = writeln!(w, "  RelIR:");
                for line in entry.expr.render().lines() {
                    let _ = writeln!(w, "    {line}");
                }
                let _ = writeln!(w, "  SQL:");
                let _ = writeln!(w, "    {}", entry.sql);
                // A root `matching` over a shipped relation bakes a second
                // plan: the runtime fires this specialized form instead when
                // the shipped relation holds exactly one row.
                if let Some(card1) = &entry.card1_sql {
                    let _ = writeln!(w, "  SQL (card-1 dispatch):");
                    let _ = writeln!(w, "    {card1}");
                }
            }
        }
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
    let Some((source, kind)) = read_input(args, "emit-llvm") else {
        return ExitCode::from(1);
    };
    if let Err(code) = require_cd(kind, "emit-llvm") {
        return code;
    }

    let plan = discover_plan_for_input(args);
    let lower_out = coddl_procir::lower_with_plan(&source, FileId(0), plan.as_ref());
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

    let Some((source, kind)) = read_input(&positional, "emit-obj") else {
        return ExitCode::from(1);
    };
    if let Err(code) = require_cd(kind, "emit-obj") {
        return code;
    }

    let plan = discover_plan_for_input(&positional);
    let lower_out = coddl_procir::lower_with_plan(&source, FileId(0), plan.as_ref());
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

/// Which backend a `compile` or `run` invocation should use. Defaults
/// differ per subcommand: `compile` defaults to LLVM (optimized
/// AOT), `run` to Cranelift (fast iteration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Llvm,
    Cranelift,
}

impl Backend {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "llvm" => Ok(Backend::Llvm),
            "cranelift" => Ok(Backend::Cranelift),
            other => Err(format!(
                "unknown backend `{other}` (expected `llvm` or `cranelift`)"
            )),
        }
    }
}

/// Parse the `--backend=<name>` and `-o <path>` flags out of an
/// argument list. Whatever isn't a known flag becomes a positional
/// argument; the caller decides what to do with positionals.
struct CompileArgs {
    backend: Option<Backend>,
    output: Option<String>,
    positional: Vec<String>,
}

fn parse_compile_args(args: &[String], cmd: &str) -> Result<CompileArgs, ExitCode> {
    let mut backend: Option<Backend> = None;
    let mut output: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(value) = arg.strip_prefix("--backend=") {
            match Backend::parse(value) {
                Ok(b) => backend = Some(b),
                Err(msg) => {
                    eprintln!("coddl {cmd}: {msg}");
                    return Err(ExitCode::from(2));
                }
            }
            i += 1;
        } else if arg == "-o" {
            if i + 1 >= args.len() {
                eprintln!("coddl {cmd}: `-o` requires a path argument");
                return Err(ExitCode::from(2));
            }
            output = Some(args[i + 1].clone());
            i += 2;
        } else {
            positional.push(arg.clone());
            i += 1;
        }
    }

    Ok(CompileArgs {
        backend,
        output,
        positional,
    })
}

fn print_diagnostics(diagnostics: &[Diagnostic], source_map: &[PathBuf]) {
    for d in diagnostics {
        // A multi-unit program tags each diagnostic with its unit's `FileId`;
        // the source map turns that back into a file name so an error inside an
        // imported module reports against the module, not the entry file.
        match source_map
            .get(d.span.file.0 as usize)
            .filter(|p| !p.as_os_str().is_empty())
        {
            Some(path) => eprintln!(
                "{}: {} [{}] at {}:{}..{}",
                d.severity,
                d.message,
                d.code,
                path.display(),
                d.span.start,
                d.span.end
            ),
            _ => eprintln!(
                "{}: {} [{}] at {}..{}",
                d.severity, d.message, d.code, d.span.start, d.span.end
            ),
        }
    }
}

/// FileId → path, matching the plan layer's allocation: `0` = the entry `.cd`,
/// `1`/`2` reserved for the `.cddb`/`.cdstore` companions (left blank here —
/// their diagnostics still print without a name), `3..` = the imported modules
/// in graph order. An out-of-range or blank entry falls back to bare offsets.
fn build_source_map(cd_path: &Path, graph: &coddl_plan::ModuleGraph) -> Vec<PathBuf> {
    let mut map = vec![cd_path.to_path_buf(), PathBuf::new(), PathBuf::new()];
    for m in &graph.modules {
        map.push(m.file.clone());
    }
    map
}

/// Discover the Phase 16 plan for a `.cd` file input, if the first
/// positional argument names one. Stdin and other non-path inputs
/// return `None`. Plan diagnostics print to stderr; the caller can
/// still decide whether to bail.
fn discover_plan_for_input(positional: &[String]) -> Option<coddl_plan::Plan> {
    let cd_path = positional
        .first()
        .filter(|s| s.as_str() != "-")
        .map(PathBuf::from)?;
    let out = coddl_plan::discover_and_validate(&cd_path);
    let source_map = build_source_map(&cd_path, &out.module_graph);
    print_diagnostics(&out.diagnostics, &source_map);
    if out
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error)
    {
        return None;
    }
    out.plan
}

/// Lower `source` to ProcIR. Returns `None` if any error diagnostic
/// was emitted; diagnostics print to stderr unconditionally.
///
/// When `cd_path` is `Some` (`.cd` input came from a file path, not
/// stdin), the driver first runs Phase 16 plan discovery against the
/// companion `.cddb` / `.cdstore`. Plan diagnostics flow through the
/// same channel; on success, the resolved `Plan` is passed to
/// `lower_with_plan` so public-relvar references resolve and `main`
/// gets per-relvar init/release wrapping. Stdin and standalone (no
/// public relvars) inputs go through the legacy plan-less path.
fn lower_or_bail(source: &str, cd_path: Option<&Path>) -> Option<coddl_procir::Module> {
    // The `.cd` is typechecked in both the plan pass (`discover_and_validate`)
    // and lowering (`lower_with_plan`), so its diagnostics surface in both.
    // Remember the plan pass's set and, after lowering, print only the
    // diagnostics it didn't already show — otherwise every `.cd` diagnostic
    // (error or warning) would report twice.
    let mut plan_diags: Vec<Diagnostic> = Vec::new();
    let mut source_map: Vec<PathBuf> = Vec::new();
    let plan = if let Some(path) = cd_path {
        let plan_out = coddl_plan::discover_and_validate(path);
        source_map = build_source_map(path, &plan_out.module_graph);
        print_diagnostics(&plan_out.diagnostics, &source_map);
        if plan_out
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
        {
            return None;
        }
        plan_diags = plan_out.diagnostics;
        plan_out.plan
    } else {
        None
    };
    let out = coddl_procir::lower_with_plan(source, FileId(0), plan.as_ref());
    let fresh: Vec<Diagnostic> = out
        .diagnostics
        .iter()
        .filter(|&d| !plan_diags.contains(d))
        .cloned()
        .collect();
    print_diagnostics(&fresh, &source_map);
    if out
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error)
    {
        return None;
    }
    out.module
}

/// Build the binary for `module` at `output_path` using `backend`,
/// using `scratch` for intermediate artifacts.
fn build_binary(
    module: &coddl_procir::Module,
    backend: Backend,
    output_path: &Path,
    runtime: &Path,
    scratch: &tempfile::TempDir,
    cmd: &str,
) -> Result<(), ExitCode> {
    use coddl_procir::Codegen as _;
    match backend {
        Backend::Llvm => {
            let mut be = coddl_codegen_llvm::LlvmBackend::new();
            let ir = be.emit(module).map_err(|err| {
                eprintln!("coddl {cmd}: {err}");
                ExitCode::from(1)
            })?;
            link::link_llvm_ir(&ir, output_path, runtime, scratch).map_err(|err| {
                eprintln!("coddl {cmd}: {err}");
                ExitCode::from(1)
            })
        }
        Backend::Cranelift => {
            let mut be = coddl_codegen_cranelift::CraneliftBackend::new().map_err(|err| {
                eprintln!("coddl {cmd}: {err}");
                ExitCode::from(1)
            })?;
            let obj = be.emit(module).map_err(|err| {
                eprintln!("coddl {cmd}: {err}");
                ExitCode::from(1)
            })?;
            link::link_cranelift_object(&obj, output_path, runtime, scratch).map_err(|err| {
                eprintln!("coddl {cmd}: {err}");
                ExitCode::from(1)
            })
        }
    }
}

fn cmd_compile(args: &[String]) -> ExitCode {
    let parsed = match parse_compile_args(args, "compile") {
        Ok(p) => p,
        Err(code) => return code,
    };
    let backend = parsed.backend.unwrap_or(Backend::Llvm);

    let Some((source, kind)) = read_input(&parsed.positional, "compile") else {
        return ExitCode::from(1);
    };
    if let Err(code) = require_cd(kind, "compile") {
        return code;
    }
    if let Err(code) = require_program(&source, "compile") {
        return code;
    }
    let cd_path = parsed
        .positional
        .first()
        .filter(|s| s.as_str() != "-")
        .map(PathBuf::from);
    let Some(module) = lower_or_bail(&source, cd_path.as_deref()) else {
        return ExitCode::from(1);
    };

    let output_path = match parsed.output {
        Some(p) => PathBuf::from(p),
        None => match parsed.positional.first().map(String::as_str) {
            Some(path) if path != "-" => {
                let stem = Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("a.out");
                PathBuf::from(stem)
            }
            _ => {
                eprintln!("coddl compile: stdin input requires `-o <path>`");
                return ExitCode::from(2);
            }
        },
    };

    let runtime = match runtime::discover() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("coddl compile: {err}");
            return ExitCode::from(1);
        }
    };

    let scratch = match tempfile::tempdir() {
        Ok(t) => t,
        Err(err) => {
            eprintln!("coddl compile: tempdir: {err}");
            return ExitCode::from(1);
        }
    };

    match build_binary(
        &module,
        backend,
        &output_path,
        &runtime,
        &scratch,
        "compile",
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

fn cmd_run(args: &[String]) -> ExitCode {
    let parsed = match parse_compile_args(args, "run") {
        Ok(p) => p,
        Err(code) => return code,
    };
    let backend = parsed.backend.unwrap_or(Backend::Cranelift);
    if parsed.output.is_some() {
        eprintln!("coddl run: `-o` is not accepted; use `coddl compile` to write a binary");
        return ExitCode::from(2);
    }

    let Some((source, kind)) = read_input(&parsed.positional, "run") else {
        return ExitCode::from(1);
    };
    if let Err(code) = require_cd(kind, "run") {
        return code;
    }
    if let Err(code) = require_program(&source, "run") {
        return code;
    }
    let cd_path = parsed
        .positional
        .first()
        .filter(|s| s.as_str() != "-")
        .map(PathBuf::from);
    let Some(module) = lower_or_bail(&source, cd_path.as_deref()) else {
        return ExitCode::from(1);
    };

    let runtime = match runtime::discover() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("coddl run: {err}");
            return ExitCode::from(1);
        }
    };

    let scratch = match tempfile::tempdir() {
        Ok(t) => t,
        Err(err) => {
            eprintln!("coddl run: tempdir: {err}");
            return ExitCode::from(1);
        }
    };
    let binary = scratch.path().join("coddl_run");

    if let Err(code) = build_binary(&module, backend, &binary, &runtime, &scratch, "run") {
        return code;
    }

    let status = match Command::new(&binary).status() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("coddl run: spawn {}: {err}", binary.display());
            return ExitCode::from(1);
        }
    };
    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::from(128), // killed by signal
    }
}

fn cmd_fmt(args: &[String]) -> ExitCode {
    // Modes: default writes the formatted text to stdout; `--check` verifies the
    // input is already formatted (exit 1 if not, no output); `--write` rewrites
    // the file in place. `--check`/`--write` back the git pre-commit hook and a
    // future formatter-improvement sweep respectively.
    let mut check = false;
    let mut write = false;
    let mut positional: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            "--write" => write = true,
            _ => positional.push(a.clone()),
        }
    }
    if check && write {
        eprintln!("coddl fmt: --check and --write are mutually exclusive");
        return ExitCode::from(2);
    }

    let Some((source, kind)) = read_input(&positional, "fmt") else {
        return ExitCode::from(1);
    };
    if let Err(code) = require_cd(kind, "fmt") {
        return code;
    }

    let opts = coddl_fmt::FormatOptions::default();
    let out = coddl_fmt::format(&source, &opts, kind);

    // The path label (`None` for stdin) — used in messages and as the `--write`
    // target.
    let path = positional.first().map(String::as_str).filter(|s| *s != "-");

    // `--check` / `--write` refuse to act on input the formatter couldn't parse
    // cleanly: reformatting around a parse error could drop or mangle bytes.
    if (check || write) && !out.diagnostics.is_empty() {
        for d in &out.diagnostics {
            eprintln!("{}: {} [{}]", d.severity, d.message, d.code);
        }
        return ExitCode::from(1);
    }

    if check {
        if out.text == source {
            return ExitCode::SUCCESS;
        }
        match path {
            Some(p) => eprintln!("coddl fmt: {p} is not formatted (run `coddl fmt --write {p}`)"),
            None => eprintln!("coddl fmt: input is not formatted"),
        }
        return ExitCode::from(1);
    }

    if write {
        let Some(p) = path else {
            eprintln!("coddl fmt: --write needs a file path (cannot rewrite stdin)");
            return ExitCode::from(2);
        };
        if out.text != source {
            if let Err(err) = std::fs::write(p, out.text.as_bytes()) {
                eprintln!("coddl fmt: write {p}: {err}");
                return ExitCode::from(1);
            }
        }
        return ExitCode::SUCCESS;
    }

    // Default: emit the formatted text to stdout. Parse-error diagnostics still
    // surface and fail, but the (best-effort) formatted text is written first.
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

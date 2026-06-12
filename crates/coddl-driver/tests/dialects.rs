//! Integration tests for the `.cddb` / `.cdmap` / `.cdstore` dialect
//! parsers wired through the `coddl` driver.
//!
//! Each test invokes the built `coddl` binary against one of the
//! companion files in `examples/hello-world-db/` and asserts that the
//! parser produces a clean tree (zero diagnostics) and that
//! `coddl check` rejects dialect input with a clear error.

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn companion_path(name: &str) -> PathBuf {
    workspace_root().join(format!("examples/hello-world-db/{name}"))
}

fn coddl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_coddl"))
}

#[test]
fn coddl_parse_cddb_round_trips_example() {
    let out = coddl()
        .args(["parse"])
        .arg(companion_path("greetings.cddb"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl parse <.cddb> failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("CDDB_ROOT@"),
        "expected CST dump to start with CDDB_ROOT, got: {}",
        stdout.lines().next().unwrap_or("")
    );
}

// `.cdmap` integration test omitted: hello-world-db is identity-mapped
// (the catalog's relvars are reused under the same names), so the
// example doesn't ship a `.cdmap` file. The `.cdmap` parser is
// exercised thoroughly by the unit tests in
// `coddl_syntax::parser_cdmap`; an end-to-end driver test against an
// example file lands when a non-identity adapter example appears
// (Phase 16+).

#[test]
fn coddl_parse_cdmap_stdin_round_trips() {
    // Confirm the driver dispatches `.cdmap` correctly when fed an
    // explicit path — using a tempfile since the project doesn't ship
    // a `.cdmap` example today.
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::with_suffix(".cdmap").expect("tempfile");
    writeln!(tmp, "map myapp to mydb;\nGreetings = Greetings;").expect("write");
    let path = tmp.into_temp_path();

    let out = coddl()
        .args(["parse"])
        .arg(&path)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl parse <.cdmap> failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("CDMAP_ROOT@"),
        "expected CST dump to start with CDMAP_ROOT, got: {}",
        stdout.lines().next().unwrap_or("")
    );
}

#[test]
fn coddl_parse_cdstore_round_trips_example() {
    let out = coddl()
        .args(["parse"])
        .arg(companion_path("greetings.cdstore"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl parse <.cdstore> failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("CDSTORE_ROOT@"),
        "expected CST dump to start with CDSTORE_ROOT, got: {}",
        stdout.lines().next().unwrap_or("")
    );
}

#[test]
fn coddl_check_rejects_cddb_file() {
    // Downstream passes are .cd-only today — `check` rejects dialect
    // input with a clear error.
    let out = coddl()
        .args(["check"])
        .arg(companion_path("greetings.cddb"))
        .output()
        .expect("spawn coddl");
    assert!(
        !out.status.success(),
        "expected `coddl check <.cddb>` to fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("only accepts .cd files"),
        "expected dialect-rejection message, got stderr:\n{stderr}"
    );
}

#[test]
fn coddl_lex_works_on_any_dialect() {
    // The lexer is dialect-agnostic; `coddl lex` should succeed on a
    // `.cddb` file even though its grammar is different.
    let out = coddl()
        .args(["lex"])
        .arg(companion_path("greetings.cddb"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl lex <.cddb> failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The tokens themselves don't reveal the dialect — just confirm
    // the output is non-empty token stream.
    assert!(
        stdout.contains("Ident"),
        "expected Ident tokens in lex output"
    );
}

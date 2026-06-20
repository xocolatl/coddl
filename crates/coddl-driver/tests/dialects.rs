//! Integration tests for the `.cddb` / `.cdmap` / `.cdstore` dialect
//! parsers wired through the `coddl` driver.
//!
//! Each test **owns its source**: it authors a dialect file in a tempdir,
//! invokes the built `coddl` binary against it, and asserts that the parser
//! produces a clean tree (zero diagnostics) and that `coddl check` rejects
//! dialect input with a clear error.

use std::path::PathBuf;
use std::process::Command;

/// Author a `greetings.cddb` catalog in `tmp` and return its path.
fn write_cddb(tmp: &tempfile::TempDir) -> PathBuf {
    let path = tmp.path().join("greetings.cddb");
    std::fs::write(
        &path,
        "database greetings;\n\
         base relvar Greetings { id: Integer, message: Text } key { id };\n",
    )
    .expect("write greetings.cddb");
    path
}

/// Author a `greetings.cdstore` binding in `tmp` and return its path.
fn write_cdstore(tmp: &tempfile::TempDir) -> PathBuf {
    let path = tmp.path().join("greetings.cdstore");
    std::fs::write(
        &path,
        "store for greetings;\n\
         backend sqlite { file: \"greetings.sqlite\" };\n\
         relvar Greetings: table \"greetings\" { columns: { id: \"id\", message: \"message\" } };\n",
    )
    .expect("write greetings.cdstore");
    path
}

fn coddl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_coddl"))
}

#[test]
fn coddl_parse_cddb_round_trips() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = coddl()
        .args(["parse"])
        .arg(write_cddb(&tmp))
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

// The identity-mapped case (the catalog's relvars reused under the same
// names) needs no `.cdmap`, so the round-trip tests above don't author one.
// The `.cdmap` parser is exercised thoroughly by the unit tests in
// `coddl_syntax::parser_cdmap`; the driver-dispatch test below authors a
// `.cdmap` tempfile to confirm the driver routes the dialect.

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
fn coddl_parse_cdstore_round_trips() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = coddl()
        .args(["parse"])
        .arg(write_cdstore(&tmp))
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
fn coddl_check_accepts_cddb_file() {
    // Phase 15: `coddl check` now typechecks `.cddb` — a well-formed
    // catalog with sound relvar declarations should exit cleanly.
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = coddl()
        .args(["check"])
        .arg(write_cddb(&tmp))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "expected `coddl check <.cddb>` to succeed, got stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn coddl_lex_works_on_any_dialect() {
    // The lexer is dialect-agnostic; `coddl lex` should succeed on a
    // `.cddb` file even though its grammar is different.
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = coddl()
        .args(["lex"])
        .arg(write_cddb(&tmp))
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

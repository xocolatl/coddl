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
fn coddl_provision_seeds_from_catalog() {
    // `coddl provision <db>.cddb` reconciles the SQLite database its catalog
    // declares: create-or-verify each base table, then re-seed it to its INIT
    // value. This test authors a catalog *with* an INIT value plus its store
    // binding, points the target file at a tempdir via the env override, and
    // asserts the seeded rows land.
    let tmp = tempfile::tempdir().expect("tempdir");
    let cddb = tmp.path().join("greetings.cddb");
    std::fs::write(
        &cddb,
        "database greetings;\n\
         base relvar Greetings { id: Integer, message: Text } key { id };\n\
         Greetings := Relation { { id: 1, message: \"hi\" }, { id: 2, message: \"yo\" } };\n",
    )
    .expect("write greetings.cddb");
    write_cdstore(&tmp);

    // Resolve the same file the runtime would, but steered into the tempdir so
    // no `greetings.sqlite` is left in the test's working directory.
    let db = tmp.path().join("greetings.sqlite");
    let out = coddl()
        .args(["provision"])
        .arg(&cddb)
        .env("CODDL_GREETINGS_FILE", &db)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl provision <.cddb> failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("greetings: created, 2 row(s)"),
        "expected a create-and-seed summary line, got:\n{stdout}"
    );

    // The rows are actually in the database.
    let conn = rusqlite::Connection::open(&db).expect("open seeded db");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM greetings", [], |r| r.get(0))
        .expect("count seeded rows");
    assert_eq!(n, 2, "expected 2 seeded rows");

    // Re-provisioning is idempotent: the matching table is verified, not
    // recreated, and the row count is unchanged.
    let out2 = coddl()
        .args(["provision"])
        .arg(&cddb)
        .env("CODDL_GREETINGS_FILE", &db)
        .output()
        .expect("spawn coddl");
    assert!(out2.status.success(), "re-provision failed");
    assert!(
        String::from_utf8_lossy(&out2.stdout).contains("greetings: verified, 2 row(s)"),
        "expected a verify summary on re-provision, got:\n{}",
        String::from_utf8_lossy(&out2.stdout)
    );
}

#[test]
fn coddl_provision_rejects_non_cddb() {
    // provision reconciles from a catalog; a `.cd` (or any non-`.cddb`) input is
    // a usage error (exit 2), never touched.
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::with_suffix(".cd").expect("tempfile");
    writeln!(tmp, "program p;\noper main {{}}").expect("write");
    let out = coddl()
        .args(["provision"])
        .arg(tmp.path())
        .output()
        .expect("spawn coddl");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 for a non-`.cddb` provision target, stderr=\n{}",
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

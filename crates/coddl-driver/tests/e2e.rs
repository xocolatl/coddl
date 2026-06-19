//! End-to-end tests for the `coddl` driver.
//!
//! Invokes the built `coddl` binary as a subprocess (located via the
//! `CARGO_BIN_EXE_coddl` env var that Cargo sets for integration
//! tests). Each test exercises one of the new subcommands —
//! `coddl run` or `coddl compile` — against `hello-world.cd` and
//! asserts the resulting binary's stdout.
//!
//! Tests fail loudly if `clang` / `cc` is missing on PATH or if the
//! runtime staticlib hasn't been built.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

/// `examples/<name>/<name>.cd` is the on-disk convention.
fn example_path(name: &str) -> PathBuf {
    workspace_root().join(format!("examples/{name}/{name}.cd"))
}

fn hello_world_path() -> PathBuf {
    example_path("hello-world")
}

fn ensure_runtime_built() {
    let path = workspace_root().join("target/debug/libcoddl_runtime.a");
    if path.exists() {
        return;
    }
    let status = Command::new("cargo")
        .args(["build", "-p", "coddl-runtime"])
        .current_dir(workspace_root())
        .status()
        .expect("invoke cargo");
    assert!(status.success(), "cargo build -p coddl-runtime failed");
    assert!(
        path.exists(),
        "expected runtime staticlib at {} after build",
        path.display()
    );
}

fn coddl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_coddl"))
}

#[test]
fn coddl_run_default_backend_prints_hello_world() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run"])
        .arg(hello_world_path())
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"Hello, world!\n");
}

#[test]
fn coddl_run_llvm_backend_prints_hello_world() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(hello_world_path())
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=llvm failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"Hello, world!\n");
}

#[test]
fn coddl_run_cranelift_backend_prints_hello_world() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(hello_world_path())
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=cranelift failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"Hello, world!\n");
}

#[test]
fn coddl_compile_llvm_produces_runnable_binary() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = tmp.path().join("hello_llvm");
    let out = coddl()
        .args(["compile", "--backend=llvm"])
        .arg(hello_world_path())
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("spawn coddl compile");
    assert!(
        out.status.success(),
        "compile failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let run = Command::new(&bin).output().expect("run binary");
    assert!(run.status.success(), "binary exit {}", run.status);
    assert_eq!(run.stdout, b"Hello, world!\n");
}

#[test]
fn coddl_compile_cranelift_produces_runnable_binary() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = tmp.path().join("hello_cranelift");
    let out = coddl()
        .args(["compile", "--backend=cranelift"])
        .arg(hello_world_path())
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("spawn coddl compile");
    assert!(
        out.status.success(),
        "compile failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let run = Command::new(&bin).output().expect("run binary");
    assert!(run.status.success(), "binary exit {}", run.status);
    assert_eq!(run.stdout, b"Hello, world!\n");
}

/// The cross-backend equivalence invariant: for any source program,
/// both backends produce byte-identical stdout. This is the
/// validation discipline documented in `docs/validation.md` —
/// adding a new example program means adding a parameterized assert
/// pair here.
#[test]
fn hello_world_byte_identical_across_backends() {
    ensure_runtime_built();

    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(hello_world_path())
        .output()
        .expect("spawn coddl run --backend=llvm");
    assert!(
        llvm.status.success(),
        "LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&llvm.stderr)
    );

    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(hello_world_path())
        .output()
        .expect("spawn coddl run --backend=cranelift");
    assert!(
        cranelift.status.success(),
        "Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&cranelift.stderr)
    );

    assert_eq!(
        llvm.stdout,
        cranelift.stdout,
        "backends disagree:\n  LLVM:      {:?}\n  Cranelift: {:?}",
        String::from_utf8_lossy(&llvm.stdout),
        String::from_utf8_lossy(&cranelift.stdout)
    );
    assert_eq!(
        llvm.stdout,
        b"Hello, world!\n",
        "both backends produced unexpected stdout: {:?}",
        String::from_utf8_lossy(&llvm.stdout)
    );
}

// ── Transaction example ───────────────────────────────────────────────

#[test]
fn transaction_llvm_backend_prints_ok() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(example_path("transaction"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "transaction LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"ok\n");
}

#[test]
fn transaction_cranelift_backend_prints_ok() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(example_path("transaction"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "transaction Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"ok\n");
}

#[test]
fn transaction_byte_identical_across_backends() {
    ensure_runtime_built();
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(example_path("transaction"))
        .output()
        .expect("spawn LLVM");
    assert!(
        llvm.status.success(),
        "LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(example_path("transaction"))
        .output()
        .expect("spawn Cranelift");
    assert!(
        cranelift.status.success(),
        "Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&cranelift.stderr)
    );
    assert_eq!(
        llvm.stdout,
        cranelift.stdout,
        "transaction backends disagree:\n  LLVM:      {:?}\n  Cranelift: {:?}",
        String::from_utf8_lossy(&llvm.stdout),
        String::from_utf8_lossy(&cranelift.stdout)
    );
    assert_eq!(llvm.stdout, b"ok\n");
}

// ── Tuple let + field access (Phase 18) ───────────────────────────────

/// Inline-source program exercising tuple literal + field access. The
/// e2e suite owns the canonical Phase 18 program rather than depending
/// on an `examples/` dir — the latter is a deletable scratchpad.
const TUPLE_LET_SRC: &str = "\
program tuple_let;
oper main {} [
    let t = {message: \"hi\"};
    write_line { message: t.message };
];
";

/// Write the inline tuple-let program to a tempdir and return both
/// the tempdir handle (kept alive by the caller) and the source path.
fn write_tuple_let(tmp: &tempfile::TempDir) -> PathBuf {
    let src_path = tmp.path().join("tuple-let.cd");
    std::fs::write(&src_path, TUPLE_LET_SRC).expect("write tuple-let.cd");
    src_path
}

#[test]
fn tuple_let_llvm_backend_prints_hi() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_tuple_let(&tmp);
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "tuple-let LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hi\n");
}

#[test]
fn tuple_let_cranelift_backend_prints_hi() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_tuple_let(&tmp);
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "tuple-let Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hi\n");
}

#[test]
fn tuple_let_byte_identical_across_backends() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_tuple_let(&tmp);
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn LLVM");
    assert!(
        llvm.status.success(),
        "LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn Cranelift");
    assert!(
        cranelift.status.success(),
        "Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&cranelift.stderr)
    );
    assert_eq!(
        llvm.stdout,
        cranelift.stdout,
        "tuple-let backends disagree:\n  LLVM:      {:?}\n  Cranelift: {:?}",
        String::from_utf8_lossy(&llvm.stdout),
        String::from_utf8_lossy(&cranelift.stdout)
    );
    assert_eq!(llvm.stdout, b"hi\n");
}

// ── Relation literals (Phase 19) ──────────────────────────────────────

/// Phase 19 e2e program. Source order is `{a: 2}, {a: 1}, {a: 1}`;
/// `coddl_relation_seal` must sort ascending and adjacent-dedup, so
/// stdout is `{a: 1}\n{a: 2}\n`. The duplicate-elimination
/// requirement (RM Pro 3) is part of what's being validated; the
/// seal-then-print pipeline must produce a deterministic, total
/// order so cross-backend byte equality works.
const RELATION_LIT_SRC: &str = "\
program relation_lit;
oper main {} [
    let r = Relation { {a: 2}, {a: 1}, {a: 1} };
    write_relation { rel: r };
];
";

fn write_relation_lit(tmp: &tempfile::TempDir) -> PathBuf {
    let src_path = tmp.path().join("relation-lit.cd");
    std::fs::write(&src_path, RELATION_LIT_SRC).expect("write relation-lit.cd");
    src_path
}

#[test]
fn relation_lit_llvm_backend_prints_seal_order() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_relation_lit(&tmp);
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "relation-lit LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"{a: 1}\n{a: 2}\n");
}

#[test]
fn relation_lit_cranelift_backend_prints_seal_order() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_relation_lit(&tmp);
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "relation-lit Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"{a: 1}\n{a: 2}\n");
}

#[test]
fn relation_lit_byte_identical_across_backends() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_relation_lit(&tmp);
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn LLVM");
    assert!(
        llvm.status.success(),
        "LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn Cranelift");
    assert!(
        cranelift.status.success(),
        "Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&cranelift.stderr)
    );
    assert_eq!(
        llvm.stdout,
        cranelift.stdout,
        "relation-lit backends disagree:\n  LLVM:      {:?}\n  Cranelift: {:?}",
        String::from_utf8_lossy(&llvm.stdout),
        String::from_utf8_lossy(&cranelift.stdout)
    );
    assert_eq!(llvm.stdout, b"{a: 1}\n{a: 2}\n");
}

// ── `where` restriction (Phase 20) ────────────────────────────────────

const WHERE_FILTER_SRC: &str = "\
program where_filter;
oper main {} [
    let r = Relation { {a: 1}, {a: 2}, {a: 3} };
    write_relation { rel: r where a = 2 };
];
";

fn write_where_filter(tmp: &tempfile::TempDir) -> PathBuf {
    let src_path = tmp.path().join("where-filter.cd");
    std::fs::write(&src_path, WHERE_FILTER_SRC).expect("write where-filter.cd");
    src_path
}

#[test]
fn where_llvm_backend_filters_to_single_match() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_where_filter(&tmp);
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "where-filter LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"{a: 2}\n");
}

#[test]
fn where_cranelift_backend_filters_to_single_match() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_where_filter(&tmp);
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "where-filter Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"{a: 2}\n");
}

#[test]
fn where_byte_identical_across_backends() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_where_filter(&tmp);
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn LLVM");
    assert!(
        llvm.status.success(),
        "LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn Cranelift");
    assert!(
        cranelift.status.success(),
        "Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&cranelift.stderr)
    );
    assert_eq!(
        llvm.stdout,
        cranelift.stdout,
        "where-filter backends disagree:\n  LLVM:      {:?}\n  Cranelift: {:?}",
        String::from_utf8_lossy(&llvm.stdout),
        String::from_utf8_lossy(&cranelift.stdout)
    );
    assert_eq!(llvm.stdout, b"{a: 2}\n");
}

// ── extract (Phase 21) ────────────────────────────────────────────────

const EXTRACT_SRC: &str = "\
program extract_test;
oper main {} [
    let r = Relation { {a: 1, b: \"hi\"}, {a: 2, b: \"ho\"} };
    let t = extract (r where a = 2);
    write_line { message: t.b };
];
";

fn write_extract_src(tmp: &tempfile::TempDir) -> PathBuf {
    let p = tmp.path().join("extract.cd");
    std::fs::write(&p, EXTRACT_SRC).expect("write extract.cd");
    p
}

#[test]
fn extract_llvm_backend_prints_field() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_extract_src(&tmp);
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "extract LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"ho\n");
}

#[test]
fn extract_cranelift_backend_prints_field() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_extract_src(&tmp);
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "extract Cranelift run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"ho\n");
}

#[test]
fn extract_byte_identical_across_backends() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = write_extract_src(&tmp);
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&src)
        .output()
        .expect("spawn LLVM");
    assert!(llvm.status.success());
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&src)
        .output()
        .expect("spawn Cranelift");
    assert!(cranelift.status.success());
    assert_eq!(llvm.stdout, cranelift.stdout);
    assert_eq!(llvm.stdout, b"ho\n");
}

/// `extract` of a zero-row relation aborts (cardinality != 1).
const EXTRACT_ZERO_SRC: &str = "\
program extract_zero;
oper main {} [
    let r = Relation { {a: 1} };
    let t = extract (r where a = 99);
    write_line { message: \"unreachable\" };
];
";

#[test]
fn extract_aborts_on_zero_tuples() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path().join("extract-zero.cd");
    std::fs::write(&p, EXTRACT_ZERO_SRC).expect("write");
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&p)
        .output()
        .expect("spawn coddl");
    assert!(
        !out.status.success(),
        "expected abort on zero-tuple extract, got success with stdout={:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("extract") && stderr.contains("expected exactly 1"),
        "stderr didn't carry the extract diagnostic: {stderr}"
    );
}

/// `extract` of a multi-row relation aborts.
const EXTRACT_MULTI_SRC: &str = "\
program extract_multi;
oper main {} [
    let r = Relation { {a: 1}, {a: 2} };
    let t = extract r;
    write_line { message: \"unreachable\" };
];
";

#[test]
fn extract_aborts_on_multi_tuples() {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path().join("extract-multi.cd");
    std::fs::write(&p, EXTRACT_MULTI_SRC).expect("write");
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&p)
        .output()
        .expect("spawn coddl");
    assert!(
        !out.status.success(),
        "expected abort on multi-tuple extract"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("extract") && stderr.contains("expected exactly 1"),
        "stderr didn't carry the extract diagnostic: {stderr}"
    );
}

// ── Database-backed reads (public relvar + SQLite) ───────────────────
//
// These tests own their source + fixtures (`write_pushdown_fixtures` /
// `seed_greetings_fixtures`); none reads `examples/hello-world-db`, which is a
// hand-editable playground a test must never depend on. End-to-end "a
// DB-backed read prints its value on both backends" is covered by the
// owned-source `relvar_pushdown_audit_{llvm,cranelift}` tests below.

#[test]
fn greetings_env_var_override_picks_alternate_path() {
    // CODDL_GREETINGS_FILE must override the `.cdstore`'s baked `file:`
    // default. The default fixture db says "hello world"; pointing the
    // override at a db that says "override hello" and seeing that message
    // proves the override flows through to the actual connection.
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cd, _default_db) = write_pushdown_fixtures(tmp.path()); // default: "hello world"

    let alt = tmp.path().join("alt.sqlite");
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "sqlite3 '{}' \"CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL); INSERT INTO greetings (id, message) VALUES (1, 'override hello');\"",
            alt.display()
        ))
        .status()
        .expect("invoke sqlite3");
    assert!(status.success(), "alt SQLite seed failed");

    let out = coddl()
        .env("CODDL_GREETINGS_FILE", &alt)
        .args(["run", "--backend=llvm"])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run with override failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"override hello\n");
}

// ── SQL-pushdown acceptance (assert against the audit log) ────────────

/// Strip the `YYYY-MM-DD HH:MM:SS.mmm - sqlite - ` prefix from one audit
/// line and return the captured SQL. Returns `None` if the line does not
/// conform to the audit format — every non-empty line must, so the caller
/// treats `None` as a hard failure (the format is part of the contract).
///
/// Hand-rolled rather than `regex`-backed: the workspace pulls no `regex`
/// crate (the runtime's own `audit.rs` hand-rolls its date logic too), and
/// adding one just to split on a fixed prefix isn't worth the lock-graph
/// churn.
fn audit_sql(line: &str) -> Option<&str> {
    const SEP: &str = " - sqlite - ";
    let idx = line.find(SEP)?;
    let ts = &line[..idx];
    if !is_audit_timestamp(ts) {
        return None;
    }
    Some(&line[idx + SEP.len()..])
}

/// `YYYY-MM-DD HH:MM:SS.mmm` — exactly the shape `audit::format_utc` emits.
fn is_audit_timestamp(ts: &str) -> bool {
    let b = ts.as_bytes();
    if b.len() != 23 {
        return false;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    let punct = |i: usize, c: u8| b[i] == c;
    (0..4).all(digit)
        && punct(4, b'-')
        && (5..7).all(digit)
        && punct(7, b'-')
        && (8..10).all(digit)
        && punct(10, b' ')
        && (11..13).all(digit)
        && punct(13, b':')
        && (14..16).all(digit)
        && punct(16, b':')
        && (17..19).all(digit)
        && punct(19, b'.')
        && (20..23).all(digit)
}

/// The single statement the pushed-down read must lower to — the source
/// projects to `{message}`, so the SELECT list narrows to that one column.
/// No `DISTINCT`: `where id = 1` pins the key, bounding cardinality to ≤ 1, so
/// the projection is provably duplicate-free. The literal `1` is inlined by
/// the legacy `trace` callback.
const EXPECTED_PUSHED_SQL: &str = r#"SELECT "message" FROM "greetings" WHERE "id" = 1"#;

/// Author a self-contained relvar-rooted pushdown program — `.cd` plus its
/// `greetings.cddb` / `greetings.cdstore` companions — into `dir`, and seed a
/// SQLite db at `<dir>/greetings.sqlite`. Returns the `.cd` and db paths.
///
/// This test **owns its source** rather than reading `examples/hello-world-db`:
/// the audit test asserts a *compiler property* (a relvar-rooted
/// `where … project …` lowers to one pushed `SELECT`, no startup scan), which
/// must not be coupled to a hand-editable example whose author may legitimately
/// rewrite it to read in-process.
/// Write the `greetings` database companions (`.cddb` / `.cdstore`) into `dir`
/// and seed a SQLite db at `<dir>/greetings.sqlite` with the single
/// `(1, 'hello world')` row. Returns the db path. The caller writes its own
/// `.cd` (with `database greetings;`) alongside.
fn seed_greetings_fixtures(dir: &Path) -> PathBuf {
    std::fs::write(
        dir.join("greetings.cddb"),
        "database greetings;\n\
         base relvar Greetings { id: Integer, message: Text } key { id };\n",
    )
    .expect("write greetings.cddb");
    std::fs::write(
        dir.join("greetings.cdstore"),
        "store for greetings;\n\
         backend sqlite { file: \"greetings.sqlite\" };\n\
         relvar Greetings: table \"greetings\" { columns: { id: \"id\", message: \"message\" } };\n",
    )
    .expect("write greetings.cdstore");

    let db = dir.join("greetings.sqlite");
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "sqlite3 '{}' \"CREATE TABLE greetings (id INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (id)); INSERT INTO greetings (id, message) VALUES (1, 'hello world');\"",
            db.display()
        ))
        .status()
        .expect("invoke sqlite3");
    assert!(status.success(), "greetings fixture seed failed");
    db
}

fn write_pushdown_fixtures(dir: &Path) -> (PathBuf, PathBuf) {
    let cd = dir.join("pushdown.cd");
    std::fs::write(
        &cd,
        "program hello_world_db;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             let g = transaction [ extract (Greetings where id = 1 project {message}) ];\n\
             write_line { message: g.message };\n\
         ];\n",
    )
    .expect("write pushdown.cd");
    let db = seed_greetings_fixtures(dir);
    (cd, db)
}

/// Compile + run a self-owned relvar-rooted pushdown program on `backend`,
/// pointing `CODDL_AUDIT_LOG` at a fresh per-run temp file, then assert the
/// audit log proves the pushdown path ran: the program printed `hello world`,
/// every logged line is well-formed, **no** statement is a `FROM "greetings"`
/// full-table scan (no `WHERE`), and **exactly one** statement is the
/// parameterized filter — byte-for-byte `EXPECTED_PUSHED_SQL`.
///
/// A fresh log path per run is mandatory: the sink opens in append mode, so
/// reusing a path would mix runs and a stale full-scan line would break the
/// counts.
fn assert_pushdown_audit(backend: &str) {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cd, db) = write_pushdown_fixtures(tmp.path());
    let log = tmp.path().join("audit.log");

    let out = coddl()
        .env("CODDL_AUDIT_LOG", &log)
        .env("CODDL_GREETINGS_FILE", &db)
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend={backend} {:?} failed: stderr=\n{}",
        cd,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        out.stdout, b"hello world\n",
        "unexpected stdout on {backend}: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let contents = std::fs::read_to_string(&log).unwrap_or_else(|e| {
        panic!("read audit log {}: {e}", log.display());
    });
    // Every non-empty line must parse — the format itself is part of the
    // contract this test pins. Collect the captured SQL text.
    let sqls: Vec<&str> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            audit_sql(l).unwrap_or_else(|| panic!("malformed audit line ({backend}): {l:?}"))
        })
        .collect();
    assert!(
        !sqls.is_empty(),
        "audit log empty on {backend}: the run logged no SQL"
    );

    // No startup full-table scan: nothing reads `greetings` without a filter.
    let scans: Vec<&&str> = sqls
        .iter()
        .filter(|s| s.contains("greetings") && !s.contains("WHERE"))
        .collect();
    assert!(
        scans.is_empty(),
        "startup full-table scan(s) present on {backend}: {scans:?}"
    );

    // Exactly one filtered read of `greetings`, and it is the pushed query.
    let filtered: Vec<&&str> = sqls
        .iter()
        .filter(|s| s.contains("greetings") && s.contains("WHERE"))
        .collect();
    assert_eq!(
        filtered.len(),
        1,
        "expected exactly one pushed filtered query on {backend}, got {filtered:?}"
    );
    assert_eq!(
        *filtered[0], EXPECTED_PUSHED_SQL,
        "pushed SQL diverged from the golden text on {backend}"
    );
}

#[test]
fn relvar_pushdown_audit_llvm() {
    assert_pushdown_audit("llvm");
}

#[test]
fn relvar_pushdown_audit_cranelift() {
    assert_pushdown_audit("cranelift");
}

// Helper-level checks proving the acceptance assertions are non-vacuous —
// they reject the pre-pushdown world (a startup full scan) and a malformed
// line — without needing a live runtime to regress.

#[test]
fn audit_sql_strips_prefix_and_validates_format() {
    let line = r#"2026-06-19 07:12:36.948 - sqlite - SELECT DISTINCT "id" FROM "greetings" WHERE "id" = 1"#;
    assert_eq!(
        audit_sql(line),
        Some(r#"SELECT DISTINCT "id" FROM "greetings" WHERE "id" = 1"#)
    );
    // Malformed timestamp prefixes are rejected (None), so the integration
    // test panics rather than silently skipping a non-conforming line.
    assert_eq!(audit_sql("2026-6-19 07:12:36.948 - sqlite - SELECT 1"), None);
    assert_eq!(audit_sql("not a log line at all"), None);
    assert_eq!(audit_sql("2026-06-19 07:12:36.948 - postgres - SELECT 1"), None);
}

#[test]
fn scan_classifier_catches_the_pre_pushdown_full_scan() {
    // The legacy startup read (no WHERE) is exactly what the acceptance test must reject.
    let legacy = "SELECT id, message FROM greetings";
    assert!(legacy.contains("greetings") && !legacy.contains("WHERE"));
    // The pushed read is classified as filtered, not a scan.
    assert!(EXPECTED_PUSHED_SQL.contains("greetings") && EXPECTED_PUSHED_SQL.contains("WHERE"));
}

// ── in-process projection (Inst::Project → coddl_relation_project) ────

/// `project` over an in-memory relation literal (not relvar-rooted, so the
/// cut declines) exercises the in-process projection path. Three rows
/// project to `{a}` → `{a:1}` appears twice and collapses, so the sealed
/// output is `{a: 1}` then `{a: 2}`.
const PROJECT_INPROCESS_SRC: &str = "\
program project_inprocess;
oper main {} [
    let r = Relation { {a: 1, b: 10}, {a: 1, b: 20}, {a: 2, b: 30} };
    let p = r project {a};
    write_relation { rel: p };
];
";

fn run_project_inprocess(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("project-inprocess.cd");
    std::fs::write(&src, PROJECT_INPROCESS_SRC).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "in-process project on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn project_inprocess_llvm_narrows_and_dedups() {
    assert_eq!(run_project_inprocess("llvm"), b"{a: 1}\n{a: 2}\n");
}

#[test]
fn project_inprocess_cranelift_narrows_and_dedups() {
    assert_eq!(run_project_inprocess("cranelift"), b"{a: 1}\n{a: 2}\n");
}

#[test]
fn project_inprocess_byte_identical_across_backends() {
    assert_eq!(
        run_project_inprocess("llvm"),
        run_project_inprocess("cranelift"),
    );
}

// ── project all but { … } (TTM project-away) ─────────────────────────

/// `project all but {b}` keeps the complement `{a}` — same result as
/// `project {a}`: three rows collapse to the sealed `{a: 1}`, `{a: 2}`.
const PROJECT_ALL_BUT_SRC: &str = "\
program project_all_but;
oper main {} [
    let r = Relation { {a: 1, b: 10}, {a: 1, b: 20}, {a: 2, b: 30} };
    let p = r project all but {b};
    write_relation { rel: p };
];
";

fn run_all_but_inprocess(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("all-but.cd");
    std::fs::write(&src, PROJECT_ALL_BUT_SRC).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "in-process all-but on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn project_all_but_inprocess_keeps_complement() {
    assert_eq!(run_all_but_inprocess("llvm"), b"{a: 1}\n{a: 2}\n");
    assert_eq!(run_all_but_inprocess("cranelift"), b"{a: 1}\n{a: 2}\n");
}

#[test]
fn project_all_but_pushed_keeps_complement() {
    // `Greetings where id = 1 project all but {id}` keeps {message}; pushes to
    // `SELECT "message" FROM "greetings" WHERE "id" = 1` (key-filtered → no
    // DISTINCT), the same query `project {message}` produces.
    for backend in ["llvm", "cranelift"] {
        ensure_runtime_built();
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = seed_greetings_fixtures(tmp.path());
        let cd = tmp.path().join("all-but-pushed.cd");
        std::fs::write(
            &cd,
            "program ab;\n\
             database greetings;\n\
             public relvar Greetings { id: Integer, message: Text } key { id };\n\
             oper main {} [ let g = transaction [ extract (Greetings where id = 1 project all but {id}) ]; write_line { message: g.message }; ];\n",
        )
        .expect("write cd");
        let log = tmp.path().join("audit.log");
        let out = coddl()
            .env("CODDL_GREETINGS_FILE", &db)
            .env("CODDL_AUDIT_LOG", &log)
            .args(["run", &format!("--backend={backend}")])
            .arg(&cd)
            .output()
            .expect("spawn coddl");
        assert!(
            out.status.success(),
            "pushed all-but on {backend} failed: stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"hello world\n", "on {backend}");
        let log_txt = std::fs::read_to_string(&log).expect("read audit log");
        assert!(
            log_txt.contains(r#"SELECT "message" FROM "greetings" WHERE "id" = 1"#),
            "expected message-only no-DISTINCT pushed SQL on {backend}, got:\n{log_txt}"
        );
    }
}

/// `project {}` collapses a multi-row relation to one empty tuple
/// (`reltrue`), not N — a set, per RM Pro 3.
const PROJECT_NULLARY_SRC: &str = "\
program project_nullary;
oper main {} [
    let r = Relation { {a: 1}, {a: 2} };
    let p = r project {};
    write_relation { rel: p };
];
";

fn run_project_nullary(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("project-nullary.cd");
    std::fs::write(&src, PROJECT_NULLARY_SRC).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "nullary project on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn project_nullary_collapses_to_single_empty_tuple() {
    assert_eq!(run_project_nullary("llvm"), b"{}\n");
    assert_eq!(run_project_nullary("cranelift"), b"{}\n");
}

/// Pushed nullary projection: `Greetings where id = <n> project {}` lowers to
/// `SELECT DISTINCT 1 … WHERE "id" = ?`, which the runtime marshals against the
/// empty descriptor as `reltrue` (one `{}` row when the tuple exists) or
/// `relfalse` (no rows when it doesn't).
fn run_pushed_nullary(backend: &str, where_id: i64) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = seed_greetings_fixtures(tmp.path());
    let cd = tmp.path().join("np.cd");
    std::fs::write(
        &cd,
        format!(
            "program np;\n\
             database greetings;\n\
             public relvar Greetings {{ id: Integer, message: Text }} key {{ id }};\n\
             oper main {{}} [ let g = transaction [ Greetings where id = {where_id} project {{}} ]; write_relation {{ rel: g }}; ];\n"
        ),
    )
    .expect("write np.cd");
    let out = coddl()
        .env("CODDL_GREETINGS_FILE", &db)
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "pushed nullary on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn pushed_nullary_projection_is_reltrue_or_relfalse() {
    for backend in ["llvm", "cranelift"] {
        // id = 1 is present → reltrue (one empty tuple).
        assert_eq!(run_pushed_nullary(backend, 1), b"{}\n", "reltrue on {backend}");
        // id = 999 is absent → relfalse (zero tuples, no output).
        assert_eq!(run_pushed_nullary(backend, 999), b"", "relfalse on {backend}");
    }
}

// ── rename (pushed to SQL via AS) ────────────────────────────────────

#[test]
fn pushed_rename_aliases_columns() {
    // `Greetings where id = 1 rename {id: identifier, message: msg}` pushes to
    // `SELECT "id" AS "identifier", "message" AS "msg" … WHERE "id" = 1`; the
    // renamed `msg` is read back and printed.
    for backend in ["llvm", "cranelift"] {
        ensure_runtime_built();
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = seed_greetings_fixtures(tmp.path());
        let cd = tmp.path().join("rn.cd");
        std::fs::write(
            &cd,
            "program rn;\n\
             database greetings;\n\
             public relvar Greetings { id: Integer, message: Text } key { id };\n\
             oper main {} [ let g = transaction [ extract (Greetings where id = 1 rename {id: identifier, message: msg}) ]; write_line { message: g.msg }; ];\n",
        )
        .expect("write rn.cd");
        let log = tmp.path().join("audit.log");
        let out = coddl()
            .env("CODDL_GREETINGS_FILE", &db)
            .env("CODDL_AUDIT_LOG", &log)
            .args(["run", &format!("--backend={backend}")])
            .arg(&cd)
            .output()
            .expect("spawn coddl");
        assert!(
            out.status.success(),
            "pushed rename on {backend} failed: stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"hello world\n", "on {backend}");
        let log_txt = std::fs::read_to_string(&log).expect("read audit log");
        assert!(
            log_txt.contains(
                r#"SELECT "id" AS "identifier", "message" AS "msg" FROM "greetings" WHERE "id" = 1"#
            ),
            "expected the rename pushed via AS on {backend}, got:\n{log_txt}"
        );
    }
}

// ── in-process rename (Inst::Rename → coddl_relation_rename) ──────────

/// `rename` over an in-memory relation literal (not relvar-rooted, so the cut
/// declines) exercises the in-process path. Renaming `a → z` re-sorts the
/// heading from `{a, b}` to `{b, z}`, so the runtime must *permute* record
/// bytes into the new canonical layout, not just relabel. Output is sealed in
/// `{b, z}` order: `{b: 10, z: 1}` then `{b: 20, z: 2}`.
const RENAME_INPROCESS_SRC: &str = "\
program rename_inprocess;
oper main {} [
    let r = Relation { {a: 1, b: 10}, {a: 2, b: 20} };
    let s = r rename {a: z};
    write_relation { rel: s };
];
";

fn run_rename_inprocess(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("rename-inprocess.cd");
    std::fs::write(&src, RENAME_INPROCESS_SRC).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "in-process rename on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn rename_inprocess_llvm_permutes_into_new_layout() {
    assert_eq!(run_rename_inprocess("llvm"), b"{b: 10, z: 1}\n{b: 20, z: 2}\n");
}

#[test]
fn rename_inprocess_cranelift_permutes_into_new_layout() {
    assert_eq!(
        run_rename_inprocess("cranelift"),
        b"{b: 10, z: 1}\n{b: 20, z: 2}\n"
    );
}

#[test]
fn rename_inprocess_byte_identical_across_backends() {
    assert_eq!(run_rename_inprocess("llvm"), run_rename_inprocess("cranelift"));
}

#[test]
fn rename_inprocess_after_transaction_escape() {
    // Owned twin of the hello-world example: a pushed rename whose relation
    // *escapes* the transaction as the block's tail value (a `let`-bound
    // local), then a second, in-process rename over that local, then extract +
    // print. Covers both the in-process rename path and a relation surviving as
    // a transaction's return value.
    for backend in ["llvm", "cranelift"] {
        ensure_runtime_built();
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = seed_greetings_fixtures(tmp.path());
        let cd = tmp.path().join("escape.cd");
        std::fs::write(
            &cd,
            "program escape;\n\
             database greetings;\n\
             public relvar Greetings { id: Integer, message: Text } key { id };\n\
             oper main {} [ let g = transaction [ let x = Greetings where id = 1 rename {id: identifier, message: msg}; x ]; let g2 = g rename {msg: the_message}; let t = extract g2; write_line { message: t.the_message }; ];\n",
        )
        .expect("write escape.cd");
        let out = coddl()
            .env("CODDL_GREETINGS_FILE", &db)
            .args(["run", &format!("--backend={backend}")])
            .arg(&cd)
            .output()
            .expect("spawn coddl");
        assert!(
            out.status.success(),
            "escape rename on {backend} failed: stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"hello world\n", "on {backend}");
    }
}

// ── Text equality in `where` (pushed via param + in-process via coddl_text_eq) ──

#[test]
fn pushed_text_where_binds_a_text_param() {
    // `Greetings where message = "hello world"` is relvar-rooted, so the Text
    // literal pushes as a bound parameter; the audit log (SQLite's expanded
    // SQL) shows it inlined as `'hello world'`.
    for backend in ["llvm", "cranelift"] {
        ensure_runtime_built();
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = seed_greetings_fixtures(tmp.path());
        let cd = tmp.path().join("tw.cd");
        std::fs::write(
            &cd,
            "program tw;\n\
             database greetings;\n\
             public relvar Greetings { id: Integer, message: Text } key { id };\n\
             oper main {} [ let g = transaction [ extract (Greetings where message = \"hello world\") ]; write_line { message: g.message }; ];\n",
        )
        .expect("write tw.cd");
        let log = tmp.path().join("audit.log");
        let out = coddl()
            .env("CODDL_GREETINGS_FILE", &db)
            .env("CODDL_AUDIT_LOG", &log)
            .args(["run", &format!("--backend={backend}")])
            .arg(&cd)
            .output()
            .expect("spawn coddl");
        assert!(
            out.status.success(),
            "pushed text where on {backend} failed: stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"hello world\n", "on {backend}");
        let log_txt = std::fs::read_to_string(&log).expect("read audit log");
        assert!(
            log_txt.contains(r#"WHERE "message" = 'hello world'"#),
            "expected the text restriction pushed on {backend}, got:\n{log_txt}"
        );
    }
}

/// In-process Text `where` over an in-memory relation literal (not relvar-
/// rooted, so the cut declines) routes the comparison through the runtime's
/// `coddl_text_eq` byte compare. Output is sealed in `{n, name}` order.
const TEXT_WHERE_EQ_SRC: &str = "\
program text_where_eq;
oper main {} [
    let r = Relation { {name: \"alice\", n: 1}, {name: \"bob\", n: 2} };
    let s = r where name = \"bob\";
    write_relation { rel: s };
];
";

const TEXT_WHERE_NEQ_SRC: &str = "\
program text_where_neq;
oper main {} [
    let r = Relation { {name: \"alice\", n: 1}, {name: \"bob\", n: 2} };
    let s = r where name <> \"bob\";
    write_relation { rel: s };
];
";

fn run_text_where_inprocess(src: &str, backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("text-where.cd");
    std::fs::write(&path, src).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&path)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "in-process text where on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn text_where_inprocess_eq_byte_identical() {
    let llvm = run_text_where_inprocess(TEXT_WHERE_EQ_SRC, "llvm");
    assert_eq!(llvm, b"{n: 2, name: \"bob\"}\n");
    assert_eq!(llvm, run_text_where_inprocess(TEXT_WHERE_EQ_SRC, "cranelift"));
}

#[test]
fn text_where_inprocess_neq_byte_identical() {
    let llvm = run_text_where_inprocess(TEXT_WHERE_NEQ_SRC, "llvm");
    assert_eq!(llvm, b"{n: 1, name: \"alice\"}\n");
    assert_eq!(llvm, run_text_where_inprocess(TEXT_WHERE_NEQ_SRC, "cranelift"));
}

// ── field-init shorthand (`{ name }` ≡ `{ name: name }`) ─────────────

fn run_shorthand(src: &str, backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("shorthand.cd");
    std::fs::write(&path, src).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&path)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "shorthand on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Operator-call shorthand: `write_line { message }` forwards the same-named
/// local.
const CALL_SHORTHAND_SRC: &str = "\
program call_shorthand;
oper main {} [
    let message = \"shorthand works\";
    write_line { message };
];
";

/// Tuple-literal shorthand: `{ message }` builds `Tuple { message: Text }`
/// from the same-named local; the field reads back through `t.message`.
const TUPLE_SHORTHAND_SRC: &str = "\
program tuple_shorthand;
oper main {} [
    let message = \"from a tuple\";
    let t = { message };
    write_line { message: t.message };
];
";

#[test]
fn call_field_init_shorthand_runs_byte_identical() {
    let llvm = run_shorthand(CALL_SHORTHAND_SRC, "llvm");
    assert_eq!(llvm, b"shorthand works\n");
    assert_eq!(llvm, run_shorthand(CALL_SHORTHAND_SRC, "cranelift"));
}

#[test]
fn tuple_field_init_shorthand_runs_byte_identical() {
    let llvm = run_shorthand(TUPLE_SHORTHAND_SRC, "llvm");
    assert_eq!(llvm, b"from a tuple\n");
    assert_eq!(llvm, run_shorthand(TUPLE_SHORTHAND_SRC, "cranelift"));
}

// ── binding transparency (relation `let`-aliases fold into one pushed query) ──

#[test]
fn binding_transparency_folds_to_single_pushed_query() {
    // Owned twin of hello-world-db: `gg` and `greeting` are transparent
    // relation aliases, so the decomposed `let gg = Greetings; gg where id = 1`
    // lowers to ONE pushed `SELECT … WHERE "id" = 1` — no `SELECT *` for the
    // unused/aliased `gg`, no in-process `where`.
    for backend in ["llvm", "cranelift"] {
        ensure_runtime_built();
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = seed_greetings_fixtures(tmp.path());
        let cd = tmp.path().join("bt.cd");
        std::fs::write(
            &cd,
            "program bt;\n\
             database greetings;\n\
             public relvar Greetings { id: Integer, message: Text } key { id };\n\
             oper main {} [\n\
                 let message = transaction [\n\
                     let gg = Greetings;\n\
                     let greeting = gg where id = 1;\n\
                     (extract greeting).message\n\
                 ];\n\
                 write_line { message };\n\
             ];\n",
        )
        .expect("write bt.cd");
        let log = tmp.path().join("audit.log");
        let out = coddl()
            .env("CODDL_GREETINGS_FILE", &db)
            .env("CODDL_AUDIT_LOG", &log)
            .args(["run", &format!("--backend={backend}")])
            .arg(&cd)
            .output()
            .expect("spawn coddl");
        assert!(
            out.status.success(),
            "binding transparency on {backend} failed: stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"hello world\n", "on {backend}");

        let log_txt = std::fs::read_to_string(&log).expect("read audit log");
        let selects: Vec<&str> = log_txt
            .lines()
            .filter(|l| l.contains("SELECT"))
            .collect();
        assert_eq!(
            selects.len(),
            1,
            "expected exactly one query on {backend}, got:\n{log_txt}"
        );
        assert!(
            selects[0].contains(r#"WHERE "id" = 1"#),
            "the single query should be the pushed filter on {backend}, got:\n{log_txt}"
        );
    }
}

#[test]
fn diagnostics_are_not_double_reported() {
    // `coddl run` typechecks the `.cd` in both the plan pass and lowering;
    // a diagnostic must still be printed exactly once, not twice.
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let cd = tmp.path().join("dup.cd");
    std::fs::write(
        &cd,
        "program dup;\noper main {} [ let greeting = 1; write_line { message: \"hi\" }; ];\n",
    )
    .expect("write dup.cd");
    let out = coddl().args(["run"]).arg(&cd).output().expect("spawn coddl");
    assert!(
        out.status.success(),
        "run failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hi\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("T0032").count(),
        1,
        "the unused-binding warning must print exactly once, got:\n{stderr}"
    );
}

#[test]
fn fmt_reformats_to_canonical_and_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let messy = tmp.path().join("messy.cd");
    std::fs::write(
        &messy,
        "program p;\noper   main {}[ write_line{message:\"hi\"} ; ];\n",
    )
    .expect("write messy.cd");
    let out = coddl().args(["fmt"]).arg(&messy).output().expect("spawn coddl");
    assert!(out.status.success(), "fmt failed: {:?}", out.status);
    let formatted = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        formatted,
        "program p;\noper main {} [\n    write_line { message: \"hi\" };\n];\n"
    );

    // Formatting the formatted output is byte-identical (idempotent).
    let clean = tmp.path().join("clean.cd");
    std::fs::write(&clean, &formatted).expect("write clean.cd");
    let out2 = coddl().args(["fmt"]).arg(&clean).output().expect("spawn coddl");
    assert_eq!(String::from_utf8(out2.stdout).expect("utf8"), formatted);
}

#[test]
fn public_relvar_outside_transaction_diagnoses_t0025() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cd_path = tmp.path().join("bad.cd");
    let cddb_path = tmp.path().join("greetings.cddb");
    let cdstore_path = tmp.path().join("greetings.cdstore");
    std::fs::write(
        &cd_path,
        "program bad; database greetings; \
         public relvar Greetings { id: Integer, message: Text } key { id }; \
         oper main {} [ let g = extract (Greetings where id = 1); ];",
    )
    .expect("write cd");
    std::fs::write(
        &cddb_path,
        "database greetings; base relvar Greetings { id: Integer, message: Text } key { id };",
    )
    .expect("write cddb");
    std::fs::write(
        &cdstore_path,
        "store for greetings; backend sqlite { file: \"x.sqlite\" }; \
         relvar Greetings: table \"g\" { columns: { id: \"id\", message: \"message\" } };",
    )
    .expect("write cdstore");
    let out = coddl()
        .args(["check"])
        .arg(&cd_path)
        .output()
        .expect("spawn coddl");
    assert!(!out.status.success(), "expected check to fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("T0025"),
        "stderr didn't carry T0025: {stderr}"
    );
}

#[test]
fn coddl_run_unknown_backend_fails_clearly() {
    // No `ensure_runtime_built()` needed — we never get to linking.
    let out = coddl()
        .args(["run", "--backend=foo"])
        .arg(hello_world_path())
        .output()
        .expect("spawn coddl");
    assert!(
        !out.status.success(),
        "expected failure, got success with stdout={:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown backend") && stderr.contains("foo"),
        "stderr didn't mention unknown backend: {stderr}"
    );
}

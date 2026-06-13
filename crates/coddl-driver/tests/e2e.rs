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

use std::path::PathBuf;
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

// ── Phase 22: hello-world-db (public relvar + SQLite) ────────────────

/// Build (or refresh) the `examples/hello-world-db/greetings.sqlite`
/// fixture via the shipped seed script. The script is idempotent: it
/// removes any existing `.sqlite` file before re-seeding. Required
/// because the file is gitignored. Guarded against concurrent
/// invocation so the parallel test scheduler doesn't race on the
/// `rm -f` + `sqlite3 ...` sequence.
fn ensure_hello_world_db_seeded() {
    use std::sync::OnceLock;
    static SEEDED: OnceLock<()> = OnceLock::new();
    SEEDED.get_or_init(|| {
        let script = workspace_root().join("examples/hello-world-db/seed-db.sh");
        assert!(
            script.exists(),
            "seed script missing at {}",
            script.display()
        );
        let status = Command::new("sh")
            .arg(&script)
            .status()
            .expect("invoke seed-db.sh");
        assert!(status.success(), "seed-db.sh failed");
    });
}

#[test]
fn hello_world_db_llvm_backend_prints_message() {
    ensure_runtime_built();
    ensure_hello_world_db_seeded();
    let cd = example_path("hello-world-db");
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=llvm {:?} failed: stderr=\n{}",
        cd,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.stdout, b"hello world\n");
}

#[test]
fn hello_world_db_cranelift_backend_prints_message() {
    ensure_runtime_built();
    ensure_hello_world_db_seeded();
    let cd = example_path("hello-world-db");
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=cranelift {:?} failed: stderr=\n{}",
        cd,
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.stdout, b"hello world\n");
}

#[test]
fn hello_world_db_byte_identical_across_backends() {
    ensure_runtime_built();
    ensure_hello_world_db_seeded();
    let cd = example_path("hello-world-db");
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&cd)
        .output()
        .expect("spawn coddl (llvm)");
    let cl = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(&cd)
        .output()
        .expect("spawn coddl (cranelift)");
    assert!(llvm.status.success(), "llvm run failed");
    assert!(cl.status.success(), "cranelift run failed");
    assert_eq!(
        llvm.stdout, cl.stdout,
        "byte equality violated:\n  llvm={:?}\n  cranelift={:?}",
        String::from_utf8_lossy(&llvm.stdout),
        String::from_utf8_lossy(&cl.stdout)
    );
}

#[test]
fn hello_world_db_env_var_override_picks_alternate_path() {
    ensure_runtime_built();
    ensure_hello_world_db_seeded();
    // Re-seed a parallel fixture with a different message; point the
    // env override at it; assert the override path actually flows
    // through to materialization. Same `.cd` source; different DB →
    // different stdout.
    let tmp = tempfile::tempdir().expect("tempdir");
    let alt = tmp.path().join("alt.sqlite");
    let alt_str = alt.display().to_string();
    let status = Command::new("sh")
        .args(["-c"])
        .arg(format!(
            "sqlite3 '{alt_str}' \"CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL); INSERT INTO greetings (id, message) VALUES (1, 'override hello');\""
        ))
        .status()
        .expect("invoke sqlite3");
    assert!(status.success(), "alt SQLite seed failed");

    let cd = example_path("hello-world-db");
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

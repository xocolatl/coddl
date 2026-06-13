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

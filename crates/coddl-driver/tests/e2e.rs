//! End-to-end tests for the `coddl` driver.
//!
//! Invokes the built `coddl` binary as a subprocess (located via the
//! `CARGO_BIN_EXE_coddl` env var that Cargo sets for integration
//! tests). Each test exercises one of the new subcommands —
//! `coddl run` or `coddl compile` — against `hello-world.cdl` and
//! asserts the resulting binary's stdout.
//!
//! Tests fail loudly if `clang` / `cc` is missing on PATH or if the
//! runtime staticlib hasn't been built.

use std::path::PathBuf;
use std::process::Command;

const HELLO_WORLD_REL: &str = "examples/hello-world/hello-world.cdl";

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn hello_world_path() -> PathBuf {
    workspace_root().join(HELLO_WORLD_REL)
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

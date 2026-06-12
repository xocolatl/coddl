//! End-to-end test for the LLVM backend.
//!
//! Lowers `examples/hello-world/hello-world.cd` to ProcIR, emits LLVM
//! IR text, invokes `clang` to compile + link with the runtime
//! staticlib, runs the binary, and asserts stdout equals
//! `"Hello, world!\n"`. Fails loudly if `clang` is missing on PATH or
//! if the runtime staticlib hasn't been built.

use std::path::PathBuf;
use std::process::Command;

use coddl_codegen_llvm::LlvmBackend;
use coddl_diagnostics::FileId;
use coddl_procir::{lower, Codegen};

const HELLO_WORLD: &str = "program hello_world;\n\
                           \n\
                           oper main {}\n\
                           [\n\
                               write_line{message: \"Hello, world!\"};\n\
                           ];\n";

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn runtime_staticlib() -> PathBuf {
    let path = workspace_root().join("target/debug/libcoddl_runtime.a");
    if !path.exists() {
        let status = Command::new("cargo")
            .args(["build", "-p", "coddl-runtime"])
            .current_dir(workspace_root())
            .status()
            .expect("invoke cargo");
        assert!(status.success(), "cargo build -p coddl-runtime failed");
    }
    assert!(
        path.exists(),
        "expected runtime staticlib at {}; build it with `cargo build -p coddl-runtime`",
        path.display()
    );
    path
}

#[test]
fn hello_world_llvm_e2e() {
    // 1. Lower.
    let lower_out = lower(HELLO_WORLD, FileId(0));
    assert!(
        lower_out.diagnostics.is_empty(),
        "lowering reported diagnostics: {:?}",
        lower_out.diagnostics
    );
    let module = lower_out.module.expect("module produced");

    // 2. Emit.
    let mut backend = LlvmBackend::new();
    let ir = backend.emit(&module).expect("emit ok");

    // 3. Write to temp.
    let tmp = tempfile::tempdir().expect("tempdir");
    let ir_path = tmp.path().join("hello.ll");
    let bin_path = tmp.path().join("hello");
    std::fs::write(&ir_path, &ir).expect("write IR");

    // 4. Link with runtime.
    let runtime = runtime_staticlib();
    let output = Command::new("clang")
        .arg(&ir_path)
        .arg(&runtime)
        .arg("-o")
        .arg(&bin_path)
        .output()
        .expect("invoke clang — is it on PATH?");
    assert!(
        output.status.success(),
        "clang failed:\nstderr:\n{}\nIR was:\n{ir}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 5. Run.
    let run = Command::new(&bin_path)
        .output()
        .expect("run compiled binary");
    assert!(
        run.status.success(),
        "binary exited with {}:\nstderr:\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        run.stdout,
        b"Hello, world!\n",
        "unexpected stdout: {:?}",
        String::from_utf8_lossy(&run.stdout)
    );
}

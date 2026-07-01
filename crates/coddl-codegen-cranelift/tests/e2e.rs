//! End-to-end test for the Cranelift backend.
//!
//! Lowers an inline hello-world program (the test owns its source) to
//! ProcIR, emits a native object file via Cranelift, invokes `cc` to link
//! with the runtime staticlib, runs the binary, and asserts stdout equals
//! `"Hello, world!\n"`. Fails loudly if `cc` is missing or the
//! runtime staticlib hasn't been built.

use std::path::PathBuf;
use std::process::Command;

use coddl_codegen_cranelift::CraneliftBackend;
use coddl_diagnostics::FileId;
use coddl_procir::{lower, Codegen};

const HELLO_WORLD: &str = "program hello_world;\n\
                           \n\
                           oper main {}\n\
                           [\n\
                               write_line{message: \"Hello, world!\"};\n\
                           ];\n";

// `cardinality { self: Sequence T }` reads the element count out of the RC
// header and prints it (via `to_text { self: Integer }`). Three elements →
// `3`. Uses the explicit prefix form; the dot receiver form is not wired yet.
const CARDINALITY: &str = "program p;\n\
                           oper main {}\n\
                           [\n\
                               let xs = Sequence [\"a\", \"b\", \"c\"];\n\
                               let n = cardinality { self: xs };\n\
                               write_line { message: to_text { self: n } };\n\
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

/// Lower `src`, emit a native object via Cranelift, link it with the runtime
/// staticlib via `cc`, run the binary, and return its stdout. Asserts each
/// step succeeds.
fn compile_and_run(src: &str) -> Vec<u8> {
    let lower_out = lower(src, FileId(0));
    assert!(
        lower_out.diagnostics.is_empty(),
        "lowering reported diagnostics: {:?}",
        lower_out.diagnostics
    );
    let module = lower_out.module.expect("module produced");

    let mut backend = CraneliftBackend::new().expect("ISA setup");
    let obj_bytes = backend.emit(&module).expect("emit ok");

    let tmp = tempfile::tempdir().expect("tempdir");
    let obj_path = tmp.path().join("prog.o");
    let bin_path = tmp.path().join("prog");
    std::fs::write(&obj_path, &obj_bytes).expect("write object");

    let runtime = runtime_staticlib();
    let output = Command::new("cc")
        .arg(&obj_path)
        .arg(&runtime)
        .arg("-o")
        .arg(&bin_path)
        .output()
        .expect("invoke cc — is it on PATH?");
    assert!(
        output.status.success(),
        "cc failed:\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let run = Command::new(&bin_path)
        .output()
        .expect("run compiled binary");
    assert!(
        run.status.success(),
        "binary exited with {}:\nstderr:\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    run.stdout
}

#[test]
fn hello_world_cranelift_e2e() {
    let stdout = compile_and_run(HELLO_WORLD);
    assert_eq!(
        stdout,
        b"Hello, world!\n",
        "unexpected stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn cardinality_cranelift_e2e() {
    let stdout = compile_and_run(CARDINALITY);
    assert_eq!(
        stdout,
        b"3\n",
        "unexpected stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
}

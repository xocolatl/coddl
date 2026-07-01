//! End-to-end test for the LLVM backend.
//!
//! Lowers an inline hello-world program (the test owns its source) to
//! ProcIR, emits LLVM IR text, invokes `clang` to compile + link with the
//! runtime staticlib, runs the binary, and asserts stdout equals
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

// `cardinality { self: Sequence T }` reads the element count out of the RC
// header and prints it (via `to_text { self: Integer }`). Three elements →
// `3`. Uses the explicit prefix form (`cardinality { self: xs }`); the dot
// receiver form is not wired yet.
const CARDINALITY: &str = "program p;\n\
                           oper main {}\n\
                           [\n\
                               let xs = Sequence [\"a\", \"b\", \"c\"];\n\
                               let n = cardinality { self: xs };\n\
                               write_line { message: to_text { self: n } };\n\
                           ];\n";

// Postfix sequence indexing `s[i]` — 0-based. `xs[1]` on `["Alice", "Bob"]`
// is the second element, printed directly. The element is retained into an
// owned copy so it outlives the sequence's scope-exit release.
const INDEX: &str = "program p;\n\
                     oper main {}\n\
                     [\n\
                         let xs = Sequence [\"Alice\", \"Bob\"];\n\
                         write_line { message: xs[1] };\n\
                     ];\n";

// An out-of-bounds index is a runtime error: `coddl_seq_index` aborts with a
// diagnostic and a non-zero exit.
const INDEX_OOB: &str = "program p;\n\
                         oper main {}\n\
                         [\n\
                             let xs = Sequence [\"Alice\", \"Bob\"];\n\
                             write_line { message: xs[5] };\n\
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

/// Lower `src`, emit LLVM IR, link it with the runtime staticlib via `clang`,
/// and run the binary. Asserts the lower/emit/link steps succeed; returns the
/// run's full `Output` **without** asserting on its exit status (so callers can
/// assert either success or a runtime abort).
fn run_program(src: &str) -> std::process::Output {
    // 1. Lower.
    let lower_out = lower(src, FileId(0));
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
    let ir_path = tmp.path().join("prog.ll");
    let bin_path = tmp.path().join("prog");
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
    Command::new(&bin_path)
        .output()
        .expect("run compiled binary")
}

/// Compile + run `src`, asserting a clean exit, and return its stdout.
fn compile_and_run(src: &str) -> Vec<u8> {
    let run = run_program(src);
    assert!(
        run.status.success(),
        "binary exited with {}:\nstderr:\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    run.stdout
}

#[test]
fn hello_world_llvm_e2e() {
    let stdout = compile_and_run(HELLO_WORLD);
    assert_eq!(
        stdout,
        b"Hello, world!\n",
        "unexpected stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn cardinality_llvm_e2e() {
    let stdout = compile_and_run(CARDINALITY);
    assert_eq!(
        stdout,
        b"3\n",
        "unexpected stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn index_llvm_e2e() {
    let stdout = compile_and_run(INDEX);
    assert_eq!(
        stdout,
        b"Bob\n",
        "unexpected stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn index_out_of_bounds_aborts_llvm_e2e() {
    let run = run_program(INDEX_OOB);
    assert!(
        !run.status.success(),
        "out-of-bounds index should exit non-zero, got {}",
        run.status
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("out of bounds"),
        "expected an out-of-bounds diagnostic on stderr, got: {stderr:?}"
    );
}

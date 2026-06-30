//! End-to-end tests for the `coddl` driver.
//!
//! Invokes the built `coddl` binary as a subprocess (located via the
//! `CARGO_BIN_EXE_coddl` env var that Cargo sets for integration
//! tests). Each test exercises one of the subcommands — `coddl run` or
//! `coddl compile` — against a program the suite **authors itself** (into
//! a tempdir; never a hand-editable on-disk scratchpad) and asserts the
//! resulting binary's stdout.
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

/// A process-lifetime tempdir holding the source programs the suite authors
/// for its subprocess `coddl` runs. A `OnceLock` keeps the `TempDir` alive for
/// the whole test binary, so the returned paths stay valid across runs. The
/// suite **owns every source it runs** — it never reads a hand-editable on-disk
/// scratchpad, which a developer may freely rewrite or delete.
fn fixtures_dir() -> &'static Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let tmp = tempfile::tempdir().expect("fixtures tempdir");
        for (name, src) in [
            ("hello-world", HELLO_WORLD_SRC),
            ("sequence-construct", SEQUENCE_CONSTRUCT_SRC),
            ("hello-everyone", HELLO_EVERYONE_SRC),
            ("transaction", TRANSACTION_SRC),
            ("join-times-compose", JOIN_TIMES_COMPOSE_SRC),
            ("union-intersect-minus", UNION_INTERSECT_MINUS_SRC),
            ("transitive-closure", TCLOSE_SRC),
        ] {
            std::fs::write(tmp.path().join(format!("{name}.cd")), src)
                .unwrap_or_else(|e| panic!("write {name}.cd fixture: {e}"));
        }
        tmp
    })
    .path()
}

/// Path to a suite-authored source program by name.
fn fixture_path(name: &str) -> PathBuf {
    fixtures_dir().join(format!("{name}.cd"))
}

fn hello_world_path() -> PathBuf {
    fixture_path("hello-world")
}

const HELLO_WORLD_SRC: &str = "\
program hello_world;
oper main {} [
    write_line { message: \"Hello, world!\" };
];
";

const SEQUENCE_CONSTRUCT_SRC: &str = "\
program sequence_construct;
oper main {} [
    let _names = Sequence [\"Alice\", \"Bob\"];
    write_line { message: \"constructed\" };
];
";

// A user-defined `to_text { self: Sequence Text }` overload (open overloading)
// that string interpolation dispatches to: `{names}` desugars to
// `to_text { self: names }`, picking the user overload for the sequence value.
const HELLO_EVERYONE_SRC: &str = "\
program hello_world;
oper to_text { self: Sequence Text } -> Text [ \"everyone\" ];
oper main {} [
    let names = Sequence [\"Alice\", \"Bob\"];
    let message = format { template: f\"Hello, {names}!\", params: { names } };
    write_line { message };
];
";

const TRANSACTION_SRC: &str = "\
program transaction_demo;
oper main {} [
    let ok = transaction [
        \"ok\"
    ];
    write_line { message: ok };
];
";

const JOIN_TIMES_COMPOSE_SRC: &str = "\
program join_times_compose;

private relvar Employees { emp_id: Integer, emp_name: Text, dept_id: Integer } key { emp_id };
private relvar Departments { dept_id: Integer, dept_name: Text } key { dept_id };
private relvar JobTitles { title: Text } key { title };
private relvar Locations { location: Text } key { location };

oper main {} [
    Departments := Relation {
        { dept_id: 10, dept_name: \"Engineering\" },
        { dept_id: 20, dept_name: \"Sales\" },
        { dept_id: 30, dept_name: \"Marketing\" },
    };
    Employees := Relation {
        { emp_id: 1, emp_name: \"Ada\", dept_id: 10 },
        { emp_id: 2, emp_name: \"Grace\", dept_id: 10 },
        { emp_id: 3, emp_name: \"Alan\", dept_id: 20 },
        { emp_id: 4, emp_name: \"Edsger\", dept_id: 30 },
    };
    JobTitles := Relation {
        { title: \"Engineer\" },
        { title: \"Manager\" },
    };
    Locations := Relation {
        { location: \"London\" },
        { location: \"Paris\" },
    };

    let staffed = Employees join Departments;
    write_relation { rel: staffed };
    let grid = JobTitles times Locations;
    write_relation { rel: grid };
    let dept_names = Employees compose Departments;
    write_relation { rel: dept_names };
    let eng = (Employees join Departments) where dept_name = \"Engineering\" project { emp_name, dept_name };
    write_relation { rel: eng };
];
";

const UNION_INTERSECT_MINUS_SRC: &str = "\
program union_intersect_minus;

private relvar Morning { id: Integer, name: Text } key { id };
private relvar Evening { id: Integer, name: Text } key { id };

oper main {} [
    Morning := Relation {
        { id: 1, name: \"Ada\" },
        { id: 2, name: \"Grace\" },
        { id: 3, name: \"Alan\" },
    };
    Evening := Relation {
        { id: 2, name: \"Grace\" },
        { id: 3, name: \"Alan\" },
        { id: 4, name: \"Edsger\" },
    };

    write_relation { rel: Morning };
    write_relation { rel: Evening };

    let both = Morning intersect Evening;
    write_relation { rel: both };

    let either = Morning union Evening;
    write_relation { rel: either };

    let morning_only = Morning minus Evening;
    write_relation { rel: morning_only };
];
";

const TCLOSE_SRC: &str = "\
program transitive_closure;

private relvar Edges { from: Integer, to: Integer } key { from, to };
private relvar Contains { major: Integer, minor: Integer, qty: Integer } key { major, minor };

oper main {} [
    Edges := Relation {
        { from: 1, to: 2 },
        { from: 2, to: 3 },
        { from: 3, to: 4 },
    };
    Contains := Relation {
        { major: 1, minor: 2, qty: 2 },
        { major: 1, minor: 3, qty: 1 },
        { major: 2, minor: 4, qty: 32 },
        { major: 3, minor: 5, qty: 1 },
    };

    write_relation { rel: Edges };

    let reachable = Edges tclose;
    write_relation { rel: reachable };

    let all_parts = Contains tclose { major, minor };
    write_relation { rel: all_parts };
];
";

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
fn coddl_run_llvm_constructs_sequence() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("sequence-construct"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=llvm failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"constructed\n");
}

#[test]
fn coddl_run_cranelift_constructs_sequence() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("sequence-construct"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=cranelift failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"constructed\n");
}

#[test]
fn coddl_run_llvm_interpolates_via_user_to_text() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("hello-everyone"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=llvm failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"Hello, everyone!\n");
}

#[test]
fn coddl_run_cranelift_interpolates_via_user_to_text() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("hello-everyone"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend=cranelift failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"Hello, everyone!\n");
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
        .arg(fixture_path("transaction"))
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
        .arg(fixture_path("transaction"))
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
        .arg(fixture_path("transaction"))
        .output()
        .expect("spawn LLVM");
    assert!(
        llvm.status.success(),
        "LLVM run failed: stderr=\n{}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("transaction"))
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
/// on a hand-editable on-disk scratchpad.
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

// ── arithmetic & concatenation (Chunk 1) ──────────────────────────────

/// Write `src` to a temp file, run it on both backends, and assert each
/// succeeds and produces exactly `expected` (so the backends also agree).
fn run_both_backends_expect(src: &str, name: &str, expected: &[u8]) {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_path = tmp.path().join(name);
    std::fs::write(&src_path, src).expect("write source");
    for backend in ["llvm", "cranelift"] {
        let out = coddl()
            .args(["run", &format!("--backend={backend}")])
            .arg(&src_path)
            .output()
            .expect("spawn coddl");
        assert!(
            out.status.success(),
            "{name} {backend} run failed: stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            out.stdout,
            expected,
            "{name} {backend} stdout mismatch: got {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn concat_text_and_char_prints_joined_text() {
    // `"Hi" || '!'` → `Hi!` — exercises Text||Character, CharToText, and the
    // runtime `coddl_text_concat` end to end.
    let src = "\
program concat_test;
oper main {} [
    write_line { message: \"Hi\" || '!' };
];
";
    run_both_backends_expect(src, "concat.cd", b"Hi!\n");
}

/// Like [`run_both_backends_expect`], but feeds `stdin` to the child's
/// standard input — for programs that call `read_line`.
fn run_both_backends_with_stdin(src: &str, name: &str, stdin: &[u8], expected: &[u8]) {
    use std::io::Write;
    use std::process::Stdio;
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_path = tmp.path().join(name);
    std::fs::write(&src_path, src).expect("write source");
    for backend in ["llvm", "cranelift"] {
        let mut child = coddl()
            .args(["run", &format!("--backend={backend}")])
            .arg(&src_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn coddl");
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(stdin)
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait coddl");
        assert!(
            out.status.success(),
            "{name} {backend} run failed: stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            out.stdout,
            expected,
            "{name} {backend} stdout mismatch: got {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn read_line_echoes_into_greeting() {
    // `read_line` prints the prompt (no newline), reads a stdin line with the
    // trailing newline stripped, and the value flows back through `||`.
    // Exercises the runtime `coddl_read_line` and the Text-return out-param
    // ABI (a builtin Call binding a `(ptr, len)` result) end to end.
    let src = "\
program greet;
oper main {} [
    let name = read_line { prompt: \"Name: \" };
    write_line { message: \"Hello, \" || name || \"!\" };
];
";
    run_both_backends_with_stdin(src, "read-line.cd", b"Vik\n", b"Name: Hello, Vik!\n");
}

#[test]
fn format_interpolates_read_line_into_greeting() {
    // The `format` intrinsic twin of `read_line_echoes_into_greeting`:
    // `f"Hello, {name}!"` + `params: { name }` desugars to the same
    // `to_text`/`||` chain. Exercises FORMAT_STRING_LIT lexing, the
    // `format` check + desugar, and `to_text` (Text identity) end to end.
    let src = "\
program greet_fmt;
oper main {} [
    let name = read_line { prompt: \"Name: \" };
    let message = format { template: f\"Hello, {name}!\", params: { name: name } };
    write_line { message };
];
";
    run_both_backends_with_stdin(src, "format-greet.cd", b"Vik\n", b"Name: Hello, Vik!\n");
}

#[test]
fn format_interpolates_a_character() {
    // A `Character` placeholder exercises the second `to_text` overload
    // (CharToText) inside `format`.
    let src = "\
program greet_char;
oper main {} [
    let message = format { template: f\"grade: {g}\", params: { g: 'A' } };
    write_line { message };
];
";
    run_both_backends_expect(src, "format-char.cd", b"grade: A\n");
}

#[test]
fn format_interpolates_an_integer() {
    // An `Integer` placeholder exercises the `to_text { self: Integer }`
    // overload → `coddl_int_to_text` end to end (overloading across types).
    let src = "\
program greet_int;
oper main {} [
    let message = format { template: f\"count: {n}\", params: { n: 7 } };
    write_line { message };
];
";
    run_both_backends_expect(src, "format-int.cd", b"count: 7\n");
}

#[test]
fn read_line_at_eof_yields_empty_text() {
    // Closed stdin (no bytes) → `read_line` returns the empty Text, so the
    // greeting collapses to just the bracketing literals. Confirms the
    // zero-length payload path crosses the ABI cleanly.
    let src = "\
program greet_eof;
oper main {} [
    let name = read_line { prompt: \"Name: \" };
    write_line { message: \"[\" || name || \"]\" };
];
";
    run_both_backends_with_stdin(src, "read-line-eof.cd", b"", b"Name: []\n");
}

#[test]
fn arithmetic_in_where_filters_in_process() {
    // `a + b > 4` over three rows keeps exactly `{a: 2, b: 3}` (sum 5). Runs
    // in-process (arithmetic predicates don't push to SQL).
    let src = "\
program arith_where;
oper main {} [
    let r = Relation { {a: 1, b: 1}, {a: 2, b: 3}, {a: 0, b: 0} };
    write_relation { rel: r where a + b > 4 };
];
";
    run_both_backends_expect(src, "arith-where.cd", b"{a: 2, b: 3}\n");
}

#[test]
fn integer_division_truncates_toward_zero() {
    // `5 / 2 = 2` (not 2.5): the row survives the predicate, observably
    // confirming integer (truncating) division.
    let src = "\
program div_trunc;
oper main {} [
    let r = Relation { {a: 5} };
    write_relation { rel: r where a / 2 = 2 };
];
";
    run_both_backends_expect(src, "div-trunc.cd", b"{a: 5}\n");
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
// `seed_greetings_fixtures`); none reads a hand-editable on-disk scratchpad,
// which a test must never depend on. End-to-end "a
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
/// This test **owns its source** rather than reading an on-disk scratchpad:
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

/// `coddl explain` is compile-time only (no runtime, no `run`): it dumps the
/// as-lowered RelIR for each SQL-pushed relational expression, paired with the
/// SQL it became. Assert the `project { message } (Greetings where id = 1)`
/// program surfaces its RelIR tree (project over restrict over the relvar leaf)
/// and the matching `SELECT`.
#[test]
fn explain_dumps_relir_tree_paired_with_its_sql() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (cd, _db) = write_pushdown_fixtures(tmp.path());

    let out = coddl()
        .args(["explain"])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl explain {:?} failed: stderr=\n{}",
        cd,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "Project { keep: message }",
        "Restrict { id = 1 }",
        "RelvarRef Greetings { db: greetings, table: greetings }",
        r#"SELECT "message" FROM "greetings" WHERE "id" = ?"#,
    ] {
        assert!(
            stdout.contains(needle),
            "explain output missing {needle:?}; got:\n{stdout}"
        );
    }
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

// ── surgical writes (relational assignment → DML) ─────────────────────

/// Seed a fresh two-row `greetings` db + its `.cddb`/`.cdstore` companions,
/// compile and run `program` on `backend`, then return the rows left in the
/// table afterwards as `"id|message"` lines sorted by id. Querying the
/// persisted file directly (via the `sqlite3` CLI) proves the write reached the
/// table, independent of any in-process read path. The suite owns its source —
/// it never reads `examples/`.
fn run_greetings_dml(backend: &str, program: &str) -> Vec<String> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
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
    let seed = Command::new("sqlite3")
        .arg(&db)
        .arg(
            "CREATE TABLE greetings (id INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (id)); \
             INSERT INTO greetings (id, message) VALUES (1, 'hello world'), (2, 'goodbye');",
        )
        .status()
        .expect("invoke sqlite3");
    assert!(seed.success(), "greetings DML fixture seed failed");

    let cd = dir.join("dml.cd");
    std::fs::write(&cd, program).expect("write dml.cd");

    let out = coddl()
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

    let q = Command::new("sqlite3")
        .arg(&db)
        .arg("SELECT id || '|' || message FROM greetings ORDER BY id")
        .output()
        .expect("sqlite3 read-back");
    assert!(
        q.status.success(),
        "sqlite3 read-back failed: {}",
        String::from_utf8_lossy(&q.stderr)
    );
    String::from_utf8_lossy(&q.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// `R := R minus (R where id = 1)` inside a transaction emits a surgical
/// `DELETE FROM greetings WHERE id = ?` that persists — only the id=2 row
/// survives. Same result on both backends.
fn assert_delete_where_persists(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := Greetings minus (Greetings where id = 1); ];\n\
         ];\n",
    );
    assert_eq!(rows, vec!["2|goodbye".to_string()], "backend={backend}");
}

#[test]
fn dml_delete_where_persists_llvm() {
    assert_delete_where_persists("llvm");
}

#[test]
fn dml_delete_where_persists_cranelift() {
    assert_delete_where_persists("cranelift");
}

/// `R := R minus R` empties the relvar with a whole-table `DELETE FROM
/// greetings`. No rows survive.
fn assert_self_truncate_empties(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := Greetings minus Greetings; ];\n\
         ];\n",
    );
    assert!(rows.is_empty(), "expected empty table on {backend}, got {rows:?}");
}

#[test]
fn dml_self_truncate_empties_llvm() {
    assert_self_truncate_empties("llvm");
}

#[test]
fn dml_self_truncate_empties_cranelift() {
    assert_self_truncate_empties("cranelift");
}

/// `truncate R;` is sugar for `R := R minus R` — it desugars to the same
/// whole-table `DELETE FROM greetings` and empties the relvar. No rows survive.
fn assert_truncate_stmt_empties(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ truncate Greetings; ];\n\
         ];\n",
    );
    assert!(rows.is_empty(), "expected empty table on {backend}, got {rows:?}");
}

#[test]
fn dml_truncate_stmt_empties_llvm() {
    assert_truncate_stmt_empties("llvm");
}

#[test]
fn dml_truncate_stmt_empties_cranelift() {
    assert_truncate_stmt_empties("cranelift");
}

/// `delete R where p;` is sugar for `R := R minus (R where p)` — it desugars to
/// the same surgical `DELETE FROM greetings WHERE id = ?`, so only the id=2 row
/// survives. Same result on both backends.
fn assert_delete_stmt_persists(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ delete Greetings where id = 1; ];\n\
         ];\n",
    );
    assert_eq!(rows, vec!["2|goodbye".to_string()], "backend={backend}");
}

#[test]
fn dml_delete_stmt_persists_llvm() {
    assert_delete_stmt_persists("llvm");
}

#[test]
fn dml_delete_stmt_persists_cranelift() {
    assert_delete_stmt_persists("cranelift");
}

/// `insert R { {…} };` is sugar for `R := R union Relation { {…} }` — the
/// tuple-set's rows ship into greetings idempotently. A new id (3) is added;
/// re-inserting an existing tuple (id 1) is a no-op. Same result on both
/// backends.
fn assert_insert_stmt_persists(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [\n\
                 insert Greetings { {id: 3, message: \"howdy\"}, {id: 1, message: \"hello world\"} };\n\
             ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec![
            "1|hello world".to_string(),
            "2|goodbye".to_string(),
            "3|howdy".to_string(),
        ],
        "backend={backend}"
    );
}

#[test]
fn dml_insert_stmt_persists_llvm() {
    assert_insert_stmt_persists("llvm");
}

#[test]
fn dml_insert_stmt_persists_cranelift() {
    assert_insert_stmt_persists("cranelift");
}

/// `update R where p { c: e };` is sugar for the substitute-union shape — a
/// surgical `UPDATE greetings SET message = ? WHERE id = ?`. Only the id=1 row's
/// message changes; id=2 is untouched. Same result on both backends.
fn assert_update_stmt_persists(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ update Greetings where id = 1 { message: \"hi!\" }; ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec!["1|hi!".to_string(), "2|goodbye".to_string()],
        "backend={backend}"
    );
}

#[test]
fn dml_update_stmt_persists_llvm() {
    assert_update_stmt_persists("llvm");
}

#[test]
fn dml_update_stmt_persists_cranelift() {
    assert_update_stmt_persists("cranelift");
}

/// Binding transparency: `let r = R where id = 1; R := R minus r` folds to the
/// same `DELETE … WHERE id = ?` as the inline form — the alias is substituted
/// before recognition, so it persists identically.
fn assert_delete_via_binding(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [\n\
                 let r = Greetings where id = 1;\n\
                 Greetings := Greetings minus r;\n\
             ];\n\
         ];\n",
    );
    assert_eq!(rows, vec!["2|goodbye".to_string()], "backend={backend}");
}

#[test]
fn dml_delete_via_binding_transparency_llvm() {
    assert_delete_via_binding("llvm");
}

#[test]
fn dml_delete_via_binding_transparency_cranelift() {
    assert_delete_via_binding("cranelift");
}

/// Seed a fresh db with two same-heading tables — `greetings` (ids 1..4) and
/// `stale` (ids 2,3, the tuples to purge) — plus a `.cddb`/`.cdstore` declaring
/// both relvars, run `program` on `backend`, and return the surviving
/// `greetings` rows as `"id|message"` lines sorted by id. The suite owns its
/// source — it never reads `examples/`.
fn run_two_relvar_dml(backend: &str, program: &str) -> Vec<String> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    std::fs::write(
        dir.join("greetings.cddb"),
        "database greetings;\n\
         base relvar Greetings { id: Integer, message: Text } key { id };\n\
         base relvar Stale { id: Integer, message: Text } key { id };\n",
    )
    .expect("write greetings.cddb");
    std::fs::write(
        dir.join("greetings.cdstore"),
        "store for greetings;\n\
         backend sqlite { file: \"greetings.sqlite\" };\n\
         relvar Greetings: table \"greetings\" { columns: { id: \"id\", message: \"message\" } };\n\
         relvar Stale: table \"stale\" { columns: { id: \"id\", message: \"message\" } };\n",
    )
    .expect("write greetings.cdstore");
    let db = dir.join("greetings.sqlite");
    let seed = Command::new("sqlite3")
        .arg(&db)
        .arg(
            "CREATE TABLE greetings (id INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (id)); \
             CREATE TABLE stale (id INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (id)); \
             INSERT INTO greetings (id, message) VALUES \
               (1, 'hello world'), (2, 'goodbye'), (3, 'farewell'), (4, 'so long'); \
             INSERT INTO stale (id, message) VALUES (2, 'goodbye'), (3, 'farewell');",
        )
        .status()
        .expect("invoke sqlite3");
    assert!(seed.success(), "two-relvar DML fixture seed failed");

    let cd = dir.join("dml.cd");
    std::fs::write(&cd, program).expect("write dml.cd");

    let out = coddl()
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

    let q = Command::new("sqlite3")
        .arg(&db)
        .arg("SELECT id || '|' || message FROM greetings ORDER BY id")
        .output()
        .expect("sqlite3 read-back");
    assert!(
        q.status.success(),
        "sqlite3 read-back failed: {}",
        String::from_utf8_lossy(&q.stderr)
    );
    String::from_utf8_lossy(&q.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// `R := R minus S` (two same-heading relvars) emits an anti-join
/// `DELETE FROM greetings WHERE EXISTS (... stale ...)` that persists — every
/// greetings tuple also in stale (ids 2, 3) is removed, leaving ids 1 and 4.
fn assert_anti_join_minus_relvar_persists(backend: &str) {
    let rows = run_two_relvar_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         public relvar Stale { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := Greetings minus Stale; ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec!["1|hello world".to_string(), "4|so long".to_string()],
        "backend={backend}"
    );
}

#[test]
fn dml_anti_join_minus_relvar_llvm() {
    assert_anti_join_minus_relvar_persists("llvm");
}

#[test]
fn dml_anti_join_minus_relvar_cranelift() {
    assert_anti_join_minus_relvar_persists("cranelift");
}

/// Seed `greetings` (ids 1,2) and a same-heading `new_arrivals` (id 2 — already
/// present — and id 3 — new), declare both relvars, run `program`, and return
/// the surviving `greetings` rows. The suite owns its source.
fn run_union_dml(backend: &str, program: &str) -> Vec<String> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    std::fs::write(
        dir.join("greetings.cddb"),
        "database greetings;\n\
         base relvar Greetings { id: Integer, message: Text } key { id };\n\
         base relvar NewArrivals { id: Integer, message: Text } key { id };\n",
    )
    .expect("write greetings.cddb");
    std::fs::write(
        dir.join("greetings.cdstore"),
        "store for greetings;\n\
         backend sqlite { file: \"greetings.sqlite\" };\n\
         relvar Greetings: table \"greetings\" { columns: { id: \"id\", message: \"message\" } };\n\
         relvar NewArrivals: table \"new_arrivals\" { columns: { id: \"id\", message: \"message\" } };\n",
    )
    .expect("write greetings.cdstore");
    let db = dir.join("greetings.sqlite");
    let seed = Command::new("sqlite3")
        .arg(&db)
        .arg(
            "CREATE TABLE greetings (id INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (id)); \
             CREATE TABLE new_arrivals (id INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (id)); \
             INSERT INTO greetings (id, message) VALUES (1, 'hello world'), (2, 'goodbye'); \
             INSERT INTO new_arrivals (id, message) VALUES (2, 'goodbye'), (3, 'farewell');",
        )
        .status()
        .expect("invoke sqlite3");
    assert!(seed.success(), "union DML fixture seed failed");

    let cd = dir.join("dml.cd");
    std::fs::write(&cd, program).expect("write dml.cd");

    let out = coddl()
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

    let q = Command::new("sqlite3")
        .arg(&db)
        .arg("SELECT id || '|' || message FROM greetings ORDER BY id")
        .output()
        .expect("sqlite3 read-back");
    assert!(q.status.success(), "sqlite3 read-back failed");
    String::from_utf8_lossy(&q.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// `R := R union S` emits an idempotent `INSERT … WHERE NOT EXISTS`: the new
/// tuple (id 3) is added, the already-present one (id 2) is a no-op.
fn assert_union_relvar_inserts_idempotently(backend: &str) {
    let rows = run_union_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         public relvar NewArrivals { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := Greetings union NewArrivals; ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec![
            "1|hello world".to_string(),
            "2|goodbye".to_string(),
            "3|farewell".to_string(),
        ],
        "backend={backend}"
    );
}

#[test]
fn dml_union_relvar_inserts_idempotently_llvm() {
    assert_union_relvar_inserts_idempotently("llvm");
}

#[test]
fn dml_union_relvar_inserts_idempotently_cranelift() {
    assert_union_relvar_inserts_idempotently("cranelift");
}

/// `R := R union Relation { … }` — the right operand is an in-memory relation
/// literal (not SQL-backed), so its rows are shipped from the process into the
/// table via a batched `VALUES` insert (`coddl_exec_insert`). The literal here
/// has one already-present tuple (id 2, a no-op) and one new (id 3).
fn assert_union_literal_inserts_idempotently(backend: &str) {
    // `run_greetings_dml` seeds greetings with ids 1 ('hello world') and 2
    // ('goodbye'); the union literal repeats id 2 and adds id 3.
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [\n\
                 Greetings := Greetings union Relation {\n\
                     { id: 2, message: \"goodbye\" },\n\
                     { id: 3, message: \"farewell\" },\n\
                 };\n\
             ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec![
            "1|hello world".to_string(),
            "2|goodbye".to_string(),
            "3|farewell".to_string(),
        ],
        "backend={backend}"
    );
}

#[test]
fn dml_union_literal_inserts_idempotently_llvm() {
    assert_union_literal_inserts_idempotently("llvm");
}

#[test]
fn dml_union_literal_inserts_idempotently_cranelift() {
    assert_union_literal_inserts_idempotently("cranelift");
}

// ── comparison-predicate pushdown (`<>` `<` `<=` `>` `>=`) ─────────────

/// Read `Greetings where <pred> project {message}` (a predicate matching
/// exactly one of the two seeded rows), and assert (a) it printed `expect_msg`
/// and (b) the comparison **pushed** — the audit log shows one filtered
/// `greetings` query carrying `expect_op`, and no full-table scan. Proves the
/// operator goes typecheck → push → run, not just that the result is correct.
fn assert_comparison_pushes(backend: &str, pred: &str, expect_msg: &str, expect_op: &str) {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db = seed_greetings_fixtures(dir); // companions + (1, 'hello world')
    let add = Command::new("sqlite3")
        .arg(&db)
        .arg("INSERT INTO greetings (id, message) VALUES (2, 'goodbye');")
        .status()
        .expect("invoke sqlite3");
    assert!(add.success(), "add second greetings row");

    let cd = dir.join("cmp.cd");
    std::fs::write(
        &cd,
        format!(
            "program p;\n\
             database greetings;\n\
             public relvar Greetings {{ id: Integer, message: Text }} key {{ id }};\n\
             oper main {{}} [\n\
                 let g = transaction [ extract (Greetings where {pred} project {{ message }}) ];\n\
                 write_line {{ message: g.message }};\n\
             ];\n"
        ),
    )
    .expect("write cmp.cd");

    let log = dir.join("audit.log");
    let out = coddl()
        .env("CODDL_AUDIT_LOG", &log)
        .env("CODDL_GREETINGS_FILE", &db)
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "coddl run --backend={backend} (pred {pred}) failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        out.stdout,
        format!("{expect_msg}\n").into_bytes(),
        "pred {pred} on {backend}: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let contents = std::fs::read_to_string(&log).expect("read audit log");
    let sqls: Vec<&str> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| audit_sql(l).unwrap_or_else(|| panic!("malformed audit line: {l:?}")))
        .collect();
    // No unfiltered scan of greetings.
    assert!(
        !sqls.iter().any(|s| s.contains("greetings") && !s.contains("WHERE")),
        "unexpected full scan (pred {pred}, {backend}): {sqls:?}"
    );
    // Exactly one pushed query, carrying the comparison operator.
    let pushed: Vec<&&str> = sqls
        .iter()
        .filter(|s| s.contains("greetings") && s.contains(expect_op))
        .collect();
    assert_eq!(
        pushed.len(),
        1,
        "expected one pushed `{expect_op}` query (pred {pred}, {backend}), got {sqls:?}"
    );
}

#[test]
fn comparison_ne_pushes_llvm() {
    assert_comparison_pushes("llvm", "id <> 1", "goodbye", "<>");
}

#[test]
fn comparison_ne_pushes_cranelift() {
    assert_comparison_pushes("cranelift", "id <> 1", "goodbye", "<>");
}

#[test]
fn comparison_lt_pushes_llvm() {
    assert_comparison_pushes("llvm", "id < 2", "hello world", "<");
}

#[test]
fn comparison_lt_pushes_cranelift() {
    assert_comparison_pushes("cranelift", "id < 2", "hello world", "<");
}

// ── surgical UPDATE (substitute-union recognition) ────────────────────

/// `R := (R where id <> 1) union ((R where id = 1) replace { message: message
/// || "!" })` — TTM's UPDATE expansion — emits `UPDATE greetings SET message =
/// (message || '!') WHERE id = ?`. Only the matching row (id 1) changes. The
/// `update` sugar desugars to the heading-preserving substitute-union the UPDATE
/// recognition matches (a computed value that reads the target attribute).
fn assert_update_where_persists(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ update Greetings where id = 1 { message: message || \"!\" }; ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec!["1|hello world!".to_string(), "2|goodbye".to_string()],
        "backend={backend}"
    );
}

#[test]
fn dml_update_where_persists_llvm() {
    assert_update_where_persists("llvm");
}

#[test]
fn dml_update_where_persists_cranelift() {
    assert_update_where_persists("cranelift");
}

/// Update-all (no `where`) updates every row — a bare substitute → `UPDATE
/// greetings SET …` (no WHERE). The `update` sugar without a `where` clause.
fn assert_update_all_persists(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ update Greetings { message: message || \"!\" }; ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec!["1|hello world!".to_string(), "2|goodbye!".to_string()],
        "backend={backend}"
    );
}

#[test]
fn dml_update_all_persists_llvm() {
    assert_update_all_persists("llvm");
}

#[test]
fn dml_update_all_persists_cranelift() {
    assert_update_all_persists("cranelift");
}

// ── keep-filter delete, semi-minus intersect, replace-all ─────────────

/// `R := R where id <> 1` keeps the matching rows by deleting their complement:
/// a surgical `DELETE FROM greetings WHERE id = ?` (the negation of the filter),
/// not a wipe. The kept row (id 2) survives; the filtered-out row (id 1) is gone.
fn assert_keep_filter_deletes_complement(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := Greetings where id <> 1; ];\n\
         ];\n",
    );
    assert_eq!(rows, vec!["2|goodbye".to_string()], "backend={backend}");
}

#[test]
fn dml_keep_filter_deletes_complement_llvm() {
    assert_keep_filter_deletes_complement("llvm");
}

#[test]
fn dml_keep_filter_deletes_complement_cranelift() {
    assert_keep_filter_deletes_complement("cranelift");
}

/// `R := R intersect S` keeps the tuples present in both by deleting the
/// `R`-rows with no match in `S`: a semi-minus `DELETE FROM greetings WHERE NOT
/// EXISTS (… stale …)`. greetings (ids 1..4) ∩ stale (ids 2,3) leaves ids 2, 3.
fn assert_intersect_semi_minus_persists(backend: &str) {
    let rows = run_two_relvar_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         public relvar Stale { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := Greetings intersect Stale; ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec!["2|goodbye".to_string(), "3|farewell".to_string()],
        "backend={backend}"
    );
}

#[test]
fn dml_intersect_semi_minus_persists_llvm() {
    assert_intersect_semi_minus_persists("llvm");
}

#[test]
fn dml_intersect_semi_minus_persists_cranelift() {
    assert_intersect_semi_minus_persists("cranelift");
}

/// `R := S` (target absent from the RHS) is a replace-all: truncate `greetings`,
/// then `INSERT … SELECT` from the pushable source. greetings (ids 1,2) becomes
/// new_arrivals (ids 2,3) — a pure-SQL two-statement transaction, no hydration.
fn assert_replace_all_pushable_persists(backend: &str) {
    let rows = run_union_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         public relvar NewArrivals { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := NewArrivals; ];\n\
         ];\n",
    );
    assert_eq!(
        rows,
        vec!["2|goodbye".to_string(), "3|farewell".to_string()],
        "backend={backend}"
    );
}

#[test]
fn dml_replace_all_pushable_persists_llvm() {
    assert_replace_all_pushable_persists("llvm");
}

#[test]
fn dml_replace_all_pushable_persists_cranelift() {
    assert_replace_all_pushable_persists("cranelift");
}

/// `R := Relation { … }` (a literal, target absent) is a replace-all by shipping:
/// truncate `greetings`, then ship the literal's rows from the process (the empty
/// table makes the idempotent template always insert). Only the reset row remains.
fn assert_replace_all_ship_persists(backend: &str) {
    let rows = run_greetings_dml(
        backend,
        "program insert_update_delete;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [\n\
             transaction [ Greetings := Relation { { id: 9, message: \"reset\" } }; ];\n\
         ];\n",
    );
    assert_eq!(rows, vec!["9|reset".to_string()], "backend={backend}");
}

#[test]
fn dml_replace_all_ship_persists_llvm() {
    assert_replace_all_ship_persists("llvm");
}

#[test]
fn dml_replace_all_ship_persists_cranelift() {
    assert_replace_all_ship_persists("cranelift");
}

// ── extend pushdown ───────────────────────────────────────────────────

/// Write the `sales` database companions and seed a SQLite db with two rows.
/// Returns the db path; the caller writes its own `.cd` (with `database sales;`)
/// alongside. The suite owns this fixture — it never reads `examples/`.
fn seed_sales_fixtures(dir: &Path) -> PathBuf {
    std::fs::write(
        dir.join("sales.cddb"),
        "database sales;\n\
         base relvar Sales { id: Integer, customer: Text, item: Text, unit_cents: Integer, qty: Integer } key { id };\n",
    )
    .expect("write sales.cddb");
    std::fs::write(
        dir.join("sales.cdstore"),
        "store for sales;\n\
         backend sqlite { file: \"sales.sqlite\" };\n\
         relvar Sales: table \"sales\" { columns: { id, customer, item, unit_cents, qty } };\n",
    )
    .expect("write sales.cdstore");

    let db = dir.join("sales.sqlite");
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "sqlite3 '{}' \"CREATE TABLE sales (id INTEGER NOT NULL, customer TEXT NOT NULL, item TEXT NOT NULL, unit_cents INTEGER NOT NULL, qty INTEGER NOT NULL, PRIMARY KEY (id)); INSERT INTO sales (id, customer, item, unit_cents, qty) VALUES (1, 'ada', 'widget', 500, 3), (2, 'bo', 'gadget', 800, 2);\"",
            db.display()
        ))
        .status()
        .expect("invoke sqlite3");
    assert!(status.success(), "sales fixture seed failed");
    db
}

/// Run a relvar-rooted `extend` over the seeded `sales` db on `backend`: assert
/// the computed `line_cents = unit_cents * qty` column appears in the output
/// tuple set, and that `CODDL_AUDIT_LOG` proves the computed column pushed to
/// SQL (`("unit_cents" * "qty") AS "line_cents"`).
fn assert_extend_pushdown(backend: &str) {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = seed_sales_fixtures(tmp.path());
    let cd = tmp.path().join("ext.cd");
    std::fs::write(
        &cd,
        "program sales_db;\n\
         database sales;\n\
         public relvar Sales { id: Integer, customer: Text, item: Text, unit_cents: Integer, qty: Integer } key { id };\n\
         oper main {} [\n\
             let p = transaction [ Sales extend { line_cents: unit_cents * qty } ];\n\
             write_relation { rel: p };\n\
         ];\n",
    )
    .expect("write ext.cd");
    let log = tmp.path().join("audit.log");

    let out = coddl()
        .env("CODDL_AUDIT_LOG", &log)
        .env("CODDL_SALES_FILE", &db)
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "extend pushdown on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(&[
            r#"{customer: "ada", id: 1, item: "widget", line_cents: 1500, qty: 3, unit_cents: 500}"#,
            r#"{customer: "bo", id: 2, item: "gadget", line_cents: 1600, qty: 2, unit_cents: 800}"#,
        ]),
        "unexpected extend output on {backend}",
    );

    let contents = std::fs::read_to_string(&log).expect("read audit log");
    let sqls: Vec<&str> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| audit_sql(l).unwrap_or_else(|| panic!("malformed audit line ({backend}): {l:?}")))
        .collect();
    assert!(
        sqls.iter()
            .any(|s| s.contains(r#"("unit_cents" * "qty") AS "line_cents""#)),
        "expected the computed extend column in pushed SQL on {backend}; got {sqls:?}",
    );
}

#[test]
fn extend_pushdown_audit_llvm() {
    assert_extend_pushdown("llvm");
}

#[test]
fn extend_pushdown_audit_cranelift() {
    assert_extend_pushdown("cranelift");
}

/// Run a materialized (in-memory) `extend` on `backend` and return stdout: an
/// Integer arithmetic extend over a relation literal, then a Text concatenation
/// extend — both computed per-tuple in-process.
fn run_in_process_extend(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let cd = tmp.path().join("inproc-extend.cd");
    std::fs::write(
        &cd,
        "program p;\n\
         oper main {} [\n\
             let s = Relation { {a: 1, b: 2}, {a: 3, b: 1} } extend { sum: a + b };\n\
             write_relation { rel: s };\n\
             let t = Relation { {x: \"wid\", y: \"get\"} } extend { word: x || y };\n\
             write_relation { rel: t };\n\
         ];\n",
    )
    .expect("write inproc-extend.cd");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "in-process extend on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn extend_in_process_computes_integer_and_text() {
    let expected = sorted_tuples(&[
        "{a: 1, b: 2, sum: 3}",
        "{a: 3, b: 1, sum: 4}",
        r#"{word: "widget", x: "wid", y: "get"}"#,
    ]);
    for backend in ["llvm", "cranelift"] {
        assert_eq!(
            tuple_lines(&run_in_process_extend(backend)),
            expected,
            "in-process extend output on {backend}"
        );
    }
}

#[test]
fn extend_boolean_value_fails_with_t0046() {
    // Only Integer/Text are representable as relation cells in v1; a Boolean
    // (comparison) value is rejected at typecheck.
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let cd = tmp.path().join("boolext.cd");
    std::fs::write(
        &cd,
        "program p;\noper main {} [ let _s = Relation { {a: 1, b: 2} } extend {c: a = b}; ];\n",
    )
    .expect("write boolext.cd");
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(!out.status.success(), "Boolean-valued extend should fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("T0046"),
        "expected T0046, got stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── general-expression replace (compute + consume) ────────────────────

/// Run an in-memory general `replace` on `backend`: collapse (consume the read
/// attrs), in-place (`x: f(x)`), and concat-collapse — all computed in-process.
fn run_in_process_general_replace(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let cd = tmp.path().join("gen-replace.cd");
    std::fs::write(
        &cd,
        "program p;\n\
         oper main {} [\n\
             let s = Relation { {a: 1, b: 2}, {a: 3, b: 1} };\n\
             write_relation { rel: s replace { c: a * b } };\n\
             write_relation { rel: s replace { a: a + 1 } };\n\
             let t = Relation { {x: \"wid\", y: \"get\"} };\n\
             write_relation { rel: t replace { z: x || y } };\n\
         ];\n",
    )
    .expect("write gen-replace.cd");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "in-process general replace on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn general_replace_in_process_computes_and_consumes() {
    let expected = sorted_tuples(&[
        "{c: 2}",  // a*b for {1,2}; a,b consumed
        "{c: 3}",  // a*b for {3,1}
        "{a: 2, b: 2}", // a+1 in place
        "{a: 4, b: 1}",
        r#"{z: "widget"}"#, // x||y; x,y consumed
    ]);
    for backend in ["llvm", "cranelift"] {
        assert_eq!(
            tuple_lines(&run_in_process_general_replace(backend)),
            expected,
            "in-process general replace output on {backend}"
        );
    }
}

/// Push a general `replace` over the seeded `sales` db on `backend`: assert the
/// collapse output + that `CODDL_AUDIT_LOG` shows the computed column pushed
/// with the consumed columns absent.
fn assert_general_replace_pushdown(backend: &str) {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = seed_sales_fixtures(tmp.path());
    let cd = tmp.path().join("rep.cd");
    std::fs::write(
        &cd,
        "program sales_db;\n\
         database sales;\n\
         public relvar Sales { id: Integer, customer: Text, item: Text, unit_cents: Integer, qty: Integer } key { id };\n\
         oper main {} [\n\
             let p = transaction [ Sales replace { line_cents: unit_cents * qty } ];\n\
             write_relation { rel: p };\n\
         ];\n",
    )
    .expect("write rep.cd");
    let log = tmp.path().join("audit.log");

    let out = coddl()
        .env("CODDL_AUDIT_LOG", &log)
        .env("CODDL_SALES_FILE", &db)
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "general replace pushdown on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    // unit_cents/qty consumed → only id, customer, item, line_cents survive.
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(&[
            r#"{customer: "ada", id: 1, item: "widget", line_cents: 1500}"#,
            r#"{customer: "bo", id: 2, item: "gadget", line_cents: 1600}"#,
        ]),
        "general replace output on {backend}",
    );
    let contents = std::fs::read_to_string(&log).expect("read audit log");
    let sqls: Vec<&str> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| audit_sql(l).unwrap_or_else(|| panic!("malformed audit line ({backend}): {l:?}")))
        .collect();
    assert!(
        sqls.iter().any(|s| {
            s.contains(r#"("unit_cents" * "qty") AS "line_cents""#)
                && !s.contains(r#""unit_cents", "qty""#)
        }),
        "expected the pushed computed column with consumed cols absent on {backend}; got {sqls:?}",
    );
}

#[test]
fn general_replace_pushdown_llvm() {
    assert_general_replace_pushdown("llvm");
}

#[test]
fn general_replace_pushdown_cranelift() {
    assert_general_replace_pushdown("cranelift");
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
    // `Greetings where id = 1 rename {identifier: id, msg: message}` pushes to
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
             oper main {} [ let g = transaction [ extract (Greetings where id = 1 rename {identifier: id, msg: message}) ]; write_line { message: g.msg }; ];\n",
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
            "pushed replace on {backend} failed: stderr=\n{}",
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
/// heading from `{a, b}` to `{b, z}`, so the runtime must *permute* record bytes
/// into the new canonical layout, not just relabel. Output is sealed in
/// `{b, z}` order: `{b: 10, z: 1}` then `{b: 20, z: 2}`.
const RENAME_INPROCESS_SRC: &str = "\
program rename_inprocess;
oper main {} [
    let r = Relation { {a: 1, b: 10}, {a: 2, b: 20} };
    let s = r rename {z: a};
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
    assert_eq!(
        run_rename_inprocess("llvm"),
        run_rename_inprocess("cranelift")
    );
}

#[test]
fn rename_inprocess_after_transaction_escape() {
    // Owned twin of the hello-world example: a pushed rename whose relation
    // *escapes* the transaction as the block's tail value (a `let`-bound
    // local), then a second, in-process rename over that local, then extract +
    // print. Covers both the in-process rename path and a relation surviving
    // as a transaction's return value.
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
             oper main {} [ let g = transaction [ let x = Greetings where id = 1 rename {identifier: id, msg: message}; x ]; let g2 = g rename {the_message: msg}; let t = extract g2; write_line { message: t.the_message }; ];\n",
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
    // Owned twin of the hello-world db example: `gg` and `greeting` are transparent
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

// ── join-times-compose (in-process, private relvars; M1b parity) ──────────

/// Parse `write_relation` stdout into a **sorted** `Vec` of tuple-lines. A
/// relation is a set with no tuple order (RM Pro 1), and each tuple renders
/// identically on both backends (canonical heading order), so two relations'
/// outputs are equal iff their line *sets* match — never their raw byte order.
/// (For the all-`Text` product below the seal even orders by string pointer, so
/// the line order genuinely differs across backends; that's harmless here.)
fn tuple_lines(stdout: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(stdout);
    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
    lines.sort();
    lines
}

/// The expected `&[&str]` tuple set, sorted for comparison with `tuple_lines`.
fn sorted_tuples(tuples: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = tuples.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

/// The in-process twin populates four `private` relvars, then dumps the natural
/// join `Employees join Departments` (on `dept_id`), the Cartesian product
/// `JobTitles times Locations`, and the composition `Employees compose
/// Departments` (join on `dept_id`, then drop it). Tuple order is unspecified
/// (RM Pro 1), so the tests compare this set, not bytes.
const JOIN_TIMES_COMPOSE_TUPLES: &[&str] = &[
    // Employees join Departments
    "{dept_id: 10, dept_name: \"Engineering\", emp_id: 1, emp_name: \"Ada\"}",
    "{dept_id: 10, dept_name: \"Engineering\", emp_id: 2, emp_name: \"Grace\"}",
    "{dept_id: 20, dept_name: \"Sales\", emp_id: 3, emp_name: \"Alan\"}",
    "{dept_id: 30, dept_name: \"Marketing\", emp_id: 4, emp_name: \"Edsger\"}",
    // JobTitles times Locations
    "{location: \"London\", title: \"Engineer\"}",
    "{location: \"London\", title: \"Manager\"}",
    "{location: \"Paris\", title: \"Engineer\"}",
    "{location: \"Paris\", title: \"Manager\"}",
    // Employees compose Departments (dept_id dropped)
    "{dept_name: \"Engineering\", emp_id: 1, emp_name: \"Ada\"}",
    "{dept_name: \"Engineering\", emp_id: 2, emp_name: \"Grace\"}",
    "{dept_name: \"Sales\", emp_id: 3, emp_name: \"Alan\"}",
    "{dept_name: \"Marketing\", emp_id: 4, emp_name: \"Edsger\"}",
    // (Employees join Departments) where dept_name = "Engineering" project {emp_name, dept_name}
    "{dept_name: \"Engineering\", emp_name: \"Ada\"}",
    "{dept_name: \"Engineering\", emp_name: \"Grace\"}",
];

#[test]
fn join_times_compose_inprocess_llvm_dumps_join_and_times() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("join-times-compose"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "join-times-compose LLVM failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(JOIN_TIMES_COMPOSE_TUPLES)
    );
}

#[test]
fn join_times_compose_inprocess_cranelift_dumps_join_and_times() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("join-times-compose"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "join-times-compose Cranelift failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(JOIN_TIMES_COMPOSE_TUPLES)
    );
}

#[test]
fn join_times_compose_inprocess_relations_equal_across_backends() {
    // Both backends compute the same relations, so the same tuple sets — the
    // printed order may differ (RM Pro 1; the all-Text product sorts by pointer).
    ensure_runtime_built();
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("join-times-compose"))
        .output()
        .expect("spawn LLVM");
    assert!(llvm.status.success());
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("join-times-compose"))
        .output()
        .expect("spawn Cranelift");
    assert!(cranelift.status.success());
    assert_eq!(
        tuple_lines(&llvm.stdout),
        tuple_lines(&cranelift.stdout),
        "backends disagree on the join-times-compose tuple set"
    );
}

/// The in-process twin populates two identical-heading `private` relvars
/// (`Morning`, `Evening`, heading { id, name }) that overlap in two tuples, dumps
/// each raw, then dumps `Morning intersect Evening` (the overlap), `Morning union
/// Evening` (everyone), and `Morning minus Evening` (morning-only). Tuple order is
/// unspecified (RM Pro 1), so the tests compare this set, not bytes; the shared
/// tuples recur across the queries.
const UNION_INTERSECT_MINUS_TUPLES: &[&str] = &[
    // Morning
    "{id: 1, name: \"Ada\"}",
    "{id: 2, name: \"Grace\"}",
    "{id: 3, name: \"Alan\"}",
    // Evening
    "{id: 2, name: \"Grace\"}",
    "{id: 3, name: \"Alan\"}",
    "{id: 4, name: \"Edsger\"}",
    // Morning intersect Evening
    "{id: 2, name: \"Grace\"}",
    "{id: 3, name: \"Alan\"}",
    // Morning union Evening
    "{id: 1, name: \"Ada\"}",
    "{id: 2, name: \"Grace\"}",
    "{id: 3, name: \"Alan\"}",
    "{id: 4, name: \"Edsger\"}",
    // Morning minus Evening
    "{id: 1, name: \"Ada\"}",
];

#[test]
fn union_intersect_minus_inprocess_llvm_dumps_relvars() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("union-intersect-minus"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "union-intersect-minus LLVM failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(UNION_INTERSECT_MINUS_TUPLES)
    );
}

#[test]
fn union_intersect_minus_inprocess_cranelift_dumps_relvars() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("union-intersect-minus"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "union-intersect-minus Cranelift failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(UNION_INTERSECT_MINUS_TUPLES)
    );
}

#[test]
fn union_intersect_minus_inprocess_relations_equal_across_backends() {
    ensure_runtime_built();
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("union-intersect-minus"))
        .output()
        .expect("spawn LLVM");
    assert!(llvm.status.success());
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("union-intersect-minus"))
        .output()
        .expect("spawn Cranelift");
    assert!(cranelift.status.success());
    assert_eq!(
        tuple_lines(&llvm.stdout),
        tuple_lines(&cranelift.stdout),
        "backends disagree on the union-intersect-minus tuple set"
    );
}

/// The in-process twin populates a binary edge relvar (`Edges`, heading { from,
/// to }) and a wider bill-of-materials (`Contains`, heading { major, minor, qty
/// }), dumps the raw edges, then dumps `Edges tclose` (the reachability closure,
/// bare form) and `Contains tclose { major, minor }` (the brace form — pick two
/// columns, then close). Tuple order is unspecified (RM Pro 1), so the tests
/// compare this set, not bytes; the direct edges recur in the closure.
const TCLOSE_TUPLES: &[&str] = &[
    // Edges (raw direct edges)
    "{from: 1, to: 2}",
    "{from: 2, to: 3}",
    "{from: 3, to: 4}",
    // Edges tclose — direct edges plus the transitively reachable pairs
    "{from: 1, to: 2}",
    "{from: 2, to: 3}",
    "{from: 3, to: 4}",
    "{from: 1, to: 3}",
    "{from: 2, to: 4}",
    "{from: 1, to: 4}",
    // Contains tclose { major, minor } — project to {major, minor}, then close:
    // direct (1,2),(1,3),(2,4),(3,5) plus transitive (1,4),(1,5)
    "{major: 1, minor: 2}",
    "{major: 1, minor: 3}",
    "{major: 2, minor: 4}",
    "{major: 3, minor: 5}",
    "{major: 1, minor: 4}",
    "{major: 1, minor: 5}",
];

#[test]
fn tclose_inprocess_llvm_dumps_closures() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("transitive-closure"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "transitive-closure LLVM failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(tuple_lines(&out.stdout), sorted_tuples(TCLOSE_TUPLES));
}

#[test]
fn tclose_inprocess_cranelift_dumps_closures() {
    ensure_runtime_built();
    let out = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("transitive-closure"))
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "transitive-closure Cranelift failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(tuple_lines(&out.stdout), sorted_tuples(TCLOSE_TUPLES));
}

#[test]
fn tclose_inprocess_relations_equal_across_backends() {
    ensure_runtime_built();
    let llvm = coddl()
        .args(["run", "--backend=llvm"])
        .arg(fixture_path("transitive-closure"))
        .output()
        .expect("spawn LLVM");
    assert!(llvm.status.success());
    let cranelift = coddl()
        .args(["run", "--backend=cranelift"])
        .arg(fixture_path("transitive-closure"))
        .output()
        .expect("spawn Cranelift");
    assert!(cranelift.status.success());
    assert_eq!(
        tuple_lines(&llvm.stdout),
        tuple_lines(&cranelift.stdout),
        "backends disagree on the transitive-closure tuple set"
    );
}

// ── tclose SQL pushdown (WITH RECURSIVE) ─────────────────────────────────────

/// Write the `tclose` database companions (`.cddb` / `.cdstore`) into `dir` and
/// seed `<dir>/tclose.sqlite` with a binary edge graph (`edges`, columns
/// `"from"`/`"to"` — reserved SQL keywords, quoted) and a bill-of-materials
/// (`contains`). Returns the db path. The caller writes its own `.cd` (with
/// `database tclose;`) alongside. Tests own their source — never the example.
fn seed_tclose_fixtures(dir: &Path) -> PathBuf {
    std::fs::write(
        dir.join("tclose.cddb"),
        "database tclose;\n\
         base relvar Edges { from: Integer, to: Integer } key { from, to };\n\
         base relvar Contains { major: Integer, minor: Integer, qty: Integer } key { major, minor };\n",
    )
    .expect("write tclose.cddb");
    std::fs::write(
        dir.join("tclose.cdstore"),
        "store for tclose;\n\
         backend sqlite { file: \"tclose.sqlite\" };\n\
         relvar Edges: table \"edges\" { columns: { from: \"from\", to: \"to\" } };\n\
         relvar Contains: table \"contains\" { columns: { major: \"major\", minor: \"minor\", qty: \"qty\" } };\n",
    )
    .expect("write tclose.cdstore");

    let db = dir.join("tclose.sqlite");
    // Pass the SQL as a single sqlite3 argument (no shell), so the quoted
    // `"from"`/`"to"` identifiers need no extra escaping.
    let sql = "CREATE TABLE edges (\"from\" INTEGER NOT NULL, \"to\" INTEGER NOT NULL, PRIMARY KEY (\"from\", \"to\")); \
               CREATE TABLE contains (major INTEGER NOT NULL, minor INTEGER NOT NULL, qty INTEGER NOT NULL, PRIMARY KEY (major, minor)); \
               INSERT INTO edges (\"from\", \"to\") VALUES (1,2),(2,3),(3,4); \
               INSERT INTO contains (major, minor, qty) VALUES (1,2,2),(1,3,1),(2,4,32),(3,5,1);";
    let status = Command::new("sqlite3")
        .arg(&db)
        .arg(sql)
        .status()
        .expect("invoke sqlite3");
    assert!(status.success(), "tclose fixture seed failed");
    db
}

/// The two pushed closures, dumped: `Edges tclose` (reachability over the 1→2→3→4
/// chain, adding 1→3, 2→4, 1→4) and `Contains tclose { major, minor }` (transitive
/// containment, adding 1→4 and 1→5). Tuple order is unspecified (RM Pro 1), so the
/// test compares this set.
const TCLOSE_DB_TUPLES: &[&str] = &[
    // Edges tclose
    "{from: 1, to: 2}",
    "{from: 2, to: 3}",
    "{from: 3, to: 4}",
    "{from: 1, to: 3}",
    "{from: 2, to: 4}",
    "{from: 1, to: 4}",
    // Contains tclose { major, minor }
    "{major: 1, minor: 2}",
    "{major: 1, minor: 3}",
    "{major: 2, minor: 4}",
    "{major: 3, minor: 5}",
    "{major: 1, minor: 4}",
    "{major: 1, minor: 5}",
];

/// The exact `WITH RECURSIVE` query each closure pushes — the golden text the
/// audit log must contain. Pins the recursive-CTE emission end-to-end.
const TCLOSE_EDGES_SQL: &str = r#"WITH RECURSIVE coddl_tc_op("from", "to") AS (SELECT "from", "to" FROM "edges"), coddl_tc("from", "to") AS (SELECT "from", "to" FROM coddl_tc_op UNION SELECT coddl_tc."from", coddl_tc_op."to" FROM coddl_tc JOIN coddl_tc_op ON coddl_tc."to" = coddl_tc_op."from") SELECT DISTINCT "from", "to" FROM coddl_tc"#;
const TCLOSE_CONTAINS_SQL: &str = r#"WITH RECURSIVE coddl_tc_op("major", "minor") AS (SELECT "major", "minor" FROM "contains"), coddl_tc("major", "minor") AS (SELECT "major", "minor" FROM coddl_tc_op UNION SELECT coddl_tc."major", coddl_tc_op."minor" FROM coddl_tc JOIN coddl_tc_op ON coddl_tc."minor" = coddl_tc_op."major") SELECT DISTINCT "major", "minor" FROM coddl_tc"#;

/// Compile + run a self-owned relvar-rooted `tclose` program on `backend`: each
/// closure must push to SQL as a `WITH RECURSIVE` query (asserted via the audit
/// log) and return the correct closure tuple set.
fn assert_tclose_pushdown_audit(backend: &str) {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = seed_tclose_fixtures(tmp.path());
    let cd = tmp.path().join("tclose-db.cd");
    std::fs::write(
        &cd,
        "program transitive_closure_db;\n\
         database tclose;\n\
         public relvar Edges { from: Integer, to: Integer } key { from, to };\n\
         public relvar Contains { major: Integer, minor: Integer, qty: Integer } key { major, minor };\n\
         oper main {} [\n\
             let reachable = transaction [ Edges tclose ];\n\
             write_relation { rel: reachable };\n\
             let all_parts = transaction [ Contains tclose { major, minor } ];\n\
             write_relation { rel: all_parts };\n\
         ];\n",
    )
    .expect("write tclose-db.cd");
    let log = tmp.path().join("audit.log");

    let out = coddl()
        .env("CODDL_AUDIT_LOG", &log)
        .env("CODDL_TCLOSE_FILE", &db)
        .args(["run", &format!("--backend={backend}")])
        .arg(&cd)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "tclose pushdown on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(TCLOSE_DB_TUPLES),
        "wrong closure tuple set on {backend}"
    );

    // The audit log must show each closure pushed as its `WITH RECURSIVE` query.
    let contents = std::fs::read_to_string(&log)
        .unwrap_or_else(|e| panic!("read audit log {}: {e}", log.display()));
    let sqls: Vec<&str> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| audit_sql(l).unwrap_or_else(|| panic!("malformed audit line ({backend}): {l:?}")))
        .collect();
    for needle in [TCLOSE_EDGES_SQL, TCLOSE_CONTAINS_SQL] {
        assert!(
            sqls.iter().any(|s| *s == needle),
            "audit log on {backend} missing pushed query:\n{needle}\ngot:\n{sqls:#?}"
        );
    }
}

#[test]
fn tclose_pushdown_audit_llvm() {
    assert_tclose_pushdown_audit("llvm");
}

#[test]
fn tclose_pushdown_audit_cranelift() {
    assert_tclose_pushdown_audit("cranelift");
}

// ── inline nested-tuple cells (relation literal with a tuple-valued attr) ──

/// Print a relation literal whose tuples carry tuple-valued attributes — an
/// integer pair `pt: {x, y}` and a Text-bearing `who: {name, age}` — including a
/// duplicate record. Exercises the inline nested-tuple cell layout, the nested
/// descriptor, recursive store, recursive print, and content-aware dedup that
/// recurses into the Text-in-tuple cell.
const NESTED_TUPLE_SRC: &str = "\
program nested_tuple;
oper main {} [
    let r = Relation {
        { id: 1, pt: { x: 10, y: 20 }, who: { name: \"ada\", age: 30 } },
        { id: 2, pt: { x: 30, y: 40 }, who: { name: \"bo\", age: 25 } },
        { id: 2, pt: { x: 30, y: 40 }, who: { name: \"bo\", age: 25 } },
    };
    write_relation { rel: r };
];
";

fn run_nested_tuple_lit(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("nested-tuple.cd");
    std::fs::write(&src, NESTED_TUPLE_SRC).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "nested-tuple literal on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn nested_tuple_literal_prints_and_dedups() {
    // Nested attrs render name-sorted (`age` before `name`); the duplicate id=2
    // record collapses (dedup recurses into the Text-in-tuple cell, content-aware).
    let expected = sorted_tuples(&[
        r#"{id: 1, pt: {x: 10, y: 20}, who: {age: 30, name: "ada"}}"#,
        r#"{id: 2, pt: {x: 30, y: 40}, who: {age: 25, name: "bo"}}"#,
    ]);
    for backend in ["llvm", "cranelift"] {
        assert_eq!(
            tuple_lines(&run_nested_tuple_lit(backend)),
            expected,
            "nested-tuple output on {backend}"
        );
    }
}

#[test]
fn nested_tuple_literal_byte_identical_across_backends() {
    assert_eq!(
        run_nested_tuple_lit("llvm"),
        run_nested_tuple_lit("cranelift")
    );
}

// ── wrap / unwrap (in-process restructure) ────────────────────────────

/// `wrap` groups attributes into a tuple-valued attribute; `unwrap` expands it.
/// Exercises: wrap (prints nested), the wrap∘unwrap round-trip (= the original),
/// wrap-then-`project { t }` (the tuple cell copies whole, not truncated — the
/// `cell_width_desc` fix), and a `join` on a Text-bearing tuple key (content-
/// aware equality — the unified `cmp_cell` fix).
const WRAP_UNWRAP_SRC: &str = "\
program wrap_unwrap;
oper main {} [
    let r = Relation { {a: 1, n: \"x\", c: 7}, {a: 2, n: \"y\", c: 8} };
    write_relation { rel: r wrap { t: {a, n} } };
    write_relation { rel: r wrap { t: {a, n} } unwrap { t } };
    write_relation { rel: r wrap { t: {a, n} } project { t } };
    let s = Relation { {a: 1, n: \"x\", w: 100}, {a: 2, n: \"y\", w: 200} };
    write_relation { rel: (r wrap { k: {a, n} }) join (s wrap { k: {a, n} }) };
];
";

fn run_wrap_unwrap(backend: &str) -> Vec<u8> {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("wrap-unwrap.cd");
    std::fs::write(&src, WRAP_UNWRAP_SRC).expect("write src");
    let out = coddl()
        .args(["run", &format!("--backend={backend}")])
        .arg(&src)
        .output()
        .expect("spawn coddl");
    assert!(
        out.status.success(),
        "wrap/unwrap on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn wrap_unwrap_restructures_and_composes() {
    let expected = sorted_tuples(&[
        // wrap: `a`/`n` grouped into `t`, `c` survives (nested attrs name-sorted).
        r#"{c: 7, t: {a: 1, n: "x"}}"#,
        r#"{c: 8, t: {a: 2, n: "y"}}"#,
        // wrap ∘ unwrap = the original.
        r#"{a: 1, c: 7, n: "x"}"#,
        r#"{a: 2, c: 8, n: "y"}"#,
        // wrap then project { t }: the whole tuple cell survives.
        r#"{t: {a: 1, n: "x"}}"#,
        r#"{t: {a: 2, n: "y"}}"#,
        // join on the Text-bearing tuple key `k` (content-aware match).
        r#"{c: 7, k: {a: 1, n: "x"}, w: 100}"#,
        r#"{c: 8, k: {a: 2, n: "y"}, w: 200}"#,
    ]);
    for backend in ["llvm", "cranelift"] {
        assert_eq!(
            tuple_lines(&run_wrap_unwrap(backend)),
            expected,
            "wrap/unwrap output on {backend}"
        );
    }
}

#[test]
fn wrap_unwrap_byte_identical_across_backends() {
    assert_eq!(run_wrap_unwrap("llvm"), run_wrap_unwrap("cranelift"));
}

// ── wrap/unwrap SQL pushdown (relvar-rooted → flat leaf-column SELECT) ──

/// A relvar-rooted `wrap` pushes to SQL: the heading restructure is free, the
/// SQL selects the flat leaf columns (depth-first order of the wrapped heading),
/// and the runtime materializes them into the nested record. The audit log shows
/// the pushed leaf-column SELECT — no in-process restructure query.
fn assert_wrap_pushdown(backend: &str) {
    ensure_runtime_built();
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = seed_greetings_fixtures(tmp.path());
    let cd = tmp.path().join("wp.cd");
    std::fs::write(
        &cd,
        "program wp;\n\
         database greetings;\n\
         public relvar Greetings { id: Integer, message: Text } key { id };\n\
         oper main {} [ let g = transaction [ Greetings wrap { t: {id, message} } ]; write_relation { rel: g }; ];\n",
    )
    .expect("write wp.cd");
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
        "pushed wrap on {backend} failed: stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `id`/`message` grouped into the tuple-valued attribute `t`; nested print.
    assert_eq!(
        tuple_lines(&out.stdout),
        sorted_tuples(&[r#"{t: {id: 1, message: "hello world"}}"#]),
        "wrap output on {backend}",
    );
    // Pushed as flat leaf columns (the tuple has no SQL column); the nesting is
    // reconstructed at materialization. No separate restructure query.
    let contents = std::fs::read_to_string(&log).expect("read audit log");
    let sqls: Vec<&str> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| audit_sql(l).unwrap_or_else(|| panic!("malformed audit line ({backend}): {l:?}")))
        .collect();
    assert!(
        sqls.iter().any(|s| *s == EXPECTED_PUSHED_WRAP_SQL),
        "audit log on {backend} missing the pushed wrap query:\n{EXPECTED_PUSHED_WRAP_SQL}\ngot:\n{sqls:#?}",
    );
}

const EXPECTED_PUSHED_WRAP_SQL: &str = r#"SELECT DISTINCT "id", "message" FROM "greetings""#;

#[test]
fn wrap_pushdown_llvm() {
    assert_wrap_pushdown("llvm");
}

#[test]
fn wrap_pushdown_cranelift() {
    assert_wrap_pushdown("cranelift");
}

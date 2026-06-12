//! Runtime-staticlib discovery.
//!
//! Locates `libcoddl_runtime.a` for the linker. Lookup order:
//!
//! 1. The `CODDL_RUNTIME` environment variable, interpreted as an
//!    absolute path to the staticlib.
//! 2. `<exe-dir>/libcoddl_runtime.a` — the directory containing the
//!    `coddl` binary. In dev (`cargo build`), `target/debug/coddl`
//!    and `target/debug/libcoddl_runtime.a` are siblings; this path
//!    resolves automatically after `cargo build -p coddl-runtime`.
//!
//! Installed binaries either place the staticlib next to the
//! executable or set `CODDL_RUNTIME=...` explicitly. The error
//! message points at both options so the user knows their choices.

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum RuntimeError {
    /// `current_exe()` failed — extremely rare; usually a symlink-
    /// resolution edge case.
    ExePath(String),
    /// Neither the env var nor the side-by-side lookup found a file.
    NotFound { searched: Vec<PathBuf> },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::ExePath(msg) => write!(f, "resolving coddl executable path: {msg}"),
            RuntimeError::NotFound { searched } => {
                writeln!(f, "could not find libcoddl_runtime.a; tried:")?;
                for p in searched {
                    writeln!(f, "  - {}", p.display())?;
                }
                writeln!(f, "Build it with `cargo build -p coddl-runtime`, or set")?;
                write!(f, "CODDL_RUNTIME=/path/to/libcoddl_runtime.a")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Resolve the runtime staticlib path.
pub fn discover() -> Result<PathBuf, RuntimeError> {
    let mut searched: Vec<PathBuf> = Vec::new();

    if let Ok(env) = std::env::var("CODDL_RUNTIME") {
        let path = PathBuf::from(env);
        if path.exists() {
            return Ok(path);
        }
        searched.push(path);
    }

    let exe = std::env::current_exe().map_err(|e| RuntimeError::ExePath(e.to_string()))?;
    if let Some(dir) = exe.parent() {
        let side = dir.join("libcoddl_runtime.a");
        if side.exists() {
            return Ok(side);
        }
        searched.push(side);
    }

    Err(RuntimeError::NotFound { searched })
}

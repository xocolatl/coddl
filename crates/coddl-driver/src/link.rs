//! Linker invocation shared by `compile` and `run`.
//!
//! Two entry points: `link_llvm_ir` for the LLVM backend (shells out
//! to `clang`) and `link_cranelift_object` for the Cranelift backend
//! (shells out to `cc`). Each writes the artifact to a scratch file
//! inside a caller-supplied `TempDir` and invokes the toolchain to
//! produce a binary at `output`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

#[derive(Debug)]
pub enum LinkError {
    /// Writing the intermediate artifact to disk failed.
    WriteScratch { path: PathBuf, err: String },
    /// `clang` / `cc` could not be spawned (missing from PATH, etc.).
    Spawn { tool: String, err: String },
    /// The toolchain returned a non-zero exit status. `stderr` carries
    /// whatever it printed.
    Toolchain {
        tool: String,
        status: i32,
        stderr: String,
    },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::WriteScratch { path, err } => {
                write!(f, "writing scratch file {}: {err}", path.display())
            }
            LinkError::Spawn { tool, err } => {
                write!(f, "invoking {tool}: {err} (is it on PATH?)")
            }
            LinkError::Toolchain {
                tool,
                status,
                stderr,
            } => {
                write!(f, "{tool} exited with {status}:\n{stderr}")
            }
        }
    }
}

impl std::error::Error for LinkError {}

/// Link a string of LLVM IR text with the runtime staticlib into a
/// native executable at `output`.
pub fn link_llvm_ir(
    ir: &str,
    output: &Path,
    runtime: &Path,
    scratch: &TempDir,
) -> Result<(), LinkError> {
    let ir_path = scratch.path().join("module.ll");
    std::fs::write(&ir_path, ir).map_err(|e| LinkError::WriteScratch {
        path: ir_path.clone(),
        err: e.to_string(),
    })?;
    run_tool("clang", &[ir_path.as_os_str(), runtime.as_os_str()], output)
}

/// Link a Cranelift-emitted object byte buffer with the runtime
/// staticlib into a native executable at `output`.
pub fn link_cranelift_object(
    obj: &[u8],
    output: &Path,
    runtime: &Path,
    scratch: &TempDir,
) -> Result<(), LinkError> {
    let obj_path = scratch.path().join("module.o");
    std::fs::write(&obj_path, obj).map_err(|e| LinkError::WriteScratch {
        path: obj_path.clone(),
        err: e.to_string(),
    })?;
    run_tool("cc", &[obj_path.as_os_str(), runtime.as_os_str()], output)
}

fn run_tool(tool: &str, inputs: &[&std::ffi::OsStr], output: &Path) -> Result<(), LinkError> {
    let cmd_output = Command::new(tool)
        .args(inputs)
        .arg("-o")
        .arg(output)
        .output()
        .map_err(|e| LinkError::Spawn {
            tool: tool.to_string(),
            err: e.to_string(),
        })?;
    if !cmd_output.status.success() {
        return Err(LinkError::Toolchain {
            tool: tool.to_string(),
            status: cmd_output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&cmd_output.stderr).into_owned(),
        });
    }
    Ok(())
}

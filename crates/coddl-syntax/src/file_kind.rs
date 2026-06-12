//! File-kind dispatch for the Coddl source family.
//!
//! Coddl programs are spread across four file kinds with distinct
//! grammars and roles:
//!
//! - `.cd`      — application source
//! - `.cddb`    — database catalog
//! - `.cdmap`   — external → conceptual adapter
//! - `.cdstore` — conceptual → physical binding
//!
//! All four share the same lexer (no reserved words) and CST
//! infrastructure; only the parser dispatch differs. The driver and the
//! LSP analyzer resolve a path or URI to a [`FileKind`] once and pass it
//! through every analysis call.

use std::path::Path;

/// Which dialect of the Coddl source family a file belongs to. The
/// shape of every analysis pipeline call depends on this.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Hash)]
pub enum FileKind {
    /// `.cd` — application source (`program`, `oper`, `let`, …).
    Cd,
    /// `.cddb` — database catalog (`database`, `base relvar`,
    /// `virtual relvar`, …).
    Cddb,
    /// `.cdmap` — external → conceptual adapter (`map <prog> to <db>;`,
    /// identity / project / rename entries).
    Cdmap,
    /// `.cdstore` — conceptual → physical binding (`store for`,
    /// `backend`, `relvar X: table "…" { columns: … }`).
    Cdstore,
}

impl FileKind {
    /// Resolve a bare file extension (no leading dot, case-sensitive)
    /// to a [`FileKind`]. Returns `None` for unrecognized extensions.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "cd" => Some(FileKind::Cd),
            "cddb" => Some(FileKind::Cddb),
            "cdmap" => Some(FileKind::Cdmap),
            "cdstore" => Some(FileKind::Cdstore),
            _ => None,
        }
    }

    /// Resolve a file path's extension to a [`FileKind`]. Returns
    /// `None` if the path has no extension, the extension isn't valid
    /// UTF-8, or the extension isn't a recognized dialect.
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|s| s.to_str())
            .and_then(Self::from_extension)
    }

    /// The canonical extension string (no leading dot) for this kind.
    /// Inverse of [`from_extension`](Self::from_extension).
    pub fn extension(self) -> &'static str {
        match self {
            FileKind::Cd => "cd",
            FileKind::Cddb => "cddb",
            FileKind::Cdmap => "cdmap",
            FileKind::Cdstore => "cdstore",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn from_extension_resolves_every_dialect() {
        assert_eq!(FileKind::from_extension("cd"), Some(FileKind::Cd));
        assert_eq!(FileKind::from_extension("cddb"), Some(FileKind::Cddb));
        assert_eq!(FileKind::from_extension("cdmap"), Some(FileKind::Cdmap));
        assert_eq!(FileKind::from_extension("cdstore"), Some(FileKind::Cdstore));
    }

    #[test]
    fn from_extension_rejects_unknown() {
        assert_eq!(FileKind::from_extension("rs"), None);
        assert_eq!(FileKind::from_extension(""), None);
        assert_eq!(FileKind::from_extension("CD"), None); // case-sensitive
    }

    #[test]
    fn from_path_pulls_extension() {
        let path = PathBuf::from("examples/hello-world-db/greetings.cddb");
        assert_eq!(FileKind::from_path(&path), Some(FileKind::Cddb));
    }

    #[test]
    fn from_path_handles_no_extension() {
        let path = PathBuf::from("Makefile");
        assert_eq!(FileKind::from_path(&path), None);
    }

    #[test]
    fn extension_round_trips() {
        for kind in [
            FileKind::Cd,
            FileKind::Cddb,
            FileKind::Cdmap,
            FileKind::Cdstore,
        ] {
            assert_eq!(FileKind::from_extension(kind.extension()), Some(kind));
        }
    }
}

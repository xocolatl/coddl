//! Shared diagnostic data type for every Coddl frontend crate.
//!
//! Every analysis pass is `fn(Input) -> (Output, Vec<Diagnostic>)`. The
//! CLI driver renders these to the terminal; `coddl-lsp` serializes
//! them to LSP `PublishDiagnostics`.

use std::fmt;

/// Stable identifier for a source file within a compilation session.
///
/// The file table itself lives in the driver / LSP; library crates
/// never resolve `FileId` to a path on their own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct FileId(pub u32);

/// Byte-range source span. `start` and `end` are byte offsets into the
/// file's UTF-8 source; `end` is exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(file: FileId, start: u32, end: u32) -> Self {
        Self { file, start, end }
    }

    /// A zero-length span at the start of `file` — useful for diagnostics
    /// that can't attribute themselves to a specific byte range.
    pub const fn synthetic(file: FileId) -> Self {
        Self::new(file, 0, 0)
    }

    pub const fn len(&self) -> u32 {
        self.end - self.start
    }

    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Smallest span covering both `self` and `other`. Both must share a file.
    pub fn merge(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span::new(
            self.file,
            self.start.min(other.start),
            self.end.max(other.end),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
    Note,
    Help,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => f.write_str("error"),
            Severity::Warning => f.write_str("warning"),
            Severity::Note => f.write_str("note"),
            Severity::Help => f.write_str("help"),
        }
    }
}

/// A diagnostic emitted by any frontend pass.
///
/// `code` is a stable identifier like `"E0001"` so users can search,
/// filter, and disable individual classes. `related` carries supporting
/// spans (e.g. "previously declared here") that the LSP renders as
/// related-information entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub related: Vec<(Span, String)>,
}

impl Diagnostic {
    pub fn error(span: Span, code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, span, code, message)
    }

    pub fn warning(span: Span, code: &'static str, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, span, code, message)
    }

    pub fn new(
        severity: Severity,
        span: Span,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            span,
            severity,
            code,
            message: message.into(),
            related: Vec::new(),
        }
    }

    pub fn with_related(mut self, span: Span, note: impl Into<String>) -> Self {
        self.related.push((span, note.into()));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_merge_grows_outward() {
        let a = Span::new(FileId(0), 4, 8);
        let b = Span::new(FileId(0), 2, 6);
        let m = a.merge(b);
        assert_eq!(m.start, 2);
        assert_eq!(m.end, 8);
    }

    #[test]
    fn diagnostic_builder_attaches_related() {
        let d = Diagnostic::error(Span::synthetic(FileId(0)), "E0001", "boom")
            .with_related(Span::synthetic(FileId(0)), "see here");
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.related.len(), 1);
    }
}

//! Convert frontend diagnostic types into LSP wire types.
//!
//! Spans become `Range`s via the document's `LineIndex`. Severities
//! map onto `DiagnosticSeverity`. Diagnostic codes are surfaced as
//! `NumberOrString::String` so editors can filter / link them.

use coddl_diagnostics::{Diagnostic as CoddlDiagnostic, Severity};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range};

use crate::line_index::LineIndex;

/// Build an LSP `Diagnostic` from a Coddl one, using `line_index`
/// to map the span's byte offsets to LSP positions.
pub fn diagnostic(d: &CoddlDiagnostic, line_index: &LineIndex) -> Diagnostic {
    let start = line_index.position(d.span.start);
    let end = line_index.position(d.span.end);
    Diagnostic {
        range: Range::new(start, end),
        severity: Some(severity(d.severity)),
        code: Some(NumberOrString::String(d.code.to_string())),
        source: Some("coddl".into()),
        message: d.message.clone(),
        ..Default::default()
    }
}

fn severity(s: Severity) -> DiagnosticSeverity {
    match s {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Note => DiagnosticSeverity::INFORMATION,
        Severity::Help => DiagnosticSeverity::HINT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coddl_diagnostics::{FileId, Span};
    use std::sync::Arc;

    fn idx(s: &str) -> LineIndex {
        LineIndex::new(Arc::from(s))
    }

    #[test]
    fn diagnostic_span_converts_to_range() {
        // Source has `world` starting at byte 6 on line 1.
        let src = "hello\nworld\n";
        let span = Span::new(FileId(0), 6, 11); // covers "world"
        let d = CoddlDiagnostic::error(span, "T9999", "boom");
        let lsp = diagnostic(&d, &idx(src));
        assert_eq!(lsp.range.start, tower_lsp::lsp_types::Position::new(1, 0));
        assert_eq!(lsp.range.end, tower_lsp::lsp_types::Position::new(1, 5));
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.code, Some(NumberOrString::String("T9999".into())));
        assert_eq!(lsp.source.as_deref(), Some("coddl"));
        assert_eq!(lsp.message, "boom");
    }

    #[test]
    fn severity_mapping_exhaustive() {
        assert_eq!(severity(Severity::Error), DiagnosticSeverity::ERROR);
        assert_eq!(severity(Severity::Warning), DiagnosticSeverity::WARNING);
        assert_eq!(severity(Severity::Note), DiagnosticSeverity::INFORMATION);
        assert_eq!(severity(Severity::Help), DiagnosticSeverity::HINT);
    }
}

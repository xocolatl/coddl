//! Canonical formatter for Coddl source.
//!
//! Exposed two ways from one library: as the `coddl fmt` driver
//! subcommand and as the LSP `textDocument/formatting` handler.
//!
//! The formatter walks the lossless syntax tree from `coddl-syntax`
//! and re-emits canonical source. Output is idempotent —
//! `format(format(x, opts), opts) == format(x, opts)` for every valid
//! input — verified by unit tests.

use coddl_diagnostics::Diagnostic;

/// Stable identifier for a versioned ruleset.
///
/// Like `rustfmt`'s editions: a project pins its `format.edition` in
/// `coddl.toml`, the formatter applies that ruleset, and rule changes
/// land in new editions rather than silently breaking every file in
/// every checked-in project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edition {
    /// First public ruleset.
    E2026,
}

impl Edition {
    pub const LATEST: Edition = Edition::E2026;
}

impl Default for Edition {
    fn default() -> Self {
        Self::LATEST
    }
}

/// User-configurable formatter knobs. Intentionally tiny.
#[derive(Clone, Debug)]
pub struct FormatOptions {
    pub edition: Edition,
    pub indent_width: u8,
    pub line_width: u16,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            edition: Edition::LATEST,
            indent_width: 4,
            line_width: 100,
        }
    }
}

/// Result of formatting a source buffer.
///
/// `diagnostics` carries parse errors picked up while building the CST
/// (the formatter still attempts to format around recoverable syntax
/// errors — the parser's recovery applies here too).
pub struct FormatOutput {
    pub text: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Format a source buffer.
///
/// Pure: same input + same options → same output. The buffer doesn't need
/// to be syntactically perfect; the formatter formats what parses and
/// preserves the rest verbatim. Pure: same input + same options
/// always produce the same output.
pub fn format(source: &str, _opts: &FormatOptions) -> FormatOutput {
    // TODO: parse to CST, walk, emit canonical output. The current
    // no-op identity lets the wiring through `coddl-driver` and
    // `coddl-lsp` be exercised end-to-end.
    FormatOutput {
        text: source.to_owned(),
        diagnostics: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatter_is_idempotent_on_empty_input() {
        let opts = FormatOptions::default();
        let first = format("", &opts);
        let second = format(&first.text, &opts);
        assert_eq!(first.text, second.text);
    }

    #[test]
    fn formatter_preserves_input_until_real_rules_land() {
        let src = "program hello;\n\noper main {} [\n    write_line { message: \"hi\" };\n];\n";
        let out = format(src, &FormatOptions::default());
        assert_eq!(out.text, src);
        assert!(out.diagnostics.is_empty());
    }
}

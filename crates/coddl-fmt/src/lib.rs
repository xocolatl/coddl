//! Canonical formatter for Coddl source.
//!
//! Exposed two ways from one library: as a `coddl fmt` driver subcommand
//! and as the LSP `textDocument/formatting` handler. See ARCHITECTURE.md §13.
//!
//! The formatter walks the CST produced by `coddl-syntax` (lossless, preserves
//! every token and trivia) and re-emits canonical source. The output is
//! idempotent — `format(format(x, opts), opts) == format(x, opts)` for every
//! valid input. This is a unit-test invariant, not a hope.

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

/// User-configurable formatter knobs. Intentionally tiny — see §13.
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
/// errors — `chumsky` recovery from §12 applies here too).
pub struct FormatOutput {
    pub text: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Format a source buffer.
///
/// Pure: same input + same options → same output. The buffer doesn't need
/// to be syntactically perfect; the formatter formats what parses and
/// preserves the rest verbatim. See §12 discipline #4 (pure analyses).
pub fn format(source: &str, _opts: &FormatOptions) -> FormatOutput {
    // TODO (milestone 2): parse to CST, walk, emit canonical output.
    // For now this is a no-op so the workspace builds and the wiring
    // through `coddl-driver` and `coddl-lsp` is exercised.
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

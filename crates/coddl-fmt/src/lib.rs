//! Canonical formatter for Coddl source.
//!
//! Exposed two ways from one library: as the `coddl fmt` driver
//! subcommand and as the LSP `textDocument/formatting` handler.
//!
//! The formatter walks the lossless syntax tree from `coddl-syntax`
//! and re-emits canonical source. Output is idempotent —
//! `format(format(x, opts), opts) == format(x, opts)` for every valid
//! input — verified by unit tests.

mod printer;

use coddl_diagnostics::{Diagnostic, FileId};
use coddl_syntax::FileKind;

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

/// Format a source buffer of the given dialect.
///
/// Pure: same input + same options + same kind → same output, and idempotent
/// (`format(format(x))` == `format(x)`). The buffer doesn't need to parse
/// cleanly — the formatter formats the recovered CST and returns the parse
/// diagnostics alongside.
pub fn format(source: &str, opts: &FormatOptions, kind: FileKind) -> FormatOutput {
    let parsed = coddl_syntax::parse(source, FileId(0), kind);
    let text = printer::print(&parsed.tree, opts.indent_width as usize);
    FormatOutput {
        text,
        diagnostics: parsed.diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        format(src, &FormatOptions::default(), FileKind::Cd).text
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(fmt(""), "");
    }

    #[test]
    fn reformats_messy_into_canonical() {
        let src = "program p;\noper   main {}[ write_line{message:\"hi\"} ; ];\n";
        let want = "program p;\noper main {} [\n    write_line { message: \"hi\" };\n];\n";
        let got = fmt(src);
        assert_eq!(got, want, "\n=== got ===\n{got}=== want ===\n{want}");
    }

    #[test]
    fn already_canonical_is_a_fixpoint() {
        let src = "program p;\noper main {} [\n    write_line { message: \"hi\" };\n];\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn is_idempotent() {
        let src = "program p;\noper   main {}[ write_line{message:\"hi\"} ; ];\n";
        let once = fmt(src);
        assert_eq!(fmt(&once), once, "second pass changed the output");
    }

    #[test]
    fn spacing_around_operators_colon_and_dot() {
        let src = "program p;\noper main {} [ let x=g.message where id=1; ];\n";
        let got = fmt(src);
        assert!(got.contains("let x = g.message where id = 1;"), "got:\n{got}");
        assert_eq!(fmt(&got), got);
    }

    #[test]
    fn spacing_around_arithmetic_and_concat_operators() {
        let src = "program p;\noper main {} [ let x=1+2*3; let y=\"a\"||\"b\"; ];\n";
        let got = fmt(src);
        assert!(got.contains("let x = 1 + 2 * 3;"), "got:\n{got}");
        assert!(got.contains("let y = \"a\" || \"b\";"), "got:\n{got}");
        assert_eq!(fmt(&got), got);
    }

    #[test]
    fn preserves_leading_and_trailing_comments() {
        let src = "// header\nprogram p; // trailing\noper main {} [\n    write_line { message: \"hi\" }; // note\n];\n";
        let got = fmt(src);
        assert!(got.starts_with("// header\nprogram p; // trailing\n"), "got:\n{got}");
        assert!(got.contains("write_line { message: \"hi\" }; // note"), "got:\n{got}");
        assert_eq!(fmt(&got), got, "comments must survive idempotently");
    }

    #[test]
    fn multiline_heading_and_block_are_preserved_and_idempotent() {
        let src = "program p;\npublic relvar G {\n    id: Integer,\n    name: Text,\n} key { id };\noper main {} [\n    let t = transaction [\n        G where id = 1\n    ];\n];\n";
        let got = fmt(src);
        assert_eq!(got, src, "already-canonical multi-line input is a fixpoint:\n{got}");
    }

    #[test]
    fn collapses_blank_line_runs_to_one() {
        let src = "program p;\n\n\n\noper main {} [];\n";
        let got = fmt(src);
        assert_eq!(got, "program p;\n\noper main {} [];\n", "got:\n{got}");
    }

    #[test]
    fn formats_other_dialects_too() {
        // A `.cddb` catalog: spacing normalizes via the same printer.
        let src = "database greetings;\nbase relvar Greetings {id: Integer,message: Text} key {id};\n";
        let got = format(src, &FormatOptions::default(), FileKind::Cddb).text;
        assert!(got.contains("{ id: Integer, message: Text }"), "got:\n{got}");
        assert_eq!(
            format(&got, &FormatOptions::default(), FileKind::Cddb).text,
            got
        );
    }
}

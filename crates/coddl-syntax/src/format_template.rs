//! Scanner for the body of an `f"…"` format-string literal.
//!
//! A format string is a `Text` template: literal runs interleaved with
//! `{name}` placeholders that name attributes of the `args` tuple at the
//! `format` call site. This module turns the raw token text into an
//! alternating sequence of [`TemplateChunk`]s — escape-decoded literal
//! bytes and placeholder identifiers — so that both the typechecker
//! (`coddl-types`, which checks each placeholder against the args
//! heading) and the lowerer (`coddl-procir`, which emits the
//! `|| to_text { … } ||` chain) agree on the structure byte-for-byte.
//!
//! Escaping mirrors plain string literals (`\n \r \t \" \\ \u{…}`), plus
//! `{{` / `}}` for literal braces. The lexer does **not** validate
//! placeholders — all structural diagnostics originate here, with byte
//! ranges relative to the start of the token text so the caller can add
//! the token's span start to produce a source span.

use std::ops::Range;

use unicode_ident::{is_xid_continue, is_xid_start};

/// One piece of a parsed format template, in source order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateChunk {
    /// A run of literal text — already escape-decoded, with `{{`/`}}`
    /// collapsed to single braces. UTF-8 bytes ready to emit as a `Text`
    /// constant. Empty runs are never produced.
    Literal(Vec<u8>),
    /// A `{name}` placeholder. `name` is the referenced attribute; `range`
    /// is the byte range of the identifier within the token text (for
    /// diagnostics).
    Placeholder { name: String, range: Range<usize> },
}

/// A structural problem in a format template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateError {
    pub kind: TemplateErrorKind,
    /// Byte range within the token text covering the offending region.
    pub range: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateErrorKind {
    /// `{` with no matching `}` before the end of the template.
    UnmatchedOpenBrace,
    /// A `}` that is neither part of `}}` nor closing a placeholder.
    UnmatchedCloseBrace,
    /// `{}` — a placeholder with no name.
    EmptyPlaceholder,
    /// `{…}` whose body is not a bare identifier (spaces, punctuation, …).
    InvalidPlaceholderIdent,
}

impl TemplateErrorKind {
    /// A human-readable tail for the T0058 diagnostic message.
    pub fn message(self) -> &'static str {
        match self {
            TemplateErrorKind::UnmatchedOpenBrace => {
                "unmatched `{` in format template (use `{{` for a literal brace)"
            }
            TemplateErrorKind::UnmatchedCloseBrace => {
                "unmatched `}` in format template (use `}}` for a literal brace)"
            }
            TemplateErrorKind::EmptyPlaceholder => "empty `{}` placeholder in format template",
            TemplateErrorKind::InvalidPlaceholderIdent => {
                "format placeholder must be a bare attribute name"
            }
        }
    }
}

/// Number of bytes of the `f"` prefix the scanner skips before the body.
const PREFIX_LEN: usize = 2; // `f` + `"`

/// Parse the body of a `FORMAT_STRING_LIT` token (`token_text` includes the
/// `f"` prefix and trailing `"`). Returns the chunk sequence on success, or
/// every structural error found. All ranges are byte offsets within
/// `token_text`.
pub fn parse_format_template(token_text: &str) -> Result<Vec<TemplateChunk>, Vec<TemplateError>> {
    // Strip `f"` … `"`. Tolerate a missing close quote (unterminated
    // literals are already an E0003 from the lexer) by treating whatever
    // follows the prefix as the body.
    let body = token_text.strip_prefix("f\"").unwrap_or(token_text);
    let inner = body.strip_suffix('"').unwrap_or(body);

    let mut chunks = Vec::new();
    let mut errors = Vec::new();
    let mut lit: Vec<u8> = Vec::new();

    // Byte offset (within token_text) of the next char to be produced.
    let mut chars = inner.char_indices().peekable();

    // Push the accumulated literal run, if any, as a chunk.
    macro_rules! flush_lit {
        ($lit:expr, $chunks:expr) => {
            if !$lit.is_empty() {
                $chunks.push(TemplateChunk::Literal(std::mem::take(&mut $lit)));
            }
        };
    }

    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                // Escape — same set as a plain string literal.
                let Some((_, esc)) = chars.next() else { break };
                match esc {
                    'n' => lit.push(b'\n'),
                    'r' => lit.push(b'\r'),
                    't' => lit.push(b'\t'),
                    '"' => lit.push(b'"'),
                    '\\' => lit.push(b'\\'),
                    'u' => {
                        // `\u{XXXX}` — the lexer already validated the form.
                        if chars.peek().map(|&(_, c)| c) != Some('{') {
                            break;
                        }
                        chars.next(); // '{'
                        let mut hex = String::new();
                        for (_, h) in chars.by_ref() {
                            if h == '}' {
                                break;
                            }
                            hex.push(h);
                        }
                        if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                            if let Some(ch) = char::from_u32(cp) {
                                let mut buf = [0u8; 4];
                                lit.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                    }
                    _ => {
                        // Unknown escape survived lexing only in a format
                        // string (the plain-string path asserts this can't
                        // happen). Be lenient: keep the literal backslash.
                        lit.push(b'\\');
                        let mut buf = [0u8; 4];
                        lit.extend_from_slice(esc.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            '{' => {
                if chars.peek().map(|&(_, c)| c) == Some('{') {
                    chars.next(); // second '{'
                    lit.push(b'{');
                    continue;
                }
                // A placeholder. Everything from here to the next `}` is the
                // name; absolute offset of `{` is PREFIX_LEN + i.
                flush_lit!(lit, chunks);
                let open = PREFIX_LEN + i;
                let name_start = chars.peek().map(|&(j, _)| PREFIX_LEN + j);
                let mut name = String::new();
                let mut closed = false;
                let mut name_end = name_start.unwrap_or(open + 1);
                while let Some(&(j, c)) = chars.peek() {
                    if c == '}' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    name.push(c);
                    name_end = PREFIX_LEN + j + c.len_utf8();
                    chars.next();
                }
                if !closed {
                    errors.push(TemplateError {
                        kind: TemplateErrorKind::UnmatchedOpenBrace,
                        range: open..(PREFIX_LEN + inner.len()),
                    });
                    break;
                }
                let name_range = name_start.unwrap_or(open + 1)..name_end;
                if name.is_empty() {
                    errors.push(TemplateError {
                        kind: TemplateErrorKind::EmptyPlaceholder,
                        range: open..(name_end + 1), // include the `}`
                    });
                } else if !is_ident(&name) {
                    errors.push(TemplateError {
                        kind: TemplateErrorKind::InvalidPlaceholderIdent,
                        range: name_range,
                    });
                } else {
                    chunks.push(TemplateChunk::Placeholder {
                        name,
                        range: name_range,
                    });
                }
            }
            '}' => {
                if chars.peek().map(|&(_, c)| c) == Some('}') {
                    chars.next(); // second '}'
                    lit.push(b'}');
                    continue;
                }
                errors.push(TemplateError {
                    kind: TemplateErrorKind::UnmatchedCloseBrace,
                    range: (PREFIX_LEN + i)..(PREFIX_LEN + i + 1),
                });
            }
            _ => {
                let mut buf = [0u8; 4];
                lit.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    flush_lit!(lit, chunks);

    if errors.is_empty() {
        Ok(chunks)
    } else {
        Err(errors)
    }
}

/// A Coddl identifier: `XID_Start`/`_` then `XID_Continue`/`_`.
fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if is_xid_start(c) || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| is_xid_continue(c) || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> TemplateChunk {
        TemplateChunk::Literal(s.as_bytes().to_vec())
    }

    fn ok(text: &str) -> Vec<TemplateChunk> {
        parse_format_template(text).expect("template should parse")
    }

    fn names(chunks: &[TemplateChunk]) -> Vec<&str> {
        chunks
            .iter()
            .filter_map(|c| match c {
                TemplateChunk::Placeholder { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_text_is_one_literal() {
        assert_eq!(ok(r#"f"hello""#), vec![lit("hello")]);
    }

    #[test]
    fn single_placeholder_between_literals() {
        let chunks = ok(r#"f"Hello, {name}!""#);
        assert_eq!(
            chunks,
            vec![
                lit("Hello, "),
                TemplateChunk::Placeholder {
                    name: "name".into(),
                    range: 10..14
                },
                lit("!"),
            ]
        );
    }

    #[test]
    fn placeholder_offset_indexes_into_token_text() {
        // f"Hello, {name}!"
        //  0123456789…  -> `{` at 8, `name` spans bytes 9..13 of the token.
        let text = r#"f"Hello, {name}!""#;
        let chunks = ok(text);
        if let TemplateChunk::Placeholder { range, .. } = &chunks[1] {
            assert_eq!(&text[range.clone()], "name");
        } else {
            panic!("expected placeholder");
        }
    }

    #[test]
    fn leading_and_trailing_placeholders() {
        assert_eq!(names(&ok(r#"f"{a}{b}""#)), vec!["a", "b"]);
        assert_eq!(ok(r#"f"{a}{b}""#).len(), 2); // no empty literals between
    }

    #[test]
    fn escaped_braces_are_literal() {
        assert_eq!(ok(r#"f"{{not a placeholder}}""#), vec![lit("{not a placeholder}")]);
    }

    #[test]
    fn escapes_decode_like_strings() {
        assert_eq!(ok(r#"f"a\nb\"c""#), vec![lit("a\nb\"c")]);
    }

    #[test]
    fn unicode_escape_decodes() {
        assert_eq!(ok(r#"f"\u{41}{x}""#), vec![lit("A"), TemplateChunk::Placeholder { name: "x".into(), range: 9..10 }]);
    }

    #[test]
    fn empty_placeholder_errors() {
        let errs = parse_format_template(r#"f"{}""#).unwrap_err();
        assert_eq!(errs[0].kind, TemplateErrorKind::EmptyPlaceholder);
    }

    #[test]
    fn spaced_placeholder_is_invalid() {
        let errs = parse_format_template(r#"f"{ x }""#).unwrap_err();
        assert_eq!(errs[0].kind, TemplateErrorKind::InvalidPlaceholderIdent);
    }

    #[test]
    fn unmatched_open_brace_errors() {
        let errs = parse_format_template(r#"f"{abc""#).unwrap_err();
        assert_eq!(errs[0].kind, TemplateErrorKind::UnmatchedOpenBrace);
    }

    #[test]
    fn unmatched_close_brace_errors() {
        let errs = parse_format_template(r#"f"a}b""#).unwrap_err();
        assert_eq!(errs[0].kind, TemplateErrorKind::UnmatchedCloseBrace);
    }
}

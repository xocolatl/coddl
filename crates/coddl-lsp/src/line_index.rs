//! Map a byte offset in a UTF-8 source buffer to an LSP `Position`
//! (`line`, `character`). The LSP spec measures `character` in UTF-16
//! code units, so we compute UTF-16 widths per line up to the offset.
//!
//! Cheap to build and cheap to query: O(n) construction (one pass for
//! newline offsets), O(log n + line-prefix-length) per `position`
//! lookup. Carries an owned `Arc<str>` so it can live inside
//! analyzer snapshots that outlive a single request.

use std::sync::Arc;

use tower_lsp::lsp_types::Position;

pub struct LineIndex {
    /// Byte offset at which each line starts (line 0 always starts at
    /// byte 0; line N starts at the byte after the N-th '\n').
    line_starts: Vec<u32>,
    text: Arc<str>,
}

impl LineIndex {
    pub fn new(text: Arc<str>) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        Self { line_starts, text }
    }

    /// Convert a byte offset into an LSP `Position`. Out-of-range
    /// offsets clamp to the end of the file.
    pub fn position(&self, byte_offset: u32) -> Position {
        let offset = byte_offset.min(self.text.len() as u32);

        // `line_starts` is non-decreasing and starts with 0; the line
        // containing `offset` is the largest index whose entry is
        // ≤ offset.
        let line = match self.line_starts.binary_search(&offset) {
            Ok(idx) => idx,
            Err(idx) => idx - 1,
        };
        let line_start = self.line_starts[line] as usize;
        let line_text = &self.text[line_start..offset as usize];
        let character = line_text.encode_utf16().count() as u32;
        Position {
            line: line as u32,
            character,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(s: &str) -> LineIndex {
        LineIndex::new(Arc::from(s))
    }

    #[test]
    fn position_at_start() {
        let i = idx("hello\nworld\n");
        assert_eq!(i.position(0), Position::new(0, 0));
    }

    #[test]
    fn position_on_first_line() {
        let i = idx("hello\nworld\n");
        assert_eq!(i.position(3), Position::new(0, 3));
    }

    #[test]
    fn position_at_line_boundary() {
        let i = idx("hello\nworld\n");
        // Offset 6 is the first byte of "world" — start of line 1.
        assert_eq!(i.position(6), Position::new(1, 0));
    }

    #[test]
    fn position_on_second_line() {
        let i = idx("hello\nworld\n");
        assert_eq!(i.position(8), Position::new(1, 2));
    }

    #[test]
    fn position_clamps_past_end() {
        let i = idx("ab\n");
        // Past EOF — clamps to the end-of-buffer position.
        assert_eq!(i.position(99), Position::new(1, 0));
    }

    #[test]
    fn utf16_width_for_supplementary_codepoints() {
        // U+1F600 (😀) is two UTF-16 code units. `b` after the emoji
        // sits at UTF-16 character 3 (a=1, emoji=2, then b).
        let text = "a😀b";
        let i = idx(text);
        let b_offset = text.find('b').unwrap() as u32;
        assert_eq!(i.position(b_offset), Position::new(0, 3));
    }
}

//! Map a byte offset in a UTF-8 source buffer to an LSP `Position`
//! (`line`, `character`). The LSP spec measures `character` in UTF-16
//! code units, so we compute UTF-16 widths per line up to the offset.
//!
//! Cheap to build and cheap to query: O(n) construction (one pass for
//! newline offsets), O(log n + line-prefix-length) per `position`
//! lookup.

use tower_lsp::lsp_types::Position;

pub struct LineIndex<'src> {
    /// Byte offset at which each line starts (line 0 always starts at
    /// byte 0; line N starts at the byte after the N-th '\n').
    line_starts: Vec<u32>,
    text: &'src str,
}

impl<'src> LineIndex<'src> {
    pub fn new(text: &'src str) -> Self {
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
        // ≤ offset. The trailing EOF entry guarantees the search has
        // a right neighbor for every real line.
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

    #[test]
    fn position_at_start() {
        let idx = LineIndex::new("hello\nworld\n");
        assert_eq!(idx.position(0), Position::new(0, 0));
    }

    #[test]
    fn position_on_first_line() {
        let idx = LineIndex::new("hello\nworld\n");
        assert_eq!(idx.position(3), Position::new(0, 3));
    }

    #[test]
    fn position_at_line_boundary() {
        let idx = LineIndex::new("hello\nworld\n");
        // Offset 6 is the first byte of "world" — start of line 1.
        assert_eq!(idx.position(6), Position::new(1, 0));
    }

    #[test]
    fn position_on_second_line() {
        let idx = LineIndex::new("hello\nworld\n");
        assert_eq!(idx.position(8), Position::new(1, 2));
    }

    #[test]
    fn position_clamps_past_end() {
        let idx = LineIndex::new("ab\n");
        // Past EOF — clamps to the end-of-buffer position.
        assert_eq!(idx.position(99), Position::new(1, 0));
    }

    #[test]
    fn utf16_width_for_supplementary_codepoints() {
        // U+1F600 (😀) is two UTF-16 code units. Three bytes after the
        // emoji's start in UTF-8 (4 bytes), we're past the emoji.
        let text = "a😀b";
        let idx = LineIndex::new(text);
        let b_offset = text.find('b').unwrap() as u32;
        // `a` is 1 UTF-16 unit; the emoji is 2; so `b` is at char 3.
        assert_eq!(idx.position(b_offset), Position::new(0, 3));
    }
}

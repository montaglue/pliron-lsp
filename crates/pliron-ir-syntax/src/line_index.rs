//! Conversions between byte offsets, LSP positions (UTF-8 / UTF-16 / UTF-32
//! code units) and pliron positions (1-based line, 1-based char column).
//!
//! Only `\n` starts a new line (that is what both LSP and `combine` do); a
//! `\r` before it is an ordinary character for pliron and is ignored for
//! LSP column purposes only insofar as clients never point inside `\r\n`.

use crate::lexer::Offset;

/// How LSP positions count columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Encoding {
    Utf8,
    #[default]
    Utf16,
    Utf32,
}

/// A 0-based LSP-style position in some [`Encoding`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinePos {
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Debug)]
pub struct LineIndex {
    /// Byte offset of the start of every line.
    line_starts: Vec<Offset>,
    len: Offset,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as Offset + 1);
            }
        }
        LineIndex {
            line_starts,
            len: text.len() as Offset,
        }
    }

    pub fn len(&self) -> Offset {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    /// 0-based line containing `offset`.
    pub fn line_of(&self, offset: Offset) -> u32 {
        match self.line_starts.binary_search(&offset) {
            Ok(l) => l as u32,
            Err(l) => l as u32 - 1,
        }
    }

    pub fn line_start(&self, line: u32) -> Offset {
        self.line_starts
            .get(line as usize)
            .copied()
            .unwrap_or(self.len)
    }

    /// End of the line's content (before the `\n`, and before a `\r\n`).
    pub fn line_end(&self, line: u32, text: &str) -> Offset {
        let next = self
            .line_starts
            .get(line as usize + 1)
            .copied()
            .unwrap_or(self.len);
        let mut end = next;
        if end > self.line_start(line) && text.as_bytes().get(end as usize - 1) == Some(&b'\n') {
            end -= 1;
        }
        if end > self.line_start(line) && text.as_bytes().get(end as usize - 1) == Some(&b'\r') {
            end -= 1;
        }
        end
    }

    /// Convert a byte offset to an LSP position.
    pub fn position(&self, offset: Offset, text: &str, enc: Encoding) -> LinePos {
        let offset = offset.min(self.len);
        let line = self.line_of(offset);
        let start = self.line_start(line) as usize;
        let segment = &text[start..floor_char_boundary(text, offset as usize)];
        LinePos {
            line,
            col: count_units(segment, enc),
        }
    }

    /// Convert an LSP position to a byte offset (clamped to the line end).
    pub fn offset(&self, pos: LinePos, text: &str, enc: Encoding) -> Offset {
        if pos.line as usize >= self.line_starts.len() {
            return self.len;
        }
        let start = self.line_start(pos.line);
        // Up to (not including) the `\n`; a `\r` is an ordinary char here.
        let mut end = self.line_start(pos.line + 1);
        if end > start && text.as_bytes().get(end as usize - 1) == Some(&b'\n') {
            end -= 1;
        }
        let line_text = &text[start as usize..end as usize];
        let mut units = 0u32;
        for (i, c) in line_text.char_indices() {
            if units >= pos.col {
                return start + i as Offset;
            }
            units += char_units(c, enc);
        }
        end
    }

    /// Convert a pliron (1-based line, 1-based char column) to a byte offset.
    pub fn offset_of_pliron(&self, line: u32, column: u32, text: &str) -> Offset {
        let line0 = line.saturating_sub(1);
        if line0 as usize >= self.line_starts.len() {
            return self.len;
        }
        let start = self.line_start(line0);
        let next = self.line_start(line0 + 1);
        let line_text = &text[start as usize..next as usize];
        let col0 = column.saturating_sub(1) as usize;
        match line_text.char_indices().nth(col0) {
            Some((i, _)) => start + i as Offset,
            None => next,
        }
    }

    /// Convert a byte offset to a pliron (1-based line, 1-based char column).
    pub fn pliron_position(&self, offset: Offset, text: &str) -> (u32, u32) {
        let p = self.position(offset, text, Encoding::Utf32);
        (p.line + 1, p.col + 1)
    }
}

fn char_units(c: char, enc: Encoding) -> u32 {
    match enc {
        Encoding::Utf8 => c.len_utf8() as u32,
        Encoding::Utf16 => c.len_utf16() as u32,
        Encoding::Utf32 => 1,
    }
}

fn count_units(s: &str, enc: Encoding) -> u32 {
    match enc {
        Encoding::Utf8 => s.len() as u32,
        Encoding::Utf16 => s.chars().map(|c| c.len_utf16() as u32).sum(),
        Encoding::Utf32 => s.chars().count() as u32,
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_encodings() {
        let text = "ab\nx😀y\r\nzé\n";
        let li = LineIndex::new(text);
        assert_eq!(li.line_count(), 4);
        for enc in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            for (off, _) in text.char_indices() {
                let off = off as Offset;
                let p = li.position(off, text, enc);
                assert_eq!(li.offset(p, text, enc), off, "{enc:?} at {off}");
            }
        }
        // '😀' is 2 UTF-16 units, 1 UTF-32 unit, 4 UTF-8 bytes.
        let y = text.find('y').unwrap() as Offset;
        assert_eq!(li.position(y, text, Encoding::Utf16), LinePos { line: 1, col: 3 });
        assert_eq!(li.position(y, text, Encoding::Utf32), LinePos { line: 1, col: 2 });
        assert_eq!(li.position(y, text, Encoding::Utf8), LinePos { line: 1, col: 5 });
    }

    #[test]
    fn pliron_positions() {
        let text = "a😀b\n  cd";
        let li = LineIndex::new(text);
        let b = text.find('b').unwrap() as Offset;
        assert_eq!(li.pliron_position(b, text), (1, 3));
        assert_eq!(li.offset_of_pliron(1, 3, text), b);
        let c = text.find('c').unwrap() as Offset;
        assert_eq!(li.offset_of_pliron(2, 3, text), c);
        // Past the end of a line clamps to the next line start.
        assert_eq!(li.offset_of_pliron(1, 99, text), text.find('\n').unwrap() as Offset + 1);
    }

    #[test]
    fn clamping() {
        let text = "abc\ndef";
        let li = LineIndex::new(text);
        assert_eq!(li.offset(LinePos { line: 0, col: 99 }, text, Encoding::Utf16), 3);
        assert_eq!(li.offset(LinePos { line: 9, col: 0 }, text, Encoding::Utf16), 7);
        assert_eq!(li.line_end(0, text), 3);
    }
}

//! Open documents and position conversions.

use std::sync::Arc;

use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use pliron_ir_syntax::{Analysis, Encoding, Knowledge, LineIndex, LinePos, Offset, analyze};

use crate::exact::Exact;

pub struct Document {
    pub text: String,
    pub version: i32,
    pub line_index: LineIndex,
    pub hash: u64,
    /// Instant, dialect-agnostic analysis (fallback).
    pub syntax: Arc<Analysis>,
    /// Exact analysis from a dialect engine, for the text with hash
    /// `exact.text_hash` (may be stale).
    pub exact: Option<Arc<Exact>>,
    /// Why there is no (fresh) exact analysis, if known.
    pub engine_note: Option<String>,
}

impl Document {
    pub fn new(text: String, version: i32, knowledge: &Knowledge) -> Document {
        let line_index = LineIndex::new(&text);
        let hash = pliron_lsp_protocol::text_hash(&text);
        let syntax = Arc::new(analyze(&text, knowledge));
        Document {
            text,
            version,
            line_index,
            hash,
            syntax,
            exact: None,
            engine_note: None,
        }
    }

    /// The exact analysis, if it is for the current text.
    pub fn fresh_exact(&self) -> Option<&Exact> {
        self.exact.as_deref().filter(|x| x.text_hash == self.hash)
    }

    pub fn apply_changes(
        &mut self,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        enc: Encoding,
        knowledge: &Knowledge,
    ) {
        for change in changes {
            match change.range {
                None => self.text = change.text,
                Some(range) => {
                    let li = LineIndex::new(&self.text);
                    let start = li.offset(lsp_to_linepos(range.start), &self.text, enc) as usize;
                    let end = li.offset(lsp_to_linepos(range.end), &self.text, enc) as usize;
                    let (start, end) = (start.min(end), start.max(end));
                    self.text.replace_range(start..end, &change.text);
                }
            }
        }
        self.version = version;
        self.reanalyze(knowledge);
    }

    pub fn reanalyze(&mut self, knowledge: &Knowledge) {
        self.line_index = LineIndex::new(&self.text);
        self.hash = pliron_lsp_protocol::text_hash(&self.text);
        self.syntax = Arc::new(analyze(&self.text, knowledge));
    }

    pub fn offset(&self, pos: Position, enc: Encoding) -> Offset {
        self.line_index.offset(lsp_to_linepos(pos), &self.text, enc)
    }

    pub fn position(&self, off: Offset, enc: Encoding) -> Position {
        let p = self.line_index.position(off, &self.text, enc);
        Position {
            line: p.line,
            character: p.col,
        }
    }

    pub fn range(&self, (s, e): (Offset, Offset), enc: Encoding) -> Range {
        Range {
            start: self.position(s, enc),
            end: self.position(e, enc),
        }
    }

    pub fn slice(&self, (s, e): (Offset, Offset)) -> &str {
        &self.text[s as usize..e as usize]
    }
}

pub fn lsp_to_linepos(p: Position) -> LinePos {
    LinePos {
        line: p.line,
        col: p.character,
    }
}

//! Formatting: indentation and whitespace only.
//!
//! Re-printing with pliron would rename values and add outlined attributes,
//! so the formatter only re-indents lines according to the block / region
//! structure, trims trailing whitespace and ensures a final newline. It
//! never edits inside multi-line strings and does nothing while brackets are
//! unbalanced.

use pliron_ir_syntax::lexer::{Offset, TokenKind};
use pliron_ir_syntax::tree::{BlockId, ErrorKind, StmtId, Tree};

use crate::document::Document;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    None,
    StmtFirst(StmtId),
    StmtCont(StmtId),
    HeaderFirst(BlockId),
    HeaderCont(BlockId),
    RegionClose(u32),
    Outlined,
}

/// Indentation style: are a block's ops indented one level deeper than its
/// header (pliron's printer) or at the same level?
fn ops_deeper_than_header(tree: &Tree, text: &str) -> bool {
    let col = |off: Offset| {
        let line_start = text[..off as usize].rfind('\n').map(|p| p + 1).unwrap_or(0);
        text[line_start..off as usize].chars().count()
    };
    let (mut deeper, mut same) = (0, 0);
    for b in tree.block_ids() {
        let blk = tree.block(b);
        if tree.tok(blk.label).kind != TokenKind::BlockLabel {
            continue;
        }
        if let Some(first) = blk.stmts.first() {
            let s = tree.stmt(*first);
            let (hc, sc) = (col(tree.tok(blk.label).start), col(tree.tok(s.first).start));
            if sc > hc {
                deeper += 1;
            } else {
                same += 1;
            }
        }
    }
    deeper >= same
}

/// Desired indentation level of every line (`None`: leave it alone).
#[allow(clippy::needless_range_loop)]
fn line_levels(doc: &Document) -> Option<Vec<Option<usize>>> {
    let tree = &doc.syntax.tree;
    let text = &doc.text;
    if !doc.syntax.tree.lex_errors.is_empty()
        || tree.errors.iter().any(|e| {
            matches!(e.kind, ErrorKind::Unclosed { .. }) || e.message.starts_with("unmatched")
        })
    {
        return None;
    }
    let deeper = ops_deeper_than_header(tree, text);
    let mut owner = vec![Owner::None; tree.tokens.len()];
    let mut stmt_level = vec![0usize; tree.stmts.len()];
    let mut block_level = vec![0usize; tree.blocks.len()];

    // Levels, top-down (statements are numbered in pre-order).
    for i in 0..tree.stmts.len() {
        let sid = StmtId(i as u32);
        let level = match tree.stmt(sid).parent_block {
            None => 0,
            Some(b) => block_level[b.0 as usize] + usize::from(deeper),
        };
        stmt_level[i] = level;
        for r in &tree.stmt(sid).regions {
            for b in &tree.region(*r).blocks {
                block_level[b.0 as usize] = level + 1;
            }
        }
    }
    for i in 0..tree.stmts.len() {
        let sid = StmtId(i as u32);
        let s = tree.stmt(sid);
        for t in s.results.iter().chain(s.op_name.iter()).chain(s.body.iter()).chain(s.semicolon.iter()) {
            owner[*t as usize] = Owner::StmtCont(sid);
        }
        owner[s.first as usize] = Owner::StmtFirst(sid);
        for r in &s.regions {
            let reg = tree.region(*r);
            owner[reg.open as usize] = Owner::StmtCont(sid);
            if let Some(c) = reg.close {
                owner[c as usize] = Owner::RegionClose(r.0);
            }
        }
    }
    for b in tree.block_ids() {
        let blk = tree.block(b);
        if tree.tok(blk.label).kind != TokenKind::BlockLabel {
            continue;
        }
        let end = blk.colon.unwrap_or(blk.label);
        for t in blk.label..=end {
            owner[t as usize] = Owner::HeaderCont(b);
        }
        owner[blk.label as usize] = Owner::HeaderFirst(b);
    }
    if let Some(o) = &tree.outlined {
        for o in owner.iter_mut().skip(o.header as usize) {
            *o = Owner::Outlined;
        }
    }

    let li = &doc.line_index;
    let mut levels = vec![None; li.line_count() as usize];
    let mut ti = 0;
    for (line, level) in levels.iter_mut().enumerate() {
        let start = li.line_start(line as u32);
        let end = li.line_end(line as u32, text);
        while ti < tree.tokens.len() && tree.tokens[ti].end <= start {
            ti += 1;
        }
        let Some(tok) = tree.tokens.get(ti) else {
            break;
        };
        if tok.start < start {
            // The line starts inside a token (a multi-line string).
            continue;
        }
        if tok.start >= end && end > start {
            continue;
        }
        if tok.start > end {
            continue;
        }
        *level = match owner[ti] {
            Owner::None => None,
            Owner::StmtFirst(s) => Some(stmt_level[s.0 as usize]),
            Owner::StmtCont(s) => Some(stmt_level[s.0 as usize] + 1),
            Owner::HeaderFirst(b) => Some(block_level[b.0 as usize]),
            Owner::HeaderCont(b) => Some(block_level[b.0 as usize] + 1),
            Owner::RegionClose(r) => Some(stmt_level[tree.regions[r as usize].owner.0 as usize]),
            Owner::Outlined => Some(0),
        };
    }
    Some(levels)
}

/// A text edit in byte offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub start: Offset,
    pub end: Offset,
    pub text: String,
}

/// Formatting edits for `doc` (`lines`: optional inclusive line range).
/// Returns `None` when the document cannot be formatted safely.
pub fn format_edits(doc: &Document, unit: &str, lines: Option<(u32, u32)>) -> Option<Vec<Edit>> {
    let levels = line_levels(doc)?;
    let text = &doc.text;
    let li = &doc.line_index;
    let tokens = &doc.syntax.tree.tokens;
    let in_token = |off: Offset| tokens.iter().any(|t| t.start < off && off < t.end);
    let mut edits = Vec::new();
    for (line, level) in levels.iter().enumerate() {
        let line = line as u32;
        if let Some((a, b)) = lines
            && (line < a || line > b)
        {
            continue;
        }
        let start = li.line_start(line);
        let end = li.line_end(line, text);
        let content = &text[start as usize..end as usize];
        if in_token(start) {
            continue; // inside a multi-line string
        }
        let trimmed = content.trim_start();
        if trimmed.is_empty() {
            if !content.is_empty() {
                edits.push(Edit { start, end, text: String::new() });
            }
            continue;
        }
        let indent_len = (content.len() - trimmed.len()) as Offset;
        if let Some(level) = level {
            let want = unit.repeat(*level);
            if content[..indent_len as usize] != want {
                edits.push(Edit {
                    start,
                    end: start + indent_len,
                    text: want,
                });
            }
        }
        let trailing = content.len() - content.trim_end().len();
        if trailing > 0 && !in_token(end) {
            edits.push(Edit {
                start: end - trailing as Offset,
                end,
                text: String::new(),
            });
        }
    }
    if lines.is_none() && !text.is_empty() && !text.ends_with('\n') {
        let end = text.len() as Offset;
        edits.push(Edit { start: end, end, text: "\n".into() });
    }
    Some(edits)
}

/// Apply edits (sorted, non-overlapping) to a text.
pub fn apply(text: &str, edits: &[Edit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    let mut edits = edits.to_vec();
    edits.sort_by_key(|e| e.start);
    for e in edits {
        out.push_str(&text[pos..e.start as usize]);
        out.push_str(&e.text);
        pos = e.end as usize;
    }
    out.push_str(&text[pos..]);
    out
}

/// Format a whole text (used by `pliron-lsp fmt`).
pub fn format_text(text: &str, unit: &str) -> Option<String> {
    let doc = Document::new(text.to_string(), 0, &Default::default());
    format_edits(&doc, unit, None).map(|e| apply(text, &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reindents_printer_style() {
        let src = "builtin.module @m {\n^e():\n      llvm.func @f: t [] {\n  ^bb0(x: i64,\ny: i64):\n llvm.return x   \n}\n}";
        let out = format_text(src, "  ").unwrap();
        assert_eq!(
            out,
            "builtin.module @m {\n  ^e():\n    llvm.func @f: t [] {\n      ^bb0(x: i64,\n        y: i64):\n        llvm.return x\n    }\n}\n"
        );
        // Idempotent.
        assert_eq!(format_text(&out, "  ").unwrap(), out);
    }

    #[test]
    fn keeps_compact_style_and_strings() {
        let src = "builtin.module @m {\n  ^e():\n  t.s \"a\n   b\"\n}\n";
        let out = format_text(src, "  ").unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn refuses_unbalanced() {
        assert!(format_text("builtin.module @m {\n^e():\n t.r\n", "  ").is_none());
    }
}

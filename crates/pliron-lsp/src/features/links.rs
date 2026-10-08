//! Source locations written in IR, as pliron prints them in the
//! `outlined_attributes:` section (`@["src/kernel.rs": line: 12, column: 5]`,
//! also inside `fused[...]`, `callsite(...)` and `name: .., loc: (..)`),
//! become links to that file and position.

use std::path::{Path, PathBuf};

use pliron_ir_syntax::{Offset, TokenKind};

use crate::document::Document;
use crate::exact::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLoc {
    /// `"path": line: N, column: M`
    pub range: Range,
    pub path: String,
    /// 1-based.
    pub line: u32,
    /// 1-based, in characters.
    pub column: u32,
}

/// All source locations in a document.
pub fn source_locations(doc: &Document) -> Vec<SourceLoc> {
    let text = &doc.text;
    let mut out = Vec::new();
    for t in &doc.syntax.tree.tokens {
        if t.kind != TokenKind::String || t.end - t.start < 2 {
            continue;
        }
        let Some((line, column, end)) = position_after(text, t.end as usize) else {
            continue;
        };
        let raw = &text[t.start as usize + 1..t.end as usize - 1];
        out.push(SourceLoc {
            range: (t.start, end as Offset),
            path: raw.replace("\\\"", "\"").replace("\\\\", "\\"),
            line,
            column,
        });
    }
    out
}

/// The location at `off`.
pub fn location_at(doc: &Document, off: Offset) -> Option<SourceLoc> {
    source_locations(doc)
        .into_iter()
        .find(|l| l.range.0 <= off && off <= l.range.1)
}

/// Parse `: line: N, column: M` at `i`; returns (line, column, end).
fn position_after(text: &str, mut i: usize) -> Option<(u32, u32, usize)> {
    let b = text.as_bytes();
    let skip_ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    let expect = |i: &mut usize, s: &str| {
        skip_ws(i);
        let ok = text[*i..].starts_with(s);
        if ok {
            *i += s.len();
        }
        ok
    };
    let number = |i: &mut usize| -> Option<u32> {
        skip_ws(i);
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        text[start..*i].parse().ok()
    };
    if !(expect(&mut i, ":") && expect(&mut i, "line") && expect(&mut i, ":")) {
        return None;
    }
    let line = number(&mut i)?;
    if !(expect(&mut i, ",") && expect(&mut i, "column") && expect(&mut i, ":")) {
        return None;
    }
    let column = number(&mut i)?;
    Some((line.max(1), column.max(1), i))
}

/// The file a location's path refers to: absolute, or relative to the IR
/// file's directory or one of its parents (producers usually record paths
/// relative to their crate), or to a workspace folder.
pub fn resolve(path: &str, ir_file: Option<&Path>, roots: &[PathBuf]) -> Option<PathBuf> {
    let p = Path::new(path);
    if path.is_empty() || path == "<in-memory>" {
        return None;
    }
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    ir_file
        .and_then(Path::parent)
        .into_iter()
        .flat_map(Path::ancestors)
        .chain(roots.iter().map(PathBuf::as_path))
        .map(|base| base.join(p))
        .find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_locations_in_outlined_entries() {
        let text = "builtin.module @m {\n^e():\n  t.c !0\n}\n\noutlined_attributes:\n!0 = @[\"src/a.rs\": line: 12, column: 5], []\n!1 = @[fused[\"/abs/b.rs\": line: 3, column: 1, <in-memory>: line: 1, column: 1]], []\n!2 = [k = \"not: a location\"]\n";
        let doc = Document::new(text.into(), 0, &Default::default());
        let locs = source_locations(&doc);
        assert_eq!(locs.len(), 2, "{locs:#?}");
        assert_eq!((locs[0].path.as_str(), locs[0].line, locs[0].column), ("src/a.rs", 12, 5));
        assert_eq!(doc.slice(locs[0].range), "\"src/a.rs\": line: 12, column: 5");
        assert_eq!(locs[1].path, "/abs/b.rs");
        assert!(location_at(&doc, text.find("line: 12").unwrap() as u32).is_some());
    }

    #[test]
    fn resolves_relative_to_parents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target/out")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        let ir = dir.path().join("target/out/k.pliron");
        assert_eq!(resolve("src/a.rs", Some(&ir), &[]), Some(dir.path().join("src/a.rs")));
        assert_eq!(resolve("src/missing.rs", Some(&ir), &[]), None);
        assert_eq!(resolve("<in-memory>", Some(&ir), &[]), None);
    }
}

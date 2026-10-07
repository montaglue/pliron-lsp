//! Document symbols (outline) and folding ranges.

use lsp_types::{DocumentSymbol, FoldingRange, FoldingRangeKind, SymbolKind};
use pliron_ir_syntax::{Encoding, Offset, StmtId};
use pliron_lsp_protocol::op_traits;

use crate::document::Document;

#[allow(deprecated, clippy::too_many_arguments)]
fn symbol(
    doc: &Document,
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    range: (Offset, Offset),
    selection: (Offset, Offset),
    children: Vec<DocumentSymbol>,
    enc: Encoding,
) -> DocumentSymbol {
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range: doc.range(range, enc),
        selection_range: doc.range(selection, enc),
        children: (!children.is_empty()).then_some(children),
    }
}

pub fn document_symbols(doc: &Document, enc: Encoding) -> Vec<DocumentSymbol> {
    match doc.fresh_exact() {
        Some(x) => {
            let root = x.model.root;
            exact_symbols(doc, x, root, enc)
        }
        None => doc
            .syntax
            .tree
            .top
            .iter()
            .flat_map(|s| syntax_symbols(doc, *s, enc))
            .collect(),
    }
}

fn exact_symbols(
    doc: &Document,
    x: &crate::exact::Exact,
    op: u32,
    enc: Encoding,
) -> Vec<DocumentSymbol> {
    let info = &x.model.ops[op as usize];
    let mut children = Vec::new();
    for r in &info.regions {
        for b in &x.model.regions[*r as usize].blocks {
            for child in &x.model.blocks[*b as usize].ops {
                children.extend(exact_symbols(doc, x, *child, enc));
            }
        }
    }
    let Some(sym) = x.symbols.iter().find(|s| s.op == op) else {
        return children;
    };
    let Some((range, _)) = x.op_span.get(&op).copied() else {
        return children;
    };
    let kind = if info.traits & op_traits::SYMBOL_TABLE != 0 {
        SymbolKind::MODULE
    } else if !info.regions.is_empty() {
        SymbolKind::FUNCTION
    } else {
        SymbolKind::VARIABLE
    };
    vec![symbol(
        doc,
        format!("@{}", sym.name),
        Some(info.opid.clone()),
        kind,
        range,
        sym.range,
        children,
        enc,
    )]
}

fn syntax_symbols(doc: &Document, stmt: StmtId, enc: Encoding) -> Vec<DocumentSymbol> {
    let a = &doc.syntax;
    let s = a.tree.stmt(stmt);
    let mut children = Vec::new();
    for r in &s.regions {
        for b in &a.tree.region(*r).blocks {
            for child in &a.tree.block(*b).stmts {
                children.extend(syntax_symbols(doc, *child, enc));
            }
        }
    }
    let Some(def) = a.stmt_symbol[stmt.0 as usize] else {
        return children;
    };
    let d = a.def(def);
    let tok = a.tree.tok(d.tok);
    let opname = s.op_name.map(|t| a.tree.tok(t).text(&doc.text).to_string());
    let kind = if s.parent_block.is_none() {
        SymbolKind::MODULE
    } else if !s.regions.is_empty() {
        SymbolKind::FUNCTION
    } else {
        SymbolKind::VARIABLE
    };
    vec![symbol(
        doc,
        format!("@{}", d.name),
        opname,
        kind,
        a.tree.stmt_range(stmt),
        (tok.start, tok.end),
        children,
        enc,
    )]
}

pub fn folding_ranges(doc: &Document) -> Vec<FoldingRange> {
    let li = &doc.line_index;
    let mut ranges: Vec<(Offset, Offset)> = match doc.fresh_exact() {
        Some(x) => x.region_spans.iter().map(|(r, _)| *r).collect(),
        None => doc
            .syntax
            .tree
            .region_ids()
            .map(|r| doc.syntax.tree.region_range(r))
            .collect(),
    };
    // Blocks fold too (syntax layer knows their extent in both cases).
    for b in doc.syntax.tree.block_ids() {
        ranges.push(doc.syntax.tree.block_range(b));
    }
    if let Some(o) = &doc.syntax.tree.outlined {
        let start = doc.syntax.tree.tok(o.header).start;
        ranges.push((start, doc.text.len() as Offset));
    }
    let mut out: Vec<FoldingRange> = ranges
        .into_iter()
        .filter_map(|(s, e)| {
            let (sl, el) = (li.line_of(s), li.line_of(e.saturating_sub(1).max(s)));
            (el > sl).then_some(FoldingRange {
                start_line: sl,
                start_character: None,
                end_line: el.saturating_sub(1).max(sl),
                end_character: None,
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            })
        })
        .collect();
    out.sort_by_key(|f| (f.start_line, f.end_line));
    out.dedup_by_key(|f| (f.start_line, f.end_line));
    out
}

//! The exact analysis of a document: an engine result (from real, compiled
//! pliron + dialect parsers) mapped onto byte offsets of the document text.

use std::collections::HashMap;

use pliron_ir_syntax::lexer::{Offset, TokenKind, lex};
use pliron_ir_syntax::LineIndex;
use pliron_lsp_protocol::{AnalyzeResult, DiagPhase, Model, Pos, SpanKind, ValueDef};

pub type Range = (Offset, Offset);

/// A recorded span, in byte offsets.
#[derive(Clone, Debug)]
pub struct XSpan {
    pub start: Offset,
    pub end: Offset,
    pub kind: SpanKind,
}

#[derive(Clone, Debug)]
pub struct XDiag {
    pub range: Range,
    pub message: String,
    pub phase: DiagPhase,
}

/// A symbol defined by an op (`@name`).
#[derive(Clone, Debug)]
pub struct SymbolDef {
    pub name: String,
    pub op: u32,
    pub range: Range,
}

#[derive(Clone, Debug, Default)]
pub struct Exact {
    pub text_hash: u64,
    pub model: Model,
    /// All spans, sorted by (start, -end).
    pub spans: Vec<XSpan>,
    pub value_def: HashMap<u32, Range>,
    pub value_uses: HashMap<u32, Vec<Range>>,
    pub block_def: HashMap<u32, Range>,
    pub block_uses: HashMap<u32, Vec<Range>>,
    /// Op index -> (whole span, name span).
    pub op_span: HashMap<u32, (Range, Range)>,
    pub region_spans: Vec<(Range, bool)>,
    pub symbols: Vec<SymbolDef>,
    /// `@name` uses: (range, name).
    pub symbol_uses: Vec<(Range, String)>,
    pub diagnostics: Vec<XDiag>,
    pub elapsed_us: u64,
}

fn to_offset(li: &LineIndex, text: &str, p: Pos) -> Offset {
    li.offset_of_pliron(p.line, p.column, text)
}

/// Shrink a range so it does not end in whitespace.
fn trim_end(text: &str, (s, mut e): Range) -> Range {
    while e > s
        && text[..e as usize]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        e -= text[..e as usize].chars().next_back().unwrap().len_utf8() as Offset;
    }
    (s, e)
}

impl Exact {
    pub fn new(res: AnalyzeResult, text: &str, li: &LineIndex) -> Exact {
        let mut x = Exact {
            text_hash: res.text_hash,
            model: res.model.unwrap_or_default(),
            elapsed_us: res.elapsed_us,
            ..Exact::default()
        };
        for s in res.spans {
            let start = to_offset(li, text, s.start);
            let end = to_offset(li, text, s.end).max(start);
            let (start, end) = trim_end(text, (start, end));
            let r = (start, end);
            match &s.kind {
                SpanKind::ResultDef { value } | SpanKind::BlockArgDef { value } => {
                    x.value_def.entry(*value).or_insert(r);
                }
                SpanKind::OperandUse { value } => x.value_uses.entry(*value).or_default().push(r),
                SpanKind::BlockLabel { block } => {
                    x.block_def.entry(*block).or_insert(r);
                }
                SpanKind::SuccessorUse { block } => x.block_uses.entry(*block).or_default().push(r),
                SpanKind::Op {
                    op,
                    name_start,
                    name_end,
                } => {
                    let ns = to_offset(li, text, *name_start);
                    let ne = to_offset(li, text, *name_end).max(ns);
                    x.op_span.insert(*op, (r, (ns, ne)));
                }
                SpanKind::Region { closed } => {
                    // A region span is `{` .. `}` (inclusive of the brace).
                    let end = if *closed { end + 1 } else { end };
                    x.region_spans.push(((start, end), *closed));
                }
                _ => {}
            }
            x.spans.push(XSpan {
                start,
                end,
                kind: s.kind,
            });
        }
        x.spans
            .sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));

        let (tokens, _) = lex(text);
        // Symbol definitions: the first `@name` token inside the op's own
        // span (outside its regions) that matches the op's symbol name.
        for (op, info) in x.model.ops.iter().enumerate() {
            let Some(name) = &info.symbol else { continue };
            let Some(((s, e), _)) = x.op_span.get(&(op as u32)).copied() else {
                continue;
            };
            let regions: Vec<Range> = x
                .region_spans
                .iter()
                .map(|(r, _)| *r)
                .filter(|(rs, re)| s <= *rs && *re <= e)
                .collect();
            let def = tokens.iter().find(|t| {
                t.kind == TokenKind::SymbolRef
                    && s <= t.start
                    && t.end <= e
                    && t.name(text) == name
                    && !regions.iter().any(|(rs, re)| *rs <= t.start && t.end <= *re)
            });
            if let Some(t) = def {
                x.symbols.push(SymbolDef {
                    name: name.clone(),
                    op: op as u32,
                    range: (t.start, t.end),
                });
            }
        }
        for t in &tokens {
            if t.kind == TokenKind::SymbolRef
                && !x.symbols.iter().any(|d| d.range == (t.start, t.end))
            {
                x.symbol_uses.push(((t.start, t.end), t.name(text).to_string()));
            }
        }

        let token_range = |off: Offset| -> Range {
            match tokens.iter().find(|t| t.start <= off && off < t.end) {
                Some(t) => (t.start, t.end),
                None => {
                    let end = text[off as usize..]
                        .chars()
                        .next()
                        .map(|c| off + c.len_utf8() as Offset)
                        .unwrap_or(off);
                    (off, end)
                }
            }
        };
        for d in res.parse_errors.into_iter().chain(res.verify_errors) {
            let range = match d.pos {
                Some(p) => token_range(to_offset(li, text, p)),
                None => (0, 0),
            };
            x.diagnostics.push(XDiag {
                range,
                message: d.message,
                phase: d.phase,
            });
        }
        x
    }

    /// The innermost span (of any kind) containing `off`, preferring the
    /// smallest one.
    pub fn spans_at(&self, off: Offset) -> impl Iterator<Item = &XSpan> {
        self.spans
            .iter()
            .filter(move |s| s.start <= off && off <= s.end && s.start < s.end)
    }

    /// The most specific span at `off`, ignoring structural spans (ops and
    /// regions).
    pub fn leaf_at(&self, off: Offset) -> Option<&XSpan> {
        self.spans_at(off)
            .filter(|s| !matches!(s.kind, SpanKind::Op { .. } | SpanKind::Region { .. }))
            .min_by_key(|s| s.end - s.start)
    }

    /// The innermost op whose span contains `off`.
    pub fn op_at(&self, off: Offset) -> Option<u32> {
        self.op_span
            .iter()
            .filter(|(_, ((s, e), _))| *s <= off && off <= *e)
            .min_by_key(|(_, ((s, e), _))| e - s)
            .map(|(op, _)| *op)
    }

    /// Op whose name span contains `off`.
    pub fn op_name_at(&self, off: Offset) -> Option<u32> {
        self.op_span
            .iter()
            .find(|(_, (_, (s, e)))| *s <= off && off <= *e)
            .map(|(op, _)| *op)
    }

    pub fn symbol_def_by_name(&self, name: &str) -> Option<&SymbolDef> {
        self.symbols.iter().find(|s| s.name == name)
    }

    pub fn symbol_at(&self, off: Offset) -> Option<String> {
        self.symbols
            .iter()
            .find(|d| d.range.0 <= off && off <= d.range.1)
            .map(|d| d.name.clone())
            .or_else(|| {
                self.symbol_uses
                    .iter()
                    .find(|(r, _)| r.0 <= off && off <= r.1)
                    .map(|(_, n)| n.clone())
            })
    }

    /// Value defined or used at `off`.
    pub fn value_at(&self, off: Offset) -> Option<u32> {
        self.spans_at(off).find_map(|s| match s.kind {
            SpanKind::ResultDef { value }
            | SpanKind::BlockArgDef { value }
            | SpanKind::OperandUse { value } => Some(value),
            _ => None,
        })
    }

    /// Block whose label is defined or used at `off`.
    pub fn block_at(&self, off: Offset) -> Option<u32> {
        self.spans_at(off).find_map(|s| match s.kind {
            SpanKind::BlockLabel { block } | SpanKind::SuccessorUse { block } => Some(block),
            _ => None,
        })
    }

    pub fn is_unresolved(&self, value: u32) -> bool {
        matches!(
            self.model.values.get(value as usize).map(|v| v.def),
            Some(ValueDef::Detached { unresolved: true })
        )
    }
}

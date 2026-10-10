//! The exact analysis of a document: an engine result (from real, compiled
//! pliron + dialect parsers) mapped onto byte offsets of the document text.

use std::collections::HashMap;

use pliron_ir_syntax::LineIndex;
use pliron_ir_syntax::lexer::{Offset, TokenKind, lex};
use pliron_lsp_protocol::{
    AnalyzeResult, DiagPhase, HookEdit, HookFix, HookSeverity, HookTarget, Model, Pos, SpanKind,
    ValueDef,
};

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
    pub severity: HookSeverity,
    /// The lint hook that reported it (for [`DiagPhase::Lint`]).
    pub source: Option<String>,
    /// Quick fixes from the lint (text edits).
    pub fixes: Vec<XFix>,
}

#[derive(Clone, Debug)]
pub struct XFix {
    pub title: String,
    pub edits: Vec<(Range, String)>,
}

/// An inlay hint from a dialect hook.
#[derive(Clone, Debug)]
pub struct XHint {
    pub offset: Offset,
    pub label: String,
    /// Shown before the entity (operands) rather than after it.
    pub before: bool,
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
    pub hints: Vec<XHint>,
    /// The IR as pliron prints it (with the round trip).
    pub printed: Option<String>,
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

fn tk_range(t: &pliron_ir_syntax::Token) -> Range {
    (t.start, t.end)
}

/// The text to delete to remove the statement of an op spanning `op`: with
/// its `;` (the one after it, or for the last op of a block the one before
/// it) and, when it is alone on its lines, the whole lines.
pub fn remove_op_range(text: &str, (s, e): Range) -> Range {
    let b = text.as_bytes();
    let (s, e) = (s as usize, e as usize);
    let mut end = e;
    while end < b.len() && matches!(b[end], b' ' | b'\t') {
        end += 1;
    }
    let mut start = s;
    if end < b.len() && b[end] == b';' {
        end += 1;
        // The rest of the line, if blank.
        let mut j = end;
        while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\r') {
            j += 1;
        }
        let line_start = text[..s].rfind('\n').map_or(0, |p| p + 1);
        if (j >= b.len() || b[j] == b'\n') && text[line_start..s].trim().is_empty() {
            start = line_start;
            end = (j + 1).min(b.len());
        }
    } else {
        // The last op of a block: drop the `;` before it instead.
        let mut i = s;
        while i > 0 && b[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        if i > 0 && b[i - 1] == b';' {
            start = i - 1;
        } else {
            // The only op: its whole line(s), if alone on them.
            let line_start = text[..s].rfind('\n').map_or(0, |p| p + 1);
            let mut j = end;
            while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\r') {
                j += 1;
            }
            if text[line_start..s].trim().is_empty() && (j >= b.len() || b[j] == b'\n') {
                start = line_start;
                end = (j + 1).min(b.len());
            }
        }
    }
    (start as Offset, end as Offset)
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
        x.spans.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));

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
                    && !regions
                        .iter()
                        .any(|(rs, re)| *rs <= t.start && t.end <= *re)
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
                x.symbol_uses
                    .push(((t.start, t.end), t.name(text).to_string()));
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
            let mut range = match d.pos {
                Some(p) => token_range(to_offset(li, text, p)),
                None => (0, 0),
            };
            // pliron reports unknown names at the start of the op / type;
            // point at the name itself.
            if let Some((kind, name)) = crate::features::actions::unregistered(&d.message) {
                let from = range.0;
                let found = tokens.iter().find(|t| {
                    t.start >= from
                        && t.start <= from + 4096
                        && match kind {
                            "dialect" => {
                                t.text(text) == name || t.text(text).split('.').next() == Some(name)
                            }
                            _ => t.text(text) == name,
                        }
                });
                if let Some(t) = found {
                    range = (t.start, t.end);
                }
            }
            x.diagnostics.push(XDiag {
                range,
                message: d.message,
                phase: d.phase,
                severity: HookSeverity::Error,
                source: None,
                fixes: Vec::new(),
            });
        }
        // Round trip problems: at the name of the op they are about.
        x.printed = res.printed;
        for d in res.round_trip {
            let Some(p) = d.pos else { continue };
            let off = to_offset(li, text, p);
            let range = x
                .op_span
                .values()
                .find(|(whole, _)| whole.0 == off)
                .map(|(_, name)| *name)
                .unwrap_or_else(|| token_range(off));
            x.diagnostics.push(XDiag {
                range,
                message: d.message,
                phase: DiagPhase::RoundTrip,
                severity: HookSeverity::Warning,
                source: None,
                fixes: Vec::new(),
            });
        }
        for d in res.hook_diags {
            let Some(range) = x
                .target_range(d.op, d.target)
                .or_else(|| x.target_range(d.op, HookTarget::OpName))
            else {
                continue;
            };
            x.diagnostics.push(XDiag {
                range,
                message: d.message,
                phase: DiagPhase::Lint,
                severity: d.severity,
                source: Some(d.source),
                fixes: d
                    .fixes
                    .iter()
                    .filter_map(|f| x.fix_edits(text, &tokens, d.op, f))
                    .collect(),
            });
        }
        for h in res.hook_hints {
            let Some((s, e)) = x.target_range(h.op, h.target) else {
                continue;
            };
            let before = matches!(h.target, HookTarget::Operand { .. });
            x.hints.push(XHint {
                offset: if before { s } else { e },
                label: h.label,
                before,
            });
        }
        x
    }

    /// The text edits of a lint's quick fix, or `None` if a target is not in
    /// the text.
    fn fix_edits(
        &self,
        text: &str,
        tokens: &[pliron_ir_syntax::Token],
        op: u32,
        fix: &HookFix,
    ) -> Option<XFix> {
        let mut edits = Vec::new();
        for e in &fix.edits {
            edits.push(match e {
                HookEdit::Replace { target, text: t } => {
                    (self.target_range(op, *target)?, t.clone())
                }
                HookEdit::InsertBefore { target, text: t } => {
                    let (s, _) = self.target_range(op, *target)?;
                    ((s, s), t.clone())
                }
                HookEdit::InsertAfter { target, text: t } => {
                    let (_, e) = self.target_range(op, *target)?;
                    ((e, e), t.clone())
                }
                HookEdit::ReplaceWord {
                    target,
                    word,
                    text: t,
                } => {
                    let (s, e) = self.target_range(op, *target)?;
                    // Not inside the op's regions.
                    let regions: Vec<Range> = self
                        .region_spans
                        .iter()
                        .map(|(r, _)| *r)
                        .filter(|(rs, re)| s <= *rs && *re <= e)
                        .collect();
                    let tok = tokens.iter().find(|tk| {
                        s <= tk.start
                            && tk.end <= e
                            && tk.text(text) == word
                            && !regions
                                .iter()
                                .any(|(rs, re)| *rs <= tk.start && tk.end <= *re)
                    })?;
                    ((tk_range(tok)), t.clone())
                }
                HookEdit::RemoveOp => (
                    remove_op_range(text, self.op_span.get(&op)?.0),
                    String::new(),
                ),
            });
        }
        Some(XFix {
            title: fix.title.clone(),
            edits,
        })
    }

    /// The range of what a hook targeted, relative to op `op`.
    pub fn target_range(&self, op: u32, target: HookTarget) -> Option<Range> {
        let (whole, name) = *self.op_span.get(&op)?;
        let info = self.model.ops.get(op as usize)?;
        match target {
            HookTarget::OpName => Some(name),
            HookTarget::Op => Some(whole),
            HookTarget::Result { index } => self
                .value_def
                .get(info.results.get(index as usize)?)
                .copied(),
            HookTarget::Operand { index } => {
                let v = info.operands.get(index as usize)?;
                // The same value may be used several times (`add c, c`):
                // take its n-th use in this op (not in an op nested in it).
                let nth = info.operands[..index as usize]
                    .iter()
                    .filter(|o| *o == v)
                    .count();
                let mut uses: Vec<Range> = self
                    .value_uses
                    .get(v)?
                    .iter()
                    .filter(|r| whole.0 <= r.0 && r.1 <= whole.1 && self.op_at(r.0) == Some(op))
                    .copied()
                    .collect();
                uses.sort();
                uses.get(nth).copied()
            }
        }
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

    /// The op whose isolation scope holds names defined in the regions of
    /// `op` (pliron's SSA name scopes are per `IsolatedFromAbove` op).
    fn isolation_root(&self, mut op: u32) -> u32 {
        let m = &self.model;
        loop {
            let o = &m.ops[op as usize];
            if o.traits & pliron_lsp_protocol::op_traits::ISOLATED_FROM_ABOVE != 0 {
                return op;
            }
            match o.parent_block {
                Some(b) => op = m.regions[m.blocks[b as usize].region as usize].parent_op,
                None => return op,
            }
        }
    }

    /// The name scope of a value (`None` for top-level results).
    fn value_scope(&self, v: u32) -> Option<u32> {
        let m = &self.model;
        match m.values.get(v as usize)?.def {
            ValueDef::Result { op, .. } => {
                let b = m.ops[op as usize].parent_block?;
                Some(self.isolation_root(m.regions[m.blocks[b as usize].region as usize].parent_op))
            }
            ValueDef::Arg { block, .. } => Some(
                self.isolation_root(m.regions[m.blocks[block as usize].region as usize].parent_op),
            ),
            ValueDef::Detached { .. } => None,
        }
    }

    /// Would renaming value `v` to `name` clash with another value of its
    /// scope? Returns the clashing value's definition.
    pub fn value_conflict(&self, v: u32, name: &str) -> Option<Range> {
        let scope = self.value_scope(v)?;
        (0..self.model.values.len() as u32)
            .filter(|w| *w != v)
            .filter(|w| self.model.values[*w as usize].given_name.as_deref() == Some(name))
            .find(|w| self.value_scope(*w) == Some(scope))
            .and_then(|w| self.value_def.get(&w).copied())
    }

    /// Would renaming block `b` to `name` clash with a block of its region?
    pub fn block_conflict(&self, b: u32, name: &str) -> Option<Range> {
        let m = &self.model;
        let region = m.blocks.get(b as usize)?.region;
        m.regions[region as usize]
            .blocks
            .iter()
            .find(|o| **o != b && m.blocks[**o as usize].label.as_deref() == Some(name))
            .and_then(|o| self.block_def.get(o).copied())
    }

    pub fn is_unresolved(&self, value: u32) -> bool {
        matches!(
            self.model.values.get(value as usize).map(|v| v.def),
            Some(ValueDef::Detached { unresolved: true })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pliron_lsp_protocol::{EngineDiag, OpInfo, Span};

    #[test]
    fn removing_an_op_keeps_the_separators_right() {
        let remove = |text: &str, op: &str| {
            let s = text.find(op).unwrap();
            let (a, b) = remove_op_range(text, (s as Offset, (s + op.len()) as Offset));
            format!("{}{}", &text[..a as usize], &text[b as usize..])
        };
        let block = "  ^e():\n    a = t.x;\n    b = t.y;\n    t.r\n  }\n";
        // A middle op: its line.
        assert_eq!(
            remove(block, "b = t.y"),
            "  ^e():\n    a = t.x;\n    t.r\n  }\n"
        );
        // The last op: the `;` before it.
        assert_eq!(
            remove(block, "t.r"),
            "  ^e():\n    a = t.x;\n    b = t.y\n  }\n"
        );
        // The only op: its line.
        assert_eq!(remove("  ^e():\n    t.r\n  }\n", "t.r"), "  ^e():\n  }\n");
        // Ops on one line.
        assert_eq!(remove("{ a = t.x; t.r }", "a = t.x"), "{  t.r }");
    }

    #[test]
    fn round_trip_problems_are_warnings_at_the_op_name() {
        let text = "builtin.module @m {\n  ^e():\n  x = t.op 1\n}\n";
        let li = LineIndex::new(text);
        let pos = |line, column| Pos { line, column };
        let res = AnalyzeResult {
            text_hash: 0,
            parse_errors: Vec::new(),
            verify_errors: Vec::new(),
            model: Some(Model {
                ops: vec![OpInfo::default(), OpInfo::default()],
                ..Model::default()
            }),
            spans: vec![Span {
                start: pos(3, 3),
                end: pos(3, 13),
                kind: SpanKind::Op {
                    op: 1,
                    name_start: pos(3, 7),
                    name_end: pos(3, 11),
                },
            }],
            hook_diags: Vec::new(),
            hook_hints: Vec::new(),
            round_trip: vec![EngineDiag {
                phase: DiagPhase::RoundTrip,
                pos: Some(pos(3, 3)),
                message: "printing and parsing this operation again changes it".into(),
                op: None,
            }],
            printed: Some("printed".into()),
            elapsed_us: 0,
        };
        let x = Exact::new(res, text, &li);
        let d = &x.diagnostics[0];
        assert_eq!(&text[d.range.0 as usize..d.range.1 as usize], "t.op");
        assert_eq!(
            (d.phase, d.severity),
            (DiagPhase::RoundTrip, HookSeverity::Warning)
        );
        assert_eq!(x.printed.as_deref(), Some("printed"));
    }
}

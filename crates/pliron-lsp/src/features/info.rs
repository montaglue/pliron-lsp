//! Hover and inlay hints.

use lsp_types::{
    Hover, HoverContents, InlayHint, InlayHintKind, InlayHintLabel, MarkupContent, MarkupKind,
};
use pliron_ir_syntax::{DefKind, Encoding, Offset, Provenance, Role};
use pliron_lsp_protocol::{SpanKind, ValueDef};

use crate::document::Document;
use crate::exact::Exact;

fn code(s: &str) -> String {
    format!("```pliron\n{s}\n```")
}

fn markdown(range: Option<lsp_types::Range>, value: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range,
    }
}

fn value_origin(x: &Exact, v: u32) -> String {
    let m = &x.model;
    match m.values.get(v as usize).map(|v| v.def) {
        Some(ValueDef::Result { op, index }) => {
            format!("result #{index} of `{}`", m.ops[op as usize].opid)
        }
        Some(ValueDef::Arg { block, index }) => {
            let label = m.blocks[block as usize]
                .label
                .as_deref()
                .unwrap_or("<block>");
            format!("argument #{index} of block `^{label}`")
        }
        Some(ValueDef::Detached { unresolved: true }) => "**undefined value**".into(),
        _ => "value".into(),
    }
}

fn op_hover(x: &Exact, op: u32) -> String {
    let m = &x.model;
    let o = &m.ops[op as usize];
    let mut s = String::new();
    let results: Vec<String> = o
        .results
        .iter()
        .map(|v| m.values[*v as usize].ty.clone())
        .collect();
    let operands: Vec<String> = o
        .operands
        .iter()
        .map(|v| m.values[*v as usize].ty.clone())
        .collect();
    let mut sig = o.opid.clone();
    if let Some(sym) = &o.symbol {
        sig.push_str(&format!(" @{sym}"));
    }
    s.push_str(&code(&sig));
    s.push_str(&format!(
        "\n\n`({})` → `({})`",
        operands.join(", "),
        results.join(", ")
    ));
    let attrs: Vec<_> = o
        .attrs
        .iter()
        .filter(|a| a.key != "builtin_given_names")
        .collect();
    if !attrs.is_empty() {
        s.push_str("\n\n**Attributes**\n");
        for a in attrs {
            s.push_str(&format!("- `{}`: `{}`\n", a.key, a.text));
        }
    }
    let mut traits = Vec::new();
    use pliron_lsp_protocol::op_traits::*;
    for (bit, name) in [
        (ISOLATED_FROM_ABOVE, "IsolatedFromAbove"),
        (SYMBOL, "Symbol"),
        (SYMBOL_TABLE, "SymbolTable"),
        (TERMINATOR, "Terminator"),
    ] {
        if o.traits & bit != 0 {
            traits.push(name);
        }
    }
    if !traits.is_empty() {
        s.push_str(&format!("\n\n*{}*", traits.join(" · ")));
    }
    // From the dialect's hover hooks.
    for note in &o.notes {
        s.push_str("\n\n");
        s.push_str(note);
    }
    s
}

pub fn hover(
    doc: &Document,
    off: Offset,
    enc: Encoding,
    index: Option<&crate::index::DialectIndex>,
) -> Option<Hover> {
    // Op / type / attribute names: what the IR says plus the Rust docs.
    if let Some((name, kind, range)) = super::dialect_name_at(doc, off) {
        let mut parts = Vec::new();
        if let Some(x) = doc.fresh_exact()
            && let Some(op) = x.op_name_at(off)
        {
            parts.push(op_hover(x, op));
        }
        if let Some(e) = index.and_then(|i| i.lookup(&name, kind)) {
            if parts.is_empty() {
                parts.push(code(&name));
            }
            parts.push(super::entry_docs(e));
        }
        if !parts.is_empty() {
            return Some(markdown(
                Some(doc.range(range, enc)),
                parts.join("\n\n---\n\n"),
            ));
        }
    }
    if let Some(x) = doc.fresh_exact() {
        if let Some(sp) = x.leaf_at(off) {
            let range = Some(doc.range((sp.start, sp.end), enc));
            match &sp.kind {
                SpanKind::ResultDef { value }
                | SpanKind::BlockArgDef { value }
                | SpanKind::OperandUse { value } => {
                    let v = &x.model.values[*value as usize];
                    let name = doc.slice((sp.start, sp.end));
                    return Some(markdown(
                        range,
                        format!(
                            "{}\n\n{}",
                            code(&format!("{name}: {}", v.ty)),
                            value_origin(x, *value)
                        ),
                    ));
                }
                SpanKind::BlockLabel { block } | SpanKind::SuccessorUse { block } => {
                    let b = &x.model.blocks[*block as usize];
                    let args: Vec<String> = b
                        .args
                        .iter()
                        .map(|v| x.model.values[*v as usize].ty.clone())
                        .collect();
                    return Some(markdown(
                        range,
                        code(&format!(
                            "^{}({})",
                            b.label.as_deref().unwrap_or(""),
                            args.join(", ")
                        )),
                    ));
                }
                SpanKind::Keyword => {
                    let op = x.op_at(off).map(|o| x.model.ops[o as usize].opid.clone());
                    return Some(markdown(
                        range,
                        match op {
                            Some(op) => format!("keyword of `{op}`"),
                            None => "keyword".into(),
                        },
                    ));
                }
                _ => {}
            }
        }
        // Types: the innermost type span.
        if let Some(sp) = x
            .spans_at(off)
            .filter(|s| matches!(s.kind, SpanKind::Type { .. }))
            .min_by_key(|s| s.end - s.start)
            && let SpanKind::Type { text, .. } = &sp.kind
        {
            return Some(markdown(
                Some(doc.range((sp.start, sp.end), enc)),
                format!("{}\n\ntype", code(text)),
            ));
        }
        if let Some(name) = x.symbol_at(off) {
            if let Some(d) = x.symbol_def_by_name(&name) {
                return Some(markdown(None, op_hover(x, d.op)));
            }
            return Some(markdown(
                None,
                format!("`@{name}`: symbol not defined in this document"),
            ));
        }
        if let Some(op) = x.op_at(off) {
            let _ = op;
        }
    }
    syntax_hover(doc, off, enc)
}

fn syntax_hover(doc: &Document, off: Offset, enc: Encoding) -> Option<Hover> {
    let a = &doc.syntax;
    let tok = a.tree.token_at(off)?;
    let t = a.tree.tok(tok);
    let range = Some(doc.range((t.start, t.end), enc));
    let note = doc
        .engine_note
        .as_deref()
        .map(|n| format!("\n\n---\n*{n}*"))
        .unwrap_or_default();
    match a.role(tok) {
        Role::Def(d) | Role::Use(d, _) => {
            let def = a.def(d);
            let what = match def.kind {
                DefKind::Result => "result",
                DefKind::BlockArg => "block argument",
                DefKind::Label => "block",
                DefKind::Symbol => "symbol",
                DefKind::Outline => "outlined attribute entry",
            };
            let ty = def.ty.map(|ty| {
                let txt = &doc.text[ty.start as usize..ty.end as usize];
                match ty.provenance {
                    Provenance::Exact => format!(": {txt}"),
                    Provenance::Heuristic => format!(": {txt}  (guessed)"),
                }
            });
            Some(markdown(
                range,
                format!(
                    "{}\n\n{what}{note}",
                    code(&format!("{}{}", t.text(&doc.text), ty.unwrap_or_default()))
                ),
            ))
        }
        Role::OpName => Some(markdown(
            range,
            format!("{}\n\noperation{note}", code(t.text(&doc.text))),
        )),
        _ => None,
    }
}

/// Types the parsers recorded at the top level of an op (not nested in
/// another type or attribute, and not inside the op's regions).
fn top_level_types(x: &Exact, op: u32) -> Vec<&str> {
    let Some(((s, e), _)) = x.op_span.get(&op).copied() else {
        return Vec::new();
    };
    let regions: Vec<(Offset, Offset)> = x
        .region_spans
        .iter()
        .map(|(r, _)| *r)
        .filter(|(rs, re)| s <= *rs && *re <= e)
        .collect();
    let inside_region = |a: Offset, b: Offset| regions.iter().any(|(rs, re)| *rs <= a && b <= *re);
    let containers: Vec<(Offset, Offset)> = x
        .spans
        .iter()
        .filter(|sp| matches!(sp.kind, SpanKind::Type { .. } | SpanKind::Attr { .. }))
        .filter(|sp| s <= sp.start && sp.end <= e && !inside_region(sp.start, sp.end))
        .map(|sp| (sp.start, sp.end))
        .collect();
    x.spans
        .iter()
        .filter_map(|sp| match &sp.kind {
            SpanKind::Type { text, .. }
                if s <= sp.start
                    && sp.end <= e
                    && !inside_region(sp.start, sp.end)
                    && !containers.iter().any(|(cs, ce)| {
                        *cs <= sp.start && sp.end <= *ce && (*cs, *ce) != (sp.start, sp.end)
                    }) =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Inlay hints: result types that the op's syntax does not spell out, and
/// hints from the dialect's inlay hooks.
pub fn inlay_hints(doc: &Document, range: (Offset, Offset), enc: Encoding) -> Vec<InlayHint> {
    let Some(x) = doc.fresh_exact() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sp in &x.spans {
        let SpanKind::ResultDef { value } = sp.kind else {
            continue;
        };
        if sp.end < range.0 || sp.start > range.1 {
            continue;
        }
        let v = &x.model.values[value as usize];
        if let ValueDef::Result { op, .. } = v.def
            && top_level_types(x, op).contains(&v.ty.as_str())
        {
            continue;
        }
        out.push(InlayHint {
            position: doc.position(sp.end, enc),
            label: InlayHintLabel::String(format!(": {}", v.ty)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: Some(true),
            data: None,
        });
    }
    for h in &x.hints {
        if h.offset < range.0 || h.offset > range.1 {
            continue;
        }
        out.push(InlayHint {
            position: doc.position(h.offset, enc),
            label: InlayHintLabel::String(h.label.clone()),
            kind: h.before.then_some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: None,
            padding_left: Some(!h.before),
            padding_right: Some(h.before),
            data: None,
        });
    }
    out.sort_by_key(|h| (h.position.line, h.position.character));
    out
}

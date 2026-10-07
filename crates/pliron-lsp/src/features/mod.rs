//! LSP features.

pub mod entity;
pub mod info;
pub mod semantic_tokens;
pub mod structure;
pub mod views;

use std::collections::BTreeSet;

use lsp_types::{
    CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity, Documentation,
    MarkupContent, MarkupKind, NumberOrString,
};
use pliron_ir_syntax::{DefKind, Encoding, Offset, Role, Severity, TokenKind};
use pliron_lsp_protocol::{DiagPhase, SpanKind};

use crate::document::Document;
use crate::index::{DialectIndex, Entry, EntryKind};

/// The op / type / attribute name under the cursor: (name, kind, range).
pub fn dialect_name_at(doc: &Document, off: Offset) -> Option<(String, Option<EntryKind>, (Offset, Offset))> {
    if let Some(x) = doc.fresh_exact() {
        if let Some(op) = x.op_name_at(off) {
            let (_, name) = x.op_span[&op];
            return Some((x.model.ops[op as usize].opid.clone(), Some(EntryKind::Op), name));
        }
        for s in x.spans_at(off) {
            let (name_end, kind) = match s.kind {
                SpanKind::Type { name_end, .. } => (name_end, EntryKind::Type),
                SpanKind::Attr { name_end } => (name_end, EntryKind::Attr),
                _ => continue,
            };
            let ne = doc.line_index.offset_of_pliron(name_end.line, name_end.column, &doc.text);
            if s.start <= off && off <= ne {
                return Some((doc.slice((s.start, ne)).to_string(), Some(kind), (s.start, ne)));
            }
        }
    }
    let a = &doc.syntax;
    let tok = a.tree.token_at(off)?;
    let t = a.tree.tok(tok);
    if t.kind != TokenKind::QualName {
        return None;
    }
    let kind = match a.role(tok) {
        Role::OpName => Some(EntryKind::Op),
        _ => None,
    };
    Some((t.text(&doc.text).to_string(), kind, (t.start, t.end)))
}

/// Markdown documentation of an index entry.
pub fn entry_docs(e: &Entry) -> String {
    let mut s = String::new();
    if !e.docs.is_empty() {
        s.push_str(&e.docs);
        s.push_str("\n\n");
    }
    if let Some(f) = &e.format {
        s.push_str(&format!("**Format:** `{f}`\n\n"));
    }
    if let Some(i) = &e.interfaces {
        s.push_str(&format!("**Interfaces:** `{i}`\n\n"));
    }
    let uri = lsp_types::Url::from_file_path(&e.file)
        .map(|u| format!("{u}#L{}", e.line + 1))
        .unwrap_or_default();
    let file = e.file.file_name().and_then(|f| f.to_str()).unwrap_or("source");
    s.push_str(&format!(
        "*{} `{}` — defined by Rust type [`{}`]({uri}) in `{file}:{}`*",
        e.kind.describe(),
        e.name,
        e.rust_name,
        e.line + 1
    ));
    s
}

/// Diagnostics for a document. Returns `None` when nothing should be
/// published yet (an engine analysis of the current text is pending).
pub fn diagnostics(doc: &Document, enc: Encoding, engine_expected: bool) -> Option<Vec<Diagnostic>> {
    if let Some(x) = doc.fresh_exact() {
        return Some(
            x.diagnostics
                .iter()
                .map(|d| Diagnostic {
                    range: doc.range(d.range, enc),
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: Some(NumberOrString::String(
                        match d.phase {
                            DiagPhase::Parse => "parse",
                            DiagPhase::Verify => "verify",
                            DiagPhase::Panic => "panic",
                        }
                        .into(),
                    )),
                    source: Some("pliron".into()),
                    message: d.message.clone(),
                    ..Diagnostic::default()
                })
                .collect(),
        );
    }
    if engine_expected && doc.engine_note.is_none() {
        return None;
    }
    let mut out: Vec<Diagnostic> = doc
        .syntax
        .diagnostics
        .iter()
        .map(|d| Diagnostic {
            range: doc.range((d.start, d.end), enc),
            severity: Some(match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
                Severity::Info => DiagnosticSeverity::INFORMATION,
            }),
            source: Some("pliron-syntax".into()),
            message: d.message.clone(),
            ..Diagnostic::default()
        })
        .collect();
    if let Some(note) = &doc.engine_note {
        out.push(Diagnostic {
            range: doc.range((0, 0), enc),
            severity: Some(DiagnosticSeverity::INFORMATION),
            source: Some("pliron-lsp".into()),
            message: note.clone(),
            ..Diagnostic::default()
        });
    }
    Some(out)
}

/// Completion at `off`.
pub fn completion(
    doc: &Document,
    off: Offset,
    known_ops: &BTreeSet<String>,
    index: Option<&DialectIndex>,
) -> Vec<CompletionItem> {
    let a = &doc.syntax;
    let before = &doc.text[..off as usize];
    let word_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || matches!(c, '_' | '.' | '^' | '@'))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(before.len());
    let word = &before[word_start..];
    let mut items = Vec::new();

    let innermost_region = a
        .tree
        .region_ids()
        .filter(|r| {
            let (s, e) = a.tree.region_range(*r);
            s < off && off <= e
        })
        .min_by_key(|r| {
            let (s, e) = a.tree.region_range(*r);
            e - s
        });

    if word.starts_with('^') {
        if let Some(r) = innermost_region {
            for d in a.labels_of_region(r) {
                let def = a.def(d);
                items.push(CompletionItem {
                    label: format!("^{}", def.name),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some("block".into()),
                    ..CompletionItem::default()
                });
            }
        }
        return items;
    }
    if word.starts_with('@') {
        let mut names = BTreeSet::new();
        for d in a.defs_of_kind(DefKind::Symbol) {
            names.insert(a.def(d).name.clone());
        }
        if let Some(x) = doc.exact.as_deref() {
            for s in &x.symbols {
                names.insert(s.name.clone());
            }
        }
        for n in names {
            items.push(CompletionItem {
                label: format!("@{n}"),
                kind: Some(CompletionItemKind::FUNCTION),
                ..CompletionItem::default()
            });
        }
        return items;
    }

    // Values in scope.
    if let Some(r) = innermost_region {
        let scope = a.region_scope[r.0 as usize];
        for (i, d) in a.defs.iter().enumerate() {
            let s = match d.kind {
                DefKind::Result => d.stmt.map(|s| a.stmt_scope[s.0 as usize]),
                DefKind::BlockArg => d.block.map(|b| a.region_scope[a.tree.block(b).region.0 as usize]),
                _ => None,
            };
            if s != Some(scope) {
                continue;
            }
            let tok = a.tree.tok(d.tok);
            if tok.start <= off && off <= tok.end {
                continue;
            }
            let ty = doc
                .fresh_exact()
                .and_then(|x| x.value_at(tok.start).map(|v| x.model.values[v as usize].ty.clone()))
                .or_else(|| d.ty.map(|t| doc.text[t.start as usize..t.end as usize].to_string()));
            let _ = i;
            items.push(CompletionItem {
                label: d.name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: ty,
                ..CompletionItem::default()
            });
        }
    }

    // Op names: from the dialect sources (with docs), registered ones seen
    // by the engine, and those used in this document.
    let mut ops: BTreeSet<String> = known_ops.clone();
    for s in &a.tree.stmts {
        if let Some(t) = s.op_name {
            ops.insert(a.tree.tok(t).text(&doc.text).to_string());
        }
    }
    if let Some(index) = index {
        for e in index.ops() {
            ops.insert(e.name.clone());
        }
    }
    for op in ops {
        let entry = index.and_then(|i| i.lookup(&op, Some(EntryKind::Op)));
        items.push(CompletionItem {
            label: op,
            kind: Some(CompletionItemKind::FUNCTION),
            detail: entry.and_then(|e| e.format.clone()),
            documentation: entry.map(|e| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: entry_docs(e),
                })
            }),
            ..CompletionItem::default()
        });
    }
    items
}

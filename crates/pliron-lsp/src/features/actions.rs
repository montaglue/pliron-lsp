//! Code actions: quick fixes for diagnostics.

use std::collections::{BTreeSet, HashMap};

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, TextEdit, Url, WorkspaceEdit,
};
use pliron_ir_syntax::{DefKind, Encoding, ErrorKind, Offset};

use crate::document::Document;
use crate::index::{DialectIndex, EntryKind};

/// Levenshtein distance (by chars).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest candidates to `name` (best first).
pub fn suggestions<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let max = (name.chars().count() / 3).max(2);
    let mut scored: Vec<(usize, &str)> = candidates
        .into_iter()
        .filter(|c| *c != name)
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, _)| *d <= max)
        .collect();
    scored.sort();
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(3).map(|(_, c)| c.to_string()).collect()
}

/// `Unregistered <kind> <name>` in a pliron error message.
pub fn unregistered(message: &str) -> Option<(&str, &str)> {
    let rest = &message[message.find("Unregistered ")? + "Unregistered ".len()..];
    let (kind, rest) = rest.split_once(' ')?;
    let name = rest.split(|c: char| c.is_whitespace() || c == ',').next()?;
    (!name.is_empty()).then_some((kind, name))
}

/// `Value x is not defined…` / `Block label ^x is not defined…`.
fn undefined_name(message: &str) -> Option<(bool, &str)> {
    if let Some(rest) = message.strip_prefix("Value ") {
        return rest.split_whitespace().next().map(|n| (false, n));
    }
    if let Some(rest) = message.strip_prefix("Block label ^") {
        return rest.split_whitespace().next().map(|n| (true, n));
    }
    // Messages of the syntax layer.
    if let Some(rest) = message.strip_prefix("undefined value `") {
        return rest.split('`').next().map(|n| (false, n));
    }
    if let Some(rest) = message.strip_prefix("undefined block label `^") {
        return rest.split('`').next().map(|n| (true, n));
    }
    None
}

fn fix(uri: &Url, title: String, diag: &Diagnostic, edits: Vec<TextEdit>, preferred: bool) -> CodeActionOrCommand {
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        }),
        is_preferred: Some(preferred),
        ..CodeAction::default()
    })
}

/// Last non-whitespace offset before `off` (insertion point for a `;`).
fn end_of_previous(doc: &Document, off: Offset) -> Offset {
    let before = &doc.text[..off as usize];
    before.trim_end().len() as Offset
}

/// Quick fixes for the diagnostics in a code action request.
pub fn code_actions(
    doc: &Document,
    uri: &Url,
    diagnostics: &[Diagnostic],
    enc: Encoding,
    index: Option<&DialectIndex>,
    known_ops: &BTreeSet<String>,
) -> Vec<CodeActionOrCommand> {
    let mut out = Vec::new();
    for diag in diagnostics {
        let start = doc.offset(diag.range.start, enc);
        let end = doc.offset(diag.range.end, enc);
        let msg = diag.message.as_str();

        // Unknown op / type / attribute / dialect names.
        if let Some((kind, name)) = unregistered(msg) {
            let kind = match kind {
                "Op" => Some(EntryKind::Op),
                "type" => Some(EntryKind::Type),
                "attribute" => Some(EntryKind::Attr),
                _ => None, // dialect
            };
            let mut candidates: BTreeSet<String> = BTreeSet::new();
            if let Some(index) = index {
                for e in &index.entries {
                    match kind {
                        Some(k) if e.kind == k => {
                            candidates.insert(e.name.clone());
                        }
                        None => {
                            if let Some((d, _)) = e.name.split_once('.') {
                                candidates.insert(d.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            if kind == Some(EntryKind::Op) {
                candidates.extend(known_ops.iter().cloned());
            }
            // For a dialect, replace just the dialect prefix.
            let (range, base) = if kind.is_none() {
                let text = doc.slice((start, end));
                match text.find('.') {
                    Some(dot) if &text[..dot] == name => ((start, start + dot as Offset), name),
                    _ => ((start, end), name),
                }
            } else {
                ((start, end), name)
            };
            for (i, s) in suggestions(base, candidates.iter().map(String::as_str))
                .into_iter()
                .enumerate()
            {
                out.push(fix(
                    uri,
                    format!("Change to `{s}`"),
                    diag,
                    vec![TextEdit {
                        range: doc.range(range, enc),
                        new_text: s,
                    }],
                    i == 0,
                ));
            }
            continue;
        }

        // Undefined values / labels: suggest similar names in scope.
        if let Some((is_label, name)) = undefined_name(msg) {
            let a = &doc.syntax;
            let kinds: &[DefKind] = if is_label {
                &[DefKind::Label]
            } else {
                &[DefKind::Result, DefKind::BlockArg]
            };
            let names: BTreeSet<&str> = a
                .defs
                .iter()
                .filter(|d| kinds.contains(&d.kind))
                .map(|d| d.name.as_str())
                .collect();
            // The diagnostic may cover `^name`: keep the sigil.
            let (s, e) = if is_label && doc.slice((start, end)).starts_with('^') {
                (start + 1, end)
            } else {
                (start, end)
            };
            for (i, cand) in suggestions(name, names).into_iter().enumerate() {
                out.push(fix(
                    uri,
                    format!("Change to `{}{cand}`", if is_label { "^" } else { "" }),
                    diag,
                    vec![TextEdit {
                        range: doc.range((s, e), enc),
                        new_text: cand,
                    }],
                    i == 0,
                ));
            }
            continue;
        }

        // Separators.
        if msg.starts_with("expected `;` between operations") || msg.starts_with("missing `;`") {
            let at = end_of_previous(doc, start);
            out.push(fix(
                uri,
                "Insert `;`".into(),
                diag,
                vec![TextEdit {
                    range: doc.range((at, at), enc),
                    new_text: ";".into(),
                }],
                true,
            ));
            continue;
        }
        if msg.starts_with("expected an operation after `;`") || msg.contains("after the last operation") {
            let semi = if doc.slice((start, end)) == ";" {
                Some(start)
            } else {
                doc.text[..start as usize].rfind(';').map(|p| p as Offset)
            };
            if let Some(p) = semi {
                out.push(fix(
                    uri,
                    "Remove `;`".into(),
                    diag,
                    vec![TextEdit {
                        range: doc.range((p, p + 1), enc),
                        new_text: String::new(),
                    }],
                    true,
                ));
            }
            continue;
        }

        // Structural fixes from the syntax layer.
        for d in &doc.syntax.diagnostics {
            if d.start != start || d.end != end || d.message != msg {
                continue;
            }
            match &d.kind {
                ErrorKind::Unclosed { closer, insert_at } => out.push(fix(
                    uri,
                    format!("Insert `{closer}`"),
                    diag,
                    vec![TextEdit {
                        range: doc.range((*insert_at, *insert_at), enc),
                        new_text: closer.to_string(),
                    }],
                    true,
                )),
                ErrorKind::MissingSemicolon { insert_at } => out.push(fix(
                    uri,
                    "Insert `;`".into(),
                    diag,
                    vec![TextEdit {
                        range: doc.range((*insert_at, *insert_at), enc),
                        new_text: ";".into(),
                    }],
                    true,
                )),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_and_suggestions() {
        assert_eq!(edit_distance("llvm.ad", "llvm.add"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        let s = suggestions("llvm.ad", ["llvm.add", "llvm.and", "llvm.sub", "builtin.module"]);
        assert_eq!(s, ["llvm.add", "llvm.and"]);
        assert!(suggestions("zzzzzz", ["llvm.add"]).is_empty());
    }

    #[test]
    fn parse_messages() {
        assert_eq!(unregistered("Unregistered Op llvm.ad"), Some(("Op", "llvm.ad")));
        assert_eq!(
            unregistered("blah\nUnregistered type foo.bar\nmore"),
            Some(("type", "foo.bar"))
        );
        assert_eq!(undefined_name("Value w2 is not defined in this scope"), Some((false, "w2")));
        assert_eq!(
            undefined_name("Block label ^bb9 is not defined in this region"),
            Some((true, "bb9"))
        );
    }
}

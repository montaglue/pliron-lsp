//! Symbols across files: the IR files of the workspace, workspace symbol
//! search, references / rename / reference counts and call hierarchy for
//! `@symbol` ops.

use std::path::{Path, PathBuf};

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CodeLens, Command,
    Location, SymbolInformation, SymbolKind, Url,
};
use pliron_ir_syntax::{DefKind, Encoding, Offset, Role, TokenKind};
use pliron_lsp_protocol::op_traits;

use crate::document::Document;

pub type Range = (Offset, Offset);

/// A symbol defined by an op (`@name`).
#[derive(Clone, Debug)]
pub struct SymDef {
    pub name: String,
    /// The `@name` token.
    pub range: Range,
    /// The whole defining op.
    pub op_range: Range,
    /// The defining op's name.
    pub detail: String,
    pub kind: SymbolKind,
}

/// A `@name` reference.
#[derive(Clone, Debug)]
pub struct SymUse {
    pub name: String,
    pub range: Range,
    /// Index (into `defs`) of the innermost symbol op containing the use.
    pub enclosing: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct FileSymbols {
    pub defs: Vec<SymDef>,
    pub uses: Vec<SymUse>,
}

/// Symbol definitions and references of a document (exact when an engine
/// analysis of the current text exists, syntactic otherwise).
pub fn file_symbols(doc: &Document) -> FileSymbols {
    let mut out = FileSymbols::default();
    if let Some(x) = doc.fresh_exact() {
        for d in &x.symbols {
            let info = &x.model.ops[d.op as usize];
            let op_range = x.op_span.get(&d.op).map(|(r, _)| *r).unwrap_or(d.range);
            out.defs.push(SymDef {
                name: d.name.clone(),
                range: d.range,
                op_range,
                detail: info.opid.clone(),
                kind: if info.traits & op_traits::SYMBOL_TABLE != 0 {
                    SymbolKind::MODULE
                } else if !info.regions.is_empty() {
                    SymbolKind::FUNCTION
                } else {
                    SymbolKind::VARIABLE
                },
            });
        }
        for (r, name) in &x.symbol_uses {
            out.uses.push(SymUse {
                name: name.clone(),
                range: *r,
                enclosing: None,
            });
        }
    } else {
        let a = &doc.syntax;
        for d in a.defs_of_kind(DefKind::Symbol) {
            let def = a.def(d);
            let Some(stmt) = def.stmt else { continue };
            let t = a.tree.tok(def.tok);
            let s = a.tree.stmt(stmt);
            out.defs.push(SymDef {
                name: def.name.clone(),
                range: (t.start, t.end),
                op_range: a.tree.stmt_range(stmt),
                detail: s
                    .op_name
                    .map(|o| a.tree.tok(o).text(&doc.text).to_string())
                    .unwrap_or_default(),
                kind: if s.parent_block.is_none() {
                    SymbolKind::MODULE
                } else if !s.regions.is_empty() {
                    SymbolKind::FUNCTION
                } else {
                    SymbolKind::VARIABLE
                },
            });
        }
        for (i, t) in a.tree.tokens.iter().enumerate() {
            if t.kind == TokenKind::SymbolRef && !matches!(a.role(i as u32), Role::Def(_)) {
                out.uses.push(SymUse {
                    name: t.name(&doc.text).to_string(),
                    range: (t.start, t.end),
                    enclosing: None,
                });
            }
        }
    }
    // Innermost enclosing definition of every use.
    for u in &mut out.uses {
        u.enclosing = out
            .defs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.op_range.0 <= u.range.0 && u.range.1 <= d.op_range.1)
            .min_by_key(|(_, d)| d.op_range.1 - d.op_range.0)
            .map(|(i, _)| i);
    }
    out
}

/// All `.pliron` / `.plir` files under `roots`.
pub fn find_ir_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                if depth < 12 && !name.starts_with('.') && !matches!(&*name, "target" | "node_modules") {
                    walk(&p, out, depth + 1);
                }
            } else if p.extension().is_some_and(|x| x == "pliron" || x == "plir") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for r in roots {
        walk(r, &mut out, 0);
    }
    out.sort();
    out.dedup();
    out
}

/// A file of the workspace with its symbols.
pub struct IndexedFile<'a> {
    pub uri: &'a Url,
    pub doc: &'a Document,
    pub symbols: FileSymbols,
}

fn loc(f: &IndexedFile, r: Range, enc: Encoding) -> Location {
    Location {
        uri: f.uri.clone(),
        range: f.doc.range(r, enc),
    }
}

/// Is `query` a case-insensitive subsequence of `name`?
fn fuzzy(name: &str, query: &str) -> bool {
    let mut it = name.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|q| it.any(|c| c == q))
}

#[allow(deprecated)]
pub fn workspace_symbols(files: &[IndexedFile], query: &str, enc: Encoding) -> Vec<SymbolInformation> {
    let mut out = Vec::new();
    for f in files {
        for d in &f.symbols.defs {
            if fuzzy(&d.name, query) {
                out.push(SymbolInformation {
                    name: format!("@{}", d.name),
                    kind: d.kind,
                    tags: None,
                    deprecated: None,
                    location: loc(f, d.range, enc),
                    container_name: Some(d.detail.clone()),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.len().cmp(&b.name.len()).then(a.name.cmp(&b.name)));
    out.truncate(512);
    out
}

/// Definitions of `@name` (current file first).
pub fn definitions<'a>(files: &'a [IndexedFile], name: &str, current: &Url) -> Vec<(&'a IndexedFile<'a>, &'a SymDef)> {
    let mut out: Vec<_> = files
        .iter()
        .flat_map(|f| f.symbols.defs.iter().filter(|d| d.name == name).map(move |d| (f, d)))
        .collect();
    out.sort_by_key(|(f, _)| f.uri != current);
    out
}

/// An occurrence of a symbol in a workspace file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// Index into the files.
    pub file: usize,
    /// The `@name` token.
    pub range: Range,
    pub is_def: bool,
}

fn file_name(f: &IndexedFile) -> String {
    f.uri
        .path_segments()
        .and_then(|mut s| s.next_back().map(str::to_string))
        .unwrap_or_else(|| f.uri.to_string())
}

/// `@name` as seen from `current`: its definition and the references that
/// resolve to it, across the workspace. The definition is the one in
/// `current` or, failing that, in the only file that defines the name;
/// other files that define their own `@name` are left alone. Without a
/// definition anywhere, all references count (an external symbol).
pub fn symbol_occurrences(files: &[IndexedFile], name: &str, current: &Url) -> Result<Vec<Occurrence>, String> {
    let definers: Vec<usize> = (0..files.len())
        .filter(|i| files[*i].symbols.defs.iter().any(|d| d.name == name))
        .collect();
    let home = match files.iter().position(|f| f.uri == current) {
        Some(c) if definers.contains(&c) => Some(c),
        _ => match definers.as_slice() {
            [] => None,
            [one] => Some(*one),
            many => {
                let names: Vec<String> = many.iter().map(|i| file_name(&files[*i])).collect();
                return Err(format!(
                    "`@{name}` is defined in {} files ({}); do this from the file whose definition you mean",
                    many.len(),
                    names.join(", ")
                ));
            }
        },
    };
    let mut out = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if definers.contains(&i) && Some(i) != home {
            continue;
        }
        if Some(i) == home {
            out.extend(f.symbols.defs.iter().filter(|d| d.name == name).map(|d| Occurrence {
                file: i,
                range: d.range,
                is_def: true,
            }));
        }
        out.extend(f.symbols.uses.iter().filter(|u| u.name == name).map(|u| Occurrence {
            file: i,
            range: u.range,
            is_def: false,
        }));
    }
    Ok(out)
}

/// Why `occurrences` cannot be renamed to `@new`: a file they are in
/// already defines it.
pub fn rename_conflict(files: &[IndexedFile], occurrences: &[Occurrence], new: &str) -> Option<String> {
    let mut touched: Vec<usize> = occurrences.iter().map(|o| o.file).collect();
    touched.dedup();
    touched
        .into_iter()
        .find(|i| files[*i].symbols.defs.iter().any(|d| d.name == new))
        .map(|i| format!("`@{new}` is already defined in {}", file_name(&files[i])))
}

/// "N references" above every symbol definition of `uri` (except symbol
/// tables such as modules). The command is `pliron.showReferences` with
/// the arguments of VS Code's `editor.action.showReferences`.
pub fn reference_lenses(files: &[IndexedFile], uri: &Url, enc: Encoding) -> Vec<CodeLens> {
    let Some(f) = files.iter().find(|f| f.uri == uri) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for d in &f.symbols.defs {
        if d.kind == SymbolKind::MODULE {
            continue;
        }
        let Ok(occurrences) = symbol_occurrences(files, &d.name, uri) else {
            continue;
        };
        let refs: Vec<Location> = occurrences
            .iter()
            .filter(|o| !o.is_def)
            .map(|o| loc(&files[o.file], o.range, enc))
            .collect();
        let range = f.doc.range(d.range, enc);
        let n = refs.len();
        out.push(CodeLens {
            range,
            command: Some(Command {
                title: format!("{n} reference{}", if n == 1 { "" } else { "s" }),
                command: "pliron.showReferences".into(),
                arguments: Some(vec![
                    serde_json::json!(uri),
                    serde_json::json!(range.start),
                    serde_json::json!(refs),
                ]),
            }),
            data: None,
        });
    }
    out
}

pub fn item(f: &IndexedFile, d: &SymDef, enc: Encoding) -> CallHierarchyItem {
    CallHierarchyItem {
        name: format!("@{}", d.name),
        kind: d.kind,
        tags: None,
        detail: Some(d.detail.clone()),
        uri: f.uri.clone(),
        range: f.doc.range(d.op_range, enc),
        selection_range: f.doc.range(d.range, enc),
        data: None,
    }
}

/// Who references `@name`, grouped by the symbol op containing the
/// reference.
pub fn incoming_calls(files: &[IndexedFile], name: &str, enc: Encoding) -> Vec<CallHierarchyIncomingCall> {
    let mut out: Vec<CallHierarchyIncomingCall> = Vec::new();
    for f in files {
        for u in f.symbols.uses.iter().filter(|u| u.name == name) {
            let Some(e) = u.enclosing else { continue };
            let from = item(f, &f.symbols.defs[e], enc);
            let range = f.doc.range(u.range, enc);
            match out
                .iter_mut()
                .find(|c| c.from.uri == from.uri && c.from.selection_range == from.selection_range)
            {
                Some(c) => c.from_ranges.push(range),
                None => out.push(CallHierarchyIncomingCall {
                    from,
                    from_ranges: vec![range],
                }),
            }
        }
    }
    out
}

/// Which symbols the op at `item` references (outside nested symbol ops).
pub fn outgoing_calls(
    files: &[IndexedFile],
    uri: &Url,
    selection_start: lsp_types::Position,
    enc: Encoding,
) -> Vec<CallHierarchyOutgoingCall> {
    let Some(f) = files.iter().find(|f| f.uri == uri) else {
        return Vec::new();
    };
    let Some(di) = f
        .symbols
        .defs
        .iter()
        .position(|d| f.doc.range(d.range, enc).start == selection_start)
    else {
        return Vec::new();
    };
    let mut out: Vec<CallHierarchyOutgoingCall> = Vec::new();
    for u in f.symbols.uses.iter().filter(|u| u.enclosing == Some(di)) {
        let range = f.doc.range(u.range, enc);
        if let Some(c) = out.iter_mut().find(|c| c.to.name == format!("@{}", u.name)) {
            c.from_ranges.push(range);
            continue;
        }
        let to = match definitions(files, &u.name, uri).first() {
            Some((tf, td)) => item(tf, td, enc),
            // An external symbol: point at the reference itself.
            None => CallHierarchyItem {
                name: format!("@{}", u.name),
                kind: SymbolKind::FUNCTION,
                tags: None,
                detail: Some("external".into()),
                uri: uri.clone(),
                range,
                selection_range: range,
                data: None,
            },
        };
        out.push(CallHierarchyOutgoingCall {
            to,
            from_ranges: vec![range],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match() {
        assert!(fuzzy("sum_plus_seven", "sps"));
        assert!(fuzzy("Main", "ma"));
        assert!(!fuzzy("main", "x"));
    }

    #[test]
    fn calls_in_one_file() {
        let text = "builtin.module @m {\n^e():\n  t.func @a {\n  ^b():\n    t.call @b;\n    t.call @b;\n    t.ret\n  };\n  t.func @b {\n  ^b():\n    t.call @a\n  }\n}\n";
        let doc = Document::new(text.into(), 0, &Default::default());
        let uri = Url::parse("file:///x.pliron").unwrap();
        let files = vec![IndexedFile {
            uri: &uri,
            doc: &doc,
            symbols: file_symbols(&doc),
        }];
        let inc = incoming_calls(&files, "b", Encoding::Utf16);
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].from.name, "@a");
        assert_eq!(inc[0].from_ranges.len(), 2);
        let a = files[0].symbols.defs.iter().find(|d| d.name == "a").unwrap();
        let out = outgoing_calls(&files, &uri, doc.range(a.range, Encoding::Utf16).start, Encoding::Utf16);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to.name, "@b");
        let syms = workspace_symbols(&files, "", Encoding::Utf16);
        assert_eq!(syms.len(), 3);
    }

    #[test]
    fn symbols_across_files() {
        // a.pliron defines @f and calls @g; b.pliron defines @g and calls
        // @f; c.pliron defines its own @f (left alone) and calls it.
        let a = "builtin.module @a {\n^e():\n  t.func @f {\n  ^b():\n    t.call @g\n  }\n}\n";
        let b = "builtin.module @b {\n^e():\n  t.func @g {\n  ^b():\n    t.call @f;\n    t.call @f\n  }\n}\n";
        let c = "builtin.module @c {\n^e():\n  t.func @f {\n  ^b():\n    t.call @f\n  }\n}\n";
        let docs: Vec<Document> = [a, b, c].iter().map(|t| Document::new(t.to_string(), 0, &Default::default())).collect();
        let uris: Vec<Url> = ["a", "b", "c"].iter().map(|n| Url::parse(&format!("file:///w/{n}.pliron")).unwrap()).collect();
        let files: Vec<IndexedFile> = docs
            .iter()
            .zip(&uris)
            .map(|(doc, uri)| IndexedFile { uri, doc, symbols: file_symbols(doc) })
            .collect();

        // From a.pliron, @f is a's: its definition and b's two calls.
        let occ = symbol_occurrences(&files, "f", &uris[0]).unwrap();
        let per_file: Vec<(usize, bool)> = occ.iter().map(|o| (o.file, o.is_def)).collect();
        assert_eq!(per_file, [(0, true), (1, false), (1, false)]);
        // From b.pliron, @f is ambiguous (a and c define it).
        let err = symbol_occurrences(&files, "f", &uris[1]).unwrap_err();
        assert!(err.contains("a.pliron") && err.contains("c.pliron"), "{err}");
        // From c.pliron, @f is c's own.
        assert_eq!(symbol_occurrences(&files, "f", &uris[2]).unwrap().iter().filter(|o| o.file == 2).count(), 2);
        // Renaming a's @f to @g clashes in b.pliron; to @h it does not.
        assert!(rename_conflict(&files, &occ, "g").unwrap().contains("b.pliron"));
        assert!(rename_conflict(&files, &occ, "h").is_none());

        // Lenses: @f in a.pliron has 2 references; the module has none.
        let lenses = reference_lenses(&files, &uris[0], Encoding::Utf16);
        assert_eq!(lenses.len(), 1);
        let cmd = lenses[0].command.as_ref().unwrap();
        assert_eq!(cmd.title, "2 references");
        assert_eq!(cmd.arguments.as_ref().unwrap()[2].as_array().unwrap().len(), 2);
    }
}

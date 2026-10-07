//! Dialect source index: where ops, types and attributes are defined in Rust
//! and what their docs say. Built by scanning dialect crate sources with
//! `syn` (no compilation needed), so it is available immediately and is
//! refreshed as soon as a dialect source file is saved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::visit::Visit;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Op,
    Type,
    Attr,
}

impl EntryKind {
    pub fn describe(self) -> &'static str {
        match self {
            EntryKind::Op => "operation",
            EntryKind::Type => "type",
            EntryKind::Attr => "attribute",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: EntryKind,
    /// `dialect.name`
    pub name: String,
    /// Rust type implementing it.
    pub rust_name: String,
    pub file: PathBuf,
    /// 0-based line and column of the Rust type name.
    pub line: u32,
    pub column: u32,
    pub docs: String,
    pub format: Option<String>,
    pub interfaces: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DialectIndex {
    pub entries: Vec<Entry>,
    by_name: HashMap<String, Vec<usize>>,
}

impl DialectIndex {
    /// Index all `.rs` files under the given source directories.
    pub fn build(dirs: &[PathBuf]) -> DialectIndex {
        let mut idx = DialectIndex::default();
        let mut files = Vec::new();
        for d in dirs {
            collect_rs(d, &mut files, 0);
        }
        files.sort();
        files.dedup();
        for f in files {
            idx.add_file(&f);
        }
        idx
    }

    pub fn add_file(&mut self, file: &Path) {
        let Ok(text) = std::fs::read_to_string(file) else {
            return;
        };
        let mut found = Vec::new();
        scan(&text, file, &mut found);
        for e in found {
            self.by_name
                .entry(e.name.clone())
                .or_default()
                .push(self.entries.len());
            self.entries.push(e);
        }
    }

    /// Re-index one file (after it changed on disk).
    pub fn refresh_file(&mut self, file: &Path) {
        let mut entries: Vec<Entry> = std::mem::take(&mut self.entries)
            .into_iter()
            .filter(|e| e.file != file)
            .collect();
        self.by_name.clear();
        let mut fresh = Vec::new();
        if let Ok(text) = std::fs::read_to_string(file) {
            scan(&text, file, &mut fresh);
        }
        entries.extend(fresh);
        for (i, e) in entries.iter().enumerate() {
            self.by_name.entry(e.name.clone()).or_default().push(i);
        }
        self.entries = entries;
    }

    /// Entries with this name; `kind` narrows the choice when known.
    pub fn lookup(&self, name: &str, kind: Option<EntryKind>) -> Option<&Entry> {
        let ids = self.by_name.get(name)?;
        let mut it = ids.iter().map(|i| &self.entries[*i]);
        match kind {
            Some(k) => it.clone().find(|e| e.kind == k).or_else(|| it.next()),
            None => it.next(),
        }
    }

    pub fn ops(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.kind == EntryKind::Op)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if depth < 10 && !matches!(name, "target" | ".git" | "tests" | "benches" | "examples") {
                collect_rs(&p, out, depth + 1);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Is `s` a `dialect.name` string?
fn is_qualname(s: &str) -> bool {
    let mut parts = s.split('.');
    let ok = |p: &str| {
        let mut c = p.chars();
        matches!(c.next(), Some(c) if c.is_alphabetic() || c == '_')
            && c.all(|c| c.is_alphanumeric() || c == '_')
    };
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => ok(a) && ok(b) && parts.all(ok),
        _ => false,
    }
}

fn lit_str(t: &TokenTree) -> Option<String> {
    let TokenTree::Literal(l) = t else { return None };
    syn::parse_str::<syn::LitStr>(&l.to_string())
        .ok()
        .map(|s| s.value())
}

/// `key = value` pairs of an attribute's arguments (values as token
/// streams).
fn key_values(tokens: TokenStream) -> Vec<(String, Vec<TokenTree>)> {
    let mut out = Vec::new();
    let toks: Vec<TokenTree> = tokens.into_iter().collect();
    let mut i = 0;
    while i < toks.len() {
        if let TokenTree::Ident(id) = &toks[i]
            && matches!(toks.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '=')
        {
            let mut j = i + 2;
            let mut value = Vec::new();
            while j < toks.len()
                && !matches!(&toks[j], TokenTree::Punct(p) if p.as_char() == ',')
            {
                value.push(toks[j].clone());
                j += 1;
            }
            out.push((id.to_string(), value));
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

fn docs_of(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for a in attrs {
        if a.path().is_ident("doc")
            && let syn::Meta::NameValue(nv) = &a.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            let v = s.value();
            lines.push(v.strip_prefix(' ').unwrap_or(&v).to_string());
        }
    }
    lines.join("\n").trim().to_string()
}

struct Scanner<'a> {
    file: &'a Path,
    out: &'a mut Vec<Entry>,
}

impl Scanner<'_> {
    fn item(&mut self, attrs: &[syn::Attribute], ident: &syn::Ident) {
        let mut name = None;
        let mut kind = None;
        let mut format = None;
        let mut interfaces = None;
        for a in attrs {
            let Some(last) = a.path().segments.last() else { continue };
            let attr_name = last.ident.to_string();
            let tokens = match &a.meta {
                syn::Meta::List(l) => l.tokens.clone(),
                _ => TokenStream::new(),
            };
            let k = match attr_name.as_str() {
                "pliron_op" | "def_op" => Some(EntryKind::Op),
                "pliron_type" | "def_type" => Some(EntryKind::Type),
                "pliron_attr" | "def_attribute" => Some(EntryKind::Attr),
                _ => None,
            };
            match attr_name.as_str() {
                "def_op" | "def_type" | "def_attribute" => {
                    kind = k;
                    name = tokens.clone().into_iter().find_map(|t| lit_str(&t));
                }
                "pliron_op" | "pliron_type" | "pliron_attr" => {
                    kind = k;
                    for (key, value) in key_values(tokens.clone()) {
                        match key.as_str() {
                            "name" => name = value.first().and_then(lit_str),
                            "format" => format = value.first().and_then(lit_str),
                            "interfaces" => {
                                interfaces = value.first().map(|v| match v {
                                    TokenTree::Group(g) => g.stream().to_string(),
                                    other => other.to_string(),
                                })
                            }
                            _ => {}
                        }
                    }
                }
                "format_op" | "format_type" | "format_attribute" | "format" => {
                    if let Some(f) = tokens.clone().into_iter().find_map(|t| lit_str(&t)) {
                        format = Some(f);
                    }
                }
                _ => {}
            }
        }
        let (Some(name), Some(kind)) = (name, kind) else {
            return;
        };
        let start = ident.span().start();
        self.out.push(Entry {
            kind,
            name,
            rust_name: ident.to_string(),
            file: self.file.to_path_buf(),
            line: start.line.saturating_sub(1) as u32,
            column: start.column as u32,
            docs: docs_of(attrs),
            format,
            interfaces,
        });
    }

    /// Item-level macro invocations that define ops through `macro_rules!`
    /// (e.g. `new_int_bin_op!(/// docs \n AddOp, "llvm.add")`).
    fn macro_invocation(&mut self, tokens: TokenStream) {
        let toks: Vec<TokenTree> = tokens.into_iter().collect();
        let mut docs = Vec::new();
        let mut rust_name: Option<proc_macro2::Ident> = None;
        let mut name = None;
        let mut format = None;
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                TokenTree::Punct(p) if p.as_char() == '#' => {
                    if let Some(TokenTree::Group(g)) = toks.get(i + 1)
                        && g.delimiter() == Delimiter::Bracket
                    {
                        let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                        if matches!(inner.first(), Some(TokenTree::Ident(id)) if id == "doc")
                            && let Some(s) = inner.iter().find_map(lit_str)
                        {
                            docs.push(s.strip_prefix(' ').unwrap_or(&s).to_string());
                        }
                        i += 2;
                        continue;
                    }
                }
                TokenTree::Ident(id) if rust_name.is_none() => rust_name = Some(id.clone()),
                t => {
                    if let Some(s) = lit_str(t) {
                        if name.is_none() && is_qualname(&s) {
                            name = Some(s);
                        } else if format.is_none() && (s.contains('`') || s.contains('$')) {
                            format = Some(s);
                        }
                    }
                }
            }
            i += 1;
        }
        if let (Some(id), Some(name)) = (rust_name, name) {
            let start = id.span().start();
            self.out.push(Entry {
                kind: EntryKind::Op,
                name,
                rust_name: id.to_string(),
                file: self.file.to_path_buf(),
                line: start.line.saturating_sub(1) as u32,
                column: start.column as u32,
                docs: docs.join("\n").trim().to_string(),
                format,
                interfaces: None,
            });
        }
    }
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.item(&i.attrs, &i.ident);
        syn::visit::visit_item_struct(self, i);
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.item(&i.attrs, &i.ident);
        syn::visit::visit_item_enum(self, i);
    }

    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        if i.ident.is_none() {
            self.macro_invocation(i.mac.tokens.clone());
        }
    }
}

fn scan(text: &str, file: &Path, out: &mut Vec<Entry>) {
    let Ok(ast) = syn::parse_file(text) else {
        return;
    };
    Scanner { file, out }.visit_file(&ast);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_ops_types_and_macro_ops() {
        let src = r#"
/// Adds things.
/// Really.
#[pliron_op(
    name = "toy.add",
    format = "$0 `, ` $1 ` : ` type($0)",
    interfaces = [OneResultInterface, NOpdsInterface<2>],
    verifier = "succ"
)]
pub struct AddOp;

/// A number.
#[pliron_type(name = "toy.num", format = "`<` $width `>`")]
pub struct NumType { width: u32 }

#[def_op("old.style")]
#[format_op("$0")]
struct OldOp;

new_int_bin_op!(
    /// Equivalent to LLVM's Add.
    AddOp2,
    "llvm.add"
);
"#;
        let mut out = Vec::new();
        scan(src, Path::new("/x/lib.rs"), &mut out);
        let names: Vec<(&str, EntryKind)> = out.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            [
                ("toy.add", EntryKind::Op),
                ("toy.num", EntryKind::Type),
                ("old.style", EntryKind::Op),
                ("llvm.add", EntryKind::Op)
            ]
        );
        let add = &out[0];
        assert_eq!(add.docs, "Adds things.\nReally.");
        assert_eq!(add.format.as_deref(), Some("$0 `, ` $1 ` : ` type($0)"));
        assert_eq!(add.rust_name, "AddOp");
        assert_eq!(add.line, 9);
        assert!(add.interfaces.as_deref().unwrap().contains("NOpdsInterface"));
        assert_eq!(out[2].format.as_deref(), Some("$0"));
        assert_eq!(out[3].docs, "Equivalent to LLVM's Add.");
        assert_eq!(out[3].rust_name, "AddOp2");
    }
}

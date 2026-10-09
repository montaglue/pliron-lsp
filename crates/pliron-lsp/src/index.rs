//! Dialect source index: where ops, types and attributes are defined in Rust
//! and what their docs say. Built by scanning dialect crate sources with
//! `syn` (no compilation needed), so it is available immediately and is
//! refreshed as soon as a dialect source file is saved.
//!
//! `pliron_lsp_api::hints! { ... }` blocks in the sources override what is
//! derived from the `#[pliron_op(...)]` attributes and doc comments.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pliron_ir_syntax::{Knowledge, OpFacts};
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
    /// Operand names from `operands = (lhs, rhs: Type, _)`.
    pub operands: Vec<String>,
    /// Completion snippet of what follows the name (from `hints!`).
    pub snippet: Option<String>,
    /// Syntax facts from `hints!`.
    pub isolated: Option<bool>,
    pub symbol: Option<bool>,
    pub keywords: Vec<String>,
}

/// One entry of a `pliron_lsp_api::hints!` block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hint {
    pub kind: Option<EntryKind>,
    pub name: String,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub doc: Option<String>,
    pub format: Option<String>,
    pub snippet: Option<String>,
    pub operands: Option<Vec<String>>,
    pub isolated: Option<bool>,
    pub symbol: Option<bool>,
    pub keywords: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct DialectIndex {
    /// Entries with hints applied.
    pub entries: Vec<Entry>,
    by_name: HashMap<String, Vec<usize>>,
    /// What the sources define, before hints.
    derived: Vec<Entry>,
    hints: Vec<Hint>,
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
            if let Ok(text) = std::fs::read_to_string(&f) {
                scan(&text, &f, &mut idx.derived, &mut idx.hints);
            }
        }
        idx.merge();
        idx
    }

    /// Index source text (for tests and in-memory sources).
    pub fn add_source(&mut self, text: &str, file: &Path) {
        scan(text, file, &mut self.derived, &mut self.hints);
        self.merge();
    }

    /// Re-index one file (after it changed on disk).
    pub fn refresh_file(&mut self, file: &Path) {
        self.derived.retain(|e| e.file != file);
        self.hints.retain(|h| h.file != file);
        if let Ok(text) = std::fs::read_to_string(file) {
            scan(&text, file, &mut self.derived, &mut self.hints);
        }
        self.merge();
    }

    /// Apply the hints to the derived entries.
    fn merge(&mut self) {
        let mut entries = self.derived.clone();
        for h in &self.hints {
            let matching: Vec<usize> = (0..entries.len())
                .filter(|i| {
                    entries[*i].name == h.name && h.kind.is_none_or(|k| entries[*i].kind == k)
                })
                .collect();
            let targets = if matching.is_empty() {
                // Not found in the sources (e.g. defined by an unusual
                // macro): the hint defines the entry.
                entries.push(Entry {
                    kind: h.kind.unwrap_or(EntryKind::Op),
                    name: h.name.clone(),
                    rust_name: "hints!".into(),
                    file: h.file.clone(),
                    line: h.line,
                    column: h.column,
                    docs: String::new(),
                    format: None,
                    interfaces: None,
                    operands: Vec::new(),
                    snippet: None,
                    isolated: None,
                    symbol: None,
                    keywords: Vec::new(),
                });
                vec![entries.len() - 1]
            } else {
                matching
            };
            for i in targets {
                let e = &mut entries[i];
                if let Some(d) = &h.doc {
                    e.docs = d.clone();
                }
                if let Some(f) = &h.format {
                    e.format = Some(f.clone());
                }
                if let Some(s) = &h.snippet {
                    e.snippet = Some(s.clone());
                }
                if let Some(o) = &h.operands {
                    e.operands = o.clone();
                }
                if let Some(k) = &h.keywords {
                    e.keywords = k.clone();
                }
                e.isolated = h.isolated.or(e.isolated);
                e.symbol = h.symbol.or(e.symbol);
            }
        }
        self.by_name.clear();
        for (i, e) in entries.iter().enumerate() {
            self.by_name.entry(e.name.clone()).or_default().push(i);
        }
        self.entries = entries;
    }

    /// What the syntax layer should know about the ops (used before the
    /// engine is built): from interfaces, format keywords and hints.
    pub fn knowledge(&self) -> Knowledge {
        let mut k = Knowledge::default();
        for e in self.ops() {
            let has = |iface: &str| {
                e.interfaces
                    .as_deref()
                    .is_some_and(|i| {
                        i.split(|c: char| !c.is_alphanumeric() && c != '_')
                            .any(|w| w == iface)
                    })
                    .then_some(true)
            };
            let mut keywords = e.keywords.clone();
            if let Some(f) = &e.format {
                for el in crate::format::parse(f) {
                    if let crate::format::Elem::Lit(l) = el
                        && pliron_ir_syntax::lexer::is_identifier(l.trim())
                        && !keywords.iter().any(|k| k == l.trim())
                    {
                        keywords.push(l.trim().to_string());
                    }
                }
            }
            k.ops.insert(
                e.name.clone(),
                OpFacts {
                    isolated: e.isolated.or_else(|| has("IsolatedFromAboveInterface")),
                    symbol: e.symbol.or_else(|| has("SymbolOpInterface")),
                    keywords,
                    trailing_result_type: None,
                },
            );
        }
        k
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
    let TokenTree::Literal(l) = t else {
        return None;
    };
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
            while j < toks.len() && !matches!(&toks[j], TokenTree::Punct(p) if p.as_char() == ',') {
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

/// Names in `(lhs, rhs: Type, _)` (the first identifier of each element).
fn operand_names(tokens: TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    let mut first: Option<String> = None;
    let mut started = false;
    for t in tokens {
        match &t {
            TokenTree::Punct(p) if p.as_char() == ',' => {
                out.push(first.take().unwrap_or_else(|| "_".into()));
                started = false;
            }
            TokenTree::Ident(id) if !started => {
                first = Some(id.to_string());
                started = true;
            }
            _ => started = true,
        }
    }
    if started || first.is_some() {
        out.push(first.unwrap_or_else(|| "_".into()));
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
    hints: &'a mut Vec<Hint>,
}

impl Scanner<'_> {
    fn item(&mut self, attrs: &[syn::Attribute], ident: &syn::Ident) {
        let mut name = None;
        let mut kind = None;
        let mut format = None;
        let mut interfaces = None;
        let mut operands = Vec::new();
        for a in attrs {
            let Some(last) = a.path().segments.last() else {
                continue;
            };
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
                            "operands" => {
                                if let Some(TokenTree::Group(g)) = value.first() {
                                    operands = operand_names(g.stream());
                                }
                            }
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
                // Project-specific wrappers of the pliron macros (e.g.
                // `#[cube_op(name = "cube.read", ...)]`): any attribute with
                // a `name = "dialect.name"` argument.
                other if kind.is_none() && !matches!(other, "doc" | "derive" | "cfg" | "allow") => {
                    let kv = key_values(tokens.clone());
                    if let Some(n) = kv
                        .iter()
                        .find(|(k, _)| k == "name")
                        .and_then(|(_, v)| v.first().and_then(lit_str))
                        .filter(|n| is_qualname(n))
                    {
                        name = Some(n);
                        kind = Some(if other.contains("type") {
                            EntryKind::Type
                        } else if other.contains("attr") {
                            EntryKind::Attr
                        } else {
                            EntryKind::Op
                        });
                        for (key, value) in kv {
                            match key.as_str() {
                                "format" => format = value.first().and_then(lit_str),
                                "operands" => {
                                    if let Some(TokenTree::Group(g)) = value.first() {
                                        operands = operand_names(g.stream());
                                    }
                                }
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
            operands,
            snippet: None,
            isolated: None,
            symbol: None,
            keywords: Vec::new(),
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
                operands: Vec::new(),
                snippet: None,
                isolated: None,
                symbol: None,
                keywords: Vec::new(),
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
        if i.ident.is_some() {
            return;
        }
        if i.mac
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "hints")
        {
            self.hints_block(i.mac.tokens.clone());
        } else {
            self.macro_invocation(i.mac.tokens.clone());
        }
    }
}

impl Scanner<'_> {
    /// `hints! { op "d.x" { key: value, ... } type "d.t" { ... } }`
    fn hints_block(&mut self, tokens: TokenStream) {
        let toks: Vec<TokenTree> = tokens.into_iter().collect();
        let mut i = 0;
        while i + 2 < toks.len() {
            let (TokenTree::Ident(kind), Some(name), TokenTree::Group(body)) =
                (&toks[i], lit_str(&toks[i + 1]), &toks[i + 2])
            else {
                i += 1;
                continue;
            };
            i += 3;
            let start = toks[i - 2].span().start();
            let mut h = Hint {
                kind: match kind.to_string().as_str() {
                    "op" => Some(EntryKind::Op),
                    "type" => Some(EntryKind::Type),
                    "attr" => Some(EntryKind::Attr),
                    _ => continue,
                },
                name,
                file: self.file.to_path_buf(),
                line: start.line.saturating_sub(1) as u32,
                column: start.column as u32,
                ..Hint::default()
            };
            for (key, value) in colon_values(body.stream()) {
                let strings = || match value.first() {
                    Some(TokenTree::Group(g)) => {
                        g.stream().into_iter().filter_map(|t| lit_str(&t)).collect()
                    }
                    _ => Vec::new(),
                };
                let boolean = || match value.first() {
                    Some(TokenTree::Ident(b)) => Some(b == "true"),
                    _ => None,
                };
                let string = || value.first().and_then(lit_str);
                match key.as_str() {
                    "doc" => h.doc = string(),
                    "format" => h.format = string(),
                    "snippet" => h.snippet = string(),
                    "operands" => h.operands = Some(strings()),
                    "keywords" => h.keywords = Some(strings()),
                    "isolated" => h.isolated = boolean(),
                    "symbol" => h.symbol = boolean(),
                    _ => {}
                }
            }
            self.hints.push(h);
        }
    }
}

/// `key: value` pairs separated by commas (values as token trees).
fn colon_values(tokens: TokenStream) -> Vec<(String, Vec<TokenTree>)> {
    let mut out = Vec::new();
    let toks: Vec<TokenTree> = tokens.into_iter().collect();
    let mut i = 0;
    while i < toks.len() {
        if let TokenTree::Ident(id) = &toks[i]
            && matches!(toks.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
        {
            let mut j = i + 2;
            let mut value = Vec::new();
            while j < toks.len() && !matches!(&toks[j], TokenTree::Punct(p) if p.as_char() == ',') {
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

fn scan(text: &str, file: &Path, out: &mut Vec<Entry>, hints: &mut Vec<Hint>) {
    let Ok(ast) = syn::parse_file(text) else {
        return;
    };
    Scanner { file, out, hints }.visit_file(&ast);
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

/// Reads a builtin.
#[cube_op(name = "cube.read_builtin", format = "`(` $builtin `)` ` : ` type($0)")]
pub struct ReadBuiltinOp;
"#;
        let mut out = Vec::new();
        scan(src, Path::new("/x/lib.rs"), &mut out, &mut Vec::new());
        let names: Vec<(&str, EntryKind)> = out.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            [
                ("toy.add", EntryKind::Op),
                ("toy.num", EntryKind::Type),
                ("old.style", EntryKind::Op),
                ("llvm.add", EntryKind::Op),
                ("cube.read_builtin", EntryKind::Op)
            ]
        );
        let add = &out[0];
        assert_eq!(add.docs, "Adds things.\nReally.");
        assert_eq!(add.format.as_deref(), Some("$0 `, ` $1 ` : ` type($0)"));
        assert_eq!(add.rust_name, "AddOp");
        assert_eq!(add.line, 9);
        assert!(
            add.interfaces
                .as_deref()
                .unwrap()
                .contains("NOpdsInterface")
        );
        assert_eq!(out[2].format.as_deref(), Some("$0"));
        assert_eq!(out[3].docs, "Equivalent to LLVM's Add.");
        assert_eq!(out[3].rust_name, "AddOp2");
    }

    #[test]
    fn hints_override_and_add() {
        let src = r#"
/// Derived docs.
#[pliron_op(
    name = "my.for",
    interfaces = [IsolatedFromAboveInterface],
)]
pub struct ForOp;

/// Prints.
#[pliron_op(name = "my.print", format = "`value` ` = ` $0")]
pub struct PrintOp;

pliron_lsp_api::hints! {
    op "my.for" {
        format: "$0 `to` $1 region($0)",
        snippet: "${1:lb} to ${2:ub} {\n\t$0\n}",
        operands: ["lb", "ub"],
        keywords: ["to"],
    }
    type "my.vec" {
        doc: "A vector.",
    }
}
"#;
        let mut idx = DialectIndex::default();
        idx.add_source(src, Path::new("/x/lib.rs"));
        let f = idx.lookup("my.for", Some(EntryKind::Op)).unwrap();
        assert_eq!(f.rust_name, "ForOp");
        assert_eq!(f.docs, "Derived docs.");
        assert_eq!(f.format.as_deref(), Some("$0 `to` $1 region($0)"));
        assert_eq!(f.snippet.as_deref(), Some("${1:lb} to ${2:ub} {\n\t$0\n}"));
        assert_eq!(f.operands, ["lb", "ub"]);
        // Not defined in the sources: the hint defines it.
        let v = idx.lookup("my.vec", None).unwrap();
        assert_eq!(
            (v.kind, v.docs.as_str(), v.line),
            (EntryKind::Type, "A vector.", 19)
        );

        let k = idx.knowledge();
        let facts = &k.ops["my.for"];
        assert_eq!(facts.isolated, Some(true));
        assert_eq!(facts.keywords, ["to"]);
        // Keywords from declarative formats.
        assert_eq!(k.ops["my.print"].keywords, ["value"]);
    }
}

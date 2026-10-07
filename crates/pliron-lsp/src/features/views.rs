//! Text views for editor extensions (like rust-analyzer's "View HIR" /
//! "View Syntax Tree"): the engine's IR model, the syntax layer's tree and
//! the dialect registry.

use std::fmt::Write as _;

use pliron_ir_syntax::{BlockId, StmtId};
use pliron_lsp_protocol::{Model, Pos, ValueDef, op_traits};

use crate::document::Document;
use crate::index::{DialectIndex, EntryKind};

fn pos(p: Option<Pos>) -> String {
    p.map(|p| format!("{}:{}", p.line, p.column))
        .unwrap_or_else(|| "?".into())
}

fn value_name(m: &Model, v: u32) -> String {
    let info = &m.values[v as usize];
    match (&info.given_name, info.def) {
        (_, ValueDef::Detached { unresolved: true }) => format!("<undefined %{v}>"),
        (Some(n), _) => n.clone(),
        (None, _) => format!("%{v}"),
    }
}

/// The engine's model of a document, as an indented tree.
pub fn model_tree(doc: &Document) -> String {
    let Some(x) = doc.exact.as_deref() else {
        return "No engine analysis for this document yet.\n".into();
    };
    let mut out = String::new();
    if x.text_hash != doc.hash {
        out.push_str("// (stale: the document changed since this analysis)\n");
    }
    writeln!(
        out,
        "// {} ops, {} blocks, {} values; analyzed in {} µs\n",
        x.model.ops.len(),
        x.model.blocks.len(),
        x.model.values.len(),
        x.elapsed_us
    )
    .unwrap();
    for d in &x.diagnostics {
        writeln!(out, "// error: {}", d.message.replace('\n', " ")).unwrap();
    }
    if x.model.ops.is_empty() {
        return out;
    }
    fn op(out: &mut String, m: &Model, id: u32, depth: usize) {
        let o = &m.ops[id as usize];
        let ind = "  ".repeat(depth);
        let results: Vec<String> = o
            .results
            .iter()
            .map(|v| format!("{}: {}", value_name(m, *v), m.values[*v as usize].ty))
            .collect();
        let mut line = String::new();
        if !results.is_empty() {
            write!(line, "{} = ", results.join(", ")).unwrap();
        }
        line.push_str(&o.opid);
        if let Some(s) = &o.symbol {
            write!(line, " @{s}").unwrap();
        }
        if !o.operands.is_empty() {
            let ops: Vec<String> = o.operands.iter().map(|v| value_name(m, *v)).collect();
            write!(line, " ({})", ops.join(", ")).unwrap();
        }
        for s in &o.successors {
            let label = m.blocks[*s as usize].label.as_deref().unwrap_or("?");
            write!(line, " ^{label}").unwrap();
        }
        let attrs: Vec<String> = o
            .attrs
            .iter()
            .filter(|a| a.key != "builtin_given_names")
            .map(|a| format!("{} = {}", a.key, a.text))
            .collect();
        if !attrs.is_empty() {
            write!(line, " [{}]", attrs.join(", ")).unwrap();
        }
        let mut traits = Vec::new();
        for (bit, name) in [
            (op_traits::ISOLATED_FROM_ABOVE, "isolated"),
            (op_traits::SYMBOL_TABLE, "symbol-table"),
            (op_traits::TERMINATOR, "terminator"),
        ] {
            if o.traits & bit != 0 {
                traits.push(name);
            }
        }
        if !traits.is_empty() {
            write!(line, "  {{{}}}", traits.join(", ")).unwrap();
        }
        writeln!(out, "{ind}{line}    // {}", pos(o.pos)).unwrap();
        for (ri, r) in o.regions.iter().enumerate() {
            writeln!(out, "{ind}  region #{ri}").unwrap();
            for b in &m.regions[*r as usize].blocks {
                let bi = &m.blocks[*b as usize];
                let args: Vec<String> = bi
                    .args
                    .iter()
                    .map(|v| format!("{}: {}", value_name(m, *v), m.values[*v as usize].ty))
                    .collect();
                writeln!(
                    out,
                    "{ind}    ^{}({})    // {}",
                    bi.label.as_deref().unwrap_or("?"),
                    args.join(", "),
                    pos(bi.pos)
                )
                .unwrap();
                for child in &bi.ops {
                    op(out, m, *child, depth + 3);
                }
            }
        }
    }
    op(&mut out, &x.model, x.model.root, 0);
    out
}

/// The syntax layer's structural tree of a document.
pub fn syntax_tree(doc: &Document) -> String {
    let a = &doc.syntax;
    let t = &a.tree;
    let text = &doc.text;
    let mut out = String::new();
    let span = |s: u32, e: u32| {
        let (l1, c1) = doc.line_index.pliron_position(s, text);
        let (l2, c2) = doc.line_index.pliron_position(e, text);
        format!("{l1}:{c1}..{l2}:{c2}")
    };
    fn stmt(out: &mut String, doc: &Document, id: StmtId, depth: usize, span: &dyn Fn(u32, u32) -> String) {
        let t = &doc.syntax.tree;
        let text = &doc.text;
        let s = t.stmt(id);
        let ind = "  ".repeat(depth);
        let results: Vec<&str> = s.results.iter().map(|r| t.tok(*r).text(text)).collect();
        let op = s.op_name.map(|o| t.tok(o).text(text)).unwrap_or("<missing op>");
        let (a, b) = t.stmt_range(id);
        let res = if results.is_empty() {
            String::new()
        } else {
            format!("{} = ", results.join(", "))
        };
        writeln!(out, "{ind}STMT {res}{op}  ({} body tokens)  {}", s.body.len(), span(a, b)).unwrap();
        for r in &s.regions {
            let (a, b) = t.region_range(*r);
            writeln!(out, "{ind}  REGION  {}", span(a, b)).unwrap();
            for blk in &t.region(*r).blocks {
                block(out, doc, *blk, depth + 2, span);
            }
        }
    }
    fn block(out: &mut String, doc: &Document, id: BlockId, depth: usize, span: &dyn Fn(u32, u32) -> String) {
        let t = &doc.syntax.tree;
        let text = &doc.text;
        let b = t.block(id);
        let ind = "  ".repeat(depth);
        let args: Vec<String> = b
            .args
            .iter()
            .map(|a| {
                let ty = a
                    .ty
                    .map(|(s, e)| &text[t.tok(s).start as usize..t.tok(e).end as usize])
                    .unwrap_or("?");
                format!("{}: {ty}", t.tok(a.name).text(text))
            })
            .collect();
        let (a, e) = t.block_range(id);
        writeln!(
            out,
            "{ind}BLOCK {}({})  {}",
            t.tok(b.label).text(text),
            args.join(", "),
            span(a, e)
        )
        .unwrap();
        for s in &b.stmts {
            stmt(out, doc, *s, depth + 1, span);
        }
    }
    for s in &t.top {
        stmt(&mut out, doc, *s, 0, &span);
    }
    if let Some(o) = &t.outlined {
        writeln!(out, "OUTLINED ({} entries)", o.entries.len()).unwrap();
    }
    if !a.diagnostics.is_empty() {
        out.push('\n');
        for d in &a.diagnostics {
            writeln!(out, "// {:?}: {} at {}", d.severity, d.message, span(d.start, d.end)).unwrap();
        }
    }
    out
}

/// The dialect registry known from the dialect sources, as markdown.
pub fn registry(index: Option<&DialectIndex>) -> String {
    let Some(index) = index else {
        return "No dialect index for this document yet.".into();
    };
    let mut out = String::from("# Dialect registry\n");
    for kind in [EntryKind::Op, EntryKind::Type, EntryKind::Attr] {
        let mut entries: Vec<_> = index.entries.iter().filter(|e| e.kind == kind).collect();
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        writeln!(out, "\n## {}s ({})\n", kind.describe(), entries.len()).unwrap();
        out.push_str("| name | Rust type | defined in |\n|---|---|---|\n");
        for e in entries {
            let uri = lsp_types::Url::from_file_path(&e.file)
                .map(|u| format!("{u}#L{}", e.line + 1))
                .unwrap_or_default();
            let file = e
                .file
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("?");
            writeln!(
                out,
                "| `{}` | `{}` | [{file}:{}]({uri}) |",
                e.name,
                e.rust_name,
                e.line + 1
            )
            .unwrap();
        }
    }
    out
}

//! Semantic highlighting.
//!
//! With an exact analysis, every word is classified by what the (compiled)
//! dialect parsers actually parsed it as: op names, types, attributes,
//! attribute keys, format keywords, SSA values, block labels. Words inside
//! an op that no parser primitive claimed are op-specific keywords of
//! hand-written parsers (`if`, `else`, ...). Without an exact analysis the
//! syntax layer's roles are used.

use std::collections::HashMap;

use lsp_types::{SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend};
use pliron_ir_syntax::{DefKind, Encoding, Offset, Role, TokenKind};
use pliron_lsp_protocol::SpanKind;

use crate::document::Document;

/// Token types, in legend order.
pub const TYPES: &[&str] = &[
    "namespace",  // 0: dialect prefix
    "function",   // 1: op name, symbols
    "type",       // 2
    "macro",      // 3: attribute names, `!N`
    "property",   // 4: attribute keys
    "keyword",    // 5
    "variable",   // 6: SSA values
    "parameter",  // 7: block arguments
    "label",      // 8: block labels (custom)
    "number",     // 9
    "string",     // 10
    "enumMember", // 11: words inside attributes
    "operator",   // 12
];

pub const MODIFIERS: &[&str] = &["declaration", "defaultLibrary", "static"];

const DECL: u32 = 1 << 0;
const DEFAULT_LIB: u32 = 1 << 1;

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TYPES.iter().map(|t| SemanticTokenType::new(t)).collect(),
        token_modifiers: MODIFIERS
            .iter()
            .map(|m| SemanticTokenModifier::new(m))
            .collect(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tok {
    start: Offset,
    end: Offset,
    ty: u32,
    mods: u32,
}

fn builtin_mod(name: &str) -> u32 {
    if name.starts_with("builtin.") {
        DEFAULT_LIB
    } else {
        0
    }
}

/// Split a qualified op name into a dialect prefix (namespace) and the rest.
fn qualname(out: &mut Vec<Tok>, text: &str, start: Offset, end: Offset, ty: u32, mods: u32) {
    let s = &text[start as usize..end as usize];
    match s.find('.') {
        Some(dot) => {
            out.push(Tok {
                start,
                end: start + dot as Offset,
                ty: 0,
                mods,
            });
            out.push(Tok {
                start: start + dot as Offset + 1,
                end,
                ty,
                mods,
            });
        }
        None => out.push(Tok {
            start,
            end,
            ty,
            mods,
        }),
    }
}

fn exact_tokens(doc: &Document) -> Option<Vec<Tok>> {
    let x = doc.fresh_exact()?;
    let text = &doc.text;
    // Leaf spans by start offset.
    let mut leaf: HashMap<Offset, (Offset, &SpanKind)> = HashMap::new();
    for s in &x.spans {
        match s.kind {
            SpanKind::Op { .. } | SpanKind::Region { .. } | SpanKind::OpError => {}
            SpanKind::Type { .. } | SpanKind::Attr { .. } => {}
            _ => {
                leaf.entry(s.start).or_insert((s.end, &s.kind));
            }
        }
    }
    let op_names: HashMap<Offset, (Offset, u32)> = x
        .op_span
        .iter()
        .map(|(op, (_, (ns, ne)))| (*ns, (*ne, *op)))
        .collect();
    let type_names: HashMap<Offset, Offset> = x
        .spans
        .iter()
        .filter_map(|s| match &s.kind {
            SpanKind::Type { .. } => Some((s.start, s.end)),
            _ => None,
        })
        .collect();
    let type_name_end: HashMap<Offset, ()> = HashMap::new();
    let _ = type_name_end;
    let in_kind = |off: Offset, want: fn(&SpanKind) -> bool| {
        x.spans
            .iter()
            .any(|s| want(&s.kind) && s.start <= off && off < s.end)
    };
    let symbol_defs: HashMap<Offset, ()> = x.symbols.iter().map(|d| (d.range.0, ())).collect();

    let mut out = Vec::new();
    for t in &doc.syntax.tree.tokens {
        let (s, e) = (t.start, t.end);
        if let Some((end, kind)) = leaf.get(&s) {
            let (ty, mods) = match kind {
                SpanKind::ResultDef { .. } => (6, DECL),
                SpanKind::BlockArgDef { .. } => (7, DECL),
                SpanKind::OperandUse { value } => {
                    let is_arg = matches!(
                        x.model.values.get(*value as usize).map(|v| v.def),
                        Some(pliron_lsp_protocol::ValueDef::Arg { .. })
                    );
                    (if is_arg { 7 } else { 6 }, 0)
                }
                SpanKind::BlockLabel { .. } => (8, DECL),
                SpanKind::SuccessorUse { .. } => (8, 0),
                SpanKind::AttrKey => (4, 0),
                // Marked by a hand-written parser (pliron_lsp_api::token!).
                SpanKind::Token { token_type } => {
                    match TYPES.iter().position(|t| t == token_type) {
                        Some(ty) => (ty as u32, 0),
                        None => continue,
                    }
                }
                SpanKind::Keyword => {
                    // Punctuation literals are not interesting as keywords.
                    if text[s as usize..(*end).min(e) as usize]
                        .chars()
                        .any(char::is_alphanumeric)
                    {
                        (5, 0)
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            out.push(Tok {
                start: s,
                end: e.min(*end).max(s),
                ty,
                mods,
            });
            continue;
        }
        match t.kind {
            TokenKind::QualName => {
                let name = t.text(text);
                if let Some((ne, _)) = op_names.get(&s)
                    && *ne == e
                {
                    qualname(&mut out, text, s, e, 1, builtin_mod(name));
                } else if type_names.contains_key(&s) {
                    qualname(&mut out, text, s, e, 2, builtin_mod(name));
                } else if in_kind(s, |k| matches!(k, SpanKind::Attr { .. })) {
                    qualname(&mut out, text, s, e, 3, builtin_mod(name));
                } else if in_kind(s, |k| matches!(k, SpanKind::Type { .. })) {
                    qualname(&mut out, text, s, e, 2, builtin_mod(name));
                }
            }
            TokenKind::SymbolRef => out.push(Tok {
                start: s,
                end: e,
                ty: 1,
                mods: if symbol_defs.contains_key(&s) {
                    DECL
                } else {
                    0
                },
            }),
            TokenKind::OutlineRef => out.push(Tok {
                start: s,
                end: e,
                ty: 3,
                mods: 0,
            }),
            TokenKind::Number => out.push(Tok {
                start: s,
                end: e,
                ty: 9,
                mods: 0,
            }),
            TokenKind::String => out.push(Tok {
                start: s,
                end: e,
                ty: 10,
                mods: 0,
            }),
            TokenKind::Arrow => out.push(Tok {
                start: s,
                end: e,
                ty: 12,
                mods: 0,
            }),
            TokenKind::Ident => {
                if in_kind(s, |k| matches!(k, SpanKind::Type { .. })) {
                    out.push(Tok {
                        start: s,
                        end: e,
                        ty: 2,
                        mods: 0,
                    });
                } else if in_kind(s, |k| matches!(k, SpanKind::Attr { .. })) {
                    out.push(Tok {
                        start: s,
                        end: e,
                        ty: 11,
                        mods: 0,
                    });
                } else if in_kind(s, |k| matches!(k, SpanKind::Op { .. })) {
                    // Claimed by no parser primitive: a word of an op's own
                    // (hand-written) syntax.
                    out.push(Tok {
                        start: s,
                        end: e,
                        ty: 5,
                        mods: 0,
                    });
                }
            }
            _ => {}
        }
    }
    Some(out)
}

fn syntax_tokens(doc: &Document) -> Vec<Tok> {
    let a = &doc.syntax;
    let text = &doc.text;
    let mut out = Vec::new();
    for (i, t) in a.tree.tokens.iter().enumerate() {
        let (s, e) = (t.start, t.end);
        let role = a.role(i as u32);
        match (t.kind, role) {
            (TokenKind::QualName, Role::OpName) => {
                qualname(&mut out, text, s, e, 1, builtin_mod(t.text(text)))
            }
            (TokenKind::QualName, _) => {
                qualname(&mut out, text, s, e, 2, builtin_mod(t.text(text)))
            }
            (_, Role::Def(d)) => {
                let ty = match a.def(d).kind {
                    DefKind::Result => 6,
                    DefKind::BlockArg => 7,
                    DefKind::Label => 8,
                    DefKind::Symbol => 1,
                    DefKind::Outline => 3,
                };
                out.push(Tok {
                    start: s,
                    end: e,
                    ty,
                    mods: DECL,
                });
            }
            (_, Role::Use(d, _)) => {
                let ty = match a.def(d).kind {
                    DefKind::Result => 6,
                    DefKind::BlockArg => 7,
                    DefKind::Label => 8,
                    DefKind::Symbol => 1,
                    DefKind::Outline => 3,
                };
                out.push(Tok {
                    start: s,
                    end: e,
                    ty,
                    mods: 0,
                });
            }
            (_, Role::Keyword) => out.push(Tok {
                start: s,
                end: e,
                ty: 5,
                mods: 0,
            }),
            (_, Role::AttrKey) => out.push(Tok {
                start: s,
                end: e,
                ty: 4,
                mods: 0,
            }),
            (TokenKind::BlockLabel, _) => out.push(Tok {
                start: s,
                end: e,
                ty: 8,
                mods: 0,
            }),
            (TokenKind::SymbolRef, _) => out.push(Tok {
                start: s,
                end: e,
                ty: 1,
                mods: 0,
            }),
            (TokenKind::OutlineRef, _) => out.push(Tok {
                start: s,
                end: e,
                ty: 3,
                mods: 0,
            }),
            (TokenKind::Number, _) => out.push(Tok {
                start: s,
                end: e,
                ty: 9,
                mods: 0,
            }),
            (TokenKind::String, _) => out.push(Tok {
                start: s,
                end: e,
                ty: 10,
                mods: 0,
            }),
            (TokenKind::Arrow, _) => out.push(Tok {
                start: s,
                end: e,
                ty: 12,
                mods: 0,
            }),
            _ => {}
        }
    }
    out
}

/// Encode semantic tokens for `doc`, optionally limited to a byte range.
pub fn semantic_tokens(
    doc: &Document,
    enc: Encoding,
    range: Option<(Offset, Offset)>,
) -> Vec<SemanticToken> {
    let mut toks = exact_tokens(doc).unwrap_or_else(|| syntax_tokens(doc));
    toks.retain(|t| t.end > t.start);
    if let Some((rs, re)) = range {
        toks.retain(|t| t.end >= rs && t.start <= re);
    }
    toks.sort_by_key(|t| (t.start, t.end));
    toks.dedup_by(|b, a| b.start < a.end); // drop overlaps (keep first)

    let mut out = Vec::with_capacity(toks.len());
    let (mut prev_line, mut prev_col) = (0u32, 0u32);
    for t in toks {
        // Split multi-line tokens (e.g. strings) per line.
        let mut s = t.start;
        while s < t.end {
            let line = doc.line_index.line_of(s);
            let line_end = doc.line_index.line_end(line, &doc.text).max(s);
            let e = t.end.min(line_end);
            if e > s {
                let p = doc.line_index.position(s, &doc.text, enc);
                let pe = doc.line_index.position(e, &doc.text, enc);
                let delta_line = p.line - prev_line;
                let delta_start = if delta_line == 0 {
                    p.col - prev_col
                } else {
                    p.col
                };
                out.push(SemanticToken {
                    delta_line,
                    delta_start,
                    length: pe.col - p.col,
                    token_type: t.ty,
                    token_modifiers_bitset: t.mods,
                });
                prev_line = p.line;
                prev_col = p.col;
            }
            s = doc.line_index.line_start(line + 1).max(e + 1);
        }
    }
    out
}

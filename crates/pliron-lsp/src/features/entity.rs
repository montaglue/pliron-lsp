//! "What is under the cursor?" — shared by definition, references,
//! highlight and rename. Exact engine spans win; the syntax layer is the
//! fallback.

use pliron_ir_syntax::{DefKind, Offset, Role, TokenKind};

use crate::document::Document;
use crate::exact::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityKind {
    Value,
    Block,
    Symbol,
    Outline,
}

#[derive(Clone, Debug)]
pub struct Entity {
    pub kind: EntityKind,
    /// Name without sigil.
    pub name: String,
    /// Range of the name under the cursor.
    pub at: Range,
    pub def: Option<Range>,
    pub uses: Vec<Range>,
    /// Whether the information comes from the dialect engine.
    pub exact: bool,
}

impl Entity {
    /// All occurrences (definition first).
    pub fn occurrences(&self) -> Vec<Range> {
        let mut v: Vec<Range> = self.def.into_iter().collect();
        for u in &self.uses {
            if !v.contains(u) {
                v.push(*u);
            }
        }
        v
    }
}

/// Strip a leading sigil from a range of the text.
fn strip_sigil(doc: &Document, (s, e): Range) -> Range {
    match doc.text[s as usize..e as usize].chars().next() {
        Some('^' | '@' | '!') => (s + 1, e),
        _ => (s, e),
    }
}

pub fn entity_at(doc: &Document, off: Offset) -> Option<Entity> {
    if let Some(x) = doc.fresh_exact() {
        if let Some(v) = x.value_at(off) {
            let def = x.value_def.get(&v).copied();
            let uses = x.value_uses.get(&v).cloned().unwrap_or_default();
            let at = def
                .into_iter()
                .chain(uses.iter().copied())
                .find(|(s, e)| *s <= off && off <= *e)?;
            return Some(Entity {
                kind: EntityKind::Value,
                name: doc.slice(at).to_string(),
                at,
                def,
                uses,
                exact: true,
            });
        }
        if let Some(b) = x.block_at(off) {
            let def = x.block_def.get(&b).copied();
            let uses = x.block_uses.get(&b).cloned().unwrap_or_default();
            let at = def
                .into_iter()
                .chain(uses.iter().copied())
                .find(|(s, e)| *s <= off && off <= *e)?;
            return Some(Entity {
                kind: EntityKind::Block,
                name: doc.slice(strip_sigil(doc, at)).to_string(),
                at,
                def,
                uses,
                exact: true,
            });
        }
        if let Some(name) = x.symbol_at(off) {
            let def = x.symbol_def_by_name(&name).map(|d| d.range);
            let uses: Vec<Range> = x
                .symbol_uses
                .iter()
                .filter(|(_, n)| *n == name)
                .map(|(r, _)| *r)
                .collect();
            let at = def
                .into_iter()
                .chain(uses.iter().copied())
                .find(|(s, e)| *s <= off && off <= *e)?;
            return Some(Entity {
                kind: EntityKind::Symbol,
                name,
                at,
                def,
                uses,
                exact: true,
            });
        }
    }
    syntax_entity_at(doc, off)
}

fn syntax_entity_at(doc: &Document, off: Offset) -> Option<Entity> {
    let a = &doc.syntax;
    let tok = a.tree.token_at(off)?;
    let t = a.tree.tok(tok);
    let def = match a.role(tok) {
        Role::Def(d) | Role::Use(d, _) => d,
        _ => {
            // An unresolved symbol still has same-name occurrences.
            if t.kind == TokenKind::SymbolRef {
                let name = t.name(&doc.text).to_string();
                let uses = a
                    .tree
                    .tokens
                    .iter()
                    .filter(|o| o.kind == TokenKind::SymbolRef && o.name(&doc.text) == name)
                    .map(|o| (o.start, o.end))
                    .collect();
                return Some(Entity {
                    kind: EntityKind::Symbol,
                    name,
                    at: (t.start, t.end),
                    def: None,
                    uses,
                    exact: false,
                });
            }
            return None;
        }
    };
    let d = a.def(def);
    let def_tok = a.tree.tok(d.tok);
    let kind = match d.kind {
        DefKind::Result | DefKind::BlockArg => EntityKind::Value,
        DefKind::Label => EntityKind::Block,
        DefKind::Symbol => EntityKind::Symbol,
        DefKind::Outline => EntityKind::Outline,
    };
    let uses = a.uses[def.0 as usize]
        .iter()
        .map(|u| {
            let t = a.tree.tok(*u);
            (t.start, t.end)
        })
        .collect();
    Some(Entity {
        kind,
        name: d.name.clone(),
        at: (t.start, t.end),
        def: Some((def_tok.start, def_tok.end)),
        uses,
        exact: false,
    })
}

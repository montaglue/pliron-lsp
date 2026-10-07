//! Name resolution over the structural [`Tree`], mirroring pliron's
//! `NameTracker`:
//!
//! * SSA names live in *isolation scopes*: one per `IsolatedFromAbove` op
//!   (and the top level). Nested non-isolated regions share their parent's
//!   scope, and definitions are visible regardless of order (forward
//!   references are legal, e.g. in graph regions).
//! * Block labels are scoped per region.
//! * Symbols (`@name`) are looked up through enclosing regions, then
//!   document-wide.
//! * `!N` refers to an entry of the `outlined_attributes:` section.
//!
//! Which ops are isolated / define symbols is dialect knowledge; it comes
//! from [`Knowledge`] when available and from heuristics otherwise.

use std::collections::HashMap;

use crate::lexer::{Offset, TokenKind};
use crate::tree::{BlockId, ErrorKind, RegionId, StmtId, TokIdx, Tree};

/// Dialect knowledge that refines the syntactic analysis.
#[derive(Clone, Debug, Default)]
pub struct Knowledge {
    pub ops: HashMap<String, OpFacts>,
}

#[derive(Clone, Debug, Default)]
pub struct OpFacts {
    /// Does the op implement `IsolatedFromAboveInterface`?
    pub isolated: Option<bool>,
    /// Does the op implement `SymbolOpInterface` (its `@name` is a definition)?
    pub symbol: Option<bool>,
    /// Literal words of the op's syntax (never SSA value uses).
    pub keywords: Vec<String>,
    /// The op's syntax ends with `: type($0)` (the trailing type is the
    /// result type for sure).
    pub trailing_result_type: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefKind {
    /// Result of an operation.
    Result,
    /// Block argument.
    BlockArg,
    /// Block label.
    Label,
    /// Symbol defined by a symbol op.
    Symbol,
    /// Outlined attribute entry.
    Outline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provenance {
    /// Read from syntax that unambiguously denotes the type.
    Exact,
    /// Guessed from syntax (e.g. a trailing `: type`).
    Heuristic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeSpan {
    pub start: Offset,
    pub end: Offset,
    pub provenance: Provenance,
}

#[derive(Clone, Debug)]
pub struct Def {
    pub kind: DefKind,
    pub tok: TokIdx,
    pub name: String,
    /// Defining statement (results, symbols).
    pub stmt: Option<StmtId>,
    /// Defining block (block args, labels).
    pub block: Option<BlockId>,
    /// Result/argument index.
    pub index: u32,
    /// Syntactic type of a value.
    pub ty: Option<TypeSpan>,
}

/// What a token is, for highlighting and navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    None,
    /// The op name of a statement.
    OpName,
    /// A qualified name that is not an op name (type or attribute).
    TypeOrAttr,
    /// Definition site.
    Def(DefId),
    /// A use resolved to a definition.
    Use(DefId, Confidence),
    /// An identifier that resolves to nothing (keyword / unknown word).
    Word,
    /// A literal keyword of the op's syntax (from [`Knowledge`]).
    Keyword,
    /// `key` in `key = value` / `key: value` inside brackets.
    AttrKey,
    /// An unresolved `^label` / `@sym` / `!N`.
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub start: Offset,
    pub end: Offset,
    pub severity: Severity,
    pub message: String,
    pub kind: ErrorKind,
}

/// The complete syntactic analysis of a document.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub tree: Tree,
    pub defs: Vec<Def>,
    /// Role of every token (parallel to `tree.tokens`).
    pub roles: Vec<Role>,
    /// Uses of every definition (token indices).
    pub uses: Vec<Vec<TokIdx>>,
    /// Isolation scope of every statement's results / every region's values.
    pub stmt_scope: Vec<u32>,
    pub region_scope: Vec<u32>,
    /// Parent of every isolation scope (scope 0 is the root).
    pub scope_parent: Vec<Option<u32>>,
    /// Is a statement's op considered isolated / a symbol op?
    pub stmt_isolated: Vec<bool>,
    pub stmt_symbol: Vec<Option<DefId>>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Analysis {
    pub fn def(&self, id: DefId) -> &Def {
        &self.defs[id.0 as usize]
    }

    pub fn role(&self, tok: TokIdx) -> Role {
        self.roles.get(tok as usize).copied().unwrap_or_default()
    }

    /// The definition a token defines or refers to.
    pub fn def_of_token(&self, tok: TokIdx) -> Option<DefId> {
        match self.role(tok) {
            Role::Def(d) | Role::Use(d, _) => Some(d),
            _ => None,
        }
    }

    /// All definitions of a given kind visible as SSA values at `stmt`
    /// (for completion): its isolation scope, outer scopes excluded.
    pub fn values_in_scope_of(&self, stmt: StmtId) -> Vec<DefId> {
        let scope = self.stmt_scope[stmt.0 as usize];
        self.defs
            .iter()
            .enumerate()
            .filter(|(_, d)| matches!(d.kind, DefKind::Result | DefKind::BlockArg))
            .filter(|(_, d)| self.value_def_scope(d) == Some(scope))
            .map(|(i, _)| DefId(i as u32))
            .collect()
    }

    fn value_def_scope(&self, d: &Def) -> Option<u32> {
        match d.kind {
            DefKind::Result => d.stmt.map(|s| self.stmt_scope[s.0 as usize]),
            DefKind::BlockArg => d
                .block
                .map(|b| self.region_scope[self.tree.block(b).region.0 as usize]),
            _ => None,
        }
    }

    /// Labels defined in a region.
    pub fn labels_of_region(&self, region: RegionId) -> Vec<DefId> {
        self.defs
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                d.kind == DefKind::Label
                    && d.block.is_some_and(|b| self.tree.block(b).region == region)
            })
            .map(|(i, _)| DefId(i as u32))
            .collect()
    }

    pub fn defs_of_kind(&self, kind: DefKind) -> impl Iterator<Item = DefId> + '_ {
        self.defs
            .iter()
            .enumerate()
            .filter(move |(_, d)| d.kind == kind)
            .map(|(i, _)| DefId(i as u32))
    }
}

pub fn analyze(src: &str, knowledge: &Knowledge) -> Analysis {
    let tree = crate::tree::parse(src);
    Resolver::new(src, tree, knowledge).run()
}

struct Resolver<'a> {
    src: &'a str,
    k: &'a Knowledge,
    a: Analysis,
    /// (scope, name) -> value defs
    values: HashMap<(u32, String), Vec<DefId>>,
    /// (region, name) -> label defs
    labels: HashMap<(u32, String), Vec<DefId>>,
    /// (region or u32::MAX for top level, name) -> symbol defs
    symbols: HashMap<(u32, String), Vec<DefId>>,
    outline: HashMap<u32, DefId>,
}

const TOP: u32 = u32::MAX;

impl<'a> Resolver<'a> {
    fn new(src: &'a str, tree: Tree, k: &'a Knowledge) -> Self {
        let n_tok = tree.tokens.len();
        let n_stmt = tree.stmts.len();
        let n_region = tree.regions.len();
        let a = Analysis {
            tree,
            defs: Vec::new(),
            roles: vec![Role::None; n_tok],
            uses: Vec::new(),
            stmt_scope: vec![0; n_stmt],
            region_scope: vec![0; n_region],
            scope_parent: vec![None],
            stmt_isolated: vec![false; n_stmt],
            stmt_symbol: vec![None; n_stmt],
            diagnostics: Vec::new(),
        };
        Resolver {
            src,
            k,
            a,
            values: HashMap::new(),
            labels: HashMap::new(),
            symbols: HashMap::new(),
            outline: HashMap::new(),
        }
    }

    fn text(&self, tok: TokIdx) -> &'a str {
        self.a.tree.tok(tok).text(self.src)
    }

    fn name(&self, tok: TokIdx) -> &'a str {
        self.a.tree.tok(tok).name(self.src)
    }

    fn op_name(&self, stmt: StmtId) -> Option<&'a str> {
        self.a.tree.stmt(stmt).op_name.map(|t| self.text(t))
    }

    fn facts(&self, stmt: StmtId) -> Option<&'a OpFacts> {
        self.op_name(stmt).and_then(|n| self.k.ops.get(n))
    }

    fn diag(&mut self, tok: TokIdx, severity: Severity, message: String) {
        let t = self.a.tree.tok(tok);
        self.a.diagnostics.push(Diagnostic {
            start: t.start,
            end: t.end,
            severity,
            message,
            kind: ErrorKind::Generic,
        });
    }

    fn add_def(&mut self, def: Def) -> DefId {
        let id = DefId(self.a.defs.len() as u32);
        self.a.roles[def.tok as usize] = Role::Def(id);
        self.a.defs.push(def);
        self.a.uses.push(Vec::new());
        id
    }

    fn run(mut self) -> Analysis {
        self.syntax_diagnostics();
        // Scopes, top-down. Statements are created in pre-order, so a
        // region's owner always has a smaller id than the region's ops.
        for i in 0..self.a.tree.stmts.len() {
            let sid = StmtId(i as u32);
            let isolated = self.is_isolated(sid);
            self.a.stmt_isolated[i] = isolated;
            let scope = match self.a.tree.stmt_region(sid) {
                Some(r) => self.a.region_scope[r.0 as usize],
                None => 0,
            };
            self.a.stmt_scope[i] = scope;
            for r in self.a.tree.stmt(sid).regions.clone() {
                let rs = if isolated {
                    let id = self.a.scope_parent.len() as u32;
                    self.a.scope_parent.push(Some(scope));
                    id
                } else {
                    scope
                };
                self.a.region_scope[r.0 as usize] = rs;
            }
        }
        self.collect_defs();
        self.resolve_uses();
        self.a
    }

    fn syntax_diagnostics(&mut self) {
        let mut diags = Vec::new();
        for e in &self.a.tree.lex_errors {
            diags.push(Diagnostic {
                start: e.start,
                end: e.end,
                severity: Severity::Error,
                message: e.message.clone(),
                kind: ErrorKind::Generic,
            });
        }
        for e in &self.a.tree.errors {
            diags.push(Diagnostic {
                start: e.start,
                end: e.end,
                severity: if e.warning {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                message: e.message.clone(),
                kind: e.kind.clone(),
            });
        }
        self.a.diagnostics.extend(diags);
    }

    /// The first body token, if it is a `@symbol`.
    fn leading_symbol(&self, sid: StmtId) -> Option<TokIdx> {
        let s = self.a.tree.stmt(sid);
        let first = *s.body.first()?;
        (self.a.tree.tok(first).kind == TokenKind::SymbolRef).then_some(first)
    }

    fn is_isolated(&self, sid: StmtId) -> bool {
        if let Some(i) = self.facts(sid).and_then(|f| f.isolated) {
            return i;
        }
        let s = self.a.tree.stmt(sid);
        if s.parent_block.is_none() {
            return true;
        }
        self.leading_symbol(sid).is_some() && s.results.is_empty() && !s.regions.is_empty()
    }

    fn defines_symbol(&self, sid: StmtId) -> Option<TokIdx> {
        let lead = self.leading_symbol(sid);
        match self.facts(sid).and_then(|f| f.symbol) {
            Some(true) => lead,
            Some(false) => None,
            None => lead.filter(|_| self.a.tree.stmt(sid).results.is_empty()),
        }
    }

    fn collect_defs(&mut self) {
        // Outlined entries.
        if let Some(o) = self.a.tree.outlined.clone() {
            for e in &o.entries {
                if self.outline.contains_key(&e.index) {
                    self.diag(
                        e.index_tok,
                        Severity::Error,
                        format!("duplicate outlined attribute entry `!{}`", e.index),
                    );
                    continue;
                }
                let id = self.add_def(Def {
                    kind: DefKind::Outline,
                    tok: e.index_tok,
                    name: e.index.to_string(),
                    stmt: None,
                    block: None,
                    index: e.index,
                    ty: None,
                });
                self.outline.insert(e.index, id);
            }
        }

        // Block labels and arguments.
        for bi in 0..self.a.tree.blocks.len() {
            let bid = BlockId(bi as u32);
            let b = self.a.tree.block(bid).clone();
            if self.a.tree.tok(b.label).kind == TokenKind::BlockLabel {
                let name = self.name(b.label).to_string();
                let key = (b.region.0, name.clone());
                let id = self.add_def(Def {
                    kind: DefKind::Label,
                    tok: b.label,
                    name: name.clone(),
                    stmt: None,
                    block: Some(bid),
                    index: 0,
                    ty: None,
                });
                if self.labels.contains_key(&key) {
                    self.diag(
                        b.label,
                        Severity::Error,
                        format!("block label `^{name}` is defined more than once in this region"),
                    );
                }
                self.labels.entry(key).or_default().push(id);
            }
            let scope = self.a.region_scope[b.region.0 as usize];
            for (i, arg) in b.args.iter().enumerate() {
                let ty = arg.ty.map(|(s, e)| TypeSpan {
                    start: self.a.tree.tok(s).start,
                    end: self.a.tree.tok(e).end,
                    provenance: Provenance::Exact,
                });
                self.value_def(DefKind::BlockArg, arg.name, None, Some(bid), i as u32, scope, ty);
            }
        }

        // Results and symbols.
        for si in 0..self.a.tree.stmts.len() {
            let sid = StmtId(si as u32);
            let s = self.a.tree.stmt(sid).clone();
            let scope = self.a.stmt_scope[si];
            let types = self.result_types(sid);
            for (i, r) in s.results.iter().enumerate() {
                let ty = types.get(i).copied().flatten();
                self.value_def(DefKind::Result, *r, Some(sid), None, i as u32, scope, ty);
            }
            if let Some(sym) = self.defines_symbol(sid) {
                let name = self.name(sym).to_string();
                let table = self.a.tree.stmt_region(sid).map(|r| r.0).unwrap_or(TOP);
                let key = (table, name.clone());
                let id = self.add_def(Def {
                    kind: DefKind::Symbol,
                    tok: sym,
                    name: name.clone(),
                    stmt: Some(sid),
                    block: None,
                    index: 0,
                    ty: None,
                });
                if self.symbols.contains_key(&key) {
                    self.diag(
                        sym,
                        Severity::Warning,
                        format!("symbol `@{name}` is defined more than once in this symbol table"),
                    );
                }
                self.symbols.entry(key).or_default().push(id);
                self.a.stmt_symbol[si] = Some(id);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn value_def(
        &mut self,
        kind: DefKind,
        tok: TokIdx,
        stmt: Option<StmtId>,
        block: Option<BlockId>,
        index: u32,
        scope: u32,
        ty: Option<TypeSpan>,
    ) {
        let name = self.text(tok).to_string();
        let key = (scope, name.clone());
        let id = self.add_def(Def {
            kind,
            tok,
            name: name.clone(),
            stmt,
            block,
            index,
            ty,
        });
        if self.values.contains_key(&key) {
            self.diag(
                tok,
                Severity::Error,
                format!("value `{name}` is defined more than once in this scope"),
            );
        }
        self.values.entry(key).or_default().push(id);
    }

    /// Syntactic result types of a statement, one entry per result.
    fn result_types(&self, sid: StmtId) -> Vec<Option<TypeSpan>> {
        let s = self.a.tree.stmt(sid);
        let n = s.results.len();
        if n == 0 {
            return Vec::new();
        }
        let toks = &self.a.tree.tokens;
        // Canonical form: `: <(operand types) -> (result types)>` at depth 0.
        for (k, &t) in s.body.iter().enumerate() {
            if s.body_depth[k] == 0
                && toks[t as usize].is_punct(':')
                && s.body.get(k + 1).is_some_and(|n| toks[*n as usize].is_punct('<'))
                && let Some(tys) = self.canonical_result_types(&s.body[k + 1..])
                    && tys.len() == n
                {
                    return tys.into_iter().map(Some).collect();
                }
        }
        if n != 1 {
            return vec![None; n];
        }
        // Trailing `: type` at depth 0.
        let Some(k) = (0..s.body.len())
            .rev()
            .find(|&k| s.body_depth[k] == 0 && toks[s.body[k] as usize].is_punct(':'))
        else {
            return vec![None];
        };
        let mut ty_toks: Vec<TokIdx> = s.body[k + 1..].to_vec();
        while ty_toks
            .last()
            .is_some_and(|t| toks[*t as usize].kind == TokenKind::OutlineRef)
        {
            ty_toks.pop();
        }
        let (Some(&first), Some(&last)) = (ty_toks.first(), ty_toks.last()) else {
            return vec![None];
        };
        let first_tok = toks[first as usize];
        if first_tok.kind != TokenKind::QualName {
            return vec![None];
        }
        let facts = self.facts(sid);
        let certain = facts.and_then(|f| f.trailing_result_type) == Some(true);
        if !certain {
            // Function-like types after `:` are usually a callee signature
            // (e.g. `llvm.call`), not the result type.
            let head = first_tok.text(self.src);
            let has_arrow = ty_toks
                .iter()
                .any(|t| toks[*t as usize].kind == TokenKind::Arrow);
            if has_arrow || head.ends_with(".func") || head.ends_with(".function") {
                return vec![None];
            }
            if facts.and_then(|f| f.trailing_result_type) == Some(false) {
                return vec![None];
            }
        }
        vec![Some(TypeSpan {
            start: first_tok.start,
            end: toks[last as usize].end,
            provenance: if certain {
                Provenance::Exact
            } else {
                Provenance::Heuristic
            },
        })]
    }

    /// Parse `<(a, b) -> (c, d)>` (tokens starting at `<`) into result types.
    fn canonical_result_types(&self, toks_idx: &[TokIdx]) -> Option<Vec<TypeSpan>> {
        let toks = &self.a.tree.tokens;
        let arrow = toks_idx
            .iter()
            .position(|t| toks[*t as usize].kind == TokenKind::Arrow)?;
        let rest = &toks_idx[arrow + 1..];
        if !rest.first().is_some_and(|t| toks[*t as usize].is_punct('(')) {
            return None;
        }
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut cur: Option<(Offset, Offset)> = None;
        for &t in &rest[1..] {
            let tok = toks[t as usize];
            match tok.kind {
                TokenKind::Punct('(' | '<' | '[' | '{') => depth += 1,
                TokenKind::Punct(')' | '>' | ']' | '}') if depth > 0 => depth -= 1,
                TokenKind::Punct(')') if depth == 0 => {
                    if let Some((s, e)) = cur.take() {
                        out.push(TypeSpan {
                            start: s,
                            end: e,
                            provenance: Provenance::Exact,
                        });
                    }
                    return Some(out);
                }
                TokenKind::Punct(',') if depth == 0 => {
                    if let Some((s, e)) = cur.take() {
                        out.push(TypeSpan {
                            start: s,
                            end: e,
                            provenance: Provenance::Exact,
                        });
                    }
                    continue;
                }
                _ => {}
            }
            cur = Some(match cur {
                Some((s, _)) => (s, tok.end),
                None => (tok.start, tok.end),
            });
        }
        None
    }

    fn resolve_uses(&mut self) {
        for si in 0..self.a.tree.stmts.len() {
            let sid = StmtId(si as u32);
            let s = self.a.tree.stmt(sid).clone();
            if let Some(op) = s.op_name {
                self.a.roles[op as usize] = Role::OpName;
            }
            let keywords: &[String] = self
                .facts(sid)
                .map(|f| f.keywords.as_slice())
                .unwrap_or_default();
            let contexts = self.word_contexts(sid);
            let region = self.a.tree.stmt_region(sid);
            for (k, &t) in s.body.iter().enumerate() {
                let tok = self.a.tree.tok(t);
                if self.a.roles[t as usize] != Role::None {
                    continue; // e.g. the symbol definition
                }
                match tok.kind {
                    TokenKind::QualName => self.a.roles[t as usize] = Role::TypeOrAttr,
                    TokenKind::Ident => {
                        let word = self.text(t);
                        let ctx = contexts[k];
                        if keywords.iter().any(|kw| kw == word) {
                            self.a.roles[t as usize] = Role::Keyword;
                        } else if ctx == WordCtx::AttrKey {
                            self.a.roles[t as usize] = Role::AttrKey;
                        } else {
                            let conf = match ctx {
                                WordCtx::Operand => Confidence::High,
                                WordCtx::Plain => Confidence::Medium,
                                _ => Confidence::Low,
                            };
                            self.resolve_value(sid, t, conf);
                        }
                    }
                    TokenKind::BlockLabel => self.resolve_label(region, t),
                    TokenKind::SymbolRef => self.resolve_symbol(region, t),
                    TokenKind::OutlineRef => self.resolve_outline(t),
                    _ => {}
                }
            }
        }
        for b in self.a.tree.blocks.clone() {
            if let Some(o) = b.outline_ref {
                self.resolve_outline(o);
            }
        }
    }

    fn add_use(&mut self, tok: TokIdx, def: DefId, conf: Confidence) {
        self.a.roles[tok as usize] = Role::Use(def, conf);
        self.a.uses[def.0 as usize].push(tok);
    }

    fn resolve_value(&mut self, sid: StmtId, tok: TokIdx, conf: Confidence) {
        let name = self.text(tok);
        let use_off = self.a.tree.tok(tok).start;
        let mut scope = Some(self.a.stmt_scope[sid.0 as usize]);
        let mut outer = false;
        while let Some(sc) = scope {
            if let Some(defs) = self.values.get(&(sc, name.to_string())) {
                // Prefer the nearest preceding definition.
                let def = defs
                    .iter()
                    .copied()
                    .filter(|d| self.a.tree.tok(self.a.def(*d).tok).start <= use_off)
                    .max_by_key(|d| self.a.tree.tok(self.a.def(*d).tok).start)
                    .unwrap_or(defs[0]);
                let conf = if outer { Confidence::Low } else { conf };
                self.add_use(tok, def, conf);
                return;
            }
            scope = self.a.scope_parent[sc as usize];
            outer = true;
        }
        self.a.roles[tok as usize] = Role::Word;
        if conf == Confidence::High {
            self.diag(tok, Severity::Error, format!("undefined value `{name}`"));
        }
    }

    fn resolve_label(&mut self, region: Option<RegionId>, tok: TokIdx) {
        let name = self.name(tok).to_string();
        let found = region.and_then(|r| self.labels.get(&(r.0, name.clone())).map(|d| d[0]));
        match found {
            Some(def) => self.add_use(tok, def, Confidence::High),
            None => {
                self.a.roles[tok as usize] = Role::Unresolved;
                self.diag(
                    tok,
                    Severity::Error,
                    format!("undefined block label `^{name}` in this region"),
                );
            }
        }
    }

    fn resolve_symbol(&mut self, mut region: Option<RegionId>, tok: TokIdx) {
        let name = self.name(tok).to_string();
        loop {
            let table = region.map(|r| r.0).unwrap_or(TOP);
            if let Some(defs) = self.symbols.get(&(table, name.clone())) {
                let d = defs[0];
                self.add_use(tok, d, Confidence::High);
                return;
            }
            match region {
                Some(r) => {
                    let owner = self.a.tree.region(r).owner;
                    region = self.a.tree.stmt_region(owner);
                }
                None => break,
            }
        }
        // Document-wide fallback (nested symbol tables).
        if let Some(d) = self
            .symbols
            .iter()
            .filter(|((_, n), _)| *n == name)
            .map(|(_, d)| d[0])
            .min()
        {
            self.add_use(tok, d, Confidence::Medium);
            return;
        }
        self.a.roles[tok as usize] = Role::Unresolved;
        self.diag(
            tok,
            Severity::Warning,
            format!("symbol `@{name}` is not defined in this document"),
        );
    }

    fn resolve_outline(&mut self, tok: TokIdx) {
        let idx: u32 = self.name(tok).parse().unwrap_or(u32::MAX);
        match self.outline.get(&idx) {
            Some(d) => {
                let d = *d;
                self.add_use(tok, d, Confidence::High);
            }
            None => {
                self.a.roles[tok as usize] = Role::Unresolved;
                if self.a.tree.outlined.is_some() {
                    self.diag(
                        tok,
                        Severity::Warning,
                        format!("no outlined attribute entry `!{idx}`"),
                    );
                }
            }
        }
    }

    /// Classify every body token of a statement.
    fn word_contexts(&self, sid: StmtId) -> Vec<WordCtx> {
        let s = self.a.tree.stmt(sid);
        let toks = &self.a.tree.tokens;
        let kind = |k: usize| toks[s.body[k] as usize].kind;
        let n = s.body.len();
        let mut ctx = vec![WordCtx::Plain; n];

        // Whole body is `a, b, c [: type...]`?
        let simple_end = (0..n)
            .find(|&k| s.body_depth[k] == 0 && kind(k) == TokenKind::Punct(':'))
            .unwrap_or(n);
        let simple = simple_end > 0
            && (0..simple_end).all(|k| {
                (k % 2 == 0 && kind(k) == TokenKind::Ident)
                    || (k % 2 == 1 && kind(k) == TokenKind::Punct(','))
            })
            && simple_end % 2 == 1;
        if simple {
            for c in ctx.iter_mut().take(simple_end) {
                *c = WordCtx::Operand;
            }
        }

        // Canonical operand list: `(` right after the op name.
        if n > 0 && kind(0) == TokenKind::Punct('(') {
            let mut k = 1;
            while k < n && !(s.body_depth[k] == 0 && kind(k) == TokenKind::Punct(')')) {
                if kind(k) == TokenKind::Ident && s.body_depth[k] == 1 {
                    ctx[k] = WordCtx::Operand;
                }
                k += 1;
            }
        }

        for k in 0..n {
            let t = kind(k);
            let prev = k.checked_sub(1).map(kind);
            let next = (k + 1 < n).then(|| kind(k + 1));
            // Successor arguments: `^bb(a, b)`.
            if t == TokenKind::BlockLabel && next == Some(TokenKind::Punct('(')) {
                let d = s.body_depth[k];
                let mut j = k + 2;
                while j < n && !(s.body_depth[j] == d + 1 && kind(j) == TokenKind::Punct(')')) {
                    if kind(j) == TokenKind::Ident && s.body_depth[j] == d + 1 {
                        ctx[j] = WordCtx::Operand;
                    }
                    j += 1;
                }
            }
            if t != TokenKind::Ident || ctx[k] == WordCtx::Operand {
                continue;
            }
            if s.body_depth[k] > 0
                && matches!(next, Some(TokenKind::Punct('=' | ':')))
                && matches!(prev, Some(TokenKind::Punct('{' | '[' | '<' | ',')))
            {
                ctx[k] = WordCtx::AttrKey;
            } else if prev == Some(TokenKind::QualName)
                || prev == Some(TokenKind::Punct('='))
                || (s.body_depth[k] > 0 && prev == Some(TokenKind::Punct(':')))
            {
                ctx[k] = WordCtx::TypeParam;
            }
        }
        ctx
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordCtx {
    /// Certainly an SSA operand.
    Operand,
    /// Unknown syntax; may be an operand.
    Plain,
    /// A word inside type/attribute syntax.
    TypeParam,
    /// `key` of `key = value`.
    AttrKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &str = r#"builtin.module @m {
  ^entry():
  llvm.func @callee: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {
    ^entry(a: builtin.integer i64):
    llvm.return a
  };
  llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {
    ^entry(x: builtin.integer i64):
    y = builtin.constant <builtin.integer <1: i64>> : builtin.integer i64;
    one = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1;
    llvm.cond_br if one ^bb0(x, y) else ^bb1(x, y)

    ^bb0(x0: builtin.integer i64, y0: builtin.integer i64):
    llvm.br ^bb2(y0, y0)

    ^bb1(x1: builtin.integer i64, y1: builtin.integer i64):
    llvm.br ^bb2(x1, y1)

    ^bb2(x2: builtin.integer i64, y2: builtin.integer i64):
    z = llvm.add x2, y2 <{nsw=false,nuw=false}> : builtin.integer i64;
    r = llvm.call @callee (z) : llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false>;
    llvm.return r
  }
}
"#;

    fn tok_at(a: &Analysis, src: &str, needle: &str, nth: usize) -> TokIdx {
        let off = src.match_indices(needle).nth(nth).unwrap().0 as Offset;
        a.tree.token_at(off).unwrap()
    }

    fn def_name(a: &Analysis, tok: TokIdx) -> Option<(DefKind, String)> {
        a.def_of_token(tok).map(|d| (a.def(d).kind, a.def(d).name.clone()))
    }

    #[test]
    fn demo_resolves_cleanly() {
        let a = analyze(DEMO, &Knowledge::default());
        assert!(a.diagnostics.is_empty(), "{:#?}", a.diagnostics);
        // `one` used in cond_br resolves to its definition.
        let use_one = tok_at(&a, DEMO, "one ^bb0", 0);
        assert_eq!(def_name(&a, use_one), Some((DefKind::Result, "one".into())));
        // `x` in successor args resolves to the entry block arg.
        let x_use = tok_at(&a, DEMO, "x, y)", 0);
        let d = a.def_of_token(x_use).unwrap();
        assert_eq!(a.def(d).kind, DefKind::BlockArg);
        assert!(matches!(a.role(x_use), Role::Use(_, Confidence::High)));
        // Labels.
        let bb2 = tok_at(&a, DEMO, "^bb2(y0", 0);
        assert_eq!(def_name(&a, bb2), Some((DefKind::Label, "bb2".into())));
        // Symbols: `@callee` in the call resolves to the func def.
        let callee_use = tok_at(&a, DEMO, "@callee (z)", 0);
        assert_eq!(def_name(&a, callee_use), Some((DefKind::Symbol, "callee".into())));
        let callee_def = tok_at(&a, DEMO, "@callee:", 0);
        assert!(matches!(a.role(callee_def), Role::Def(_)));
        // `nsw` is an attribute key, `false` is a plain word.
        let nsw = tok_at(&a, DEMO, "nsw", 0);
        assert_eq!(a.role(nsw), Role::AttrKey);
        // `i64` after a qualname is a word, not a value.
        let i64_tok = tok_at(&a, DEMO, "i64", 0);
        assert_eq!(a.role(i64_tok), Role::Word);
    }

    #[test]
    fn isolated_scopes_separate_names() {
        // `a` is defined in @callee and must not be visible in @f.
        let src = "builtin.module @m {\n^e():\n  t.f @g {\n  ^b(a: i):\n    t.r a\n  };\n  t.f @h {\n  ^b():\n    t.r a\n  }\n}";
        let k = Knowledge::default();
        let an = analyze(src, &k);
        // The second `t.r a`: `a` lives in a sibling isolated scope, which
        // is not an ancestor, so it stays unresolved.
        let second_a = tok_at(&an, src, "a\n  }\n}", 0);
        assert_eq!(an.role(second_a), Role::Word);
        let d = an.diagnostics.iter().find(|d| d.message.contains("undefined value"));
        assert!(d.is_some(), "{:#?}", an.diagnostics);
    }

    #[test]
    fn forward_references_in_graph_regions() {
        let src = "t.g @g {\n^b():\n  x = t.use y : i;\n  y = t.def : i\n}";
        let an = analyze(src, &Knowledge::default());
        let y_use = tok_at(&an, src, "y : i", 0);
        assert_eq!(def_name(&an, y_use), Some((DefKind::Result, "y".into())));
    }

    #[test]
    fn result_types() {
        let a = analyze(DEMO, &Knowledge::default());
        let z = a.def_of_token(tok_at(&a, DEMO, "z =", 0)).unwrap();
        let ty = a.def(z).ty.unwrap();
        assert_eq!(&DEMO[ty.start as usize..ty.end as usize], "builtin.integer i64");
        assert_eq!(ty.provenance, Provenance::Heuristic);
        // The call's trailing type is a function type: no guess.
        let r = a.def_of_token(tok_at(&a, DEMO, "r =", 0)).unwrap();
        assert!(a.def(r).ty.is_none());
        // Block args are exact.
        let x = a.def_of_token(tok_at(&a, DEMO, "x: builtin", 0)).unwrap();
        assert_eq!(a.def(x).ty.unwrap().provenance, Provenance::Exact);
    }

    #[test]
    fn canonical_form_types() {
        let src = "v0, v1 = test.dual_def () [] []: <() -> (builtin.integer si64, builtin.integer si32)>";
        let a = analyze(src, &Knowledge::default());
        let v1 = a.def_of_token(tok_at(&a, src, "v1", 0)).unwrap();
        let ty = a.def(v1).ty.unwrap();
        assert_eq!(&src[ty.start as usize..ty.end as usize], "builtin.integer si32");
        assert_eq!(ty.provenance, Provenance::Exact);
    }

    #[test]
    fn undefined_names_diagnosed() {
        let src = "builtin.module @m {\n^e():\n  x = t.a q, r;\n  t.br ^nowhere(x)\n}";
        let a = analyze(src, &Knowledge::default());
        let msgs: Vec<_> = a.diagnostics.iter().map(|d| d.message.clone()).collect();
        assert!(msgs.iter().any(|m| m.contains("undefined value `q`")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("^nowhere")), "{msgs:?}");
    }

    #[test]
    fn knowledge_keywords_and_symbols() {
        let src = "builtin.module @m {\n^e():\n  c = t.cmp if x : i\n}";
        let mut k = Knowledge::default();
        k.ops.insert(
            "t.cmp".into(),
            OpFacts {
                keywords: vec!["if".into()],
                ..OpFacts::default()
            },
        );
        let a = analyze(src, &k);
        assert_eq!(a.role(tok_at(&a, src, "if", 0)), Role::Keyword);
    }
}

//! An error-tolerant structural parser for pliron textual IR.
//!
//! It recognises what is common to every pliron document regardless of the
//! dialects involved:
//!
//! ```text
//! file      := stmt [outlined]
//! stmt      := [ident (',' ident)* '='] qualname body* (region body*)*
//! region    := '{' block* '}'
//! block     := '^' label '(' [arg (',' arg)*] ')' ['[' attrs ']'] ['!' N] ':' [stmt (';' stmt)*]
//! arg       := ident ':' type-tokens
//! outlined  := 'outlined_attributes' ':' ('!' N '=' tokens)*
//! ```
//!
//! The op body (everything after the op name) is dialect specific, so it is
//! kept as a flat list of tokens; nested regions are recognised (a `{` at
//! depth 0 followed by a block header or `}`) and parsed recursively.
//!
//! The parser never fails; problems are recorded in [`Tree::errors`] and
//! parsing resumes at the next statement/block boundary.

use crate::lexer::{LexError, Offset, Token, TokenKind, lex};

pub type TokIdx = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StmtId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegionId(pub u32);

#[derive(Clone, Debug, Default)]
pub struct Stmt {
    /// First token of the statement.
    pub first: TokIdx,
    /// Last token of the statement (inclusive), excluding a terminating `;`.
    pub last: TokIdx,
    /// Result name tokens (`Ident`).
    pub results: Vec<TokIdx>,
    /// The op name token (`QualName`), if present.
    pub op_name: Option<TokIdx>,
    /// The statement's own tokens after the op name, excluding nested regions.
    pub body: Vec<TokIdx>,
    /// Bracket depth of every body token (parallel to `body`).
    pub body_depth: Vec<u16>,
    pub regions: Vec<RegionId>,
    pub parent_block: Option<BlockId>,
    /// The `;` that follows this statement, if any.
    pub semicolon: Option<TokIdx>,
}

#[derive(Clone, Debug)]
pub struct BlockArg {
    pub name: TokIdx,
    /// Inclusive token range of the argument's type.
    pub ty: Option<(TokIdx, TokIdx)>,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub label: TokIdx,
    pub args: Vec<BlockArg>,
    /// The `(` and `)` of the argument list.
    pub args_parens: Option<(TokIdx, Option<TokIdx>)>,
    /// `!N` in the header.
    pub outline_ref: Option<TokIdx>,
    /// The `:` that ends the header.
    pub colon: Option<TokIdx>,
    pub stmts: Vec<StmtId>,
    pub region: RegionId,
    /// Last token of the block (its last statement, or the header).
    pub last: TokIdx,
}

#[derive(Clone, Debug)]
pub struct Region {
    pub open: TokIdx,
    pub close: Option<TokIdx>,
    pub blocks: Vec<BlockId>,
    pub owner: StmtId,
}

#[derive(Clone, Debug)]
pub struct OutlineEntry {
    /// The `!N` token.
    pub index_tok: TokIdx,
    pub index: u32,
    pub first: TokIdx,
    pub last: TokIdx,
    /// Byte range of a leading `@[ ... ]` location (plus its trailing comma),
    /// which must be masked before handing the text to pliron.
    pub loc: Option<(Offset, Offset)>,
}

#[derive(Clone, Debug)]
pub struct OutlinedSection {
    /// The `outlined_attributes` token.
    pub header: TokIdx,
    pub entries: Vec<OutlineEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Generic,
    /// A `;` is missing; inserting one at `insert_at` fixes it.
    MissingSemicolon {
        insert_at: Offset,
    },
    /// A bracket is never closed; inserting `closer` at `insert_at` fixes it.
    Unclosed {
        closer: char,
        insert_at: Offset,
    },
    /// A `;` after the last operation of a block (rejected by pliron 0.18).
    TrailingSemicolon,
    /// Text that pliron ignores.
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    pub start: Offset,
    pub end: Offset,
    pub message: String,
    pub kind: ErrorKind,
    /// Warnings are reported with a lower severity.
    pub warning: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub tokens: Vec<Token>,
    pub stmts: Vec<Stmt>,
    pub blocks: Vec<Block>,
    pub regions: Vec<Region>,
    /// Top-level statements. pliron parses only the first one.
    pub top: Vec<StmtId>,
    pub outlined: Option<OutlinedSection>,
    pub errors: Vec<SyntaxError>,
    pub lex_errors: Vec<LexError>,
}

impl Tree {
    pub fn stmt(&self, id: StmtId) -> &Stmt {
        &self.stmts[id.0 as usize]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }

    pub fn region(&self, id: RegionId) -> &Region {
        &self.regions[id.0 as usize]
    }

    pub fn tok(&self, idx: TokIdx) -> Token {
        self.tokens[idx as usize]
    }

    pub fn stmt_ids(&self) -> impl Iterator<Item = StmtId> + '_ {
        (0..self.stmts.len() as u32).map(StmtId)
    }

    pub fn block_ids(&self) -> impl Iterator<Item = BlockId> + '_ {
        (0..self.blocks.len() as u32).map(BlockId)
    }

    pub fn region_ids(&self) -> impl Iterator<Item = RegionId> + '_ {
        (0..self.regions.len() as u32).map(RegionId)
    }

    /// Byte range covered by a statement (including nested regions).
    pub fn stmt_range(&self, id: StmtId) -> (Offset, Offset) {
        let s = self.stmt(id);
        (self.tok(s.first).start, self.tok(s.last).end)
    }

    /// Byte range covered by a block (header + statements).
    pub fn block_range(&self, id: BlockId) -> (Offset, Offset) {
        let b = self.block(id);
        (self.tok(b.label).start, self.tok(b.last).end)
    }

    /// Byte range of a region including braces.
    pub fn region_range(&self, id: RegionId) -> (Offset, Offset) {
        let r = self.region(id);
        let end = match r.close {
            Some(c) => self.tok(c).end,
            None => r
                .blocks
                .last()
                .map(|b| self.tok(self.block(*b).last).end)
                .unwrap_or(self.tok(r.open).end),
        };
        (self.tok(r.open).start, end)
    }

    /// The region containing a statement, if it is nested.
    pub fn stmt_region(&self, id: StmtId) -> Option<RegionId> {
        self.stmt(id).parent_block.map(|b| self.block(b).region)
    }

    /// The statement owning the region that contains `id`.
    pub fn parent_stmt(&self, id: StmtId) -> Option<StmtId> {
        self.stmt_region(id).map(|r| self.region(r).owner)
    }

    /// Index of the first token that starts at or after `offset`.
    pub fn token_at_or_after(&self, offset: Offset) -> usize {
        self.tokens.partition_point(|t| t.end <= offset)
    }

    /// The token containing (or ending exactly at) `offset`. Prefers a
    /// token that contains the offset over one that ends at it.
    pub fn token_at(&self, offset: Offset) -> Option<TokIdx> {
        let i = self.token_at_or_after(offset);
        if let Some(t) = self.tokens.get(i)
            && t.contains(offset)
        {
            return Some(i as TokIdx);
        }
        if i > 0 && self.tokens[i - 1].end == offset {
            return Some(i as TokIdx - 1);
        }
        None
    }

    /// The innermost statement whose range contains `offset`.
    pub fn stmt_at(&self, offset: Offset) -> Option<StmtId> {
        self.stmt_ids()
            .filter(|id| {
                let (s, e) = self.stmt_range(*id);
                s <= offset && offset <= e
            })
            .min_by_key(|id| {
                let (s, e) = self.stmt_range(*id);
                e - s
            })
    }
}

pub fn parse(src: &str) -> Tree {
    let (tokens, lex_errors) = lex(src);
    let mut p = Parser {
        src,
        tree: Tree {
            tokens,
            lex_errors,
            ..Tree::default()
        },
        pos: 0,
    };
    p.file();
    p.tree
}

struct Parser<'a> {
    src: &'a str,
    tree: Tree,
    pos: usize,
}

const OUTLINED_HEADER: &str = "outlined_attributes";

impl Parser<'_> {
    fn at_end(&self) -> bool {
        self.pos >= self.tree.tokens.len()
    }

    fn kind_at(&self, i: usize) -> Option<TokenKind> {
        self.tree.tokens.get(i).map(|t| t.kind)
    }

    fn is_punct_at(&self, i: usize, c: char) -> bool {
        self.kind_at(i) == Some(TokenKind::Punct(c))
    }

    fn text_at(&self, i: usize) -> &str {
        self.tree.tokens[i].text(self.src)
    }

    fn error(&mut self, start: Offset, end: Offset, message: impl Into<String>, kind: ErrorKind) {
        self.tree.errors.push(SyntaxError {
            start,
            end,
            message: message.into(),
            kind,
            warning: false,
        });
    }

    fn warning(&mut self, start: Offset, end: Offset, message: impl Into<String>, kind: ErrorKind) {
        self.tree.errors.push(SyntaxError {
            start,
            end,
            message: message.into(),
            kind,
            warning: true,
        });
    }

    fn error_at_tok(&mut self, i: usize, message: impl Into<String>) {
        let (s, e) = match self.tree.tokens.get(i) {
            Some(t) => (t.start, t.end),
            None => {
                let end = self.src.len() as Offset;
                (end, end)
            }
        };
        self.error(s, e, message, ErrorKind::Generic);
    }

    /// Does token `i` start a new line (only whitespace before it)?
    fn starts_line(&self, i: usize) -> bool {
        let start = self.tree.tokens[i].start as usize;
        let line_start = self.src[..start].rfind('\n').map(|p| p + 1).unwrap_or(0);
        self.src[line_start..start].trim().is_empty()
    }

    /// `ident (, ident)* =` starting at `i`; returns the index of `=`.
    fn results_pattern(&self, i: usize) -> Option<usize> {
        let mut j = i;
        loop {
            if self.kind_at(j) != Some(TokenKind::Ident) {
                return None;
            }
            j += 1;
            if self.is_punct_at(j, ',') {
                j += 1;
                continue;
            }
            if self.is_punct_at(j, '=') {
                return Some(j);
            }
            return None;
        }
    }

    /// Does a statement start at `i`?
    fn is_stmt_start(&self, i: usize) -> bool {
        match self.kind_at(i) {
            Some(TokenKind::QualName) => true,
            Some(TokenKind::Ident) => self
                .results_pattern(i)
                .is_some_and(|eq| self.kind_at(eq + 1) == Some(TokenKind::QualName)),
            _ => false,
        }
    }

    /// Like [`Parser::is_stmt_start`] but also accepts `x = <not a qualname>`
    /// (an incomplete statement while typing).
    fn is_loose_stmt_start(&self, i: usize) -> bool {
        self.is_stmt_start(i) || self.results_pattern(i).is_some()
    }

    /// Skip a balanced group starting at the opener at `i`; returns the
    /// index of the matching closer (or `None` if unclosed).
    fn skip_group(&self, i: usize) -> Option<usize> {
        let mut stack = Vec::new();
        let mut j = i;
        while let Some(t) = self.tree.tokens.get(j) {
            match t.kind {
                TokenKind::Punct(c @ ('(' | '[' | '{' | '<')) => stack.push(c),
                TokenKind::Punct(c @ (')' | ']' | '}' | '>')) => {
                    let open = matching_open(c);
                    if stack.last() == Some(&open) {
                        stack.pop();
                        if stack.is_empty() {
                            return Some(j);
                        }
                    } else if c != '>' {
                        return None;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        None
    }

    /// Is there a block header (`^l(...) [..] [!N] :`) at `i`?
    fn is_block_header(&self, i: usize) -> bool {
        if self.kind_at(i) != Some(TokenKind::BlockLabel) {
            return false;
        }
        let mut j = i + 1;
        if self.is_punct_at(j, '(') {
            match self.skip_group(j) {
                Some(close) => j = close + 1,
                None => return false,
            }
        }
        if self.is_punct_at(j, '[') {
            match self.skip_group(j) {
                Some(close) => j = close + 1,
                None => return false,
            }
        }
        if self.kind_at(j) == Some(TokenKind::OutlineRef) {
            j += 1;
        }
        self.is_punct_at(j, ':')
    }

    fn is_outlined_header(&self, i: usize) -> bool {
        self.kind_at(i) == Some(TokenKind::Ident)
            && self.text_at(i) == OUTLINED_HEADER
            && self.is_punct_at(i + 1, ':')
    }

    fn file(&mut self) {
        while !self.at_end() {
            if !self.tree.top.is_empty() && self.is_outlined_header(self.pos) {
                self.outlined_section();
                break;
            }
            if self.is_loose_stmt_start(self.pos) {
                let start_tok = self.pos;
                let id = self.stmt(None);
                if !self.tree.top.is_empty() {
                    let (s, e) = self.tree.stmt_range(id);
                    let _ = start_tok;
                    self.warning(
                        s,
                        e,
                        "pliron parses a single top-level operation; this is ignored",
                        ErrorKind::Ignored,
                    );
                }
                self.tree.top.push(id);
                continue;
            }
            if self.is_punct_at(self.pos, ';') {
                self.pos += 1;
                continue;
            }
            let i = self.pos;
            if self.tree.top.is_empty() {
                self.error_at_tok(i, "expected an operation");
            } else {
                let t = self.tree.tokens[i];
                self.warning(
                    t.start,
                    t.end,
                    "text after the top-level operation is ignored by pliron",
                    ErrorKind::Ignored,
                );
            }
            self.pos += 1;
        }
    }

    fn stmt(&mut self, parent_block: Option<BlockId>) -> StmtId {
        let id = StmtId(self.tree.stmts.len() as u32);
        let first = self.pos as TokIdx;
        self.tree.stmts.push(Stmt {
            first,
            last: first,
            parent_block,
            ..Stmt::default()
        });

        let mut results = Vec::new();
        if let Some(eq) = self.results_pattern(self.pos) {
            let mut j = self.pos;
            while j < eq {
                if self.kind_at(j) == Some(TokenKind::Ident) {
                    results.push(j as TokIdx);
                }
                j += 1;
            }
            self.pos = eq + 1;
        }

        let mut op_name = None;
        if self.kind_at(self.pos) == Some(TokenKind::QualName) {
            op_name = Some(self.pos as TokIdx);
            self.pos += 1;
        } else {
            self.error_at_tok(self.pos, "expected an operation name (`dialect.op`)");
        }

        let mut body = Vec::new();
        let mut body_depth = Vec::new();
        let mut regions = Vec::new();
        let mut semicolon = None;
        let mut stack: Vec<(char, usize)> = Vec::new();
        let mut last = self.pos.saturating_sub(1);

        while let Some(tok) = self.tree.tokens.get(self.pos).copied() {
            let depth0 = stack.is_empty();
            match tok.kind {
                TokenKind::Punct(';') if depth0 => {
                    semicolon = Some(self.pos as TokIdx);
                    self.pos += 1;
                    break;
                }
                TokenKind::Punct('}') if depth0 => break,
                TokenKind::Punct('{') if depth0 && self.looks_like_region(self.pos) => {
                    let r = self.region(id);
                    regions.push(r);
                    last = self.pos - 1;
                    continue;
                }
                TokenKind::BlockLabel if depth0 && self.is_block_header(self.pos) => break,
                _ if depth0
                    && op_name.is_some()
                    && self.pos > first as usize
                    && self.starts_line(self.pos)
                    && (self.is_outlined_header(self.pos)
                        || (self.kind_at(self.pos) == Some(TokenKind::Ident)
                            && self.is_stmt_start(self.pos))) =>
                {
                    if !self.is_outlined_header(self.pos) {
                        let prev = self.tree.tokens[last];
                        self.error(
                            prev.end,
                            prev.end,
                            "missing `;` between operations",
                            ErrorKind::MissingSemicolon {
                                insert_at: prev.end,
                            },
                        );
                    }
                    break;
                }
                TokenKind::Punct(c @ ('(' | '[' | '{' | '<')) => {
                    body.push(self.pos as TokIdx);
                    body_depth.push(stack.len() as u16);
                    stack.push((c, self.pos));
                }
                TokenKind::Punct(c @ (')' | ']' | '}' | '>')) => {
                    let open = matching_open(c);
                    if let Some(k) = stack.iter().rposition(|(o, _)| *o == open) {
                        // Unclosed `<` in between are tolerated silently;
                        // other unclosed brackets are reported.
                        for (o, at) in stack.drain(k + 1..).collect::<Vec<_>>() {
                            if o != '<' {
                                let t = self.tree.tokens[at];
                                self.error(
                                    t.start,
                                    t.end,
                                    format!("unclosed `{o}`"),
                                    ErrorKind::Unclosed {
                                        closer: matching_close(o),
                                        insert_at: tok.start,
                                    },
                                );
                            }
                        }
                        stack.pop();
                    } else if c != '>' {
                        self.error(
                            tok.start,
                            tok.end,
                            format!("unmatched `{c}`"),
                            ErrorKind::Generic,
                        );
                    }
                    body_depth.push(stack.len() as u16);
                    body.push(self.pos as TokIdx);
                }
                _ => {
                    body.push(self.pos as TokIdx);
                    body_depth.push(stack.len() as u16);
                }
            }
            last = self.pos;
            self.pos += 1;
        }

        for (o, at) in stack {
            if o != '<' {
                let t = self.tree.tokens[at];
                let insert_at = self.tree.tokens[last].end;
                self.error(
                    t.start,
                    t.end,
                    format!("unclosed `{o}`"),
                    ErrorKind::Unclosed {
                        closer: matching_close(o),
                        insert_at,
                    },
                );
            }
        }

        let last = (last as TokIdx).max(first);
        let s = &mut self.tree.stmts[id.0 as usize];
        s.results = results;
        s.op_name = op_name;
        s.body = body;
        s.body_depth = body_depth;
        s.regions = regions;
        s.semicolon = semicolon;
        s.last = last;
        id
    }

    /// At a `{` (depth 0 in a statement): is this a region?
    fn looks_like_region(&self, i: usize) -> bool {
        match self.kind_at(i + 1) {
            Some(TokenKind::Punct('}')) => true,
            Some(TokenKind::BlockLabel) => self.is_block_header(i + 1),
            _ => self.is_stmt_start(i + 1),
        }
    }

    fn region(&mut self, owner: StmtId) -> RegionId {
        let id = RegionId(self.tree.regions.len() as u32);
        let open = self.pos as TokIdx;
        self.tree.regions.push(Region {
            open,
            close: None,
            blocks: Vec::new(),
            owner,
        });
        self.pos += 1;
        let mut blocks = Vec::new();
        loop {
            if self.at_end() {
                let t = self.tree.tokens[open as usize];
                let end = self.src.len() as Offset;
                self.error(
                    t.start,
                    t.end,
                    "unclosed region `{`",
                    ErrorKind::Unclosed {
                        closer: '}',
                        insert_at: end,
                    },
                );
                break;
            }
            if self.is_punct_at(self.pos, '}') {
                self.tree.regions[id.0 as usize].close = Some(self.pos as TokIdx);
                self.pos += 1;
                break;
            }
            if self.kind_at(self.pos) == Some(TokenKind::BlockLabel) {
                blocks.push(self.block(id));
                continue;
            }
            if self.is_loose_stmt_start(self.pos) {
                // Operations without a block header: pliron requires one.
                let t = self.tree.tokens[self.pos];
                self.error(
                    t.start,
                    t.end,
                    "expected a block header (`^label():`) before operations",
                    ErrorKind::Generic,
                );
                blocks.push(self.headerless_block(id));
                continue;
            }
            self.error_at_tok(self.pos, "expected a block (`^label(...):`) or `}`");
            self.pos += 1;
        }
        self.tree.regions[id.0 as usize].blocks = blocks;
        id
    }

    fn block(&mut self, region: RegionId) -> BlockId {
        let id = BlockId(self.tree.blocks.len() as u32);
        let label = self.pos as TokIdx;
        self.pos += 1;
        let mut args = Vec::new();
        let mut args_parens = None;
        if self.is_punct_at(self.pos, '(') {
            let open = self.pos as TokIdx;
            self.pos += 1;
            let mut close = None;
            loop {
                if self.at_end() {
                    break;
                }
                if self.is_punct_at(self.pos, ')') {
                    close = Some(self.pos as TokIdx);
                    self.pos += 1;
                    break;
                }
                if self.kind_at(self.pos) == Some(TokenKind::Ident) {
                    let name = self.pos as TokIdx;
                    self.pos += 1;
                    let mut ty = None;
                    if self.is_punct_at(self.pos, ':') {
                        self.pos += 1;
                        ty = self.type_tokens_until(&[',', ')']);
                    } else {
                        self.error_at_tok(
                            self.pos,
                            "expected `:` and a type after block argument name",
                        );
                    }
                    args.push(BlockArg { name, ty });
                    if self.is_punct_at(self.pos, ',') {
                        self.pos += 1;
                    }
                    continue;
                }
                if self.is_punct_at(self.pos, ':')
                    || self.is_punct_at(self.pos, '{')
                    || self.is_punct_at(self.pos, '}')
                    || self.is_stmt_start(self.pos)
                {
                    break;
                }
                self.error_at_tok(self.pos, "expected a block argument (`name: type`)");
                self.pos += 1;
            }
            if close.is_none() {
                let t = self.tree.tokens[open as usize];
                let insert_at = self.tree.tokens[self.pos.saturating_sub(1)].end;
                self.error(
                    t.start,
                    t.end,
                    "unclosed `(` in block header",
                    ErrorKind::Unclosed {
                        closer: ')',
                        insert_at,
                    },
                );
            }
            args_parens = Some((open, close));
        } else {
            let t = self.tree.tokens[label as usize];
            self.error(
                t.end,
                t.end,
                "expected `(` after block label (use `()` for no arguments)",
                ErrorKind::Generic,
            );
        }
        if self.is_punct_at(self.pos, '[')
            && let Some(close) = self.skip_group(self.pos)
        {
            self.pos = close + 1;
        }
        let mut outline_ref = None;
        if self.kind_at(self.pos) == Some(TokenKind::OutlineRef) {
            outline_ref = Some(self.pos as TokIdx);
            self.pos += 1;
        }
        let mut colon = None;
        if self.is_punct_at(self.pos, ':') {
            colon = Some(self.pos as TokIdx);
            self.pos += 1;
        } else {
            let prev = self.tree.tokens[self.pos.saturating_sub(1)];
            self.error(
                prev.end,
                prev.end,
                "expected `:` after block header",
                ErrorKind::Generic,
            );
        }
        let header_last = (self.pos.saturating_sub(1)) as TokIdx;
        self.tree.blocks.push(Block {
            label,
            args,
            args_parens,
            outline_ref,
            colon,
            stmts: Vec::new(),
            region,
            last: header_last,
        });
        let stmts = self.block_body(id);
        let b = &mut self.tree.blocks[id.0 as usize];
        b.stmts = stmts;
        if let Some(&last_stmt) = b.stmts.last() {
            let s = &self.tree.stmts[last_stmt.0 as usize];
            b.last = s.semicolon.unwrap_or(s.last).max(header_last);
        }
        id
    }

    /// A block for operations that are missing their header (recovery).
    fn headerless_block(&mut self, region: RegionId) -> BlockId {
        let id = BlockId(self.tree.blocks.len() as u32);
        let label = self.pos as TokIdx;
        self.tree.blocks.push(Block {
            label,
            args: Vec::new(),
            args_parens: None,
            outline_ref: None,
            colon: None,
            stmts: Vec::new(),
            region,
            last: label,
        });
        let stmts = self.block_body(id);
        let b = &mut self.tree.blocks[id.0 as usize];
        b.stmts = stmts;
        if let Some(&last_stmt) = b.stmts.last() {
            let s = &self.tree.stmts[last_stmt.0 as usize];
            b.last = s.semicolon.unwrap_or(s.last);
        }
        id
    }

    fn block_body(&mut self, block: BlockId) -> Vec<StmtId> {
        let mut stmts = Vec::new();
        loop {
            if self.at_end() || self.is_punct_at(self.pos, '}') || self.is_block_header(self.pos) {
                break;
            }
            if self.is_loose_stmt_start(self.pos) {
                stmts.push(self.stmt(Some(block)));
                continue;
            }
            if self.is_punct_at(self.pos, ';') {
                self.error_at_tok(self.pos, "unexpected `;`");
                self.pos += 1;
                continue;
            }
            if self.kind_at(self.pos) == Some(TokenKind::BlockLabel) {
                // A label that is not a well-formed header: parse it as a
                // (broken) block header in the enclosing region.
                break;
            }
            self.error_at_tok(self.pos, "expected an operation");
            self.pos += 1;
        }
        // A `;` after the last operation of a block is rejected by pliron.
        if let Some(&last) = stmts.last()
            && let Some(semi) = self.tree.stmts[last.0 as usize].semicolon
        {
            let next_ends_block = self.at_end()
                || self.is_punct_at(self.pos, '}')
                || self.kind_at(self.pos) == Some(TokenKind::BlockLabel);
            if next_ends_block {
                let t = self.tree.tokens[semi as usize];
                self.warning(
                    t.start,
                    t.end,
                    "`;` separates operations; pliron rejects it after the last operation of a block",
                    ErrorKind::TrailingSemicolon,
                );
            }
        }
        stmts
    }

    /// Collect type tokens until one of `stops` at depth 0. Returns the
    /// inclusive token range, or `None` if empty.
    fn type_tokens_until(&mut self, stops: &[char]) -> Option<(TokIdx, TokIdx)> {
        let start = self.pos;
        let mut depth = 0i32;
        while let Some(t) = self.tree.tokens.get(self.pos) {
            match t.kind {
                TokenKind::Punct(c) if depth == 0 && stops.contains(&c) => break,
                TokenKind::Punct('(' | '[' | '{' | '<') => depth += 1,
                TokenKind::Punct(')' | ']' | '}' | '>') => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.pos += 1;
        }
        (self.pos > start).then(|| (start as TokIdx, (self.pos - 1) as TokIdx))
    }

    fn outlined_section(&mut self) {
        let header = self.pos as TokIdx;
        self.pos += 2; // `outlined_attributes` `:`
        let mut entries = Vec::new();
        while !self.at_end() {
            let is_entry_start = |p: &Self, i: usize| {
                p.kind_at(i) == Some(TokenKind::OutlineRef) && p.is_punct_at(i + 1, '=')
            };
            if !is_entry_start(self, self.pos) {
                let t = self.tree.tokens[self.pos];
                self.error(
                    t.start,
                    t.end,
                    "expected an outlined attribute entry (`!N = ...`)",
                    ErrorKind::Generic,
                );
                self.pos += 1;
                continue;
            }
            let index_tok = self.pos as TokIdx;
            let index = self.text_at(self.pos)[1..].parse().unwrap_or(u32::MAX);
            self.pos += 2;
            let mut loc = None;
            if self.is_punct_at(self.pos, '@') && self.is_punct_at(self.pos + 1, '[') {
                let at = self.tree.tokens[self.pos].start;
                if let Some(close) = self.skip_group(self.pos + 1) {
                    let mut end_tok = close;
                    if self.is_punct_at(close + 1, ',') {
                        end_tok = close + 1;
                    }
                    loc = Some((at, self.tree.tokens[end_tok].end));
                }
            }
            let mut j = self.pos;
            while j < self.tree.tokens.len() && !(is_entry_start(self, j) && self.starts_line(j)) {
                j += 1;
            }
            let last = (j.max(self.pos + 1) - 1) as TokIdx;
            entries.push(OutlineEntry {
                index_tok,
                index,
                first: index_tok,
                last: last.max(index_tok + 1),
                loc,
            });
            self.pos = j;
        }
        self.tree.outlined = Some(OutlinedSection { header, entries });
    }
}

fn matching_open(c: char) -> char {
    match c {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        '>' => '<',
        _ => c,
    }
}

fn matching_close(c: char) -> char {
    match c {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &str = r#"builtin.module @m {
  ^entry():
  llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {
    ^entry(x: builtin.integer i64):
    y = builtin.constant <builtin.integer <1: i64>> : builtin.integer i64;
    llvm.cond_br if y ^bb0(x) else ^bb1(x)

    ^bb0(a: builtin.integer i64):
    llvm.return a

    ^bb1(b: builtin.integer i64):
    z = llvm.add b, y <{nsw=false,nuw=false}> : builtin.integer i64;
    llvm.return z
  }
}
"#;

    fn names(tree: &Tree, src: &str, toks: &[TokIdx]) -> Vec<String> {
        toks.iter()
            .map(|t| tree.tok(*t).text(src).to_string())
            .collect()
    }

    #[test]
    fn demo_structure() {
        let tree = parse(DEMO);
        assert!(tree.errors.is_empty(), "{:?}", tree.errors);
        assert_eq!(tree.top.len(), 1);
        let module = tree.stmt(tree.top[0]);
        assert_eq!(
            tree.tok(module.op_name.unwrap()).text(DEMO),
            "builtin.module"
        );
        assert_eq!(module.regions.len(), 1);
        let mregion = tree.region(module.regions[0]);
        assert_eq!(mregion.blocks.len(), 1);
        let mblock = tree.block(mregion.blocks[0]);
        assert_eq!(tree.tok(mblock.label).text(DEMO), "^entry");
        assert_eq!(mblock.stmts.len(), 1);
        let func = tree.stmt(mblock.stmts[0]);
        assert_eq!(tree.tok(func.op_name.unwrap()).text(DEMO), "llvm.func");
        let fregion = tree.region(func.regions[0]);
        assert_eq!(fregion.blocks.len(), 3);
        let entry = tree.block(fregion.blocks[0]);
        assert_eq!(
            names(
                &tree,
                DEMO,
                &entry.args.iter().map(|a| a.name).collect::<Vec<_>>()
            ),
            ["x"]
        );
        let (s, e) = entry.args[0].ty.unwrap();
        assert_eq!(
            &DEMO[tree.tok(s).start as usize..tree.tok(e).end as usize],
            "builtin.integer i64"
        );
        assert_eq!(entry.stmts.len(), 2);
        let c = tree.stmt(entry.stmts[0]);
        assert_eq!(names(&tree, DEMO, &c.results), ["y"]);
        assert!(c.semicolon.is_some());
        let br = tree.stmt(entry.stmts[1]);
        assert!(br.semicolon.is_none());
        let bb1 = tree.block(fregion.blocks[2]);
        assert_eq!(bb1.stmts.len(), 2);
    }

    #[test]
    fn missing_semicolon_recovers() {
        let src = "builtin.module @m {\n^e():\n  a = t.c 1\n  b = t.c 2;\n  t.r b\n}\n";
        let tree = parse(src);
        assert_eq!(tree.errors.len(), 1, "{:?}", tree.errors);
        assert!(matches!(
            tree.errors[0].kind,
            ErrorKind::MissingSemicolon { .. }
        ));
        let block = tree.block(tree.region(tree.stmt(tree.top[0]).regions[0]).blocks[0]);
        assert_eq!(block.stmts.len(), 3);
    }

    #[test]
    fn group_braces_are_not_regions() {
        let src = "x = llvm.add a, b <{nsw=false}> : i64";
        let tree = parse(src);
        assert!(tree.errors.is_empty(), "{:?}", tree.errors);
        assert!(tree.stmt(tree.top[0]).regions.is_empty());
    }

    #[test]
    fn unclosed_region_reported() {
        let src = "builtin.module @m {\n^e():\n  t.r\n";
        let tree = parse(src);
        assert!(
            tree.errors
                .iter()
                .any(|e| matches!(e.kind, ErrorKind::Unclosed { closer: '}', .. })),
            "{:?}",
            tree.errors
        );
    }

    #[test]
    fn outlined_section() {
        let src = "builtin.module @bar {\n  ^b() !0:\n    c = t.c 1 !2;\n    t.r c !3\n} !5\n\noutlined_attributes:\n!0 = @[<in-memory>: line: 3, column: 3], []\n!2 = @[<in-memory>: line: 8, column: 9], [builtin_given_names = builtin.given_names [c]]\n";
        let tree = parse(src);
        assert!(tree.errors.is_empty(), "{:?}", tree.errors);
        let o = tree.outlined.as_ref().unwrap();
        assert_eq!(o.entries.len(), 2);
        assert_eq!(o.entries[0].index, 0);
        let (s, e) = o.entries[1].loc.unwrap();
        assert_eq!(
            &src[s as usize..e as usize],
            "@[<in-memory>: line: 8, column: 9],"
        );
        let block = tree.block(tree.region(tree.stmt(tree.top[0]).regions[0]).blocks[0]);
        assert!(block.outline_ref.is_some());
    }

    #[test]
    fn trailing_semicolon_warns() {
        let src = "builtin.module @m {\n^e():\n  t.r;\n}";
        let tree = parse(src);
        assert_eq!(tree.errors.len(), 1);
        assert_eq!(tree.errors[0].kind, ErrorKind::TrailingSemicolon);
        assert!(tree.errors[0].warning);
    }

    #[test]
    fn garbage_never_panics() {
        for src in [
            "",
            "}",
            "{",
            "^",
            "^a(",
            "^a(x:",
            "x =",
            "x = y",
            "a.b {",
            "a.b { ^c(",
            "outlined_attributes:",
            "a.b\noutlined_attributes:\n!0 = @[",
            "x, = a.b",
            "a.b (((((",
            "a.b )))",
            "a.b { ^c(): d.e { ^f(): } }",
        ] {
            let _ = parse(src);
        }
    }
}

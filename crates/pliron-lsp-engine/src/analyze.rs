//! Parse + verify a document with the instrumented pliron and convert the
//! result into the protocol model.

use std::collections::HashMap;
use std::time::Instant;

use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{
    IsTerminatorInterface, IsolatedFromAboveInterface, OneResultInterface, SymbolOpInterface,
    SymbolTableInterface,
};
use pliron::builtin::ops::ForwardRefOp;
use pliron::combine::stream::position::SourcePosition;
use pliron::common_traits::Named;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::location::{Located, Location};
use pliron::lsp::{Event, RecordOptions, parse_recorded};
use pliron::op::{op_cast, op_impls};
use pliron::operation::{Operation, verify_operation};
use pliron::printable::Printable;
use pliron::r#type::Typed;
use pliron::value::{DefiningEntity, Value};
use pliron_lsp_protocol::{
    AnalyzeParams, AnalyzeResult, AttrInfo, BlockInfo, DiagPhase, EngineDiag, Model, OpInfo, Pos,
    RegionInfo, Span, SpanKind, ValueDef, ValueInfo, VerifyMode, op_traits,
};

fn pos(p: SourcePosition) -> Pos {
    Pos {
        line: p.line.max(1) as u32,
        column: p.column.max(1) as u32,
    }
}

fn loc_pos(loc: &Location) -> Option<Pos> {
    match loc {
        Location::SrcPos { pos: p, .. } => Some(pos(*p)),
        Location::Fused { locations, .. } => locations.iter().find_map(loc_pos),
        Location::Named { child_loc, .. } => loc_pos(child_loc),
        Location::CallSite { callee, caller } => loc_pos(callee).or_else(|| loc_pos(caller)),
        Location::Unknown => None,
    }
}

/// The result for a request whose analysis panicked.
pub(crate) fn panicked(text_hash: u64, message: String) -> AnalyzeResult {
    AnalyzeResult {
        text_hash,
        parse_errors: vec![EngineDiag {
            phase: DiagPhase::Panic,
            pos: None,
            message: format!("the parser panicked: {message}"),
            op: None,
        }],
        verify_errors: Vec::new(),
        model: None,
        spans: Vec::new(),
        elapsed_us: 0,
    }
}

/// Analyze a document.
pub fn analyze(params: &AnalyzeParams) -> AnalyzeResult {
    let started = Instant::now();
    let mut ctx = Context::new();
    let (res, recording) = parse_recorded(
        &mut ctx,
        &params.text,
        RecordOptions {
            recover: true,
            keep_parsed_locations: true,
        },
    );

    let mut parse_errors: Vec<EngineDiag> = recording
        .errors
        .iter()
        .map(|e| EngineDiag {
            phase: DiagPhase::Parse,
            pos: Some(pos(e.pos)),
            message: e.message.clone(),
            op: None,
        })
        .collect();

    let mut b = ModelBuilder::new(&ctx, params.max_attr_len as usize);
    let mut top_op = None;
    match res {
        Ok(top) => {
            let root = b.op(top);
            b.model.root = root;
            top_op = Some(top);
        }
        Err(e) => parse_errors.push(EngineDiag {
            phase: DiagPhase::Parse,
            pos: loc_pos(&e.loc),
            message: e.err.to_string(),
            op: None,
        }),
    }

    let spans = recording
        .events
        .iter()
        .filter_map(|ev| b.span(ev))
        .collect();

    let mut verify_errors = Vec::new();
    if let (Some(top), true, VerifyMode::First) =
        (top_op, parse_errors.is_empty(), params.verify)
        && let Err(e) = verify_operation(top, &ctx)
    {
        verify_errors.push(EngineDiag {
            phase: DiagPhase::Verify,
            pos: loc_pos(&e.loc),
            message: e.err.to_string(),
            op: None,
        });
    }

    let model = top_op.map(|_| std::mem::take(&mut b.model));
    AnalyzeResult {
        text_hash: params.text_hash,
        parse_errors,
        verify_errors,
        model: if params.want_model { model } else { None },
        spans,
        elapsed_us: started.elapsed().as_micros() as u64,
    }
}

struct ModelBuilder<'c> {
    ctx: &'c Context,
    model: Model,
    ops: HashMap<Ptr<Operation>, u32>,
    blocks: HashMap<Ptr<BasicBlock>, u32>,
    values: HashMap<Value, u32>,
    max_attr_len: usize,
}

impl<'c> ModelBuilder<'c> {
    fn new(ctx: &'c Context, max_attr_len: usize) -> Self {
        ModelBuilder {
            ctx,
            model: Model::default(),
            ops: HashMap::new(),
            blocks: HashMap::new(),
            values: HashMap::new(),
            max_attr_len: max_attr_len.max(16),
        }
    }

    fn render<T: Printable + ?Sized>(&self, t: &T) -> String {
        let s = t.disp(self.ctx).to_string();
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Register an op (and its regions, recursively).
    fn op(&mut self, op: Ptr<Operation>) -> u32 {
        if let Some(id) = self.ops.get(&op) {
            return *id;
        }
        let id = self.model.ops.len() as u32;
        self.model.ops.push(OpInfo::default());
        self.ops.insert(op, id);
        let ctx = self.ctx;

        let opid = Operation::get_opid(op, ctx).to_string();
        let op_dyn = Operation::get_op_dyn(op, ctx);
        let mut traits = 0;
        if op_impls::<dyn IsolatedFromAboveInterface>(op_dyn.as_ref()) {
            traits |= op_traits::ISOLATED_FROM_ABOVE;
        }
        if op_impls::<dyn SymbolTableInterface>(op_dyn.as_ref()) {
            traits |= op_traits::SYMBOL_TABLE;
        }
        if op_impls::<dyn IsTerminatorInterface>(op_dyn.as_ref()) {
            traits |= op_traits::TERMINATOR;
        }
        if op_impls::<dyn OneResultInterface>(op_dyn.as_ref()) {
            traits |= op_traits::ONE_RESULT;
        }
        let symbol = op_cast::<dyn SymbolOpInterface>(op_dyn.as_ref()).map(|s| {
            traits |= op_traits::SYMBOL;
            s.get_symbol_name(ctx).to_string()
        });

        let (pos, results, operands, successors, attrs, regions, parent) = {
            let o = op.deref(ctx);
            let attrs: Vec<AttrInfo> = o
                .attributes
                .0
                .iter()
                .map(|(k, a)| {
                    let text = self.render(a.as_ref());
                    let truncated = text.chars().count() > self.max_attr_len;
                    let text = if truncated {
                        text.chars().take(self.max_attr_len).collect::<String>() + "…"
                    } else {
                        text
                    };
                    AttrInfo {
                        key: k.to_string(),
                        text,
                        truncated,
                    }
                })
                .collect();
            (
                loc_pos(&o.loc()),
                o.results().collect::<Vec<_>>(),
                o.operands().collect::<Vec<_>>(),
                o.successors().collect::<Vec<_>>(),
                attrs,
                o.regions().collect::<Vec<_>>(),
                o.get_parent_block(),
            )
        };
        let parent_block = parent.map(|b| self.block(b));
        let results = results.into_iter().map(|v| self.value(v)).collect();
        let operands = operands.into_iter().map(|v| self.value(v)).collect();
        let successors = successors.into_iter().map(|b| self.block(b)).collect();
        {
            let info = &mut self.model.ops[id as usize];
            info.opid = opid;
            info.pos = pos;
            info.parent_block = parent_block;
            info.results = results;
            info.operands = operands;
            info.successors = successors;
            info.attrs = attrs;
            info.symbol = symbol;
            info.traits = traits;
        }

        let mut region_ids = Vec::new();
        for region in regions {
            let rid = self.model.regions.len() as u32;
            self.model.regions.push(RegionInfo {
                parent_op: id,
                blocks: Vec::new(),
            });
            region_ids.push(rid);
            let blocks: Vec<_> = region.deref(ctx).iter(ctx).collect();
            let mut block_ids = Vec::new();
            for block in blocks {
                let bid = self.block(block);
                self.model.blocks[bid as usize].region = rid;
                block_ids.push(bid);
                let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
                let mut op_ids = Vec::new();
                for child in ops {
                    let cid = self.op(child);
                    self.model.ops[cid as usize].parent_block = Some(bid);
                    op_ids.push(cid);
                }
                self.model.blocks[bid as usize].ops = op_ids;
            }
            self.model.regions[rid as usize].blocks = block_ids;
        }
        self.model.ops[id as usize].regions = region_ids;
        id
    }

    fn block(&mut self, block: Ptr<BasicBlock>) -> u32 {
        if let Some(id) = self.blocks.get(&block) {
            return *id;
        }
        let id = self.model.blocks.len() as u32;
        self.model.blocks.push(BlockInfo::default());
        self.blocks.insert(block, id);
        let (label, pos, args) = {
            let b = block.deref(self.ctx);
            (
                b.given_name(self.ctx).map(|l| l.to_string()),
                loc_pos(&b.loc()),
                b.arguments().collect::<Vec<_>>(),
            )
        };
        let args: Vec<u32> = args.into_iter().map(|v| self.value(v)).collect();
        let info = &mut self.model.blocks[id as usize];
        info.label = label;
        info.pos = pos;
        info.args = args;
        id
    }

    fn value(&mut self, v: Value) -> u32 {
        if let Some(id) = self.values.get(&v) {
            return *id;
        }
        let id = self.model.values.len() as u32;
        self.values.insert(v, id);
        let ctx = self.ctx;
        let ty = self.render(&v.get_type(ctx));
        let given_name = v.given_name(ctx).map(|n| n.to_string());
        self.model.values.push(ValueInfo {
            def: ValueDef::Detached { unresolved: false },
            ty,
            given_name,
        });
        let index = v.try_find_index(ctx).unwrap_or(0) as u32;
        let def = match v.defining_entity() {
            DefiningEntity::Op(op) if Operation::is_op::<ForwardRefOp>(op, ctx) => {
                ValueDef::Detached { unresolved: true }
            }
            DefiningEntity::Op(op) => match self.ops.get(&op) {
                Some(o) => ValueDef::Result { op: *o, index },
                // The defining op hasn't been visited yet (forward use):
                // register it now; its parent is fixed up when reached.
                None if op.deref(ctx).get_parent_block().is_some() => ValueDef::Result {
                    op: self.op(op),
                    index,
                },
                None => ValueDef::Detached { unresolved: false },
            },
            DefiningEntity::Block(b) => ValueDef::Arg {
                block: self.block(b),
                index,
            },
        };
        self.model.values[id as usize].def = def;
        id
    }

    fn span(&mut self, ev: &Event) -> Option<Span> {
        let (start, end, kind) = match ev {
            Event::Op {
                op,
                start,
                opid_start,
                opid_end,
                end,
            } => {
                let op = self.op(*op);
                (
                    *start,
                    *end,
                    SpanKind::Op {
                        op,
                        name_start: pos(*opid_start),
                        name_end: pos(*opid_end),
                    },
                )
            }
            Event::OpError { start, end } => (*start, *end, SpanKind::OpError),
            Event::ResultDef { value, start, end } => (
                *start,
                *end,
                SpanKind::ResultDef {
                    value: self.value(*value),
                },
            ),
            Event::OperandUse { value, start, end } => (
                *start,
                *end,
                SpanKind::OperandUse {
                    value: self.value(*value),
                },
            ),
            Event::SuccessorUse { block, start, end } => (
                *start,
                *end,
                SpanKind::SuccessorUse {
                    block: self.block(*block),
                },
            ),
            Event::BlockLabel { block, start, end } => (
                *start,
                *end,
                SpanKind::BlockLabel {
                    block: self.block(*block),
                },
            ),
            Event::BlockArgDef { value, start, end } => (
                *start,
                *end,
                SpanKind::BlockArgDef {
                    value: self.value(*value),
                },
            ),
            Event::Type {
                ty,
                start,
                id_end,
                end,
            } => (
                *start,
                *end,
                SpanKind::Type {
                    name_end: pos(*id_end),
                    text: self.render(ty),
                },
            ),
            Event::Attr { start, id_end, end } => (
                *start,
                *end,
                SpanKind::Attr {
                    name_end: pos(*id_end),
                },
            ),
            Event::AttrKey { start, end } => (*start, *end, SpanKind::AttrKey),
            Event::Keyword { start, end } => (*start, *end, SpanKind::Keyword),
            Event::Region { open, close, .. } => (
                *open,
                close.unwrap_or(*open),
                SpanKind::Region {
                    closed: close.is_some(),
                },
            ),
        };
        Some(Span {
            start: pos(start),
            end: pos(end),
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pliron_llvm as _;
    use pliron_lsp_protocol::text_hash;

    fn run(text: &str) -> AnalyzeResult {
        analyze(&AnalyzeParams {
            text_hash: text_hash(text),
            text: text.to_string(),
            verify: VerifyMode::First,
            want_model: true,
            max_attr_len: 200,
        })
    }

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
    c = llvm.icmp z <SLT> x : builtin.integer i1;
    r = llvm.call @callee (z) : llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false>;
    llvm.return r
  }
}
"#;

    fn text_at(text: &str, s: &Span) -> String {
        let li: Vec<&str> = text.split('\n').collect();
        let line = li[s.start.line as usize - 1];
        if s.start.line == s.end.line {
            line.chars()
                .skip(s.start.column as usize - 1)
                .take((s.end.column - s.start.column) as usize)
                .collect()
        } else {
            line.chars().skip(s.start.column as usize - 1).collect()
        }
    }

    #[test]
    fn demo_exact_spans() {
        let r = run(DEMO);
        assert!(r.parse_errors.is_empty(), "{:?}", r.parse_errors);
        assert!(r.verify_errors.is_empty(), "{:?}", r.verify_errors);
        let m = r.model.as_ref().unwrap();
        // Every operand use of `z` points at text "z" and resolves to the
        // result of llvm.add.
        let z_uses: Vec<_> = r
            .spans
            .iter()
            .filter(|s| matches!(s.kind, SpanKind::OperandUse { value } if m.values[value as usize].given_name.as_deref() == Some("z")))
            .collect();
        assert_eq!(z_uses.len(), 2, "{z_uses:?}");
        for s in &z_uses {
            assert_eq!(text_at(DEMO, s), "z");
        }
        // Op name spans.
        let names: Vec<String> = r
            .spans
            .iter()
            .filter_map(|s| match &s.kind {
                SpanKind::Op { name_start, name_end, .. } => Some(text_at(
                    DEMO,
                    &Span { start: *name_start, end: *name_end, kind: SpanKind::Keyword },
                )),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"llvm.call".to_string()), "{names:?}");
        // Types and keywords are recorded.
        assert!(r.spans.iter().any(|s| matches!(&s.kind, SpanKind::Type { text, .. } if text == "builtin.integer i64")));
        assert!(r.spans.iter().any(|s| matches!(s.kind, SpanKind::BlockLabel { .. }) && text_at(DEMO, s) == "^bb2"));
        assert!(r.spans.iter().any(|s| matches!(s.kind, SpanKind::SuccessorUse { .. }) && text_at(DEMO, s) == "^bb2"));
        assert!(r.spans.iter().any(|s| matches!(s.kind, SpanKind::BlockArgDef { .. }) && text_at(DEMO, s) == "x2"));
        assert!(r.spans.iter().any(|s| matches!(s.kind, SpanKind::ResultDef { .. }) && text_at(DEMO, s) == "z"));
        assert!(r.spans.iter().any(|s| matches!(s.kind, SpanKind::Keyword) && text_at(DEMO, s) == ":"));
    }

    #[test]
    fn recovers_and_reports_multiple_errors() {
        // A typo in the first op of @f and an undefined name further down.
        let text = DEMO
            .replace("y = builtin.constant <builtin.integer <1: i64>>", "y = builtin.constnt <builtin.integer <1: i64>>")
            .replace("z = llvm.add x2, y2", "z = llvm.add x2, w2");
        let r = run(&text);
        let msgs: Vec<String> = r.parse_errors.iter().map(|e| format!("{:?} {}", e.pos, e.message)).collect();
        assert!(msgs.iter().any(|m| m.contains("Unregistered Op builtin.constnt")), "{msgs:#?}");
        assert!(msgs.iter().any(|m| m.contains("w2")), "{msgs:#?}");
        // `y` is a placeholder from the failed op: no cascading error.
        assert!(!msgs.iter().any(|m| m.contains("Value y ")), "{msgs:#?}");
        // The rest of the function is still parsed.
        let m = r.model.unwrap();
        assert!(m.ops.iter().any(|o| o.opid == "llvm.call"));
        assert!(r.spans.iter().any(|s| matches!(s.kind, SpanKind::OperandUse { .. }) && text_at(&text, s) == "z"));
    }

    #[test]
    fn module_with_func() {
        let text = "builtin.module @m {\n  ^entry():\n    builtin.func @f: builtin.function <() -> ()> {\n      ^bb0():\n    }\n}\n";
        let r = run(text);
        assert!(r.parse_errors.is_empty(), "{:?}", r.parse_errors);
        let m = r.model.unwrap();
        assert_eq!(m.ops[m.root as usize].opid, "builtin.module");
        assert!(m.ops.iter().any(|o| o.symbol.as_deref() == Some("f")));
    }
}

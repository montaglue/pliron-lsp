//! Parse + verify a document with the instrumented pliron and convert the
//! result into the protocol model.

use std::collections::{HashMap, HashSet};
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
        hook_diags: Vec::new(),
        hook_hints: Vec::new(),
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
    // Ops known to pass verification (lints only run on those).
    let mut verified = false;
    let mut failed_ops = HashSet::new();
    if let (Some(top), true) = (top_op, parse_errors.is_empty()) {
        match params.verify {
            VerifyMode::Off => {}
            VerifyMode::First => {
                if let Err(e) = verify_operation(top, &ctx) {
                    verify_errors.push(EngineDiag {
                        phase: DiagPhase::Verify,
                        pos: loc_pos(&e.loc),
                        message: e.err.to_string(),
                        op: None,
                    });
                }
                verified = verify_errors.is_empty();
            }
            VerifyMode::All => {
                (verify_errors, failed_ops) = verify_all(top, &ctx);
                verified = true;
            }
        }
    }

    // Dialect hooks (pliron-lsp-api).
    let mut hook_diags = Vec::new();
    let mut hook_hints = Vec::new();
    if top_op.is_some() && crate::hooks::any() {
        let mut ops: Vec<(Ptr<Operation>, u32)> = b.ops.iter().map(|(p, i)| (*p, *i)).collect();
        ops.sort_by_key(|(_, i)| *i);
        for (op, id) in ops {
            if verified && !failed_ops.contains(&op) {
                crate::hooks::lint(&ctx, op, id, &mut hook_diags);
            }
            if params.want_model {
                b.model.ops[id as usize].notes = crate::hooks::hover(&ctx, op);
                crate::hooks::inlay(&ctx, op, id, &mut hook_hints);
            }
        }
    }

    let model = top_op.map(|_| std::mem::take(&mut b.model));
    AnalyzeResult {
        text_hash: params.text_hash,
        parse_errors,
        verify_errors,
        model: if params.want_model { model } else { None },
        spans,
        hook_diags,
        hook_hints,
        elapsed_us: started.elapsed().as_micros() as u64,
    }
}

/// Verify every operation and every block separately, collecting all
/// errors (pliron's `verify_operation` stops at the first one). Each check
/// is pliron's own verifier; errors found again through a parent are
/// reported once. Also returns the ops that failed their own verification.
fn verify_all(top: Ptr<Operation>, ctx: &Context) -> (Vec<EngineDiag>, HashSet<Ptr<Operation>>) {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use pliron::common_traits::Verify;

    fn collect(
        ctx: &Context,
        op: Ptr<Operation>,
        ops: &mut Vec<Ptr<Operation>>,
        blocks: &mut Vec<Ptr<BasicBlock>>,
    ) {
        ops.push(op);
        let regions: Vec<_> = op.deref(ctx).regions().collect();
        for r in regions {
            let bs: Vec<_> = r.deref(ctx).iter(ctx).collect();
            for b in bs {
                blocks.push(b);
                let children: Vec<_> = b.deref(ctx).iter(ctx).collect();
                for c in children {
                    collect(ctx, c, ops, blocks);
                }
            }
        }
    }
    let mut ops = Vec::new();
    let mut blocks = Vec::new();
    collect(ctx, top, &mut ops, &mut blocks);

    let mut out: Vec<EngineDiag> = Vec::new();
    let mut push = |pos: Option<Pos>, message: String| {
        if !out.iter().any(|d| d.pos == pos && d.message == message) {
            out.push(EngineDiag {
                phase: DiagPhase::Verify,
                pos,
                message,
                op: None,
            });
        }
    };
    let mut failed = HashSet::new();
    // Innermost first, so that an error is attributed to the deepest entity
    // reporting it.
    for op in ops.iter().rev() {
        let loc = op.deref(ctx).loc();
        match catch_unwind(AssertUnwindSafe(|| op.deref(ctx).verify(ctx))) {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => push(loc_pos(&e.loc).or(loc_pos(&loc)), e.err.to_string()),
            Err(_) => push(
                loc_pos(&loc),
                format!("the verifier panicked: {}", crate::take_panic()),
            ),
        }
        failed.insert(*op);
    }
    for b in blocks.iter().rev() {
        let loc = b.deref(ctx).loc();
        match catch_unwind(AssertUnwindSafe(|| b.deref(ctx).verify(ctx))) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => push(loc_pos(&e.loc).or(loc_pos(&loc)), e.err.to_string()),
            Err(_) => push(
                loc_pos(&loc),
                format!("the verifier panicked: {}", crate::take_panic()),
            ),
        }
    }
    if let Ok(Err(e)) = catch_unwind(AssertUnwindSafe(|| {
        pliron::operation::verify_value_dominance(top, ctx)
    })) {
        push(loc_pos(&e.loc), e.err.to_string());
    }
    out.sort_by_key(|d| d.pos);
    (out, failed)
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
            Event::Token { kind, start, end } => (
                *start,
                *end,
                SpanKind::Token {
                    token_type: kind.to_string(),
                },
            ),
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
    fn reports_every_verifier_error() {
        // Two functions whose entry blocks lack a terminator.
        let text = "builtin.module @m {\n  ^entry():\n  builtin.func @a: builtin.function <() -> ()> {\n    ^bb0():\n    c = builtin.constant <builtin.integer <1: i64>> : builtin.integer i64\n  };\n  builtin.func @b: builtin.function <() -> ()> {\n    ^bb1():\n    d = builtin.constant <builtin.integer <2: i64>> : builtin.integer i64\n  }\n}\n";
        let r = analyze(&AnalyzeParams {
            text_hash: text_hash(text),
            text: text.to_string(),
            verify: VerifyMode::All,
            want_model: true,
            max_attr_len: 200,
        });
        assert!(r.parse_errors.is_empty(), "{:?}", r.parse_errors);
        let lines: Vec<u32> = r.verify_errors.iter().filter_map(|d| d.pos.map(|p| p.line)).collect();
        assert!(lines.contains(&4) && lines.contains(&8), "{:#?}", r.verify_errors);
        // pliron's own verify_operation stops at the first.
        let first = run(text);
        assert_eq!(first.verify_errors.len(), 1);
    }

    /// Hooks registered with pliron-lsp-api (in engine mode).
    #[cfg(feature = "hooks")]
    mod hooks {
        use super::*;
        use pliron_lsp_api::{Diagnostics, InlayHints, Target};
        use pliron_lsp_protocol::{HookSeverity, HookTarget};

        fn is(ctx: &Context, op: Ptr<Operation>, name: &str) -> bool {
            Operation::get_opid(op, ctx).to_string() == name
        }

        fn unused_constants(ctx: &Context, op: Ptr<Operation>, diags: &mut Diagnostics) {
            if is(ctx, op, "builtin.constant") && !op.deref(ctx).get_result(0).is_used(ctx) {
                diags.warning("unused constant").at(Target::Result(0));
            }
        }
        pliron_lsp_api::lint!(unused_constants);

        fn panicky(ctx: &Context, op: Ptr<Operation>, _: &mut Diagnostics) {
            if is(ctx, op, "llvm.return") {
                panic!("boom");
            }
        }
        pliron_lsp_api::lint!(panicky);

        fn func_note(ctx: &Context, op: Ptr<Operation>) -> Option<String> {
            is(ctx, op, "llvm.func").then(|| "an LLVM function".to_string())
        }
        pliron_lsp_api::hover!(func_note);

        fn first_operand(ctx: &Context, op: Ptr<Operation>, hints: &mut InlayHints) {
            if is(ctx, op, "llvm.add") {
                hints.add(Target::Operand(0), "lhs:");
            }
        }
        pliron_lsp_api::inlay!(first_operand);

        #[test]
        fn hooks_run() {
            assert!(crate::hooks::names().iter().any(|n| n.ends_with("unused_constants")));
            let text = DEMO.replace(
                "    llvm.return r\n  }\n}",
                "    k = builtin.constant <builtin.integer <7: i64>> : builtin.integer i64;\n    llvm.return r\n  }\n}",
            );
            let r = analyze(&AnalyzeParams {
                text_hash: text_hash(&text),
                text: text.clone(),
                verify: VerifyMode::All,
                want_model: true,
                max_attr_len: 200,
            });
            assert!(r.parse_errors.is_empty() && r.verify_errors.is_empty(), "{r:#?}");
            let m = r.model.as_ref().unwrap();
            let unused: Vec<_> = r.hook_diags.iter().filter(|d| d.message == "unused constant").collect();
            // `one` is used by cond_br, `y` by the branch; only `k` is unused.
            assert_eq!(unused.len(), 1, "{:#?}", r.hook_diags);
            assert_eq!(unused[0].severity, HookSeverity::Warning);
            assert_eq!(unused[0].target, HookTarget::Result { index: 0 });
            let k = m.ops[unused[0].op as usize].results[0];
            assert_eq!(m.values[k as usize].given_name.as_deref(), Some("k"));
            assert!(unused[0].source.ends_with("unused_constants"));
            // A panicking hook is reported, not fatal.
            assert!(r.hook_diags.iter().any(|d| d.message.contains("panicked: boom")), "{:#?}", r.hook_diags);
            assert!(m.ops.iter().filter(|o| o.opid == "llvm.func").all(|o| o.notes == ["an LLVM function"]));
            assert!(r.hook_hints.iter().any(|h| h.label == "lhs:" && h.target == HookTarget::Operand { index: 0 }));
        }
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

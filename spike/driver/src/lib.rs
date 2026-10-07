//! Spike driver: parse + verify IR with real pliron and dump a line-oriented
//! model. Run natively (`cargo run --bin native`) as the oracle, and under
//! rust-analyzer's MIR interpreter by the spike runner.

use std::collections::HashMap;
use std::fmt::Write as _;

use pliron::context::{Context, Ptr};
use pliron::irfmt::parsers::spaced;
use pliron::linked_list::ContainsLinkedList;
use pliron::location::{Located, Location};
use pliron::operation::{Operation, verify_operation};
use pliron::parsable::parse_from_str;
use pliron::printable::Printable;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm as _;

pub mod input;

fn loc_str(loc: &Location) -> String {
    match loc {
        Location::SrcPos { pos, .. } => format!("{}:{}", pos.line, pos.column),
        _ => "?".to_string(),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn value_id(ids: &mut HashMap<Value, usize>, v: Value) -> usize {
    let next = ids.len();
    *ids.entry(v).or_insert(next)
}

fn dump_op(
    ctx: &Context,
    op: Ptr<Operation>,
    depth: usize,
    ids: &mut HashMap<Value, usize>,
    out: &mut String,
) {
    let opref = op.deref(ctx);
    let indent = depth * 2;
    let opid = Operation::get_opid(op, ctx);
    let results: Vec<String> = opref
        .results()
        .map(|v| {
            let id = value_id(ids, v);
            format!("%{}: {}", id, one_line(&v.get_type(ctx).disp(ctx).to_string()))
        })
        .collect();
    let operands: Vec<String> = opref
        .operands()
        .map(|v| format!("%{}", value_id(ids, v)))
        .collect();
    let _ = writeln!(
        out,
        "{:indent$}op {} @{} results=[{}] operands=[{}] succs={}",
        "",
        opid,
        loc_str(&opref.loc()),
        results.join(", "),
        operands.join(", "),
        opref.successors().count(),
    );
    for region in opref.regions() {
        for block in region.deref(ctx).iter(ctx) {
            let bref = block.deref(ctx);
            let args: Vec<String> = bref
                .arguments()
                .map(|v| {
                    let id = value_id(ids, v);
                    format!("%{}: {}", id, one_line(&v.get_type(ctx).disp(ctx).to_string()))
                })
                .collect();
            let _ = writeln!(
                out,
                "{:indent$}  block @{} args=[{}]",
                "",
                loc_str(&bref.loc()),
                args.join(", "),
            );
            for child in bref.iter(ctx) {
                dump_op(ctx, child, depth + 2, ids, out);
            }
        }
    }
}

/// Parse, verify and dump `text`.
pub fn analyze_text(text: &str) -> String {
    let mut out = String::new();
    let mut ctx = Context::new();
    match parse_from_str(spaced(Operation::top_level_parser()), &mut ctx, text) {
        Err(e) => {
            let _ = writeln!(
                out,
                "parse-error @{} {}",
                loc_str(&e.loc),
                one_line(&e.disp(&ctx).to_string())
            );
        }
        Ok(top) => {
            match verify_operation(top, &ctx) {
                Ok(()) => out.push_str("verify ok\n"),
                Err(e) => {
                    let _ = writeln!(
                        out,
                        "verify-error @{} {}",
                        loc_str(&e.loc),
                        one_line(&e.disp(&ctx).to_string())
                    );
                }
            }
            let mut ids = HashMap::new();
            dump_op(&ctx, top, 0, &mut ids, &mut out);
        }
    }
    out
}

/// Entry point evaluated by the interpreter.
pub fn __pliron_lsp_analyze() {
    print!("{}", analyze_text(input::INPUT));
}

// ---- bisection probes -------------------------------------------------------

pub fn probe_print() {
    println!("hello from interpreter");
}

pub fn probe_hashmap() {
    let mut m: HashMap<u32, String> = HashMap::new();
    m.insert(1, "a".to_string());
    println!("map len {}", m.len());
}

pub fn probe_regs() {
    let n = pliron::context::get_context_registrations().count();
    println!("registrations: {n}");
}

pub fn probe_ctx() {
    let _ctx = Context::new();
    println!("context ok");
}

pub fn probe_parse_type() {
    let mut ctx = Context::new();
    let r = parse_from_str(pliron::irfmt::parsers::type_parser(), &mut ctx, "builtin.integer i64");
    match r {
        Ok(t) => println!("type ok: {}", t.disp(&ctx)),
        Err(e) => println!("type err: {}", e.disp(&ctx)),
    }
}

pub fn probe_loop() {
    let mut s = 0u64;
    let mut i = 0u64;
    while i < 1_000_000 {
        s = s.wrapping_add(i.wrapping_mul(i));
        i += 1;
    }
    assert!(s != 1);
}

pub fn probe_combine() {
    use pliron::combine::Parser;
    use pliron::combine::parser::char::digit;
    use pliron::combine::many1;
    let r = many1::<String, _, _>(digit()).parse("12345");
    assert!(r.is_ok());
    assert!(r.unwrap().0 == "12345");
}

pub fn probe_vec_string() {
    let v: Vec<String> = (0..10).map(|i| format!("item{i}")).collect();
    assert!(v.join(",").len() > 10);
}

use pliron::context::Context;
use pliron::irfmt::parsers::spaced;
use pliron::operation::{Operation, verify_operation};
use pliron::parsable::parse_from_str;
use std::time::Instant;

fn main() {
    let text = pliron_lsp_spike_driver::input::INPUT;
    let t0 = Instant::now();
    let mut ctx = Context::new();
    let t1 = Instant::now();
    let top = parse_from_str(spaced(Operation::top_level_parser()), &mut ctx, text).expect("parse");
    let t2 = Instant::now();
    verify_operation(top, &ctx).expect("verify");
    let t3 = Instant::now();
    // Second parse into a fresh context (registration cost again) and into the same one.
    let mut ctx2 = Context::new();
    let t4 = Instant::now();
    let _ = parse_from_str(spaced(Operation::top_level_parser()), &mut ctx2, text).expect("parse2");
    let t5 = Instant::now();
    eprintln!(
        "ctx_new={:?} parse={:?} verify={:?} ctx_new2={:?} parse2={:?}",
        t1 - t0, t2 - t1, t3 - t2, t4 - t3, t5 - t4
    );
}

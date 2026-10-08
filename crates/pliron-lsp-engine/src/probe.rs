//! Registration probes: which op/type/attribute/dialect names does this
//! engine know? pliron does not expose its registry, so each name is parsed
//! and the "Unregistered ..." errors are recognised.

use std::panic::{AssertUnwindSafe, catch_unwind};

use pliron::combine::Parser;
use pliron::context::Context;
use pliron::dialect::DialectName;
use pliron::irfmt::parsers::{attr_parser, type_parser};
use pliron::location::Source;
use pliron::operation::{Operation, OperationParserConfig};
use pliron::parsable::{Parsable, State, StateStream, state_stream_from_iterator};
use pliron_lsp_protocol::ProbeParams;

/// Parse `text` with `parser`, returning the error message on failure.
/// (Works across pliron versions, unlike `parse_from_str`.)
fn parse_err<'a, P: Parser<StateStream<'a>>>(ctx: &'a mut Context, text: &'a str, mut parser: P) -> Option<String> {
    let stream = state_stream_from_iterator(text.chars(), State::new(ctx, Source::InMemory));
    parser.parse(stream).err().map(|e| e.to_string())
}

fn registered(ctx: &mut Context, kind: &str, name: &str) -> bool {
    let text = format!("{name} ");
    let res = catch_unwind(AssertUnwindSafe(|| match kind {
        "op" => parse_err(
            ctx,
            &text,
            Operation::parser(OperationParserConfig {
                look_for_outlined_attrs: false,
            }),
        ),
        "type" => parse_err(ctx, &text, type_parser()),
        "attr" => parse_err(ctx, &text, attr_parser()),
        _ => parse_err(ctx, &text, DialectName::parser(())),
    }));
    match res {
        // A panic means the name reached a dialect parser: it is registered.
        Err(_) => true,
        Ok(None) => true,
        Ok(Some(msg)) => !msg.contains("Unregistered"),
    }
}

pub fn probe(params: &ProbeParams) -> ProbeParams {
    let mut ctx = Context::new();
    let mut check = |kind: &str, names: &[String]| -> Vec<String> {
        names
            .iter()
            .filter(|n| registered(&mut ctx, kind, n))
            .cloned()
            .collect()
    };
    ProbeParams {
        dialects: check("dialect", &params.dialects),
        ops: check("op", &params.ops),
        types: check("type", &params.types),
        attrs: check("attr", &params.attrs),
    }
}

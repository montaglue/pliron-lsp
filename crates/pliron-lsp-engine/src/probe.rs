//! Registration probes: which op/type/attribute/dialect names does this
//! engine know? pliron does not expose its registry, so each name is parsed
//! and the "Unregistered ..." errors are recognised.

use std::panic::{AssertUnwindSafe, catch_unwind};

use pliron::context::Context;
use pliron::dialect::DialectName;
use pliron::irfmt::parsers::{attr_parser, type_parser};
use pliron::operation::{Operation, OperationParserConfig};
use pliron::parsable::{Parsable, parse_from_str};
use pliron_lsp_protocol::ProbeParams;

fn registered(ctx: &mut Context, kind: &str, name: &str) -> bool {
    let text = format!("{name} ");
    let res = catch_unwind(AssertUnwindSafe(|| match kind {
        "op" => parse_from_str(
            Operation::parser(OperationParserConfig {
                look_for_outlined_attrs: false,
            }),
            ctx,
            &text,
        )
        .err()
        .map(|e| e.err.to_string()),
        "type" => parse_from_str(type_parser(), ctx, &text)
            .err()
            .map(|e| e.err.to_string()),
        "attr" => parse_from_str(attr_parser(), ctx, &text)
            .err()
            .map(|e| e.err.to_string()),
        _ => parse_from_str(DialectName::parser(()), ctx, &text)
            .err()
            .map(|e| e.err.to_string()),
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

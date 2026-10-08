//! A toy dialect used to test pliron-lsp's automatic dialect bundles.

use pliron::builtin::op_interfaces::{
    IsTerminatorInterface, NOpdsInterface, NResultsInterface, OneResultInterface, SameOperandsAndResultType,
    SameOperandsType, SameResultsType,
};
use pliron::builtin::attributes::StringAttr;
use pliron::combine::{Parser, many1, parser::char::digit};
use pliron::context::{Context, Ptr};
use pliron::derive::{pliron_op, pliron_type};
use pliron::identifier::Identifier;
use pliron::irfmt::parsers::spaced;
use pliron::location::{Located, Location};
use pliron::op::{Op, OpObj};
use pliron::operation::Operation;
use pliron::parsable::{Parsable, ParseResult, StateStream};
use pliron::printable::{self, Printable};
use pliron::{dict_key, input_err};
use pliron_lsp_api::{Diagnostics, InlayHints, Target};

/// An integer constant.
#[pliron_op(
    name = "toy.const",
    format = "`<` $toy_const_value `>` ` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface],
    attributes = (toy_const_value),
    verifier = "succ"
)]
pub struct ConstOp;

/// Adds two values of the same type.
#[pliron_op(
    name = "toy.add",
    format = "$0 `, ` $1 ` : ` type($0)",
    interfaces = [
        OneResultInterface,
        NOpdsInterface<2>,
        SameOperandsType,
        SameResultsType,
        SameOperandsAndResultType
    ],
    verifier = "succ"
)]
pub struct AddOp;

/// Prints a value.
#[pliron_op(
    name = "toy.print",
    format = "`value` ` = ` $0",
    interfaces = [NResultsInterface<0>, NOpdsInterface<1>],
    verifier = "succ"
)]
pub struct PrintOp;

/// Returns from a function.
#[pliron_op(
    name = "toy.return",
    format = "",
    interfaces = [IsTerminatorInterface, NResultsInterface<0>, NOpdsInterface<0>],
    verifier = "succ"
)]
pub struct ReturnOp;

/// A toy number type of some width.
#[pliron_type(name = "toy.num", format = "`<` $width `>`", generate_get = true, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct NumType {
    width: u32,
}

dict_key!(TOY_REPEAT_COUNT, "toy_repeat_count");

/// Repeats the enclosing block. Its syntax is hand-written:
/// `toy.repeat 3 times`.
#[pliron_op(
    name = "toy.repeat",
    interfaces = [NResultsInterface<0>, NOpdsInterface<0>],
    verifier = "succ"
)]
pub struct RepeatOp;

impl Printable for RepeatOp {
    fn fmt(
        &self,
        ctx: &Context,
        _state: &printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        let op = self.op.deref(ctx);
        let count = op.attributes.get::<StringAttr>(&TOY_REPEAT_COUNT).map(|c| c.as_str()).unwrap_or("0");
        write!(f, "{} {count} times", Self::get_opid_static())
    }
}

impl Parsable for RepeatOp {
    type Arg = Vec<(Identifier, Location)>;
    type Parsed = OpObj;
    fn parse<'a>(state_stream: &mut StateStream<'a>, results: Self::Arg) -> ParseResult<'a, Self::Parsed> {
        if !results.is_empty() {
            input_err!(state_stream.loc(), "toy.repeat has no results")?
        }
        let op = Operation::new(state_stream.state.ctx, Self::get_concrete_op_info(), vec![], vec![], vec![], 0);
        // `keyword!` and `token!` behave like plain parsers, and tell
        // pliron-lsp how to highlight what they parse.
        (
            spaced(pliron_lsp_api::token!(Number, many1::<String, _, _>(digit()))),
            spaced(pliron_lsp_api::keyword!("times")),
        )
            .parse_stream(state_stream)
            .map(|(count, _)| -> OpObj {
                let ctx = &mut state_stream.state.ctx;
                op.deref_mut(ctx).attributes.set(TOY_REPEAT_COUNT.clone(), StringAttr::new(count));
                OpObj::new(RepeatOp { op })
            })
            .into()
    }
}

// What pliron-lsp cannot derive for the hand-written syntax.
pliron_lsp_api::hints! {
    op "toy.repeat" {
        format: "$count `times`",
        snippet: "${1:2} times",
        keywords: ["times"],
    }
}

/// `x + x` is probably a mistake in a toy program.
fn self_add(ctx: &Context, op: Ptr<Operation>, diags: &mut Diagnostics) {
    if Operation::get_op::<AddOp>(op, ctx).is_some() {
        let o = op.deref(ctx);
        if o.get_operand(0) == o.get_operand(1) {
            diags.warning("adds a value to itself").at(Target::Operand(1));
        }
    }
}
pliron_lsp_api::lint!(self_add);

fn use_count(ctx: &Context, op: Ptr<Operation>) -> Option<String> {
    let o = op.deref(ctx);
    (o.get_num_results() == 1).then(|| format!("Result used {} time(s).", o.get_result(0).num_uses(ctx)))
}
pliron_lsp_api::hover!(use_count);

fn unused(ctx: &Context, op: Ptr<Operation>, hints: &mut InlayHints) {
    let o = op.deref(ctx);
    if o.get_num_results() == 1 && !o.get_result(0).is_used(ctx) {
        hints.add(Target::Op, "(unused)");
    }
}
pliron_lsp_api::inlay!(unused);

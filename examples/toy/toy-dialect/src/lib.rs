//! A toy dialect used to test pliron-lsp's automatic dialect bundles.

use pliron::builtin::op_interfaces::{
    IsTerminatorInterface, NOpdsInterface, NResultsInterface, OneResultInterface, SameOperandsAndResultType,
    SameOperandsType, SameResultsType,
};
use pliron::derive::{pliron_op, pliron_type};

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

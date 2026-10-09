//! Round trip: print the parsed IR with the dialects' printers, parse the
//! printed text again and compare, operation by operation. Differences are
//! printer/parser bugs of a dialect.
//!
//! The printer gives every operation and block with a known location an
//! outlined `@[<in-memory>: line: L, column: C]` entry; re-parsing without
//! `keep_parsed_locations` puts those locations on the re-parsed entities,
//! so each one is matched with the original at its position.

use std::collections::{BTreeMap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};

use pliron::basic_block::BasicBlock;
use pliron::combine::stream::position::SourcePosition;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::location::{Located, Location};
use pliron::lsp::{Event, RecordOptions, parse_recorded};
use pliron::operation::Operation;
use pliron::printable::Printable;
use pliron::r#type::Typed;
use pliron_lsp_protocol::{DiagPhase, EngineDiag, Pos};

/// At most this many differences are listed per operation.
const MAX_DIFFS: usize = 4;

fn src_pos(loc: &Location) -> Option<Pos> {
    match loc {
        Location::SrcPos { pos, .. } => Some(Pos {
            line: pos.line.max(1) as u32,
            column: pos.column.max(1) as u32,
        }),
        _ => None,
    }
}

fn render<T: Printable + ?Sized>(ctx: &Context, t: &T) -> String {
    t.disp(ctx)
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Operations in pre-order.
fn ops_of(ctx: &Context, top: Ptr<Operation>) -> Vec<Ptr<Operation>> {
    fn walk(ctx: &Context, op: Ptr<Operation>, out: &mut Vec<Ptr<Operation>>) {
        out.push(op);
        let regions: Vec<_> = op.deref(ctx).regions().collect();
        for r in regions {
            let blocks: Vec<_> = r.deref(ctx).iter(ctx).collect();
            for b in blocks {
                let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
                for o in ops {
                    walk(ctx, o, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(ctx, top, &mut out);
    out
}

/// What must survive printing and re-parsing.
#[derive(PartialEq, Eq, Debug)]
struct Shape {
    opid: String,
    results: Vec<String>,
    operands: Vec<String>,
    successors: usize,
    /// Regions -> blocks -> argument types.
    regions: Vec<Vec<Vec<String>>>,
    attrs: BTreeMap<String, String>,
}

fn shape(ctx: &Context, op: Ptr<Operation>) -> Shape {
    let o = op.deref(ctx);
    let block_args = |b: Ptr<BasicBlock>| -> Vec<String> {
        b.deref(ctx)
            .arguments()
            .map(|v| render(ctx, &v.get_type(ctx)))
            .collect()
    };
    Shape {
        opid: Operation::get_opid(op, ctx).to_string(),
        results: o.results().map(|v| render(ctx, &v.get_type(ctx))).collect(),
        operands: o
            .operands()
            .map(|v| render(ctx, &v.get_type(ctx)))
            .collect(),
        successors: o.successors().count(),
        regions: o
            .regions()
            .map(|r| r.deref(ctx).iter(ctx).map(block_args).collect())
            .collect(),
        attrs: o
            .attributes
            .0
            .iter()
            .map(|(k, a)| (k.to_string(), render(ctx, a.as_ref())))
            .collect(),
    }
}

fn list(v: &[String]) -> String {
    format!("({})", v.join(", "))
}

/// Human-readable differences between an operation and its re-parsed form.
fn differences(a: &Shape, b: &Shape) -> Vec<String> {
    let mut out = Vec::new();
    if a.opid != b.opid {
        out.push(format!("it becomes `{}`", b.opid));
        return out;
    }
    if a.results != b.results {
        out.push(format!(
            "result types {} become {}",
            list(&a.results),
            list(&b.results)
        ));
    }
    if a.operands != b.operands {
        out.push(format!(
            "operand types {} become {}",
            list(&a.operands),
            list(&b.operands)
        ));
    }
    if a.successors != b.successors {
        out.push(format!(
            "{} successor(s) become {}",
            a.successors, b.successors
        ));
    }
    if a.regions.len() != b.regions.len() {
        out.push(format!(
            "{} region(s) become {}",
            a.regions.len(),
            b.regions.len()
        ));
    } else {
        for (i, (ra, rb)) in a.regions.iter().zip(&b.regions).enumerate() {
            if ra.len() != rb.len() {
                out.push(format!(
                    "region #{i}: {} block(s) become {}",
                    ra.len(),
                    rb.len()
                ));
            } else if let Some((j, (ba, bb))) =
                ra.iter().zip(rb).enumerate().find(|(_, (x, y))| x != y)
            {
                out.push(format!(
                    "region #{i}, block #{j}: arguments {} become {}",
                    list(ba),
                    list(bb)
                ));
            }
        }
    }
    for (k, v) in &a.attrs {
        match b.attrs.get(k) {
            None => out.push(format!("attribute `{k}` ({v}) is lost")),
            Some(w) if w != v => out.push(format!("attribute `{k}`: `{v}` becomes `{w}`")),
            _ => {}
        }
    }
    for (k, w) in &b.attrs {
        if !a.attrs.contains_key(k) {
            out.push(format!("attribute `{k}` = `{w}` appears"));
        }
    }
    out
}

/// Byte offset of a (1-based line, 1-based char column) position.
fn offset(text: &str, p: SourcePosition) -> usize {
    let mut off = 0;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i + 1 == p.line as usize {
            return off
                + line
                    .char_indices()
                    .nth((p.column.max(1) - 1) as usize)
                    .map(|(b, _)| b)
                    .unwrap_or(line.len());
        }
        off += line.len();
    }
    text.len()
}

/// The outline index (`!N`) the printer gave the statement starting at
/// byte `start`: the first `!N` at bracket depth 0 before the statement
/// ends.
fn outline_index_from(text: &str, start: usize) -> Option<usize> {
    let b = text.as_bytes();
    let (mut i, mut depth, mut in_str) = (start, 0i32, false);
    while i < b.len() {
        let c = b[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' | b'(' | b'[' | b'<' => depth += 1,
                // `->` in function types is not a bracket.
                b'>' if i > 0 && b[i - 1] == b'-' => {}
                b'}' | b')' | b']' | b'>' => {
                    depth -= 1;
                    if depth < 0 {
                        return None;
                    }
                }
                b';' if depth == 0 => return None,
                b'\n' if depth == 0 && i > start => {
                    // A statement continues on the next line only inside
                    // brackets.
                    return None;
                }
                b'!' if depth == 0 => {
                    let digits: String = text[i + 1..]
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect();
                    if !digits.is_empty() {
                        return digits.parse().ok();
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// `!N = @[<in-memory>: line: L, column: C]` in the outlined section.
fn outlined_position(text: &str, index: usize) -> Option<Pos> {
    let section = text.rfind("outlined_attributes:")?;
    let prefix = format!("!{index} = @[");
    let line = text[section..]
        .lines()
        .find(|l| l.trim_start().starts_with(&prefix))?;
    let number_after = |key: &str| -> Option<u32> {
        let rest = &line[line.find(key)? + key.len()..];
        rest.trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    Some(Pos {
        line: number_after("line:")?,
        column: number_after("column:")?,
    })
}

fn line_at(text: &str, off: usize) -> &str {
    let s = text[..off.min(text.len())].rfind('\n').map_or(0, |p| p + 1);
    let e = text[s..].find('\n').map_or(text.len(), |p| s + p);
    text[s..e].trim()
}

/// The printed line of an error, with the line before it when the error's
/// line is only a bracket (e.g. a region the printer put on its own line).
fn printed_context(text: &str, off: usize) -> String {
    let line = line_at(text, off);
    let line_start = text[..off.min(text.len())].rfind('\n').map_or(0, |p| p + 1);
    if line.chars().count() <= 2 && line_start > 0 {
        format!("{} {line}", line_at(text, line_start - 1))
    } else {
        line.to_string()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// Print `top`; returns the text, or a diagnostic for the operation whose
/// printer panics.
fn print(ctx: &Context, top: Ptr<Operation>) -> Result<String, EngineDiag> {
    let first = match catch_unwind(AssertUnwindSafe(|| top.deref(ctx).disp(ctx).to_string())) {
        Ok(text) => return Ok(text),
        Err(payload) => crate::panic_message(&*payload),
    };
    // The innermost operation whose printing panics.
    let culprit = ops_of(ctx, top).into_iter().rev().find_map(|op| {
        catch_unwind(AssertUnwindSafe(|| op.deref(ctx).disp(ctx).to_string()))
            .err()
            .map(|payload| (op, crate::panic_message(&*payload)))
    });
    let (pos, message) = match culprit {
        Some((op, m)) => (src_pos(&op.deref(ctx).loc()), m),
        None => (src_pos(&top.deref(ctx).loc()), first),
    };
    Err(EngineDiag {
        phase: DiagPhase::RoundTrip,
        pos,
        message: format!("printing this operation panicked: {message}"),
        op: None,
    })
}

/// Round-trip `top` (parsed from the document, with parsed locations).
/// Returns the printed text and the problems found.
pub fn round_trip(ctx: &Context, top: Ptr<Operation>) -> (Option<String>, Vec<EngineDiag>) {
    let printed = match print(ctx, top) {
        Ok(t) => t,
        Err(d) => return (None, vec![d]),
    };
    let diag = |pos: Option<Pos>, message: String| EngineDiag {
        phase: DiagPhase::RoundTrip,
        pos,
        message,
        op: None,
    };

    let mut ctx2 = Context::new();
    let (res, recording) = parse_recorded(
        &mut ctx2,
        &printed,
        RecordOptions {
            recover: true,
            keep_parsed_locations: false,
        },
    );

    // The printed text does not parse: attribute each error to the
    // operation it is in, through that operation's outline index.
    let mut errors: Vec<(SourcePosition, String)> = recording
        .errors
        .iter()
        .map(|e| (e.pos, e.message.clone()))
        .collect();
    if let Err(e) = &res
        && errors.is_empty()
    {
        let pos = match &e.loc {
            Location::SrcPos { pos, .. } => *pos,
            _ => SourcePosition { line: 1, column: 1 },
        };
        errors.push((pos, e.err.to_string()));
    }
    if !errors.is_empty() {
        let op_errors: Vec<(SourcePosition, SourcePosition)> = recording
            .events
            .iter()
            .filter_map(|ev| match ev {
                Event::OpError { start, end } => Some((*start, *end)),
                _ => None,
            })
            .collect();
        let mut out: Vec<EngineDiag> = Vec::new();
        for (pos, message) in errors {
            let at = offset(&printed, pos);
            // The failed statement around the error, else the error's own.
            let stmt = op_errors
                .iter()
                .map(|(s, e)| (offset(&printed, *s), offset(&printed, *e)))
                .filter(|(s, e)| *s <= at && at <= *e)
                .max_by_key(|(s, _)| *s)
                .map_or(at, |(s, _)| s);
            let original = outline_index_from(&printed, stmt)
                .and_then(|n| outlined_position(&printed, n))
                .or_else(|| src_pos(&top.deref(ctx).loc()));
            let message = format!(
                "pliron cannot parse what it printed for this operation: {}\nprinted as: {}",
                message.lines().next().unwrap_or(""),
                truncate(&printed_context(&printed, at), 160)
            );
            if !out.iter().any(|d| d.pos == original) {
                out.push(diag(original, message));
            }
        }
        return (Some(printed), out);
    }
    let Ok(top2) = res else {
        return (Some(printed), Vec::new());
    };

    // Match operations by their (original) positions and compare.
    let reparsed: HashMap<Pos, Ptr<Operation>> = ops_of(&ctx2, top2)
        .into_iter()
        .filter_map(|op| Some((src_pos(&op.deref(&ctx2).loc())?, op)))
        .collect();
    let mut out = Vec::new();
    for op in ops_of(ctx, top) {
        let Some(pos) = src_pos(&op.deref(ctx).loc()) else {
            continue;
        };
        let Some(op2) = reparsed.get(&pos) else {
            out.push(diag(
                Some(pos),
                "this operation is missing after printing and parsing it again".into(),
            ));
            continue;
        };
        let diffs = differences(&shape(ctx, op), &shape(&ctx2, *op2));
        if !diffs.is_empty() {
            let more = diffs.len().saturating_sub(MAX_DIFFS);
            let mut message = format!(
                "printing and parsing this operation again changes it: {}",
                diffs[..diffs.len().min(MAX_DIFFS)].join("; ")
            );
            if more > 0 {
                message.push_str(&format!(" (and {more} more)"));
            }
            out.push(diag(Some(pos), message));
        }
    }
    (Some(printed), out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_indices_and_positions() {
        let printed = "builtin.module @m {\n  ^e():\n    x = t.a <{k = 1}> : i64 !2;\n    t.f {\n      t.r !4\n    } !3\n} !0\n\noutlined_attributes:\n!0 = @[<in-memory>: line: 1, column: 1], []\n!2 = @[<in-memory>: line: 5, column: 7], []\n!3 = @[<in-memory>: line: 6, column: 3], []\n";
        let at = |needle: &str| printed.find(needle).unwrap();
        assert_eq!(outline_index_from(printed, at("x = t.a")), Some(2));
        // After the region, not the nested op's.
        assert_eq!(outline_index_from(printed, at("t.f")), Some(3));
        assert_eq!(outline_index_from(printed, at("t.r")), Some(4));
        let f = "llvm.func @f: llvm.func <i64 (i64) -> ()> [] {\n  llvm.return !1\n} !7\n";
        assert_eq!(outline_index_from(f, 0), Some(7));
        assert_eq!(
            outlined_position(printed, 2),
            Some(Pos { line: 5, column: 7 })
        );
        assert_eq!(outlined_position(printed, 9), None);
    }

    /// Test ops with buggy printers.
    mod buggy {
        use pliron::builtin::attributes::StringAttr;
        use pliron::builtin::op_interfaces::{NOpdsInterface, NResultsInterface};
        use pliron::combine::{Parser, many1, optional, parser::char::digit};
        use pliron::context::Context;
        use pliron::derive::pliron_op;
        use pliron::dict_key;
        use pliron::identifier::Identifier;
        use pliron::irfmt::parsers::spaced;
        use pliron::location::Location;
        use pliron::op::{Op, OpObj};
        use pliron::operation::Operation;
        use pliron::parsable::{Parsable, ParseResult, StateStream};
        use pliron::printable::{self, Printable};

        dict_key!(RT_COUNT, "rt_count");

        /// `rt.lossy [N]`, printed without its `N`.
        #[pliron_op(
            name = "rt.lossy",
            interfaces = [NOpdsInterface<0>, NResultsInterface<0>],
            verifier = "succ"
        )]
        pub struct LossyOp;

        impl Printable for LossyOp {
            fn fmt(
                &self,
                _: &Context,
                _: &printable::State,
                f: &mut core::fmt::Formatter<'_>,
            ) -> core::fmt::Result {
                write!(f, "{}", Self::get_opid_static())
            }
        }

        impl Parsable for LossyOp {
            type Arg = Vec<(Identifier, Location)>;
            type Parsed = OpObj;
            fn parse<'a>(
                state_stream: &mut StateStream<'a>,
                _: Self::Arg,
            ) -> ParseResult<'a, OpObj> {
                let op = Operation::new(
                    state_stream.state.ctx,
                    Self::get_concrete_op_info(),
                    vec![],
                    vec![],
                    vec![],
                    0,
                );
                optional(spaced(many1::<String, _, _>(digit())))
                    .parse_stream(state_stream)
                    .map(|n| -> OpObj {
                        if let Some(n) = n {
                            op.deref_mut(state_stream.state.ctx)
                                .attributes
                                .set(RT_COUNT.clone(), StringAttr::new(n));
                        }
                        OpObj::new(LossyOp { op })
                    })
                    .into()
            }
        }

        /// `rt.garbled`, printed as something its parser rejects.
        #[pliron_op(
            name = "rt.garbled",
            interfaces = [NOpdsInterface<0>, NResultsInterface<0>],
            verifier = "succ"
        )]
        pub struct GarbledOp;

        impl Printable for GarbledOp {
            fn fmt(
                &self,
                _: &Context,
                _: &printable::State,
                f: &mut core::fmt::Formatter<'_>,
            ) -> core::fmt::Result {
                write!(f, "{} ???", Self::get_opid_static())
            }
        }

        impl Parsable for GarbledOp {
            type Arg = Vec<(Identifier, Location)>;
            type Parsed = OpObj;
            fn parse<'a>(
                state_stream: &mut StateStream<'a>,
                _: Self::Arg,
            ) -> ParseResult<'a, OpObj> {
                let op = Operation::new(
                    state_stream.state.ctx,
                    Self::get_concrete_op_info(),
                    vec![],
                    vec![],
                    vec![],
                    0,
                );
                Ok(OpObj::new(GarbledOp { op })).into_parse_result()
            }
        }

        use pliron::parsable::IntoParseResult;
    }

    fn run(text: &str) -> pliron_lsp_protocol::AnalyzeResult {
        crate::analyze(&pliron_lsp_protocol::AnalyzeParams {
            text_hash: 0,
            text: text.to_string(),
            verify: pliron_lsp_protocol::VerifyMode::All,
            want_model: false,
            max_attr_len: 200,
            round_trip: true,
        })
    }

    #[test]
    fn finds_lossy_and_unparsable_printers() {
        let lossy = run("builtin.module @m {\n  ^entry():\n  rt.lossy;\n  rt.lossy 3\n}\n");
        assert!(
            lossy.parse_errors.is_empty() && lossy.verify_errors.is_empty(),
            "{lossy:#?}"
        );
        assert_eq!(lossy.round_trip.len(), 1, "{:#?}", lossy.round_trip);
        let d = &lossy.round_trip[0];
        assert_eq!(d.pos, Some(Pos { line: 4, column: 3 }));
        assert!(
            d.message.contains("attribute `rt_count`") && d.message.contains("is lost"),
            "{}",
            d.message
        );

        let garbled = run("builtin.module @m {\n  ^entry():\n  rt.lossy;\n  rt.garbled\n}\n");
        assert_eq!(garbled.round_trip.len(), 1, "{:#?}", garbled.round_trip);
        let d = &garbled.round_trip[0];
        assert_eq!(d.pos, Some(Pos { line: 4, column: 3 }));
        assert!(
            d.message.contains("cannot parse what it printed"),
            "{}",
            d.message
        );
        assert!(
            d.message.contains("printed as: rt.garbled ???"),
            "{}",
            d.message
        );
        assert!(garbled.printed.unwrap().contains("rt.garbled ???"));
    }
}

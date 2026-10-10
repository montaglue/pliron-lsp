//! Passes the editor can run on a document: pliron's own, and those that
//! dialects register with `pliron-lsp-api` (`pass!`).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::location::{Located, Location};
use pliron::lsp::{RecordOptions, parse_recorded};
use pliron::operation::{Operation, verify_operation};
use pliron::printable::Printable;
use pliron_lsp_protocol::{PassInfo, PassResult, RunPassParams};

type Run = fn(&mut Context, Ptr<Operation>) -> Result<(), String>;

/// pliron's passes (their APIs differ between pliron versions).
fn builtins() -> Vec<(&'static str, &'static str, Run)> {
    #[allow(unused_mut)]
    let mut out: Vec<(&'static str, &'static str, Run)> = Vec::new();
    #[cfg(not(feature = "pliron_0_16"))]
    out.push((
        "pliron.dce",
        "Dead code elimination: removes operations without side effects whose results are unused (pliron::opts::dce)",
        |ctx, op| {
            pliron::opts::dce::dce(op, ctx)
                .map(|_| ())
                .map_err(|e| e.err.to_string())
        },
    ));
    out
}

pub fn list() -> Vec<PassInfo> {
    builtins()
        .into_iter()
        .map(|(name, description, _)| PassInfo {
            name: name.into(),
            description: description.into(),
        })
        .chain(crate::hooks::pass_infos())
        .collect()
}

fn run_named(name: &str, ctx: &mut Context, op: Ptr<Operation>) -> Option<Result<(), String>> {
    if let Some((_, _, run)) = builtins().into_iter().find(|(n, _, _)| *n == name) {
        return Some(run(ctx, op));
    }
    crate::hooks::run_pass(name, ctx, op)
}

/// Forget source locations, so that printing does not outline them (the
/// before/after texts compare better without).
fn strip_locations(ctx: &mut Context, op: Ptr<Operation>) {
    op.deref_mut(ctx).set_loc(Location::Unknown);
    let regions: Vec<_> = op.deref(ctx).regions().collect();
    for r in regions {
        let blocks: Vec<_> = r.deref(ctx).iter(ctx).collect();
        for b in blocks {
            b.deref_mut(ctx).set_loc(Location::Unknown);
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            for o in ops {
                strip_locations(ctx, o);
            }
        }
    }
}

fn print(ctx: &Context, op: Ptr<Operation>) -> Result<String, String> {
    catch_unwind(AssertUnwindSafe(|| op.deref(ctx).disp(ctx).to_string()))
        .map(|t| without_name_outlines(&t))
        .map_err(|p| format!("printing panicked: {}", crate::panic_message(&*p)))
}

/// pliron outlines the names of values (`!N = [builtin_given_names = ...]`)
/// and numbers the entries in print order, so removing one op renumbers all
/// later ones. The names are already in the printed values (`c_v1`): drop
/// these entries and their `!N` references, keeping other outlined
/// attributes.
fn without_name_outlines(printed: &str) -> String {
    const HEADER: &str = "outlined_attributes:";
    let Some(at) = printed.rfind(HEADER) else {
        return printed.to_string();
    };
    let (ir, section) = printed.split_at(at);
    let mut names_only = std::collections::HashSet::new();
    let mut kept = Vec::new();
    for line in section[HEADER.len()..]
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let entry = line.split_once(" = ").and_then(|(n, rest)| {
            let n: usize = n.trim().strip_prefix('!')?.parse().ok()?;
            let only_names = rest.trim().starts_with("[builtin_given_names = ")
                && rest.matches(" = ").count() == 1;
            Some((n, only_names))
        });
        match entry {
            Some((n, true)) => {
                names_only.insert(n);
            }
            _ => kept.push(line),
        }
    }
    // ` !N` references, followed by a separator.
    let mut out = String::with_capacity(ir.len());
    let mut rest = ir;
    while let Some(i) = rest.find(" !") {
        let digits: String = rest[i + 2..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let after = rest[i + 2 + digits.len()..].chars().next();
        let ends = matches!(after, None | Some(';' | ':' | '\n' | ' ' | '\r'));
        match digits.parse::<usize>() {
            Ok(n) if ends && names_only.contains(&n) => {
                out.push_str(&rest[..i]);
                rest = &rest[i + 2 + digits.len()..];
            }
            _ => {
                out.push_str(&rest[..i + 2]);
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    let mut out = out.trim_end().to_string();
    out.push('\n');
    if !kept.is_empty() {
        out.push('\n');
        out.push_str(HEADER);
        out.push('\n');
        for l in kept {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// Parse and verify the document, then print it, run the pass and print it
/// again.
pub fn run(params: &RunPassParams) -> PassResult {
    let started = Instant::now();
    let mut result = PassResult::default();
    let mut ctx = Context::new();
    let (res, recording) = parse_recorded(
        &mut ctx,
        &params.text,
        RecordOptions {
            recover: false,
            keep_parsed_locations: true,
        },
    );
    let top = match (res, recording.errors.first()) {
        (Ok(top), None) => top,
        (_, Some(e)) => {
            result
                .errors
                .push(format!("the document does not parse: {}", e.message));
            return result;
        }
        (Err(e), None) => {
            result
                .errors
                .push(format!("the document does not parse: {}", e.err));
            return result;
        }
    };
    if let Err(e) = verify_operation(top, &ctx) {
        result
            .errors
            .push(format!("the document does not verify: {}", e.err));
        return result;
    }
    strip_locations(&mut ctx, top);
    match print(&ctx, top) {
        Ok(t) => result.before = Some(t),
        Err(e) => {
            result.errors.push(e);
            return result;
        }
    }

    match catch_unwind(AssertUnwindSafe(|| run_named(&params.pass, &mut ctx, top))) {
        Ok(None) => {
            result
                .errors
                .push(format!("there is no pass `{}`", params.pass));
            return result;
        }
        Ok(Some(Ok(()))) => {}
        Ok(Some(Err(e))) => result.errors.push(format!("the pass failed: {e}")),
        Err(p) => {
            result
                .errors
                .push(format!("the pass panicked: {}", crate::panic_message(&*p)));
            return result;
        }
    }
    if let Err(e) = verify_operation(top, &ctx) {
        result
            .errors
            .push(format!("the result does not verify: {}", e.err));
    }
    match print(&ctx, top) {
        Ok(t) => result.after = Some(t),
        Err(e) => result.errors.push(e),
    }
    result.elapsed_us = started.elapsed().as_micros() as u64;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &str = "builtin.module @m {\n  ^entry():\n  llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {\n    ^entry(x: builtin.integer i64):\n    c = llvm.icmp x <SLT> x : builtin.integer i1;\n    llvm.return x\n  }\n}\n";

    #[test]
    fn runs_dce() {
        use pliron_llvm as _;
        assert!(list().iter().any(|p| p.name == "pliron.dce"));
        let r = run(&RunPassParams {
            text: DEMO.into(),
            pass: "pliron.dce".into(),
        });
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let (before, after) = (r.before.unwrap(), r.after.unwrap());
        assert!(
            before.contains("llvm.icmp") && !after.contains("llvm.icmp"),
            "{after}"
        );
        // No outlined locations or names: the texts compare well.
        assert!(
            !before.contains("outlined_attributes") && !before.contains(" !"),
            "{before}"
        );
        assert!(before.contains("c_v1 = llvm.icmp"), "{before}");

        let r = run(&RunPassParams {
            text: DEMO.into(),
            pass: "nope".into(),
        });
        assert_eq!(r.errors, ["there is no pass `nope`"]);
        let r = run(&RunPassParams {
            text: "builtin.module @m {".into(),
            pass: "pliron.dce".into(),
        });
        assert!(
            r.errors[0].starts_with("the document does not parse"),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn drops_only_name_outlines() {
        let printed = "x_v0 = t.a !0;\n  t.b !1;\n  t.c !2:\n\noutlined_attributes:\n!0 = [builtin_given_names = builtin.given_names [x]]\n!1 = [k = t.attr <3>]\n!2 = [builtin_given_names = builtin.given_names [y]]\n";
        assert_eq!(
            without_name_outlines(printed),
            "x_v0 = t.a;\n  t.b !1;\n  t.c:\n\noutlined_attributes:\n!1 = [k = t.attr <3>]\n"
        );
    }
}

//! Hooks that dialects register with `pliron-lsp-api` (lints, hover notes
//! and inlay hints). Without the `hooks` feature there are none.

use pliron::context::{Context, Ptr};
use pliron::operation::Operation;
use pliron_lsp_protocol::{HookDiag, HookHint};

#[cfg(feature = "hooks")]
mod imp {
    use std::any::Any;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use pliron_lsp_api::__private::{HOVERS, INLAYS, LINTS};
    use pliron_lsp_api::{Diagnostics, InlayHints, Severity, Target};
    use pliron_lsp_protocol::{HookSeverity, HookTarget};

    use super::*;

    fn target(t: Target) -> HookTarget {
        match t {
            Target::OpName => HookTarget::OpName,
            Target::Op => HookTarget::Op,
            Target::Result(i) => HookTarget::Result { index: i as u32 },
            Target::Operand(i) => HookTarget::Operand { index: i as u32 },
        }
    }

    pub fn names() -> Vec<String> {
        let lints = LINTS.iter().map(|h| format!("lint {}", h.name));
        let hovers = HOVERS.iter().map(|h| format!("hover {}", h.name));
        let inlays = INLAYS.iter().map(|h| format!("inlay {}", h.name));
        lints.chain(hovers).chain(inlays).collect()
    }

    pub fn lint(ctx: &Context, op: Ptr<Operation>, id: u32, out: &mut Vec<HookDiag>) {
        for h in LINTS.iter() {
            let mut diags = Diagnostics::default();
            let run = catch_unwind(AssertUnwindSafe(|| {
                (h.run)(ctx as &dyn Any, &op as &dyn Any, &mut diags)
            }));
            for d in diags.take() {
                out.push(HookDiag {
                    op: id,
                    target: target(d.target),
                    severity: match d.severity {
                        Severity::Error => HookSeverity::Error,
                        Severity::Warning => HookSeverity::Warning,
                        Severity::Info => HookSeverity::Info,
                        Severity::Hint => HookSeverity::Hint,
                    },
                    message: d.message,
                    source: h.name.to_string(),
                });
            }
            if let Err(payload) = run {
                out.push(HookDiag {
                    op: id,
                    target: HookTarget::OpName,
                    severity: HookSeverity::Warning,
                    message: format!(
                        "lint `{}` panicked: {}",
                        h.name,
                        crate::panic_message(&*payload)
                    ),
                    source: h.name.to_string(),
                });
            }
        }
    }

    pub fn hover(ctx: &Context, op: Ptr<Operation>) -> Vec<String> {
        HOVERS
            .iter()
            .filter_map(|h| {
                catch_unwind(AssertUnwindSafe(|| {
                    (h.run)(ctx as &dyn Any, &op as &dyn Any)
                }))
                .unwrap_or_else(|payload| {
                    Some(format!(
                        "*hover hook `{}` panicked: {}*",
                        h.name,
                        crate::panic_message(&*payload)
                    ))
                })
            })
            .collect()
    }

    pub fn inlay(ctx: &Context, op: Ptr<Operation>, id: u32, out: &mut Vec<HookHint>) {
        for h in INLAYS.iter() {
            let mut hints = InlayHints::default();
            let _ = catch_unwind(AssertUnwindSafe(|| {
                (h.run)(ctx as &dyn Any, &op as &dyn Any, &mut hints)
            }));
            out.extend(hints.take().into_iter().map(|hint| HookHint {
                op: id,
                target: target(hint.target),
                label: hint.label,
            }));
        }
    }

    pub fn any() -> bool {
        !(LINTS.is_empty() && HOVERS.is_empty() && INLAYS.is_empty())
    }
}

#[cfg(not(feature = "hooks"))]
mod imp {
    use super::*;

    pub fn names() -> Vec<String> {
        Vec::new()
    }

    pub fn lint(_: &Context, _: Ptr<Operation>, _: u32, _: &mut Vec<HookDiag>) {}

    pub fn hover(_: &Context, _: Ptr<Operation>) -> Vec<String> {
        Vec::new()
    }

    pub fn inlay(_: &Context, _: Ptr<Operation>, _: u32, _: &mut Vec<HookHint>) {}

    pub fn any() -> bool {
        false
    }
}

pub use imp::{any, hover, inlay, lint, names};

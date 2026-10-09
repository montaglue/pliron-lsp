//! Optional customization of [pliron-lsp] for pliron dialect crates.
//!
//! pliron-lsp does not need this crate. It finds a project's dialects,
//! builds them into an analysis engine and reads their Rust sources for
//! docs and formats on its own. Depend on this crate only to change or
//! extend that behaviour:
//!
//! | What | Macro | Takes effect |
//! |---|---|---|
//! | Docs, completion snippets, signature help and syntax facts, e.g. for ops with a hand-written parser | [`hints!`] | immediately (read from the source) |
//! | Highlighting of hand-written syntax | [`keyword!`], [`token!`] | after the engine rebuild |
//! | Extra diagnostics, in the editor and in `pliron-lsp check` | [`lint!`] | after the engine rebuild |
//! | Extra hover text for operations | [`hover!`] | after the engine rebuild |
//! | Extra inlay hints | [`inlay!`] | after the engine rebuild |
//!
//! In normal builds the macros only type-check their arguments: this crate
//! has no dependencies and adds no code to a dialect. The engine that
//! pliron-lsp builds for a project turns the hooks on (through the internal
//! `engine` feature).
//!
//! [pliron-lsp]: https://github.com/montaglue/pliron-lsp

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// What a diagnostic or an inlay hint is attached to, relative to the
/// operation the hook was called with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The operation's name (`dialect.op`).
    OpName,
    /// The whole operation. An inlay hint goes after its end.
    Op,
    /// The name of the operation's `i`-th result.
    Result(usize),
    /// The `i`-th operand, where this operation uses it. An inlay hint goes
    /// before it.
    Operand(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub target: Target,
}

impl Diagnostic {
    /// Attach the diagnostic to `target` (the default is [`Target::OpName`]).
    pub fn at(&mut self, target: Target) -> &mut Self {
        self.target = target;
        self
    }
}

/// Diagnostics reported by a [`lint!`] hook.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn push(&mut self, severity: Severity, message: impl Into<String>) -> &mut Diagnostic {
        self.items.push(Diagnostic {
            severity,
            message: message.into(),
            target: Target::OpName,
        });
        self.items.last_mut().expect("just pushed")
    }

    pub fn error(&mut self, message: impl Into<String>) -> &mut Diagnostic {
        self.push(Severity::Error, message)
    }

    pub fn warning(&mut self, message: impl Into<String>) -> &mut Diagnostic {
        self.push(Severity::Warning, message)
    }

    pub fn info(&mut self, message: impl Into<String>) -> &mut Diagnostic {
        self.push(Severity::Info, message)
    }

    pub fn hint(&mut self, message: impl Into<String>) -> &mut Diagnostic {
        self.push(Severity::Hint, message)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[doc(hidden)]
    pub fn take(&mut self) -> Vec<Diagnostic> {
        core::mem::take(&mut self.items)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayHint {
    pub target: Target,
    pub label: String,
}

/// Inlay hints added by an [`inlay!`] hook.
#[derive(Debug, Default)]
pub struct InlayHints {
    items: Vec<InlayHint>,
}

impl InlayHints {
    /// Show `label` next to `target` (see [`Target`] for where).
    pub fn add(&mut self, target: Target, label: impl Into<String>) {
        self.items.push(InlayHint {
            target,
            label: label.into(),
        });
    }

    pub fn iter(&self) -> impl Iterator<Item = &InlayHint> {
        self.items.iter()
    }

    #[doc(hidden)]
    pub fn take(&mut self) -> Vec<InlayHint> {
        core::mem::take(&mut self.items)
    }
}

/// Semantic token types for [`token!`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Operator,
    Number,
    String,
    Variable,
    Parameter,
    Function,
    Type,
    EnumMember,
    Property,
    Namespace,
    Label,
    Macro,
}

impl TokenKind {
    /// The LSP semantic token type name.
    pub const fn as_str(self) -> &'static str {
        match self {
            TokenKind::Keyword => "keyword",
            TokenKind::Operator => "operator",
            TokenKind::Number => "number",
            TokenKind::String => "string",
            TokenKind::Variable => "variable",
            TokenKind::Parameter => "parameter",
            TokenKind::Function => "function",
            TokenKind::Type => "type",
            TokenKind::EnumMember => "enumMember",
            TokenKind::Property => "property",
            TokenKind::Namespace => "namespace",
            TokenKind::Label => "label",
            TokenKind::Macro => "macro",
        }
    }
}

/// Static information about operations, types and attributes, read by
/// pliron-lsp straight from the source (no build needed). It replaces what
/// pliron-lsp derives automatically from `#[pliron_op(...)]` and doc
/// comments, which matters most for items with a hand-written parser.
///
/// Each entry is `op`, `type` or `attr`, the `dialect.name`, and any of
/// these keys:
///
/// | Key | Value | Used for |
/// |---|---|---|
/// | `doc` | markdown | hover and completion docs (replaces the doc comment) |
/// | `format` | a pliron format string, e.g. ``"$0 `to` $1 `:` type($0)"`` | signature help, completion and docs; it only describes the syntax |
/// | `snippet` | LSP snippet of what follows the name, e.g. `"${1:lhs}, ${2:rhs}"` | completion (replaces the one derived from the format) |
/// | `operands` | operand names, e.g. `["lhs", "rhs"]` | signature help |
/// | `isolated` | `bool`: implements `IsolatedFromAboveInterface` | name scoping before the engine is built |
/// | `symbol` | `bool`: the `@name` in it defines a symbol | go to definition before the engine is built |
/// | `keywords` | words of its syntax, e.g. `["to", "step"]` | so they are not taken for value names before the engine is built |
///
/// ```
/// pliron_lsp_api::hints! {
///     op "mydialect.for" {
///         format: "$0 `to` $1 `step` $2 region($0)",
///         snippet: "${1:lb} to ${2:ub} step ${3:1} {\n\t^body(${4:i}: ${5:i64}):\n\t$0\n}",
///         operands: ["lb", "ub", "step"],
///         keywords: ["to", "step"],
///     }
///     type "mydialect.vec" {
///         doc: "A fixed-size vector, e.g. `mydialect.vec <4 x builtin.integer i32>`.",
///     }
/// }
/// ```
///
/// Unknown keys and kinds, and names without a dialect, do not compile:
///
/// ```compile_fail
/// pliron_lsp_api::hints! { op "mydialect.for" { colour: "red" } }
/// ```
/// ```compile_fail
/// pliron_lsp_api::hints! { region "mydialect.for" { doc: "" } }
/// ```
/// ```compile_fail
/// pliron_lsp_api::hints! { op "for" { doc: "" } }
/// ```
#[macro_export]
macro_rules! hints {
    () => {};
    ($kind:tt $name:literal { $($key:ident : $value:expr),* $(,)? } $($rest:tt)*) => {
        $crate::__hint_kind!($kind);
        const _: () = {
            $crate::__private::name($name);
            $( $crate::__private::keys::$key($value); )*
        };
        $crate::hints!($($rest)*);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __hint_kind {
    (op) => {};
    (type) => {};
    (attr) => {};
    ($other:tt) => {
        ::core::compile_error!(::core::concat!(
            "pliron_lsp_api::hints!: expected `op`, `type` or `attr`, found `",
            ::core::stringify!($other),
            "`"
        ));
    };
}

/// Register a lint: a function called for every operation that passed
/// verification, which can report extra diagnostics. They appear in the
/// editor and in `pliron-lsp check`.
///
/// ```
/// use pliron::context::{Context, Ptr};
/// use pliron::operation::Operation;
/// use pliron_lsp_api::{Diagnostics, Target};
///
/// /// Results that nothing uses.
/// fn unused_results(ctx: &Context, op: Ptr<Operation>, diags: &mut Diagnostics) {
///     let o = op.deref(ctx);
///     for i in 0..o.get_num_results() {
///         if !o.get_result(i).is_used(ctx) {
///             diags.hint("this result is never used").at(Target::Result(i));
///         }
///     }
/// }
/// pliron_lsp_api::lint!(unused_results);
/// ```
#[cfg(not(feature = "engine"))]
#[macro_export]
macro_rules! lint {
    ($f:path) => {
        const _: fn(
            &::pliron::context::Context,
            ::pliron::context::Ptr<::pliron::operation::Operation>,
            &mut $crate::Diagnostics,
        ) = $f;
    };
}

/// Register a hover hook: markdown shown when hovering an operation.
///
/// ```
/// use pliron::context::{Context, Ptr};
/// use pliron::operation::Operation;
///
/// fn operand_count(ctx: &Context, op: Ptr<Operation>) -> Option<String> {
///     let n = op.deref(ctx).get_num_operands();
///     (n > 0).then(|| format!("{n} operand(s)"))
/// }
/// pliron_lsp_api::hover!(operand_count);
/// ```
#[cfg(not(feature = "engine"))]
#[macro_export]
macro_rules! hover {
    ($f:path) => {
        const _: fn(
            &::pliron::context::Context,
            ::pliron::context::Ptr<::pliron::operation::Operation>,
        ) -> ::core::option::Option<::pliron::alloc::string::String> = $f;
    };
}

/// Register an inlay hint hook, called for every operation.
///
/// ```
/// use pliron::context::{Context, Ptr};
/// use pliron::operation::Operation;
/// use pliron_lsp_api::{InlayHints, Target};
///
/// fn unused(ctx: &Context, op: Ptr<Operation>, hints: &mut InlayHints) {
///     let o = op.deref(ctx);
///     if o.get_num_results() > 0 && !o.get_result(0).is_used(ctx) {
///         hints.add(Target::Op, "(unused)");
///     }
/// }
/// pliron_lsp_api::inlay!(unused);
/// ```
#[cfg(not(feature = "engine"))]
#[macro_export]
macro_rules! inlay {
    ($f:path) => {
        const _: fn(
            &::pliron::context::Context,
            ::pliron::context::Ptr<::pliron::operation::Operation>,
            &mut $crate::InlayHints,
        ) = $f;
    };
}

/// A literal keyword parser for hand-written `Parsable` impls, like
/// `combine::parser::char::string`, that pliron-lsp highlights as a
/// keyword. (Declarative formats get this automatically.)
///
/// ```
/// use pliron::combine::{Parser, many1, parser::char::digit};
/// use pliron::parsable::StateStream;
///
/// fn step<'a>() -> impl Parser<StateStream<'a>, Output = String> + 'a {
///     pliron_lsp_api::keyword!("step")
///         .with(pliron_lsp_api::token!(Number, many1::<String, _, _>(digit())))
/// }
/// ```
#[cfg(not(feature = "engine"))]
#[macro_export]
macro_rules! keyword {
    ($lit:expr) => {
        ::pliron::combine::parser::char::string($lit)
    };
}

/// Wrap a parser so that pliron-lsp highlights what it parses as a
/// [`TokenKind`] (written without the `TokenKind::` prefix). See
/// [`keyword!`] for an example.
#[cfg(not(feature = "engine"))]
#[macro_export]
macro_rules! token {
    ($kind:ident, $parser:expr) => {{
        let _ = $crate::TokenKind::$kind;
        $parser
    }};
}

#[cfg(feature = "engine")]
#[macro_export]
macro_rules! lint {
    ($f:path) => {
        const _: () = {
            #[$crate::__private::linkme::distributed_slice($crate::__private::LINTS)]
            #[linkme(crate = $crate::__private::linkme)]
            static HOOK: $crate::__private::LintHook = $crate::__private::LintHook {
                name: ::core::concat!(::core::module_path!(), "::", ::core::stringify!($f)),
                run: |ctx, op, out| {
                    let f: fn(
                        &::pliron::context::Context,
                        ::pliron::context::Ptr<::pliron::operation::Operation>,
                        &mut $crate::Diagnostics,
                    ) = $f;
                    if let (::core::option::Option::Some(ctx), ::core::option::Option::Some(op)) = (
                        ctx.downcast_ref::<::pliron::context::Context>(),
                        op.downcast_ref::<::pliron::context::Ptr<::pliron::operation::Operation>>(),
                    ) {
                        f(ctx, *op, out)
                    }
                },
            };
        };
    };
}

#[cfg(feature = "engine")]
#[macro_export]
macro_rules! hover {
    ($f:path) => {
        const _: () = {
            #[$crate::__private::linkme::distributed_slice($crate::__private::HOVERS)]
            #[linkme(crate = $crate::__private::linkme)]
            static HOOK: $crate::__private::HoverHook = $crate::__private::HoverHook {
                name: ::core::concat!(::core::module_path!(), "::", ::core::stringify!($f)),
                run: |ctx, op| {
                    let f: fn(
                        &::pliron::context::Context,
                        ::pliron::context::Ptr<::pliron::operation::Operation>,
                    )
                        -> ::core::option::Option<::pliron::alloc::string::String> = $f;
                    match (
                        ctx.downcast_ref::<::pliron::context::Context>(),
                        op.downcast_ref::<::pliron::context::Ptr<::pliron::operation::Operation>>(),
                    ) {
                        (::core::option::Option::Some(ctx), ::core::option::Option::Some(op)) => {
                            f(ctx, *op)
                        }
                        _ => ::core::option::Option::None,
                    }
                },
            };
        };
    };
}

#[cfg(feature = "engine")]
#[macro_export]
macro_rules! inlay {
    ($f:path) => {
        const _: () = {
            #[$crate::__private::linkme::distributed_slice($crate::__private::INLAYS)]
            #[linkme(crate = $crate::__private::linkme)]
            static HOOK: $crate::__private::InlayHook = $crate::__private::InlayHook {
                name: ::core::concat!(::core::module_path!(), "::", ::core::stringify!($f)),
                run: |ctx, op, out| {
                    let f: fn(
                        &::pliron::context::Context,
                        ::pliron::context::Ptr<::pliron::operation::Operation>,
                        &mut $crate::InlayHints,
                    ) = $f;
                    if let (::core::option::Option::Some(ctx), ::core::option::Option::Some(op)) = (
                        ctx.downcast_ref::<::pliron::context::Context>(),
                        op.downcast_ref::<::pliron::context::Ptr<::pliron::operation::Operation>>(),
                    ) {
                        f(ctx, *op, out)
                    }
                },
            };
        };
    };
}

#[cfg(feature = "engine")]
#[macro_export]
macro_rules! keyword {
    ($lit:expr) => {
        ::pliron::lsp::keyword($lit)
    };
}

#[cfg(feature = "engine")]
#[macro_export]
macro_rules! token {
    ($kind:ident, $parser:expr) => {
        ::pliron::lsp::token($crate::TokenKind::$kind.as_str(), $parser)
    };
}

#[doc(hidden)]
pub mod __private {
    use core::any::Any;

    /// Fails to compile unless `name` looks like `dialect.name`.
    pub const fn name(name: &str) {
        let b = name.as_bytes();
        let mut i = 0;
        let mut dot = false;
        while i < b.len() {
            if b[i] == b'.' && i > 0 && i + 1 < b.len() {
                dot = true;
            }
            i += 1;
        }
        assert!(
            dot,
            "pliron_lsp_api::hints!: names look like `dialect.name`"
        );
    }

    /// The keys accepted by [`hints!`](crate::hints) (an unknown key fails
    /// to resolve here).
    pub mod keys {
        pub const fn doc(_: &str) {}
        pub const fn format(_: &str) {}
        pub const fn snippet(_: &str) {}
        pub const fn operands<const N: usize>(_: [&str; N]) {}
        pub const fn isolated(_: bool) {}
        pub const fn symbol(_: bool) {}
        pub const fn keywords<const N: usize>(_: [&str; N]) {}
    }

    pub struct LintHook {
        pub name: &'static str,
        pub run: fn(&dyn Any, &dyn Any, &mut crate::Diagnostics),
    }

    pub struct HoverHook {
        pub name: &'static str,
        pub run: fn(&dyn Any, &dyn Any) -> Option<alloc::string::String>,
    }

    pub struct InlayHook {
        pub name: &'static str,
        pub run: fn(&dyn Any, &dyn Any, &mut crate::InlayHints),
    }

    #[cfg(feature = "engine")]
    pub use linkme;

    #[cfg(feature = "engine")]
    #[linkme::distributed_slice]
    pub static LINTS: [LintHook];

    #[cfg(feature = "engine")]
    #[linkme::distributed_slice]
    pub static HOVERS: [HoverHook];

    #[cfg(feature = "engine")]
    #[linkme::distributed_slice]
    pub static INLAYS: [InlayHook];
}

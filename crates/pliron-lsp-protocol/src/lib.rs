//! Wire protocol between the `pliron-lsp` frontend and dialect engines.
//!
//! An engine is a native binary that links pliron plus a project's dialect
//! crates. The frontend talks to it over stdin/stdout using JSON lines.
//! Every protocol line written by the engine is prefixed with
//! [`LINE_MARKER`], so that stray output printed by dialect code cannot
//! corrupt the stream: the frontend only looks at text after the last marker
//! on a line and logs everything else.
//!
//! Positions are pliron positions: 1-based line, 1-based column counted in
//! Unicode scalar values (that is what `combine`'s `SourcePosition` reports).

use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 2;

/// Prefix of every protocol line written by an engine.
pub const LINE_MARKER: &str = "\u{1}PLSP\u{1}";

/// A request from the frontend to an engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum RequestBody {
    /// Handshake; answered with [`EngineInfo`].
    Hello { protocol: u32 },
    /// Parse (and optionally verify) a document.
    Analyze(AnalyzeParams),
    /// Which of the given names are registered in this engine?
    Probe(ProbeParams),
    /// Exit cleanly.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzeParams {
    /// Hash of the (unmasked) document text, echoed back in the result.
    pub text_hash: u64,
    /// The text to parse. The frontend may have masked parts of it (with
    /// same-length whitespace) so positions are preserved.
    pub text: String,
    pub verify: VerifyMode,
    pub want_model: bool,
    /// Rendered attributes longer than this (in chars) are truncated.
    pub max_attr_len: u32,
    /// Print the IR with the dialects' printers, parse that again and
    /// compare (see [`AnalyzeResult::round_trip`]).
    #[serde(default)]
    pub round_trip: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    Off,
    /// `verify_operation` on the top-level op (stops at the first error).
    First,
    /// Verify every operation and block separately and report all errors.
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeParams {
    pub dialects: Vec<String>,
    pub ops: Vec<String>,
    pub types: Vec<String>,
    pub attrs: Vec<String>,
}

/// A response from an engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub enum ResponseBody {
    Hello(EngineInfo),
    Analyze(AnalyzeResult),
    Probe(ProbeParams),
    Error { message: String },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineInfo {
    pub protocol: u32,
    /// Hash of the engine sources this binary was built from.
    pub engine_src_hash: String,
    /// Identifies the dialect bundle (or "reference").
    pub bundle_id: String,
    /// Crates linked into this engine (name, version).
    pub crates: Vec<CrateInfo>,
    /// Number of context registrations (ops + types + attributes + ...).
    pub registrations: u32,
    /// Panic message if `Context::new()` failed in this engine.
    pub context_error: Option<String>,
    /// Hooks registered by dialects with `pliron-lsp-api` (e.g.
    /// `"lint my_dialect::check_widths"`).
    #[serde(default)]
    pub hooks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzeResult {
    pub text_hash: u64,
    /// Parse errors. The engine's pliron recovers from errors inside blocks
    /// and regions, so there may be several.
    pub parse_errors: Vec<EngineDiag>,
    /// Verification errors (at most one with [`VerifyMode::First`]).
    /// Verification is skipped when there are parse errors.
    pub verify_errors: Vec<EngineDiag>,
    /// The IR model, if the top-level operation could be parsed.
    pub model: Option<Model>,
    /// Source spans of everything the (dialect) parsers parsed.
    pub spans: Vec<Span>,
    /// Diagnostics reported by dialect lint hooks.
    #[serde(default)]
    pub hook_diags: Vec<HookDiag>,
    /// Inlay hints from dialect hooks.
    #[serde(default)]
    pub hook_hints: Vec<HookHint>,
    /// With [`AnalyzeParams::round_trip`], for a document without errors:
    /// operations whose printed form does not parse, or parses into
    /// something else (dialect printer/parser bugs).
    #[serde(default)]
    pub round_trip: Vec<EngineDiag>,
    /// With [`AnalyzeParams::round_trip`]: the IR as pliron prints it.
    #[serde(default)]
    pub printed: Option<String>,
    pub elapsed_us: u64,
}

/// What a hook diagnostic or inlay hint is attached to, relative to an op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum HookTarget {
    OpName,
    Op,
    Result { index: u32 },
    Operand { index: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A diagnostic reported by a dialect lint hook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDiag {
    /// Index into [`Model::ops`].
    pub op: u32,
    pub target: HookTarget,
    pub severity: HookSeverity,
    pub message: String,
    /// The hook that reported it.
    pub source: String,
}

/// An inlay hint from a dialect hook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookHint {
    /// Index into [`Model::ops`].
    pub op: u32,
    pub target: HookTarget,
    pub label: String,
}

/// A source span recorded by the instrumented pliron parser. `end` is
/// exclusive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: Pos,
    pub end: Pos,
    pub kind: SpanKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpanKind {
    /// A whole operation; `name_start..name_end` is its `dialect.op` name.
    Op {
        op: u32,
        name_start: Pos,
        name_end: Pos,
    },
    /// An operation that failed to parse (until where recovery resumed).
    OpError,
    /// Definition of an op result.
    ResultDef { value: u32 },
    /// Use of a value as an operand.
    OperandUse { value: u32 },
    /// Use of a block as a successor.
    SuccessorUse { block: u32 },
    /// Block label in a block header.
    BlockLabel { block: u32 },
    /// Block argument name in a block header.
    BlockArgDef { value: u32 },
    /// A type; `name_end` ends its `dialect.type` name. `text` is the type
    /// as printed by pliron.
    Type { name_end: Pos, text: String },
    /// An attribute; `name_end` ends its `dialect.attr` name.
    Attr { name_end: Pos },
    /// A key in an attribute dictionary.
    AttrKey,
    /// A literal keyword of a declarative op/type/attribute format.
    Keyword,
    /// A token marked by a hand-written parser (`pliron_lsp_api::token!`);
    /// `token_type` is a semantic token type name.
    Token { token_type: String },
    /// A region; `start` is its `{`, `end` its `}` (or `start` if unclosed).
    Region { closed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagPhase {
    Parse,
    Verify,
    /// The engine panicked while parsing or verifying.
    Panic,
    /// Reported by a dialect lint hook (frontend only; engines send
    /// [`HookDiag`]s).
    Lint,
    /// Printing and re-parsing does not give the same IR.
    RoundTrip,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineDiag {
    pub phase: DiagPhase,
    pub pos: Option<Pos>,
    pub message: String,
    /// Index into [`Model::ops`] of the op the error is attributed to.
    pub op: Option<u32>,
}

/// A pliron source position: 1-based line, 1-based char column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

/// An arena-encoded snapshot of the parsed IR.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub root: u32,
    pub ops: Vec<OpInfo>,
    pub regions: Vec<RegionInfo>,
    pub blocks: Vec<BlockInfo>,
    pub values: Vec<ValueInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpInfo {
    pub opid: String,
    pub pos: Option<Pos>,
    pub parent_block: Option<u32>,
    pub results: Vec<u32>,
    pub operands: Vec<u32>,
    pub successors: Vec<u32>,
    pub regions: Vec<u32>,
    pub attrs: Vec<AttrInfo>,
    /// Symbol name, for ops implementing `SymbolOpInterface`.
    pub symbol: Option<String>,
    /// Bit set of [`op_traits`].
    pub traits: u32,
    /// Extra hover text from dialect hooks (markdown).
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Bits of [`OpInfo::traits`].
pub mod op_traits {
    pub const ISOLATED_FROM_ABOVE: u32 = 1 << 0;
    pub const SYMBOL: u32 = 1 << 1;
    pub const SYMBOL_TABLE: u32 = 1 << 2;
    pub const TERMINATOR: u32 = 1 << 3;
    pub const ONE_RESULT: u32 = 1 << 4;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttrInfo {
    pub key: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RegionInfo {
    pub parent_op: u32,
    pub blocks: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlockInfo {
    pub label: Option<String>,
    pub pos: Option<Pos>,
    pub region: u32,
    pub args: Vec<u32>,
    pub ops: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueInfo {
    pub def: ValueDef,
    /// The type, as printed by pliron.
    pub ty: String,
    /// The name pliron recorded for this value, if any.
    pub given_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueDef {
    Result {
        op: u32,
        index: u32,
    },
    Arg {
        block: u32,
        index: u32,
    },
    /// A value that is not attached to the parsed IR (e.g. inside an
    /// operation that failed to parse, or an unresolved name).
    Detached {
        unresolved: bool,
    },
}

/// Encode a message as one protocol line (marker + JSON + newline).
pub fn encode_line<T: Serialize>(msg: &T) -> String {
    let mut s = String::from(LINE_MARKER);
    s.push_str(&serde_json::to_string(msg).expect("protocol messages always serialize"));
    s.push('\n');
    s
}

/// Extract the JSON payload of a protocol line, if it is one.
pub fn decode_payload(line: &str) -> Option<&str> {
    line.rfind(LINE_MARKER)
        .map(|i| line[i + LINE_MARKER.len()..].trim_end_matches(['\r', '\n']))
}

/// FNV-1a hash of a text, used to tag analysis requests/results.
pub fn text_hash(text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request {
            id: 7,
            body: RequestBody::Analyze(AnalyzeParams {
                text_hash: 42,
                text: "builtin.module @m {}".into(),
                verify: VerifyMode::First,
                want_model: true,
                max_attr_len: 200,
                round_trip: false,
            }),
        };
        let line = encode_line(&req);
        let payload = decode_payload(&line).unwrap();
        let back: Request = serde_json::from_str(payload).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn noise_before_marker_is_ignored() {
        let resp = Response {
            id: 1,
            body: ResponseBody::Shutdown,
        };
        let line = format!("debug print from a dialect{}", encode_line(&resp));
        let back: Response = serde_json::from_str(decode_payload(&line).unwrap()).unwrap();
        assert_eq!(back, resp);
        assert!(decode_payload("plain stdout noise").is_none());
    }
}

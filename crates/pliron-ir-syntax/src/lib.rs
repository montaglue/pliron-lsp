//! Dialect-agnostic, error-tolerant syntax layer for pliron textual IR.
//!
//! This crate knows nothing about specific dialects: it recognises the
//! structure every pliron document shares (statements, blocks, regions,
//! names) and resolves names the way pliron's `NameTracker` does. It works
//! on any IR instantly and is the baseline that richer layers refine.

pub mod lexer;
pub mod line_index;
pub mod sema;
pub mod tree;

pub use lexer::{Offset, Token, TokenKind};
pub use line_index::{Encoding, LineIndex, LinePos};
pub use sema::{
    Analysis, Confidence, Def, DefId, DefKind, Diagnostic, Knowledge, OpFacts, Provenance, Role,
    Severity, TypeSpan, analyze,
};
pub use tree::{BlockId, ErrorKind, RegionId, StmtId, TokIdx, Tree};

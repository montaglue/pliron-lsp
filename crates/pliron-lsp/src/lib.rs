//! pliron-lsp: a language server for pliron textual IR.
//!
//! Exact analysis comes from *dialect engines*: native binaries that link
//! (instrumented) pliron plus the dialects of a project and parse documents
//! with the real dialect parsers. A dialect-agnostic syntax layer
//! (`pliron-ir-syntax`) answers instantly and whenever no engine result for
//! the current text is available.

pub mod bundle;
pub mod document;
pub mod engine;
pub mod exact;
pub mod features;
pub mod index;
pub mod projects;
pub mod server;

/// Run the server on stdin/stdout.
pub fn run_stdio() -> anyhow::Result<()> {
    let (connection, io_threads) = lsp_server::Connection::stdio();
    server::run(connection)?;
    io_threads.join()?;
    Ok(())
}

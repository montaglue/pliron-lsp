//! pliron-lsp: a language server for pliron textual IR.
//!
//! Exact analysis comes from *dialect engines*: native binaries that link
//! (instrumented) pliron plus the dialects of a project and parse documents
//! with the real dialect parsers. A dialect-agnostic syntax layer
//! (`pliron-ir-syntax`) answers instantly and whenever no engine result for
//! the current text is available.

pub mod bundle;
pub mod cli;
pub mod document;
pub mod engine;
pub mod exact;
pub mod features;
pub mod format;
pub mod index;
pub mod patcher;
pub mod projects;
pub mod server;
pub mod workspace;

/// `std::fs::canonicalize`, but without the `\\?\` prefix Windows adds to
/// ordinary drive paths: cargo, rustc, linkers and current directories do
/// not all accept it.
pub fn canonicalize(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let c = std::fs::canonicalize(path)?;
    if cfg!(windows)
        && let Some(rest) = c.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return Ok(std::path::PathBuf::from(rest));
    }
    Ok(c)
}

/// Run the server on stdin/stdout.
pub fn run_stdio() -> anyhow::Result<()> {
    let (connection, io_threads) = lsp_server::Connection::stdio();
    server::run(connection)?;
    io_threads.join()?;
    Ok(())
}

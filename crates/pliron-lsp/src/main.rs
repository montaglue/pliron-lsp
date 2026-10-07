fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("pliron-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    pliron_lsp::run_stdio()
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("pliron-lsp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("check") => std::process::exit(pliron_lsp::cli::check(&args[1..])?),
        Some("fmt") => std::process::exit(pliron_lsp::cli::fmt(&args[1..])?),
        Some("cache") => std::process::exit(pliron_lsp::cli::cache(&args[1..])?),
        Some("-h" | "--help" | "help") => {
            println!(
                "pliron-lsp {}\n\nusage:\n  pliron-lsp                 run the language server on stdio\n  pliron-lsp check [paths]   lint pliron IR files (see `check --help`)\n  pliron-lsp fmt [paths]     format pliron IR files (see `fmt --help`)\n  pliron-lsp cache [--clean] show (or delete) the engine build cache shared by projects",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        _ => pliron_lsp::run_stdio(),
    }
}

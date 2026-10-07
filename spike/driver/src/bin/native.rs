fn main() {
    let t = std::time::Instant::now();
    pliron_lsp_spike_driver::__pliron_lsp_analyze();
    eprintln!("native analyze: {:?}", t.elapsed());
}

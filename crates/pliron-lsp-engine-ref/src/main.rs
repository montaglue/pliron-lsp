//! The reference engine: pliron's builtin dialect plus `pliron-llvm`.
//! Used for documents outside any cargo workspace that depends on pliron.

use pliron_llvm as _;

fn main() {
    pliron_lsp_engine::run_stdio(pliron_lsp_engine::BundleInfo {
        bundle_id: "reference",
        crates: &[("pliron", "0.18.0"), ("pliron-llvm", "0.18.0")],
    });
}

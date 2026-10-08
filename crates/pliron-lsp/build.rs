//! Embed the sources needed to generate dialect bundles: the engine, the
//! protocol, and pliron-lsp's instrumented pliron / pliron-derive.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn collect(root: &Path, rel: &Path, out: &mut Vec<(String, PathBuf)>, prefix: &str) {
    let dir = root.join(rel);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let r = rel.join(e.file_name());
        if p.is_dir() {
            collect(root, &r, out, prefix);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push((format!("{prefix}/{}", r.display()), p));
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let crates = manifest.parent().unwrap();
    let repo = crates.parent().unwrap();
    let sources: [(&str, PathBuf, &[&str]); 2] = [
        ("pliron-lsp-protocol", crates.join("pliron-lsp-protocol"), &[]),
        ("pliron-lsp-engine", crates.join("pliron-lsp-engine"), &[]),
    ];
    let mut files = Vec::new();
    for (name, dir, extra) in &sources {
        println!("cargo:rerun-if-changed={}", dir.display());
        collect(dir, Path::new("src"), &mut files, name);
        for f in *extra {
            let p = dir.join(f);
            if p.is_file() {
                files.push((format!("{name}/{f}"), p));
            }
        }
    }
    // Instrumentation patches, one per supported pliron version line.
    let patches_dir = repo.join("third_party/patches");
    println!("cargo:rerun-if-changed={}", patches_dir.display());
    let mut patches = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&patches_dir) {
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if let Some(v) = name.strip_prefix("pliron-").and_then(|n| n.strip_suffix(".patch")) {
                patches.push((v.to_string(), p.clone()));
                files.push((format!("patches/{name}"), p));
            }
        }
    }
    patches.sort();
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut code = String::from("pub static FILES: &[(&str, &str)] = &[\n");
    for (rel, path) in &files {
        let content = std::fs::read(path).unwrap();
        for b in rel.as_bytes().iter().chain(content.iter()) {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        writeln!(code, "    ({rel:?}, include_str!({:?})),", path.display().to_string()).unwrap();
    }
    code.push_str("];\n");
    code.push_str("/// (pliron version line, patch)\npub static PATCHES: &[(&str, &str)] = &[\n");
    for (v, p) in &patches {
        writeln!(code, "    ({v:?}, include_str!({:?})),", p.display().to_string()).unwrap();
    }
    code.push_str("];\n");
    writeln!(code, "pub const SRC_HASH: &str = \"{hash:016x}\";").unwrap();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded.rs");
    std::fs::write(out, code).unwrap();
}

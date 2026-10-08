//! Development tasks (`cargo xtask <task>`), in the spirit of
//! rust-analyzer's xtask.
//!
//! * `cargo xtask dist [--no-build] [--target <vscode-target>]`
//!   builds release binaries of the server and the reference engine, puts
//!   them into `editors/vscode/server/` and packages a platform-specific
//!   VS Code extension into `dist/pliron-<target>.vsix`.
//! * `cargo xtask install [--server] [--client]`
//!   installs the server binaries with `cargo install` and/or the packaged
//!   extension with `code --install-extension`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    eprintln!("$ {cmd:?}");
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}

/// The VS Code platform target of the host.
fn host_target() -> anyhow::Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win32-x64",
        ("windows", "aarch64") => "win32-arm64",
        (os, arch) => bail!("unsupported host {os}/{arch}"),
    })
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

const BINARIES: &[(&str, &str)] = &[
    ("pliron-lsp", "pliron-lsp"),
    ("pliron-lsp-engine-ref", "pliron-lsp-engine-ref"),
];

fn npm() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

fn npx() -> &'static str {
    if cfg!(windows) { "npx.cmd" } else { "npx" }
}

fn dist(build: bool, target: Option<String>) -> anyhow::Result<PathBuf> {
    let root = root();
    let target = match target {
        Some(t) => t,
        None => host_target()?.to_string(),
    };
    if build {
        let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        cmd.current_dir(&root).args(["build", "--release"]);
        for (package, _) in BINARIES {
            cmd.args(["-p", package]);
        }
        run(&mut cmd)?;
    }

    let ext = root.join("editors/vscode");
    let server = ext.join("server");
    std::fs::create_dir_all(&server)?;
    for (_, bin) in BINARIES {
        let from = root.join("target/release").join(exe(bin));
        let to = server.join(exe(bin));
        // Copy then rename, so a running server keeps its (old) binary.
        let tmp = to.with_extension("tmp");
        std::fs::copy(&from, &tmp)
            .with_context(|| format!("copying {} (build with --release first)", from.display()))?;
        std::fs::rename(&tmp, &to)?;
        eprintln!("bundled {}", to.display());
    }

    if !ext.join("node_modules").is_dir() {
        run(Command::new(npm()).current_dir(&ext).args(["ci", "--no-audit", "--no-fund"]))?;
    }
    run(Command::new(npm()).current_dir(&ext).args(["run", "compile"]))?;

    let out_dir = root.join("dist");
    std::fs::create_dir_all(&out_dir)?;
    let vsix = out_dir.join(format!("pliron-{target}.vsix"));
    run(Command::new(npx())
        .current_dir(&ext)
        .args(["vsce", "package", "--target", &target, "-o"])
        .arg(&vsix))?;
    eprintln!("\npackaged {}", vsix.display());
    Ok(vsix)
}

fn install(server: bool, client: bool) -> anyhow::Result<()> {
    let root = root();
    if server {
        for (package, _) in BINARIES {
            run(Command::new("cargo")
                .current_dir(&root)
                .args(["install", "--locked", "--path"])
                .arg(root.join("crates").join(package)))?;
        }
    }
    if client {
        let vsix = dist(true, None)?;
        run(Command::new("code")
            .arg("--install-extension")
            .arg(&vsix)
            .arg("--force"))?;
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let task = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    let flag = |f: &str| rest.iter().any(|a| a == f);
    match task.as_str() {
        "dist" => {
            let target = rest
                .iter()
                .position(|a| a == "--target")
                .and_then(|i| rest.get(i + 1).cloned());
            dist(!flag("--no-build"), target).map(|_| ())
        }
        "install" => {
            let (server, client) = match (flag("--server"), flag("--client")) {
                (false, false) => (true, true),
                other => other,
            };
            install(server, client)
        }
        _ => {
            eprintln!(
                "usage:\n  cargo xtask dist [--no-build] [--target <vscode-target>]\n  cargo xtask install [--server] [--client]"
            );
            Ok(())
        }
    }
}

//! Development tasks (`cargo xtask <task>`), in the spirit of
//! rust-analyzer's xtask.
//!
//! * `cargo xtask dist [--no-build] [--target <vscode-target>]`
//!   builds release binaries of the server and the reference engine, puts
//!   them into `editors/vscode/server/` and packages a platform-specific
//!   VS Code extension into `dist/pliron-<target>.vsix`. With `--target`,
//!   the binaries are built for that platform (cross-compiling if needed).
//! * `cargo xtask install [--server] [--client]`
//!   installs the server binaries with `cargo install` and/or the packaged
//!   extension with `code --install-extension`.
//! * `cargo xtask test-vscode [--bundled]`
//!   runs the VS Code extension's integration tests in a Linux container
//!   (Docker) on a virtual display, so no VS Code window opens on the
//!   host. `--bundled` tests the packaged extension with its bundled server.
//! * `cargo xtask check-pliron <0.17 | latest | head | git:URL[#BRANCH]>`
//!   checks pliron-lsp against a pliron release line, the newest release,
//!   the head of pliron's repository or another git branch: a tiny
//!   dialect crate is created, and `pliron-lsp check` must build its engine
//!   (instrumenting that pliron) and report exact diagnostics.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

/// Cargo's target directory (`CARGO_TARGET_DIR` or `<root>/target`).
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target"))
}

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

/// The Rust target triple of a VS Code platform target.
fn rust_triple(vscode_target: &str) -> anyhow::Result<&'static str> {
    Ok(match vscode_target {
        "darwin-arm64" => "aarch64-apple-darwin",
        "darwin-x64" => "x86_64-apple-darwin",
        "linux-x64" => "x86_64-unknown-linux-gnu",
        "linux-arm64" => "aarch64-unknown-linux-gnu",
        "win32-x64" => "x86_64-pc-windows-msvc",
        "win32-arm64" => "aarch64-pc-windows-msvc",
        other => bail!("unknown VS Code target {other}"),
    })
}

fn exe_for(name: &str, vscode_target: &str) -> String {
    if vscode_target.starts_with("win32") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
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
    // An explicit target is built with `--target` (into target/<triple>/).
    let (target, out, triple) = match target {
        Some(t) => {
            let triple = rust_triple(&t)?;
            let out = target_dir().join(triple).join("release");
            (t, out, Some(triple))
        }
        None => (
            host_target()?.to_string(),
            target_dir().join("release"),
            None,
        ),
    };
    if build {
        let mut cmd = cargo();
        cmd.current_dir(&root).args(["build", "--release"]);
        if let Some(triple) = triple {
            cmd.args(["--target", triple]);
        }
        for (package, _) in BINARIES {
            cmd.args(["-p", package]);
        }
        run(&mut cmd)?;
    }

    let ext = root.join("editors/vscode");
    let server = ext.join("server");
    std::fs::create_dir_all(&server)?;
    for (_, bin) in BINARIES {
        let from = out.join(exe_for(bin, &target));
        let to = server.join(exe_for(bin, &target));
        // Copy then rename, so a running server keeps its (old) binary.
        let tmp = to.with_extension("tmp");
        std::fs::copy(&from, &tmp)
            .with_context(|| format!("copying {} (build with --release first)", from.display()))?;
        std::fs::rename(&tmp, &to)?;
        eprintln!("bundled {}", to.display());
    }

    if !ext.join("node_modules").is_dir() {
        run(Command::new(npm())
            .current_dir(&ext)
            .args(["ci", "--no-audit", "--no-fund"]))?;
    }
    run(Command::new(npm())
        .current_dir(&ext)
        .args(["run", "compile"]))?;

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

/// The tiny dialect of `check-pliron`: one op whose format has a keyword.
const CHECK_DIALECT: &str = r#"//! A tiny dialect for checking pliron-lsp against a pliron version.

use pliron::builtin::op_interfaces::{NOpdsInterface, NResultsInterface};
use pliron::derive::pliron_op;

/// Does nothing, now.
#[pliron_op(
    name = "ci.nop",
    format = "`now`",
    interfaces = [NOpdsInterface<0>, NResultsInterface<0>],
    verifier = "succ"
)]
pub struct NopOp;
"#;

const CHECK_GOOD: &str = "builtin.module @m {\n  ^entry():\n  ci.nop now;\n  ci.nop now\n}\n";

/// An unknown op (line 4) and a wrong keyword (line 5): the instrumented
/// parser must report both, at their positions.
const CHECK_BAD: &str =
    "builtin.module @m {\n  ^entry():\n  ci.nop now;\n  ci.nopp now;\n  ci.nop later\n}\n";

/// `cargo xtask check-pliron <source>`, see the module docs.
fn check_pliron(source: &str) -> anyhow::Result<()> {
    let root = root();
    let (spec, slug) = match source {
        // The default branch of pliron's repository.
        "head" => (
            r#"{ git = "https://github.com/pliron-org/pliron" }"#.to_string(),
            "head".to_string(),
        ),
        s if s.starts_with("git:") => {
            let rest = &s[4..];
            match rest.split_once('#') {
                Some((url, branch)) => (
                    format!("{{ git = {url:?}, branch = {branch:?} }}"),
                    format!("git-{branch}"),
                ),
                None => (format!("{{ git = {rest:?} }}"), "git".to_string()),
            }
        }
        // The newest release on crates.io.
        "latest" => (r#""*""#.to_string(), "latest".to_string()),
        version => (format!("{version:?}"), version.replace('.', "_")),
    };
    run(cargo()
        .current_dir(&root)
        .args(["build", "-p", "pliron-lsp"]))?;
    let server = target_dir().join("debug").join(exe("pliron-lsp"));

    // Kept under target/ so that repeated runs build incrementally.
    let dir = root.join("target/check-pliron").join(&slug);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"ci-dialect\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\npliron = {spec}\n\n[workspace]\n"
        ),
    )?;
    std::fs::write(dir.join("src/lib.rs"), CHECK_DIALECT)?;
    std::fs::write(dir.join("good.pliron"), CHECK_GOOD)?;
    std::fs::write(dir.join("bad.pliron"), CHECK_BAD)?;
    // Always resolve the newest matching pliron.
    let _ = std::fs::remove_file(dir.join("Cargo.lock"));
    run(cargo().current_dir(&dir).arg("generate-lockfile"))?;
    // pliron 0.16 and 0.17 accept any pliron-derive 0.x but only build with
    // their own version: pin it, as a project's lock file does.
    let lock = std::fs::read_to_string(dir.join("Cargo.lock"))?;
    let version_of = |name: &str| {
        lock.split("[[package]]")
            .find(|p| p.contains(&format!("name = \"{name}\"\n")))
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
            .map(|v| v.trim_matches('"').to_string())
    };
    if let (Some(pliron), Some(derive)) = (version_of("pliron"), version_of("pliron-derive"))
        && pliron != derive
        && !source.starts_with("git:")
        && source != "head"
    {
        run(cargo()
            .current_dir(&dir)
            .args(["update", "pliron-derive", "--precise", &pliron]))?;
    }

    let check = |file: &str| -> anyhow::Result<(bool, String)> {
        eprintln!("$ pliron-lsp check {file}");
        let out = Command::new(&server)
            .current_dir(&dir)
            .args(["check", file])
            // Like an editor starts it: without the toolchain that running
            // under `cargo` pins.
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("CARGO")
            .env_remove("RUSTC")
            .output()
            .context("running pliron-lsp")?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        print!("{stdout}");
        Ok((out.status.success(), stdout))
    };
    if !check("good.pliron")?.0 {
        bail!("good.pliron must check cleanly against pliron {source}");
    }
    let (ok, out) = check("bad.pliron")?;
    let expected = [
        "bad.pliron:4:3: error[parse]: Unregistered Op ci.nopp",
        "bad.pliron:5:",
    ];
    if ok || !expected.iter().all(|e| out.contains(e)) {
        bail!(
            "bad.pliron must report {expected:?} (from the dialect engine) against pliron {source}"
        );
    }
    eprintln!("\npliron {source}: OK");
    Ok(())
}

/// `cargo xtask test-vscode [--bundled]`, see the module docs.
fn test_vscode(bundled: bool) -> anyhow::Result<()> {
    let root = root();
    let image = "pliron-lsp-vscode-tests";
    run(Command::new("docker")
        .args(["build", "--tag", image])
        .arg(root.join("editors/vscode/docker")))?;
    // The repository is mounted read-only and copied into a volume (with
    // the timestamps, so builds stay incremental); the build directory,
    // node_modules, the downloaded VS Code and the bundled server only
    // exist in volumes, never in the host's working tree.
    let tests = if bundled {
        "cargo xtask dist\nPLIRON_TEST_BUNDLED=1 xvfb-run -a npm test"
    } else {
        "cargo build -p pliron-lsp -p pliron-lsp-engine-ref\nxvfb-run -a npm test"
    };
    let script = format!(
        "set -e\n\
         rsync -a --delete --exclude target/ --exclude .git/ --exclude node_modules/ \
           --exclude /dist --exclude /editors/vscode/.vscode-test \
           --exclude /editors/vscode/out --exclude /editors/vscode/server /src/ /work/\n\
         (cd editors/vscode && npm ci --no-audit --no-fund)\n\
         cd editors/vscode\n\
         {tests}\n"
    );
    run(Command::new("docker")
        .args(["run", "--rm", "--init"])
        .arg("--volume")
        .arg(format!("{}:/src:ro", root.display()))
        .args([
            "--volume",
            "pliron-lsp-vscode-work:/work",
            "--volume",
            "pliron-lsp-vscode-target:/cargo-target",
            "--volume",
            "pliron-lsp-cargo-registry:/usr/local/cargo/registry",
            "--volume",
            "pliron-lsp-cargo-git:/usr/local/cargo/git",
            image,
            "bash",
            "-c",
            &script,
        ]))
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
        "test-vscode" => test_vscode(flag("--bundled")),
        "check-pliron" => match rest.first() {
            Some(source) => check_pliron(source),
            None => {
                bail!("usage: cargo xtask check-pliron <0.17 | latest | head | git:URL[#BRANCH]>")
            }
        },
        "install" => {
            let (server, client) = match (flag("--server"), flag("--client")) {
                (false, false) => (true, true),
                other => other,
            };
            install(server, client)
        }
        _ => {
            eprintln!(
                "usage:\n  cargo xtask dist [--no-build] [--target <vscode-target>]\n  cargo xtask install [--server] [--client]\n  cargo xtask test-vscode [--bundled]\n  cargo xtask check-pliron <0.17 | latest | head | git:URL[#BRANCH]>"
            );
            Ok(())
        }
    }
}

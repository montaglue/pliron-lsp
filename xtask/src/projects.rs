//! `cargo xtask check-projects`: checks pliron-lsp against real projects
//! that use pliron, before each release.
//!
//! Each project is fetched at a pinned commit into
//! `target/check-projects/<name>`, and `pliron-lsp check` runs on its IR
//! files: those the project has, plus the samples in
//! `xtask/projects/<name>/` (copied in as `pliron-lsp-samples/`). The
//! project's own dialect engine must be built, with the dialect crates and
//! the diagnostics recorded in `xtask/projects/<name>.expected`.
//!
//! On a machine with nix, each check runs in `nix-shell -p zlib libxml2`
//! (crates that link LLVM statically need them), or in the project's own
//! `nix develop` shell when it has one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::{cargo, exe, root, run, target_dir};

struct Project {
    name: &'static str,
    repo: &'static str,
    rev: &'static str,
    /// The cargo workspace inside the repository.
    dir: &'static str,
    /// The project's own IR files (directories or files in `dir`).
    ir: &'static [&'static str],
    /// `cargo update` first: the committed `Cargo.lock` does not build.
    update: bool,
    /// Build inside the `nix develop` shell of `dir` (when nix is there).
    flake: bool,
}

const PROJECTS: &[Project] = &[
    Project {
        name: "pliron-common-dialects",
        repo: "https://github.com/pliron-org/pliron-common-dialects",
        rev: "48700305b597d97c77ff1f875042d72620cce079",
        dir: "",
        ir: &[],
        update: false,
        flake: false,
    },
    Project {
        name: "pliron-tensor",
        repo: "https://github.com/pliron-org/pliron-tensor",
        rev: "dcf0f87ace28d2453ed9ad69fb5612d6d3325ece",
        dir: "",
        ir: &["tests/resources"],
        // Its lock file pins pliron-common-dialects and pliron revisions
        // that do not build together.
        update: true,
        flake: false,
    },
    Project {
        name: "plironnx",
        repo: "https://github.com/pliron-org/plironnx",
        rev: "9a25a5b47ce907d93cd8895613828ca44ab490cb",
        dir: "",
        ir: &["plironnx-tests/tests/resources"],
        update: false,
        flake: false,
    },
    Project {
        name: "plirun",
        repo: "https://github.com/akkaze/plirun",
        rev: "8ee4e1c6c68d41ccecae92acb498d36ddc4b55ba",
        dir: "",
        // Its dialects are optional features, off by default: it gets the
        // reference engine, which does not know them.
        ir: &["tests/resources"],
        update: false,
        flake: false,
    },
    Project {
        name: "cubecl",
        repo: "https://github.com/tracel-ai/cubecl",
        rev: "17c297434d6d573a1c046b5d5bc3160744f89f36",
        dir: "",
        ir: &[],
        update: false,
        flake: false,
    },
    Project {
        name: "cuda-oxide",
        repo: "https://github.com/NVIDIA/cuda-rust",
        rev: "96b1d4b20cb39e76435c56e5fe31051fc614db69",
        dir: "cuda-oxide",
        ir: &[],
        update: false,
        flake: true,
    },
];

const SAMPLES: &str = "pliron-lsp-samples";

/// What a check must produce: the engine's dialect crates (none for the
/// reference engine) and the diagnostics (one per line, continuation lines
/// indented).
#[derive(PartialEq)]
struct Outcome {
    crates: BTreeSet<String>,
    diagnostics: BTreeSet<String>,
}

impl Outcome {
    fn parse(text: &str) -> Outcome {
        let mut crates = BTreeSet::new();
        let mut lines = Vec::new();
        for line in text.lines() {
            if let Some(c) = line.strip_prefix("# engine:") {
                crates.extend(
                    c.split(',')
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty()),
                );
            } else if !line.starts_with('#') && !line.is_empty() {
                lines.push(line);
            }
        }
        Outcome {
            crates,
            diagnostics: records(&lines),
        }
    }

    fn render(&self) -> String {
        let crates: Vec<_> = self.crates.iter().map(String::as_str).collect();
        let mut s = format!(
            "# Written by `cargo xtask check-projects --bless`.\n# engine: {}\n",
            crates.join(", ")
        );
        for d in &self.diagnostics {
            s.push_str(d);
            s.push('\n');
        }
        s
    }
}

/// Whether `line` starts a diagnostic: `file:line:column: ...`.
fn is_diagnostic(line: &str) -> bool {
    let mut parts = line.splitn(4, ':');
    let (Some(file), Some(l), Some(c), Some(_)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    !file.is_empty() && !file.starts_with(' ') && number(l) && number(c)
}

/// Diagnostics with their continuation lines, as one string each. Other
/// output (a nix shell's greeting, say) is ignored.
fn records(lines: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cur: Option<String> = None;
    for line in lines {
        if line.starts_with("    ")
            && let Some(c) = &mut cur
        {
            c.push('\n');
            c.push_str(line);
            continue;
        }
        out.extend(cur.take());
        if is_diagnostic(line) {
            cur = Some(line.to_string());
        }
    }
    out.extend(cur);
    out
}

/// The crates of "building dialect engine (N dialect crate(s): a, b...)".
fn engine_crates(stderr: &str) -> Option<BTreeSet<String>> {
    let line = stderr
        .lines()
        .find_map(|l| l.trim().strip_prefix("building dialect engine ("))?;
    let list = line.split_once("crate(s): ")?.1;
    let list = list.split([';', '(']).next()?.trim_end_matches(')');
    Some(list.split(", ").map(|c| c.trim().to_string()).collect())
}

/// Fetch `rev` (or the default branch's head) of `p` into `dest`.
fn fetch(p: &Project, dest: &Path, latest: bool) -> anyhow::Result<String> {
    if !dest.join(".git").exists() {
        std::fs::create_dir_all(dest)?;
        run(Command::new("git").current_dir(dest).args(["init", "-q"]))?;
    }
    let what = if latest { "HEAD" } else { p.rev };
    run(Command::new("git")
        .current_dir(dest)
        .args(["fetch", "-q", "--depth", "1", p.repo, what]))?;
    // A clean checkout: no lock files or samples from earlier runs.
    run(Command::new("git")
        .current_dir(dest)
        .args(["checkout", "-q", "--force", "FETCH_HEAD"]))?;
    run(Command::new("git").current_dir(dest).args([
        "clean",
        "-q",
        "-fdx",
        "--exclude",
        "target/",
    ]))?;
    let out = Command::new("git")
        .current_dir(dest)
        .args(["rev-parse", "HEAD"])
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn has_nix() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `program args...`, inside the project's nix shell when there is nix.
fn in_shell(p: &Project, ws: &Path, program: &Path, args: &[String]) -> Command {
    if !has_nix() {
        let mut c = Command::new(program);
        c.args(args);
        return c;
    }
    if p.flake {
        let mut c = Command::new("nix");
        c.arg("develop")
            .arg(ws)
            .arg("--command")
            .arg(program)
            .args(args);
        return c;
    }
    let line: Vec<String> = std::iter::once(program.display().to_string())
        .chain(args.iter().cloned())
        .map(|a| sh_quote(&a))
        .collect();
    let mut c = Command::new("nix-shell");
    c.args(["-p", "zlib", "libxml2", "--run", &line.join(" ")]);
    c
}

fn check(
    p: &Project,
    server: &Path,
    cache: &Path,
    latest: bool,
    bless: bool,
) -> anyhow::Result<()> {
    let root = root();
    let clone = target_dir().join("check-projects").join(p.name);
    let rev = fetch(p, &clone, latest)?;
    let ws = clone.join(p.dir);
    let mut paths: Vec<String> = p.ir.iter().map(|d| d.to_string()).collect();
    let samples = root.join("xtask/projects").join(p.name);
    if samples.is_dir() {
        let dest = ws.join(SAMPLES);
        std::fs::create_dir_all(&dest)?;
        for e in std::fs::read_dir(&samples)? {
            let e = e?;
            std::fs::copy(e.path(), dest.join(e.file_name()))?;
        }
        paths.push(SAMPLES.into());
    }
    if paths.is_empty() {
        bail!("{}: no IR files to check", p.name);
    }
    if p.update {
        run(cargo().current_dir(&ws).arg("update"))?;
    }

    let mut args = vec!["check".to_string()];
    args.extend(paths);
    eprintln!("$ pliron-lsp {} (in {})", args.join(" "), ws.display());
    let out = in_shell(p, &ws, server, &args)
        .current_dir(&ws)
        .env("PLIRON_LSP_CACHE_DIR", cache)
        // Like an editor starts it: without the toolchain that running
        // under `cargo` pins.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .output()
        .context("running pliron-lsp")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let report: String = stderr
        .lines()
        .filter(|l| !l.trim_start().starts_with("Compiling") && !l.contains("Blocking waiting"))
        .map(|l| format!("{l}\n"))
        .collect();
    eprint!("{report}");

    let prefix = format!("{}/", ws.display());
    let lines: Vec<String> = stdout.lines().map(|l| l.replace(&prefix, "")).collect();
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    // None: the project has no dialects beyond the reference engine's.
    let crates = engine_crates(&stderr).unwrap_or_default();
    if stderr.contains("checking syntax only") {
        bail!("{}: building the dialect engine failed", p.name);
    }
    if stderr.contains("retrying with minimal features") {
        eprintln!(
            "note: {}: the engine only built with minimal features",
            p.name
        );
    }
    let got = Outcome {
        crates,
        diagnostics: records(&lines),
    };

    let expected_file = root
        .join("xtask/projects")
        .join(format!("{}.expected", p.name));
    if bless {
        std::fs::write(&expected_file, got.render())?;
        eprintln!("{}: wrote {} (at {rev})", p.name, expected_file.display());
        return Ok(());
    }
    let expected = Outcome::parse(&std::fs::read_to_string(&expected_file).unwrap_or_default());
    if got == expected {
        eprintln!(
            "{}: OK ({} dialect crates, {} known diagnostics, at {rev})",
            p.name,
            got.crates.len(),
            got.diagnostics.len()
        );
        return Ok(());
    }
    let diff = |what: &str, a: &BTreeSet<String>, b: &BTreeSet<String>| {
        for x in a.difference(b) {
            eprintln!("  {what}: {x}");
        }
    };
    eprintln!("{}: differs from {}:", p.name, expected_file.display());
    diff("unexpected engine crate", &got.crates, &expected.crates);
    diff("missing engine crate", &expected.crates, &got.crates);
    diff("new diagnostic", &got.diagnostics, &expected.diagnostics);
    diff("gone diagnostic", &expected.diagnostics, &got.diagnostics);
    bail!("{}: unexpected result (at {rev})", p.name)
}

/// `cargo xtask check-projects [--latest] [--bless] [names...]`.
pub fn check_projects(args: &[String]) -> anyhow::Result<()> {
    let latest = args.iter().any(|a| a == "--latest");
    let bless = args.iter().any(|a| a == "--bless");
    let names: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    for n in &names {
        if !PROJECTS.iter().any(|p| p.name == n.as_str()) {
            let all: Vec<_> = PROJECTS.iter().map(|p| p.name).collect();
            bail!("unknown project {n} (known: {})", all.join(", "));
        }
    }
    run(cargo()
        .current_dir(root())
        .args(["build", "-p", "pliron-lsp"]))?;
    let server = target_dir().join("debug").join(exe("pliron-lsp"));
    // Kept between runs, so that only what changed is rebuilt.
    let cache = std::env::var_os("PLIRON_LSP_CACHE_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| target_dir().join("check-projects/cache"));

    let mut failed = Vec::new();
    for p in PROJECTS
        .iter()
        .filter(|p| names.is_empty() || names.iter().any(|n| n.as_str() == p.name))
    {
        eprintln!("\n=== {}", p.name);
        if let Err(e) = check(p, &server, &cache, latest, bless) {
            eprintln!("error: {e:#}");
            failed.push(p.name);
        }
    }
    if !failed.is_empty() {
        bail!("failed: {}", failed.join(", "));
    }
    eprintln!("\nall projects OK");
    Ok(())
}

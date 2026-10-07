//! Dialect bundles: automatically generated engine binaries that link a
//! project's dialect crates — no user code required.
//!
//! For a cargo project, pliron-lsp:
//! 1. runs `cargo metadata` and finds the crates that define pliron
//!    entities (they depend on pliron / pliron-derive and use the
//!    registration macros);
//! 2. writes a small package under `<target>/pliron-lsp/bundle/` that
//!    depends on exactly those crates (same sources, versions and resolved
//!    features as the project), vendors the engine and patches pliron with
//!    pliron-lsp's instrumented copy;
//! 3. builds it with the project's own toolchain into a separate target
//!    directory, and runs the result as the project's engine.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};
use cargo_metadata::{DependencyKind, Metadata, MetadataCommand, Package, PackageId, TargetKind};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

/// Hash of the embedded engine + pliron sources.
pub fn engine_src_hash() -> &'static str {
    embedded::SRC_HASH
}

/// The pliron version line the instrumented copy supports.
const SUPPORTED_PLIRON: (u64, u64) = (0, 18);

/// Strings whose presence marks a crate as defining pliron entities.
const REGISTRATION_MARKERS: &[&str] = &[
    "pliron_op",
    "pliron_type",
    "pliron_attr",
    "def_op",
    "def_type",
    "def_attribute",
    "context_registration!",
];

/// The nearest ancestor directory of `file` that contains a `Cargo.toml`.
pub fn find_project_dir(file: &Path) -> Option<PathBuf> {
    let mut dir = if file.is_dir() {
        Some(file)
    } else {
        file.parent()
    };
    while let Some(d) = dir {
        if d.join("Cargo.toml").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

pub fn load_metadata(dir: &Path) -> anyhow::Result<Metadata> {
    let mut cmd = MetadataCommand::new();
    cmd.current_dir(dir);
    match cmd.clone().other_options(vec!["--offline".into()]).exec() {
        Ok(m) => Ok(m),
        Err(_) => cmd.exec().context("cargo metadata failed"),
    }
}

/// How a dependency is specified in the generated manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSpec {
    Path(PathBuf),
    Registry { version: String },
    Git { url: String, reference: Option<(String, String)>, rev: String },
}

#[derive(Clone, Debug)]
pub struct DialectCrate {
    pub name: String,
    pub version: String,
    pub spec: DepSpec,
    pub features: Vec<String>,
    /// Directory of the package (for watching sources).
    pub dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub pliron_version: String,
    pub dialects: Vec<DialectCrate>,
}

/// Why a project cannot have a dialect engine.
#[derive(Clone, Debug)]
pub enum NoBundle {
    /// The project doesn't use pliron at all.
    NotPliron,
    /// It uses a pliron version/source we cannot instrument.
    Unsupported(String),
}

fn dep_spec(pkg: &Package) -> DepSpec {
    match &pkg.source {
        None => DepSpec::Path(pkg.manifest_path.parent().unwrap().into()),
        Some(src) => {
            let repr = &src.repr;
            if let Some(rest) = repr.strip_prefix("git+") {
                let (url_q, rev) = rest.split_once('#').unwrap_or((rest, ""));
                let (url, query) = url_q.split_once('?').unwrap_or((url_q, ""));
                let reference = query
                    .split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()));
                DepSpec::Git {
                    url: url.to_string(),
                    reference,
                    rev: rev.to_string(),
                }
            } else {
                DepSpec::Registry {
                    version: pkg.version.to_string(),
                }
            }
        }
    }
}

fn is_crates_io(pkg: &Package) -> bool {
    pkg.source.as_ref().is_some_and(|s| {
        s.repr == "registry+https://github.com/rust-lang/crates.io-index"
            || s.repr == "sparse+https://index.crates.io/"
    })
}

fn lib_target(pkg: &Package) -> Option<&cargo_metadata::Target> {
    pkg.targets.iter().find(|t| {
        t.kind
            .iter()
            .any(|k| matches!(k, TargetKind::Lib | TargetKind::RLib))
    })
}

fn sources_contain(dir: &Path, needles: &[&str], depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth < 8
                && p.file_name().is_some_and(|n| n != "target" && n != ".git")
                && sources_contain(&p, needles, depth + 1)
            {
                return true;
            }
        } else if p.extension().is_some_and(|x| x == "rs")
            && let Ok(text) = std::fs::read_to_string(&p)
            && needles.iter().any(|n| text.contains(n))
        {
            return true;
        }
    }
    false
}

/// `[workspace.metadata.pliron-lsp]` configuration.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct Config {
    /// Crates to always include.
    include: Vec<String>,
    /// Crates to never include.
    exclude: Vec<String>,
}

/// Choose the dialect crates of a project.
pub fn select(meta: &Metadata) -> Result<Selection, NoBundle> {
    let pliron = meta
        .packages
        .iter()
        .find(|p| p.name == "pliron")
        .ok_or(NoBundle::NotPliron)?;
    if (pliron.version.major, pliron.version.minor) != SUPPORTED_PLIRON {
        return Err(NoBundle::Unsupported(format!(
            "pliron {} is not supported (pliron-lsp instruments pliron {}.{}.x)",
            pliron.version, SUPPORTED_PLIRON.0, SUPPORTED_PLIRON.1
        )));
    }
    if !is_crates_io(pliron) {
        return Err(NoBundle::Unsupported(format!(
            "pliron from {} is not supported yet (only crates.io pliron {}.{}.x)",
            pliron.source.as_ref().map(|s| s.repr.as_str()).unwrap_or("a local path"),
            SUPPORTED_PLIRON.0,
            SUPPORTED_PLIRON.1
        )));
    }
    let config: Config = meta
        .workspace_metadata
        .get("pliron-lsp")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    let core: BTreeSet<&PackageId> = meta
        .packages
        .iter()
        .filter(|p| p.name == "pliron" || p.name == "pliron-derive")
        .map(|p| &p.id)
        .collect();
    let by_id: HashMap<&PackageId, &Package> = meta.packages.iter().map(|p| (&p.id, p)).collect();
    let resolve = meta.resolve.as_ref().ok_or(NoBundle::NotPliron)?;

    let mut dialects = Vec::new();
    for node in &resolve.nodes {
        let Some(pkg) = by_id.get(&node.id) else { continue };
        if core.contains(&node.id) {
            continue;
        }
        let Some(lib) = lib_target(pkg) else { continue };
        let forced = config.include.contains(&pkg.name);
        if config.exclude.contains(&pkg.name) {
            continue;
        }
        let uses_pliron = node.deps.iter().any(|d| {
            core.contains(&d.pkg)
                && d.dep_kinds.iter().any(|k| k.kind == DependencyKind::Normal)
        });
        if !forced && !uses_pliron {
            continue;
        }
        let dir = pkg.manifest_path.parent().unwrap().as_std_path().to_path_buf();
        // Crates needing the compiler's private crates cannot be built as
        // ordinary dependencies.
        if std::fs::read_to_string(&lib.src_path)
            .is_ok_and(|s| s.contains("feature(rustc_private)"))
        {
            continue;
        }
        if !forced && !sources_contain(&dir.join("src"), REGISTRATION_MARKERS, 0) {
            continue;
        }
        dialects.push(DialectCrate {
            name: pkg.name.clone(),
            version: pkg.version.to_string(),
            spec: dep_spec(pkg),
            features: node.features.clone(),
            dir,
        });
    }
    dialects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Selection {
        pliron_version: pliron.version.to_string(),
        dialects,
    })
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn toml_path(p: &Path) -> String {
    toml_str(&p.display().to_string())
}

/// The generated bundle manifest.
pub fn manifest(sel: &Selection, user_patches: &str) -> String {
    let mut m = String::new();
    m.push_str(
        "# Generated by pliron-lsp. Do not edit: it is regenerated automatically.\n\
         [package]\nname = \"pliron-lsp-bundle\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [[bin]]\nname = \"pliron-lsp-engine-bundle\"\npath = \"src/main.rs\"\n\n\
         [dependencies]\npliron-lsp-engine = { path = \"vendor/pliron-lsp-engine\" }\n",
    );
    for (i, d) in sel.dialects.iter().enumerate() {
        let mut fields = vec![format!("package = {}", toml_str(&d.name))];
        match &d.spec {
            DepSpec::Path(p) => fields.push(format!("path = {}", toml_path(p))),
            DepSpec::Registry { version } => {
                fields.push(format!("version = {}", toml_str(&format!("={version}"))))
            }
            DepSpec::Git {
                url,
                reference,
                rev,
            } => {
                fields.push(format!("git = {}", toml_str(url)));
                match reference {
                    Some((k, v)) => fields.push(format!("{k} = {}", toml_str(v))),
                    None if !rev.is_empty() => fields.push(format!("rev = {}", toml_str(rev))),
                    None => {}
                }
            }
        }
        fields.push("default-features = false".into());
        let feats: Vec<String> = d.features.iter().map(|f| toml_str(f)).collect();
        fields.push(format!("features = [{}]", feats.join(", ")));
        writeln!(m, "d{i} = {{ {} }}", fields.join(", ")).unwrap();
    }
    m.push_str(
        "\n[patch.crates-io]\npliron = { path = \"vendor/pliron\" }\npliron-derive = { path = \"vendor/pliron-derive\" }\n",
    );
    m.push_str(user_patches);
    m.push_str(
        "\n[profile.dev]\nopt-level = 1\ndebug = 0\nincremental = true\n\n\
         [profile.dev.package.\"*\"]\nopt-level = 2\n\n\
         # The bundle is its own workspace even though it lives under target/.\n[workspace]\n",
    );
    m
}

/// `[patch]` sections of the project's root manifest (except for pliron
/// itself), with relative paths made absolute.
pub fn user_patches(root: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return String::new();
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return String::new();
    };
    let Some(patch) = value.get("patch").and_then(|p| p.as_table()) else {
        return String::new();
    };
    let mut out = String::new();
    for (registry, entries) in patch {
        let Some(entries) = entries.as_table() else { continue };
        let mut table = toml::Table::new();
        for (name, spec) in entries {
            if name == "pliron" || name == "pliron-derive" {
                continue;
            }
            let mut spec = spec.clone();
            if let Some(t) = spec.as_table_mut()
                && let Some(p) = t.get("path").and_then(|p| p.as_str())
            {
                let abs = root.join(p);
                t.insert("path".into(), toml::Value::String(abs.display().to_string()));
            }
            table.insert(name.clone(), spec);
        }
        if table.is_empty() {
            continue;
        }
        if registry == "crates-io" {
            // Merge into the `[patch.crates-io]` table written above.
            for (name, spec) in table {
                writeln!(out, "{name} = {}", spec).unwrap();
            }
        } else {
            let mut wrapper = toml::Table::new();
            let mut inner = toml::Table::new();
            inner.insert(registry.clone(), toml::Value::Table(table));
            wrapper.insert("patch".into(), toml::Value::Table(inner));
            write!(out, "\n{}", toml::to_string(&wrapper).unwrap_or_default()).unwrap();
        }
    }
    out
}

/// Remove tables that refer to files we do not vendor (tests, examples,
/// benches) and dev-dependencies from a normalized registry manifest.
fn clean_manifest(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            skipping = t.starts_with("[[test]]")
                || t.starts_with("[[example]]")
                || t.starts_with("[[bench]]")
                || t.contains("dev-dependencies");
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

const PROTOCOL_MANIFEST: &str = "[package]\nname = \"pliron-lsp-protocol\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\n";
const ENGINE_MANIFEST: &str = "[package]\nname = \"pliron-lsp-engine\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\npliron = \"0.18\"\npliron-lsp-protocol = { path = \"../pliron-lsp-protocol\" }\nserde_json = \"1\"\n";

/// Write `content` to `path` unless it already has exactly that content
/// (so that cargo does not rebuild because of touched files).
fn write_if_changed(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("pliron-lsp-tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Where everything pliron-lsp writes for a project lives.
pub fn state_dir(meta: &Metadata) -> PathBuf {
    meta.target_directory.as_std_path().join("pliron-lsp")
}

/// Generate the bundle package; returns its directory.
pub fn generate(meta: &Metadata, sel: &Selection) -> anyhow::Result<PathBuf> {
    let root = meta.workspace_root.as_std_path();
    let dir = state_dir(meta).join("bundle");
    std::fs::create_dir_all(&dir)?;
    for (rel, content) in embedded::FILES {
        let (krate, rest) = rel.split_once('/').unwrap();
        let path = dir.join("vendor").join(krate).join(rest);
        let content = if rest == "Cargo.toml" {
            clean_manifest(content)
        } else {
            content.to_string()
        };
        write_if_changed(&path, content.as_bytes())?;
    }
    write_if_changed(
        &dir.join("vendor/pliron-lsp-protocol/Cargo.toml"),
        PROTOCOL_MANIFEST.as_bytes(),
    )?;
    write_if_changed(
        &dir.join("vendor/pliron-lsp-engine/Cargo.toml"),
        ENGINE_MANIFEST.as_bytes(),
    )?;
    write_if_changed(
        &dir.join("Cargo.toml"),
        manifest(sel, &user_patches(root)).as_bytes(),
    )?;

    let mut main = String::from("// Generated by pliron-lsp.\n#![allow(warnings)]\n");
    for i in 0..sel.dialects.len() {
        writeln!(main, "use d{i} as _;").unwrap();
    }
    let crates: Vec<String> = sel
        .dialects
        .iter()
        .map(|d| format!("({:?}, {:?})", d.name, d.version))
        .collect();
    write!(
        main,
        "\nfn main() {{\n    pliron_lsp_engine::run_stdio(pliron_lsp_engine::BundleInfo {{\n        bundle_id: {:?},\n        crates: &[{}],\n    }});\n}}\n",
        root.display().to_string(),
        crates.join(", ")
    )
    .unwrap();
    write_if_changed(&dir.join("src/main.rs"), main.as_bytes())?;

    // Pin the project's dependency versions.
    if let Ok(lock) = std::fs::read(root.join("Cargo.lock")) {
        let dest = dir.join("Cargo.lock");
        if !dest.exists() {
            write_if_changed(&dest, &lock)?;
        }
    }
    Ok(dir)
}

/// Progress of a bundle build.
#[derive(Clone, Debug)]
pub enum BuildEvent {
    /// `Compiling foo v1.2.3` and the like.
    Progress(String),
}

/// Build the bundle and copy the engine to a unique path; returns it.
pub fn build(
    meta: &Metadata,
    bundle_dir: &Path,
    mut on_event: impl FnMut(BuildEvent),
) -> anyhow::Result<PathBuf> {
    let root = meta.workspace_root.as_std_path();
    let state = state_dir(meta);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut child = Command::new(cargo)
        .args(["build", "--bin", "pliron-lsp-engine-bundle", "--message-format=json"])
        .arg("--manifest-path")
        .arg(bundle_dir.join("Cargo.toml"))
        // The project's directory decides the toolchain (rust-toolchain.toml)
        // and cargo configuration.
        .current_dir(root)
        .env("CARGO_TARGET_DIR", state.join("target"))
        .env("CARGO_TERM_COLOR", "never")
        .env("PLIRON_LSP_ENGINE_SRC_HASH", engine_src_hash())
        .env_remove("RUSTC_WRAPPER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run cargo")?;

    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let err_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    let mut exe = None;
    let mut errors = Vec::new();
    let mut success = false;
    for line in BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
    {
        while let Ok(l) = rx.try_recv() {
            let t = l.trim();
            if t.starts_with("Compiling") || t.starts_with("Checking") {
                on_event(BuildEvent::Progress(t.to_string()));
            }
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v["reason"].as_str() {
            Some("compiler-artifact") => {
                if let Some(e) = v["executable"].as_str()
                    && v["target"]["name"] == "pliron-lsp-engine-bundle"
                {
                    exe = Some(PathBuf::from(e));
                }
            }
            Some("compiler-message") => {
                if v["message"]["level"] == "error"
                    && let Some(r) = v["message"]["rendered"].as_str()
                {
                    errors.push(r.to_string());
                }
            }
            Some("build-finished") => success = v["success"].as_bool().unwrap_or(false),
            _ => {}
        }
    }
    let status = child.wait()?;
    let _ = err_thread.join();
    let stderr_rest: Vec<String> = rx.try_iter().collect();
    if !status.success() || !success {
        let mut msg = errors.join("\n");
        if msg.is_empty() {
            msg = stderr_rest.join("\n");
        }
        bail!("building the dialect engine failed:\n{msg}");
    }
    let exe = exe.context("cargo did not report the engine binary")?;

    // Run a copy, so that rebuilding (or `cargo clean`) never touches a
    // running executable.
    let engines = state.join("engines");
    std::fs::create_dir_all(&engines)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = engines.join(format!("engine-{stamp}{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&exe, &dest)?;
    // Keep the three most recent copies.
    let mut old: Vec<_> = std::fs::read_dir(&engines)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != &dest)
        .collect();
    old.sort();
    while old.len() > 2 {
        let _ = std::fs::remove_file(old.remove(0));
    }
    Ok(dest)
}

/// Directories whose `.rs` changes require rebuilding a bundle.
pub fn watched_dirs(sel: &Selection) -> Vec<PathBuf> {
    sel.dialects
        .iter()
        .filter(|d| matches!(d.spec, DepSpec::Path(_)))
        .map(|d| d.dir.clone())
        .collect()
}

/// Summaries for status messages.
pub fn describe(sel: &Selection) -> String {
    let names: Vec<&str> = sel.dialects.iter().map(|d| d.name.as_str()).collect();
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for d in &sel.dialects {
        *by_kind
            .entry(match d.spec {
                DepSpec::Path(_) => "local",
                DepSpec::Registry { .. } => "crates.io",
                DepSpec::Git { .. } => "git",
            })
            .or_default() += 1;
    }
    format!("{} dialect crate(s): {}", names.len(), names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_manifest_drops_tests_and_dev_deps() {
        let m = "[package]\nname = \"x\"\n\n[[test]]\nname = \"t\"\npath = \"tests/t.rs\"\n\n[dependencies.a]\nversion = \"1\"\n\n[dev-dependencies.b]\nversion = \"1\"\n\n[target.'cfg(unix)'.dev-dependencies]\n\n[target.'cfg(unix)'.dependencies.c]\nversion = \"1\"\n";
        let c = clean_manifest(m);
        assert!(c.contains("[dependencies.a]"));
        assert!(c.contains("[target.'cfg(unix)'.dependencies.c]"));
        assert!(!c.contains("[[test]]"));
        assert!(!c.contains("dev-dependencies"));
    }

    #[test]
    fn manifest_specs() {
        let sel = Selection {
            pliron_version: "0.18.0".into(),
            dialects: vec![
                DialectCrate {
                    name: "my-dialect".into(),
                    version: "0.1.0".into(),
                    spec: DepSpec::Path("/w/my dialect".into()),
                    features: vec!["default".into()],
                    dir: "/w/my dialect".into(),
                },
                DialectCrate {
                    name: "pliron-llvm".into(),
                    version: "0.18.0".into(),
                    spec: DepSpec::Registry {
                        version: "0.18.0".into(),
                    },
                    features: vec![],
                    dir: "/r".into(),
                },
                DialectCrate {
                    name: "g".into(),
                    version: "0.2.0".into(),
                    spec: DepSpec::Git {
                        url: "https://github.com/a/b".into(),
                        reference: Some(("branch".into(), "main".into())),
                        rev: "abc".into(),
                    },
                    features: vec![],
                    dir: "/g".into(),
                },
            ],
        };
        let m = manifest(&sel, "");
        assert!(m.contains(r#"d0 = { package = "my-dialect", path = "/w/my dialect", default-features = false, features = ["default"] }"#), "{m}");
        assert!(m.contains(r#"d1 = { package = "pliron-llvm", version = "=0.18.0", default-features = false, features = [] }"#), "{m}");
        assert!(m.contains(r#"d2 = { package = "g", git = "https://github.com/a/b", branch = "main", default-features = false, features = [] }"#), "{m}");
        assert!(m.contains("pliron = { path = \"vendor/pliron\" }"));
        assert!(m.parse::<toml::Table>().is_ok());
    }

    #[test]
    fn embedded_sources_present() {
        let names: Vec<&str> = embedded::FILES.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"pliron-lsp-engine/src/lib.rs"));
        assert!(names.contains(&"pliron-lsp-protocol/src/lib.rs"));
        assert!(names.contains(&"pliron/src/lsp.rs"));
        assert!(names.contains(&"pliron-derive/src/lib.rs"));
        assert!(names.contains(&"pliron/Cargo.toml"));
    }
}

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

/// The instrumentation patch for a pliron version line, if supported.
fn patch_for(version: &cargo_metadata::semver::Version) -> Option<(&'static str, &'static str)> {
    let key = format!("{}.{}", version.major, version.minor);
    embedded::PATCHES
        .iter()
        .find(|(v, _)| *v == key)
        .map(|(v, p)| (*v, *p))
}

/// The pliron version lines pliron-lsp can instrument.
pub fn supported_versions() -> Vec<&'static str> {
    embedded::PATCHES.iter().map(|(v, _)| *v).collect()
}

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
    Registry {
        version: String,
    },
    Git {
        url: String,
        reference: Option<(String, String)>,
        rev: String,
    },
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

/// Where the project's pliron comes from (it is copied and instrumented).
#[derive(Clone, Debug)]
pub enum PlironSource {
    /// crates.io: separate pliron and pliron-derive crate directories.
    /// Their versions may differ: pliron 0.16 and 0.17 accept any
    /// pliron-derive 0.x.
    Registry {
        pliron_dir: PathBuf,
        derive_dir: PathBuf,
        derive_version: String,
    },
    /// A git checkout of the pliron repository.
    Git {
        url: String,
        reference: Option<(String, String)>,
        rev: String,
        /// Root of the checkout (the pliron package + its workspace).
        root: PathBuf,
    },
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub pliron_version: String,
    pub pliron: PlironSource,
    /// The version line of the instrumentation patch.
    pub patch_version: &'static str,
    pub dialects: Vec<DialectCrate>,
    /// The project's `pliron-lsp-api`, if a dialect uses it: the engine
    /// then runs the dialects' hooks.
    pub api: Option<DepSpec>,
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

/// Features to enable for a dialect crate in a bundle: the project's
/// resolved features, except those that only add native bindings the
/// language server never uses (pliron-llvm's `llvm-sys` needs an LLVM
/// installation and is not needed to parse or verify LLVM-dialect IR).
fn dialect_features(name: &str, resolved: &[String]) -> Vec<String> {
    resolved
        .iter()
        .filter(|f| !(name == "pliron-llvm" && (*f == "llvm-sys" || *f == "default")))
        .cloned()
        .collect()
}

/// The same selection with all dialect features and hooks off (fallback
/// when a build with the project's features fails, e.g. because of native
/// dependencies or an incompatible `pliron-lsp-api`).
pub fn minimal_features(sel: &Selection) -> Selection {
    let mut sel = sel.clone();
    for d in &mut sel.dialects {
        d.features.clear();
    }
    sel.api = None;
    sel
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
        let Some(pkg) = by_id.get(&node.id) else {
            continue;
        };
        if core.contains(&node.id) {
            continue;
        }
        let Some(lib) = lib_target(pkg) else { continue };
        let forced = config.include.contains(&pkg.name);
        if config.exclude.contains(&pkg.name) {
            continue;
        }
        let uses_pliron = node.deps.iter().any(|d| {
            core.contains(&d.pkg) && d.dep_kinds.iter().any(|k| k.kind == DependencyKind::Normal)
        });
        if !forced && !uses_pliron {
            continue;
        }
        let dir = pkg
            .manifest_path
            .parent()
            .unwrap()
            .as_std_path()
            .to_path_buf();
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
            features: dialect_features(&pkg.name, &node.features),
            dir,
        });
    }
    dialects.sort_by(|a, b| a.name.cmp(&b.name));
    // Nothing beyond what the reference engine has (builtin + llvm).
    if dialects.iter().all(|d| d.name == "pliron-llvm") {
        return Err(NoBundle::NotPliron);
    }

    let Some((patch_version, _)) = patch_for(&pliron.version) else {
        return Err(NoBundle::Unsupported(format!(
            "pliron {} is not supported (pliron-lsp instruments pliron {})",
            pliron.version,
            supported_versions().join(", ")
        )));
    };
    let pliron_dir = pliron
        .manifest_path
        .parent()
        .unwrap()
        .as_std_path()
        .to_path_buf();
    let source = if is_crates_io(pliron) {
        // The pliron-derive that pliron actually uses (not necessarily of
        // the same version).
        let derive = resolve
            .nodes
            .iter()
            .find(|n| n.id == pliron.id)
            .and_then(|n| {
                n.deps
                    .iter()
                    .filter_map(|d| by_id.get(&d.pkg))
                    .find(|p| p.name == "pliron-derive")
            })
            .copied()
            .or_else(|| {
                meta.packages
                    .iter()
                    .find(|p| p.name == "pliron-derive" && p.version == pliron.version)
            })
            .ok_or_else(|| NoBundle::Unsupported("pliron-derive not found".into()))?;
        PlironSource::Registry {
            pliron_dir,
            derive_dir: derive
                .manifest_path
                .parent()
                .unwrap()
                .as_std_path()
                .to_path_buf(),
            derive_version: derive.version.to_string(),
        }
    } else {
        match dep_spec(pliron) {
            DepSpec::Git {
                url,
                reference,
                rev,
            } => PlironSource::Git {
                url,
                reference,
                rev,
                root: pliron_dir,
            },
            _ => {
                return Err(NoBundle::Unsupported(
                    "pliron from a local path is not supported (cargo cannot patch path dependencies)"
                        .into(),
                ));
            }
        }
    };
    let api = meta
        .packages
        .iter()
        .find(|p| p.name == "pliron-lsp-api")
        .map(dep_spec);
    Ok(Selection {
        pliron_version: pliron.version.to_string(),
        pliron: source,
        patch_version,
        dialects,
        api,
    })
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn toml_path(p: &Path) -> String {
    toml_str(&p.display().to_string())
}

/// Where a dependency comes from, as manifest fields.
fn spec_fields(spec: &DepSpec) -> Vec<String> {
    let mut fields = Vec::new();
    match spec {
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
    fields
}

/// Where a project's engine build puts things. With a [`cache_dir`], the
/// build directory and the vendored (instrumented) crates are shared by all
/// projects, so that pliron and its dependencies are compiled once.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The generated bundle package (per project).
    pub bundle: PathBuf,
    /// Copies of the built engine binary (per project).
    pub engines: PathBuf,
    /// `CARGO_TARGET_DIR`.
    pub target: PathBuf,
    /// Vendored crates, in directories named after their content.
    pub vendor: PathBuf,
    /// The engine binary's name (per project, as the build directory may be
    /// shared).
    pub bin: String,
}

/// The directory shared by the engine builds of all projects:
/// `PLIRON_LSP_CACHE_DIR`, else the platform's cache directory. `None` (or
/// an empty `PLIRON_LSP_CACHE_DIR`) keeps everything in the project's
/// target directory.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PLIRON_LSP_CACHE_DIR") {
        return (!d.is_empty()).then(|| PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Caches/pliron-lsp"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("pliron-lsp"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".cache")))
            .map(|d| d.join("pliron-lsp"))
    }
}

pub fn layout(meta: &Metadata) -> Layout {
    let state = state_dir(meta);
    let shared = cache_dir();
    let root = meta.workspace_root.to_string();
    Layout {
        bundle: state.join("bundle"),
        engines: state.join("engines"),
        target: shared
            .as_ref()
            .map_or_else(|| state.join("target"), |c| c.join("target")),
        vendor: shared.map_or_else(|| state.join("vendor"), |c| c.join("vendor")),
        bin: format!(
            "pliron-lsp-engine-{:08x}",
            pliron_lsp_protocol::text_hash(&root) as u32
        ),
    }
}

/// The vendored crates of a bundle.
#[derive(Clone, Debug)]
pub struct Vendored {
    pub engine: PathBuf,
    pub pliron: PathBuf,
    pub derive: PathBuf,
}

/// The generated bundle manifest.
pub fn manifest(sel: &Selection, user_patches: &str, vendored: &Vendored, bin: &str) -> String {
    let mut m = String::new();
    write!(
        m,
        "# Generated by pliron-lsp. Do not edit: it is regenerated automatically.\n\
         [package]\nname = \"pliron-lsp-bundle\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [[bin]]\nname = {}\npath = \"src/main.rs\"\n\n\
         [dependencies]\npliron-lsp-engine = {{ path = {} }}\n",
        toml_str(bin),
        toml_path(&vendored.engine)
    )
    .unwrap();
    for (i, d) in sel.dialects.iter().enumerate() {
        let mut fields = vec![format!("package = {}", toml_str(&d.name))];
        fields.extend(spec_fields(&d.spec));
        fields.push("default-features = false".into());
        let feats: Vec<String> = d.features.iter().map(|f| toml_str(f)).collect();
        fields.push(format!("features = [{}]", feats.join(", ")));
        writeln!(m, "d{i} = {{ {} }}", fields.join(", ")).unwrap();
    }
    // Use the instrumented copy of the project's own pliron.
    let (pliron, derive) = (toml_path(&vendored.pliron), toml_path(&vendored.derive));
    match &sel.pliron {
        PlironSource::Registry { .. } => write!(
            m,
            "\n[patch.crates-io]\npliron = {{ path = {pliron} }}\npliron-derive = {{ path = {derive} }}\n"
        )
        .unwrap(),
        PlironSource::Git { url, .. } => writeln!(
            m,
            "\n[patch.{}]\npliron = {{ path = {pliron} }}\npliron-derive = {{ path = {derive} }}\n\n[patch.crates-io]",
            toml_str(url)
        )
        .unwrap(),
    }
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
        let Some(entries) = entries.as_table() else {
            continue;
        };
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
                t.insert(
                    "path".into(),
                    toml::Value::String(abs.display().to_string()),
                );
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
/// The vendored engine's manifest: its `pliron` is the project's pliron
/// (which the bundle patches with the instrumented copy).
fn engine_manifest(sel: &Selection, protocol: &Path) -> String {
    let pliron = match &sel.pliron {
        PlironSource::Registry { .. } => toml_str(&format!("={}", sel.pliron_version)).to_string(),
        PlironSource::Git {
            url,
            reference,
            rev,
            ..
        } => {
            let r = match reference {
                Some((k, v)) => format!(", {k} = {}", toml_str(v)),
                None if !rev.is_empty() => format!(", rev = {}", toml_str(rev)),
                None => String::new(),
            };
            format!("{{ git = {}{r} }}", toml_str(url))
        }
    };
    let cfg = format!("pliron_{}", sel.patch_version.replace('.', "_"));
    // The project's own pliron-lsp-api, with its hooks turned on.
    let (api, hooks) = match &sel.api {
        Some(spec) => (
            format!(
                "pliron-lsp-api = {{ {}, features = [\"engine\"], optional = true }}\n",
                spec_fields(spec).join(", ")
            ),
            ", \"hooks\"",
        ),
        None => (String::new(), ""),
    };
    format!(
        "[package]\nname = \"pliron-lsp-engine\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\npliron = {pliron}\npliron-lsp-protocol = {{ path = {} }}\nserde_json = \"1\"\n{api}\n[features]\ndefault = [\"{cfg}\"{hooks}]\n{cfg} = []\nhooks = [{}]\n",
        toml_path(protocol),
        if sel.api.is_some() {
            "\"dep:pliron-lsp-api\""
        } else {
            ""
        }
    )
}

/// The literal-keyword parser of declarative formats.
const DERIVE_STRING_PARSER: &str = "::pliron::combine::parser::char::string(";
const DERIVE_KEYWORD_PARSER: &str = "::pliron::lsp::keyword(";

/// pliron-derive's instrumentation: the literals of declarative formats are
/// parsed with `pliron::lsp::keyword`, which records them. That is the only
/// change, made by substitution so that it fits every pliron-derive version.
fn instrument_derive(
    src: &Path,
    out: &Path,
    files: &mut HashMap<PathBuf, Vec<u8>>,
) -> anyhow::Result<()> {
    for e in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let p = e?.path();
        let name = p.file_name().unwrap_or_default().to_owned();
        if p.is_dir() {
            instrument_derive(&p, &out.join(&name), files)?;
        } else if p.extension().is_some_and(|x| x == "rs") {
            let text = std::fs::read_to_string(&p)?;
            if text.contains(DERIVE_STRING_PARSER) {
                let text = text.replace(DERIVE_STRING_PARSER, DERIVE_KEYWORD_PARSER);
                files.insert(out.join(&name), text.into_bytes());
            }
        }
    }
    Ok(())
}

/// Copy the project's pliron sources into `dest` and instrument them:
/// pliron with the patch for its version, pliron-derive by
/// [`instrument_derive`].
fn vendor_pliron(sel: &Selection, dest: &Path) -> anyhow::Result<()> {
    let (_, diff) = embedded::PATCHES
        .iter()
        .find(|(v, _)| *v == sel.patch_version)
        .context("missing instrumentation patch")?;
    // Sources and copies. Patch paths are relative to the pliron crate.
    let (trees, pliron_src, derive_src, out_pliron, out_derive) = match &sel.pliron {
        PlironSource::Registry {
            pliron_dir,
            derive_dir,
            ..
        } => (
            vec![
                (pliron_dir.clone(), dest.join("pliron")),
                (derive_dir.clone(), dest.join("pliron-derive")),
            ],
            pliron_dir.clone(),
            derive_dir.clone(),
            dest.join("pliron"),
            dest.join("pliron-derive"),
        ),
        PlironSource::Git { root, .. } => (
            vec![(root.clone(), dest.join("pliron-git"))],
            root.clone(),
            root.join("pliron-derive"),
            dest.join("pliron-git"),
            dest.join("pliron-git/pliron-derive"),
        ),
    };

    // The final content of every changed file, computed before writing
    // anything: files whose content does not change keep their mtime, so
    // regenerating a bundle does not make cargo rebuild pliron.
    let mut files: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let patched = crate::patcher::apply_in_memory(diff, |p| {
        Ok(std::fs::read_to_string(pliron_src.join(p)).ok())
    })
    .context("instrumenting pliron")?;
    for (p, text) in patched {
        files.insert(out_pliron.join(p), text.into_bytes());
    }
    instrument_derive(&derive_src.join("src"), &out_derive.join("src"), &mut files)?;
    if matches!(sel.pliron, PlironSource::Registry { .. }) {
        for (src, out) in [(&pliron_src, &out_pliron), (&derive_src, &out_derive)] {
            let cleaned = clean_manifest(&std::fs::read_to_string(src.join("Cargo.toml"))?);
            files.insert(out.join("Cargo.toml"), cleaned.into_bytes());
        }
    }
    for (from, to) in &trees {
        copy_tree(from, to, &files)?;
    }
    // Files the patch creates.
    for (path, content) in &files {
        write_if_changed(path, content)?;
    }
    Ok(())
}

/// Mirror a source tree (skipping `target/` and VCS directories), writing
/// only files whose content changed; `replace` gives the content of some
/// destination files.
fn copy_tree(from: &Path, to: &Path, replace: &HashMap<PathBuf, Vec<u8>>) -> anyhow::Result<()> {
    for e in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let e = e?;
        let name = e.file_name();
        let p = e.path();
        if p.is_dir() {
            if name == "target" || name == ".git" {
                continue;
            }
            copy_tree(&p, &to.join(&name), replace)?;
        } else if p.is_file() {
            let dest = to.join(&name);
            match replace.get(&dest) {
                Some(content) => write_if_changed(&dest, content)?,
                None => write_if_changed(&dest, &std::fs::read(&p)?)?,
            }
        }
    }
    Ok(())
}

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
    let layout = layout(meta);
    let dir = layout.bundle.clone();
    std::fs::create_dir_all(&dir)?;
    let hash =
        |parts: &[&str]| format!("{:016x}", pliron_lsp_protocol::text_hash(&parts.join("\0")));

    // The engine library and protocol, named after their sources (and, for
    // the engine, its manifest, which depends on the project's pliron).
    let protocol = layout
        .vendor
        .join(format!("protocol-{}", hash(&[embedded::SRC_HASH])))
        .join("pliron-lsp-protocol");
    let engine_toml = engine_manifest(sel, &protocol);
    let engine = layout
        .vendor
        .join(format!(
            "engine-{}",
            hash(&[embedded::SRC_HASH, &engine_toml])
        ))
        .join("pliron-lsp-engine");
    for (rel, content) in embedded::FILES {
        let (krate, rest) = rel.split_once('/').unwrap();
        let base = match krate {
            "pliron-lsp-protocol" => &protocol,
            "pliron-lsp-engine" => &engine,
            _ => continue,
        };
        write_if_changed(&base.join(rest), content.as_bytes())?;
    }
    write_if_changed(&protocol.join("Cargo.toml"), PROTOCOL_MANIFEST.as_bytes())?;
    write_if_changed(&engine.join("Cargo.toml"), engine_toml.as_bytes())?;

    // The instrumented pliron, named after its sources and the patch.
    let (_, patch) = embedded::PATCHES
        .iter()
        .find(|(v, _)| *v == sel.patch_version)
        .context("missing instrumentation patch")?;
    let (source, version) = match &sel.pliron {
        PlironSource::Registry {
            pliron_dir,
            derive_dir,
            ..
        } => (
            format!("{}\0{}", pliron_dir.display(), derive_dir.display()),
            sel.pliron_version.clone(),
        ),
        PlironSource::Git { root, .. } => (root.display().to_string(), sel.pliron_version.clone()),
    };
    let pliron_dir = layout.vendor.join(format!(
        "pliron-{version}-{}",
        hash(&[&source, patch, DERIVE_KEYWORD_PARSER])
    ));
    vendor_pliron(sel, &pliron_dir)?;
    let vendored = match &sel.pliron {
        PlironSource::Registry { .. } => Vendored {
            engine: engine.clone(),
            pliron: pliron_dir.join("pliron"),
            derive: pliron_dir.join("pliron-derive"),
        },
        PlironSource::Git { .. } => Vendored {
            engine: engine.clone(),
            pliron: pliron_dir.join("pliron-git"),
            derive: pliron_dir.join("pliron-git/pliron-derive"),
        },
    };
    write_if_changed(
        &dir.join("Cargo.toml"),
        manifest(sel, &user_patches(root), &vendored, &layout.bin).as_bytes(),
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
    toolchain: &crate::toolchain::Choice,
    mut on_event: impl FnMut(BuildEvent),
) -> anyhow::Result<PathBuf> {
    let root = meta.workspace_root.as_std_path();
    let layout = layout(meta);
    let mut cmd = match toolchain {
        crate::toolchain::Choice::Default => {
            Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        }
        crate::toolchain::Choice::Use(name) => {
            let mut c = Command::new("rustup");
            c.args(["run", name, "cargo"]);
            c
        }
    };
    let mut child = cmd
        .args([
            "build",
            "--bin",
            layout.bin.as_str(),
            "--message-format=json",
        ])
        .arg("--manifest-path")
        .arg(bundle_dir.join("Cargo.toml"))
        // The project's directory decides the cargo configuration, and the
        // toolchain unless one is given (see `toolchain`).
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &layout.target)
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
            // `Blocking`: another build uses the shared build directory.
            if t.starts_with("Compiling") || t.starts_with("Checking") || t.starts_with("Blocking")
            {
                on_event(BuildEvent::Progress(t.to_string()));
            }
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v["reason"].as_str() {
            Some("compiler-artifact") => {
                if let Some(e) = v["executable"].as_str()
                    && v["target"]["name"] == layout.bin.as_str()
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
    let engines = layout.engines;
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
    let hooks = if sel.api.is_some() {
        " (with pliron-lsp-api hooks)"
    } else {
        ""
    };
    format!(
        "{} dialect crate(s): {}{hooks}",
        names.len(),
        names.join(", ")
    )
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
            pliron: PlironSource::Registry {
                pliron_dir: "/p".into(),
                derive_dir: "/d".into(),
                derive_version: "0.18.0".into(),
            },
            patch_version: "0.18",
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
            api: Some(DepSpec::Path("/w/api".into())),
        };
        let vendored = Vendored {
            engine: "/c/engine".into(),
            pliron: "/c/p/pliron".into(),
            derive: "/c/p/pliron-derive".into(),
        };
        let m = manifest(&sel, "", &vendored, "pliron-lsp-engine-0123abcd");
        assert!(m.contains(r#"d0 = { package = "my-dialect", path = "/w/my dialect", default-features = false, features = ["default"] }"#), "{m}");
        assert!(m.contains(r#"d1 = { package = "pliron-llvm", version = "=0.18.0", default-features = false, features = [] }"#), "{m}");
        assert!(m.contains(r#"d2 = { package = "g", git = "https://github.com/a/b", branch = "main", default-features = false, features = [] }"#), "{m}");
        assert!(m.contains("pliron = { path = \"/c/p/pliron\" }"), "{m}");
        assert!(
            m.contains("[[bin]]\nname = \"pliron-lsp-engine-0123abcd\""),
            "{m}"
        );
        assert!(
            m.contains("pliron-lsp-engine = { path = \"/c/engine\" }"),
            "{m}"
        );
        assert!(m.parse::<toml::Table>().is_ok());
        // Hooks: the engine uses the project's pliron-lsp-api in engine mode.
        let e = engine_manifest(&sel, Path::new("/c/protocol"));
        assert!(
            e.contains("pliron-lsp-protocol = { path = \"/c/protocol\" }"),
            "{e}"
        );
        let t: toml::Table = e.parse().unwrap();
        assert_eq!(
            t["dependencies"]["pliron-lsp-api"]["path"].as_str(),
            Some("/w/api"),
            "{e}"
        );
        assert_eq!(t["features"]["default"].as_array().unwrap().len(), 2, "{e}");
        assert!(
            !engine_manifest(&minimal_features(&sel), Path::new("/c/protocol"))
                .contains("pliron-lsp-api")
        );
    }

    #[test]
    fn embedded_sources_present() {
        let names: Vec<&str> = embedded::FILES.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"pliron-lsp-engine/src/lib.rs"));
        assert!(names.contains(&"pliron-lsp-protocol/src/lib.rs"));
        assert_eq!(supported_versions(), ["0.16", "0.17", "0.18", "0.19"]);
        for (_, p) in embedded::PATCHES {
            assert!(p.contains("+++ b/src/lsp.rs"));
        }
    }

    #[test]
    fn derive_instrumentation_is_a_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("lib.rs"), "fn f() {}\n").unwrap();
        std::fs::write(
            src.join("sub/derive_format.rs"),
            "quote! { ::pliron::combine::parser::char::string(#lit) }\n",
        )
        .unwrap();
        let mut files = HashMap::new();
        instrument_derive(&src, Path::new("/out/src"), &mut files).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[Path::new("/out/src/sub/derive_format.rs")],
            b"quote! { ::pliron::lsp::keyword(#lit) }\n"
        );
    }

    #[test]
    fn git_pliron_manifest() {
        let sel = Selection {
            pliron_version: "0.17.0".into(),
            pliron: PlironSource::Git {
                url: "https://github.com/pliron-org/pliron.git".into(),
                reference: Some(("rev".into(), "e23ff9f".into())),
                rev: "e23ff9f".into(),
                root: "/r".into(),
            },
            patch_version: "0.17",
            dialects: vec![],
            api: None,
        };
        let vendored = Vendored {
            engine: "/c/engine".into(),
            pliron: "/c/p/pliron-git".into(),
            derive: "/c/p/pliron-git/pliron-derive".into(),
        };
        let m = manifest(&sel, "x = { path = \"/x\" }\n", &vendored, "e");
        assert!(m.contains("[patch.\"https://github.com/pliron-org/pliron.git\"]\npliron = { path = \"/c/p/pliron-git\" }"), "{m}");
        assert!(
            m.contains("[patch.crates-io]\nx = { path = \"/x\" }"),
            "{m}"
        );
        assert!(m.parse::<toml::Table>().is_ok(), "{m}");
        let e = engine_manifest(&sel, Path::new("/c/protocol"));
        assert!(
            e.contains(
                "pliron = { git = \"https://github.com/pliron-org/pliron.git\", rev = \"e23ff9f\" }"
            ),
            "{e}"
        );
        assert!(e.contains("default = [\"pliron_0_17\"]"), "{e}");
        assert!(e.parse::<toml::Table>().is_ok(), "{e}");
    }
}

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
        .find(|(v, ..)| *v == key)
        .map(|(v, _, p)| (*v, *p))
}

/// The pliron version lines pliron-lsp can instrument.
pub fn supported_versions() -> Vec<&'static str> {
    let mut v: Vec<&str> = embedded::PATCHES.iter().map(|(v, ..)| *v).collect();
    v.dedup();
    v
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

/// The project's metadata, offline when possible. Without a lockfile (a
/// library, say) cargo resolves from scratch, and offline it would pick
/// whatever git revisions happen to be cached instead of the ones `cargo
/// build` fetches, and write them into a new `Cargo.lock`.
pub fn load_metadata(dir: &Path) -> anyhow::Result<Metadata> {
    let mut cmd = MetadataCommand::new();
    cmd.current_dir(dir);
    if has_lockfile(dir)
        && let Ok(m) = cmd.clone().other_options(vec!["--offline".into()]).exec()
    {
        return Ok(m);
    }
    cmd.exec().context("cargo metadata failed")
}

/// Whether `dir` or a parent (its workspace root) has a `Cargo.lock`.
fn has_lockfile(dir: &Path) -> bool {
    dir.ancestors().any(|d| d.join("Cargo.lock").is_file())
}

/// How a dependency is specified in the generated manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSpec {
    Path(PathBuf),
    Registry {
        version: String,
    },
    /// Exactly as the project declares it: cargo treats `rev = <commit>`
    /// and the default branch as different sources even at the same
    /// commit, and would link the crate twice. The commit is pinned by the
    /// project's `Cargo.lock`, which the bundle starts from.
    Git {
        url: String,
        /// `branch`/`tag`/`rev` and its value; `None` for the default branch.
        reference: Option<(String, String)>,
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
        /// As in [`DepSpec::Git`].
        reference: Option<(String, String)>,
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
                let url_q = rest.split_once('#').map_or(rest, |(u, _)| u);
                let (url, query) = url_q.split_once('?').unwrap_or((url_q, ""));
                let reference = query
                    .split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()));
                DepSpec::Git {
                    url: url.to_string(),
                    reference,
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

/// The pliron to build the engine for. The graph can have several (the
/// dependencies of an unrelated dependency, say), but one binary can link
/// only one: their registries collide. Take the one the workspace members
/// use, the newest if there are several.
fn project_pliron<'a>(
    meta: &'a Metadata,
    by_id: &HashMap<&PackageId, &'a Package>,
) -> Option<&'a Package> {
    let all = meta.packages.iter().filter(|p| p.name == "pliron");
    let used = meta
        .resolve
        .iter()
        .flat_map(|r| &r.nodes)
        .filter(|n| meta.workspace_members.contains(&n.id))
        .flat_map(|n| &n.deps)
        .filter_map(|d| by_id.get(&d.pkg).copied())
        .filter(|p| p.name == "pliron");
    used.max_by(|a, b| a.version.cmp(&b.version))
        .or_else(|| all.max_by(|a, b| a.version.cmp(&b.version)))
}

/// Choose the dialect crates of a project.
pub fn select(meta: &Metadata) -> Result<Selection, NoBundle> {
    let by_id: HashMap<&PackageId, &Package> = meta.packages.iter().map(|p| (&p.id, p)).collect();
    let resolve = meta.resolve.as_ref().ok_or(NoBundle::NotPliron)?;
    let pliron = project_pliron(meta, &by_id).ok_or(NoBundle::NotPliron)?;
    let config: Config = meta
        .workspace_metadata
        .get("pliron-lsp")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    // That pliron and its pliron-derive; dialects of any other pliron in
    // the graph are left out.
    let mut core: BTreeSet<&PackageId> = resolve
        .nodes
        .iter()
        .filter(|n| n.id == pliron.id)
        .flat_map(|n| &n.deps)
        .filter(|d| by_id.get(&d.pkg).is_some_and(|p| p.name == "pliron-derive"))
        .map(|d| &d.pkg)
        .collect();
    core.insert(&pliron.id);
    let other_core: BTreeSet<&PackageId> = meta
        .packages
        .iter()
        .filter(|p| (p.name == "pliron" || p.name == "pliron-derive") && !core.contains(&p.id))
        .map(|p| &p.id)
        .collect();

    let mut dialects = Vec::new();
    for node in &resolve.nodes {
        let Some(pkg) = by_id.get(&node.id) else {
            continue;
        };
        if core.contains(&node.id) || other_core.contains(&node.id) {
            continue;
        }
        let Some(lib) = lib_target(pkg) else { continue };
        let forced = config.include.contains(&pkg.name);
        if config.exclude.contains(&pkg.name) {
            continue;
        }
        let normal_deps = || {
            node.deps
                .iter()
                .filter(|d| d.dep_kinds.iter().any(|k| k.kind == DependencyKind::Normal))
        };
        if normal_deps().any(|d| other_core.contains(&d.pkg)) {
            continue;
        }
        let uses_pliron = normal_deps().any(|d| core.contains(&d.pkg));
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
            DepSpec::Git { url, reference } => PlironSource::Git {
                url,
                reference,
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
        DepSpec::Git { url, reference } => {
            fields.push(format!("git = {}", toml_str(url)));
            if let Some((k, v)) = reference {
                fields.push(format!("{k} = {}", toml_str(v)));
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
    /// `CARGO_TARGET_DIR`: in the shared cache, one per instrumented pliron
    /// (all projects with that pliron share it; it is removed with it).
    pub target: PathBuf,
    /// The instrumented pliron's directory name under `vendor`.
    pub pliron_key: String,
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

/// The instrumented pliron's directory name: its version, and a hash of
/// its sources and the instrumentation.
fn pliron_key(sel: &Selection) -> anyhow::Result<String> {
    let (patch, _) = instrument_pliron(sel)?;
    let source = match &sel.pliron {
        PlironSource::Registry {
            pliron_dir,
            derive_dir,
            ..
        } => format!("{}\0{}", pliron_dir.display(), derive_dir.display()),
        PlironSource::Git { root, .. } => root.display().to_string(),
    };
    let hash =
        pliron_lsp_protocol::text_hash(&[source.as_str(), patch, DERIVE_KEYWORD_PARSER].join("\0"));
    Ok(format!("pliron-{}-{hash:016x}", sel.pliron_version))
}

pub fn layout(meta: &Metadata, sel: &Selection) -> anyhow::Result<Layout> {
    let state = state_dir(meta);
    let shared = cache_dir();
    let root = meta.workspace_root.to_string();
    let pliron_key = pliron_key(sel)?;
    Ok(Layout {
        bundle: state.join("bundle"),
        engines: state.join("engines"),
        target: shared.as_ref().map_or_else(
            || state.join("target"),
            |c| c.join("target").join(&pliron_key),
        ),
        vendor: shared.map_or_else(|| state.join("vendor"), |c| c.join("vendor")),
        bin: format!(
            "pliron-lsp-engine-{:08x}",
            pliron_lsp_protocol::text_hash(&root) as u32
        ),
        pliron_key,
    })
}

/// Name of the file whose mtime says when a cache entry was last used.
const LAST_USED: &str = ".last-used";

/// Remove cache entries unused for this long...
pub const CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);
/// ...and build directories larger than this (cargo never deletes stale
/// artifacts).
pub const CACHE_MAX_TARGET: u64 = 20 << 30;

fn mark_used(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(LAST_USED), b"")?;
    Ok(())
}

/// When a cache entry was last used.
fn last_used(entry: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(entry.join(LAST_USED))
        .or_else(|_| std::fs::metadata(entry))
        .and_then(|m| m.modified())
        .ok()
}

/// Total size of the files under `p`.
pub fn disk_size(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p)
            .map(|es| es.flatten().map(|e| disk_size(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// An entry of the shared cache: an instrumented pliron, an engine source
/// version, or the build directory of an instrumented pliron.
#[derive(Clone, Debug)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub bytes: u64,
    pub last_used: Option<std::time::SystemTime>,
}

/// The entries of the cache at `cache`.
pub fn cache_entries(cache: &Path) -> Vec<CacheEntry> {
    let mut out = Vec::new();
    for sub in ["vendor", "target"] {
        let Ok(entries) = std::fs::read_dir(cache.join(sub)) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            out.push(CacheEntry {
                bytes: disk_size(&path),
                last_used: last_used(&path),
                path,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Remove the cache entries unused for `max_age`, build directories larger
/// than `max_target` (unless used within the last hour), and leftovers of
/// older cache layouts. Returns what was removed.
pub fn gc(cache: &Path, max_age: std::time::Duration, max_target: u64) -> Vec<CacheEntry> {
    let now = std::time::SystemTime::now();
    let age = |e: &CacheEntry| {
        e.last_used
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default()
    };
    let mut removed = Vec::new();
    for e in cache_entries(cache) {
        let in_target = e.path.parent().is_some_and(|p| p.ends_with("target"));
        let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let stale = age(&e) > max_age
            // The build directory used to be shared by all plirons.
            || (in_target && !name.starts_with("pliron-"))
            || (in_target && e.bytes > max_target && age(&e) > std::time::Duration::from_secs(3600));
        if !stale {
            continue;
        }
        let ok = if e.path.is_dir() {
            std::fs::remove_dir_all(&e.path).is_ok()
        } else {
            std::fs::remove_file(&e.path).is_ok()
        };
        if ok {
            removed.push(e);
        }
    }
    removed
}

/// [`gc`] of the shared cache with the default limits, at most once a day.
pub fn auto_gc() -> Vec<CacheEntry> {
    let Some(cache) = cache_dir() else {
        return Vec::new();
    };
    let marker = cache.join(".last-gc");
    let recent = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(24 * 3600));
    if recent || std::fs::create_dir_all(&cache).is_err() {
        return Vec::new();
    }
    let _ = std::fs::write(&marker, b"");
    gc(&cache, CACHE_MAX_AGE, CACHE_MAX_TARGET)
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
        PlironSource::Git { url, reference, .. } => {
            let r = match reference {
                Some((k, v)) => format!(", {k} = {}", toml_str(v)),
                None => String::new(),
            };
            format!("{{ git = {}{r} }}", toml_str(url))
        }
    };
    let cfg = format!("pliron_{}", sel.patch_version.replace('.', "_"));
    // All version features exist; the project's is on.
    let versions: String = supported_versions()
        .iter()
        .map(|v| format!("pliron_{} = []\n", v.replace('.', "_")))
        .collect();
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
        "[package]\nname = \"pliron-lsp-engine\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\npliron = {pliron}\npliron-lsp-protocol = {{ path = {} }}\nserde_json = \"1\"\n{api}\n[features]\ndefault = [\"{cfg}\"{hooks}]\n{versions}hooks = [{}]\n",
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
/// The pliron package's directory.
fn pliron_src(sel: &Selection) -> &Path {
    match &sel.pliron {
        PlironSource::Registry { pliron_dir, .. } => pliron_dir,
        PlironSource::Git { root, .. } => root,
    }
}

/// The instrumentation patch for the project's pliron and the files it
/// produces: the first of its version line's patches that applies (later
/// ones follow changes on pliron's development branch).
fn instrument_pliron(sel: &Selection) -> anyhow::Result<(&'static str, Vec<(String, String)>)> {
    let src = pliron_src(sel);
    let mut errors = Vec::new();
    for (_, name, diff) in embedded::PATCHES
        .iter()
        .filter(|(v, ..)| *v == sel.patch_version)
    {
        match crate::patcher::apply_in_memory(diff, |p| {
            Ok(std::fs::read_to_string(src.join(p)).ok())
        }) {
            Ok(files) => return Ok((diff, files)),
            Err(e) => errors.push(format!("{name}: {e:#}")),
        }
    }
    if errors.is_empty() {
        bail!("missing instrumentation patch");
    }
    bail!("instrumenting pliron: {}", errors.join("; "))
}

fn vendor_pliron(sel: &Selection, dest: &Path) -> anyhow::Result<()> {
    let (_, patched) = instrument_pliron(sel)?;
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
    let layout = layout(meta, sel)?;
    let dir = layout.bundle.clone();
    std::fs::create_dir_all(&dir)?;
    if cache_dir().is_some() {
        // Builds of this project before the cache was shared.
        let state = state_dir(meta);
        for old in [state.join("target"), dir.join("vendor")] {
            let _ = std::fs::remove_dir_all(old);
        }
    }
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
    let pliron_dir = layout.vendor.join(&layout.pliron_key);
    vendor_pliron(sel, &pliron_dir)?;
    // Cache entries in use (see `gc`).
    for used in [
        protocol.parent(),
        engine.parent(),
        Some(pliron_dir.as_path()),
        Some(layout.target.as_path()),
    ]
    .into_iter()
    .flatten()
    {
        mark_used(used)?;
    }
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
    pin_project_lock(root, &dir)?;
    Ok(dir)
}

/// The project's lockfile, as the copy the bundle's `Cargo.lock` was made
/// from.
const PROJECT_LOCK: &str = "project.lock";

/// Pin the project's dependency versions: the bundle starts from its
/// `Cargo.lock`. cargo adds the bundle's own packages to the copy, so it is
/// replaced only when the project's lockfile changes (`cargo update`, say);
/// otherwise the engine would keep building the old revisions of the
/// dialect crates against a newly vendored pliron.
fn pin_project_lock(root: &Path, dir: &Path) -> anyhow::Result<()> {
    let Ok(lock) = std::fs::read(root.join("Cargo.lock")) else {
        return Ok(());
    };
    let copied = dir.join(PROJECT_LOCK);
    if std::fs::read(&copied).ok().as_deref() != Some(&lock[..]) {
        write_if_changed(&dir.join("Cargo.lock"), &lock)?;
        write_if_changed(&copied, &lock)?;
    }
    Ok(())
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
    sel: &Selection,
    bundle_dir: &Path,
    toolchain: &crate::toolchain::Choice,
    mut on_event: impl FnMut(BuildEvent),
) -> anyhow::Result<PathBuf> {
    let root = meta.workspace_root.as_std_path();
    let layout = layout(meta, sel)?;
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
    cmd.args([
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
    .stderr(Stdio::piped());
    let mut child = crate::builds::spawn(&mut cmd).context("cannot run cargo")?;

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
    fn lockfile_of_the_project_or_its_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let member = tmp.path().join("ws/member");
        std::fs::create_dir_all(&member).unwrap();
        assert!(!has_lockfile(&member));
        std::fs::write(tmp.path().join("ws/Cargo.lock"), "").unwrap();
        assert!(has_lockfile(&member));
    }

    #[test]
    fn bundle_lock_follows_the_project_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, dir) = (tmp.path().join("p"), tmp.path().join("b"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let bundle_lock = || std::fs::read_to_string(dir.join("Cargo.lock")).unwrap();
        std::fs::write(root.join("Cargo.lock"), "v1").unwrap();
        pin_project_lock(&root, &dir).unwrap();
        assert_eq!(bundle_lock(), "v1");
        // cargo adds the bundle's packages; they stay while the project's
        // lockfile is the same.
        std::fs::write(dir.join("Cargo.lock"), "v1 + bundle").unwrap();
        pin_project_lock(&root, &dir).unwrap();
        assert_eq!(bundle_lock(), "v1 + bundle");
        // After `cargo update` in the project, its versions are pinned again.
        std::fs::write(root.join("Cargo.lock"), "v2").unwrap();
        pin_project_lock(&root, &dir).unwrap();
        assert_eq!(bundle_lock(), "v2");
    }

    #[test]
    fn dialects_of_one_pliron() {
        // plironnx: its own pliron 0.20 dialects, and burn's cubecl crates
        // with pliron 0.19 in the graph.
        let tmp = tempfile::tempdir().unwrap();
        let registry = "registry+https://github.com/rust-lang/crates.io-index";
        let mut packages = vec![];
        let mut nodes = vec![];
        let mut add = |name: &str, version: &str, deps: &[&str], dialect: bool| {
            let id = format!("{name}@{version}");
            let dir = tmp.path().join(&id);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            let lib = dir.join("src/lib.rs");
            std::fs::write(&lib, if dialect { "#[pliron_op]" } else { "" }).unwrap();
            packages.push(serde_json::json!({
                "name": name, "version": version, "id": id, "source": registry,
                "dependencies": [], "features": {},
                "manifest_path": dir.join("Cargo.toml"),
                "targets": [{ "name": name, "kind": ["lib"], "src_path": lib }],
            }));
            let deps: Vec<_> = deps
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "name": d.split('@').next().unwrap(), "pkg": d,
                        "dep_kinds": [{ "kind": null, "target": null }],
                    })
                })
                .collect();
            nodes.push(serde_json::json!({ "id": id, "deps": deps, "dependencies": [] }));
        };
        add("pliron-derive", "0.19.0", &[], false);
        add("pliron", "0.19.0", &["pliron-derive@0.19.0"], false);
        add("pliron-derive", "0.20.0", &[], false);
        add("pliron", "0.20.0", &["pliron-derive@0.20.0"], false);
        add("pliron-llvm", "0.19.0", &["pliron@0.19.0"], true);
        add("pliron-llvm", "0.20.0", &["pliron@0.20.0"], true);
        add("cubecl-ir", "0.11.4", &["pliron@0.19.0"], true);
        add(
            "tensor",
            "0.1.0",
            &["pliron@0.20.0", "pliron-llvm@0.20.0"],
            true,
        );
        add("onnx", "0.1.0", &["pliron@0.20.0", "tensor@0.1.0"], true);
        let meta: Metadata = serde_json::from_value(serde_json::json!({
            "packages": packages,
            "workspace_members": ["onnx@0.1.0"],
            "resolve": { "nodes": nodes, "root": null },
            "workspace_root": tmp.path(),
            "target_directory": tmp.path().join("target"),
            "version": 1,
        }))
        .unwrap();
        let sel = select(&meta).unwrap();
        assert_eq!(sel.pliron_version, "0.20.0");
        let PlironSource::Registry { derive_version, .. } = &sel.pliron else {
            panic!("{:?}", sel.pliron);
        };
        assert_eq!(derive_version, "0.20.0");
        let dialects: Vec<_> = sel
            .dialects
            .iter()
            .map(|d| format!("{}@{}", d.name, d.version))
            .collect();
        assert_eq!(
            dialects,
            ["onnx@0.1.0", "pliron-llvm@0.20.0", "tensor@0.1.0"]
        );
    }

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
                    },
                    features: vec![],
                    dir: "/g".into(),
                },
                DialectCrate {
                    name: "h".into(),
                    version: "0.3.0".into(),
                    spec: DepSpec::Git {
                        url: "https://github.com/a/h.git".into(),
                        reference: None,
                    },
                    features: vec![],
                    dir: "/h".into(),
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
        // The default branch stays the default branch: pinning the commit
        // with `rev` would make cargo build a second copy of the crate.
        assert!(m.contains(r#"d3 = { package = "h", git = "https://github.com/a/h.git", default-features = false, features = [] }"#), "{m}");
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
        assert_eq!(
            supported_versions(),
            ["0.16", "0.17", "0.18", "0.19", "0.20"]
        );
        for (_, _, p) in embedded::PATCHES {
            assert!(p.contains("+++ b/src/lsp.rs"));
        }
    }

    #[test]
    fn cache_gc_removes_unused_large_and_legacy_entries() {
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path();
        let entry = |sub: &str, name: &str, bytes: usize, days_ago: u64| {
            let p = c.join(sub).join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("data"), vec![0u8; bytes]).unwrap();
            mark_used(&p).unwrap();
            let t = SystemTime::now() - Duration::from_secs(days_ago * 24 * 3600);
            std::fs::File::options()
                .write(true)
                .open(p.join(LAST_USED))
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        entry("vendor", "pliron-0.18.0-aaaa", 10, 0); // in use
        entry("vendor", "pliron-0.16.0-bbbb", 10, 40); // unused for 40 days
        entry("target", "pliron-0.18.0-aaaa", 10, 0);
        entry("target", "pliron-0.17.0-cccc", 5000, 2); // too large
        entry("target", "debug", 10, 0); // the old layout (one shared dir)
        std::fs::write(c.join("target/CACHEDIR.TAG"), "x").unwrap();

        let mut removed: Vec<PathBuf> = gc(c, CACHE_MAX_AGE, 1000)
            .into_iter()
            .map(|e| e.path.strip_prefix(c).unwrap().to_path_buf())
            .collect();
        removed.sort();
        let p = |a: &str, b: &str| Path::new(a).join(b);
        assert_eq!(
            removed,
            [
                p("target", "CACHEDIR.TAG"),
                p("target", "debug"),
                p("target", "pliron-0.17.0-cccc"),
                p("vendor", "pliron-0.16.0-bbbb"),
            ]
        );
        assert!(c.join("vendor/pliron-0.18.0-aaaa").exists());
        assert!(c.join("target/pliron-0.18.0-aaaa").exists());
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

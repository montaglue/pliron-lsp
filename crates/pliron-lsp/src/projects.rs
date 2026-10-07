//! Cargo projects and the background jobs that give them dialect engines.

use std::path::{Path, PathBuf};
use std::process::Command;

use crossbeam_channel::Sender;

use crate::bundle::{self, BuildEvent, NoBundle};
use crate::index::DialectIndex;

/// Result of preparing a project's engine.
#[derive(Debug)]
pub enum Outcome {
    /// A dialect engine was built.
    Engine {
        exe: PathBuf,
        /// The generated bundle package.
        bundle_dir: PathBuf,
        /// Directories whose changes require a rebuild.
        watched: Vec<PathBuf>,
        description: String,
    },
    /// The project does not define dialects: the reference engine applies.
    NoDialects,
    /// The project uses pliron in a way we cannot support.
    Unsupported(String),
    /// Something failed (metadata, build).
    Failed(String),
}

#[derive(Debug)]
pub enum JobEvent {
    Progress { root: PathBuf, message: String },
    /// The project's dialect sources were indexed (before building).
    Index { root: PathBuf, index: DialectIndex, dirs: Vec<PathBuf> },
    Done { root: PathBuf, outcome: Outcome },
}

/// Source directories to index for a selection: the dialect crates plus
/// pliron itself (the builtin dialect).
fn index_dirs(meta: &cargo_metadata::Metadata, sel: &bundle::Selection) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = sel.dialects.iter().map(|d| d.dir.join("src")).collect();
    if let Some(p) = meta.packages.iter().find(|p| p.name == "pliron")
        && let Some(dir) = p.manifest_path.parent()
    {
        dirs.push(dir.as_std_path().join("src"));
    }
    dirs
}

/// Build the index for documents served by the reference engine (pliron +
/// pliron-llvm sources from the cargo registry, if available).
pub fn spawn_reference_index(tx: Sender<JobEvent>) {
    std::thread::spawn(move || {
        let mut dirs = Vec::new();
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
        if let Some(home) = cargo_home
            && let Ok(regs) = std::fs::read_dir(home.join("registry/src"))
        {
            for reg in regs.flatten() {
                for krate in ["pliron-0.18.0", "pliron-llvm-0.18.0"] {
                    let d = reg.path().join(krate).join("src");
                    if d.is_dir() && !dirs.iter().any(|x: &PathBuf| x.ends_with(Path::new(krate).join("src"))) {
                        dirs.push(d);
                    }
                }
            }
        }
        let index = DialectIndex::build(&dirs);
        let _ = tx.send(JobEvent::Index {
            root: PathBuf::from(REFERENCE_KEY),
            index,
            dirs,
        });
    });
}

/// Key under which the reference index is reported.
pub const REFERENCE_KEY: &str = "reference";

/// The cargo workspace root containing `file`, if any.
pub fn workspace_root_of(file: &Path) -> Option<PathBuf> {
    let dir = bundle::find_project_dir(file)?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(&dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return Some(dir);
    }
    let manifest = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    manifest.parent().map(Path::to_path_buf).or(Some(dir))
}

/// Prepare (generate + build) the engine of the project at `root` on a
/// background thread.
pub fn spawn(root: PathBuf, tx: Sender<JobEvent>) {
    std::thread::Builder::new()
        .name("pliron-lsp-bundle".into())
        .spawn(move || {
            let progress = |message: String| {
                let _ = tx.send(JobEvent::Progress {
                    root: root.clone(),
                    message,
                });
            };
            let index = |root: &Path, dirs: &[PathBuf]| {
                let _ = tx.send(JobEvent::Index {
                    root: root.to_path_buf(),
                    index: DialectIndex::build(dirs),
                    dirs: dirs.to_vec(),
                });
            };
            let outcome = run(&root, &progress, &index);
            let _ = tx.send(JobEvent::Done { root, outcome });
        })
        .expect("spawn bundle job");
}

fn run(root: &Path, progress: &dyn Fn(String), index: &dyn Fn(&Path, &[PathBuf])) -> Outcome {
    progress("reading cargo metadata".into());
    let meta = match bundle::load_metadata(root) {
        Ok(m) => m,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    let sel = match bundle::select(&meta) {
        Ok(s) if s.dialects.is_empty() => return Outcome::NoDialects,
        Ok(s) => s,
        Err(NoBundle::NotPliron) => return Outcome::NoDialects,
        Err(NoBundle::Unsupported(r)) => return Outcome::Unsupported(r),
    };
    let dirs = index_dirs(&meta, &sel);
    index(root, &dirs);
    let description = bundle::describe(&sel);
    progress(format!("building dialect engine ({description})"));
    let dir = match bundle::generate(&meta, &sel) {
        Ok(d) => d,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    let exe = match bundle::build(&meta, &dir, |ev| match ev {
        BuildEvent::Progress(m) => progress(m),
    }) {
        Ok(exe) => exe,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    let mut watched = bundle::watched_dirs(&sel);
    watched.push(root.join("Cargo.toml"));
    watched.push(root.join("Cargo.lock"));
    Outcome::Engine {
        exe,
        bundle_dir: dir,
        watched,
        description,
    }
}

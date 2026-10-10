//! Cargo projects and the background jobs that give them dialect engines.

use std::path::{Path, PathBuf};
use std::process::Command;

use crossbeam_channel::Sender;

use crate::bundle::{self, BuildEvent, NoBundle};
use crate::index::DialectIndex;
use crate::toolchain::{self, Choice};

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
    Progress {
        root: PathBuf,
        message: String,
    },
    /// The project's dialect sources were indexed (before building).
    Index {
        root: PathBuf,
        index: DialectIndex,
        dirs: Vec<PathBuf>,
    },
    Done {
        root: PathBuf,
        outcome: Outcome,
    },
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
                    if d.is_dir()
                        && !dirs
                            .iter()
                            .any(|x: &PathBuf| x.ends_with(Path::new(krate).join("src")))
                    {
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

/// Prepare (generate + build) the dialect engine of the project at `root`,
/// synchronously.
pub fn run(root: &Path, progress: &dyn Fn(String), index: &dyn Fn(&Path, &[PathBuf])) -> Outcome {
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
    let mut description = bundle::describe(&sel);
    let plan = toolchain::plan(root, toolchain::required(&meta, &sel));
    if let Some(note) = &plan.note {
        progress(note.clone());
    }
    progress(format!("building dialect engine ({description})"));
    let dir = match bundle::generate(&meta, &sel) {
        Ok(d) => d,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    let build = |sel: &bundle::Selection, choice: &Choice| -> anyhow::Result<PathBuf> {
        let dir = bundle::generate(&meta, sel)?;
        bundle::build(&meta, sel, &dir, choice, |ev| match ev {
            BuildEvent::Progress(m) => progress(m),
        })
    };
    let mut choice = plan.choice.clone();
    let exe = match build(&sel, &choice) {
        Ok(exe) => exe,
        Err(first) => {
            let first = format!("{first:#}");
            let mut exe = None;
            // A dependency may need a newer compiler without declaring it.
            if choice == Choice::Default
                && toolchain::too_old(&first)
                && let Some((_, have)) = &plan.active
                && let Some(t) = toolchain::newest_after(root, *have)
            {
                progress(format!(
                    "retrying with toolchain {} (rustc {})",
                    t.name,
                    toolchain::display(t.version)
                ));
                choice = Choice::Use(t.name);
                exe = build(&sel, &choice).ok();
            }
            // Native dependencies enabled by the project's features may not
            // build here; the dialects themselves rarely need them.
            let exe = match exe {
                Some(exe) => Ok(exe),
                None => {
                    progress("retrying with minimal features".into());
                    build(&bundle::minimal_features(&sel), &choice)
                }
            };
            match exe {
                Ok(exe) => exe,
                Err(_) => {
                    let mut msg = first;
                    if toolchain::too_old(&msg) {
                        msg.push_str(&format!(
                            "\nhint: {}",
                            plan.note.as_deref().unwrap_or(
                                "the engine needs a newer Rust compiler: run `rustup update stable`, \
                                 or add a rust-toolchain.toml to the project"
                            )
                        ));
                    }
                    return Outcome::Failed(msg);
                }
            }
        }
    };
    if let Choice::Use(name) = &choice {
        description.push_str(&format!("; built with toolchain {name}"));
    }
    // Keep the shared build cache from growing forever (at most daily).
    for e in bundle::auto_gc() {
        progress(format!(
            "removed {} from the build cache (no longer used, {:.1} GiB)",
            e.path.display(),
            e.bytes as f64 / (1u64 << 30) as f64
        ));
    }
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

//! The Rust toolchain that builds a project's dialect engine.
//!
//! A project's own choice wins: a `rust-toolchain(.toml)` file, a rustup
//! directory override or `RUSTUP_TOOLCHAIN`. Otherwise rustup would use its
//! default toolchain; when that one is older than what the engine's
//! dependencies declare (`rust-version`), an installed toolchain that is new
//! enough builds the engine instead (`rustup run <toolchain> cargo`).
//! Nothing is installed automatically: when no installed toolchain is new
//! enough, the build is tried anyway and its error explains what to install.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};

use cargo_metadata::{DependencyKind, Metadata, PackageId};

use crate::bundle::Selection;

/// A rustc version; pre-release tags such as `-nightly` are ignored.
pub type Version = (u64, u64, u64);

/// What the engine itself and the instrumented pliron need (pliron 0.18
/// uses const `TypeId::of`).
pub const ENGINE_MIN: Version = (1, 91, 0);

pub fn parse_version(s: &str) -> Option<Version> {
    let core = s.trim().split(['-', '+', ' ']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

pub fn display((major, minor, patch): Version) -> String {
    if patch == 0 {
        format!("{major}.{minor}")
    } else {
        format!("{major}.{minor}.{patch}")
    }
}

/// The rustc version an engine for `sel` needs: the newest `rust-version`
/// declared by the packages it builds (pliron, the dialect crates and their
/// normal and build dependencies), and at least [`ENGINE_MIN`]. Returns the
/// version and what needs it.
pub fn required(meta: &Metadata, sel: &Selection) -> (Version, String) {
    let mut need = (ENGINE_MIN, "pliron-lsp's engine".to_string());
    let Some(resolve) = &meta.resolve else {
        return need;
    };
    let packages: HashMap<&PackageId, &cargo_metadata::Package> =
        meta.packages.iter().map(|p| (&p.id, p)).collect();
    let nodes: HashMap<&PackageId, &cargo_metadata::Node> =
        resolve.nodes.iter().map(|n| (&n.id, n)).collect();
    let mut todo: Vec<&PackageId> = meta
        .packages
        .iter()
        .filter(|p| {
            p.name == "pliron"
                || sel
                    .dialects
                    .iter()
                    .any(|d| d.name == p.name && d.version == p.version.to_string())
        })
        .map(|p| &p.id)
        .collect();
    let mut seen: HashSet<&PackageId> = todo.iter().copied().collect();
    while let Some(id) = todo.pop() {
        if let Some(p) = packages.get(id)
            && let Some(v) = &p.rust_version
            && (v.major, v.minor, v.patch) > need.0
        {
            need = ((v.major, v.minor, v.patch), p.name.clone());
        }
        for dep in nodes.get(id).map(|n| n.deps.as_slice()).unwrap_or_default() {
            let used = dep
                .dep_kinds
                .iter()
                .any(|k| matches!(k.kind, DependencyKind::Normal | DependencyKind::Build));
            if used && seen.insert(&dep.pkg) {
                todo.push(&dep.pkg);
            }
        }
    }
    need
}

/// An installed toolchain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub version: Version,
}

impl Installed {
    /// Preference among toolchains that are new enough: stable, then pinned
    /// versions, beta, nightly; the newest of each.
    fn rank(&self) -> (u8, Reverse<Version>) {
        let class = if self.name.starts_with("stable") {
            0
        } else if self.name.starts_with(|c: char| c.is_ascii_digit()) {
            1
        } else if self.name.starts_with("beta") {
            2
        } else if self.name.starts_with("nightly") {
            3
        } else {
            4
        };
        (class, Reverse(self.version))
    }
}

/// The preferred toolchain of `installed` that is at least `need`.
pub fn best(installed: &[Installed], need: Version) -> Option<&Installed> {
    installed
        .iter()
        .filter(|t| t.version >= need)
        .min_by_key(|t| t.rank())
}

/// `rustup show active-toolchain` output: the toolchain, and why rustup
/// chose it unless it is the default (a toolchain file, an override or
/// `RUSTUP_TOOLCHAIN`).
pub fn parse_active(output: &str) -> Option<(String, Option<String>)> {
    let line = output.lines().next()?.trim();
    let (name, reason) = line.split_once(' ').unwrap_or((line, ""));
    let reason = reason.trim().trim_start_matches('(').trim_end_matches(')');
    let explicit = (reason != "default").then(|| reason.to_string());
    (!name.is_empty()).then(|| (name.to_string(), explicit))
}

fn rustup(args: &[&str], dir: &Path) -> Option<String> {
    let out = Command::new("rustup")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn version_of(toolchain: &str, dir: &Path) -> Option<Version> {
    let out = rustup(&["run", toolchain, "rustc", "-vV"], dir)?;
    out.lines()
        .find_map(|l| l.strip_prefix("release: "))
        .and_then(parse_version)
}

/// Installed toolchains with their rustc versions.
pub fn installed(dir: &Path) -> Vec<Installed> {
    let Some(list) = rustup(&["toolchain", "list"], dir) else {
        return Vec::new();
    };
    list.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|name| {
            Some(Installed {
                name: name.to_string(),
                version: version_of(name, dir)?,
            })
        })
        .collect()
}

/// Which toolchain builds the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Whatever cargo selects in the project's directory.
    Default,
    /// `rustup run <toolchain> cargo ...`
    Use(String),
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub choice: Choice,
    /// The toolchain cargo would select and its version, if rustup is used.
    pub active: Option<(String, Version)>,
    /// What to tell the user (a substitution, or what is missing).
    pub note: Option<String>,
}

/// Choose the toolchain for building in `dir` an engine that needs `need`.
pub fn plan(dir: &Path, (need, why): (Version, String)) -> Plan {
    let keep = |active, note| Plan {
        choice: Choice::Default,
        active,
        note,
    };
    let Some((name, explicit)) = rustup(&["show", "active-toolchain"], dir)
        .as_deref()
        .and_then(parse_active)
    else {
        // No rustup: cargo is what it is.
        return keep(None, None);
    };
    let Some(have) = version_of(&name, dir) else {
        return keep(None, None);
    };
    let active = Some((name.clone(), have));
    if have >= need {
        return keep(active, None);
    }
    if let Some(reason) = explicit {
        return keep(
            active,
            Some(format!(
                "{why} needs rustc {}, but toolchain {name} ({reason}) is rustc {}",
                display(need),
                display(have)
            )),
        );
    }
    let installed = installed(dir);
    match best(&installed, need) {
        Some(t) => Plan {
            choice: Choice::Use(t.name.clone()),
            active,
            note: Some(format!(
                "using toolchain {} (rustc {}): {why} needs rustc {}, the default toolchain is rustc {}",
                t.name,
                display(t.version),
                display(need),
                display(have)
            )),
        },
        None => keep(
            active,
            Some(format!(
                "{why} needs rustc {}, but the newest installed toolchain is rustc {}; \
                 run `rustup update stable` (or add a rust-toolchain.toml to the project)",
                display(need),
                display(installed.iter().map(|t| t.version).max().unwrap_or(have)),
            )),
        ),
    }
}

/// Does a build error look like the compiler is too old?
pub fn too_old(error: &str) -> bool {
    [
        "requires rustc",
        "is not supported by the following package",
        "is not yet stable",
        "error[E0658]",
    ]
    .iter()
    .any(|p| error.contains(p))
}

/// The newest installed toolchain newer than `than` (for a retry when a
/// build fails because of an undeclared compiler requirement).
pub fn newest_after(dir: &Path, than: Version) -> Option<Installed> {
    installed(dir)
        .into_iter()
        .filter(|t| t.version > than)
        .max_by_key(|t| (t.version, Reverse(t.rank().0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.101.0-nightly"), Some((1, 101, 0)));
        assert_eq!(parse_version("1.95"), Some((1, 95, 0)));
        assert_eq!(
            parse_version("1.94.1 (e408947bf 2026-03-25)"),
            Some((1, 94, 1))
        );
        assert_eq!(parse_version("x"), None);
        assert_eq!(display((1, 95, 0)), "1.95");
    }

    #[test]
    fn active_toolchain() {
        assert_eq!(
            parse_active("stable-aarch64-apple-darwin (default)\n"),
            Some(("stable-aarch64-apple-darwin".into(), None))
        );
        let pinned =
            "nightly-2026-04-03-aarch64-apple-darwin (overridden by '/p/rust-toolchain.toml')";
        assert_eq!(
            parse_active(pinned).unwrap().1.as_deref(),
            Some("overridden by '/p/rust-toolchain.toml'")
        );
        let env = "1.91-aarch64-apple-darwin (overridden by environment variable RUSTUP_TOOLCHAIN)";
        assert!(parse_active(env).unwrap().1.is_some());
    }

    #[test]
    fn prefers_stable_then_pinned_then_nightly() {
        let t = |name: &str, v: Version| Installed {
            name: name.into(),
            version: v,
        };
        let installed = [
            t("stable-x", (1, 94, 1)),
            t("1.91-x", (1, 91, 0)),
            t("1.96-x", (1, 96, 0)),
            t("nightly-x", (1, 101, 0)),
            t("nightly-2026-10-01-x", (1, 99, 0)),
        ];
        assert_eq!(best(&installed, (1, 95, 0)).unwrap().name, "1.96-x");
        assert_eq!(best(&installed, (1, 97, 0)).unwrap().name, "nightly-x");
        assert_eq!(best(&installed, (1, 90, 0)).unwrap().name, "stable-x");
        assert!(best(&installed, (1, 102, 0)).is_none());
    }

    #[test]
    fn version_errors() {
        assert!(too_old(
            "error: rustc 1.94.1 is not supported by the following package:\n  hi_sparse_bitset@0.10.0 requires rustc 1.95"
        ));
        assert!(too_old("error[E0658]: use of unstable library feature"));
        assert!(!too_old("error[E0425]: cannot find value `x`"));
    }
}

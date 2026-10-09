//! A small unified-diff applier with fuzz (like `patch -F3`), used to
//! instrument a project's own pliron sources when generating a dialect
//! bundle.

use anyhow::{Context, bail};

#[derive(Debug, Clone)]
struct Hunk {
    old_start: usize,
    /// Lines of the hunk: (' ' | '-' | '+', text).
    lines: Vec<(char, String)>,
}

#[derive(Debug, Clone)]
struct FilePatch {
    /// Path relative to the patch root (`b/` prefix stripped).
    path: String,
    /// The file is created by the patch.
    new_file: bool,
    hunks: Vec<Hunk>,
}

fn parse(diff: &str) -> anyhow::Result<Vec<FilePatch>> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = diff.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(old) = line.strip_prefix("--- ") {
            let new = lines
                .next()
                .and_then(|l| l.strip_prefix("+++ "))
                .context("malformed diff: `---` without `+++`")?;
            let strip = |p: &str| {
                let p = p.split('\t').next().unwrap_or(p).trim();
                p.split_once('/')
                    .map(|(_, r)| r.to_string())
                    .unwrap_or_default()
            };
            files.push(FilePatch {
                path: strip(new),
                new_file: old.starts_with("/dev/null") || old.contains("1970-01-01"),
                hunks: Vec::new(),
            });
        } else if let Some(h) = line.strip_prefix("@@ -") {
            let old_start: usize = h
                .split([',', ' '])
                .next()
                .and_then(|n| n.parse().ok())
                .context("malformed hunk header")?;
            let mut hunk = Hunk {
                old_start,
                lines: Vec::new(),
            };
            while let Some(l) = lines.peek() {
                if l.starts_with("@@") || l.starts_with("--- ") || l.starts_with("diff ") {
                    break;
                }
                let l = lines.next().unwrap();
                if l.starts_with('\\') {
                    continue; // "\ No newline at end of file"
                }
                let (c, text) = match l.chars().next() {
                    Some(c @ (' ' | '-' | '+')) => (c, l[1..].to_string()),
                    None => (' ', String::new()),
                    _ => continue,
                };
                hunk.lines.push((c, text));
            }
            files
                .last_mut()
                .context("hunk before file header")?
                .hunks
                .push(hunk);
        }
    }
    Ok(files)
}

fn eq_fuzzy(a: &str, b: &str) -> bool {
    a.trim_end() == b.trim_end()
}

/// Find `needle` in `hay` near `expected`; returns the start index.
fn find(hay: &[String], needle: &[&str], expected: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(expected.min(hay.len()));
    }
    let matches_at = |i: usize| {
        i + needle.len() <= hay.len() && needle.iter().zip(&hay[i..]).all(|(n, h)| eq_fuzzy(n, h))
    };
    (0..hay.len())
        .filter(|&i| matches_at(i))
        .min_by_key(|&i| i.abs_diff(expected))
}

fn apply_hunk(file: &mut Vec<String>, hunk: &Hunk, offset: &mut isize) -> bool {
    let expected = (hunk.old_start as isize - 1 + *offset).max(0) as usize;
    // Try with full context, then dropping up to 3 context lines at each end.
    for fuzz in 0..=3usize {
        let lead = hunk
            .lines
            .iter()
            .take_while(|(c, _)| *c == ' ')
            .count()
            .min(fuzz);
        let trail = hunk
            .lines
            .iter()
            .rev()
            .take_while(|(c, _)| *c == ' ')
            .count()
            .min(fuzz);
        let body = &hunk.lines[lead..hunk.lines.len() - trail];
        let old: Vec<&str> = body
            .iter()
            .filter(|(c, _)| *c != '+')
            .map(|(_, t)| t.as_str())
            .collect();
        let new: Vec<String> = body
            .iter()
            .filter(|(c, _)| *c != '-')
            .map(|(_, t)| t.clone())
            .collect();
        if let Some(at) = find(file, &old, expected + lead) {
            *offset +=
                at as isize - (expected + lead) as isize + new.len() as isize - old.len() as isize;
            file.splice(at..at + old.len(), new);
            return true;
        }
    }
    false
}

/// Apply `diff` in memory. `read` returns the current content of a patch
/// path (`None` if it does not exist). Returns the new contents of every
/// patched file, keyed by patch path.
pub fn apply_in_memory(
    diff: &str,
    read: impl Fn(&str) -> anyhow::Result<Option<String>>,
) -> anyhow::Result<Vec<(String, String)>> {
    apply_in_memory_filtered(diff, |_| true, read)
}

/// [`apply_in_memory`], for the files of the diff whose path passes
/// `filter` only.
pub fn apply_in_memory_filtered(
    diff: &str,
    filter: impl Fn(&str) -> bool,
    read: impl Fn(&str) -> anyhow::Result<Option<String>>,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut failed = Vec::new();
    let mut out = Vec::new();
    for fp in parse(diff)?.into_iter().filter(|fp| filter(&fp.path)) {
        let original = if fp.new_file {
            String::new()
        } else {
            read(&fp.path)?.with_context(|| format!("{} not found", fp.path))?
        };
        let trailing_newline = original.ends_with('\n') || fp.new_file;
        let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
        let mut offset = 0isize;
        for (i, h) in fp.hunks.iter().enumerate() {
            if !apply_hunk(&mut lines, h, &mut offset) {
                failed.push(format!("{} (hunk {})", fp.path, i + 1));
            }
        }
        let mut text = lines.join("\n");
        if trailing_newline {
            text.push('\n');
        }
        out.push((fp.path, text));
    }
    if !failed.is_empty() {
        bail!("could not apply: {}", failed.join(", "));
    }
    Ok(out)
}

/// Apply `diff` to files on disk; `map` translates patch paths to file
/// system paths.
pub fn apply(diff: &str, map: impl Fn(&str) -> std::path::PathBuf) -> anyhow::Result<()> {
    let files = apply_in_memory(diff, |p| {
        let path = map(p);
        Ok(std::fs::read_to_string(&path).ok())
    })?;
    for (p, text) in files {
        let path = map(&p);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_with_offset_and_fuzz() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        // The original file the diff was made against had no `extra` lines.
        std::fs::write(
            &f,
            "extra\nextra\nfn a() {\n    one();\n    two();\n}\nCHANGED CONTEXT\n",
        )
        .unwrap();
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1,5 +1,6 @@\n fn a() {\n     one();\n+    inserted();\n     two();\n }\n context\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,2 @@\n+pub fn x() {}\n+// new\n";
        apply(diff, |p| dir.path().join(p)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "extra\nextra\nfn a() {\n    one();\n    inserted();\n    two();\n}\nCHANGED CONTEXT\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.rs")).unwrap(),
            "pub fn x() {}\n// new\n"
        );
    }

    #[test]
    fn reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "something else\n").unwrap();
        let diff = "--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line\n";
        assert!(apply(diff, |p| dir.path().join(p)).is_err());
    }
}

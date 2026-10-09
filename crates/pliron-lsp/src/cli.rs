//! Command-line tools: `pliron-lsp check` (lint IR files, e.g. in CI) and
//! `pliron-lsp fmt` (format them).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use crossbeam_channel::Receiver;
use pliron_ir_syntax::{LineIndex, Severity};
use pliron_lsp_protocol::{
    AnalyzeParams, AnalyzeResult, HookSeverity, Request, RequestBody, Response, ResponseBody,
    VerifyMode, decode_payload, encode_line, text_hash,
};

use crate::engine::find_reference_engine;
use crate::exact::Exact;
use crate::projects::{self, Outcome};

/// A synchronous connection to an engine process.
struct SyncEngine {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Response>,
    next_id: u64,
}

impl SyncEngine {
    fn spawn(exe: &Path) -> anyhow::Result<SyncEngine> {
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting engine {}", exe.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(p) = decode_payload(&line)
                    && let Ok(r) = serde_json::from_str::<Response>(p)
                {
                    let _ = tx.send(r);
                }
            }
        });
        Ok(SyncEngine {
            child,
            stdin,
            rx,
            next_id: 1,
        })
    }

    fn analyze(
        &mut self,
        text: &str,
        round_trip: bool,
        timeout: Duration,
    ) -> anyhow::Result<AnalyzeResult> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            id,
            body: RequestBody::Analyze(AnalyzeParams {
                text_hash: text_hash(text),
                text: text.to_string(),
                verify: VerifyMode::All,
                want_model: true,
                max_attr_len: 200,
                round_trip,
            }),
        };
        self.stdin.write_all(encode_line(&req).as_bytes())?;
        self.stdin.flush()?;
        loop {
            let r = self
                .rx
                .recv_timeout(timeout)
                .context("the engine did not answer (crashed or timed out)")?;
            if r.id == id {
                return match r.body {
                    ResponseBody::Analyze(a) => Ok(a),
                    ResponseBody::Error { message } => bail!(message),
                    other => bail!("unexpected engine response {other:?}"),
                };
            }
        }
    }
}

impl Drop for SyncEngine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct Finding {
    file: String,
    line: u32,
    column: u32,
    severity: &'static str,
    source: String,
    message: String,
}

fn collect_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            out.extend(crate::workspace::find_ir_files(std::slice::from_ref(p)));
        } else {
            out.push(p.clone());
        }
    }
    out
}

/// `pliron-lsp check [--no-engine] [--no-bundles] [--engine <exe>]
/// [--roundtrip] [--format human|json] [--deny-warnings] [paths...]`
pub fn check(args: &[String]) -> anyhow::Result<i32> {
    let mut paths = Vec::new();
    let mut no_engine = false;
    let mut no_bundles = false;
    let mut engine_override = None;
    let mut json = false;
    let mut deny_warnings = false;
    let mut round_trip = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-engine" => no_engine = true,
            "--no-bundles" => no_bundles = true,
            "--engine" => engine_override = it.next().map(PathBuf::from),
            "--format" => json = it.next().is_some_and(|f| f == "json"),
            "--deny-warnings" => deny_warnings = true,
            "--roundtrip" => round_trip = true,
            "-h" | "--help" => {
                println!(
                    "usage: pliron-lsp check [--no-engine] [--no-bundles] [--engine <exe>] [--roundtrip] [--format human|json] [--deny-warnings] [paths...]\n\n  --roundtrip      also report operations whose printed form does not parse\n                   back to the same IR (bugs in a dialect's printer or parser)"
                );
                return Ok(0);
            }
            p => paths.push(PathBuf::from(p)),
        }
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    let files = collect_files(&paths);
    if files.is_empty() {
        eprintln!("no .pliron / .plir files found");
        return Ok(0);
    }

    // Engine per file: the project's dialect engine, else the reference one.
    let reference = engine_override.clone().or_else(find_reference_engine);
    let mut project_engines: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut engines: HashMap<PathBuf, SyncEngine> = HashMap::new();
    let mut findings = Vec::new();

    for file in &files {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let exe = if no_engine {
            None
        } else if engine_override.is_some() || no_bundles {
            reference.clone()
        } else {
            match projects::workspace_root_of(&crate::canonicalize(file)?) {
                None => reference.clone(),
                Some(root) => project_engines
                    .entry(root.clone())
                    .or_insert_with(|| {
                        eprintln!("preparing the dialect engine of {}...", root.display());
                        match projects::run(&root, &|m| eprintln!("  {m}"), &|_, _| {}) {
                            Outcome::Engine { exe, .. } => Some(exe),
                            Outcome::NoDialects => reference.clone(),
                            Outcome::Unsupported(r) => {
                                eprintln!("  {r}; checking syntax only");
                                None
                            }
                            Outcome::Failed(e) => {
                                eprintln!("  {e}\n  checking syntax only");
                                None
                            }
                        }
                    })
                    .clone(),
            }
        };
        let name = file.display().to_string();
        let li = LineIndex::new(&text);
        let analysis = match &exe {
            Some(exe) => {
                if !engines.contains_key(exe) {
                    engines.insert(exe.clone(), SyncEngine::spawn(exe)?);
                }
                match engines.get_mut(exe).unwrap().analyze(
                    &text,
                    round_trip,
                    Duration::from_secs(60),
                ) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        engines.remove(exe);
                        findings.push(Finding {
                            file: name.clone(),
                            line: 1,
                            column: 1,
                            severity: "error",
                            source: "pliron-lsp".into(),
                            message: format!("analysis failed: {e:#}"),
                        });
                        continue;
                    }
                }
            }
            None => None,
        };
        match analysis {
            Some(result) => {
                let x = Exact::new(result, &text, &li);
                for d in &x.diagnostics {
                    let (line, column) = li.pliron_position(d.range.0, &text);
                    findings.push(Finding {
                        file: name.clone(),
                        line,
                        column,
                        severity: match d.severity {
                            HookSeverity::Error => "error",
                            HookSeverity::Warning => "warning",
                            HookSeverity::Info => "info",
                            HookSeverity::Hint => "hint",
                        },
                        source: d
                            .source
                            .clone()
                            .unwrap_or_else(|| crate::features::phase_name(d.phase).into()),
                        message: d.message.clone(),
                    });
                }
            }
            None => {
                let a = pliron_ir_syntax::analyze(&text, &Default::default());
                for d in &a.diagnostics {
                    let (line, column) = li.pliron_position(d.start, &text);
                    findings.push(Finding {
                        file: name.clone(),
                        line,
                        column,
                        severity: match d.severity {
                            Severity::Error => "error",
                            Severity::Warning => "warning",
                            Severity::Info => "info",
                        },
                        source: "syntax".into(),
                        message: d.message.clone(),
                    });
                }
            }
        }
    }

    let errors = findings.iter().filter(|f| f.severity == "error").count();
    let warnings = findings.iter().filter(|f| f.severity == "warning").count();
    if json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else {
        for f in &findings {
            println!(
                "{}:{}:{}: {}[{}]: {}",
                f.file,
                f.line,
                f.column,
                f.severity,
                f.source,
                f.message.replace('\n', "\n    ")
            );
        }
        eprintln!(
            "checked {} file(s): {errors} error(s), {warnings} warning(s)",
            files.len()
        );
    }
    Ok(i32::from(errors > 0 || (deny_warnings && warnings > 0)))
}

/// `pliron-lsp fmt [--check] [--indent N] [paths...]`
pub fn fmt(args: &[String]) -> anyhow::Result<i32> {
    let mut paths = Vec::new();
    let mut check = false;
    let mut indent = 2usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check = true,
            "--indent" => indent = it.next().and_then(|n| n.parse().ok()).unwrap_or(2),
            "-h" | "--help" => {
                println!("usage: pliron-lsp fmt [--check] [--indent N] [paths...]");
                return Ok(0);
            }
            p => paths.push(PathBuf::from(p)),
        }
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    let unit = " ".repeat(indent);
    let mut unformatted = 0;
    for file in collect_files(&paths) {
        let text = std::fs::read_to_string(&file)?;
        let Some(out) = crate::features::formatting::format_text(&text, &unit) else {
            eprintln!(
                "{}: not formatted (unbalanced brackets or strings)",
                file.display()
            );
            continue;
        };
        if out != text {
            unformatted += 1;
            if check {
                println!("{}: needs formatting", file.display());
            } else {
                std::fs::write(&file, out)?;
                println!("formatted {}", file.display());
            }
        }
    }
    Ok(i32::from(check && unformatted > 0))
}

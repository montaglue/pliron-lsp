//! Engine builds never outlive the server: a clean shutdown and SIGTERM
//! stop cargo and everything it started, and when the server is killed,
//! the kernel stops cargo. Runs the real `pliron-lsp` binary on the toy
//! fixture, with a `cargo` whose `build` only sleeps.
#![cfg(target_os = "linux")]

use std::io::{BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId};
use serde_json::json;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let dest = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
        }
    }
}

/// The fixture refers to pliron-lsp-api by a relative path; make it
/// absolute in the copy.
fn fix_api_path(dir: &Path) {
    let m = dir.join("toy-dialect/Cargo.toml");
    let api =
        pliron_lsp::canonicalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../pliron-lsp-api"))
            .unwrap();
    let text = std::fs::read_to_string(&m).unwrap();
    let text = text.replace("../../../../../pliron-lsp-api", &api.display().to_string());
    std::fs::write(&m, text).unwrap();
}

/// `cargo build` sleeps in a child (like rustc) and records both pids;
/// everything else is the real cargo.
const FAKE_CARGO: &str = r#"#!/bin/sh
if [ "$1" = build ]; then
  sleep 600 &
  echo "$$ $!" > "$PIDS.tmp" && mv "$PIDS.tmp" "$PIDS"
  wait
  exit 1
fi
exec "$REAL_CARGO" "$@"
"#;

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    _rx: Receiver<Message>,
    /// cargo (the fake script) and its sleeping child.
    cargo: u32,
    rustc: u32,
    _tmp: tempfile::TempDir,
}

fn send(stdin: &mut ChildStdin, msg: Message) {
    msg.write(stdin).unwrap();
    stdin.flush().unwrap();
}

/// Start the server, open the toy sample, and wait until its engine build
/// runs.
fn start_building() -> Server {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("toy");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toy"),
        &dir,
    );
    fix_api_path(&dir);
    let cargo = tmp.path().join("cargo");
    std::fs::write(&cargo, FAKE_CARGO).unwrap();
    std::fs::set_permissions(&cargo, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let pids = tmp.path().join("pids");

    let mut child = Command::new(env!("CARGO_BIN_EXE_pliron-lsp"))
        .env("CARGO", &cargo)
        .env("REAL_CARGO", env!("CARGO"))
        .env("PIDS", &pids)
        .env("PLIRON_LSP_CACHE_DIR", tmp.path().join("cache"))
        .env_remove("CARGO_TARGET_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // Drain the server's output, so that it never blocks on a full pipe.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        while let Ok(Some(m)) = Message::read(&mut r) {
            let _ = tx.send(m);
        }
    });

    send(
        &mut stdin,
        Message::Request(Request::new(
            RequestId::from(1),
            "initialize".into(),
            json!({ "capabilities": {} }),
        )),
    );
    send(
        &mut stdin,
        Message::Notification(Notification::new("initialized".into(), json!({}))),
    );
    let file = dir.join("sample.pliron");
    let uri = lsp_types::Url::from_file_path(&file).unwrap().to_string();
    send(
        &mut stdin,
        Message::Notification(Notification::new(
            "textDocument/didOpen".into(),
            json!({ "textDocument": {
                "uri": uri, "languageId": "pliron", "version": 1,
                "text": std::fs::read_to_string(&file).unwrap(),
            } }),
        )),
    );

    let text = wait_for(
        Duration::from_secs(300),
        "the engine build to start",
        || std::fs::read_to_string(&pids).ok(),
    );
    let mut it = text.split_whitespace().map(|p| p.parse().unwrap());
    let (cargo, rustc) = (it.next().unwrap(), it.next().unwrap());
    assert!(alive(cargo) && alive(rustc));
    // The build leads its own process group, and it and its children are
    // batch work (`policy`, field 41 of stat: 3 = SCHED_BATCH).
    assert_eq!(stat(cargo).unwrap()[2], cargo.to_string());
    for pid in [cargo, rustc] {
        assert_eq!(stat(pid).unwrap()[38], libc::SCHED_BATCH.to_string());
    }
    Server {
        child,
        stdin: Some(stdin),
        _rx: rx,
        cargo,
        rustc,
        _tmp: tmp,
    }
}

fn wait_for<T>(timeout: Duration, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Fields of `/proc/<pid>/stat` after the command name: state, ppid, pgrp, ...
fn stat(pid: u32) -> Option<Vec<String>> {
    let s = std::fs::read_to_string(PathBuf::from(format!("/proc/{pid}/stat"))).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

fn alive(pid: u32) -> bool {
    stat(pid).is_some_and(|f| f[0] != "Z" && f[0] != "X")
}

fn wait_dead(pid: u32, what: &str) {
    wait_for(Duration::from_secs(10), what, || {
        (!alive(pid)).then_some(())
    });
}

fn kill(pid: i32, sig: i32) {
    // SAFETY: plain syscall.
    unsafe {
        libc::kill(pid, sig);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Whatever a failing test left behind.
        kill(-(self.cargo as i32), libc::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn shutdown_stops_the_engine_build() {
    let mut s = start_building();
    let stdin = s.stdin.as_mut().unwrap();
    send(
        stdin,
        Message::Request(Request::new(
            RequestId::from(2),
            "shutdown".into(),
            json!(null),
        )),
    );
    send(
        stdin,
        Message::Notification(Notification::new("exit".into(), json!(null))),
    );
    drop(s.stdin.take());
    wait_for(Duration::from_secs(10), "the server to exit", || {
        s.child.try_wait().unwrap()
    });
    wait_dead(s.cargo, "cargo to stop");
    wait_dead(s.rustc, "cargo's child to stop");
}

#[test]
fn sigterm_stops_the_engine_build() {
    let mut s = start_building();
    kill(s.child.id() as i32, libc::SIGTERM);
    let status = wait_for(Duration::from_secs(10), "the server to exit", || {
        s.child.try_wait().unwrap()
    });
    // It still dies of the signal.
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGTERM)
    );
    wait_dead(s.cargo, "cargo to stop");
    wait_dead(s.rustc, "cargo's child to stop");
}

#[test]
fn a_killed_server_takes_cargo_with_it() {
    let mut s = start_building();
    s.child.kill().unwrap();
    s.child.wait().unwrap();
    // PR_SET_PDEATHSIG; cargo's own children are not covered.
    wait_dead(s.cargo, "cargo to stop");
}

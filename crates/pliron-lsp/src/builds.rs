//! Engine build processes: they never outlive the server (or `check`), and
//! one project's engine is built by one process at a time.
//!
//! - Every build runs in its own process group (Unix), so stopping it stops
//!   cargo and all the `rustc` processes it started.
//! - Running builds are registered; [`cancel_all`] (on exit, and from the
//!   SIGTERM/SIGINT/SIGHUP handlers of [`install_signal_handlers`]) kills
//!   them and refuses new ones, so a build that fails because it was
//!   killed is not retried.
//! - On Linux, cargo also gets SIGTERM from the kernel when the thread that
//!   started it dies (`PR_SET_PDEATHSIG`), which covers a server that is
//!   killed with SIGKILL or crashes. `rustc` processes cargo already started
//!   then finish their current crate.
//! - On Linux, builds run with `SCHED_BATCH`: the same CPU share as other
//!   builds (rust-analyzer's), but they do not preempt interactive work.
//! - [`lock_project`] serializes the builds of one project's engine between
//!   processes (several editor windows, `pliron-lsp check`).

use std::fs::File;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

/// Set once the process is shutting down: no new builds.
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Process (group) ids of running builds; 0 is a free slot. Atomics, so
/// that the signal handler can read them.
static RUNNING: [AtomicU32; 32] = [const { AtomicU32::new(0) }; 32];

/// Whether [`cancel_all`] was called.
pub fn cancelled() -> bool {
    CANCELLED.load(SeqCst)
}

/// A running build. Dropping it before [`Build::wait`] kills it.
pub struct Build {
    child: Child,
    slot: Option<usize>,
    waited: bool,
}

impl std::ops::Deref for Build {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl std::ops::DerefMut for Build {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Build {
    /// Wait for the build to exit.
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait();
        self.waited = true;
        self.unregister();
        status
    }

    fn unregister(&mut self) {
        if let Some(i) = self.slot.take() {
            RUNNING[i].store(0, SeqCst);
        }
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        if !self.waited {
            kill_tree(self.child.id());
            let _ = self.child.wait();
        }
        self.unregister();
    }
}

/// Start a build command (see the module docs).
pub fn spawn(cmd: &mut Command) -> std::io::Result<Build> {
    if cancelled() {
        return Err(std::io::Error::other("pliron-lsp is shutting down"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt as _;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have died before prctl.
                if libc::getppid() != parent {
                    return Err(std::io::Error::other("parent exited"));
                }
                // Inherited by everything cargo starts. Best effort.
                let param = libc::sched_param { sched_priority: 0 };
                libc::sched_setscheduler(0, libc::SCHED_BATCH, &param);
                Ok(())
            });
        }
    }
    let child = cmd.spawn()?;
    let id = child.id();
    let slot =
        (0..RUNNING.len()).find(|&i| RUNNING[i].compare_exchange(0, id, SeqCst, SeqCst).is_ok());
    let build = Build {
        child,
        slot,
        waited: false,
    };
    // `cancel_all` may have run between the check above and registering.
    if cancelled() {
        drop(build);
        return Err(std::io::Error::other("pliron-lsp is shutting down"));
    }
    Ok(build)
}

/// Kill all running builds and refuse new ones (the process is exiting).
pub fn cancel_all() {
    CANCELLED.store(true, SeqCst);
    for slot in &RUNNING {
        let id = slot.load(SeqCst);
        if id != 0 {
            kill_tree(id);
        }
    }
}

/// Kill a build: its process group on Unix, its process tree on Windows.
/// Async-signal-safe on Unix.
fn kill_tree(id: u32) {
    #[cfg(unix)]
    // SAFETY: plain syscall. Builds lead their own process group.
    unsafe {
        libc::kill(-(id as libc::pid_t), libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &id.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Kill the running builds on SIGTERM, SIGINT and SIGHUP, then die of the
/// signal as before. Signals that are ignored (e.g. under `nohup`) stay
/// ignored.
pub fn install_signal_handlers() {
    #[cfg(unix)]
    {
        extern "C" fn on_signal(sig: libc::c_int) {
            CANCELLED.store(true, SeqCst);
            for slot in &RUNNING {
                let id = slot.load(SeqCst);
                if id != 0 {
                    kill_tree(id);
                }
            }
            // SAFETY: async-signal-safe calls.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
        }
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // SAFETY: the handler only uses atomics and async-signal-safe calls.
            unsafe {
                let handler: extern "C" fn(libc::c_int) = on_signal;
                let prev = libc::signal(sig, handler as libc::sighandler_t);
                if prev == libc::SIG_IGN {
                    libc::signal(sig, libc::SIG_IGN);
                }
            }
        }
    }
}

/// Take the build lock of the project whose pliron-lsp state lives in
/// `state_dir`, waiting for another process that holds it. `on_wait` is
/// called before waiting, with that process's id if known. The lock is
/// released when the file is dropped.
pub fn lock_project(state_dir: &Path, on_wait: &dyn Fn(Option<u32>)) -> std::io::Result<File> {
    std::fs::create_dir_all(state_dir)?;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join("build.lock"))?;
    match f.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut holder = String::new();
            let _ = f.read_to_string(&mut holder);
            on_wait(holder.trim().parse().ok());
            f.lock()?;
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    f.set_len(0)?;
    f.rewind()?;
    write!(f, "{}", std::process::id())?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_waits_for_the_first() {
        let tmp = tempfile::tempdir().unwrap();
        let first = lock_project(tmp.path(), &|_| panic!("the first lock must not wait")).unwrap();
        let dir = tmp.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let waited = std::cell::Cell::new(None);
            let _second = lock_project(&dir, &|pid| waited.set(Some(pid))).unwrap();
            tx.send(waited.get()).unwrap();
        });
        // Still waiting while the first lock is held.
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err()
        );
        drop(first);
        let waited = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(waited, Some(Some(std::process::id())));
        waiter.join().unwrap();
    }
}

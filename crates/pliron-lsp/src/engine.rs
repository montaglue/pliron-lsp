//! Engine processes: spawning, request/response plumbing, timeouts and
//! crash handling.
//!
//! An engine is a separate process (it runs arbitrary dialect parser code,
//! which may panic, abort, loop or overflow its stack). The frontend keeps
//! at most one request in flight per engine; newer text for a document
//! replaces older pending text.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use lsp_types::Url;
use pliron_lsp_protocol::{
    AnalyzeParams, AnalyzeResult, EngineInfo, PROTOCOL_VERSION, ProbeParams, Request, RequestBody,
    Response, ResponseBody, VerifyMode, decode_payload, encode_line,
};

/// Messages from engine I/O threads to the main loop.
#[derive(Debug)]
pub enum EngineEvent {
    Response {
        key: String,
        generation: u64,
        response: Box<Response>,
    },
    Exited {
        key: String,
        generation: u64,
    },
}

impl EngineEvent {
    /// The engine this event belongs to.
    pub fn key(&self) -> &str {
        match self {
            EngineEvent::Response { key, .. } | EngineEvent::Exited { key, .. } => key,
        }
    }
}

/// What a finished analysis is for.
#[derive(Debug)]
pub enum Finished {
    Hello(EngineInfo),
    Analysis {
        uri: Url,
        result: AnalyzeResult,
    },
    Probe(ProbeParams),
    /// The engine died or timed out while analyzing `uri`.
    Failed {
        uri: Option<Url>,
        message: String,
    },
}

#[derive(Clone, Debug)]
enum Pending {
    Analyze { uri: Url, text: String, hash: u64 },
    Probe(ProbeParams),
}

struct InFlight {
    id: u64,
    started: Instant,
    what: Pending,
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

/// A (re)startable engine process.
pub struct Engine {
    pub exe: PathBuf,
    /// Identifies the engine (project root or "reference").
    pub key: String,
    pub label: String,
    process: Option<Process>,
    /// Incremented on every (re)start; events of older processes are ignored.
    generation: u64,
    events: Sender<EngineEvent>,
    next_id: u64,
    in_flight: Option<InFlight>,
    queue: VecDeque<Pending>,
    pub info: Option<EngineInfo>,
    /// Text hashes that crashed or hung the engine.
    poisoned: HashSet<u64>,
    pub timeout: Duration,
    restarts: u32,
    pub last_error: Option<String>,
}

impl Engine {
    pub fn new(exe: PathBuf, key: String, label: String, events: Sender<EngineEvent>) -> Engine {
        Engine {
            exe,
            key,
            label,
            process: None,
            generation: 0,
            events,
            next_id: 1,
            in_flight: None,
            queue: VecDeque::new(),
            info: None,
            poisoned: HashSet::new(),
            timeout: Duration::from_secs(10),
            restarts: 0,
            last_error: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.process.is_some()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn start(&mut self) -> anyhow::Result<()> {
        self.generation += 1;
        let generation = self.generation;
        let mut child = Command::new(&self.exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start engine {}: {e}", self.exe.display()))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let events = self.events.clone();
        let key = self.key.clone();
        std::thread::Builder::new()
            .name("engine-stdout".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let Some(payload) = decode_payload(&line) else {
                        eprintln!("[engine stdout] {}", line.trim_end());
                        continue;
                    };
                    match serde_json::from_str::<Response>(payload) {
                        Ok(response) => {
                            let _ = events.send(EngineEvent::Response {
                                key: key.clone(),
                                generation,
                                response: Box::new(response),
                            });
                        }
                        Err(e) => eprintln!("[engine] bad response: {e}"),
                    }
                }
                let _ = events.send(EngineEvent::Exited { key, generation });
            })?;
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let tail = stderr_tail.clone();
        std::thread::Builder::new()
            .name("engine-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("[engine stderr] {line}");
                    let mut t = tail.lock().unwrap();
                    t.push_back(line);
                    if t.len() > 20 {
                        t.pop_front();
                    }
                }
            })?;
        self.process = Some(Process {
            child,
            stdin,
            stderr_tail,
        });
        self.in_flight = None;
        // Handshake first.
        self.send(Pending::Probe(ProbeParams::default()), true)?;
        Ok(())
    }

    fn stderr_tail(&self) -> String {
        self.process
            .as_ref()
            .map(|p| {
                let t = p.stderr_tail.lock().unwrap();
                t.iter().cloned().collect::<Vec<_>>().join("\n")
            })
            .unwrap_or_default()
    }

    fn kill(&mut self) {
        if let Some(mut p) = self.process.take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        self.in_flight = None;
    }

    /// Write a request to the engine. `hello` sends the handshake instead of
    /// `what`.
    fn send(&mut self, what: Pending, hello: bool) -> anyhow::Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let body = if hello {
            RequestBody::Hello {
                protocol: PROTOCOL_VERSION,
            }
        } else {
            match &what {
                Pending::Analyze { text, hash, .. } => RequestBody::Analyze(AnalyzeParams {
                    text_hash: *hash,
                    text: text.clone(),
                    verify: VerifyMode::All,
                    want_model: true,
                    max_attr_len: 200,
                }),
                Pending::Probe(p) => RequestBody::Probe(p.clone()),
            }
        };
        let proc = self
            .process
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("engine not running"))?;
        proc.stdin
            .write_all(encode_line(&Request { id, body }).as_bytes())?;
        proc.stdin.flush()?;
        self.in_flight = Some(InFlight {
            id,
            started: Instant::now(),
            what: if hello {
                Pending::Probe(ProbeParams::default())
            } else {
                what
            },
        });
        Ok(())
    }

    /// Queue an analysis of `text` for `uri` (replacing older text for the
    /// same document).
    pub fn analyze(&mut self, uri: Url, text: String, hash: u64) {
        if self.poisoned.contains(&hash) {
            return;
        }
        self.queue
            .retain(|p| !matches!(p, Pending::Analyze { uri: u, .. } if *u == uri));
        self.queue.push_back(Pending::Analyze { uri, text, hash });
        self.pump();
    }

    pub fn probe(&mut self, params: ProbeParams) {
        self.queue.push_back(Pending::Probe(params));
        self.pump();
    }

    /// Start the process if needed and send the next queued request.
    fn pump(&mut self) {
        if self.process.is_none() {
            if self.restarts > 5 {
                return;
            }
            if let Err(e) = self.start() {
                self.last_error = Some(e.to_string());
                self.restarts += 1;
                self.queue.clear();
                return;
            }
        }
        if self.in_flight.is_some() {
            return;
        }
        if let Some(next) = self.queue.pop_front()
            && let Err(e) = self.send(next, false)
        {
            self.last_error = Some(e.to_string());
            self.kill();
        }
    }

    /// Handle an event from this engine's I/O threads.
    pub fn on_event(&mut self, ev: EngineEvent) -> Vec<Finished> {
        let mut out = Vec::new();
        match ev {
            EngineEvent::Response {
                generation,
                response,
                ..
            } if generation == self.generation => {
                let Some(inflight) = self.in_flight.take() else {
                    return out;
                };
                if inflight.id != response.id {
                    self.in_flight = Some(inflight);
                    return out;
                }
                match response.body {
                    ResponseBody::Hello(info) => {
                        self.restarts = 0;
                        if let Some(e) = &info.context_error {
                            self.last_error = Some(format!("Context::new() failed: {e}"));
                        }
                        self.info = Some(info.clone());
                        out.push(Finished::Hello(info));
                    }
                    ResponseBody::Analyze(result) => {
                        if let Pending::Analyze { uri, .. } = inflight.what {
                            out.push(Finished::Analysis { uri, result });
                        }
                    }
                    ResponseBody::Probe(p) => out.push(Finished::Probe(p)),
                    ResponseBody::Error { message } => {
                        out.push(Finished::Failed { uri: None, message })
                    }
                    ResponseBody::Shutdown => {}
                }
            }
            EngineEvent::Exited { generation, .. } if generation == self.generation => {
                let tail = self.stderr_tail();
                let uri = match self.in_flight.take().map(|i| i.what) {
                    Some(Pending::Analyze { uri, hash, .. }) => {
                        self.poisoned.insert(hash);
                        Some(uri)
                    }
                    _ => None,
                };
                self.kill();
                self.restarts += 1;
                out.push(Finished::Failed {
                    uri,
                    message: format!("the dialect engine crashed\n{tail}"),
                });
            }
            _ => {}
        }
        self.pump();
        out
    }

    /// Kill the engine if the current request takes too long.
    pub fn check_timeout(&mut self) -> Option<Finished> {
        let inflight = self.in_flight.as_ref()?;
        if inflight.started.elapsed() < self.timeout {
            return None;
        }
        let uri = match &inflight.what {
            Pending::Analyze { uri, hash, .. } => {
                self.poisoned.insert(*hash);
                Some(uri.clone())
            }
            _ => None,
        };
        let tail = self.stderr_tail();
        self.kill();
        self.pump();
        Some(Finished::Failed {
            uri,
            message: format!(
                "the dialect engine did not answer within {}s (a dialect parser may be looping)\n{tail}",
                self.timeout.as_secs()
            ),
        })
    }

    pub fn shutdown(&mut self) {
        if let Some(p) = self.process.as_mut() {
            let _ = p.stdin.write_all(
                encode_line(&Request {
                    id: 0,
                    body: RequestBody::Shutdown,
                })
                .as_bytes(),
            );
            let _ = p.stdin.flush();
        }
        if let Some(mut p) = self.process.take() {
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = p.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Find the reference engine binary (builtin + llvm) next to this
/// executable.
pub fn find_reference_engine() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLIRON_LSP_ENGINE") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = format!("pliron-lsp-engine-ref{}", std::env::consts::EXE_SUFFIX);
    [dir.join(&name), dir.join("..").join(&name)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Per-engine registry knowledge gathered from probes.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    pub ops: HashSet<String>,
    pub types: HashSet<String>,
    pub attrs: HashSet<String>,
}

/// Map of engine label -> registry.
pub type Registries = HashMap<String, Registry>;

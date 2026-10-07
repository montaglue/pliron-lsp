//! The LSP main loop.
//!
//! Every document is routed to a dialect engine:
//! * documents inside a cargo project that defines pliron dialects use that
//!   project's automatically built bundle engine;
//! * everything else uses the reference engine (builtin + llvm).
//!
//! Until an engine answers for the current text, the syntax layer serves
//! all requests.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument,
    Initialized, Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    Completion, DocumentHighlightRequest, DocumentSymbolRequest, FoldingRangeRequest,
    GotoDefinition, HoverRequest, InlayHintRefreshRequest, InlayHintRequest, PrepareRenameRequest,
    References, RegisterCapability, Rename, Request as _, SemanticTokensFullRequest,
    SemanticTokensRangeRequest, SemanticTokensRefresh,
};
use lsp_types::*;
use pliron_ir_syntax::{Encoding, Knowledge, lexer::is_identifier};
use serde::Serialize;
use serde_json::Value;

use crate::document::Document;
use crate::engine::{Engine, EngineEvent, Finished, find_reference_engine};
use crate::exact::Exact;
use crate::features::{self, entity::EntityKind};
use crate::index::DialectIndex;
use crate::projects::{self, JobEvent, Outcome};

const DEBOUNCE: Duration = Duration::from_millis(150);
const REBUILD_DEBOUNCE: Duration = Duration::from_millis(700);
const REFERENCE: &str = "reference";

/// Options accepted in `initializationOptions`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitOptions {
    /// Path to an engine binary to use for every document (disables
    /// automatic dialect bundles).
    pub engine_path: Option<PathBuf>,
    /// Disable dialect engines (syntax layer only).
    pub disable_engine: bool,
    /// Do not build dialect bundles for cargo projects.
    pub disable_bundles: bool,
}

/// `pliron/status` notification.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// Project root (or "reference").
    pub engine: Option<String>,
    /// "syntax-only" | "building" | "ready" | "error"
    pub state: &'static str,
    pub message: Option<String>,
}

#[derive(Debug)]
enum ProjectState {
    Building,
    Ready,
    /// No dialects: documents use the reference engine.
    NoDialects,
    Unsupported(String),
    Failed(String),
}

struct Project {
    state: ProjectState,
    watched: Vec<PathBuf>,
    building: bool,
    rebuild_due: Option<Instant>,
    rebuild_again: bool,
    /// Generated bundle package and the engine built from it.
    bundle_dir: Option<PathBuf>,
    engine_exe: Option<PathBuf>,
    description: Option<String>,
    /// Last build progress / status message.
    message: Option<String>,
    build_started: Option<Instant>,
    last_build: Option<Duration>,
}

impl Project {
    fn new() -> Project {
        Project {
            state: ProjectState::Building,
            watched: Vec::new(),
            building: true,
            rebuild_due: None,
            rebuild_again: false,
            bundle_dir: None,
            engine_exe: None,
            description: None,
            message: None,
            build_started: Some(Instant::now()),
            last_build: None,
        }
    }
}

/// Which engine serves a document.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Reference,
    Project(PathBuf),
}

pub struct Server {
    sender: Sender<Message>,
    docs: HashMap<Url, Document>,
    routes: HashMap<Url, Route>,
    enc: Encoding,
    knowledge: Knowledge,
    engines: HashMap<String, Engine>,
    engine_tx: Sender<EngineEvent>,
    engine_rx: Receiver<EngineEvent>,
    projects: HashMap<PathBuf, Project>,
    /// Directory -> workspace root cache.
    roots: HashMap<PathBuf, Option<PathBuf>>,
    job_tx: Sender<JobEvent>,
    job_rx: Receiver<JobEvent>,
    due: HashMap<Url, Instant>,
    semantic_refresh: bool,
    inlay_refresh: bool,
    watch_registration: bool,
    known_ops: BTreeSet<String>,
    next_id: i32,
    opts: InitOptions,
    reference_exe: Option<PathBuf>,
    /// Dialect source indexes by project root (or `projects::REFERENCE_KEY`),
    /// with the directories they cover.
    indexes: HashMap<PathBuf, (Arc<DialectIndex>, Vec<PathBuf>)>,
    reference_index_requested: bool,
    /// Does the client support `window/workDoneProgress`?
    work_done_progress: bool,
}

/// Parameters of the `pliron/*` document requests.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocParams {
    text_document: Option<TextDocumentIdentifier>,
}

pub fn capabilities(enc: Encoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(match enc {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
            Encoding::Utf32 => PositionEncodingKind::UTF32,
        }),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: features::semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(true),
                work_done_progress_options: Default::default(),
            },
        )),
        inlay_hint_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["^".into(), "@".into(), ".".into()]),
            ..CompletionOptions::default()
        }),
        ..ServerCapabilities::default()
    }
}

fn negotiate_encoding(params: &InitializeParams) -> Encoding {
    let offered = params
        .capabilities
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.clone())
        .unwrap_or_default();
    if offered.contains(&PositionEncodingKind::UTF8) {
        Encoding::Utf8
    } else if offered.contains(&PositionEncodingKind::UTF32) {
        Encoding::Utf32
    } else {
        Encoding::Utf16
    }
}

/// Run the server over an established connection (after `initialize`).
pub fn run(connection: Connection) -> anyhow::Result<()> {
    let (id, params) = connection.initialize_start()?;
    let init: InitializeParams = serde_json::from_value(params)?;
    let enc = negotiate_encoding(&init);
    let result = InitializeResult {
        capabilities: capabilities(enc),
        server_info: Some(ServerInfo {
            name: "pliron-lsp".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        }),
    };
    connection.initialize_finish(id, serde_json::to_value(result)?)?;

    let opts: InitOptions = init
        .initialization_options
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let ws = init.capabilities.workspace.as_ref();
    let semantic_refresh = ws
        .and_then(|w| w.semantic_tokens.as_ref())
        .and_then(|s| s.refresh_support)
        .unwrap_or(false);
    let inlay_refresh = ws
        .and_then(|w| w.inlay_hint.as_ref())
        .and_then(|s| s.refresh_support)
        .unwrap_or(false);
    let watch_registration = ws
        .and_then(|w| w.did_change_watched_files.as_ref())
        .and_then(|d| d.dynamic_registration)
        .unwrap_or(false);

    let (engine_tx, engine_rx) = crossbeam_channel::unbounded();
    let (job_tx, job_rx) = crossbeam_channel::unbounded();
    let reference_exe = if opts.disable_engine {
        None
    } else {
        opts.engine_path.clone().or_else(find_reference_engine)
    };
    let mut server = Server {
        sender: connection.sender.clone(),
        docs: HashMap::new(),
        routes: HashMap::new(),
        enc,
        knowledge: Knowledge::default(),
        engines: HashMap::new(),
        engine_tx,
        engine_rx,
        projects: HashMap::new(),
        roots: HashMap::new(),
        job_tx,
        job_rx,
        due: HashMap::new(),
        semantic_refresh,
        inlay_refresh,
        watch_registration,
        known_ops: BTreeSet::new(),
        next_id: 0,
        opts,
        reference_exe,
        indexes: HashMap::new(),
        reference_index_requested: false,
        work_done_progress: init
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false),
    };
    if server.reference_exe.is_none() && !server.opts.disable_engine {
        server.status(None, "syntax-only", Some("reference engine not found".into()));
    }
    server.main_loop(&connection)?;
    for e in server.engines.values_mut() {
        e.shutdown();
    }
    Ok(())
}

impl Server {
    fn main_loop(&mut self, conn: &Connection) -> anyhow::Result<()> {
        loop {
            let now = Instant::now();
            let next_due = self
                .due
                .values()
                .copied()
                .chain(self.projects.values().filter_map(|p| p.rebuild_due))
                .min();
            let timeout = next_due
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(Duration::from_millis(500))
                .min(Duration::from_millis(500));
            select! {
                recv(conn.receiver) -> msg => {
                    let Ok(msg) = msg else { return Ok(()) };
                    match msg {
                        Message::Request(req) => {
                            if conn.handle_shutdown(&req)? {
                                return Ok(());
                            }
                            self.on_request(req);
                        }
                        Message::Notification(n) => self.on_notification(n),
                        Message::Response(_) => {}
                    }
                }
                recv(self.engine_rx) -> ev => {
                    if let Ok(ev) = ev {
                        self.on_engine_event(ev);
                    }
                }
                recv(self.job_rx) -> ev => {
                    if let Ok(ev) = ev {
                        self.on_job_event(ev);
                    }
                }
                default(timeout) => {}
            }
            self.flush_due();
            self.flush_rebuilds();
            let finished: Vec<Finished> = self
                .engines
                .values_mut()
                .filter_map(|e| e.check_timeout())
                .collect();
            for f in finished {
                self.on_finished(f);
            }
        }
    }

    fn send(&self, msg: Message) {
        let _ = self.sender.send(msg);
    }

    fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        self.send(Message::Notification(Notification::new(
            N::METHOD.to_string(),
            params,
        )));
    }

    fn client_request<R: lsp_types::request::Request>(&mut self, params: R::Params) {
        self.next_id += 1;
        self.send(Message::Request(Request::new(
            RequestId::from(format!("pliron-{}", self.next_id)),
            R::METHOD.to_string(),
            params,
        )));
    }

    fn status(&self, engine: Option<String>, state: &'static str, message: Option<String>) {
        self.send(Message::Notification(Notification::new(
            "pliron/status".into(),
            Status {
                engine,
                state,
                message,
            },
        )));
    }

    fn log(&self, message: String) {
        self.notify::<lsp_types::notification::LogMessage>(LogMessageParams {
            typ: MessageType::LOG,
            message,
        });
    }

    // ----- routing ------------------------------------------------------

    /// Decide (and remember) which engine serves `uri`; may start building
    /// a project's bundle.
    fn route_for(&mut self, uri: &Url) -> Route {
        if let Some(r) = self.routes.get(uri) {
            return r.clone();
        }
        let route = self.compute_route(uri);
        self.routes.insert(uri.clone(), route.clone());
        route
    }

    fn compute_route(&mut self, uri: &Url) -> Route {
        if self.opts.engine_path.is_some() || self.opts.disable_bundles || self.opts.disable_engine
        {
            return Route::Reference;
        }
        let Ok(path) = uri.to_file_path() else {
            return Route::Reference;
        };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let root = match self.roots.get(&dir) {
            Some(r) => r.clone(),
            None => {
                let r = projects::workspace_root_of(&path);
                self.roots.insert(dir, r.clone());
                r
            }
        };
        let Some(root) = root else {
            return Route::Reference;
        };
        let root = root.canonicalize().unwrap_or(root);
        if !self.projects.contains_key(&root) {
            self.projects.insert(root.clone(), Project::new());
            self.status(
                Some(root.display().to_string()),
                "building",
                Some("preparing dialect engine".into()),
            );
            self.progress_begin(&root, "preparing dialect engine");
            projects::spawn(root.clone(), self.job_tx.clone());
        }
        Route::Project(root)
    }

    // ----- work-done progress for bundle builds -------------------------

    fn progress_token(root: &Path) -> NumberOrString {
        NumberOrString::String(format!("pliron/build:{}", root.display()))
    }

    fn progress_begin(&mut self, root: &Path, message: &str) {
        if !self.work_done_progress {
            return;
        }
        let token = Self::progress_token(root);
        self.client_request::<lsp_types::request::WorkDoneProgressCreate>(
            WorkDoneProgressCreateParams {
                token: token.clone(),
            },
        );
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.notify::<lsp_types::notification::Progress>(ProgressParams {
            token,
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: format!("pliron: dialect engine for {name}"),
                cancellable: Some(false),
                message: Some(message.into()),
                percentage: None,
            })),
        });
    }

    fn progress_report(&self, root: &Path, message: &str) {
        if !self.work_done_progress {
            return;
        }
        self.notify::<lsp_types::notification::Progress>(ProgressParams {
            token: Self::progress_token(root),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                WorkDoneProgressReport {
                    cancellable: Some(false),
                    message: Some(message.into()),
                    percentage: None,
                },
            )),
        });
    }

    fn progress_end(&self, root: &Path, message: &str) {
        if !self.work_done_progress {
            return;
        }
        self.notify::<lsp_types::notification::Progress>(ProgressParams {
            token: Self::progress_token(root),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: Some(message.into()),
            })),
        });
    }

    /// The engine key serving `uri` right now (None: syntax only).
    fn engine_key(&mut self, uri: &Url) -> Option<String> {
        match self.route_for(uri) {
            Route::Reference => self.reference_exe.as_ref().map(|_| REFERENCE.to_string()),
            Route::Project(root) => {
                let key = root.display().to_string();
                let p = self.projects.get(&root)?;
                match &p.state {
                    ProjectState::NoDialects => {
                        self.reference_exe.as_ref().map(|_| REFERENCE.to_string())
                    }
                    // While (re)building or after a failed rebuild, keep
                    // using the previous engine if there is one.
                    _ => self.engines.contains_key(&key).then_some(key),
                }
            }
        }
    }

    /// The dialect source index for a document.
    fn index_for(&mut self, uri: &Url) -> Option<Arc<DialectIndex>> {
        let key = match self.routes.get(uri).cloned() {
            Some(Route::Project(root)) => match self.projects.get(&root).map(|p| &p.state) {
                Some(ProjectState::NoDialects) => PathBuf::from(projects::REFERENCE_KEY),
                _ => root,
            },
            _ => PathBuf::from(projects::REFERENCE_KEY),
        };
        if key == Path::new(projects::REFERENCE_KEY) && !self.reference_index_requested {
            self.reference_index_requested = true;
            projects::spawn_reference_index(self.job_tx.clone());
        }
        self.indexes.get(&key).map(|(i, _)| i.clone())
    }

    fn engine_mut(&mut self, key: &str) -> Option<&mut Engine> {
        if key == REFERENCE && !self.engines.contains_key(REFERENCE) {
            let exe = self.reference_exe.clone()?;
            self.engines.insert(
                REFERENCE.into(),
                Engine::new(exe, REFERENCE.into(), "reference".into(), self.engine_tx.clone()),
            );
        }
        self.engines.get_mut(key)
    }

    /// A note explaining why `uri` has no engine (shown with syntax
    /// diagnostics).
    fn no_engine_note(&self, uri: &Url) -> Option<String> {
        match self.routes.get(uri)? {
            Route::Reference => None,
            Route::Project(root) => match &self.projects.get(root)?.state {
                ProjectState::Building => None,
                ProjectState::Unsupported(r) => Some(format!("pliron-lsp: {r}; syntax features only")),
                ProjectState::Failed(e) => Some(format!("pliron-lsp: {e}")),
                _ => None,
            },
        }
    }

    // ----- diagnostics & scheduling --------------------------------------

    fn publish(&self, uri: &Url, engine_expected: bool) {
        let Some(doc) = self.docs.get(uri) else {
            return;
        };
        if let Some(diags) = features::diagnostics(doc, self.enc, engine_expected) {
            self.notify::<PublishDiagnostics>(PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics: diags,
                version: Some(doc.version),
            });
        }
    }

    fn schedule(&mut self, uri: Url, delay: Duration) {
        self.due.insert(uri, Instant::now() + delay);
    }

    fn flush_due(&mut self) {
        let now = Instant::now();
        let ready: Vec<Url> = self
            .due
            .iter()
            .filter(|(_, d)| **d <= now)
            .map(|(u, _)| u.clone())
            .collect();
        for uri in ready {
            self.due.remove(&uri);
            if !self.docs.contains_key(&uri) {
                continue;
            }
            let key = self.engine_key(&uri);
            let note = self.no_engine_note(&uri);
            let (text, hash) = {
                let doc = &self.docs[&uri];
                (doc.text.clone(), doc.hash)
            };
            let engine_error = match key.as_deref().and_then(|k| self.engine_mut(k)) {
                Some(engine) => {
                    engine.analyze(uri.clone(), text, hash);
                    engine.last_error.clone()
                }
                None => None,
            };
            if key.is_none() || engine_error.is_some() {
                if let Some(doc) = self.docs.get_mut(&uri) {
                    doc.engine_note = engine_error.or(note);
                }
                self.publish(&uri, false);
            }
        }
    }

    fn reanalyze_route(&mut self, route: &Route) {
        let uris: Vec<Url> = self
            .routes
            .iter()
            .filter(|(_, r)| *r == route)
            .map(|(u, _)| u.clone())
            .collect();
        for u in uris {
            self.schedule(u, Duration::ZERO);
        }
    }

    // ----- engines -------------------------------------------------------

    fn on_engine_event(&mut self, ev: EngineEvent) {
        let key = ev.key().to_string();
        let Some(engine) = self.engines.get_mut(&key) else {
            return;
        };
        for f in engine.on_event(ev) {
            self.on_finished(f);
        }
    }

    fn on_finished(&mut self, f: Finished) {
        match f {
            Finished::Hello(info) => {
                let state = if info.context_error.is_some() { "error" } else { "ready" };
                self.status(
                    Some(info.bundle_id.clone()),
                    state,
                    Some(match &info.context_error {
                        Some(e) => format!("Context::new() failed: {e}"),
                        None => format!(
                            "{} registrations; {}",
                            info.registrations,
                            info.crates
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    }),
                );
            }
            Finished::Analysis { uri, result } => {
                let Some(doc) = self.docs.get_mut(&uri) else { return };
                if result.text_hash != doc.hash {
                    return; // stale; a newer analysis is queued
                }
                if let Some(m) = &result.model {
                    for op in &m.ops {
                        self.known_ops.insert(op.opid.clone());
                    }
                }
                let exact = Exact::new(result, &doc.text, &doc.line_index);
                doc.exact = Some(Arc::new(exact));
                doc.engine_note = None;
                self.publish(&uri, true);
                // Lets extensions refresh views of this document.
                self.send(Message::Notification(Notification::new(
                    "pliron/analysisUpdated".into(),
                    serde_json::json!({ "uri": uri }),
                )));
                if self.semantic_refresh {
                    self.client_request::<SemanticTokensRefresh>(());
                }
                if self.inlay_refresh {
                    self.client_request::<InlayHintRefreshRequest>(());
                }
            }
            Finished::Probe(_) => {}
            Finished::Failed { uri, message } => {
                self.log(message.clone());
                if let Some(uri) = uri
                    && let Some(doc) = self.docs.get_mut(&uri)
                {
                    doc.engine_note = Some(message);
                    self.publish(&uri, false);
                }
            }
        }
    }

    // ----- projects --------------------------------------------------------

    fn on_job_event(&mut self, ev: JobEvent) {
        match ev {
            JobEvent::Progress { root, message } => {
                self.progress_report(&root, &message);
                if let Some(p) = self.projects.get_mut(&root) {
                    p.message = Some(message.clone());
                }
                self.status(Some(root.display().to_string()), "building", Some(message));
            }
            JobEvent::Index { root, index, dirs } => {
                let dirs = dirs
                    .into_iter()
                    .map(|d| d.canonicalize().unwrap_or(d))
                    .collect();
                self.indexes.insert(root, (Arc::new(index), dirs));
            }
            JobEvent::Done { root, outcome } => {
                let key = root.display().to_string();
                let Some(p) = self.projects.get_mut(&root) else { return };
                p.building = false;
                p.last_build = p.build_started.take().map(|t| t.elapsed());
                let (state, status_msg) = match outcome {
                    Outcome::Engine {
                        exe,
                        bundle_dir,
                        watched,
                        description,
                    } => {
                        p.bundle_dir = Some(bundle_dir);
                        p.engine_exe = Some(exe.clone());
                        p.description = Some(description.clone());
                        p.watched = watched
                            .into_iter()
                            .map(|w| w.canonicalize().unwrap_or(w))
                            .collect();
                        if let Some(mut old) = self.engines.remove(&key) {
                            old.shutdown();
                        }
                        let label = root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| key.clone());
                        self.engines.insert(
                            key.clone(),
                            Engine::new(exe, key.clone(), label, self.engine_tx.clone()),
                        );
                        (ProjectState::Ready, ("ready", description))
                    }
                    Outcome::NoDialects => (
                        ProjectState::NoDialects,
                        ("ready", "no dialect crates: using the reference engine".into()),
                    ),
                    Outcome::Unsupported(r) => (ProjectState::Unsupported(r.clone()), ("error", r)),
                    Outcome::Failed(e) => {
                        self.log(e.clone());
                        let first = e.lines().take(3).collect::<Vec<_>>().join(" ");
                        (ProjectState::Failed(e), ("error", first))
                    }
                };
                let p = self.projects.get_mut(&root).unwrap();
                p.state = state;
                p.message = Some(status_msg.1.clone());
                if std::mem::take(&mut p.rebuild_again) {
                    p.rebuild_due = Some(Instant::now());
                }
                self.progress_end(&root, &status_msg.1);
                self.status(Some(key), status_msg.0, Some(status_msg.1));
                self.reanalyze_route(&Route::Project(root));
            }
        }
    }

    fn flush_rebuilds(&mut self) {
        let now = Instant::now();
        let due: Vec<PathBuf> = self
            .projects
            .iter()
            .filter(|(_, p)| p.rebuild_due.is_some_and(|d| d <= now))
            .map(|(r, _)| r.clone())
            .collect();
        for root in due {
            let p = self.projects.get_mut(&root).unwrap();
            p.rebuild_due = None;
            if p.building {
                p.rebuild_again = true;
                continue;
            }
            p.building = true;
            p.build_started = Some(Instant::now());
            if !self.engines.contains_key(&root.display().to_string()) {
                p.state = ProjectState::Building;
            }
            self.progress_begin(&root, "dialect sources changed: rebuilding");
            self.status(
                Some(root.display().to_string()),
                "building",
                Some("dialect sources changed: rebuilding".into()),
            );
            projects::spawn(root, self.job_tx.clone());
        }
    }

    fn register_watchers(&mut self) {
        if !self.watch_registration {
            return;
        }
        let watchers = ["**/*.rs", "**/Cargo.toml", "**/Cargo.lock"]
            .into_iter()
            .map(|g| FileSystemWatcher {
                glob_pattern: GlobPattern::String(g.into()),
                kind: None,
            })
            .collect();
        let opts = DidChangeWatchedFilesRegistrationOptions { watchers };
        self.client_request::<RegisterCapability>(RegistrationParams {
            registrations: vec![Registration {
                id: "pliron-lsp-watch".into(),
                method: DidChangeWatchedFiles::METHOD.into(),
                register_options: serde_json::to_value(opts).ok(),
            }],
        });
    }

    /// A file changed on disk: rebuild the bundles that depend on it.
    fn on_file_changed(&mut self, path: &Path) {
        // Compare canonical paths (cargo reports canonical ones, editors may
        // not, e.g. /var vs /private/var on macOS).
        let canonical = path
            .canonicalize()
            .or_else(|_| {
                path.parent()
                    .map(|d| d.canonicalize().map(|d| d.join(path.file_name().unwrap_or_default())))
                    .unwrap_or_else(|| Ok(path.to_path_buf()))
            })
            .unwrap_or_else(|_| path.to_path_buf());
        let path = canonical.as_path();
        if path.extension().is_some_and(|x| x == "rs") {
            for (index, dirs) in self.indexes.values_mut() {
                if dirs.iter().any(|d| path.starts_with(d)) {
                    Arc::make_mut(index).refresh_file(path);
                }
            }
        }
        for p in self.projects.values_mut() {
            if p.watched.iter().any(|w| path.starts_with(w)) {
                p.rebuild_due = Some(Instant::now() + REBUILD_DEBOUNCE);
            }
        }
    }

    // ----- notifications -----------------------------------------------------

    fn on_notification(&mut self, n: Notification) {
        match n.method.as_str() {
            Initialized::METHOD => self.register_watchers(),
            DidOpenTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) {
                    let doc = Document::new(
                        p.text_document.text,
                        p.text_document.version,
                        &self.knowledge,
                    );
                    let uri = p.text_document.uri;
                    self.docs.insert(uri.clone(), doc);
                    self.route_for(&uri);
                    self.index_for(&uri);
                    self.schedule(uri, Duration::ZERO);
                }
            }
            DidChangeTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params) {
                    let uri = p.text_document.uri;
                    if let Some(doc) = self.docs.get_mut(&uri) {
                        doc.apply_changes(
                            p.content_changes,
                            p.text_document.version,
                            self.enc,
                            &self.knowledge,
                        );
                        doc.engine_note = None;
                    }
                    self.schedule(uri, DEBOUNCE);
                }
            }
            DidCloseTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) {
                    self.docs.remove(&p.text_document.uri);
                    self.due.remove(&p.text_document.uri);
                    self.routes.remove(&p.text_document.uri);
                    self.notify::<PublishDiagnostics>(PublishDiagnosticsParams {
                        uri: p.text_document.uri,
                        diagnostics: Vec::new(),
                        version: None,
                    });
                }
            }
            DidChangeWatchedFiles::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeWatchedFilesParams>(n.params) {
                    for change in p.changes {
                        if let Ok(path) = change.uri.to_file_path() {
                            self.on_file_changed(&path);
                        }
                    }
                }
            }
            "pliron/rebuild" => {
                let roots: Vec<PathBuf> = self.projects.keys().cloned().collect();
                for r in roots {
                    if let Some(p) = self.projects.get_mut(&r) {
                        p.rebuild_due = Some(Instant::now());
                    }
                }
            }
            _ => {}
        }
    }

    // ----- requests ------------------------------------------------------------

    fn reply(&self, id: RequestId, result: impl Serialize) {
        self.send(Message::Response(Response::new_ok(id, result)));
    }

    fn reply_err(&self, id: RequestId, message: String) {
        self.send(Message::Response(Response::new_err(
            id,
            lsp_server::ErrorCode::InvalidParams as i32,
            message,
        )));
    }

    fn on_request(&mut self, req: Request) {
        let id = req.id.clone();
        let result = self.handle_request(req);
        match result {
            Ok(v) => self.reply(id, v),
            Err(e) => self.reply_err(id, e.to_string()),
        }
    }

    fn doc(&self, uri: &Url) -> anyhow::Result<&Document> {
        self.docs
            .get(uri)
            .ok_or_else(|| anyhow::anyhow!("unknown document {uri}"))
    }

    fn handle_request(&mut self, req: Request) -> anyhow::Result<Value> {
        let enc = self.enc;
        Ok(match req.method.as_str() {
            HoverRequest::METHOD => {
                let p: HoverParams = serde_json::from_value(req.params)?;
                let tdp = p.text_document_position_params;
                let index = self.index_for(&tdp.text_document.uri);
                let doc = self.doc(&tdp.text_document.uri)?;
                let off = doc.offset(tdp.position, enc);
                serde_json::to_value(features::info::hover(doc, off, enc, index.as_deref()))?
            }
            GotoDefinition::METHOD => {
                let p: GotoDefinitionParams = serde_json::from_value(req.params)?;
                let tdp = p.text_document_position_params;
                let index = self.index_for(&tdp.text_document.uri);
                let doc = self.doc(&tdp.text_document.uri)?;
                let off = doc.offset(tdp.position, enc);
                let mut loc = features::entity::entity_at(doc, off)
                    .and_then(|e| e.def)
                    .map(|r| {
                        GotoDefinitionResponse::Scalar(Location {
                            uri: tdp.text_document.uri.clone(),
                            range: doc.range(r, enc),
                        })
                    });
                // Op / type / attribute names jump to their Rust definition.
                if loc.is_none()
                    && let Some((name, kind, _)) = features::dialect_name_at(doc, off)
                    && let Some(e) = index.as_deref().and_then(|i| i.lookup(&name, kind))
                    && let Ok(uri) = Url::from_file_path(&e.file)
                {
                    let p = Position {
                        line: e.line,
                        character: e.column,
                    };
                    loc = Some(GotoDefinitionResponse::Scalar(Location {
                        uri,
                        range: lsp_types::Range { start: p, end: p },
                    }));
                }
                serde_json::to_value(loc)?
            }
            References::METHOD => {
                let p: ReferenceParams = serde_json::from_value(req.params)?;
                let tdp = p.text_document_position;
                let doc = self.doc(&tdp.text_document.uri)?;
                let off = doc.offset(tdp.position, enc);
                let locs: Vec<Location> = features::entity::entity_at(doc, off)
                    .map(|e| {
                        let mut v = e.uses.clone();
                        if p.context.include_declaration {
                            v = e.occurrences();
                        }
                        v.into_iter()
                            .map(|r| Location {
                                uri: tdp.text_document.uri.clone(),
                                range: doc.range(r, enc),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::to_value(locs)?
            }
            DocumentHighlightRequest::METHOD => {
                let p: DocumentHighlightParams = serde_json::from_value(req.params)?;
                let tdp = p.text_document_position_params;
                let doc = self.doc(&tdp.text_document.uri)?;
                let off = doc.offset(tdp.position, enc);
                let hl: Vec<DocumentHighlight> = features::entity::entity_at(doc, off)
                    .map(|e| {
                        let def = e.def;
                        e.occurrences()
                            .into_iter()
                            .map(|r| DocumentHighlight {
                                range: doc.range(r, enc),
                                kind: Some(if Some(r) == def {
                                    DocumentHighlightKind::WRITE
                                } else {
                                    DocumentHighlightKind::READ
                                }),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::to_value(hl)?
            }
            PrepareRenameRequest::METHOD => {
                let p: TextDocumentPositionParams = serde_json::from_value(req.params)?;
                let doc = self.doc(&p.text_document.uri)?;
                let off = doc.offset(p.position, enc);
                let resp = features::entity::entity_at(doc, off).map(|e| {
                    let at = name_range(doc, e.at);
                    PrepareRenameResponse::RangeWithPlaceholder {
                        range: doc.range(at, enc),
                        placeholder: e.name.clone(),
                    }
                });
                serde_json::to_value(resp)?
            }
            Rename::METHOD => {
                let p: RenameParams = serde_json::from_value(req.params)?;
                let tdp = p.text_document_position;
                let doc = self.doc(&tdp.text_document.uri)?;
                let off = doc.offset(tdp.position, enc);
                let Some(e) = features::entity::entity_at(doc, off) else {
                    anyhow::bail!("nothing to rename here");
                };
                let new = p
                    .new_name
                    .trim_start_matches(['^', '@'])
                    .to_string();
                if e.kind != EntityKind::Outline && !is_identifier(&new) {
                    anyhow::bail!("`{new}` is not a valid pliron identifier");
                }
                let edits: Vec<TextEdit> = e
                    .occurrences()
                    .into_iter()
                    .map(|r| TextEdit {
                        range: doc.range(name_range(doc, r), enc),
                        new_text: new.clone(),
                    })
                    .collect();
                let mut changes = HashMap::new();
                changes.insert(tdp.text_document.uri.clone(), edits);
                serde_json::to_value(WorkspaceEdit {
                    changes: Some(changes),
                    ..WorkspaceEdit::default()
                })?
            }
            DocumentSymbolRequest::METHOD => {
                let p: DocumentSymbolParams = serde_json::from_value(req.params)?;
                let doc = self.doc(&p.text_document.uri)?;
                serde_json::to_value(DocumentSymbolResponse::Nested(
                    features::structure::document_symbols(doc, enc),
                ))?
            }
            FoldingRangeRequest::METHOD => {
                let p: FoldingRangeParams = serde_json::from_value(req.params)?;
                let doc = self.doc(&p.text_document.uri)?;
                serde_json::to_value(features::structure::folding_ranges(doc))?
            }
            SemanticTokensFullRequest::METHOD => {
                let p: SemanticTokensParams = serde_json::from_value(req.params)?;
                let doc = self.doc(&p.text_document.uri)?;
                serde_json::to_value(SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: features::semantic_tokens::semantic_tokens(doc, enc, None),
                }))?
            }
            SemanticTokensRangeRequest::METHOD => {
                let p: SemanticTokensRangeParams = serde_json::from_value(req.params)?;
                let doc = self.doc(&p.text_document.uri)?;
                let range = (doc.offset(p.range.start, enc), doc.offset(p.range.end, enc));
                serde_json::to_value(SemanticTokensRangeResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: features::semantic_tokens::semantic_tokens(doc, enc, Some(range)),
                }))?
            }
            InlayHintRequest::METHOD => {
                let p: InlayHintParams = serde_json::from_value(req.params)?;
                let doc = self.doc(&p.text_document.uri)?;
                let range = (doc.offset(p.range.start, enc), doc.offset(p.range.end, enc));
                serde_json::to_value(features::info::inlay_hints(doc, range, enc))?
            }
            Completion::METHOD => {
                let p: CompletionParams = serde_json::from_value(req.params)?;
                let tdp = p.text_document_position;
                let index = self.index_for(&tdp.text_document.uri);
                let doc = self.doc(&tdp.text_document.uri)?;
                let off = doc.offset(tdp.position, enc);
                serde_json::to_value(CompletionResponse::Array(features::completion(
                    doc,
                    off,
                    &self.known_ops,
                    index.as_deref(),
                )))?
            }
            "pliron/serverVersion" => Value::String(env!("CARGO_PKG_VERSION").into()),
            "pliron/analyzerStatus" => {
                let p: DocParams = serde_json::from_value(req.params).unwrap_or(DocParams {
                    text_document: None,
                });
                Value::String(self.analyzer_status(p.text_document.map(|t| t.uri)))
            }
            "pliron/viewEngineModel" | "pliron/viewSyntaxTree" | "pliron/dialectRegistry"
            | "pliron/bundleManifest" => {
                let p: DocParams = serde_json::from_value(req.params)?;
                let uri = p
                    .text_document
                    .ok_or_else(|| anyhow::anyhow!("missing textDocument"))?
                    .uri;
                match req.method.as_str() {
                    "pliron/viewEngineModel" => {
                        Value::String(features::views::model_tree(self.doc(&uri)?))
                    }
                    "pliron/viewSyntaxTree" => {
                        Value::String(features::views::syntax_tree(self.doc(&uri)?))
                    }
                    "pliron/dialectRegistry" => {
                        let index = self.index_for(&uri);
                        Value::String(features::views::registry(index.as_deref()))
                    }
                    _ => {
                        let manifest = match self.routes.get(&uri) {
                            Some(Route::Project(root)) => self
                                .projects
                                .get(root)
                                .and_then(|p| p.bundle_dir.as_ref())
                                .map(|d| d.join("Cargo.toml").display().to_string()),
                            _ => None,
                        };
                        serde_json::to_value(manifest)?
                    }
                }
            }
            _ => anyhow::bail!("unsupported request {}", req.method),
        })
    }
}

impl Server {
    /// A markdown report of the server's state (like rust-analyzer's
    /// "Status" command).
    fn analyzer_status(&mut self, uri: Option<Url>) -> String {
        use std::fmt::Write as _;
        let mut out = format!("# pliron-lsp {}\n\n", env!("CARGO_PKG_VERSION"));
        if let Some(uri) = &uri {
            let route = self.routes.get(uri).cloned();
            let key = self.engine_key(uri);
            let _ = writeln!(out, "## Current document\n\n`{uri}`\n");
            let _ = writeln!(
                out,
                "- route: {}",
                match &route {
                    Some(Route::Project(r)) => format!("project `{}`", r.display()),
                    Some(Route::Reference) => "reference engine".into(),
                    None => "unknown".into(),
                }
            );
            let _ = writeln!(out, "- engine: {}", key.as_deref().unwrap_or("none (syntax layer only)"));
            if let Some(doc) = self.docs.get(uri) {
                let analysis = match (&doc.exact, doc.fresh_exact()) {
                    (_, Some(x)) => format!(
                        "up to date ({} ops, {} µs)",
                        x.model.ops.len(),
                        x.elapsed_us
                    ),
                    (Some(_), None) => "stale (re-analysis pending)".into(),
                    (None, None) => "none yet".into(),
                };
                let _ = writeln!(out, "- engine analysis: {analysis}");
                if let Some(n) = &doc.engine_note {
                    let _ = writeln!(out, "- note: {n}");
                }
            }
            out.push('\n');
        }
        let _ = writeln!(out, "## Projects\n");
        if self.projects.is_empty() {
            out.push_str("None (no open document inside a cargo project).\n");
        }
        let mut roots: Vec<&PathBuf> = self.projects.keys().collect();
        roots.sort();
        for root in roots {
            let p = &self.projects[root];
            let state = match &p.state {
                ProjectState::Building => "building".to_string(),
                ProjectState::Ready => "ready".to_string(),
                ProjectState::NoDialects => "no dialect crates (reference engine)".to_string(),
                ProjectState::Unsupported(r) => format!("unsupported: {r}"),
                ProjectState::Failed(e) => format!("failed: {}", e.lines().next().unwrap_or("")),
            };
            let _ = writeln!(out, "### `{}`\n\n- state: {state}", root.display());
            if p.building {
                let _ = writeln!(out, "- build in progress: {}", p.message.as_deref().unwrap_or(""));
            }
            if let Some(d) = &p.description {
                let _ = writeln!(out, "- {d}");
            }
            if let Some(t) = p.last_build {
                let _ = writeln!(out, "- last build: {:.1} s", t.as_secs_f64());
            }
            if let Some(d) = &p.bundle_dir {
                let _ = writeln!(out, "- bundle: `{}`", d.display());
            }
            if let Some(e) = &p.engine_exe {
                let _ = writeln!(out, "- engine binary: `{}`", e.display());
            }
            let _ = writeln!(out, "- watched: {} paths\n", p.watched.len());
        }
        let _ = writeln!(out, "## Engines\n");
        if self.engines.is_empty() {
            out.push_str("None running.\n");
        }
        let mut keys: Vec<&String> = self.engines.keys().collect();
        keys.sort();
        for k in keys {
            let e = &self.engines[k];
            let _ = writeln!(
                out,
                "- **{}** (`{}`): {}",
                e.label,
                e.exe.display(),
                if e.is_running() { "running" } else { "stopped" }
            );
            if let Some(i) = &e.info {
                let crates: Vec<String> =
                    i.crates.iter().map(|c| format!("{} {}", c.name, c.version)).collect();
                let _ = writeln!(
                    out,
                    "  - {} registrations; crates: {}",
                    i.registrations,
                    crates.join(", ")
                );
            }
            if let Some(err) = &e.last_error {
                let _ = writeln!(out, "  - error: {err}");
            }
        }
        let _ = writeln!(
            out,
            "\n- reference engine: {}",
            self.reference_exe
                .as_ref()
                .map(|p| format!("`{}`", p.display()))
                .unwrap_or_else(|| "not found".into())
        );
        let _ = writeln!(out, "\n## Dialect indexes\n");
        let mut idx: Vec<(&PathBuf, usize)> =
            self.indexes.iter().map(|(k, (i, _))| (k, i.len())).collect();
        idx.sort();
        for (k, n) in idx {
            let _ = writeln!(out, "- `{}`: {n} entries", k.display());
        }
        out
    }
}

/// The range of a name without its sigil.
fn name_range(doc: &Document, (s, e): (u32, u32)) -> (u32, u32) {
    match doc.text[s as usize..e as usize].chars().next() {
        Some('^' | '@' | '!') => (s + 1, e),
        _ => (s, e),
    }
}

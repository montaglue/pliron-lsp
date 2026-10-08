//! The pliron-lsp dialect engine.
//!
//! An engine is a native binary that links (instrumented) pliron plus a set
//! of dialect crates, and serves analysis requests from the `pliron-lsp`
//! frontend over stdin/stdout (see `pliron-lsp-protocol`). Every dialect
//! linked into the binary registers itself when a `Context` is created, so
//! an engine binary is just:
//!
//! ```ignore
//! use my_dialect as _;
//! fn main() {
//!     pliron_lsp_engine::run_stdio(pliron_lsp_engine::BundleInfo::new("my-bundle"));
//! }
//! ```
//!
//! pliron-lsp generates such binaries automatically; nobody needs to write
//! them by hand.

mod analyze;
mod hooks;
mod probe;

use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use pliron::context::Context;
use pliron_lsp_protocol::{
    CrateInfo, EngineInfo, PROTOCOL_VERSION, Request, RequestBody, Response, ResponseBody,
    encode_line,
};

pub use analyze::analyze;
pub use probe::probe;

/// Hash of the engine sources, set by the bundle generator.
pub const ENGINE_SRC_HASH: &str = match option_env!("PLIRON_LSP_ENGINE_SRC_HASH") {
    Some(h) => h,
    None => "dev",
};

/// Static description of the dialect bundle an engine was built from.
#[derive(Clone, Copy, Debug)]
pub struct BundleInfo {
    pub bundle_id: &'static str,
    /// Linked crates: (name, version).
    pub crates: &'static [(&'static str, &'static str)],
}

impl BundleInfo {
    pub const fn new(bundle_id: &'static str) -> Self {
        BundleInfo {
            bundle_id,
            crates: &[],
        }
    }
}

/// Stack size of the worker thread running dialect parsers.
const WORKER_STACK: usize = 512 * 1024 * 1024;

static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => info
                .payload()
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "unknown panic".into()),
        };
        let msg = match info.location() {
            Some(l) => format!("{msg} (at {}:{})", l.file(), l.line()),
            None => msg,
        };
        eprintln!("pliron-lsp-engine: panic: {msg}");
        *LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
    }));
}

/// Take the message of the last panic.
pub(crate) fn take_panic() -> String {
    LAST_PANIC
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_else(|| "unknown panic".into())
}

/// The message of a caught panic (with its location when the panic hook
/// recorded it).
#[cfg(feature = "hooks")]
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let recorded = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner()).take();
    recorded.unwrap_or_else(|| {
        payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into())
    })
}

/// Run `f` on a worker thread with a large stack, catching panics.
pub(crate) fn guarded<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let handle = std::thread::Builder::new()
        .name("pliron-lsp-worker".into())
        .stack_size(WORKER_STACK)
        .spawn(move || catch_unwind(AssertUnwindSafe(f)));
    match handle {
        Ok(h) => match h.join() {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) | Err(_) => Err(take_panic()),
        },
        Err(e) => Err(format!("failed to spawn worker thread: {e}")),
    }
}

fn engine_info(info: &BundleInfo) -> EngineInfo {
    let (registrations, context_error) =
        match guarded(|| pliron::context::get_context_registrations().count()) {
            Ok(n) => (n as u32, None),
            Err(e) => (0, Some(e)),
        };
    let context_error = context_error.or_else(|| {
        guarded(|| {
            let _ = Context::new();
        })
        .err()
    });
    EngineInfo {
        protocol: PROTOCOL_VERSION,
        engine_src_hash: ENGINE_SRC_HASH.to_string(),
        bundle_id: info.bundle_id.to_string(),
        crates: info
            .crates
            .iter()
            .map(|(name, version)| CrateInfo {
                name: name.to_string(),
                version: version.to_string(),
            })
            .collect(),
        registrations,
        context_error,
        hooks: hooks::names(),
    }
}

/// Handle one request.
pub fn handle(info: &BundleInfo, req: Request) -> Response {
    let body = match req.body {
        RequestBody::Hello { .. } => ResponseBody::Hello(engine_info(info)),
        RequestBody::Analyze(params) => {
            let hash = params.text_hash;
            match guarded(move || analyze(&params)) {
                Ok(r) => ResponseBody::Analyze(r),
                Err(panic) => ResponseBody::Analyze(analyze::panicked(hash, panic)),
            }
        }
        RequestBody::Probe(params) => match guarded(move || probe(&params)) {
            Ok(r) => ResponseBody::Probe(r),
            Err(panic) => ResponseBody::Error {
                message: format!("probe panicked: {panic}"),
            },
        },
        RequestBody::Shutdown => ResponseBody::Shutdown,
    };
    Response { id: req.id, body }
}

/// Serve requests from stdin until `Shutdown` or end of input.
pub fn run_stdio(info: BundleInfo) {
    install_panic_hook();
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Some(payload) = pliron_lsp_protocol::decode_payload(&line).or(Some(line.trim()))
        else {
            continue;
        };
        if payload.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(payload) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("pliron-lsp-engine: bad request: {e}");
                continue;
            }
        };
        let shutdown = matches!(req.body, RequestBody::Shutdown);
        let resp = handle(&info, req);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(encode_line(&resp).as_bytes());
        let _ = out.flush();
        if shutdown {
            break;
        }
    }
}

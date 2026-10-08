//! End-to-end tests: a scripted LSP client talks to the server over an
//! in-memory connection; the server talks to the real reference engine
//! process (pliron builtin + llvm, instrumented pliron).

use std::path::PathBuf;
use std::sync::OnceLock;

mod common;

use common::Client;
use serde_json::{Value, json};

const DEMO: &str = r#"builtin.module @m {
  ^entry():
  llvm.func @callee: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {
    ^entry(a: builtin.integer i64):
    llvm.return a
  };
  llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {
    ^entry(x: builtin.integer i64):
    y = builtin.constant <builtin.integer <1: i64>> : builtin.integer i64;
    one = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1;
    llvm.cond_br if one ^bb0(x, y) else ^bb1(x, y)

    ^bb0(x0: builtin.integer i64, y0: builtin.integer i64):
    llvm.br ^bb2(y0, y0)

    ^bb1(x1: builtin.integer i64, y1: builtin.integer i64):
    llvm.br ^bb2(x1, y1)

    ^bb2(x2: builtin.integer i64, y2: builtin.integer i64):
    z = llvm.add x2, y2 <{nsw=false,nuw=false}> : builtin.integer i64;
    c = llvm.icmp z <SLT> x : builtin.integer i1;
    r = llvm.call @callee (z) : llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false>;
    llvm.return r
  }
}
"#;

const URI: &str = "file:///tmp/demo.pliron";

/// Build the reference engine once and return its path.
fn reference_engine() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-q", "-p", "pliron-lsp-engine-ref"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("cargo build");
        assert!(status.success());
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().parent().unwrap();
        let p = dir.join(format!(
            "pliron-lsp-engine-ref{}",
            std::env::consts::EXE_SUFFIX
        ));
        assert!(p.is_file(), "{}", p.display());
        p
    })
    .clone()
}

/// (line, character) of the `nth` occurrence of `needle` (+ `delta` chars).
fn pos(needle: &str, nth: usize, delta: u32) -> Value {
    let off = DEMO.match_indices(needle).nth(nth).unwrap().0;
    let line = DEMO[..off].matches('\n').count();
    let col = off - DEMO[..off].rfind('\n').map(|p| p + 1).unwrap_or(0);
    json!({ "line": line, "character": col as u32 + delta })
}

fn at(needle: &str, nth: usize) -> Value {
    json!({ "textDocument": { "uri": URI }, "position": pos(needle, nth, 0) })
}

#[test]
fn syntax_only_navigation() {
    let mut c = Client::start(json!({ "disableEngine": true }));
    c.open_uri(URI, DEMO);
    let diags = c.wait_diagnostics(|_| true);
    assert!(diags.is_empty(), "{diags:#?}");
    // Go to definition of `y` used in the cond_br.
    let def = c.request("textDocument/definition", at("y) else", 0));
    assert_eq!(def["range"]["start"], pos("y = builtin", 0, 0));
    // Go to definition of the `@callee` symbol.
    let def = c.request("textDocument/definition", at("@callee (z)", 0));
    assert_eq!(def["range"]["start"], pos("@callee:", 0, 0));
    let syms = c.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": URI } }));
    assert_eq!(syms[0]["name"], "@m");
    assert_eq!(syms[0]["children"].as_array().unwrap().len(), 2);
}

#[test]
fn engine_exact_features() {
    let engine = reference_engine();
    let mut c = Client::start(json!({ "enginePath": engine }));
    c.open_uri(URI, DEMO);
    let diags = c.wait_diagnostics(|_| true);
    assert!(diags.is_empty(), "{diags:#?}");

    // Hover on the call result: exact type computed by the llvm.call parser.
    let h = c.request("textDocument/hover", at("r = llvm.call", 0));
    let text = h["contents"]["value"].as_str().unwrap();
    assert!(text.contains("r: builtin.integer i64"), "{text}");
    assert!(text.contains("result #0 of `llvm.call`"), "{text}");

    // Inlay hint for `r` (its type is not spelled out), none for `z`.
    let hints = c.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": URI }, "range": { "start": {"line":0,"character":0}, "end": {"line":40,"character":0} } }),
    );
    let labels: Vec<String> = hints
        .as_array()
        .unwrap()
        .iter()
        .map(|h| format!("{}@{}", h["label"].as_str().unwrap(), h["position"]["line"]))
        .collect();
    assert!(labels.contains(&format!(": builtin.integer i64@{}", pos("r = llvm.call", 0, 0)["line"])), "{labels:?}");
    assert!(!labels.iter().any(|l| l.ends_with(&format!("@{}", pos("z = llvm.add", 0, 0)["line"]))), "{labels:?}");

    // Extension views (rust-analyzer style).
    let doc = json!({ "textDocument": { "uri": URI } });
    let model = c.request("pliron/viewEngineModel", doc.clone());
    let model = model.as_str().unwrap();
    assert!(model.contains("r: builtin.integer i64 = llvm.call (z)"), "{model}");
    assert!(model.contains("^bb2(x2: builtin.integer i64, y2: builtin.integer i64)"), "{model}");
    let tree = c.request("pliron/viewSyntaxTree", doc.clone());
    assert!(tree.as_str().unwrap().contains("STMT builtin.module"), "{tree}");
    let status = c.request("pliron/analyzerStatus", doc.clone());
    let status = status.as_str().unwrap();
    assert!(status.contains("route: reference engine"), "{status}");
    assert!(status.contains("engine analysis: up to date"), "{status}");
    assert_eq!(c.request("pliron/serverVersion", json!(null)), json!(env!("CARGO_PKG_VERSION")));

    // Op names: Rust docs from the dialect sources and go-to-definition into
    // Rust. `llvm.add` is defined by a `macro_rules!` macro in pliron-llvm.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let h = c.request("textDocument/hover", at("llvm.add", 0));
        let v = h["contents"]["value"].as_str().unwrap_or("").to_string();
        if v.contains("Equivalent to LLVM") {
            assert!(v.contains("AddOp"), "{v}");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no docs in hover: {v}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let def = c.request("textDocument/definition", at("llvm.add", 0));
    assert!(def["uri"].as_str().unwrap().ends_with("pliron-llvm-0.18.0/src/ops.rs"), "{def}");

    // Exact definition of `z` used by the call.
    let def = c.request("textDocument/definition", at("z) :", 0));
    assert_eq!(def["range"]["start"], pos("z = llvm.add", 0, 0));

    // Semantic tokens: `if` in `llvm.cond_br if ...` is a keyword of the
    // hand-written llvm.cond_br parser.
    let toks = c.request("textDocument/semanticTokens/full", json!({ "textDocument": { "uri": URI } }));
    let data: Vec<u64> = toks["data"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    let (mut line, mut col) = (0u64, 0u64);
    let mut decoded = Vec::new();
    for t in data.chunks(5) {
        if t[0] != 0 {
            line += t[0];
            col = t[1];
        } else {
            col += t[1];
        }
        decoded.push((line, col, t[2], t[3]));
    }
    let if_pos = pos("if one", 0, 0);
    let if_tok = decoded
        .iter()
        .find(|(l, c, _, _)| *l == if_pos["line"].as_u64().unwrap() && *c == if_pos["character"].as_u64().unwrap())
        .expect("token for `if`");
    assert_eq!(if_tok.3, 5, "keyword"); // legend index 5 = keyword

    // A typo: the engine reports the unregistered op; other errors are
    // still found after recovery.
    let broken = DEMO
        .replace("z = llvm.add", "z = llvm.ad")
        .replace("llvm.return r", "llvm.return q");
    c.change_uri(URI, 2, &broken);
    let diags = c.wait_diagnostics(|d| !d.is_empty());
    let msgs: Vec<&str> = diags.iter().map(|d| d["message"].as_str().unwrap()).collect();
    assert!(msgs.iter().any(|m| m.contains("Unregistered Op llvm.ad")), "{msgs:#?}");
    assert!(msgs.iter().any(|m| m.contains("q")), "{msgs:#?}");
}

#[test]
fn editor_features() {
    let engine = reference_engine();
    let mut c = Client::start(json!({ "enginePath": engine }));
    c.open_uri(URI, DEMO);
    c.wait_diagnostics(|_| true);

    // Signature help inside `llvm.icmp z <SLT> |x`, from llvm.icmp's format
    // (the reference index loads in the background).
    let off = DEMO.find("<SLT> x").unwrap() + "<SLT> ".len();
    let line = DEMO[..off].matches('\n').count();
    let col = off - DEMO[..off].rfind('\n').unwrap() - 1;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let sig = loop {
        let sig = c.request(
            "textDocument/signatureHelp",
            json!({ "textDocument": { "uri": URI }, "position": { "line": line, "character": col } }),
        );
        if !sig.is_null() {
            break sig;
        }
        assert!(std::time::Instant::now() < deadline, "no signature help");
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let label = sig["signatures"][0]["label"].as_str().unwrap();
    assert!(label.starts_with("llvm.icmp ") && label.contains("<predicate>"), "{label}");
    assert_eq!(sig["activeParameter"], 2, "{sig}");

    // Completion offers ops with a snippet of their syntax.
    let items = c.request("textDocument/completion", at("    llvm.return r", 0));
    let icmp = items
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == "llvm.icmp")
        .expect("llvm.icmp completion");
    let snippet = icmp["textEdit"]["newText"].as_str().unwrap();
    assert!(snippet.starts_with("llvm.icmp ${1:"), "{snippet}");
    assert!(snippet.contains("<${2:predicate}>"), "{snippet}");
    assert_eq!(icmp["insertTextFormat"], 2);

    // Workspace symbols and call hierarchy.
    let syms = c.request("workspace/symbol", json!({ "query": "cal" }));
    assert!(syms.as_array().unwrap().iter().any(|s| s["name"] == "@callee"), "{syms}");
    let items = c.request("textDocument/prepareCallHierarchy", at("@callee:", 0));
    let item = items[0].clone();
    assert_eq!(item["name"], "@callee");
    let incoming = c.request("callHierarchy/incomingCalls", json!({ "item": item }));
    assert_eq!(incoming[0]["from"]["name"], "@f", "{incoming}");
    let f_item = c.request("textDocument/prepareCallHierarchy", at("@f:", 0))[0].clone();
    let outgoing = c.request("callHierarchy/outgoingCalls", json!({ "item": f_item }));
    assert_eq!(outgoing[0]["to"]["name"], "@callee", "{outgoing}");

    // Formatting re-indents a messy document.
    let messy = DEMO.replace("    z = llvm.add", "z = llvm.add").replace("  llvm.func @f", "llvm.func @f");
    let uri2 = "file:///tmp/messy.pliron";
    c.open_uri(uri2, &messy);
    let edits = c.request(
        "textDocument/formatting",
        json!({ "textDocument": { "uri": uri2 }, "options": { "tabSize": 2, "insertSpaces": true } }),
    );
    assert_eq!(edits.as_array().unwrap().len(), 2, "{edits}");

    // Quick fix for an unknown op name.
    c.change_uri(URI, 2, &DEMO.replace("z = llvm.add", "z = llvm.ad"));
    let diags = c.wait_diagnostics(|d| !d.is_empty());
    let d = diags
        .iter()
        .find(|d| d["message"].as_str().unwrap().contains("Unregistered Op llvm.ad"))
        .unwrap()
        .clone();
    // The diagnostic points at the misspelled name itself.
    assert_eq!(d["range"]["start"], pos("llvm.add x2", 0, 0));
    let actions = c.request(
        "textDocument/codeAction",
        json!({ "textDocument": { "uri": URI }, "range": d["range"], "context": { "diagnostics": [d] } }),
    );
    let titles: Vec<&str> = actions.as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles.first(), Some(&"Change to `llvm.add`"), "{titles:?}");
}

/// `@symbols` across files (references, rename, reference counts), local
/// renames with scope checks, and source locations as links.
#[test]
fn workspace_rename_lenses_and_links() {
    let engine = reference_engine();
    let mut c = Client::start(json!({ "enginePath": engine }));
    const B_URI: &str = "file:///tmp/user.pliron";
    let b = "builtin.module @user_mod {\n  ^entry():\n  llvm.func @user: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> [] {\n    ^entry(a: builtin.integer i64):\n    r = llvm.call @callee (a) : llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false>;\n    llvm.return r\n  }\n}\n";
    c.open_uri(URI, DEMO);
    c.open_uri(B_URI, b);
    c.wait_diagnostics(|_| true);

    // References to @callee: its definition and the calls in both files.
    let refs = c.request(
        "textDocument/references",
        json!({ "textDocument": { "uri": URI }, "position": pos("@callee:", 0, 1), "context": { "includeDeclaration": true } }),
    );
    let uris: Vec<&str> = refs.as_array().unwrap().iter().map(|l| l["uri"].as_str().unwrap()).collect();
    assert_eq!(uris.iter().filter(|u| **u == URI).count(), 2, "{refs:#}");
    assert_eq!(uris.iter().filter(|u| **u == B_URI).count(), 1, "{refs:#}");

    // Rename it from the file that only calls it: both files change.
    let b_call = b.find("@callee").unwrap();
    let b_pos = json!({ "line": b[..b_call].matches('\n').count(), "character": b_call - b[..b_call].rfind('\n').unwrap() - 1 + 1 });
    let edit = c.request(
        "textDocument/rename",
        json!({ "textDocument": { "uri": B_URI }, "position": b_pos, "newName": "target" }),
    );
    assert_eq!(edit["changes"][URI].as_array().unwrap().len(), 2, "{edit:#}");
    assert_eq!(edit["changes"][B_URI].as_array().unwrap().len(), 1, "{edit:#}");
    assert!(edit["changes"][URI].as_array().unwrap().iter().all(|e| e["newText"] == "target"));
    // `@f` already exists next to it.
    let err = c
        .try_request(
            "textDocument/rename",
            json!({ "textDocument": { "uri": B_URI }, "position": b_pos, "newName": "f" }),
        )
        .unwrap_err();
    assert!(err.contains("already defined in demo.pliron"), "{err}");

    // "N references" above functions (not above the module).
    let lenses = c.request("textDocument/codeLens", json!({ "textDocument": { "uri": URI } }));
    let titles: Vec<&str> = lenses.as_array().unwrap().iter().map(|l| l["command"]["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["2 references", "0 references"], "{lenses:#}");
    assert_eq!(lenses[0]["command"]["command"], "pliron.showReferences");
    assert_eq!(lenses[0]["command"]["arguments"][2].as_array().unwrap().len(), 2);

    // Local names: `z` (3 occurrences); `y2` is taken in the same function,
    // `a` is not (it lives in @callee, another isolated scope).
    let edit = c.request("textDocument/rename", json!({ "textDocument": { "uri": URI }, "position": pos("z = llvm", 0, 0), "newName": "sum" }));
    assert_eq!(edit["changes"][URI].as_array().unwrap().len(), 3, "{edit:#}");
    let err = c
        .try_request("textDocument/rename", json!({ "textDocument": { "uri": URI }, "position": pos("z = llvm", 0, 0), "newName": "y2" }))
        .unwrap_err();
    assert!(err.contains("already names another value in this scope (line 19)"), "{err}");
    let edit = c.request("textDocument/rename", json!({ "textDocument": { "uri": URI }, "position": pos("x: builtin", 0, 0), "newName": "a" }));
    assert_eq!(edit["changes"][URI].as_array().unwrap().len(), 4, "{edit:#}");
    // Block labels clash within their region.
    let err = c
        .try_request("textDocument/rename", json!({ "textDocument": { "uri": URI }, "position": pos("^bb0(x0", 0, 1), "newName": "bb1" }))
        .unwrap_err();
    assert!(err.contains("another block"), "{err}");

    // Source locations are links (and go-to-definition targets).
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("src/kernel.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
    let ir_uri = lsp_types::Url::from_file_path(dir.path().join("out/k.pliron")).unwrap().to_string();
    let ir = "builtin.module @k {\n  ^entry():\n}\n\noutlined_attributes:\n!0 = @[\"src/kernel.rs\": line: 2, column: 5], []\n";
    c.open_uri(&ir_uri, ir);
    let links = c.request("textDocument/documentLink", json!({ "textDocument": { "uri": ir_uri } }));
    let target = links[0]["target"].as_str().unwrap();
    assert!(target.ends_with("src/kernel.rs#L2,5"), "{links:#}");
    let def = c.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": ir_uri }, "position": { "line": 5, "character": 12 } }),
    );
    assert!(def["uri"].as_str().unwrap().ends_with("src/kernel.rs"), "{def:#}");
    assert_eq!(def["range"]["start"], json!({ "line": 1, "character": 4 }));
}

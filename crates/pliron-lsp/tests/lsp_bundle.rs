//! The zero-setup flow through the LSP: opening a `.pliron` file inside a
//! cargo project with a custom dialect makes the server build that
//! project's dialect engine by itself; editing the dialect's Rust source
//! hot-reloads it.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::Client;
use serde_json::{Value, json};

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
    // Forward slashes: a TOML string, also on Windows.
    let api = api.display().to_string().replace('\\', "/");
    let text = std::fs::read_to_string(&m).unwrap();
    let text = text.replace("../../../../../pliron-lsp-api", &api);
    std::fs::write(&m, text).unwrap();
}

fn pos_of(text: &str, needle: &str) -> Value {
    let off = text.find(needle).unwrap();
    let line = text[..off].matches('\n').count();
    let col = off - text[..off].rfind('\n').map(|p| p + 1).unwrap_or(0);
    json!({ "line": line, "character": col })
}

#[test]
fn opening_a_file_builds_the_project_engine() {
    // Share the bundle build directory between runs (incremental builds).
    let shared = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-bundles");
    std::fs::create_dir_all(&shared).unwrap();
    // SAFETY: set before any other thread is started by this test binary.
    unsafe {
        std::env::set_var(
            "CARGO_TARGET_DIR",
            pliron_lsp::canonicalize(&shared).unwrap(),
        )
    };

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("toy");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toy"),
        &dir,
    );
    fix_api_path(&dir);
    let file = dir.join("sample.pliron");
    let uri = lsp_types::Url::from_file_path(&file).unwrap().to_string();
    let text = std::fs::read_to_string(&file).unwrap();

    // No configuration at all.
    let mut c = Client::start(json!(null));
    c.open_uri(&uri, &text);

    // Wait until the project's engine answers: hover on `c` shows the
    // exact op that defined it.
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let h = c.request(
            "textDocument/hover",
            json!({ "textDocument": { "uri": uri }, "position": pos_of(&text, "c = toy.add") }),
        );
        if h["contents"]["value"]
            .as_str()
            .is_some_and(|v| v.contains("result #0 of `toy.add`"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "engine never became ready");
        std::thread::sleep(Duration::from_millis(500));
    }

    // Docs and go-to-definition of the custom op come from its Rust source.
    let h = c.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": pos_of(&text, "toy.add") }),
    );
    let v = h["contents"]["value"].as_str().unwrap();
    assert!(v.contains("Adds two values of the same type."), "{v}");
    let def = c.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": pos_of(&text, "toy.add") }),
    );
    assert!(
        def["uri"]
            .as_str()
            .unwrap()
            .ends_with("toy-dialect/src/lib.rs"),
        "{def}"
    );

    // pliron-lsp-api hooks: a lint warning on the second operand of
    // `toy.add c, c`...
    let diags = c.wait_diagnostics(|d| d.iter().any(|x| x["message"] == "adds a value to itself"));
    let lint = diags
        .iter()
        .find(|x| x["message"] == "adds a value to itself")
        .unwrap();
    assert_eq!(lint["severity"], 2, "{lint:#}");
    assert!(
        lint["source"].as_str().unwrap().ends_with("self_add"),
        "{lint:#}"
    );
    let mut second_c = pos_of(&text, "toy.add c, c");
    second_c["character"] =
        json!(second_c["character"].as_u64().unwrap() + "toy.add c, ".len() as u64);
    assert_eq!(lint["range"]["start"], second_c, "{lint:#}");
    // ...hover notes and inlay hints...
    let h = c.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": pos_of(&text, "toy.add a") }),
    );
    assert!(
        h["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("Result used 3 time(s)."),
        "{h}"
    );
    let hints = c.request(
        "textDocument/inlayHint",
        json!({ "textDocument": { "uri": uri }, "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 100, "character": 0 } } }),
    );
    let unused = hints
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["label"] == "(unused)")
        .count();
    assert_eq!(unused, 2, "{hints:#}");
    // ...and `hints!`: a completion snippet for the hand-written syntax.
    let items = c.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": pos_of(&text, "toy.return") }),
    );
    let items = items
        .as_array()
        .or_else(|| items["items"].as_array())
        .unwrap()
        .clone();
    let repeat = items
        .iter()
        .find(|i| i["label"] == "toy.repeat")
        .expect("toy.repeat completion");
    assert_eq!(
        repeat["textEdit"]["newText"], "toy.repeat ${1:2} times",
        "{repeat:#}"
    );

    // Change the dialect: `toy.print value = c` becomes `toy.print show c`.
    let lib = dir.join("toy-dialect/src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, src.replace("`value` ` = ` $0", "`show` $0")).unwrap();
    c.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [ { "uri": lsp_types::Url::from_file_path(&lib).unwrap(), "type": 2 } ] }),
    );
    // The old syntax is now an error...
    let is_error = |x: &Value| x["severity"] == 1;
    let diags = c.wait_diagnostics(|d| d.iter().any(is_error));
    assert!(
        diags.iter().find(|x| is_error(x)).unwrap()["source"] == "pliron",
        "{diags:#?}"
    );
    // ...and the new one is accepted.
    c.change_uri(
        &uri,
        2,
        &text.replace("toy.print value = c", "toy.print show c"),
    );
    c.wait_diagnostics(|d| !d.is_empty() && !d.iter().any(is_error));
}

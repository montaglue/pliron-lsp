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
    unsafe { std::env::set_var("CARGO_TARGET_DIR", shared.canonicalize().unwrap()) };

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("toy");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toy"),
        &dir,
    );
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
    assert!(def["uri"].as_str().unwrap().ends_with("toy-dialect/src/lib.rs"), "{def}");

    // Change the dialect: `toy.print value = c` becomes `toy.print show c`.
    let lib = dir.join("toy-dialect/src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, src.replace("`value` ` = ` $0", "`show` $0")).unwrap();
    c.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [ { "uri": lsp_types::Url::from_file_path(&lib).unwrap(), "type": 2 } ] }),
    );
    // The old syntax is now an error...
    let diags = c.wait_diagnostics(|d| !d.is_empty());
    assert!(diags[0]["source"] == "pliron", "{diags:#?}");
    // ...and the new one is accepted.
    c.change_uri(&uri, 2, &text.replace("toy.print value = c", "toy.print show c"));
    c.wait_diagnostics(|d| d.is_empty());
}

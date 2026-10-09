//! Automatic dialect bundles: a toy workspace with a custom dialect gets an
//! engine without any user code.
//!
//! This builds pliron and the toy dialect, so it takes a while the first
//! time; the build directory is kept under the workspace `target/`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pliron_lsp::bundle;
use pliron_lsp_protocol::{
    AnalyzeParams, Request, RequestBody, Response, ResponseBody, SpanKind, VerifyMode,
    decode_payload, encode_line, text_hash,
};

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

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toy");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("toy");
    copy_dir(&src, &dir);
    fix_api_path(&dir);
    (tmp, dir)
}

/// A target directory shared between runs, so the build is incremental.
fn shared_target() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-bundles");
    std::fs::create_dir_all(&p).unwrap();
    pliron_lsp::canonicalize(&p).unwrap()
}

fn analyze(exe: &Path, text: &str) -> pliron_lsp_protocol::AnalyzeResult {
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let req = Request {
        id: 1,
        body: RequestBody::Analyze(AnalyzeParams {
            text_hash: text_hash(text),
            text: text.into(),
            verify: VerifyMode::First,
            want_model: true,
            max_attr_len: 200,
        }),
    };
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(encode_line(&req).as_bytes()).unwrap();
    stdin
        .write_all(
            encode_line(&Request {
                id: 2,
                body: RequestBody::Shutdown,
            })
            .as_bytes(),
        )
        .unwrap();
    drop(stdin);
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut result = None;
    for line in out.lines().map_while(Result::ok) {
        if let Some(p) = decode_payload(&line) {
            let r: Response = serde_json::from_str(p).unwrap();
            if let ResponseBody::Analyze(a) = r.body {
                result = Some(a);
            }
        }
    }
    let _ = child.wait();
    result.expect("analysis response")
}

#[test]
fn toy_dialect_bundle() {
    let (_tmp, dir) = fixture();
    let mut meta = bundle::load_metadata(&dir).expect("metadata");
    meta.target_directory = shared_target().try_into().unwrap();
    let sel = bundle::select(&meta).expect("selection");
    let names: Vec<&str> = sel.dialects.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["toy-dialect"]);

    let bundle_dir = bundle::generate(&meta, &sel).unwrap();
    // The engine's own minimum at least (pliron 0.18 needs rustc 1.91).
    let need = pliron_lsp::toolchain::required(&meta, &sel);
    assert!(need.0 >= pliron_lsp::toolchain::ENGINE_MIN, "{need:?}");
    let toolchain = pliron_lsp::toolchain::plan(&dir, need).choice;
    let exe = bundle::build(&meta, &bundle_dir, &toolchain, |e| eprintln!("{e:?}")).expect("build");

    let text = std::fs::read_to_string(dir.join("sample.pliron")).unwrap();
    let r = analyze(&exe, &text);
    assert!(r.parse_errors.is_empty(), "{:#?}", r.parse_errors);
    assert!(r.verify_errors.is_empty(), "{:#?}", r.verify_errors);
    let m = r.model.unwrap();
    let ops: Vec<&str> = m.ops.iter().map(|o| o.opid.as_str()).collect();
    assert!(ops.contains(&"toy.add"), "{ops:?}");
    // The `value` keyword of toy.print's format and the toy.num type.
    assert!(
        r.spans
            .iter()
            .any(|s| matches!(&s.kind, SpanKind::Type { text, .. } if text == "toy.num <32>"))
    );
    let line_of = |needle: &str| text.lines().position(|l| l.contains(needle)).unwrap() as u32 + 1;
    assert!(
        r.spans
            .iter()
            .any(|s| matches!(s.kind, SpanKind::Keyword) && s.start.line == line_of("toy.print"))
    );

    // pliron-lsp-api: the hand-written parser of `toy.repeat 3 times` marks
    // its tokens, and the dialect's hooks run.
    let repeat = line_of("toy.repeat");
    assert!(r.spans.iter().any(
        |s| matches!(&s.kind, SpanKind::Token { token_type } if token_type == "number")
            && s.start.line == repeat
    ));
    assert!(
        r.spans
            .iter()
            .any(|s| matches!(s.kind, SpanKind::Keyword) && s.start.line == repeat)
    );
    let lint: Vec<_> = r
        .hook_diags
        .iter()
        .filter(|d| d.message == "adds a value to itself")
        .collect();
    assert_eq!(lint.len(), 1, "{:#?}", r.hook_diags);
    assert_eq!(
        lint[0].target,
        pliron_lsp_protocol::HookTarget::Operand { index: 1 }
    );
    assert!(
        m.ops
            .iter()
            .any(|o| o.notes.iter().any(|n| n == "Result used 3 time(s).")),
        "{:#?}",
        m.ops
    );
    // `d` and `e` are unused.
    assert_eq!(
        r.hook_hints
            .iter()
            .filter(|h| h.label == "(unused)")
            .count(),
        2,
        "{:#?}",
        r.hook_hints
    );

    // Edit the dialect: rename the keyword and rebuild. The new syntax is
    // picked up (and the old one rejected) without touching any user code.
    let lib = dir.join("toy-dialect/src/lib.rs");
    let src = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, src.replace("`value` ` = ` $0", "`show` $0")).unwrap();
    let exe2 = bundle::build(&meta, &bundle_dir, &toolchain, |_| {}).expect("rebuild");
    let r = analyze(&exe2, &text);
    assert!(
        !r.parse_errors.is_empty(),
        "old syntax must be rejected now"
    );
    let r = analyze(
        &exe2,
        &text.replace("toy.print value = c", "toy.print show c"),
    );
    assert!(r.parse_errors.is_empty(), "{:#?}", r.parse_errors);
}

# pliron-lsp

A language server for [pliron](https://github.com/pliron-org/pliron) textual
IR that **picks up your project's custom dialects automatically**. You don't
write any glue code: open a `.pliron` file inside a cargo project that
defines dialects, and the server builds an analysis engine from those
dialects in the background. From then on, the real parsers and verifiers
of your dialects drive highlighting, diagnostics, navigation and type hints.

## Features

| Feature | Notes |
|---|---|
| Diagnostics | Real pliron parse and verifier errors. Parsing recovers from errors, so you get several per file, and undefined names are reported where they are used. |
| Semantic highlighting | Every word is classified by what the dialect's parser actually parsed it as: op names, types, attributes, keys, format keywords, values, block labels. Words in hand-written parsers, such as `if`/`else` in `llvm.cond_br`, are recognised as keywords. |
| Go to definition | SSA values, block labels, `@symbols`, and op, type and attribute names. Names jump **into the Rust source** of the dialect. |
| References, highlight, rename | Values, labels, symbols. |
| Hover | Exact value types (as printed by pliron), the defining op, op signatures and attributes, and **Rust doc comments** of ops, types and attributes. |
| Type inlay hints | Result types that the op's syntax does not spell out, e.g. the result of `llvm.call`. |
| Completion | In-scope values (with types), `^labels`, `@symbols`, op names (with docs). |
| Outline, folding | Symbol ops, regions, blocks. |
| Hot reload | Saving a dialect `.rs` file re-indexes its docs immediately and rebuilds the engine incrementally (seconds), so edits to parsers and verifiers show up while you work. |

## Quick start

Like rust-analyzer, the VS Code extension bundles the server binaries:

```sh
cargo xtask dist        # release build + dist/pliron-<platform>.vsix
code --install-extension dist/pliron-darwin-arm64.vsix
```

Or run `cargo xtask install` to install the server binaries with `cargo
install` and the extension in one step. For other editors, install the
server (`cargo install --path crates/pliron-lsp` and
`cargo install --path crates/pliron-lsp-engine-ref`) and see
[editors/README.md](editors/README.md).

Open a `.pliron`/`.plir` file:
- **Inside a cargo workspace whose crates define pliron dialects**, the
  status bar shows `pliron: building engine` while the project's engine
  compiles. The first build compiles pliron itself and takes about 30 s;
  later rebuilds take seconds.
- **Anywhere else**, files use the reference engine.

The extension adds rust-analyzer-style tools to the command palette and the
status bar menu: *Show Status*, *View Engine Model*, *View Syntax Tree*,
*Show Dialect Registry*, *Rebuild Dialect Engine*, and *Restart Server*.
See [editors/vscode/README.md](editors/vscode/README.md).

## How it works

```
 editor ──LSP──► pliron-lsp (frontend, no pliron dependency)
                   │  syntax layer: instant, dialect-agnostic fallback
                   │  dialect index: syn scan of dialect sources (docs, Rust locations)
                   │
                   ├──JSON lines──► project engine   (auto-generated, auto-built)
                   │                  = your dialect crates
                   │                  + pliron-lsp-engine
                   │                  + instrumented pliron (via [patch])
                   └──JSON lines──► reference engine (builtin + pliron-llvm)
```

**Engines.** pliron dialects are Rust code: every op, type and attribute
brings its own parser and verifier, and registers itself at link time. Only
a binary that links a project's dialect crates can parse its IR faithfully.
pliron-lsp generates that binary itself, under
`target/pliron-lsp/bundle/`:

1. It runs `cargo metadata` on the project.
2. It selects the crates that depend on pliron and use its registration
   macros.
3. It writes a small package depending on exactly those crates, with the
   same sources, versions, features, `Cargo.lock` and `[patch]` entries as
   the project.
4. It builds the package with the project's own toolchain into
   `target/pliron-lsp/target`, so it never contends with your other builds.

**Instrumented pliron.** The bundle patches pliron with a small instrumented
copy ([third_party/README.md](third_party/README.md)). While parsing, it
records the exact source span of everything the (dialect) parsers consume:
ops, results, operand uses, successors, types, attributes, format keywords.
It also recovers from errors at `;`, block headers and `}`. Positions
therefore come from the real parsers, not from a re-implemented grammar,
including for user-defined syntax.

**Engines run out of process.** A dialect parser that panics, loops or
crashes cannot take the editor down. The engine is restarted, the text that
caused the problem is not retried, and the syntax layer keeps answering.

**Syntax layer.** `pliron-ir-syntax` gives instant, best-effort structure
(blocks, regions, names, symbols) for any IR. It serves until an engine has
analyzed the current text, and whenever no engine is available.

## Configuration

Most projects need none. In the workspace `Cargo.toml`:

```toml
[workspace.metadata.pliron-lsp]
include = ["my-dialect"]        # force-include crates
exclude = ["my-codegen-backend"] # never link these into the engine
```

LSP initialization options are `enginePath`, `disableEngine` and
`disableBundles`; the VS Code settings live under `pliron.*`.

## Requirements and limitations

**Supported projects**
- Automatic engines need **pliron 0.18.x from crates.io**. Other versions
  or sources fall back to the syntax layer, with a message.
- Crates that need `rustc_private` are skipped automatically.

**What building runs**
- Building an engine compiles the dialect crates and runs their build
  scripts and proc macros, so in VS Code it only happens in trusted
  workspaces.

**Analysis scope**
- Verification reports the first verifier error.
- Analysis is per document; there are no cross-file symbols.

## Development

```
crates/pliron-lsp           frontend: server, features, bundles, dialect index
crates/pliron-ir-syntax     dialect-agnostic syntax layer
crates/pliron-lsp-engine    engine library (parse/verify/model with instrumented pliron)
crates/pliron-lsp-engine-ref  reference engine binary
crates/pliron-lsp-protocol  frontend <-> engine wire protocol
third_party/                instrumented pliron 0.18.0 + pliron-derive 0.18.0
editors/vscode              VS Code extension (bundles the server; `npm test` runs it in VS Code)
xtask/                      `cargo xtask dist` / `cargo xtask install`
spike/                      MIR-interpreter feasibility study (rejected; see its README)
```

`cargo test --workspace` runs the following (and `npm test` in
`editors/vscode` runs the extension's integration tests in a downloaded
VS Code):
- unit tests;
- engine tests on real pliron;
- end-to-end LSP tests against the reference engine;
- an automatic-bundle test on a toy dialect workspace
  (`crates/pliron-lsp/tests/fixtures/toy`), which builds the bundle, edits
  the dialect, and checks the hot reload.

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
| Diagnostics | Real pliron parse and verifier errors. Parsing recovers from errors and every op and block is verified separately, so you get all errors in a file, not just the first. Undefined names are reported where they are used. |
| Quick fixes | "Did you mean `llvm.add`?" for unknown ops, types, attributes and dialects (from the dialect sources); similar names for undefined values and labels; insert or remove `;`; insert missing brackets. |
| Semantic highlighting | Every word is classified by what the dialect's parser actually parsed it as: op names, types, attributes, keys, format keywords, values, block labels. Words in hand-written parsers, such as `if`/`else` in `llvm.cond_br`, are recognised as keywords. |
| Go to definition | SSA values, block labels, `@symbols`, and op, type and attribute names. Names jump **into the Rust source** of the dialect. |
| References, highlight, rename | Values, block arguments and labels within their pliron name scope; a rename that would clash with another name of the scope is refused. `@symbols` across the workspace's IR files: the definition in the current file (or the one file defining it) and every reference to it; other files' own `@name` are left alone. |
| Reference counts | "N references" above every `@function` (code lens), counted across the workspace; click to peek them. |
| Source locations | `"src/kernel.rs": line: 12, column: 5` in `outlined_attributes:` (also inside `fused`/`callsite`/`name` locations) is a link to that file and position; Cmd-click or peek-definition shows the code. Relative paths are resolved against the IR file's directory and its parents, then the workspace folders. |
| Round-trip check | Each document without errors is printed with the dialects' own printers and parsed again; operations whose printed form does not parse, or parses into something else (a lost attribute, other types, …), get a warning that says what changed and how it was printed. These are bugs in a dialect's printer or parser. **Show Printed Form** shows the printed text. |
| Hover | Exact value types (as printed by pliron), the defining op, op signatures and attributes, and **Rust doc comments** of ops, types and attributes. |
| Type inlay hints | Result types that the op's syntax does not spell out, e.g. the result of `llvm.call`. |
| Completion | In-scope values (with types), `^labels`, `@symbols`, op names with docs and a **snippet of their syntax** generated from the op's format string (`llvm.icmp ${1:opd0} <${2:predicate}> ${3:opd1} : ${4:type}`). |
| Signature help | While writing an op, its syntax from the format string, with the current part highlighted. |
| Formatting | Indentation and whitespace only (re-printing with pliron would rename values); detects pliron's printer style or a compact style per file. |
| Workspace symbols, call hierarchy | `@symbols` across all `.pliron`/`.plir` files of the workspace; go to a symbol defined in another file; incoming/outgoing references of `@functions`. |
| Outline, folding | Symbol ops, regions, blocks. |
| Hot reload | Saving a dialect `.rs` file re-indexes its docs immediately and rebuilds the engine incrementally (seconds), so edits to parsers and verifiers show up while you work. |

## Command line

```sh
pliron-lsp check [paths]        # lint .pliron/.plir files, e.g. in CI (exit 1 on errors)
pliron-lsp check --format json  # machine-readable findings
pliron-lsp check --roundtrip    # also check that printing and parsing agree (dialect bugs)
pliron-lsp fmt [--check] [paths]
```

`check` uses exactly what the editor uses: the project's dialect engine
(built if needed), the reference engine, or the syntax layer.

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

## Customizing from a dialect (optional)

Everything above works without touching the dialect crates. To change or
extend what the server derives, a dialect crate can depend on
[`pliron-lsp-api`](crates/pliron-lsp-api):

```toml
[dependencies]
pliron-lsp-api = { git = "https://github.com/montaglue/pliron-lsp" }
```

| Macro | What it does | Takes effect |
|---|---|---|
| `hints!` | Docs, format, completion snippet, operand names and syntax facts per op/type/attr. They override what is derived from `#[pliron_op(...)]` and doc comments, which matters most for hand-written parsers. | immediately (read from the source) |
| `keyword!`, `token!` | Drop-in parsers for hand-written `Parsable` impls that tell the server how to highlight what they parse. | after the engine rebuild |
| `lint!` | Extra diagnostics for each verified op; shown in the editor and by `pliron-lsp check`. | after the engine rebuild |
| `hover!` | Extra markdown in an op's hover. | after the engine rebuild |
| `inlay!` | Extra inlay hints at an op, its results or its operands. | after the engine rebuild |

```rust
use pliron::context::{Context, Ptr};
use pliron::operation::Operation;
use pliron_lsp_api::{Diagnostics, Target};

fn self_add(ctx: &Context, op: Ptr<Operation>, diags: &mut Diagnostics) {
    if Operation::get_op::<AddOp>(op, ctx).is_some() {
        let o = op.deref(ctx);
        if o.get_operand(0) == o.get_operand(1) {
            diags.warning("adds a value to itself").at(Target::Operand(1));
        }
    }
}
pliron_lsp_api::lint!(self_add);

pliron_lsp_api::hints! {
    op "toy.repeat" { format: "$count `times`", snippet: "${1:2} times", keywords: ["times"] }
}
```

In normal builds the crate has no dependencies and the macros expand to
type checks only, so it adds no code to the dialect. The engine the server
builds turns the hooks on. A hook that panics is reported as a warning
instead of crashing the analysis. [examples/toy](examples/toy) uses every
macro.

## Requirements and limitations

**Supported projects**
- Project engines are built for pliron **0.16, 0.17, 0.18 and 0.19**, from crates.io
  or from a git dependency: the project's own pliron source is instrumented
  with the matching patch from [third_party/patches](third_party/patches).
  pliron as a local `path` dependency is not supported (cargo cannot
  `[patch]` path dependencies).
- Tested on large real projects: **cubecl** (pliron 0.17 git, 6 dialect
  crates including `pliron-spirv` with ~780 ops) and **cuda-oxide** (pliron
  0.16 git, pinned nightly, ~490 `mir`/`nvvm` ops).
- Crates that need `rustc_private` are skipped automatically.
  pliron-llvm is built without its `llvm-sys` feature (the server never
  needs LLVM); if a build with the project's features fails, it is retried
  with minimal features.
- **Toolchain.** A project's own choice is used: its `rust-toolchain.toml`,
  a rustup override, or `RUSTUP_TOOLCHAIN` (VS Code:
  `pliron.server.extraEnv`). Otherwise, when rustup's default toolchain is
  older than what the engine's dependencies declare (`rust-version`), an
  installed toolchain that is new enough builds the engine instead (stable
  first, then pinned versions, beta, nightly), and the status says which
  and why. If a build still fails because the compiler is too old, it is
  retried with the newest installed toolchain. Nothing is installed
  automatically; the error says what to install.

**What building runs**
- Building an engine compiles the dialect crates and runs their build
  scripts and proc macros, so in VS Code it only happens in trusted
  workspaces.

**Build cache and processes**
- Engines of all projects build in one shared cache (macOS:
  `~/Library/Caches/pliron-lsp`, Linux: `~/.cache/pliron-lsp`, Windows:
  `%LOCALAPPDATA%\pliron-lsp`; set `PLIRON_LSP_CACHE_DIR` to move it, or to
  an empty value to build inside each project's `target/` instead). The
  instrumented pliron and the engine library are compiled once per pliron
  version and toolchain, so another project, worktree or clone only
  compiles its own dialect crates. `pliron-lsp cache` shows its size and
  `pliron-lsp cache --clean` deletes it.
- An engine process stops after 10 minutes without work (VS Code:
  `pliron.engine.idleTimeout`, `0` keeps it) and starts again, in
  milliseconds, when needed.

**Analysis scope**
- Exact analysis is per document (pliron's one-module-per-file model);
  `@symbols` are additionally resolved across the workspace's IR files.

## Development

```
crates/pliron-lsp           frontend: server, features, bundles, dialect index
crates/pliron-ir-syntax     dialect-agnostic syntax layer
crates/pliron-lsp-engine    engine library (parse/verify/model with instrumented pliron)
crates/pliron-lsp-engine-ref  reference engine binary
crates/pliron-lsp-protocol  frontend <-> engine wire protocol
third_party/                instrumentation patches (pliron 0.16-0.19) + instrumented pliron 0.18.0
editors/vscode              VS Code extension (bundles the server; `npm test` runs it in VS Code)
xtask/                      `cargo xtask dist` / `install` / `check-pliron`
spike/                      MIR-interpreter feasibility study (rejected; see its README)
```

`cargo test --workspace` runs the following (and `npm test` in
`editors/vscode` runs the extension's integration tests in a downloaded
VS Code, which opens VS Code windows; `cargo xtask test-vscode [--bundled]`
runs them in a Linux container with Docker instead, on a virtual display,
without touching the working tree):
- unit tests;
- engine tests on real pliron;
- end-to-end LSP tests against the reference engine;
- an automatic-bundle test on a toy dialect workspace
  (`crates/pliron-lsp/tests/fixtures/toy`), which builds the bundle, edits
  the dialect, and checks the hot reload.

**CI** (`.github/workflows/`):
- `ci.yml`, on every push to `main` and pull request: rustfmt, clippy and the
  extension's type check; the Rust tests and the VS Code integration tests
  (Linux only).
- `pliron-compat.yml`: `cargo xtask check-pliron` daily against the newest
  pliron release and the head of pliron's repository, so that a pliron
  change needing a new patch shows up early; weekly (and when the patches
  change) against every supported version line.
- `release.yml`, for a `v*` tag or on demand: extension packages for
  `linux-x64`, `linux-arm64`, `darwin-arm64`, `darwin-x64`, `win32-x64` and
  `win32-arm64` (the arm64 Linux/Windows and x64 macOS ones cross-compiled);
  only the `linux-x64` one is tested as packaged. A tag also publishes a
  GitHub release with them.

# pliron IR for VS Code

Language support for [pliron](https://github.com/pliron-org/pliron) textual IR
(`.pliron`, `.plir`), powered by **pliron-lsp**. The language server ships
inside the extension, so there is nothing else to install.

Open a `.pliron` file inside a cargo project that defines pliron dialects.
The server builds an analysis engine from *your* dialect crates in the
background; the status bar shows `pliron: building engine` while it does.
From then on your dialects' real parsers and verifiers drive everything:

- diagnostics from the real parser and verifier (with error recovery);
- semantic highlighting of ops, types, attributes, keywords, values, labels;
- go to definition for values, blocks, `@symbols`, and op/type/attribute
  names, which jump into the dialect's Rust source;
- references, rename, document highlight, outline, folding;
- hover with exact types and the Rust docs of ops/types/attributes;
- type inlay hints, completion with snippets from each op's format, and
  signature help;
- quick fixes ("did you mean `llvm.add`?"), formatting, workspace symbols and
  call hierarchy for `@functions`.

When you save a dialect `.rs` file, the engine is rebuilt and swapped in
automatically.

## Commands

| Command | |
|---|---|
| `pliron: Show Status` | Projects, engines, bundles and indexes. |
| `pliron: View Engine Model` | The IR as the dialect engine understood it. Updates while you type. |
| `pliron: View Syntax Tree` | The dialect-agnostic syntax layer's view. |
| `pliron: Show Printed Form` | The document as the dialects' printers print it (what the round-trip check parses again). |
| `pliron: Show Dialect Registry` | Ops, types and attributes defined in your dialect sources, with links. |
| `pliron: Rebuild Dialect Engine` | Force a rebuild. |
| `pliron: Open Generated Bundle Manifest` | The generated `Cargo.toml` of the engine. |
| `pliron: Restart / Stop / Start Server`, `Show Logs`, `Show Server Version` | |

Clicking the status bar item opens a menu with these commands.

## Settings

| Setting | |
|---|---|
| `pliron.server.path` | Use another `pliron-lsp` binary instead of the bundled one. |
| `pliron.server.extraEnv` | Extra environment variables for the server and the engine builds it runs. |
| `pliron.engine.enabled` | Turn dialect engines off (syntax features only). |
| `pliron.engine.path` | Use one fixed engine binary for every file. |
| `pliron.bundles.enabled` | Turn off automatic project engines. |
| `pliron.diagnostics.roundTrip` | Warn about operations whose printed form does not parse back to the same IR (on by default). |
| `pliron.trace.server` | Trace LSP traffic. |

Building a project engine compiles your dialect crates, which runs their
build scripts and proc macros. It therefore only happens in **trusted**
workspaces.

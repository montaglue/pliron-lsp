# Changelog

## 0.1.1

- Renamed to "pliron LSP".
- New `pliron/model` server request: the engine's IR model of a document,
  with the source range of each op, for tools that draw the IR.

## 0.1.0

First release.

- Diagnostics, highlighting, navigation, hover, inlay hints, completion,
  quick fixes and formatting for `.pliron` and `.plir` files, driven by the
  project's own dialect crates.
- Project engines for pliron 0.16 to 0.20; a bundled reference engine and a
  syntax-only fallback otherwise.
- Bundled language server for Linux, macOS and Windows (x64 and arm64).

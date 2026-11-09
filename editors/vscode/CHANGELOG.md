# Changelog

## 0.1.2

- Fixed "Duplicate dictionary key" errors from engines of projects that
  use git dependencies without `branch`/`tag`/`rev` (such as
  pliron-tensor): the dialect crates were built and linked twice.
- New setting `pliron.bundles.cacheDir`: `shared` (default) keeps one
  engine build cache for all projects, blank builds inside each project's
  `target/`, a path moves the cache.

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

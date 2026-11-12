# Changelog

## 0.1.3

- Fixed engines failing to start with "duplicate #[distributed_slice] with
  name CONTEXT_REGISTRATIONS" in projects that have a second pliron
  version among their dependencies (such as plironnx, which gets cubecl's
  pliron 0.19 through burn): the engine now links the dialects of the
  pliron the project itself uses only.
- Supports pliron's development branch again: its latest changes
  (pliron-org/pliron#323) made instrumenting it fail, and projects that
  take pliron from git were checked for syntax only.

## 0.1.2

- Fixed "Duplicate dictionary key" errors from engines of projects that
  use git dependencies without `branch`/`tag`/`rev` (such as
  pliron-tensor): the dialect crates were built and linked twice.
- Engines follow the project's `Cargo.lock`: after `cargo update` they
  no longer keep building the old revisions of the dialect crates.
- Projects without a `Cargo.lock` (libraries such as
  pliron-common-dialects) resolve their git dependencies like `cargo
  build` does, instead of offline from whatever revisions were cached
  (which also wrote that stale `Cargo.lock` into the project).
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

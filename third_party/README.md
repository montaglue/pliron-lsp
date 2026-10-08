# Instrumented pliron

`patches/pliron-<version>.patch` instrument pliron 0.16, 0.17 and 0.18. When
pliron-lsp builds a dialect engine for a project, it copies the project's own
pliron source (crates.io or a git checkout), applies the matching patch with
a built-in fuzzy patcher, and points the engine at the result with
`[patch]`. The patches are embedded in the `pliron-lsp` binary.

To regenerate a patch, diff a pristine pliron source against an instrumented
copy in pliron's repository layout (`src/...`, `pliron-derive/src/...`):

```sh
diff -ruN a b > third_party/patches/pliron-0.18.patch   # a = pristine, b = instrumented
```

`pliron-0.18.0/` and `pliron-derive-0.18.0/` are the crates.io releases of
[pliron](https://github.com/pliron-org/pliron) (Apache-2.0, see their
`LICENSE.md`), with a small, self-contained instrumentation patch that lets
pliron-lsp get exact source positions and error recovery out of the *real*
(dialect-defined) parsers.

This repository's own workspace (the reference engine and tests) uses these
copies through `[patch.crates-io]`; `patches/pliron-0.18.patch` is their diff
against the pristine release.

## What changed

Nothing changes unless a parse is started with `pliron::lsp::parse_recorded`
(the recorder is `None` otherwise). pliron's own unit and integration tests
pass unchanged.

| File | Change |
|---|---|
| `src/lsp.rs` (new) | Parse recorder, events, `parse_recorded`, `keyword` and `token` parsers (also used by hand-written parsers through `pliron-lsp-api`), error-recovery helpers. |
| `src/parsable.rs` | `NameTracker` holds the optional recorder; forward references re-point recorded uses; with recovery, unresolved names are reported at each *use* instead of failing at the region start. |
| `src/irfmt/parsers.rs` | `ssa_opd_parse` / `block_opd_parse` record operand / successor spans; `process_parsed_ssa_defs` records result definitions and reports a result-count mismatch as an error instead of panicking. |
| `src/operation.rs` | Records op spans and op-name spans; remembers result names of failed ops (for recovery placeholders). |
| `src/basic_block.rs` | Records block labels and arguments; recovering op loop (resync at `;`, line-start block headers, `}`). |
| `src/region.rs` | Records regions; recovering block loop. |
| `src/type.rs`, `src/attribute.rs` | Record type / attribute / attribute-key spans (including `TypedHandle<T>`). |
| `src/irfmt/outlined.rs` | When recording, keep parsed locations instead of `@[...]` locations from the outlined section. |
| `pliron-derive/src/derive_format.rs` | Format literals parse via `pliron::lsp::keyword` (recorded as keywords). |

The patch is written to be upstreamable behind a cargo feature; once pliron
ships it, bundles can simply enable that feature instead of patching.

# Instrumented pliron

`pliron-0.18.0/` and `pliron-derive-0.18.0/` are the crates.io releases of
[pliron](https://github.com/pliron-org/pliron) (Apache-2.0, see their
`LICENSE.md`), with a small, self-contained instrumentation patch that lets
pliron-lsp get exact source positions and error recovery out of the *real*
(dialect-defined) parsers.

Generated dialect bundles use these copies through `[patch.crates-io]`, so a
project's dialect crates are compiled against them without any change to the
project itself.

## What changed

Nothing changes unless a parse is started with `pliron::lsp::parse_recorded`
(the recorder is `None` otherwise). pliron's own unit and integration tests
pass unchanged.

| File | Change |
|---|---|
| `src/lsp.rs` (new) | Parse recorder, events, `parse_recorded`, `keyword` parser, error-recovery helpers. |
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

# Instrumented pliron

`patches/pliron-<version>.patch` instrument pliron 0.16, 0.17, 0.18 and
0.19. When pliron-lsp builds a dialect engine for a project, it copies the
project's own pliron source (crates.io or a git checkout), applies the
patch for its version line with a built-in fuzzy patcher, and points the
engine at the result with `[patch]`. The patches are embedded in the
`pliron-lsp` binary.

The patches cover pliron's `src/` only, and never edit `use` lists (the
inserted code uses qualified paths), so that they also fit other releases
and git revisions of their version line. pliron-derive is instrumented by a
substitution instead of a patch, which fits every version (pliron 0.16 and
0.17 accept any pliron-derive 0.x): in its sources,
`::pliron::combine::parser::char::string(` becomes `::pliron::lsp::keyword(`.

`cargo xtask check-pliron <0.18 | latest | head>` checks a version end to
end (CI runs it daily for the newest release and the head of pliron's
repository, and weekly for every supported line).

## Supporting a new pliron version

Usually the previous patch applies, with offsets:

```sh
cp -R ~/.cargo/registry/src/*/pliron-0.20.0/src a/src   # pristine
cp -R a/src b/src
(cd b && patch -p1 -F3 -i ../third_party/patches/pliron-0.19.patch)
diff -ruN a/src b/src > third_party/patches/pliron-0.20.patch
cargo xtask check-pliron 0.20
```

If hunks are rejected, port them by hand in `b/src` and diff again.

## This repository's copy

`pliron-0.18.0/` and `pliron-derive-0.18.0/` are the crates.io releases of
[pliron](https://github.com/pliron-org/pliron) (Apache-2.0, see their
`LICENSE.md`), instrumented as above. This repository's own workspace (the
reference engine and tests) uses them through `[patch.crates-io]`;
`patches/pliron-0.18.patch` is the diff of `pliron-0.18.0/src` against the
pristine release.

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
| pliron-derive (substitution, see above) | Format literals parse via `pliron::lsp::keyword` (recorded as keywords). |

The patch is written to be upstreamable behind a cargo feature; once pliron
ships it, bundles can simply enable that feature instead of patching.

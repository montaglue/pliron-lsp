# M-S spike: interpreting real pliron with rust-analyzer's MIR interpreter

**Question:** Can rust-analyzer's MIR interpreter (`ra_ap_hir_ty::mir::eval`) run real pliron 0.18 + pliron-llvm 0.18 code (`Context::new()` → parse → verify → walk) fast enough for an LSP, without compiling dialect code?

**Verdict: NO-GO** (2026-10-07). Two independent reasons, either one alone is enough:
- **Correctness:** basic std code fails under the evaluator, and so do combine and pliron.
- **Speed:** about 450× slower than a native debug build.

## Layout

- `driver/`: plain Rust crate (pliron 0.18 + pliron-llvm 0.18 with `default-features = false`). It contains the analysis entry point `__pliron_lsp_analyze`, plus bisection probes `probe_*`.
  - `cargo run --release --bin native` runs it natively and serves as the oracle.
- `runner/`: loads `driver/` with `ra_ap_load-cargo` and evaluates the named functions with the MIR interpreter (`Function::eval`, inside `attach_db`).
  - Run as: `runner/target/release/pliron-lsp-spike-runner ./driver <iterations> [fn...]`.
  - Diagnostics mode: `runner ... 1 --diag <crate...>`.
  - Build with a nightly toolchain: `ra_ap` 0.0.331 needs `if let` guards (despite declaring rust-version 1.91), and 0.0.357 needs ≥ 1.98. Validated with `nightly-2026-10-01`.

## Environment

macOS aarch64. The project's toolchain is stable 1.94.1, which supplies std sources and the proc-macro server. The runner was built with nightly.

## Results

### Loading
- `load_workspace_at` takes 6.4 s cold (including `cargo check` for build scripts and proc macros) and 0.4 s warm. Peak RSS is about 720 MB.
- Proc macros expand correctly. The `--diag` mode found no type-inference errors in pliron, pliron-llvm or combine. The only unresolved macros are wasm-only `pliron::inventory::submit!` branches.

### Evaluating real code

| Probe | ra_ap 0.0.331 (May 2026) | ra_ap 0.0.357 (Oct 2026) |
|---|---|---|
| `println!` | ✗ `atomic_load`: "monomorphization resulted in errors" | ✗ same |
| `HashMap::new` | ✗ `NotSupported("const block")` | ✗ missing FFI shim `CCRandomGenerateBytes` |
| `format!` / `collect` | — | ✗ fails inside `format` |
| combine `many1(digit())` | — | ✗ MIR lowering `TypeError("non tuple type matched with tuple pattern")` in `Many1::parse_mode_impl` |
| `get_context_registrations()` (linkme) | ✗ recursion in `lt` until the execution limit (8.7 s) | — (needs a shim either way) |
| `Context::new()` | ✗ layout of `Slot<RefCell<Operation>>`: HasErrorConst | ✗ "evaluating builtin derive impls is not supported" |
| `parse_from_str(type_parser(), …)` | ✗ layout of `Result<TypeHandle, Error>`: HasErrorType | ✗ same |
| 1M-iteration integer loop | — | ✓ 3.3–4.3 s |

### Speed

| Workload | Native release | Native debug | Interpreted |
|---|---|---|---|
| 1M-iteration loop | — | 7.4 ms | 3.3 s (**~450×**) |
| full analyze of the 25-line sample (`driver/src/input.rs`) | 4 ms | 13.5 ms | did not run; **~6 s** extrapolated even if every gap were fixed |

A 1k-line file would take minutes. The go criterion was 2 s or less.

## What making it work would take

Every one of the following would be needed. Together they amount to maintaining a fork of r-a's evaluator, not a "small patch":
- newer atomic intrinsics with const-generic orderings;
- `const {}` blocks;
- macOS FFI shims;
- evaluation of builtin derive impls (regressed in 0.0.357);
- a combine MIR-lowering type error;
- several layout errors, `HasErrorConst` and `HasErrorType`;
- a linkme/inventory shim.

Even with all of that fixed, the roughly 450× slowdown is inherent to the evaluator's design: it was built for const-eval, not for throughput.

## Second interpreter tried: Miri (rustc's MIR interpreter)

Toolchain: `nightly-2026-10-01` + the `miri` component, run as `cargo miri run`.

**Correctness: works, with one small change.**
- Unpatched, it stops at linkme with "extern static `section$start$__DATA$__linkme…` is not supported by Miri".
- `vendor/` holds copies of pliron, pliron-derive and pliron-llvm 0.18 in which every `target_family = "wasm"` became `any(target_family = "wasm", miri)`, so they use pliron's own `inventory` backend, and `inventory` became an unconditional dependency. `driver/Cargo.toml` points at them through `[patch.crates-io]`.
- With that change, the whole pipeline runs correctly under Miri: `Context::new()` → parse → verify → dump.
- std, `format!`, `collect` and combine `many1` all work.

**Speed: about 1000× too slow.** Times use `-Zmiri-disable-stacked-borrows -Zmiri-disable-validation`, the fastest settings. `src/bin/phases.rs` produced the phase numbers.

| Phase (25-line sample) | Native debug | Miri |
|---|---|---|
| `Context::new()` | 0.3–2.8 ms | 0.31–0.47 s |
| parse | 2.2–7.8 ms | 1.8–2.5 s (~800×) |
| verify | 2.2 ms | 2.25 s (~1000×) |
| 1M-iteration loop | 7.4 ms | 2.9 s (4.2 s with default checks) |

The cost is in parsing and verifying themselves, not in setup, so keeping a Miri process warm does not help. A 1k-line file would take about 1.5 minutes to parse and about the same again to verify.

## Why existing MIR interpreters are this slow on pliron

Both interpreters execute generic, unoptimized MIR one operation at a time, with dynamic per-operation bookkeeping (allocation tracking, layout computation, call frames).

- **Miri** is built for detecting undefined behaviour.
- **r-a's interpreter** is built for const evaluation.

pliron parses with combine, which passes every character through many layers of generic combinator calls. Native code inlines those away. An interpreter pays for each call, so parsing is its worst case, at about 1000×.

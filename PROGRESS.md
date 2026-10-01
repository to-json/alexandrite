# Progress toward the acceptance tests (goal: ACCEPTANCE.md A1–A6 pass)

Read this first after a context compaction.

## Layout
- `acceptance/cases/`: case files, `.expected`, `.expected_error`, `.expected_runtime`, `.budget`, `fixtures/`, `lib/primes.alx`. Written and checked against Python.
- `acceptance/refs/`: Rust references for the timing budgets (pe007, pe010, pe014). Verified.
- `acceptance/run.py`: the harness.
- `acceptance/ruby/{idiomatic,fast}/`: Ruby ports of the positive cases; `acceptance/bench.py` times them against `alx run` and the release binaries.
- `compiler/`: the `alx` crate (Rust). The root `Cargo.toml` workspace excludes `probes/`.
- `web/`: the in-browser compiler (`alx-web`, wasm32): `alx` lib front end + `wasmgen.rs` (LIR → WebAssembly; generators via resume guards) + `rt.rs` (the runtime in Rust). `build.sh`, `test.mjs` (all cases through the browser pipeline in Node), `publish.sh DEST` (the site: `../loot/webb/warez/11-alexandrite`). `pmap` is sequential there.
- `compiler/runtime/`: the C runtime (`alx.h`, `alx.c`) plus vendored libtommath (Unlicense).

## Compiler architecture (v0)
lexer → parser (AST with spans) → check (type inference per function instance, monomorphized; purity; diagnostics) → lower (typed AST → LIR: loops, temps, labeled breaks; Enumerable chains fused) → jit (Cranelift, in-process: `alx run`) / cgen (C) / rgen (Rust oracle) → driver (clang for native builds, cache in `.alx-cache/` next to the source).

v0 decisions, made deliberately (record them in DESIGN.md at the end):
- **Memory:** one program-lifetime region (per-thread bump allocator, never freed). Memory-safe because nothing is freed; per-scope pools come later.
- **Errors:** one global `Err` type; per-function error-set inference is simplified to "fallible or not".
- **Top level:** acts as `main -> ()!`. Fallible calls there have an implicit `try`; an uncaught error prints a message and exits 1.
- **Prover:** constant intervals, loop-variable intervals (range, step, each_index), fixed-length arrays, division/modulo by a nonzero constant other than -1, and the counter axiom (`v += 1` from a constant start can't overflow 64 bits in any feasible run).
- `first`, `max` and `max_by` on an empty sequence panic (v0; `T?` later).
- Conditions of `if` and `while`: a `{` after a method call is a block only if `|` follows it.
- Generators (`Enumerator.new`): C uses a state machine (Duff's device, locals hoisted); Rust uses nightly `gen` blocks.

## Checklist
- [x] workspace + CLI (`alx run|build|check [--release] [--sanitize] [--expect X] [--emit-c F] [-v] file`)
- [x] lexer, parser
- [x] checker (types, inference, purity, errors); all four compile-error cases match
- [x] lowering + C backend + runtime: all 12 positive cases correct in debug, release and sanitize
- [x] A2 check elision (pe010 release: 0 checked indexes), interval-proven arithmetic; pe010 release ≈1.0x Rust, pe007 ≈1.1x
- [x] A5, A4 (pmap: pe014 0.17x the single-threaded reference), runtime negative cases
- [x] A3 (promote with libtommath amalgamation in `runtime/libtommath/`)
- [x] A2 require: separate compilation + cache by header hash
- [x] A6 (typo suggestion, --expect, no setup, library reuse)
- [x] Rust oracle backend (all 12 programs: rustc accepts, output agrees)
- [x] harness `run.py`: budgets, sanitizers, oracle, license manifest. **76/76 checks pass.**
- [x] v0 decisions recorded in DESIGN.md ("v0 implementation")

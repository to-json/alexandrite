# Alexandrite: design decisions

The Ruby of Rust: Ruby-feel syntax, Rust's type construction, and no tracing GC. Evidence for the decisions below is in `probes/NOTES.md`.

## Settled

- **Architecture** (revised 2026-10-01, replacing "emit Rust source"):
  ```
  .alx → front end (parsing, types, purity, regions, moves: OUR checker)
       → Alexandrite IR  (language-specific optimizations live here)
       → C backend: the main one. Fast compiler for debug builds; clang/gcc for release.
       → Rust backend: a test oracle in CI, plus the bootstrap path.
       → later: Cranelift or QBE.
  ```
  - **Why:** we don't pay for rustc on every build, and we control the whole stack. Optimizations that depend on knowing the language (purity, pool lifetimes, size rules, fusion, destination-passing) run on our IR before any backend sees the code. LLVM and GCC still handle register allocation, instruction selection and vectorization.
  - **Safety:** our checker is the authority. It's simpler than Rust's because values don't escape as references: move checking is flow analysis, and pools and handles are already the front end's job (probe 07). In CI, the Rust backend's output is checked by rustc. **If rustc rejects code our checker accepted, that's a bug in our checker.**
  - **Rules for faithful C** (probe 10): emit every runtime check (bounds, overflow); arithmetic is defined (`-fwrapv` or checked builtins); drops happen at the IR's explicit points; no type punning except through `memcpy`; panic means abort. **Stack frames are capped by the IR**, with large values in pools and our own probes for large frames, because `-fstack-clash-protection` is ignored on Apple arm64. Checks the IR has proved unnecessary are left out of the emitted C. CI runs the oracle and ASan/UBSan to catch a wrong proof.
  - **Measured** (probe 10, about 15k lines): C builds are 2.2x (debug) and 2.8x (release) faster than Rust. `rustc check` alone takes longer than a complete C debug build. Optimized run time is at parity (0.96–1.23x). C debug builds run 2–6x faster than Rust debug builds.
  - **Cost:** the runtime (pools, channels, scheduler) is written in C, without rustc checking it. Tokio doesn't come along. CSP semantics stay as they are (probe 08).
  - The earlier probes emitted Rust, and their findings about semantics still hold. Probe 04 (custom MIR) is now only relevant to the Rust oracle.
  - **Consequences of the move to C:**
    - generics and traits keep Rust's *design*, but monomorphization and trait resolution are implemented in our IR
    - the standard runtime (pools, Vec, String, ordered Hash, Set, channels, scheduler) is ours, written in C
    - source maps are `#line N "file.alx"` directives, so debuggers and sanitizer reports point at `.alx` lines
    - splitting a 2x pool in place (probe 01's gap) is plain C; the checker owns proving it safe
  - **Compiler implementation language:** Rust for now. Self-hosted eventually.
  - **Interop:** using Rust crates goes through `extern "C"` shims around those crates. Using C libraries follows C conventions directly (headers, C ABI). Calls into either from pure code need `trust_pure`.
- **Concurrency runtime:** our own, in C, taking Tokio's scheduler design. Async functions (inferred, probe 08) are lowered by our compiler into stackless state machines, the way rustc lowers `async`. The scheduler uses work stealing with per-worker run queues, a global queue for tasks submitted from outside, a LIFO slot for message-passing locality, and cooperative budgeting in place of preemption. CSP semantics are as in probe 08.
- **No tracing GC.** The objections are pauses, runtime weight and hidden costs.
- **Memory model: pools.** A pool is allocated at a declaration with 2x headroom. The spare half is for copy-and-hand-off.
- **Escaping values:** pool parameters are inferred inside a module. At public boundaries and block exits, values are copied out or the whole pool moves (probes 01 and 02).
- **Syntax: braces everywhere, and `end` doesn't exist.** Methods, blocks, `struct`, `enum`, `error`, `case`, `if`: everything is `{ … }`. Escaping blocks are lambdas, `->(x) { … }`. (The sketches in probes 01–10 predate this and still use `end`.)
- **Blocks:** braces, `{ |x| … }`. They don't escape by default, so they get inlined, and `return`, `?`, `break` and `next` mean what they mean in the enclosing method. A block the callee stores must be an explicit lambda. `yield` in an inlinable method is splicing (probe 05).
- **`#[pure]`** (Rust-style attribute; the exact characters can change later):
  - **Model: destination-passing.** A pure function takes its arguments plus a pre-existing zero value of its return type (Go-style zero values) and fills it in. Scratch memory it uses internally dies at return. The caller never observes anything new coming into existence.
  - **The destination is pre-sized by the caller** according to a size relation declared in the header (e.g. `size out <= size t`). The compiler proves it where it can.
  - **Forbidden:** owning an allocator, I/O, global/static state, mutating arguments (the destination is the only writable thing; local mutation is fine), and panics.
  - **The no-panic rule: proven or fallible.** Anything that could fail at runtime (an unproven size bound, integer overflow, divide by zero, out-of-range indexing) must either be proven safe by the compiler (from loop bounds, header bounds, or ranges) or the header must declare the function fallible (e.g. `-> Tree!`). A failure then becomes an error written into the destination. The cost shows up in the header.
  - Enumerable gives you elements, not indices, so indexing rarely needs proving. Arithmetic is the main load on the prover.
  - **Prover for v0** (probe 06): intervals, structural facts about Enumerable (map keeps length, ...), types that carry facts (`NonEmpty`), and a small standard library of relational lemmas. With that, only genuinely fallible functions are fallible.
  - **Emission** (probe 06): `const fn` where the body allows it, so rustc enforces no I/O, no allocation and no non-const calls, transitively through crates. Our front end enforces no panics, read-only arguments (`&[T]`) and no `static mut`. Functions that need scratch memory are emitted as ordinary functions.
  - An exact header bound means the caller allocates 1x. Headroom is only for sizes that aren't known.
- **Identity: `@City`** is a handle into the enclosing region's pool. A plain `City` is a value. Handles are branded invisibly: the emitter writes `Id<'pool, T>` and opens each pool through a closure, so using a handle on the wrong pool, or after its pool is gone, is a rustc error (probe 03b). Removals leave tombstones by default. A pool that reuses slots must say so, and its handles then carry Vale-style generation checks.
- **Fallibility syntax:** Ruby's `?` and `!` method-name suffixes stay (`include?`, `strip!`). Propagation uses the prefix `try` (`try it.digit`). Fallible returns are written `T!` (inferred error set) or `T!E`, and emitted as `Result<T, E>` with Rust's `?`.
- **Pure code calling non-const Rust functions** is a compile error by default. `trust_pure` at the call site is an escape hatch like `unsafe`: grep-able, and the author vouches for the call. A Stable MIR call-graph verifier comes later, as a lint that upgrades trusted calls to verified ones.
- **Destination after an error:** unreadable until the caller resets it, checked at compile time like a moved-from binding (Hylo's `set`). The emitted Rust still holds zero values, so there's no UB and no reset cost.
- **Diagnostics** (probe 07): the front end owns every Alexandrite-level error: types, handles and pools, purity, block results. Only borrowck move errors and genuine type mismatches are mapped from rustc, through tagged source-map markers (`// @L:C key=val`). Any other rustc error is reported as "internal error: emitted invalid Rust", with the location and raw text. Errors are folded together only when a rule positively identifies them.
- **Pure functions with heap-shaped destinations** (`Str`, `[T]`) are emitted as a pre-sized `&mut [T]` plus a filled length when they need to be `const`. A `String` or `Vec` destination can never be `const`.
- **Concurrency: CSP on tokio, with async-ness inferred** (probe 08). A function is async if it can suspend, transitively. Pure functions are always synchronous, so coloring stops at them. Surface forms:
  - `spawn { }`: parallel; captures must move (values or whole pools); a task boundary is a copy-out boundary
  - `concurrently { } { }`: emitted as `join!`; blocks may borrow
  - `xs.pmap { }`: pure blocks only; scoped OS threads filling a pre-sized destination
  
  v0 can run on OS threads with the same meaning. All handles are branded, so rustc rejects any that cross tasks.
- **Enumerable.** Write `each`, get the rest. The compiler picks one of three ways to compile each call:
  1. **inlined** (the default): the block becomes a loop body and keeps its control flow (probe 05)
  2. **fused chain**: one loop with no intermediate collections (probe 05)
  3. **external**: a nightly `gen` block, for recursive `each`, `.next`, or zipping collections that can't be indexed. Lazy, with O(depth) boxing per element (probe 08). A hot recursive walk can use an explicit-stack helper from the library.
  
  Rules:
  - **Fuse a chain only if every block in it is pure** (inferred). If any block has a side effect, materialize after each stage to keep Ruby's eager meaning.
  - **`lazy` forces fusion, even with side effects.** Each element goes through every stage before the next one starts, so effects interleave and the chain stops early. These are the semantics of Ruby's `Enumerator::Lazy`, and the emission is probe 09's fused loop.
    - **Where it's placed matters:** `xs.map { a }.lazy.map { b }.take(3)` runs stage `a` eagerly and is lazy from `b` onward.
    - **Forced within the same expression** (ended by `take`, `first`, `to_a` or `each`): the blocks are inlined, and `return`, `try` and `break` work.
    - **Stored without being forced** (`e = xs.lazy.map { … }`): the blocks escape, so they follow lambda rules. Non-local `return` and `try` inside them are compile errors.
  - **Size rules** come from Enumerable's header and serve both as prover facts and as destination sizes:
    - `map`, `zip`, `each_with_index`: `==`
    - `select`, `reject`, `uniq`, `take_while`: `<=`
    - `take(n)`: `<= min(n, size)`
    - `each_slice(k)`: `== ceil(size / k)`
    - `flat_map` whose block returns a fixed arity k: `== k * size` (exact, probe 09)
    - `flat_map` with variable output, `chunk_while`: unknown, so 2x headroom
  - **`<=` bounds:** reserve the worst case, and shrink only when the value escapes, which is where copy-out compacts anyway. A temporary that stays in scope keeps the slack (probe 09: 2 allocations instead of 15).
  - **Ownership mode is inferred from the block:** reads only → `iter()`, mutates → `iter_mut()`, consumes → `into_iter()`.
  - The result type comes from the expected type in context, defaulting to an Array.
  - Hashes keep insertion order (IndexMap semantics).
  - `try` inside a block stops at the first error and propagates it.
  - Block shorthand: `it`, `&:name`, `{ |(k, v)| … }`.
  - `pmap` and the other parallel methods accept pure blocks only.
- **Headers:** a ceremony for declaring signatures, purity and size contracts, optionally in a separate file (prior art: OCaml `.mli`, SPARK specs). **They are also separate-compilation boundaries:** one `.o` per module, with the C ABI at the boundaries.
- **Type inference:** as far-reaching as possible (aiming at Crystal-style whole-program inference), limited by one constraint: builds must stay faster than rustc's.
- **Integer overflow in impure code:** chosen per file with a flag. The settings are **abort** (the default), **wrap** and **promote** (to bignum, Ruby-style). The spelling doesn't matter; the working spelling is `#![overflow(abort | wrap | promote)]`. Pure code stays proven or fallible.
  - **Bignum for `promote`:** libtommath (public domain / Unlicense), vendored into the runtime and linked only into programs that use `promote`. GMP was rejected because of its license (see "Runtime licensing").
- **Pool sizing when the size is only known at runtime:** grow like a Vec (doubling), and the compiler warns that the pool's size isn't known statically.
- **Error sets** (probe 11):
  - Declared with `error ParseError { BadDigit(U8), Empty }`.
  - `T!` is inferred: the union of `fail` sites and `try`'d callees' sets. Recursion is solved as a fixed point.
  - `T!E` is declared, and callers see the declared set, so it acts as a boundary. Declared sets can be unions, `T!(ParseError | Overflow)`, because builtin tags like `Overflow` come from unproven arithmetic.
  - **Exported functions must declare their set**, so headers stay stable.
  - Handling: `try e` propagates; `e rescue { |err| … }` handles; `case` matches on tags. Our checker owns exhaustiveness.
  - **In C:** every tag has a unique global number, and all sets share one `Err { tag; payload }` type, so converting to a larger set is a copy. Payloads over 16 bytes go in a pool. `default:` aborts as a backstop.
- **Strings:** UTF-8.
- **Project license: Apache-2.0 WITH LLVM-exception**, for the whole project: compiler, runtime and standard library (`LICENSE`). The exception means that runtime code compiled into a user's binary carries no notice requirement (it waives sections 4(a), 4(b) and 4(d) for embedded portions), and Apache-2.0 provides a patent grant.
- **Runtime licensing: writing a program in Alexandrite must never put an obligation on its author.** Everything linked into a user's binary (our runtime, the scheduler, vendored libraries) must be under a license that requires nothing of people who distribute binaries: no copyleft and no attribution. Allowed: public domain / Unlicense, 0BSD, MIT-0, Apache-2.0 WITH LLVM-exception. Not allowed in the runtime: (L)GPL, MPL, plain MIT/BSD/Apache-2.0 (they require a notice to travel with binaries). The compiler itself isn't linked into user programs, so this rule doesn't apply to it. Rust crates reached through `extern "C"` shims are the user's own choice and their own licenses.
- **Generics and traits** (probe 13):
  - **Hybrid duck typing.** A parameter without a type is generic, and its bound is inferred: each method used resolves to the one trait that provides it. **Exported functions carry the inferred bound in the header** (`pub def total[T: Sum](xs: T)`). Requirements that are inherent or ambiguous are fine inside a module and an error on export. Errors at a call site name the inferred bound and the line it came from.
  - **Nominal conformance:** `impl Shape for Circle { … }`. Derives generate impls. Traits with default methods are the mixins (Enumerable, Comparable).
  - **Inferred `dyn`:** in a mixed collection, the common trait is inferred from how the elements are used. If every concrete type is visible, the compiler emits a **closed tagged union with `switch` dispatch** (1.34x faster than method tables in probe 13). Otherwise it emits `{table, pointer}` with objects in pools. Explicit `dyn` is available.
  - **Code generation:** debug builds pass method tables (4.1x faster compile, 9.6x smaller binary at 200×20; +8% run time), and release builds monomorphize. Same meaning either way. The method table format is shared with `dyn`.
  - **Bound inference beyond direct calls:**
    - Chained use constrains associated types: `x.sum.to_s` infers `T: Sum, T.Output: Show`.
    - Passing `x` on to another generic function adds that function's bounds. Bounds propagate across the call graph as a fixed point, using the same machinery as error sets.
    - If a method takes generic arguments and inference can't decide the bound, it's a compile error asking for an annotation, never a guess.
  - **Syntax:**
    - generic parameters in brackets: `struct Pair[A, B] { a: A, b: B }`, `def first[T](xs: [T]) -> T?`
    - bounds inline (`[T: Sum + Show]`) or in a `where` clause: `def f[T](x: T) where T: Sum, T.Output: Show { … }`
    - associated types declared as `trait Sum { type Output; def sum -> Output }` and referred to as `T.Output`
  - **Release builds:** monomorphize, with three things to limit code size and build time:
    - instantiations shared across modules (deduplicated by type arguments)
    - one copy shared by layout-identical instantiations
    - method tables for functions known to be cold (profile-guided, later)
  - **Macros may declare the signatures they generate:** `macro def derive_json(T: Type) -> Code declares { to_json -> Str; from_json(s: Str) -> T! }`.
    - With a declaration, expansion is lazy per member, and headers list the generated signatures without running the macro.
    - Without one, the whole type expands, as in probe 12.
- **Metaprogramming** (probe 12). It covers boilerplate and derives, DSLs, generated APIs and reflection.
  - **Macros are pure functions that return code**, run by an IR interpreter inside the compiler. They build code with `quote { … }` and splice values in with `#{…}`. They're deterministic, sandboxed and cacheable, and have an evaluation step budget.
  - **Interleaved with type checking, and lazy.** A type's member table is completed the first time something looks up one of its members: its derives run and its refinements attach. Macros may look up other types (nested expansion). Dependency loops are compile errors that name the full chain. Macros nobody uses never run.
  - **Reflection at compile time:** `T.fields` (with attributes such as `#[json(rename: …)]`), `T.methods`, `T.responds?`, `T.name`.
  - **`method_missing` at compile time:** called when a lookup fails; returning nil means the method doesn't exist. Results are cached by (type, name, argument types).
  - **DSLs:** blocks with a receiver (`&block: Spec` sets the block's `self`). Free at run time, because blocks are inlined.
  - **Refinements:** `refine` / `using`, lexically scoped, emitted as functions with mangled names.
  - **Declared inputs only:** `comptime read(path)` works only on files listed in the build manifest, and their contents are hashed.
  - **Incremental cache:** keyed on *direct* dependencies, with member-table fingerprints for early cutoff. Never record dependencies transitively, which made 10 000 types take 66 s in probe 12. Use a global revision counter so a build where nothing changed skips checking entirely.
  - Exported macros are shipped in headers as IR. Macro-generated code is hygienic, except for names spliced in through `#{…}`. Errors inside generated code are reported with the chain of expansions.
  - The scheduler must use an explicit work stack, not recursion, because chains of types run deep.

## v0 implementation (compiler/, passing ACCEPTANCE.md)

Deliberate simplifications in the first compiler, each a known gap against the design above:

- **Memory:** one program-lifetime region. The runtime allocates and never frees, so it is memory-safe by construction. Per-scope pools (probes 01 and 02) aren't implemented yet.
- **Allocation:** since nothing is freed, `alx_alloc` is a per-thread bump pointer into 1 MiB malloc chunks (larger requests go to malloc). Peepholes that avoid building strings no one observes: `n.to_s.size` counts digits (bignums: compare against cached powers of ten), and `s == s.reverse` on a variable is an in-place palindrome test.
- **Error sets:** one global `Err` type. A function is either fallible or not; per-set inference (probe 11) isn't implemented yet.
- **Top level** behaves like `main -> ()!`. Fallible calls there get an implicit `try`, and an unhandled error prints a message and exits 1.
- **Prover:** constant and loop-variable intervals (ranges, `step`, `each_index`), arrays of fixed length, division or modulo by a constant other than 0 and -1, sum bounds (element interval × element count), and **the counter axiom**: `v += 1` from a constant start can't overflow 64 bits in any feasible run. Release builds drop every check that this proves unnecessary.
- **`alx run` is a JIT.** Debug runs lower the whole program (libraries included, from source) to Cranelift IR, compile it in memory, and run it in the `alx` process, with the C runtime linked into `alx` (build.rs; entry points in `runtime/jit_shims.c` take only int64 and pointer arguments). Same LIR, same runtime, same checks and messages as the C backend; edit-to-answer drops from ~140 ms (clang + link) to ~2 ms. `--release`, `--sanitize`, `--emit-*`, `alx build` and `ALX_NO_JIT=1` use clang.
- **Floats and structs (Go semantics, GO-VS-RUBY.md):** `Float` is an IEEE double; `puts`/`%v` print like Go's `fmt` (shortest digits, exponent below 1e-4 and from 1e6, `+Inf`); `format` takes Go's verbs (so far `%v %d %s %t %f %.Nf`), checked at compile time against the arguments. Only numeric literals convert to Float implicitly; Int variables need `.to_f`. Integer `/` and `%` truncate. `struct Name { field: Type }` is a value type; `Name.new(field: v)` zero-fills omitted fields, or every field positionally; places (`a[i].f op= v`) are written in place. Structs lower to the tuple layout, so every backend handles them through one path.
- **Generics:** monomorphization only. There are no method tables in debug builds yet, and no traits.
- `first`, `max`, `max_by` and `reduce` on an empty collection panic. `T?` comes later.
- **Conditions of `if` and `while`:** a `{` after a method call counts as a block only if `|` follows it.
- **Generators:** a state machine in C (labels and gotos, every variable hoisted into the state), a nightly `gen` block in the Rust oracle.
- **`pmap` blocks** can't capture locals yet.
- **Libraries** (`require`) are compiled separately and cached by a hash of their source. The generated `.alxh` header is the boundary. Exported functions must have typed parameters.
- **Bignums** (`promote`): libtommath, vendored as an amalgamation and always compiled optimized. The Rust oracle has its own small bignum, and its division only handles values that fit in 64 bits.

## Build plan

Each phase ends with something runnable. The probes' `out.rs` / `.c` files are the expected output for the matching phases.

1. **Parser:** braces-only syntax, `.alx` and header files, with spans kept for `#line` source maps.
2. **Front end:** type inference with inferred bounds; error sets; purity and the proven-or-fallible prover (intervals, Enumerable size rules, types that carry facts); handles and branding; **our move/region checker**. Every Alexandrite-level error is reported here (probe 07).
3. **IR:** explicit drops, pools, destinations and checks. Optimizations that depend on knowing the language run here: inlining blocks, fusing pure chains and `.lazy`, pre-sizing destinations, removing checks the prover made unnecessary.
4. **C backend and runtime:** pools, Vec, String (UTF-8), an insertion-ordered Hash and Set, the `Err` representation, overflow modes, stack-frame caps. Debug builds use method tables and clang -O0 (tcc where it's available). Release builds monomorphize and use -O2.
5. **CI safety net:** the Rust backend's output checked by rustc (the oracle), plus C built with ASan/UBSan, over every test program.
6. **Standard library:** Enumerable and Comparable as traits with default methods, and the size rules.
7. **Macros:** the IR interpreter, the lazy scheduler (explicit work stack, direct dependencies, early cutoff, a revision counter), and declared inputs.
8. **Concurrency:** inferred async lowered to state machines; a work-stealing scheduler in C copied from Tokio's design; channels, `spawn`, `concurrently`, `pmap`.
9. **Self-hosting.**

## Deferred measurements

Not open design questions: checks to run once the compiler exists.

- Release-build cost of monomorphization with calls spread across files (probe 13's -O2 run was flawed).
- How much monomorphization gains over method tables on code that vectorizes.
- tcc as the debug backend, measured on Linux.
- `-fstack-clash-protection` on Linux gcc and clang, versus our own stack probes.

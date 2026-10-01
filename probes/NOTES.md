# Probe notes

Toolchain: rustc 1.99.0-nightly (2026-07-11). Run any probe with `cargo run -p probe_<name>`.

`rt/` is the runtime the compiler would link against. It contains:
- `Pool<T>` and `Id<T>`: a typed arena with 2x headroom
- `Bytes` and `Span`: a byte arena for strings
- `Rooted<T>`: a value moving together with its pool, plus `compact()` for copy-out
- counters for copies and spills (a spill is a pool outgrowing its 2x headroom)
- a counting global allocator

## 01_tree: where escaping values go

Expected output: `sum = 2048` for both strategies, with 2047 nodes per tree. **Matches.**

| Strategy | Copies | Spills | Signature shape |
|---|---|---|---|
| (a) caller's pool, inferred | 0 | 0 | `doubled(src: &Pool, t: Id, out: &mut Pool) -> Id` |
| (b) copy out at return | 4094 (2 × 2047) | 0 | `doubled(t: &Rooted) -> Rooted` |

Findings:
- **(a)'s pool parameters are lifetimes.** `doubled` needs *two* of them: one region it reads from and one it writes into. Inference has to produce exactly this shape. MLKit-style inference is the right ancestor.
- **(b) is (a) plus a boundary policy.** Its private `*_into` helpers are literally the (a) functions. Copy-out isn't a different memory model. It's a choice about *where* region parameters stop appearing in signatures.
- **Implied design: region inference inside a module, copy-out (or moving the pool) at public API boundaries.** Signatures stay clean, the internals stay copy-free, and the cost appears exactly where the API boundary is.
- `compact()` copies everything reachable from the root into a fresh pool. That's a Cheney-style copying collection, run at a point you can see in the source. **Be honest that it's a scoped GC.** It's deterministic, with no pauses and no runtime, but it costs O(reachable).
- Moving the pool *without* compacting costs 0 copies, but any garbage in it travels with it. That's a reasonable default when a function's pool holds only its result, which is the case for `build` here.

## 02_lines: loops and region leaks

Expected: identical `kept` and `last` in both modes. **Matches.**

| Mode | N | kept | Peak bytes | Bytes/line | Copies | Spills |
|---|---|---|---|---|---|---|
| per-iteration pool | 10 | 2 | 471 | 47.1 | 29 | 0 |
| per-iteration pool | 10 000 | 4816 | 200 274 | 20.0 | 85 793 | 0 |
| leaky (one region) | 10 | 2 | 1 256 | 125.6 | 0 | 2 |
| leaky (one region) | 10 000 | 4816 | 1 000 256 | 100.0 | 0 | 2 |

Findings:
- **The MLKit region leak shows up as predicted.** Naive inference puts the loop body's temporaries in the outer region and pays about 5x the memory, growing linearly with N.
- **A per-iteration pool keeps scratch memory constant.** The scratch pool never spilled. What's left is the outer pool's 2x reservation plus the survivors.
- **Survivors cost one copy per byte** (85 793 bytes for 4 816 lines). That's the price of copy-and-hand-off, and it's a fair one: the leaky version pays far more in memory instead.
- **Sizes known at compile time didn't help here.** Line length and the survival rate are both runtime facts. The outer pool was sized from `n` (a runtime value) times a guessed survival rate. **"Most things are compile-time sized" holds for struct shapes and not for anything that holds strings or collections.** Pools need a policy for runtime sizing (a high-water mark from the first iteration, or a profile).
- The emitter needs one rule: **any value reachable from outside the block gets copied out; everything else lives in the block's pool.** That's escape analysis per block, which is tractable.

## 03_graph: cycles

Expected: reachable `Ashby Bree Crick Dunmore Eastfold`, `hops a->e = 4`. After removing Dunmore: `Ashby Bree Crick`, unreachable. **Matches.**

Findings:
- **Value semantics can't express cycles.** A copy of a city is a different city. The surface language needs a type that means *identity*: sketched here as `@City`, a handle into the pool of the enclosing region. This is the biggest leak of the memory model into the surface syntax. Hylo has the same leak and handles it with explicit indices.
- Removal needed tombstones (`Slot::Live` / `Slot::Dead`) so handles to a removed city stay detectable. **If pools ever reuse slots, handles need generation counters (Vale).** No reuse means memory only grows until the pool dies, which is fine for scoped pools and bad for long-lived ones.
- Handles have no lifetime (`Id<T>` is 4 bytes, `Copy`), so the borrow checker can't see when a handle outlives its pool. A handle carries no reference to its pool, so it could be used to index the wrong pool of the same type. This isn't memory-unsafe, because bounds checks catch out-of-range ids. **But an in-range id from the wrong pool silently reads the wrong value, and rustc can't catch that.** A fix would be branded pools, e.g. `Id<'pool, T>` with an invariant lifetime. That brings lifetimes back, but generated code can carry them for free.

### 03b: branded handles (`brand.rs`)

`Id<'id, T>` with an invariant brand lifetime. Each pool is opened through `with_pool(n, |pool| …)`, which creates a fresh brand that no other pool can share.

| Case | Result |
|---|---|
| use a handle on its own pool | runs (`same pool ok: 5`) |
| `--features cross`: a handle from p1 used on p2 | **rejected**, E0521 |
| `--features escape`: a handle outlives its pool | **rejected**, "lifetime may not live long enough" |

The gap from probe 03 is closed by rustc. Costs:
- Every branded pool has to be opened through a closure. That's fine for the emitter, because pools are already lexical.
- **The rustc error messages are useless to an Alexandrite user.** They need translating into something like "`@City` from `cities` used on `towns`."

## 04_custom_mir: the "inline MIR" escape hatch

`#![feature(custom_mir, core_intrinsics)]`, with `core::intrinsics::mir::mir!` inside a `#[custom_mir(dialect = ...)]` function.

| Dialect | Borrow-checked? | Observed |
|---|---|---|
| `built` | **yes** | E0499 (two `&mut`) and E0382 (use after move), with spans on the MIR source lines |
| `analysis` | **yes** | the same two errors |
| `runtime` | **no** | compiles and runs: `aliasing_mut -> 2`, `use_after_move -> 5` |

Reproduce:
- `cargo run -p probe_custom_mir` runs the sound case: `bump(41) -> 42`.
- `cargo build -p probe_custom_mir --features violations` shows the borrowck errors.
- `cargo run -p probe_custom_mir --features unchecked` runs the `runtime` dialect with borrowck skipped.

Findings:
- **The granularity is one function.** You can't drop to MIR for a block inside a Rust function. Each emitted function is either all Rust or all MIR, which is enough for a local escape hatch.
- **The `built` dialect keeps the memory proof.** That reverses the earlier guess. The hatch is safe by default, and `runtime` is the equivalent of `unsafe`.
- It's still a compiler-testing feature: nightly only, no stabilization path, and the syntax changes. Using it pins the toolchain.
- Bodies have to be fully typed, with method calls resolved by path (`String::len(r)`). Type inference and trait resolution are still the emitter's job, the same cost as a full MIR backend, but you pay it only for the functions that need it.

## 05_blocks: blocks with non-local control flow

Every function is emitted twice: once as Rust closures, once inlined as loops. Expected:
- `parse_all(bad)` = `Err("2, bo, lots")`
- `first_big_spender(600)` = `"ann"`, and `(9999)` = `None`
- `top_totals` = `[300, 120, 400]`
- `first_gap` = `4`, and `None` when there's no gap

**Matches, and the two emissions agree on every case.**

| Block feature | Closure emission | Inlined emission |
|---|---|---|
| `return x` from a block | `try_for_each` → `ControlFlow::Break` → `match` at the call site | `return x`, word for word |
| `?` in a block | works only via `Result: FromIterator`; otherwise the same as `return` | `?`, word for word |
| `next` | `return ControlFlow::Continue(())` | `continue` |
| `break v` (a value for the whole call) | `ControlFlow::Break(v)` plus unwrapping | `break 'label v`. **Rust's labeled blocks are exactly "a call whose value `break` can set."** |
| chain fusion (`select.map.take(3)`) | lazy iterator adapters (fine) | one loop that breaks at `len == 3` (fine) |

Findings:
- **Inlining is mechanical.** The block body becomes the loop body, and each control-flow keyword maps one-to-one onto a Rust keyword. Closure emission needs `ControlFlow` plumbing for each block, and nested blocks add another layer of it per level.
- The inlined code is *longer* (56 vs 46 non-blank lines), but nobody reads generated code. What matters is that it's mechanical and has no `ControlFlow` types.
- **Rule: blocks don't escape by default, so they get inlined.** A block the callee stores has to be an explicit lambda (`->(x) { … }`), where `return` means "return from the lambda." That's Ruby's own block/lambda split and Kotlin's `inline`/`crossinline`.
- **Implied design: `yield` inside an inlinable method is splicing.** If a user-defined `each` is inlinable, all of Enumerable can be built on it in the library and still compile to plain loops. Recursive iterators (trees) can't be inlined, and fall back to closures plus `ControlFlow`.
- Block = region boundary: in probe 02, the `each { |line| … }` body was the per-iteration pool. **Inlined blocks give the emitter a lexical scope where it can open and close a pool.**
- Syntax: braces for blocks (the sketches now use `{ |x| … }`); `do…end` is dropped. Ruby's `{}`/`do` precedence quirk goes with it.

## 06_pure: `#[pure]` as destination-passing style

Files: header `src.alxh`, bodies `src.alx`, emitted code `out.rs`. Expected results:
- `sum(1..100)` = 5050, and `sum(big)` = Overflow
- `doubled` gives `[-2^32, -2, 0, 14, 2^32-2]`
- `parse_digits("9071")` = `[9,0,7,1]`, and `"90x1"` gives BadDigit(`x`)
- `mirror` reverses the leaf order
- `average(MAX, MAX, MAX-3)` = MAX-1
- `median(5,1,9,3,7)` = 5

**All match. The const fn versions agree with the ordinary ones.**

### The caller sees no allocation

The caller allocates every destination before the call, sized by the header bound.

| Function | Allocations during call | Net live bytes |
|---|---|---|
| sum, doubled, parse_digits (success and error), mirror, average | 0 | 0 |
| median | 1 (scratch for sorting) | **0** |

`median` is your "we might technically allocate inside": the scratch is allocated and freed within the call, and the caller sees nothing new. In `mirror`, the destination pool sized with `Pool::exact(size t)` ended at len 15, cap 15, with no spill.

**Finding: when a header bound is exact, the 2x headroom isn't needed.** The caller allocates exactly 1x. Headroom only matters where the size isn't known in advance.

### How each proof obligation was discharged

| Obligation | Kind |
|---|---|
| `sum` overflow | **[prover]**: depends on the data, so it has to be fallible. Correct, not a weakness of the prover. |
| `doubled`: `i32 * 2` fits in `i64` | [interval] |
| `doubled`, `parse_digits`, `mirror`: writes stay in bounds | [structural]: map keeps length, one output node per input node |
| `parse_digits` bad character | **[domain]**: fallible, correctly |
| `average`: no division by zero | [interval] via the `NonEmpty` type (parse, don't validate) |
| `average`: i128 accumulator can't overflow | [interval] |
| `average`: result fits in i32 | **[relational]**: min ≤ sum/n ≤ max. An interval-only prover can't get this. |
| `median`: index n/2 < n | [interval] given n ≥ 1 |

What v0 needs from the prover:
- **interval arithmetic** plus **structural facts about Enumerable** (map keeps length, filter shrinks it, and so on)
- **types that carry facts**, like `NonEmpty`, so preconditions become values
- a small library of **relational lemmas** written once in the standard library (mean ≤ max, and so on). A general relational solver isn't needed for v0.

With those, the only fallible functions in this set are the ones that *should* be fallible. Fallible returns didn't spread to everything.

### What `const fn` enforces (nightly 1.99, `const_trait_impl`)

`konst::COMPILE_TIME = (6, 20, 6)` is computed at compile time, so these functions really are const. Generic code works with `[const] Add` bounds.

| Purity rule | Does const fn enforce it? | Evidence |
|---|---|---|
| No I/O | **yes** | E0015 on `println!` |
| No heap allocation | **yes** | `Vec::push` "not yet stable as const fn", E0493 on the Vec destructor, E0015 on `Box::new` |
| No global state (atomics, interior mutability) | **yes** | E0015 on `AtomicUsize::load` |
| No `static mut` | **no** | reads compile and return the runtime value (41). Only const-*evaluation* fails (E0080). |
| Arguments read-only | **no** | writing through `&mut [T]` compiles. Our front end must emit `&[T]` for arguments. |
| No panics | **no** | `xs[9]` compiles and panics at runtime |
| Calls to Rust crates are pure | **mostly yes** | a const fn can only call const fns, so I/O and allocation are ruled out transitively, with no MIR walker needed. Crate functions that aren't const can't be called at all. |

Costs of emitting `const fn`:
- `for` loops aren't allowed (iterator traits aren't const), so the emitter writes index `while` loops. Blocks are already inlined into loops (probe 05), so this costs almost nothing.
- **Functions that need scratch memory can't be const.** That rules out `median` (heap plus sort) and `mirror` (`Pool::put` is `Vec::push`).

**Verdict:** emit `#[pure]` as `const fn` when the body allows it. That gets transitive no-I/O and no-allocation checking from rustc for free. Our front end checks the rest: no panics (proven or fallible), read-only arguments, no `static mut`. Functions that need scratch memory are emitted as ordinary functions plus our own checks. A Stable MIR verifier is only needed for calling *non-const* crate functions from pure code.

## 07_errors: mapping rustc errors back to Alexandrite

Run with `python3 07_errors/map.py`. Each case in `cases/` has an `.alx` source and its emitted `.rs`. The emitted code carries a source map as trailing comments of the form `// @LINE:COL key=val …`. The mapper runs `rustc --error-format=json`, maps spans through those markers, and rewrites each error with per-code rules. (The mapper is Python because it's tooling. What the probe measures is how much information survives, which doesn't depend on the mapper's language.)

| case | rustc errors | alx errors | primary spans on a marker | labels on a marker | info needed |
|---|---|---|---|---|---|
| 1_moved (E0382) | 1 | 1 | 1/1 | 2/2 | position + tags |
| 2_cross_pool (E0521 ×2) | 2 | 1 | 3/3 | 4/4 | tags |
| 3_handle_escape (no code) | 1 | 1 | 1/1 | 2/2 | tags |
| 4_block_type (E0308) | 1 | 1 | 1/1 | 3/3 | position + tags + types |
| 5_pure_call (E0015 ×3, E0493) | 4 | 1 | 4/4 | 1/1 | position + tags |

The output reads well, e.g. ``error: `c` is a handle into `cities`, used on `towns` --> 2_cross_pool.alx:9:8``. But the table flatters the result. Here's what each rule actually depended on:

| case | What rustc contributed | What carried the message | Class |
|---|---|---|---|
| 1_moved | the variable name, plus the "moved here" and "used here" spans: **genuinely useful** | the `call=` tag supplied the callee | **A/B**: worth mapping |
| 2_cross_pool | **misleading**: the first error's primary span is on `cities.put` (line 8) and it says "`towns` escapes". Two errors for one mistake. | the mapper ignored rustc's spans and checked handle/pool tags itself | **C**: the front end should catch this |
| 3_handle_escape | "lifetime may not live long enough", with no error code | the `escapes=return` tag | **C** |
| 4_block_type | the types (`Vec<i64>` vs `Vec<String>`), which translate cleanly through the type table | rustc's primary span was the `out` return (line 8), not the block. The `block_result` tag put it on the block. | **B** |
| 5_pure_call | 4 errors, and **3 of them come from our own emission** (`push_str` on the `String` destination, deref coercion, the String destructor) | the `call=`/`rust=` tags | **C**, and **it hid an emitter bug** |

Findings:
- **A good error message about branding, pools or purity has to come from the front end.** For classes C, the mapper wasn't translating rustc. It was redoing a check the front end could have done, using tags the front end wrote. rustc's errors there are unreliable (E0521 points at the wrong line).
- **Map rustc errors only for the borrow checker and genuine type mismatches** (classes A and B). Every other rustc error on emitted code should be reported as **"internal error: the compiler emitted invalid Rust"**, with the mapped location and the raw rustc text attached. Elm takes the same stance: errors from the backend are compiler bugs.
- **Folding errors is dangerous.** Folding case 5's 4 errors into 1 produced a nice message and **hid a real emitter bug**: a pure function whose destination is a `String` can never be `const`, because `push_str` and the String destructor aren't const. Fix: emit heap-shaped destinations as pre-sized `&mut [T]` plus a filled length (as `parse_digits` does in probe 06), or emit the function as non-const. Only fold errors that a rule has positively identified as coming from the user's own call.
- **Source maps need semantic tags, not just positions.** Positions alone gave the right line in every case. Every good message also needed a tag (`call=`, `sink=`, `pool=`, `handle=`, `block_result=`). The emitter has to write these as it goes.
- **Column precision is lost.** Markers are per line, so case 1 points at `puts` (col 3) instead of `o` (col 8). Fix: markers per expression, or a side table that maps rustc's column range to Alexandrite columns.
- **Rules that match on message text are brittle across rustc versions.** Match on error `code` and span labels, and pin the toolchain (already required by nightly).
- rustc reports errors **one phase at a time**: borrowck errors only show up once type checking passes. One more reason for the front end to own type errors.
- Control check: nested branded pools used correctly compile cleanly (verified separately), so case 2's errors are caused by the misuse alone.

## 08_csp: CSP on tokio with inferred async

Expected: tally `ann 1350 (10), bo 1450 (10), cy 1550 (10)`; `pmap` equal to the sequential map; `max=1550 min=1350`; `find { it > 5 }` = 6 after visiting 6 of 8 leaves. **All match.**

### Inferring async

Nothing in `src.alx` is marked async. The emitter's rule: **a function is async if it can suspend** (channel send or receive, sleep, I/O), or if it calls a function that can.

| fn | async? | why |
|---|---|---|
| `parse`, `collatz` | no | `#[pure]` |
| `fetch` | yes | sleep (standing in for I/O) |
| `producer`, `parser`, `tally` | yes | channels |
| `leaves` | no | a gen block: a coroutine, but synchronous |

**Purity implies the function is synchronous.** A pure function can't do I/O or touch channels, so it can never suspend. Function coloring stops spreading at every pure function, and for free.

### Boundaries

| Case | Result |
|---|---|
| a pool moving between tasks (`tally` returns `Totals` through a `JoinHandle`) | works. **A task boundary is just another boundary where values copy out or pools move** (the probe 01 rule). |
| `tokio::join!` with two futures borrowing `t` | works. Concurrent, not parallel, with **no `'static` requirement because nothing is spawned**. |
| `--features scoped_spawn`: a `tokio::spawn`ed task borrowing `t` | **rejected**, E0373 |
| `--features handle_send`: a branded `@handle` sent into a spawned task | **rejected**, E0521 (branding catches it) |
| an unbranded `Id` sent into a spawned task | **accepted** by rustc, printed `#0`. Branding is required for rustc to act as a safety net here. |

Findings:
- **Two concurrency forms fall out of tokio's `'static` requirement:**
  - **`spawn { }`**: parallel; everything it captures must be moved in (values, or whole pools)
  - **`concurrently { } { }`**: emitted as `join!`; concurrent on one task; blocks may borrow
  
  It's the same split as moving vs borrowing, and it's visible in the syntax.
- **`pmap` doesn't use tasks at all.** Pure CPU work goes onto scoped OS threads (`std::thread::scope`), which can borrow. Each worker fills its own slice of a pre-sized destination, so it's destination-passing style again.
- **Use branded handles everywhere.** With unbranded handles, rustc can't catch a handle crossing a task boundary.

### Enumerable: recursive `each` via a gen block

`leaves` is a recursive tree walk written as a nightly `gen` block. `find` is inlined over it as a `for` loop, so `return` from the block is still a plain `return` (probe 05). It's lazy: the search stopped after 6 of 8 leaves. The cost: each level of recursion boxes its iterator, so producing one element goes through O(depth) indirections. That's fine as a fallback; a hot path would want an explicit stack.

### Process note

The first version of this emission contained a `transmute` hack to rebuild `Id`s, which was unsound because struct layout isn't guaranteed. It was replaced with `Pool::iter` / `iter_mut` in `rt`. **The emitter must never reach for `unsafe` to work around a missing runtime API. Add the API to the runtime.**

## 09_enum: Enumerable compilation decisions

### 1. Fusion is only legal for pure blocks. Confirmed.

| Chain | Result | Observable |
|---|---|---|
| impure `map { puts …; it * 2 }.take(3)`, materialized | `[2, 4, 6]` | **6 lines printed** (Ruby's meaning) |
| the same chain, fused | `[2, 4, 6]` | **3 lines printed**: same value, different behavior. Fusing an impure chain is a miscompile. |
| pure `map { it * 2 }.take(3)`, fused | `[2, 4, 6]` | 3 block calls |
| pure, materialized | `[2, 4, 6]` | 6 block calls |

### 2. Size rules (n = 100 000; the input is allocated before measuring)

| Chain | len | cap | allocs | unused bytes |
|---|---|---|---|---|
| `select.map`, plain `collect` | 33 334 | 65 536 | **15** | 257 616 |
| `select.map`, size rule (`<= size`) | 33 334 | 100 000 | **1** | **533 328** |
| `select.map`, size rule + shrink | 33 334 | 33 334 | 2 | 0 |
| `select.take(10)`, plain `collect` | 10 | 16 | 3 | 48 |
| `select.take(10)`, size rule | 10 | 10 | **1** | **0** |
| `flat_map { [it, it] }`, plain `collect` | 200 000 | 200 000 | 1 | 0 |
| `flat_map { [it, it] }`, 2x headroom | 200 000 | 200 000 | 1 | 0 |
| `flat_map { [it, it, it] }`, 2x headroom | 300 000 | 400 000 | 2 (spill) | 800 000 |

Findings:
- **A `<=` bound is a trade-off: one allocation, but sized for the worst case.** It wasted twice as much memory as `collect`'s growth by doubling. Policy: **reserve the worst case, and shrink only when the value escapes.** The copy-out boundary is already where values get compacted (probe 01's `compact`). A temporary that dies inside the scope keeps the waste, since it's freed soon anyway. Two allocations instead of 15, with no waste in anything that escapes.
- **`take(n)` is a clear win:** exact capacity in 1 allocation, vs 3 allocations with leftover space.
- **Surprise: when the block returns a fixed-size array, Rust's `flat_map` is already exact.** `collect` got the size exactly right in one allocation. The size rule should be **`flat_map` with a block of fixed arity k → `== k * size`**, an exact bound. Use 2x headroom only for blocks whose output size varies. The x3 case shows that guessing headroom when the arity is known is just wrong: it spilled and wasted 800 KB.

### 3. Ownership modes inferred from the block

`sum` (reads) → `iter()`, `each { it.total += 1 }` (mutates) → `iter_mut()`, and `each { archive << it }` (consumes) → `into_iter()`. All compile, with the expected result (`sum=60`, archive totals 11/21/31). With `--features use_after_consume`, using `orders.size` afterwards fails with **E0382**. That's class A in probe 07's scheme: worth mapping from rustc ("`orders` was consumed by the block at …").

## 10_c: C as the main backend

Files: `10_c/alx_rt.h` (the C runtime), `bench.c` (hand-emitted C), `bench.rs` (the same program as the Rust backend would emit it), `run.py` (build time, run time, sanitizers), and `scale.py` (how build time grows). Toolchain: Apple clang 17 and rustc 1.99-nightly on arm64 macOS. **tcc wasn't tested**: it isn't installed, and its arm64 macOS support is doubtful.

### Build time as the program grows

`scale.py` makes K renamed copies of every workload function and calls them all from `main`, so every copy is type-checked and code-generated. Times are medians of 3 runs, in ms.

| copies | C lines | Rust lines | clang -O0 | clang -O2 | rustc check | rustc debug | rustc -O |
|---|---|---|---|---|---|---|---|
| 1 | 105 | 119 | 45 | 47 | 38 | 88 | 109 |
| 10 | 708 | 839 | 52 | 80 | 56 | 118 | 189 |
| 50 | 3388 | 4039 | 104 | 226 | 150 | 252 | 565 |
| 200 | 13438 | 16039 | **343** | **840** | **426** | **769** | **2350** |

All four builds produce the same output at every size.

- At about 15 000 lines, **C debug builds are 2.2x faster than Rust debug builds, and C release builds are 2.8x faster than Rust release builds.**
- **`rustc check` alone (426 ms) takes longer than a complete C debug build (343 ms).** Keeping rustc as a per-build check would more than double the edit-compile loop. That confirms keeping rustc as a CI oracle, off the build path.
- The tiny single-file version (`run.py`) mostly measures startup: clang -O0 44 ms, rustc debug 102 ms.

### Run time (10M elements, best of 5)

| workload | C -O2 | Rust -O | C/Rust | C -O0 | Rust debug |
|---|---|---|---|---|---|
| sum (checked) | 3.62 | 3.25 | 1.11 | 9.94 | 60.54 |
| doubled | 1.20 | 1.16 | 1.03 | 12.30 | 34.82 |
| select_map (fused, pre-sized) | 4.59 | 4.57 | 1.00 | 12.20 | 33.61 |
| parse_digits | 2.86 | 2.60 | 1.10 | 12.52 | 47.28 |
| mirror (pool, 2M nodes) | 4.33 | 3.52 | **1.23** | 9.76 | 16.19 |
| gather (data-dependent bounds checks) | 17.04 | 17.66 | 0.96 | 31.20 | 78.74 |

The checksums agree across all four builds.

- **Optimized builds are at parity** (0.96 to 1.23). The emitted C isn't tuned yet; `mirror` is the one to look at. Candidates: copying the tagged union by value, and bounds checks rustc can remove because its slices carry their length.
- **Debug builds of C run 2 to 6x faster than Rust debug builds.** Unoptimized Rust iterators are notoriously slow, and the emitted C is already loops. Combined with the faster build, that makes the dev loop much better.

### Faithfulness checks (clang `-fsanitize=address,undefined -fno-sanitize-recover=all`)

| run | result |
|---|---|
| the whole workload | **clean**: no ASan or UBSan reports |
| out-of-range index through `ALX_IDX` | aborts with `alexandrite panic: index out of bounds at bench.c:114`: defined behavior, no UB |
| `sum` overflow / `doubled(INT32_MAX)` | `Err(Overflow)` through checked builtins / widened to 4294967294. No UBSan report. |

Findings:
- **`-fstack-clash-protection` is silently ignored by Apple clang on arm64** (`argument unused during compilation`). On this target, that faithfulness rule can't be met with the flag. **Our IR knows every function's frame size**, so: cap stack frames, put large values in pools, and emit our own stack probes for any frame that stays large. Check the gcc/clang behavior on Linux before relying on the flag anywhere.
- **Branding has no runtime existence in C.** In the Rust probes, rustc's lifetimes rejected a handle used on the wrong pool (03b, 08). In C, only our checker stands between that bug and a silent wrong read. That's consistent with the architecture, and it's why CI keeps the Rust oracle.
- `select_map` in C writes past `len` with no capacity check, relying on the structural size bound (`len <= n == cap`). The Rust version still calls `push`, which checks capacity. Where the IR has proved a bound, **C lets us actually drop the check, which the Rust backend can't do.** If the proof is wrong, that check is gone, so these elisions need the oracle and the sanitizers in CI.

## 11_errsets: error sets for `T!`

Files: `src.alx` (the sketch, in braces syntax), `infer.rs` (an inference prototype in Rust, the compiler's language: `cargo run -p probe_errsets`, plus 4 tests), and `errset.c` (the C representation, run under ASan and UBSan).

### Inference

| fn | declared | body produces | callers see | verdict |
|---|---|---|---|---|
| digit | inferred | {BadDigit} | {BadDigit} | ok |
| parse | inferred | {BadDigit, Empty, Overflow} | same | ok |
| read | {Denied, NotFound} | {} | {Denied, NotFound} | ok |
| load | inferred | {BadDigit, Denied, Empty, NotFound, Overflow} | same | ok |
| eval (recursive) | inferred | the same as load | same | ok |
| config (rescues load) | {Invalid, Missing} | {Invalid, Missing} | same | ok |
| leaky | inferred | … | … | **error**: an exported function needs an explicit set |
| narrow | {BadDigit, Empty} | {BadDigit, Empty, **Overflow**} | {BadDigit, Empty} | **error**: the declared set lacks {Overflow} |

The fixed point took 2 rounds here. A test with mutual recursion converges within 3.

Findings:
- **The rules work as written.** An inferred set is the union of the function's `fail` sites and the sets of its `try`'d callees. A declared set is what callers see, so it acts as a boundary. `rescue` stops propagation. Recursion is solved as a fixed point, which is more permissive than Zig, where recursive functions must declare their set.
- **Builtin tags leak into declared sets.** `narrow` declares `ParseError`, but unproven arithmetic adds `Overflow` (probe 06's proven-or-fallible rule). **Fix: declared sets can be unions, `Int!(ParseError | Overflow)`.** That's better than silently adding `Overflow` to every set, which would make declared sets imprecise.
- **Exported functions must declare their sets.** That keeps headers stable for separate compilation, so changing a private function can't silently change a public interface.

### The C representation

One global error type: `{ uint16_t tag; union payload }`. **Every tag in the program has a unique number**, so all sets share one C type. Converting a set to a larger one across `try` is a copy of the same type. `T!` lowers to `{ Err err; T val; }`, and `val` is unreadable after an error.

| input | load → | config → |
|---|---|---|
| `123` | 123 | 123 |
| `12x` | BadDigit, payload `'x'` | Invalid |
| `""` | Empty | Invalid |
| `99999999999999999999` | Overflow (checked builtins) | Invalid |
| `missing` | NotFound | Missing(missing) |
| `/etc` | Denied | Invalid |

ASan and UBSan are clean. `sizeof(Err)=24`, `sizeof(ResInt)=32`.

Findings:
- **C can't check exhaustiveness per set.** `-Wswitch-enum` judges every switch against *all* tags in the program. On `config`'s `rescue`, it listed 6 "unhandled" tags, including ones `load` can't produce. **Our checker owns exhaustiveness.** Emitted C gets an aborting `default:` as a backstop and is compiled with `-Wswitch`, not `-Wswitch-enum`.
- **The largest payload sets the size of every error.** A `Str` payload (16 bytes) makes every `Err` 24 bytes. Rule: payloads over 16 bytes go in a pool, and the error carries a handle to it.
- Zig's errors carry no payloads at all, which is a known pain point. Global numbering keeps Zig's cheap conversions and still allows payloads.

## 12_meta: scheduling for lazy, type-aware macros

Files: `src.alx` (the sketch) and `meta.rs` (a Rust prototype: `cargo run --release -p probe_meta`, 6 tests). The macros are Rust closures standing in for pure Alexandrite functions; the IR interpreter isn't built. What's under test is **the order of evaluation**, interleaved with member lookups from the type checker.

### The scenario, over three builds

| lookup | build 1 (cold) | build 2 (no change) | build 3 (`phone` added to account.json) |
|---|---|---|---|
| `User.==` | ok, from derive(Eq) | ok | ok |
| `Order.to_json` | ok; **forced User's expansion partway through** | ok | ok |
| `Account.find_by_email` | ok, from compile-time `method_missing` | ok | ok |
| `Account.find_by_phone` | error: `method_missing` returned nil | error | **ok**: the field exists now |
| `A.mirror_x` | error: `dependency loop: A derive(Mirror(B)) -> B derive(Mirror(A)) -> A` | same | same |
| `Leaky.new` | error: `secrets.json` is not a declared input | same | same |
| **macro runs** | 11 | **2** (only the loop, whose errors are never cached) | **5**: `schema:Account`, both `method_missing` finders, and the loop |
| never expanded | `Audit` | `Audit` | `Audit` |

What this shows:
- **Laziness works.** `Audit` is never expanded. `Order` forcing `User` partway through its own expansion works.
- **Loops are reported, not hung on.** The error carries the full chain of what depended on what.
- **Compile-time `method_missing`** generates typed finders on demand. When the schema changes, it reruns exactly the finders on that type.
- **Editing a declared input reruns only what depends on it:** the schema macro and the finders. `User` and `Order` stay cached.
- Undeclared inputs are rejected with a pointer to the build manifest.

### A bug caught during the probe: dependencies must be direct

The first version copied every nested expansion's dependencies up into its parent's cache entry. Each entry ended up holding its whole transitive dependency set, and checking membership in it was linear: **1 000 types took 97 ms and 10 000 took 66 s.**

The fix is to record **only direct dependencies**. When a macro looks up another type, it records that type's member-table *fingerprint*. The fingerprint changes if and only if something beneath it changed. This is the "early cutoff" pattern from Salsa and rust-analyzer.

`early_cutoff` (a test) checks it: adding a field to `T0` reruns T0's 3 derives and T1's `Eq` and `Json`, the two that look T0 up. T1's member table comes out identical, so **T2 through T49 stay cached.** `deps_are_direct` (a test) checks that no entry records more than 3 dependencies.

### Scaling, after the fix

A chain of N types, each with 3 derives; `Json(T_i)` forces `T_{i-1}`.

| N | lookup | types expanded | macro runs | cold ms | warm rebuild ms |
|---|---|---|---|---|---|
| 1 000 | last.to_json | 1 000 | 3 000 | 6.85 | 3.56 |
| 1 000 | T5.to_json | 6 | 18 | 0.07 | 0.06 |
| 10 000 | last.to_json | 10 000 | 30 000 | 52.03 | 30.69 |
| 10 000 | T5.to_json | 6 | 18 | 0.40 | 0.27 |

Findings:
- **Scheduling costs about 1.7 µs per macro run, and grows linearly.** Interpreted macro bodies will dominate, not the scheduler. For comparison, probe 10's whole 15 000-line C build took 343 ms.
- **Laziness is the main win.** Asking for `T5` touches 6 types out of 10 000.
- **A warm rebuild re-checks everything it reaches** (31 ms for 10 000 types) even when nothing changed. Fix: a global revision counter for inputs and declarations. If nothing changed since the last build, skip checking entirely (Salsa does this, and calls it durability).
- **Recursion depth:** the chain recurses once per type. The probe ran its scaling test with a 512 MB stack. Running it at the default stack size wasn't tested. **The real compiler needs an explicit work stack** (or a depth limit with a clear error).
- **Expansion is lazy per type, not per member.** `User` ran all 3 derives when only `to_json` was needed. To get laziness per member, a macro would declare the signatures it generates (a header for macros). That also gives other modules generated signatures without running anything. Not built.
- Loop errors are never cached, because they depend on the expansion stack. A program with a loop reruns those macros on every build. That's cheap, and it's an error state anyway.

## 13_generics: inferred bounds, and four ways to generate generic code

### 13a: inferring bounds (`bounds.rs`: `cargo run -p probe_generics`, 3 tests)

A parameter without a type is generic. Each method used on it becomes a requirement. If exactly one trait provides the method, the requirement is that trait, a nominal bound that can be written into a header.

```
pub def total[T: Sum](x: T)
pub def describe[T: Shape](x: T)
pub def show_all[T: Shape + Show](x: T)
def radius_of(x)  # internal: structural requirement, checked per call
error: cannot export `radius_pub`: `x.radius` (line 19) is an inherent method, not provided by any trait; …
error: cannot export `amb`: `x.size` (line 20) is provided by several traits: Collection, Sized2; …

error line 24: `Square` doesn't implement `Shape`, which `describe` requires (inferred from `x.name` on line 16)
error line 26: `Square` has no method `radius`, which `radius_of` calls on line 18
```

- **The hybrid works.** Ruby-style unannotated code inside a module, Rust-style bounds at the header. Errors at a call site name the inferred bound *and the line it was inferred from*, instead of pointing deep inside an instantiation the way C++ does.
- Inherent or ambiguous requirements are fine internally and block export: the same "infer inside, explicit at boundaries" rule as error sets.
- **Not covered yet:**
  - chained use (`x.sum.to_s` constrains an associated type)
  - passing `x` on to another generic function: bounds have to propagate, a fixed point like error sets
  - methods with generic arguments
  
  These are the hard part of real bound inference.

### 13b: code generation forms in C (`shapes.c`, `gen.py`)

Run time, 10M elements. `dict` is noinline, with its table hidden from the optimizer the way separate compilation would hide it:

| form | use | -O0 ms | -O2 ms |
|---|---|---|---|
| mono | release, `[Circle]` | 29.82 | 6.92 |
| dict (method table) | debug, `[Circle]` | 21.69 | **7.47** (+8%) |
| dyn `{table, ptr}`, objects in per-type pools | mixed, open set | 61.90 | 47.87 |
| enum (tagged union + `switch`) | mixed, closed set | 58.34 | **35.78** (1.34x faster than dyn) |

Results agree within each pair. Sanitizers clean. `sizeof(DynShape)=16`, `sizeof(AnyShape)=24`.

- **Process note: the first dict measurement was fake.** With a constant global table and a `static` function, clang devirtualized it into mono (6.92 vs 6.95). Fixed with `noinline` and a `volatile` table pointer.
- **Dictionaries cost only 8% here,** because the loop is bound by floating-point add latency and the indirect call always hits the same target. Code that can vectorize (integer kernels) would show a bigger gap for mono. Not measured.
- **Inferring a closed enum for mixed collections is worth it:** 1.34x faster than `dyn`, with no pointer chasing into separate pools and no indirect calls. It costs 24 bytes per element instead of 16, the size of the largest variant.

Build time and size: G generic functions × T types, every combination called from `main`:

| G × T | form | C functions | clang -O0 ms | clang -O2 ms | -O2 binary KB |
|---|---|---|---|---|---|
| 50×10 | mono | 500 | 101 | 803 | 98 |
| 50×10 | dict | 50 | 52 | 527 | 34 |
| 200×20 | mono | 4 000 | **454** | 55 684 | **681** |
| 200×20 | dict | 200 | **111** | 53 460 | **71** |

The outputs agree.

- **Debug builds: dictionaries compile 4.1x faster and produce 9.6x smaller binaries** at 200×20. That confirms dictionaries for debug builds.
- **The -O2 column is a flaw in the test, not a result.** `main` contains 4 000 calls with constant tables, so clang inlines and devirtualizes everything. It effectively monomorphizes the dict build too, and both take about 55 s compiling one huge `main`. The release-build comparison needs calls spread across functions or files. Not redone.

## Verdict on escaping values

**A hybrid.** Within a module, infer pool (region) parameters. Probe 01 (a) showed zero copies, and the inference target has a known shape. At public boundaries and block exits, copy out or move the pool:
- probe 01 (b) gives clean signatures
- probe 02 shows block-exit copy-out is what prevents region leaks

The cost of copying is visible because it happens at a syntactic boundary.

## The expressiveness gap: what emitted Rust can't express

1. **Branded handles.** Making `Id` safe against the wrong pool needs `Id<'pool, T>`. Rust *can* express this, but it's noisy (generativity tricks). Not a gap, but generated code has to carry it.
2. **Splitting one pool buffer into two halves** for copy-and-hand-off within the 2x slab. A safe `Vec` can't keep from-space and to-space in one allocation. Probe 01 used a fresh pool as the to-space instead. Doing it in place needs `unsafe` or `split_at_mut` gymnastics. **This is the first real candidate for MIR or `unsafe`.**
3. **Block-level MIR.** It doesn't exist, so the escape-hatch granularity is a whole function.

## How noisy the emitted code is

| Probe | Noise | Why |
|---|---|---|
| 01 (a) | medium | pool parameters thread through every signature |
| 01 (b) | low at the API, medium inside | `Rooted` wrapper plus `_into` helpers |
| 02 | medium | arena `Span`s replace `String`, every string op goes through the pool |
| 03 | medium-high | handles plus tombstones, `Option` on every lookup |
| 04 | high | hand-written MIR, by nature |
| 05 inlined | low | loops plus native control flow; labeled blocks for `break v` |
| 06 dps | low | destination parameter plus `Result` for fallible functions |
| 10 C | low-medium | explicit loops, `ALX_IDX` / `ALX_ADD` macros, tagged unions |
| 06 konst | medium | index `while` loops instead of iterators |
| 05 closures | medium-high | `ControlFlow` plumbing for each block that exits early |

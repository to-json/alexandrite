# Alexandrite: design decisions

The Ruby of Rust: Ruby-feel syntax, Rust's type construction, and no tracing GC. Evidence for the decisions below is in `probes/NOTES.md`.

## Settled

- **Backend:** emit Rust source and compile it in one rustc build. Keep the IR separate from the backend so it can be swapped. Move to MIR only when the expressiveness gap hurts (probe 04: custom MIR works one function at a time, and the `built` dialect still borrow-checks).
- **No tracing GC.** The objections are pauses, runtime weight and hidden costs.
- **Memory model: pools.** A pool is allocated at a declaration with 2x headroom. The spare half is for copy-and-hand-off.
- **Escaping values:** pool parameters are inferred inside a module. At public boundaries and block exits, values are copied out or the whole pool moves (probes 01 and 02).
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
- **Headers:** a ceremony for declaring signatures, purity and size contracts, optionally in a separate file (prior art: OCaml `.mli`, SPARK specs). Whether they also define boundaries for separate compilation is **undecided**. Start with contracts only.

## Open

- Column-precise source maps: per-expression markers, or a side table mapping rustc's column ranges.
- Headers as boundaries for separate compilation (still deferred).

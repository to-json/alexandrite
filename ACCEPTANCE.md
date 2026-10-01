# Acceptance tests: day-to-day Project Euler

**Goal:** someone can open an editor, write a Project Euler solution in Alexandrite the way they'd write it in Ruby, run it with one command, and get the right answer fast, with errors that make sense when they're wrong.

Problems are taken from the first few dozen, whose answers are widely published. The design decisions each test depends on are in `DESIGN.md`; the "needs phases" column refers to its build plan.

## The harness

Run it with `python3 acceptance/run.py` (`-k pe010` filters cases). It builds `alx` in release mode, runs every check below, and exits 0 only if all of them pass.


```
acceptance/
  run.py                 # runs every case in both modes, checks output and budgets
  cases/
    pe001.alx
    pe001.expected       # exact stdout
    pe001.budget         # optional: max ms per stage
    fixtures/names.txt   # data files some problems need
```

For each case, `run.py`:
1. **`alx run case.alx`**: a debug build, then runs it. (v0: the whole program is JIT-compiled in memory with Cranelift and run in-process; `alx build` without `--release` still produces a clang -O0 binary.) Checks stdout, and checks the **edit-to-answer** time (build plus run) against the budget.
2. **`alx run --release case.alx`**: release build, then runs it. Checks stdout, and checks the **run time** against a hand-written Rust reference (`refs/peNNN.rs`, `rustc -O`).
3. **Safety net:** builds the case with ASan/UBSan, and checks the Rust backend's output with rustc (the oracle). Both must be clean. A rustc rejection is a bug in our checker (DESIGN.md, Architecture).
4. **Negative cases** (`*.bad.alx` + `*.expected_error`): compilation must fail, and the first error must match the expected text, **file:line:column** included. Runtime failures (`*.expected_runtime`) give the exit status on the first line (`abort` or a number), then lines that must appear in stderr.

Global requirements that every case checks:
- **One file is a program.** `alx run file.alx` needs no manifest, no `main`, no project setup. Top-level statements run in order, as in a Ruby script.
- **No Rust or C in user-facing output,** whether in an error or a panic message.
- **Panics name the `.alx` line** (`#line` source maps).

## A1: Enumerable is the whole program

The everyday style: ranges, blocks, chains.

```
# pe001.alx
puts (1...1000).select { it % 3 == 0 || it % 5 == 0 }.sum

# pe002.alx: an infinite generator, made finite by .lazy
fibs = Enumerator.new { |y| a, b = 1, 2; loop { y << a; a, b = b, a + b } }
puts fibs.lazy.take_while { it <= 4_000_000 }.select(&:even?).sum

# pe006.alx
n = 100
puts (1..n).sum ** 2 - (1..n).sum { it * it }
```

| case | expected | budget |
|---|---|---|
| pe001 | `233168` | edit-to-answer < 300 ms |
| pe002 | `4613732` | edit-to-answer < 300 ms |
| pe006 | `25164150` | edit-to-answer < 300 ms |

**Pass if:**
- Output matches.
- The release build of pe001 allocates nothing: a fused pure chain into a scalar `sum` (counting allocator in the test runtime).
- pe002 terminates. `.lazy` over an unbounded generator stops at `take_while`, and the generator is a stackless coroutine (gen-block equivalent).

**Exercises:** inlined blocks, `it`, `&:sym`, fusion of pure chains, `.lazy`, external iterators, `**`. **Needs phases** 1–4 and 6.

## A2: Primes and the speed bar

The problems where Ruby is too slow and you'd normally switch languages. That's exactly why this project exists.

```
# pe010.alx
limit = 2_000_000
sieve = Array.new(limit, true)
sieve[0] = sieve[1] = false
(2..Int.sqrt(limit)).each { |i|
  next unless sieve[i]
  (i * i).step(limit - 1, i) { sieve[it] = false }
}
puts sieve.each_index.select { sieve[it] }.sum

# pe007.alx: reuses a helper from a local library file
require "lib/primes"   # `using` is taken by refinements
puts primes.lazy.drop(10_000).first
```

| case | expected | budget |
|---|---|---|
| pe007 | `104743` | release run ≤ 1.5× the Rust reference |
| pe010 | `142913828922` | release run ≤ 1.5× the Rust reference; debug edit-to-answer < 1 s |

**Pass if:**
- Output matches and the budgets hold.
- Indexing in the sieve's inner loop is bounds-checked in debug builds. In release builds, the checks the prover can discharge (`step` stays below `limit`, and the array size is `limit`) are removed. Count them in the emitted C.
- `lib/primes.alx` compiles once and is reused on later runs: its header is a separate-compilation boundary, and the second run doesn't rebuild it.

**Exercises:** mutable arrays, `step`, `each_index`, the prover removing checks, modules and headers, build caching. **Needs phases** 1–6.

## A3: Big numbers just work

Project Euler's bread and butter, and Ruby's `Integer` makes it painless. We get there with the per-file overflow flag.

```
# pe016.alx
#![overflow(promote)]
puts (2 ** 1000).digits.sum

# pe020.alx
#![overflow(promote)]
puts (1..100).reduce(:*).digits.sum

# pe025.alx
#![overflow(promote)]
a, b, i = 1, 1, 2
while b.to_s.size < 1000 { a, b, i = b, a + b, i + 1 }
puts i
```

| case | expected |
|---|---|
| pe016 | `1366` |
| pe020 | `648` |
| pe025 | `4782` |

Negative case: `pe020.bad.alx`, the same as pe020 without the flag.
- **Expected:** the program builds, then aborts at run time with `overflow at pe020.bad.alx:1:…` plus a hint to add `#![overflow(promote)]`.
- It must not wrap silently or print a wrong answer.

**Pass if:**
- Output matches.
- In promote mode, values that fit in a machine word stay unboxed. A promote-mode pe001 must run no more than 1.2× slower than its default-mode version.
- The negative case gives the expected abort.

**Exercises:** overflow modes, bignum in the C runtime (libtommath, vendored; only linked when `promote` is used; see DESIGN.md). **Needs phases** 1–4, plus bignum in the runtime.

## A4: Pure helpers, `pmap`, and the errors when you get it wrong

The longer problems, where you factor out helpers and want them fast.

```
# pe014.alx
#[pure] def collatz_len(n: Int) -> Int! {
  steps = 1
  while n != 1 { n = n.even? ? n / 2 : try (3 * n + 1); steps += 1 }
  steps
}
best = (1...1_000_000).to_a.pmap { try collatz_len(it) }.each_with_index.max_by(&:first)
puts best.last + 1

# pe004.alx
#[pure] def palindrome?(n: Int) -> Bool { s = n.to_s; s == s.reverse }
puts (100..999).flat_map { |a| (a..999).map { |b| a * b } }.select { palindrome?(it) }.max
```

| case | expected | budget |
|---|---|---|
| pe014 | `837799` | release run ≤ 1.5× a single-threaded Rust reference, *before* `pmap`'s speedup is counted |
| pe004 | `906609` | edit-to-answer < 500 ms |

Negative cases:

| file | change | expected first error |
|---|---|---|
| `pe014.bad1.alx` | `puts n` inside `collatz_len` | `` `#[pure] def collatz_len` can't do I/O: `puts` `` at the right line and column |
| `pe014.bad2.alx` | `try` removed from `3 * n + 1` | `` unproven arithmetic in `#[pure] def collatz_len`: `3 * n + 1` may overflow; use `try` or prove a bound `` |
| `pe004.bad.alx` | `palindrome?(it)` called with a `Str` | `` `palindrome?` expects Int, got Str `` at the call site, with no generated or C names in the message |

**Pass if:**
- Output matches.
- `pmap` actually runs in parallel: wall-clock time drops at least 2× on a machine with 4 or more cores.
- Every negative case's first error matches.

**Exercises:** `#[pure]`, proven-or-fallible, error sets with `try` inside a block, `pmap` on scoped threads, `flat_map` sizing, front-end diagnostics (probe 07). **Needs phases** 1–6 and 8 (for `pmap`).

## A5: Files, strings and sorting

The problems that come with a data file.

```
# pe022.alx
names = File.read("fixtures/names.txt").delete('"').split(",").sort
puts names.each_with_index.sum { |name, i| (i + 1) * name.bytes.sum { it - 64 } }

# pe008.alx: the 1000-digit number pasted as a heredoc
digits = <<~D.delete("\n").chars.map(&:to_i)
  7316717653133062491922511967442657474235534919493496983520312774506326239578318016984801869478851843
  …
D
puts digits.each_cons(13).map { it.reduce(:*) }.max
```

| case | expected |
|---|---|
| pe022 | `871198282` |
| pe008 | `23514624000` |

Negative case: `pe022.missing.alx` reads `fixtures/nope.txt`.
- **Expected:** a clean runtime error, `` File.read: no such file `fixtures/nope.txt` (pe022.missing.alx:1:9) ``, with exit code 1.
- Not an abort, and no C-level message.

**Pass if:**
- Output matches.
- Strings are UTF-8, and `bytes`, `chars`, `delete`, `split` and `sort` behave like Ruby on ASCII input.
- `each_cons(13)` allocates no windows: fused, each window a view into the array.
- The missing-file error reads as above.

**Exercises:** impure I/O with error sets at the top level, heredocs, strings, sorting, `each_cons`. **Needs phases** 1–6.

## A6: The day-to-day loop

Not one problem. These are what working on twenty problems in an evening feels like.

| check | how | pass if |
|---|---|---|
| **Edit-to-answer** | edit pe001 (change `1000` to `999`), rerun, 10 times | median < 300 ms (debug), on the machine from probe 10 |
| **Error round-trip** | introduce a typo (`selct`), rerun | the error names `selct`, suggests `select`, gives file:line:col, and arrives in < 200 ms |
| **Library reuse** | `pe003.alx` uses `lib/primes.alx` from A2 | a second native build doesn't rebuild the library (its header's hash is unchanged); `alx run` JITs the whole program in memory |
| **No setup** | a fresh directory containing only `pe001.alx` | `alx run pe001.alx` works, and creates nothing except, at most, a cache directory |
| **Answer check** | `alx run pe010.alx --expect 142913828922` | exit code 0 if the output matches and 1 otherwise, so solutions can be checked in a loop |
| **Oracle clean** | the CI job over all cases | rustc accepts every emitted `.rs`, and ASan/UBSan are clean |

## A7: Go-class workloads (the Benchmarks Game)

Alexandrite's capabilities follow Go's (GO-VS-RUBY.md). These cases are Benchmarks Game programs, single-threaded and at reduced size, written the way a Go programmer would write them, with Go, Rust and Ruby ports in `acceptance/{go,refs,ruby}/`.

```
# nbody.alx (excerpt): value structs, keyword construction, place updates
struct Body { x: Float, y: Float, z: Float, vx: Float, vy: Float, vz: Float, mass: Float }
bodies = [Body.new(mass: solar_mass), Body.new(x: 4.84143144246472090e+00, ...), ...]
bodies[i].vx -= dx * bodies[j].mass * mag
puts format("%.9f", energy(bodies))
```

| case | expected | budget |
|---|---|---|
| nbody (1,000,000 steps) | `-0.169075164` / `-0.169086185` (Go's output; at 1,000 steps it gives the published `-0.169087605`) | edit-to-answer < 300 ms; release ≤ 1.5x the Rust reference |

**Pass if:** output matches Go's exactly (same arithmetic, same order, Go's `%.9f`), on every backend: JIT, clang debug and release, the Rust oracle, and the browser.

**Exercises:** `Float` (IEEE, Go's printing and `fmt` verbs), `Math.sqrt`, value `struct`s with keyword construction and zero values, writes through places (`a[i].f op= v`), Go's literal-only Int→Float conversion.

## How far the build plan has to go

| tests | earliest the build plan delivers it |
|---|---|
| A1, A6 (except library reuse) | phases 1–4 plus a minimal phase 6 (Enumerable). **The first milestone.** |
| A2, A5 | phases 1–6 |
| A3 | adds bignum to the runtime |
| A4 | adds phase 8 (`pmap`) |
| A6 oracle row | phase 5 |

**License check (all cases):** every object linked into a case's binary comes from the runtime or from vendored code on DESIGN.md's runtime license allowlist. The CI job reads a license manifest for `rt/` and fails on anything else.

**The project counts as usable for day-to-day Project Euler when A1–A6 all pass on the reference machine.** A7 grows as Go-class features land.

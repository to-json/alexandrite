# Porting a Go std package to alx

The rules every std port follows (user decisions 2026-10-03, GO-VS-RUBY S1–S4, D35–D46). Read the existing packages too: `std/strconv`, `std/strings`, `std/bytes`, `std/time` and `std/encoding/json` are the reference ports.

## Done means

1. `std/<go import path>/` holds the package; the package name is the path's last element (`std/encoding/hex` → `hex`).
2. **Go's own tests, translated:** every test function and table in the package's `_test.go` files that exercises behaviour alx has, as `test "name" { ... }` blocks in `*_test.alx` files beside the package. Examples (`ExampleX`) become `example "name" { ... } outputs "..."`.
3. **Generated vectors** where outputs are deterministic: a throwaway Go program (in your scratch directory, not the repo) prints alx test code: `assert_eq f(input), "expected"` lines; check the output in as `vec_*_test.alx` with a first-line comment `# Generated from Go's <pkg> (...); do not edit by hand.` Hundreds to a few thousand assertions per package is normal.
4. Both pass: `alx test std/<path>` (JIT) and `alx test --release std/<path>` (C). Also `alx fmt --check` on your files.
5. The header comment of the main file maps Go's API to alx's and lists every difference from Go and why (see `std/strconv/strconv.alx`, `std/os/exec/exec.alx`).
6. Anything you skip is listed in the header with the reason. Skipping is only for what can't exist in alx (unsafe, reflection) — not for effort.

## The language, for porters

- **Names:** Go's names in snake_case (`HasPrefix` → `has_prefix`, `EncodeToString` → `encode_to_string`); predicates end in `?` when they return Bool and read as questions (`valid?` — follow existing packages); `pub` exports; a mutating method ends in `!` (`def write!(p: [Byte]) -> ~Int`).
- **Errors:** `pub error HexError { InvalidByte(b: Byte) ... def message -> Str { ... } }`. Functions that fail return `~T` (or `~T<HexError>`); `fail HexError.InvalidByte(b: c)` inside. Messages match Go's text exactly where Go has the error. `(T, error)` → `~T`; `(T, bool)` → `T?`.
- **`~` propagates and attaches to the next call:** `~f(x)`, `x.~m(y)`, `~(a * b)` (checked arithmetic), `~r` (a held Result). `~exec.command(..).output` is wrong — write `exec.command(..).~output`.
- **Optionals:** `T?`, `none`, `if v = opt { ... }`, `opt || default`, `opt?.m`.
- **Integers:** `Int` = I64; `I8..I64`, `U8..U64`, `Byte` = U8, `Rune` = I32. Arithmetic panics on overflow by default; use `+% -% *%` or put `#![overflow(wrap)]` as the **first line** of hash/crypto/compression files. `x.to_u32` checks, `x.as_u32` truncates (Go's `uint32(x)`). Shifts follow Go. Constants are exact (`1 << 63` fits a U64 declaration).
- **Collections:** `[T]` is a Go slice (shared storage, `a[lo...hi]`, `xs << v` appends), `[T; N]` fixed arrays, `[v; n]` fill, `Map[K, V]` insertion-ordered, `copy(dst, src)`. `xs + ys` concatenates; `xs.reverse` copies, `xs.reverse!` in place. `==` compares slices element-wise.
- **Strings:** immutable bytes; `s[i]` is a Byte, `s[a...b]` a substring, `s.size` bytes, `s.bytes`, `s.runes`, `Str.from_bytes(bs)`. Most Go `strings` functions are in `import "strings"`; Ruby sugar exists (`strip`, `start_with?`, `lines`, `s * n`) but prefer what the surrounding package does.
- **Tuples:** `(a, b)`, read with `t[0]`, destructure with `a, b = f()` (no parentheses on the left).
- **Structs:** fields + methods inside `struct Name { }`; `Name.new(field: v)`, omitted fields zero. Static methods `def self.from(...)`. Interfaces are structural (`interface Hash { def sum -> [Byte] }`).
- **Stateful types share state across copies (D36):** put the mutable fields in a one-element slice (`st: [State]`) so copies are the same object, as Go pointer receivers are. Constructors make them (`new_digest()`). See `bytes.Buffer`, `bufio.Reader`.
- **Generics:** `def max[T](xs: [T]) -> T`, bounds `T: like Int`. Blocks for function parameters: `sort_func(xs) { |a, b| ... }`; lambdas `->(x: Int) -> Int { }`; a lambda typed `-> ~T` may use `~`.
- **No reflection:** reflection-based APIs become compile-time derives (`#[derive(Json)]` in `compiler/src/derive.rs` is the model) or explicit protocols.
- **C FFI** exists (`extern def name(...) -> T = "csym"`) — use it only for OS access (as `os` does). Crypto, compression etc. are pure alx.
- **Concurrency:** `spawn { }`, `Chan[T]`, `select`, `Mutex[T]`, `Atomic[T]`, `t.wait`.
- **Tests:** `test "name" { assert cond, "why"; assert_eq got, want }`, `bench "name" { }`, `example "x" { puts ... } outputs "..."`. Test files are compiled with the package (private names visible).

## Gotchas found so far

- The std library is **embedded in the compiler at build time**: after editing `std/`, run `cargo build --release` before `alx test`. In a worktree, use *that worktree's* `target/release/alx`.
- `alx test` of a package whose test file uses backtick command literals imports `os/exec` twice — don't use backticks inside `std/os/exec` tests.
- Typed declarations `x: [T] = []` are real locals (they warn when unused).
- A `test` block is fallible: use `.unwrap` or `~` inside.
- A file's `#![...]` directive must be its first line.
- `reduce(init) { }` accumulates in init's type.
- `~f(a - 1, xs[i...j])` makes the argument arithmetic and slicing fallible too (ArithError/IndexError join your declared error set): compute arguments before the `~` call.
- `r.err` on a `~T<E>` is an open `Error`, not `E`; match it with `case e { Variant(..) => }` (qualified `E.Variant(..)` / `pkg.E.Variant(..)` works too).
- Inside a struct, calling a package function with the same name as one of the struct's methods calls the method: give the helper another name.
- Locals are function-scoped: the same name with different types in two branches is an error.
- A type that holds an `io.Reader`/`io.Writer` *interface value* can't itself be passed as one (interface values are closed sums, D12): make wrappers generic over the inner type (`Reader[R]`, like Rust's BufReader<R>), as encoding/hex, mime/quotedprintable and text/tabwriter do.
- No variadics: take a slice (`cmp.or([a, b, c])`).
- `[0; n]` and `c ? 1 : 0` adapt to a wanted Byte/U64 only in simple positions; use a typed local when in doubt.
- Big generated vector files: keep test blocks to ~100 asserts each and files to a few thousand asserts (release builds of huge files are slow).
- `"\x80"`-`"\xff"` escapes aren't allowed in string literals (Str is UTF-8 text): build such bytes with `Str.from_bytes([...])`.
- Known open compiler issues: docs/notes/port-issues.md.
- If you hit a compiler bug or a missing language feature, see "Language gaps" below.

## Language gaps

Decision: extend the language rather than bend the API. Keep each extension small, implement it on every backend (C via cgen, JIT, Rust oracle via rgen/prelude, wasm), add a test, and describe it (what, why, which package needed it) in your final report so it lands in GO-VS-RUBY.md. If the change is large or contentious, stop and report instead.

## Checklist for an agent

1. Read the Go source (`$(go env GOROOT)/src/<path>`) and its tests.
2. Port the package; translate the tests; generate vectors.
3. `cargo build --release && target/release/alx test std/<path> && target/release/alx test --release std/<path>`.
4. Run the std tests of packages you depend on or touched, and `alx fmt --check` your files.
5. Commit in your worktree (message: `std: <packages>` + summary). Report: files, test blocks and assertion counts, Go tests not translated (with reasons), differences from Go, language changes, compiler fixes.

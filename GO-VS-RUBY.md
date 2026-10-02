# Go vs Ruby: decision log

Alexandrite's **capabilities and use case follow Go** (a GC-less Go: services, tools, systems code), and Go's standard library will be ported later, minus the parts that only exist for Go's GC. **Ruby is a source of syntax and ergonomics**, not semantics. This file lists every place the two disagree that we can see coming, so each is decided once, deliberately, before code depends on it.

Status: **decided** (with who/when), **open** (recommendation given), or **current** (what v0 does today, not yet confirmed). Recommendations lean Go wherever the stdlib port depends on it.

## Numbers

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| N1 | How `puts`/`to_s` print a Float | `fmt.Println`: `100`, `1e+06`, `+Inf` | `100.0`, `1.0e+16`, `Infinity` | Go | Go | **decided** (user, 2026-10-01) |
| N2 | `sum` of Floats | plain loop | Kahan–Babuška compensated | plain | Go | **decided** (Go reference) |
| N3 | `format` / printf verbs | `%v %d %s %t %f %.Nf %e %g %x %q`, widths, flags; `%s` only for strings | Kernel#format (C-like; `%s` takes anything) | Go subset: `%v %d %s %t %f %.Nf` | Go verbs; grow with the `fmt` port | **decided** (Go reference) |
| N4 | Int and Float in one expression (`1 + x` where `x` is Float) | compile error unless the Int is an untyped constant; variables need `float64(i)` | implicit conversion | implicit (Ruby) | Go: literals adapt, Int *variables* need `.to_f` (an i64 above 2^53 silently loses precision) | **decided: Go** (user, 2026-10-01) |
| N5 | Constant arithmetic | exact, arbitrary precision at compile time (`0.1 + 0.2 == 0.3`; `-0.0` is `+0`) | ordinary runtime floats | runtime floats | keep runtime semantics for v0; revisit with the constant system | **decided: Go's exact constants** (user, 2026-10-01) |
| N6 | Integer `/` and `%` with negatives | truncate: `-7 / 2 == -3`, `-7 % 2 == -1` | floor: `-4`, `1` | floor (Ruby) | Go: ported code (hashing, encoding, time) assumes truncation | **decided: Go, truncate** (user, 2026-10-01) |
| N7 | Integer types | `int8..int64`, `uint8..uint64`, `uintptr`, `int` = 64-bit, `byte`, `rune` | one `Integer` (bignum on demand) | `Int` = i64; bignums via `#![overflow(promote)]` | add Go's sized and unsigned types; `Int` stays the default | **decided: Go's full set**: I8..I64, U8..U64, Int = I64, Byte = U8, Rune = I32; literals adapt, sized types convert explicitly (user, 2026-10-01) |
| N8 | Integer overflow | wraps silently | promotes to bignum | panics (or `try`); directives for `wrap` / `promote` | keep panic as default; add explicit wrapping operators (e.g. `+%`, as in Zig) so crypto/hash ports don't need a file-wide `#![overflow(wrap)]` | **decided: panic by default; wrapping operators `+%` `-%` `*%`** (plus `#![overflow(wrap)]` per file) (user, 2026-10-01) |
| N9 | Float → Int | truncates; NaN or out of range is implementation-defined | `FloatDomainError`, or a bignum | truncates; panics on NaN, ±Inf and out of range | keep the panic (safety over Go's silence) | current |
| N10 | `**` power operator | none (`math.Pow`) | `**` | `**` on Int | keep `**`; add `math.pow` with the stdlib | current |

## Strings and text

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| S1 | `s.size` | bytes (`len(s)`) | characters | bytes | Go; characters via `s.chars.size` / a rune count | current |
| S2 | Mutability | immutable `string`; build with `strings.Builder` / `[]byte` | mutable `String`, `<<` | immutable | Go | current |
| S3 | `byte` and `rune` types | yes | no (Integers) | no | add `Byte` and `Rune` (needed for the stdlib) | **decided: Go model**: `s[i]` is a Byte, `s.bytes`, `s.runes`, `s.size` in bytes (user, 2026-10-01) |
| S4 | String interpolation `"#{x}"` | none (`fmt.Sprintf`) | yes | no | add it: pure syntax, lowers to `format` with `%v` | **decided: add `"#{x}"`**, lowering to `%v` formatting (user, 2026-10-01) |
| S5 | Symbols `:name` | none | yes | only `&:sym` block shorthand | keep it to that | current |
| S6 | Invalid UTF-8 | strings are arbitrary bytes | encoding errors | arbitrary bytes | Go | current |

## Collections

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| C1 | Array semantics | arrays are values; **slices are views sharing a backing array**; APIs mutate through slices (`sort`, `copy`, `io.Reader.Read(p []byte)`) | references to one growable array | growable arrays with **value semantics** (copied on assign and on pass) | Go's split: owned growable `Array` plus a borrowed slice view (`a[lo...hi]`) that functions can mutate through; pools make view lifetimes checkable. Biggest decision here: it shapes every stdlib signature | **decided: both, as in Go** (user, 2026-10-01): fixed-size value arrays *and* slices, designed to make sense together. Design to be written up and approved before implementation |
| C2 | Hash / map | `map`, unordered (randomized iteration) | `Hash`, insertion-ordered | none yet (DESIGN.md says insertion-ordered) | insertion-ordered (deterministic output; no Go code depends on the randomness); name it `Map` or `Hash` | **decided: insertion-ordered `Map[K, V]`**, literal `{k => v}` (user, 2026-10-01) |
| C3 | nil vs zero values | zero values for every type; `nil` for pointers/slices/maps/interfaces | `nil` everywhere | zero values internally; `T?` planned | Go zero values; `T?` instead of nil pointers | **decided: no nil anywhere; Go zero values + `T?`** (checked narrowing, `||`, `?.`, `none`) (user, 2026-10-01) |
| C4 | Struct construction | `Body{X: 1, Mass: m}`, omitted fields zero | `Struct.new(1, 2)` or keywords | `Body.new(positional...)` | `Body.new(x: 1.0, mass: m)` with omitted fields zero (Go semantics, Ruby keyword look); positional allowed too | **decided: keywords + zero values, positional also allowed** (user, 2026-10-01) |
| C5 | Struct semantics | values (copied) | references | values | Go | current |

## Functions, methods, errors

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| F1 | Methods on structs | receivers declared outside the type; value vs pointer receivers | `def` inside the class; everything a reference | none | methods inside `struct Name { ... }`; a mutating method marked like Ruby's `!` (`def move!`) = Go pointer receiver | **decided: methods inside `struct { }`; `def name!` may mutate self (pointer receiver), plain `def` gets a copy** (user, 2026-10-01) |
| F2 | Polymorphism | structural interfaces, satisfied implicitly | duck typing | none (DESIGN.md: traits, inferred `dyn`) | Go-style structural interfaces (`io.Reader` ports directly) | **decided: structural interfaces with default methods** (user, 2026-10-01) |
| F3 | Errors | `error` values (interface), `(T, error)` returns, wrapping (`%w`, `errors.Is/As`) | exceptions | `T!` + `try`; one global `Err` | keep `T!`/`try` syntax; make the error type Go-like (interface, wrapping) so `(T, error)` ports to `T!` | **decided: Rust-shaped, layered** (user, 2026-10-01): closed inferred sets inside packages, every error implements `Error` (message, cause), open `~T<Error>` at boundaries; Results are values (`map`, `ok`, `err`, `unwrap_or`, `unwrap`); io/bufio follow Rust's std shape (EOF = `0`/`none`, `write_all`, `read_exact`). Spelling: see E1–E4 |
| F4 | Multiple return values | yes | arrays + destructuring | tuples + multi-assign | tuples are Go's multiple returns | current |
| F5 | Cleanup | `defer` | `ensure`, blocks | none | `defer` | **decided: `defer`, block-scoped** (user, 2026-10-01) |
| F6 | Panics | `panic` / `recover` (net/http recovers per request) | `raise` / `rescue` | abort the process | abort for now; per-task panic isolation later, so one bad request can't kill a server | **decided: a panic kills its task, not the process**; main still aborts; no general `recover` (user, 2026-10-01) |
| F7 | Closures | func literals capture by reference | blocks | non-escaping blocks, inlined; explicit lambdas escape | keep | current |

## Concurrency

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| K1 | Model | goroutines, channels, `select`, `sync`, `context` | threads, fibers, Ractors | `pmap`; DESIGN.md plans CSP (`spawn`, `concurrently`) with async inferred | Go's model and names (channels, `select`, `WaitGroup`, `context`), with DESIGN.md's inferred async | **decided: Go's model, Ruby-ish spelling**: `spawn {}`, `Chan[T]`, `ch << v`, `ch.recv`, `select { when ... }`, `Context` (user, 2026-10-01) |

## Program structure and tooling

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| P1 | Packages | directory = package, `import "path"`, exported = Capitalized | `require` files, modules | `require "file"` | Go's directory packages and import paths; export with an explicit `pub` (Capitalized already means a type) | **decided: directory packages, `import "path"`, export with `pub`** (`require` goes away) (user, 2026-10-01) |
| P2 | Naming of ported APIs | `strings.HasPrefix` | `start_with?` | snake_case | transliterate Go names to snake_case (`strings.has_prefix`); `?` on predicates | **decided: snake_case transliteration**, `?` on predicates, method sugar where natural (user, 2026-10-01) |
| P3 | Unused variables / imports | compile errors | warnings at most | silent | warnings | **decided: warnings; errors under `--strict` and in `alx test`**; `_` silences (user, 2026-10-01) |
| P4 | Formatter | `gofmt` (one style, no options) | rubocop (configurable) | none | `alx fmt`, gofmt-style | **decided: `alx fmt`, no options** (user, 2026-10-01) |
| P5 | Tests | `go test`, `_test.go`, table tests, benchmarks | minitest / rspec | acceptance harness only | `alx test`, `_test.alx`, `go test` conventions including benchmarks | **decided: Go's conventions**: `alx test`, `*_test.alx`, `test "name" {}`, `bench`, output-checked examples (user, 2026-10-01) |
| P6 | Dependencies | `go.mod`, minimal version selection, module proxy | gems, bundler | none | Go's module model | **decided: Go's module model**: `alx.mod`, URL imports, MVS, `alx.sum` (user, 2026-10-01) |
| P7 | Truthiness | only `bool` | `nil`/`false` falsy | only `Bool` | Go | current |

## Taste (not Go-vs-Ruby, but likely points of disagreement)

| # | Question | Decision |
|---|---|---|
| T1 | Function keyword | `def`; **`fn` and `ƒ` mean `#[pure] def`** (user, 2026-10-01) |
| T2 | Return values | implicit last expression; `return` for early exits (user, 2026-10-01) |
| T3 | Parameter types | untyped = generic with an inferred bound inside a package; `pub` functions spell their types (user, 2026-10-01) |
| T4 | Operator overloading | through standard interfaces only: `==`, `<=>`, `+ - * /`, `[]`; no custom operator symbols (user, 2026-10-01) |
| T5 | Sum types | `enum` with payloads, exhaustive matching; payload-free enums cover Go's iota (user, 2026-10-01) |
| T6 | Matching | `case x { pattern => value }`, an expression; values, ranges, enum patterns, `_` (user, 2026-10-01) |
| T7 | Loops | add `for x in xs { }` (statement; `break`/`next`/`return` as in Go's for-range) alongside Enumerable (user, 2026-10-01) |
| T8 | Arrays | C1: `[T]` is a Go slice (shared storage, reslicing, append); `[T; N]` a fixed-size value array whose slices view its storage (user, 2026-10-01) |

## Errors and optionals: spelling (user, 2026-10-01)

The logic: **fallibility is a prefix** ("approximately do this", "you'll probably get this"), **optionality is a suffix** (it matches `?` on Bool methods). `!` means only "mutates".

| # | Form | Meaning |
|---|---|---|
| E1 | `~T` / `~T<ParseError>` / `~T<ParseError \| IoError>` / `~T<Error>` | fallible T: inferred set / declared set / union / open (any error). `< >` is reserved for error sets; generics use `[ ]` (Go, RBS) |
| E2 | `T?` | optional T (no nil anywhere). `~T?<E>` = may fail, else maybe a T; `~T<E>?` = maybe a fallible T. `~` takes the type to its right including its `?` |
| E3 | `~f(x)`, `x.~m(y)`, `~(a * b)`, `~r` | propagate: the `~` attaches to the call whose name follows it (not the whole chain), or to a parenthesized expression or a held Result. A fallible call in a chain without its own `~` is an error that says where to put it |
| E4 | no `~` | you hold the Result value (`~T`); unused Results warn |
| E5 | Go's `~int` approximation constraint | spelled `like Int` in generic bounds, so `~` keeps one meaning |
| E6 | `rescue { \|e\| ... }`, `case e { Variant(x) => ... }`, `fail E`, `e.wrap("ctx")` | handling, exhaustive over closed sets, `_` required on open ones |

## Derived decisions (made while building; flagged for review)

| # | Question | Decision | Milestone |
|---|---|---|---|
| D1 | Operator precedence | Ruby's order with Go's operators slotted in (`\| ^` < `& &^` < `<< >>` < `+ -` < `* / %`); the Go port parenthesizes where Go's order differs | M1 |
| D2 | Shifts | Go: no overflow panic, count >= width gives 0 (or -1), negative count panics | M1 |
| D3 | Conversions | `to_X` checked (panics), `as_X` truncates like Go's `X(v)` | M1 |
| D4 | Constants | Ruby spelling (`NAME = expr` at the top level, capitalized), Go's exact semantics | M1 |
| D5 | Empty collections | `max`/`min`/`first`/`reduce` panic on empty (Go's `slices.Max`); lookups that commonly miss return `T?`: `find`, `m[k]`, `m.delete(k)` | M3 |
| D6 | Subslice capacity | `a[lo..hi]` shares storage but has no spare capacity, so `<<` onto a subslice copies instead of overwriting the parent's next element (Go's `append` aliasing footgun); `a[lo..hi]` is inclusive, `a[lo...hi]` exclusive (Ruby ranges), either end may be left out | M3 |
| D7 | `[v; n]` fill | a fill holding storage (a slice, a fixed array, a struct with one) is evaluated once per element, so `[[0; 3]; 3]` is three independent rows (Rust's `vec![v; n]` clones; Go's `make` gives nil rows) | M3 |
| D8 | Map printing and keys | `puts m` prints Go's `map[k:v ...]` in insertion order (Go sorts); keys are integer types, Str or Bool for now; `m[k] op= v` reads a missing key as V's zero (Go) | M3 |
| D9 | `copy(dst, src)` | Go's: copies `min(len)` elements, returns the count, overlapping slices behave like memmove | M3 |
| D10 | Fields inside methods | read bare (`x`, Ruby's attribute style); writes spell `self.x = ...` and only a `!` method may write. Assigning to a bare field name is an error rather than a new local, so it can't silently shadow | M4 |
| D11 | Receivers | a `!` method needs a variable (or field/element path) as its receiver, Go's addressability rule; a plain method gets a copy and assigning to `self` in one is an error pointing at `!` | M4 |
| D12 | Interface values | a closed sum over the program's implementors (whole-program compile), so no boxing or GC; a struct/enum converts implicitly where the interface is wanted; interface methods can't be `!` yet | M4 |
| D13 | Equality and ordering | structs, tuples and enums compare field by field (Go's comparable types); slice/map fields aren't comparable unless the type defines `def ==`; `def <=>` gives `< <= > >=` | M4 |
| D14 | Printing | Go's `%v`: slices `[1 2]`, structs `{1 2}`, maps `map[k:v]`; enums print as `Circle(1)` / `Red`; a type's `to_s` is used everywhere it's printed, nested too (Go's `Stringer`) | M4 |
| D15 | Generic instances | written `Stack[Int]`; type arguments inferred from constructor values or the declared type, else spelled `Stack[Int].new`; bounds `T: Shape` and `T: like Int` are checked per instance | M4 |
| D16 | Lambdas | `->(x: T) -> R { }`, type `(T) -> R`, called `f(x)` or `f.call(x)`; captures are **copies** for now (slices and maps still share), revisited with the memory model; `return` returns from the lambda | M4 |
| D17 | Error representation | `error` types are enums; an `Error` value is one sum over the program's error types (closed world, no boxing), carrying the location where it was raised and any `wrap` context. No stack traces | M5 |
| D18 | Builtin errors | `IoError { NotFound(path), Failed(path) }`, `ArithError { Overflow, DivZero }` (from `~(arith)`), `Failure { Msg(message) }` (from `fail "text"`, Go's `errors.New`) | M5 |
| D19 | Messages | an error's message is the value printed (`Missing(port)`) unless its type defines `def message`; `wrap("ctx")` prefixes `ctx: `; uncaught at the top level: `error: <message> (<file:line:col>)`, exit 1 | M5 |
| D20 | Error sets | `~T` infers the set; `~T<A \| B>` is checked against what the body can fail with; `~T<Error>` is open. `case` over an Error matches variants unqualified (or a type name) and needs `_` for now | M5 |
| D21 | Results without `~` | a fallible call without `~` is a `~T` value, also at the top level (no implicit exit); `~r` on a held value propagates; `ok`, `err`, `ok?`, `err?`, `unwrap`, `unwrap_or`, `rescue { \|e\| }`. A `pmap` block using `~` propagates the first failing element's error after all workers finish | M5 |
| D22 | Package names | a package's declarations are qualified by its import path; a file reaches them as `alias.name` (alias = the path's last segment, or `import alias "path"`). Only `pub` declarations cross packages, methods included (`pub def` inside a struct); fields of a public type are visible (no field-level `pub` yet). `pub def` parameters must be typed | M6 |
| D23 | Separate compilation | a package compiles separately (cached object + header) only when it's plain: no imports, interfaces, error or generic types, and its exports are typed, non-generic, non-fallible defs; anything else is checked with the program (interfaces, errors and lambdas have whole-program layouts) | M6 |
| D24 | Refinements | `refine Name for Type { def ... }`; `using Name` applies to the rest of the enclosing block (the rest of the file at the top level, including defs below it); an active refinement wins over the type's own methods (Ruby); `pub refine` exports it | M6 |
| D25 | Modules | `alx.mod`: `module`, `require`, `replace`, Go's minimal version selection over dependencies' alx.mod files; modules come from `replace` directories or the module cache (`$ALX_MODCACHE`, else `~/.alx/mod`). No downloading and no `alx.sum` yet | M6 |

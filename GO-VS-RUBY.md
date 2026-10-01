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
| N5 | Constant arithmetic | exact, arbitrary precision at compile time (`0.1 + 0.2 == 0.3`; `-0.0` is `+0`) | ordinary runtime floats | runtime floats | keep runtime semantics for v0; revisit with the constant system | open |
| N6 | Integer `/` and `%` with negatives | truncate: `-7 / 2 == -3`, `-7 % 2 == -1` | floor: `-4`, `1` | floor (Ruby) | Go: ported code (hashing, encoding, time) assumes truncation | **decided: Go, truncate** (user, 2026-10-01) |
| N7 | Integer types | `int8..int64`, `uint8..uint64`, `uintptr`, `int` = 64-bit, `byte`, `rune` | one `Integer` (bignum on demand) | `Int` = i64; bignums via `#![overflow(promote)]` | add Go's sized and unsigned types; `Int` stays the default | open |
| N8 | Integer overflow | wraps silently | promotes to bignum | panics (or `try`); directives for `wrap` / `promote` | keep panic as default; add explicit wrapping operators (e.g. `+%`, as in Zig) so crypto/hash ports don't need a file-wide `#![overflow(wrap)]` | open |
| N9 | Float → Int | truncates; NaN or out of range is implementation-defined | `FloatDomainError`, or a bignum | truncates; panics on NaN, ±Inf and out of range | keep the panic (safety over Go's silence) | current |
| N10 | `**` power operator | none (`math.Pow`) | `**` | `**` on Int | keep `**`; add `math.pow` with the stdlib | current |

## Strings and text

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| S1 | `s.size` | bytes (`len(s)`) | characters | bytes | Go; characters via `s.chars.size` / a rune count | current |
| S2 | Mutability | immutable `string`; build with `strings.Builder` / `[]byte` | mutable `String`, `<<` | immutable | Go | current |
| S3 | `byte` and `rune` types | yes | no (Integers) | no | add `Byte` and `Rune` (needed for the stdlib) | open |
| S4 | String interpolation `"#{x}"` | none (`fmt.Sprintf`) | yes | no | add it: pure syntax, lowers to `format` with `%v` | open |
| S5 | Symbols `:name` | none | yes | only `&:sym` block shorthand | keep it to that | current |
| S6 | Invalid UTF-8 | strings are arbitrary bytes | encoding errors | arbitrary bytes | Go | current |

## Collections

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| C1 | Array semantics | arrays are values; **slices are views sharing a backing array**; APIs mutate through slices (`sort`, `copy`, `io.Reader.Read(p []byte)`) | references to one growable array | growable arrays with **value semantics** (copied on assign and on pass) | Go's split: owned growable `Array` plus a borrowed slice view (`a[lo...hi]`) that functions can mutate through; pools make view lifetimes checkable. Biggest decision here: it shapes every stdlib signature | **decided: both, as in Go** (user, 2026-10-01): fixed-size value arrays *and* slices, designed to make sense together. Design to be written up and approved before implementation |
| C2 | Hash / map | `map`, unordered (randomized iteration) | `Hash`, insertion-ordered | none yet (DESIGN.md says insertion-ordered) | insertion-ordered (deterministic output; no Go code depends on the randomness); name it `Map` or `Hash` | open |
| C3 | nil vs zero values | zero values for every type; `nil` for pointers/slices/maps/interfaces | `nil` everywhere | zero values internally; `T?` planned | Go zero values; `T?` instead of nil pointers | open |
| C4 | Struct construction | `Body{X: 1, Mass: m}`, omitted fields zero | `Struct.new(1, 2)` or keywords | `Body.new(positional...)` | `Body.new(x: 1.0, mass: m)` with omitted fields zero (Go semantics, Ruby keyword look); positional allowed too | **decided: keywords + zero values, positional also allowed** (user, 2026-10-01) |
| C5 | Struct semantics | values (copied) | references | values | Go | current |

## Functions, methods, errors

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| F1 | Methods on structs | receivers declared outside the type; value vs pointer receivers | `def` inside the class; everything a reference | none | methods inside `struct Name { ... }`; a mutating method marked like Ruby's `!` (`def move!`) = Go pointer receiver | open |
| F2 | Polymorphism | structural interfaces, satisfied implicitly | duck typing | none (DESIGN.md: traits, inferred `dyn`) | Go-style structural interfaces (`io.Reader` ports directly) | open |
| F3 | Errors | `error` values (interface), `(T, error)` returns, wrapping (`%w`, `errors.Is/As`) | exceptions | `T!` + `try`; one global `Err` | keep `T!`/`try` syntax; make the error type Go-like (interface, wrapping) so `(T, error)` ports to `T!` | open |
| F4 | Multiple return values | yes | arrays + destructuring | tuples + multi-assign | tuples are Go's multiple returns | current |
| F5 | Cleanup | `defer` | `ensure`, blocks | none | `defer` | open |
| F6 | Panics | `panic` / `recover` (net/http recovers per request) | `raise` / `rescue` | abort the process | abort for now; per-task panic isolation later, so one bad request can't kill a server | open |
| F7 | Closures | func literals capture by reference | blocks | non-escaping blocks, inlined; explicit lambdas escape | keep | current |

## Concurrency

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| K1 | Model | goroutines, channels, `select`, `sync`, `context` | threads, fibers, Ractors | `pmap`; DESIGN.md plans CSP (`spawn`, `concurrently`) with async inferred | Go's model and names (channels, `select`, `WaitGroup`, `context`), with DESIGN.md's inferred async | open |

## Program structure and tooling

| # | Question | Go | Ruby | Alexandrite now | Recommendation | Status |
|---|---|---|---|---|---|---|
| P1 | Packages | directory = package, `import "path"`, exported = Capitalized | `require` files, modules | `require "file"` | Go's directory packages and import paths; export with an explicit `pub` (Capitalized already means a type) | open |
| P2 | Naming of ported APIs | `strings.HasPrefix` | `start_with?` | snake_case | transliterate Go names to snake_case (`strings.has_prefix`); `?` on predicates | open |
| P3 | Unused variables / imports | compile errors | warnings at most | silent | warnings | open |
| P4 | Formatter | `gofmt` (one style, no options) | rubocop (configurable) | none | `alx fmt`, gofmt-style | open |
| P5 | Tests | `go test`, `_test.go`, table tests, benchmarks | minitest / rspec | acceptance harness only | `alx test`, `_test.alx`, `go test` conventions including benchmarks | open |
| P6 | Dependencies | `go.mod`, minimal version selection, module proxy | gems, bundler | none | Go's module model | open |
| P7 | Truthiness | only `bool` | `nil`/`false` falsy | only `Bool` | Go | current |

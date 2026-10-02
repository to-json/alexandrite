# Roadmap: Go parity, errors, refinements

Goal of this push (user, 2026-10-01): **finish the language** to Go parity, plus DESIGN.md's error system and refinements, so the next step can be porting Go's standard library. The memory model (pools, the move/region checker) is a separate conversation *after* this push; until then everything lives in v0's program-lifetime region.

Every decision these milestones implement is in `GO-VS-RUBY.md`. Each milestone ends with: acceptance cases (`acceptance/cases/`), all backends agreeing (JIT, clang debug/release, Rust oracle; the browser where the feature makes sense there), `run.py` green, a milestone doc in `docs/milestones/`, a commit, and an update to the doc site.

| # | Milestone | Contents |
|---|---|---|
| M1 | **Numbers** | `I8..I64`, `U8..U64`, `Byte`, `Rune` (Int = I64); hex/octal/binary literals; bitwise `& \| ^ << >> &^` and unary `^`; wrapping `+% -% *%`; explicit conversions (`x.to_u32`); per-type overflow checks; Go's exact constants and top-level `const` (visible inside defs) |
| M2 | **Everyday syntax** | `for x in xs`; `case x { pat => v }` (values, ranges, `_`); `"#{x}"` interpolation; block-scoped `defer`; `fn` / `ƒ` = `#[pure] def`; `else if` |
| M3 | **Collections and absence** | C1: `[T]` slices (shared storage, `a[lo...hi]`, append, `copy`), `[T; N]` fixed arrays; Str as bytes (`s[i]` is a Byte, `bytes`, `runes`); ordered `Map[K, V]` with literals; `T?` with narrowing (`if x = e`), `\|\|`, `?.`, `none`; lookups, `find`, `first`, `max` return `T?` |
| M4 | **Types** | methods inside structs (`def m!` mutates); `enum` with payloads and exhaustive `case`; structural `interface`s with default methods; operator overloading through interfaces; explicit generics `[T]` on defs and structs (`like Int` bounds); escaping lambdas `->(x) { }` |
| M5 | **Errors** | `error` declarations; `~T`, `~T<E>`, `~T<Error>`; `~` propagation (replaces `try`); inferred closed sets; Results as values; `rescue`, `fail`, `case` over errors, `wrap`/`cause`; printing |
| M6 | **Packages and refinements** | directory packages, `import "path"`, `pub`; `alx.mod`; `refine` / `using`, lexically scoped |
| M7 | **Concurrency** | `spawn`, `Chan[T]`, `select`, waiting, `Context`; a panic kills its task, not the process |
| M8 | **Tooling** | `alx fmt`; `alx test` (`*_test.alx`, `test`, `bench`, output-checked examples); unused warnings and `--strict` |

**Status (2026-10-01): all eight milestones are done** — see `docs/milestones/` and the notebook. Next: the memory model (pools, the move/region checker), then porting Go's standard library.

Order is by dependency: numbers and syntax first (small, everything uses them), then collections (C1 changes array semantics under everything), then types, errors (which use enums and interfaces), packages, concurrency (which uses all of it) and tooling.

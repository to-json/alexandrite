# Compiler issues found while porting std

Open problems agents reported; each gets fixed in a compiler round between waves.

| # | Found by | Problem | Status |
|---|---|---|---|
| 1 | text/tabwriter | JIT gives every temporary its own stack slot (`jit.rs` `slot()`), never reused: deep call chains need 130-160 KB of a 256 KB task stack; a 100-call test block overflowed on the JIT only | fixed: slots are pooled and reused per statement |
| 2 | text/scanner | a closure capturing a struct that the closure is stored in crashes the compiler (Rust stack overflow) instead of a diagnostic | fixed: a diagnostic (a closure holds its captures by value; capture a @handle or pass a parameter) |
| 3 | encoding/csv | qualified enum patterns (`case e { csv.CsvError.FieldCount(s, l, c, rec) => }`) don't bind their variables; unqualified ones do | fixed: `Type.Variant` and `pkg.Type.Variant` patterns (the type part narrows error variants) |
| 4 | text/scanner | `((A) -> B)?` (optional function type) doesn't parse | fixed: `(T)` groups a type |
| 5 | text/scanner | lambda allocations: fixed for program-region placements only; other placements unverified | partly fixed |
| 6 | net/url | `Str.runes` / `for r in s.runes` on invalid UTF-8 is wrong (`"\x80A".runes` gives `[1]`, Go `[0xFFFD, 65]`): `alx_str_charlen`/`decode_rune` don't validate | fixed: charlen validates by Go's rules (all backends) |
| 7 | net/url | a ~800-assert test block overflowed the JIT task stack (before #1's fix) | fixed by #1 (a 1000-assert block passes) |
| 8 | containers | `opt?.field || none` gets the wrong type for `none` (JIT and C) | fixed: an optional default unifies with the value's type |
| 9 | containers | `format` has no integer precision (`%.2d`) | fixed: `%.Nd` (and x/o/b) as Go: minimum digits, 0 flag ignored, `%.0d` of 0 empty |
| 10 | containers | static methods on generic structs (`def self.make(n) -> R[T]`) report "is recursive" | fixed: a static method is generic over its type's parameters (`R.make(3, v)`, `R[Byte].make(2, 7)`) |
| 11 | containers | a generic function can't take its type only from the declared result (`x: R[Int] = mk(3)`); no explicit type arguments on calls | fixed: parameters the arguments don't decide come from the wanted type (declarations, returns, arguments); `R[T].static` gives them explicitly. Explicit `f[T](..)` on plain defs: not added (the wanted type covers the uses so far) |
| 12 | containers | `==` on optionals isn't allowed | fixed: `T? == T?` and `T? == T` (the plain side is wrapped) |
| 13 | hash | interface method result types aren't covariant (no `Cloner` interface) | fixed: a struct/enum result satisfies an interface result it implements (wrapped at dispatch); fallible covariant results (`-> ~Iface`) not yet |
| 14 | hash | an array literal mixing struct types doesn't convert to `[Iface]` even when declared | fixed: array literals coerce element-wise |
| 15 | hash | `alx test` on the JIT accepts unused imports, `--release` rejects them | not reproduced (both agree; std packages skip the check) |
| 16 | hash | maphash's `getentropy` FFI on wasm | by design: the browser provides only the time/sleep externs and refuses programs calling other FFI with an error |
| 17 | (main) | an array literal `[128, 65]` doesn't adapt to a `[Byte]` parameter (`Str.from_bytes([128, 65])`); a typed local works | fixed: array literals coerce element-wise; C declares array types used only by literals |
| 18 | (main) | a keyword (`next`) can't name a struct field or a named argument | fixed: keywords name fields (`next: T`, `N.new(next: x)`, `self.next`); fmt treats `.kw` as a value |
| 19 | regexp | `p.push!(p.new_regexp!(op))` lost the argument's effect on the receiver (index out of bounds) | fixed: `!` call arguments are evaluated before the receiver is read |
| 20 | regexp | `.err` on a `~T<ParseError>` is an open `Error`: fields only via `case e { Syntax(code, expr) => }` | open (design) |
| 21 | regexp | `syntax.OP_ANY_CHAR` in a case arm parsed as a qualified variant (after #3) | fixed: a prefix without a type part is a value |
| 22 | crypto | `hs[i]()` (calling an indexed function value) doesn't parse | open |
| 23 | crypto | `case` arms aren't converted to a wanted interface type | open |
| 24 | crypto | `"#{e}"` can't interpolate a plain enum value | open |
| 25 | crypto | `[0xee.as_u8, 1, 1]`: later elements don't take U8 from the first | fixed (D57: array literals take an earlier element's type) |
| 26 | crypto | a `[T; N]` local is heap-allocated, not a stack array | open (perf) |
| 27 | crypto | HMAC can't ask a generic `H` whether it's marshalable, so no saved keyed state: PBKDF2 3.8x Go | open (perf) |
| 28 | math/big | no add-with-carry: big Mul/String ~2x Go | open (perf) |
| 29 | math/big | tuple-returning functions get region mark/reset even when they don't allocate (`bits.add64` slow in loops) | partly fixed (D57: storage-free values) |
| 30 | math/big | no mutable package globals (caches like Go's divisor table) | open (design) |
| 31 | math/big | `format` has no `%g` | fixed: `%g` (shortest, as Go); `%.Ng` and `%G` not yet |
| 32 | math/big | a main file can't declare its own `Int`/`Float` (only packages can) | open (by design for now) |
| 33 | compress | `Gen.new(st: [State.new(r: r)])` inside a generic constructor sometimes can't infer the type parameter ("can't infer `R`"), depending on the instantiating program (base64.new_decoder over an os.File); explicit `Gen[R].new` / `State[R].new` works. Also a generic def can't infer `R` from a `pkg.Gen[R]` parameter | open (worked around: explicit type arguments; base64 fixed) |
| 34 | compress | calling a `!` method on self (or a field path) copies the whole receiver struct in and out of a one-element cell per call; with big structs (an `Error?` field is hundreds of bytes) hot `!` helpers cost more than their work (flate's decoder was 2.5x slower until its hot loop used locals) | open (perf) |
| 35 | compress | `b[i] \| b[i+1] << 8 ...` keeps 8 bounds checks (wrapping adds hide the relation); indexing an 8-byte view `w = b[i...i+8]` lets clang drop them (15x on a load64 microbenchmark) | open (perf; workaround documented) |
| 36 | compress | a method without a declared result whose last statement is an assignment returns Int, so an early bare `return` then fails "returns nil, but its last expression is Int" | open (workaround: end with `nil`) |
| 37 | compress | `alx test` ran tests in the caller's directory, so `testdata/` paths failed | fixed: tests run in the package directory (JIT and --release), as Go's do |
| 38 | math/cmplx | the C backend let clang contract `a*b - c` into an FMA on arm64 (`0.1*10.0 - 1.0` gave 5.55e-17 in `--release`, 0 on the JIT) | fixed: C is compiled with `-ffp-contract=off` (Go's amd64 results; no backend fuses) |
| 39 | math/cmplx | std/math's exp/log/sin/hypot/... were libm (Apple's natively, Rust's `libm` in wasm): an ulp off Go's, which broke Go's own cmplx tests (asinh), and the backends disagreed | fixed: the elementary functions are Go's portable code in plain alx (std/math/elem.alx), bit-exact with Go (6280 generated vectors) |
| 40 | math/cmplx | the committed web/test.mjs doesn't provide the `rt.ret` global the committed wasmgen imports (LinkError on every case); the main checkout has uncommitted fixes (run.js) | open (environment; a patched copy of the harness passes the complex cases) |
| 41 | math/rand | a `!` call through a field or interface value (`self.src.uint64!`) copies the receiver into a fresh one-element cell and back on every call (~45 ns): Rand is 15-20x slower than Go | open (perf: `call_cell` could skip receivers whose state is already in shared slices) |
| 42 | math/rand | a lambda passed to a `!` method needs typed parameters (`mutating_call` doesn't pass the parameter types down) | open |
| 43 | math/rand | the top-level functions read getentropy per call (no mutable package state for a global source, #30): ~1.5 µs per value, and Go's deprecated `Seed` can't make them deterministic (a no-op, Go 1.24's default); the browser refuses getentropy (#16) | open (design) |
| 44 | sync | a generic call's block that only decides the result type (`once_value { 42 }`) was refused; type parameters weren't bound through function or tuple types | fixed: an open result type is decided by the block's body; `bind_tparams` walks `Fn` and tuples |
| 45 | sync | `x: pkg.T[K, V] = pkg.make` (wanted type from another package) didn't infer the type parameters | fixed: a qualified instance matches its unqualified declaration |
| 46 | sync | a `case`/`if` arm ending in a statement (`ref[k] = v if c`) next to an arm with an optional value was silently dropped (lifting the arms popped the statement); a mismatched arm got the optional's type (C and JIT miscompiled) | fixed: only a trailing value is lifted; a value that doesn't become the optional keeps its type (the `if` is a statement or an error) |
| 47 | sync | an interface value counted as shared storage for tasks even when no implementor has any (a Cond holding a Locker couldn't go to two tasks) | fixed: an interface shares iff an implementor does (fixpoint over the implementors) |
| 48 | log | no way to copy a struct holding shared storage into a Mutex or task (`Mutex.new(LogState.new(out: w))` with a parameter `w`) | fixed: `s.dup` on a struct is a deep copy; Mutexes in it stay shared (deep copies, including a closure's `dup`, used to copy a captured Mutex) |
| 49 | iotest | `x == none` / `assert_eq x, none` on an `Error?` compared the payloads (C: invalid operands; JIT: panic) | fixed: comparing with a literal `none` tests presence only |
| 50 | log | a closure can't hold a closure of its own type, so Go's OnceFunc/OnceValue (a func wrapping a func) are structs with `call` | by design (closures are closed sums) |
| 51 | log, os/signal | no mutable package globals: no settable standard logger (log.SetOutput/SetFlags/SetPrefix), no channel table for signal.Stop(c) | open (design; see #30) |
| 52 | log | no caller location at run time: Lshortfile/Llongfile write `???:0` | open |
| 53 | os/user | rgen: a fallible function returning `~T?` whose body ends in `loop { ... return ... }` emits an ill-typed trailing `(true, ())` (rustc rejects it); C and JIT accept | open (worked around with `while`) |
| 54 | io/fs | an io/fs test can't import testing/fstest (it compiles a second io/fs whose types differ), so io/fs's MapFS tests live in testing/fstest | open |
| 55 | sync | a method that returns early (`return if x`) and ends in a value-producing call (`done.recv`) reports "returns nil, but its last expression is T?"; `_ = e` as the last statement has e's type | open (small) |
| 56 | crypto/cipher | two struct layouts nesting the same fields differently (`Tup(Tup(A), B)`, `Tup(Tup(A, B))`) got one C type name: a generic struct field inside a plain struct failed to compile with --release | fixed: C tuple names carry their arity (acceptance case nested_layouts) |
| 57 | crypto/aes | bounds checks on constant tables (`TE0[(s >> 24).to_i]`) in release: ~2x slower AES than Go generic | fixed: release proves `[T; N]` indexes in range from narrow integer types, `x & c`, `x >> c` and conversions |
| 58 | crypto/cipher | a mode generic over its block (`Gcm[B]`) wrapping a `cipher.Block` *interface value* can't itself become an AEAD interface value (D12) | open (design; modes are used concretely) |
| 59 | net/rpc | `T.from_json(s)` inside a generic def (a static method on a type parameter) | fixed: resolved per instance (checker) |
| 60 | net/rpc | passing a fallible def `-> ~R` where `(A) -> ~R` is wanted: "lambda result: expected R, got ~R" | fixed (checker) |
| 61 | net/rpc, mail, multipart, httputil | type parameters didn't bind through `Mutex[S[T]]`/`Atomic`/`Chan`/`Pool` or another package's generic type (`textproto.Reader[R]`, and `Reader.new` inside its package when instantiated from outside) | fixed: `bind_tparams` (checker) |
| 62 | net/rpc | `x: ~Reply = h.get` bound R to `~Reply` | fixed |
| 63 | net/smtp | `spawn { f(x) }` with `f -> ~T` (T not Unit): bad C, lost value on the JIT | fixed: a checker error asking for `spawn { ~f(x) }` |
| 64 | net/http/pprof | the JIT didn't resolve `alx_mem_held` / `alx_mem_peak` | fixed |
| 65 | net/http audit | a `Mutex` captured by a lambda that moves into another task (`f.dup`, a handler in its request task) is copied: updates under `lock` there are lost (JIT and C); `Atomic` and `Chan` share | fixed by the sync port (D55: copies of closures and structs keep a captured Mutex shared); acceptance case mutexcapture |
| 66 | httputil | `def f { spawn { 1 } }; t = spawn { f() }` crashes the compiler: "a task handle has no zero value" (`lower.rs` `zero_le`) | open (workaround: explicit `return`) |
| 67 | cookiejar | `mu.lock { \|s\| if c { s.m.delete(k) } else { s.m[k] = 1 } }`: C emits `v = 0;` for the store arm (typed `Int?`) | open (workaround: end the block with `nil`) |
| 68 | net/mail | C mistypes a statement `case` whose arms assign locals of different types (as in json-derive.md) | open |
| 69 | net/http/cgi | spawning with a struct holding a lambda: the error suggests `h.dup`, which doesn't exist | open |
| 70 | net/rpc | `r: Reply = client.call(..).unwrap` can't infer R through `.unwrap` | open |
| 71 | httptest, httptrace, fcgi | a closure can't capture a value holding a closure of its own type (handler capturing its server, a director wrapping the old one, composed trace hooks) | open (design; see #2) |
| 72 | sniff | `b("RIFF") + [0, 0, 0, 0]`: a literal after `+` on a `[Byte]` stays `[Int]` | open (workaround: a `[Byte]` parameter) |
| 73 | (main, net merge) | the Rust oracle of httpdemo grew to 7.4 MB and rustc ran for hours: every `fail` inlined a deep copy of the program-wide Error type (all error types' variants) | fixed: one shared `__error_copy` function (2.4 MB, rustc 64 s) |

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
| 25 | crypto | `[0xee.as_u8, 1, 1]`: later elements don't take U8 from the first | open |
| 26 | crypto | a `[T; N]` local is heap-allocated, not a stack array | open (perf) |
| 27 | crypto | HMAC can't ask a generic `H` whether it's marshalable, so no saved keyed state: PBKDF2 3.8x Go | open (perf) |
| 28 | math/big | no add-with-carry: big Mul/String ~2x Go | open (perf) |
| 29 | math/big | tuple-returning functions get region mark/reset even when they don't allocate (`bits.add64` slow in loops) | open (perf) |
| 30 | math/big | no mutable package globals (caches like Go's divisor table) | open (design) |
| 31 | math/big | `format` has no `%g` | open |
| 32 | math/big | a main file can't declare its own `Int`/`Float` (only packages can) | open (by design for now) |
| N1 | net/rpc | `T.from_json(s)` inside a generic def (a static method on a type parameter) | fixed: resolved per instance (checker) |
| N2 | net/rpc | passing a fallible def `-> ~R` where `(A) -> ~R` is wanted: "lambda result: expected R, got ~R" | fixed (checker) |
| N3 | net/rpc, mail, multipart, httputil | type parameters didn't bind through `Mutex[S[T]]`/`Atomic`/`Chan`/`Pool` or another package's generic type (`textproto.Reader[R]`, and `Reader.new` inside its package when instantiated from outside) | fixed: `bind_tparams` (checker) |
| N4 | net/rpc | `x: ~Reply = h.get` bound R to `~Reply` | fixed |
| N5 | net/smtp | `spawn { f(x) }` with `f -> ~T` (T not Unit): bad C, lost value on the JIT | fixed: a checker error asking for `spawn { ~f(x) }` |
| N6 | net/http/pprof | the JIT didn't resolve `alx_mem_held` / `alx_mem_peak` | fixed |
| N7 | net/http audit | a `Mutex` captured by a lambda that moves into another task (`f.dup`, a handler in its request task) is copied: updates under `lock` there are lost (JIT and C); `Atomic` and `Chan` share | open (serious) |
| N8 | httputil | `def f { spawn { 1 } }; t = spawn { f() }` crashes the compiler: "a task handle has no zero value" (`lower.rs` `zero_le`) | open (workaround: explicit `return`) |
| N9 | cookiejar | `mu.lock { \|s\| if c { s.m.delete(k) } else { s.m[k] = 1 } }`: C emits `v = 0;` for the store arm (typed `Int?`) | open (workaround: end the block with `nil`) |
| N10 | net/mail | C mistypes a statement `case` whose arms assign locals of different types (as in json-derive.md) | open |
| N11 | net/http/cgi | spawning with a struct holding a lambda: the error suggests `h.dup`, which doesn't exist | open |
| N12 | net/rpc | `r: Reply = client.call(..).unwrap` can't infer R through `.unwrap` | open |
| N13 | httptest, httptrace, fcgi | a closure can't capture a value holding a closure of its own type (handler capturing its server, a director wrapping the old one, composed trace hooks) | open (design; see #2) |
| N14 | sniff | `b("RIFF") + [0, 0, 0, 0]`: a literal after `+` on a `[Byte]` stays `[Int]` | open (workaround: a `[Byte]` parameter) |

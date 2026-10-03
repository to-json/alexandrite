# Compiler issues found while porting std

Open problems agents reported; each gets fixed in a compiler round between waves.

| # | Found by | Problem | Status |
|---|---|---|---|
| 1 | text/tabwriter | JIT gives every temporary its own stack slot (`jit.rs` `slot()`), never reused: deep call chains need 130-160 KB of a 256 KB task stack; a 100-call test block overflowed on the JIT only | fixed: slots are pooled and reused per statement |
| 2 | text/scanner | a closure capturing a struct that the closure is stored in crashes the compiler (Rust stack overflow) instead of a diagnostic | open |
| 3 | encoding/csv | qualified enum patterns (`case e { csv.CsvError.FieldCount(s, l, c, rec) => }`) don't bind their variables; unqualified ones do | fixed: `Type.Variant` and `pkg.Type.Variant` patterns (the type part narrows error variants) |
| 4 | text/scanner | `((A) -> B)?` (optional function type) doesn't parse | fixed: `(T)` groups a type |
| 5 | text/scanner | lambda allocations: fixed for program-region placements only; other placements unverified | partly fixed |
| 6 | net/url | `Str.runes` / `for r in s.runes` on invalid UTF-8 is wrong (`"\x80A".runes` gives `[1]`, Go `[0xFFFD, 65]`): `alx_str_charlen`/`decode_rune` don't validate | fixed: charlen validates by Go's rules (all backends) |
| 7 | net/url | a ~800-assert test block overflowed the JIT task stack (before #1's fix) | recheck |
| 8 | containers | `opt?.field || none` gets the wrong type for `none` (JIT and C) | open |
| 9 | containers | `format` has no integer precision (`%.2d`) | fixed: `%.Nd` (and x/o/b) as Go: minimum digits, 0 flag ignored, `%.0d` of 0 empty |
| 10 | containers | static methods on generic structs (`def self.make(n) -> R[T]`) report "is recursive" | open |
| 11 | containers | a generic function can't take its type only from the declared result (`x: R[Int] = mk(3)`); no explicit type arguments on calls | open |
| 12 | containers | `==` on optionals isn't allowed | fixed: `T? == T?` and `T? == T` (the plain side is wrapped) |
| 13 | hash | interface method result types aren't covariant (no `Cloner` interface) | open |
| 14 | hash | an array literal mixing struct types doesn't convert to `[Iface]` even when declared | fixed: array literals coerce element-wise |
| 15 | hash | `alx test` on the JIT accepts unused imports, `--release` rejects them | open |
| 16 | hash | maphash's `getentropy` FFI on wasm | check |
| 17 | (main) | an array literal `[128, 65]` doesn't adapt to a `[Byte]` parameter (`Str.from_bytes([128, 65])`); a typed local works | fixed: array literals coerce element-wise; C declares array types used only by literals |

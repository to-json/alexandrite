# L3 (part): `encoding/json` on compile-time derives

The first built-in derive (S3: "compile-time derives, no runtime type info") and the first package written on it. Decisions D37 to D40 in GO-VS-RUBY.md; the user-facing description is the header of `std/encoding/json/json.alx`.

```ruby
import "encoding/json"

#[derive(Json)]
struct User {
  name: Str
  #[json("e-mail")] email: Str?
  #[json(omit_empty)] tags: [Str]
  #[json(skip)] cache: Int
}

s = ~u.to_json                  # {"name":"ann","e-mail":null}
u = ~User.from_json(s)
v = ~json.parse(s)              # a dynamic Value: v.get("name"), v.keys, ...
```

## Derives

(encoding/json/v2's methods come from the same derive in files that use v2:
docs/notes/json-v2.md, GO-VS-RUBY D62.)

- **Syntax:** `#[derive(Json)]` (a list is accepted: `#[derive(Json, Eq)]`; `Eq` and `Show` need nothing, structs, tuples and enums already compare and print field by field) before `struct` and `enum`. Field and variant options: `#[json("name")]`, `#[json(omit_empty)]`, `#[json(skip)]`, combinable (`#[json("n", omit_empty)]`). Go's tag form `#[field(json: "n,omitempty")]` says the same (S5, data-derive.md). The lexer's attribute token now ends at the matching `]` (strings and brackets inside count).
- **Where it expands:** in the parser (`compiler/src/derive.rs`), as source text. The parser records each derived declaration while reading the module; at the end of the module the generator writes `struct Name { def ... }` as text, lexes it, gives every token the span of the declaration, and parses it with the same parser; the defs become methods of the type. No checker, lowering or backend change was needed for derives themselves. If the file has no `import "encoding/json"`, one is added.
- **What it writes:** `to_json`, `to_json_indent`, `Type.from_json`, and the protocol they use: `json_str(e) -> Str`, `json_enc(e)`, `Type.json_dec(d)`, `Type.json_zero`. Every local is `_`-prefixed so a field named `e`, `d`, `s` or `r` can't clash. `ALX_DERIVE_DEBUG=1` prints the generated text.
- **Static methods** (`def self.name`, called `Type.name(...)`) were added to the language for `from_json`; D38.
- **Errors at the derive:** a field of a local type that doesn't derive Json; map keys other than `Str`; fixed-size arrays; fn, pool-handle and fallible types; generic types; an unknown option or derive name. All point at the declaration.
- **Not done: generic types** (`struct Page[T]`). The expansion is per declaration; a field of type `T` can't be encoded without the checker (no `T.json_dec`, no refinement dispatch on a type parameter). It needs expansion per instance, which is the lazy member-table design in DESIGN.md. The error says so.

## The package

| File | Holds |
|---|---|
| `json.alx` | `JsonError`, `quote`, `quote_raw`, `html_escape`, the header documentation |
| `encode.alx` | `Encoder`, Go's float formatting, `marshal`, `marshal_indent` |
| `decode.alx` | the syntax scanners and `check` / `valid`, `Decoder`, number parsing |
| `value.alx` | `Value` (the dynamic tree), `parse`, `indent`, `compact` |

- **Encodings** (full table in the header): numbers, strings (Go's escaping, `<>&` and U+2028/9 escaped by default, invalid UTF-8 as `�`), `T?` as value or `null`, `[T]`, `Map[Str, V]` (insertion order), tuples as arrays, structs as objects. Enums (Go has no sum types; serde's externally tagged form): unit variant `"Red"`, one field `{"Circle":1.5}`, several `{"Pair":[1,"a"]}`, named fields `{"Rect":{"w":1,"h":2}}`.
- **Decoding** follows Go's Unmarshal: syntax checked first, unknown keys skipped, `null` a no-op, absent fields zero, integer range checks, `\uXXXX` with surrogate pairs. Errors are Go's messages where Go has the same error (`invalid character 'x' looking for beginning of value`, `unexpected end of JSON input`, ...) and `json: cannot unmarshal string into field User.age of type Int` for type errors.
- **`Value`:** `enum Value { Arr([Value]) }` is refused ("`Value` contains itself; it is a value, so it can't"), so a document is flat: nodes in post-order and one array of child indices. A `Value` is `(nodes, edges, id)`, cheap to copy. It can be a field of a derived type (Go's `RawMessage` / `interface{}`).
- **Nesting depth** is 10,000 (Go's limit). The scanners and the `Value` builder are iterative with an explicit stack: a recursive version overflowed the 8 MB main stack at about 300 levels, because the C backend inlines the decoder's methods into one very large frame (about 28 KB per level).

## Speed

A 714 KB document (6,000 records: ints, floats, strings with escapes, string arrays, optionals), release build, C backend, macOS arm64, best of repeated runs:

| Operation | MB/s |
|---|---|
| `to_json` (derived, to a Str) | ~74 |
| `from_json` (derived, includes the syntax pre-check) | ~28 |
| `json.parse` to a `Value` | ~28 |
| `json.valid` | ~1,100 |

What shaped it, for whoever optimizes next:

- **A fallible call returns the program's whole `Error` sum** (a struct of every error variant in the program, hundreds of bytes). The decoder is therefore infallible: the first error is kept, later calls return zero values at once, and `done!` reports it. `valid` / `check` return an index or a negative code and build the error only when something is wrong.
- **Every call in a loop got an iteration region.** The region analysis counted any call as an allocation site; a loop that calls `skip_ws(s, p)` per byte entered and left a region per byte. Calls whose result has no storage (Int, Bool, ...) are no longer sites (`compiler/src/regions.rs`): `valid` went from 77 to 1,190 MB/s, and every scanner-style loop benefits.
- **Pushing is the remaining cost.** A push onto an array owned by the caller (the encoder's pieces, the decoder's key, any `st[0].xs << x` through a one-element state slice) switches regions around the push: `alx_region_of`, `alx_region_use`, `alx_region_set` and thread-local lookups are about half of the profile of both directions. A fast path in the C backend for a push that doesn't grow the array (`len < cap`: store, no region switch) would help every program. The encoder avoids pushes where it can: a record is one interpolated string, one push.
- Floats take Go's shortest-digits path through `strconv.format_float` (a decimal big-number shift); integral values and short decimals have fast paths.

## Tests

- `std/encoding/json/json_test.alx` (7 blocks): quoting, Go's float formats, NaN/Inf, syntax errors with Go's messages, `indent` / `compact`, the dynamic `Value`, 10,000-deep nesting.
- `std/encoding/json/derives/derives_test.alx` (12 blocks, its own package because derive code imports `encoding/json`): every field type, round trips, sized-integer limits, type errors naming the field, unknown fields and nulls, string escapes and surrogates, omit_empty / skip / rename, enums in all four shapes, tuples, indent, HTML escaping, a 20,000-record, 600 KB document.
- `acceptance/cases/jsondemo.alx` (group L3): JIT, C debug/release/sanitize and the Rust oracle, and the browser; three negative cases (`jsondemo.bad1` to `bad3`) for the derive's errors.

## Compiler changes, besides the derive

- Static methods (`def self.name`), called through the type.
- Attribute tokens end at the matching `]`.
- `zero_of` knows tuples (`Struct.new` with a tuple field works).
- Error sets: a fallible `!` method call under `~` contributes the callee's error set (it was the open `Error`); reading `self` in a `!` method under `~` no longer counts as an `IndexError`; arithmetic under `~` in a `#![overflow(wrap)]` file can't fail with `ArithError`; a recursive call in a def with a declared set is covered by the declaration.
- Regions: scalar-returning calls aren't allocation sites (above).

## Found on the way (not fixed)

- `r = case x { "a" => { A } _ => { fail E.Bad("x") } }` (a `case` used as a value with a `fail` arm in braces) compiles, then aborts with `function ended without a value`. The derive assigns in each arm instead.
- A statement `case` in a loop whose first arm ends in an assignment of an `Opt`-typed field from a `~` call, with a unit arm after it, produced C that assigned `0` to a `Tup_Bool_Str` (and a Cranelift type-mismatch panic). Ending such arms with `nil` avoids it; the derive does.
- The `Error` sum's size makes every fallible call expensive (above).
- `c.some_field.err.message` needs `err?.message || ""` because `.err` is `Error?`; and a `JsonError`'s `offset` is only reachable by matching the variant.

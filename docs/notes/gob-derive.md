# S5: `#[derive(Gob)]` and `encoding/gob`

Go's gob, byte for byte, without reflection (GO-VS-RUBY D66). The user-facing description is the header of `std/encoding/gob/gob.alx`.

```ruby
import "encoding/gob"

#[derive(Gob)]
struct User {
  user_id: Int                       # sent as UserId
  #[field(gob: "Name")] full: Str
  #[field(gob: "-")] cache: Int
  tags: [Str]
  boss: User2?                       # *User2
  shape: Shape                       # an interface
}

#[derive(Gob)]
enum Shape {
  Circle(Float)                      # Go: type Circle float64, registered "main.Circle"
  Rect(Rect)                         # Go: the struct Rect, "main.Rect"
  Named(name: Str, n: I8)            # Go: struct Named { Name string; N int8 }
  #[field(gob: "int")] Num(Int)      # Go: an int in the interface
}

enc = gob.new_encoder(w)
enc.~encode!(u)
u2 = gob.new_decoder(r).~decode!(User.new)
gob.register_name("circle", Shape.Circle(0.0))   # Go's RegisterName (optional)
```

## How it fits together

Go's gob is two halves: reflection describes types (type.go: ids, wire types), and per-type encode/decode engines move values. Here:

- **Describing types** is generated: `T.gob_rtype(types)` builds an `RType` (Go's kind, `String()`, `Name()`, element/key/field types) in the encoder's `Types` table. Everything from there is Go's code over RTypes instead of reflect.Types (`std/encoding/gob/types.alx`): the `types` map keyed by `String()`, `newTypeObject` (a struct takes its id before its fields', a slice/array/map after its elements'), `buildTypeInfo`, the wire types, `sendTypeDescriptor` (`encoder.alx`). That is why the bytes match Go's exactly, including Go's quirks: a `*big.Int` field sends two ids (`getType` on the pointer type makes a second gobEncoderType named ""), a struct first reached as a map element is nameless.
- **Moving values** is generated inline per field: `gob_enc` writes deltas and values (zero fields left out; elements of slices/arrays/maps always sent), `gob_dec` reads a struct by a *plan* (for each wire field, the local field or -1), built once per (type, wire id) with Go's `compatibleType` checks and errors, cached in the Decoder; unknown fields are skipped by walking the received wire type (`Dec.skip!`).
- **Interfaces** need run-time types in Go. alx's closed sums are enums, so an enum is the interface: its value encodes as Go's `encodeInterface` (name, type descriptors, id, the value in a message of its own; the descriptors flush the message under construction exactly as Go's writer stack does). Decoding reads the name, maps it through the registry, and dispatches to the variant.
- **Registry:** `REGISTRY = Mutex.new(...)` (R11), Go's two maps. Every derived variant has a default name (`pkg.Variant`, or the `gob:` tag), so registration is only for renaming, and Go's duplicate-registration panics are kept.

## The protocol

| Method | |
|---|---|
| `self.gob_rtype(t: Types) -> Int` | the Go type, described |
| `gob_type(t) -> Int` | the same, through a value (a top-level value's type: `*big.Int` for big's) |
| `gob_type_name -> Str` | Go's Register name; an enum value: its variant's |
| `self.gob_zero -> T` | what decoding starts from (a struct's `T.new`, an enum's first variant) |
| `gob_enc(e: Enc)` | the value (a struct: fields + terminator; an enum: an interface value) |
| `self.gob_compat(d: Dec, w) -> ~Bool` | Go's compatibleType against wire type w |
| `gob_dec(d, w) -> ~T` | a value of wire type w, decoded into a copy of self (Go merges) |
| `gob_top(d, w) -> ~T` | as a whole value: the singleton delta, "no fields matched" |

A type outside the derive provides the same methods: math/big's Int, Float and Rat do (generic over `B`, `E`, `D`, so math/big doesn't import gob), and `derives/ext_test.alx` has Go's test GobEncoders and BinaryMarshalers written that way. Top-level values of the predeclared types use refinements in `basics.alx`.

## Choices

- **Names.** Go's field names are exported identifiers; an alx field's default is CamelCase (`user_id` -> `UserId`), the `gob:` key of `#[field(...)]` (parsed once in derive.rs, `Opts.tags`) overrides it, `"-"` leaves the field out. The type's Go name and package come from `#[data("pkg.T")]`, the attribute Data uses for the same thing; default `main.T`, which matches Go programs' `package main`.
- **Enum mapping** as above. A positional variant with one field sends that field's type itself, so `Rect(Rect)` interoperates with Go code that puts a `Rect` in an interface; other variants are structs named after the variant. `E?` is a nil-able interface; a nil received into a plain `E` field gives the enum's zero value.
- **Decode into a value** (`v = dec.~decode!(v)`): Go decodes into what a pointer points at, keeping fields the stream doesn't mention; a static `T.gob_decode` on a type parameter couldn't reach refinements anyway (port-issues #109).
- **Per-Encoder ids**: Go's are process-global; a fresh Encoder here numbers as a fresh Go process does, which is what the byte vectors compare against.

## Tests

- `std/encoding/gob/codec_test.alx`: the wire numbers (Go's encodeT table), scalar instructions, basics at the top level, bad data, sequential decoders, EOF, big numbers, the registry.
- `std/encoding/gob/derives/` (its own package: derived code imports encoding/gob):
  - `vec_enc_test.alx`: ~150 cases, each Go's bytes from a fresh Go process (ints, uints, floats, complex, strings, bytes, slices, single-entry maps, structs, pointers, arrays, nested, interfaces of every variant shape, nested interfaces, big numbers, streams), asserted for alx's encoder and for decoding + re-encoding.
  - `vec_goread_test.alx`: bytes alx writes (multi-entry maps, enums, big numbers, ...) with what a Go program decoded from them.
  - `vec_dec_test.alx`: Go encodes, a fresh Go process decodes into another type; alx must give the same value or Go's error text (field matching, range checks, compatibility, unregistered names).
  - `codec_test.alx`, `encoder_test.alx`, `gobencdec_test.alx`, `example_test.alx`: Go's tests (what isn't translated, and why, is at the top of each file).
- Acceptance: `gobdemo` (group S4), `fixed_copy_region` (the compiler fix the port needed).

## Compiler changes

- `derive_gob.rs` (new), hooks in `parser.rs` (a `gjobs` list, `expand_gob_derives`) and `derive.rs` (`Gob` accepted).
- `lower.rs` `copy_value`: a fixed array copied into a local is allocated in the region of the array it copies (port-issues #108).

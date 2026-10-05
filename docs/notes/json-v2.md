# encoding/json/jsontext and encoding/json/v2 (D62, D65)

Go's JSON v2: the syntactic layer (`jsontext`) ported as it is, and the
semantic layer (`v2`) on the same `#[derive(Json)]` as encoding/json (v1).
User-facing documentation: the headers of `std/encoding/json/jsontext/jsontext.alx`
and `std/encoding/json/v2/json.alx`.

```ruby
import json "encoding/json/v2"      # the package's name is v2
import "encoding/json/jsontext"

#[derive(Json)]
struct User {
  #[field(json: "name")] name: Str
  #[field(json: "age,omitzero")] age: Int
  #[field(json: "id,string")] id: U64
  #[field(json: ",embed")] extra: Map[Str, jsontext.Value]   # unknown members
}

b = ~json.marshal(u, [json.deterministic(true), jsontext.multiline(true)])
u2: User = ~json.unmarshal(b, [json.reject_unknown_members(true)])
u3 = ~json.unmarshal_into(b, u2, [])          # Go's merge into an existing value
```

## jsontext

| File | Holds (Go's file) |
|---|---|
| `jsontext.alx` | header, Kind, Cause / SyntacticError / JsonTextError, Pointer, Options and flags, hooks (options.go, errors.go, state.go's Pointer) |
| `wire.alx` | internal/jsonwire: consuming, quoting, unquoting, floats, UTF-16 comparison |
| `state.alx` | the state machine, names, namespaces |
| `encode.alx`, `decode.alx`, `token.alx`, `value.alx` | as Go's |

- Objects and arrays inside a value are consumed and reformatted with an
  explicit stack (Go recurses; 10,000 levels overflowed a task's stack).
- Tokens own their bytes (no invalidation), the decoder still discards consumed
  input when it reads more.
- Options is one struct (presence and value bits as Go's jsonflags, the
  non-boolean values, v2's format and hooks); option functions return one with a
  single setting and lists are joined in order. `get_option(opts, setter)` is
  Go's GetOption for boolean options.

## v2 on the derive

`compiler/src/derive_json2.rs` writes, for a file that imports `encoding/json/v2`
or `jsontext`:

| Method | |
|---|---|
| `json_v2_type -> Str` | Go's name of the type (`#[data("pkg.T")]`, else the alx name) |
| `json_v2_enc(e, o) -> ~Unit<Error>` | writes the value (hooks first) |
| `json_v2_dec(d, o) -> ~T<Error>` | decodes into a copy of self, returns it (merge semantics) |
| `json_v2_is_zero -> Bool` | omitzero |
| `json_v2_fields_enc`, `self.json_v2_lookup`, `self.json_v2_has`, `json_v2_field_dec` | what a struct that embeds this one calls |

- Per field, the generated code calls the per-kind functions of package v2
  (`marshal_int(e, v, o, "int")`, `unmarshal_slice(d, cur, o, "[]string", zero, elem)` ...)
  with lambdas for elements, keys and values, so the semantics live in alx code
  ported from Go's `arshal_default.go`, not in the generator.
- A hand-written type provides the same protocol (Go's MarshalerTo /
  UnmarshalerFrom): `json_v2_type`, `json_zero`, `json_v2_enc`, `json_v2_dec`,
  `json_v2_is_zero`. jsontext.Value does.
- `#[json(transparent)]` before a one-field struct: the struct is its field (v1
  and v2), which is how `[Int]` or a map is marshaled at the top level.
- One derive, one tag syntax, v1's methods unchanged. v1 is not re-implemented
  on v2 (Go 1.25+ does that): the v1 path is about 7x faster, and its
  alx-specific errors stay as they are. In a v2 file v1 accepts what only v2 can
  encode and fails at run time instead.
- Struct-level checks: `format:` options are refused unless
  `json.experimental_support_format_tag(true)` (Go 1.27 gates them the same way,
  through an internal option); the error names the field by its `data:` tag.

## Hooks (WithMarshalers / WithUnmarshalers)

Go looks functions up by run-time type. Here:

- `jsontext.Options` holds `[MarshalHook]` / `[UnmarshalHook]`: closures over
  `(HookEncoder, Arg)`. They can't name `Encoder` (Options lives inside the
  Encoder and a type can't contain itself, even in a function type); the
  interface `HookEncoder` is named, not structural, and a hook gets its Encoder
  back with `case h { jsontext.Encoder(e) => ... }`.
- `Arg` is a builtin by kind (with Go's type name) or a derived value as a
  `Marshalee` interface value (every derived type has `json_v2_type`).
  `marshal_func[T](f)` erases `f` and takes `T` back with `case m { T(v) => }`:
  the type switch of D65, which this port added.
- Builtins can't be interface values, so they have their own constructors
  (`marshal_func_str`, `marshal_to_func_int`, `unmarshal_func_bool`, ...).
- `JsonError.Unsupported` is Go's errors.ErrUnsupported (skip to the next
  function or the default); the wrappers check Go's "exactly one value" rule.

## Tests

- jsontext (35 blocks): Go's coder, decoder/encoder error, faulty I/O, resumable
  and peekable decoder, max depth, value methods, token accessors, state machine,
  namespace, pointer and jsonwire tests; their tables are generated from Go's
  (`vec_*_test.alx`) by a generator that copies Go's test tables.
- v2/gotests: Go's TestMarshal and TestUnmarshal through a generator that turns
  Go's test types into alx types (`#[data]` names, `data:` field names, Go's tags),
  values into literals and expectations into Go's output and error texts: 84 of
  401 marshal cases and 238 of 549 unmarshal cases. The rest need `any` or
  interfaces, methods (MarshalJSON, MarshalText, IsZero), time and netip types,
  Go funcs in WithMarshalers, v1 compatibility flags, unexported-field rules or
  named basic types in error texts.
- v2 (4), v2/derives (13): fold and SemanticError tables, examples (case
  sensitivity, omit fields, embedded fields, stream, multiline, EscapeForHTML,
  MarshalerTo/UnmarshalerFrom), marshal/unmarshal functions, MarshalEncode
  options, MarshalWrite/UnmarshalRead.
- Acceptance: `jsonv2` (group S4) and `typeswitch` (D65).

## Speed

1.6 MB (20,000 records), release, arm64: v2 marshal 67 ms (~25 MB/s), unmarshal
132 ms (~12 MB/s); v1 9 ms and 18 ms. v2 writes token by token through the state
machine and every per-value call returns a Result carrying the program's Error
sum; Go's `optimizeCommon` fast paths (appending to the buffer directly) are the
obvious next step.

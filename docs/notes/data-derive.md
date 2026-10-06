# S5: `#[derive(Data)]`, package `dyn`, and `#[field(...)]`

The reflection answer of GO-VS-RUBY S5 (hybrid derives): one general derive to a dynamic value tree for consumers that inspect values at run time, typed per-format derives (`Json`, later `Asn1`, `Row`, `Xml`, `Gob`) for exact wire code, and one field-attribute form all of them read. Built with the text/template and html/template ports (D59), which are its first consumer.

```ruby
import "dyn"

#[derive(Data)]
#[data("main.User")]                       # the Go type name consumers see (optional)
struct User {
  #[field(json: "name", data: "Name")] name: Str
  #[data(skip)] cache: Int
  tags: [Str]
  boss: User2?

  pub def greet(p: Str) -> Str { "#{p}, #{name}" }   # callable by consumers
  pub def to_s -> Str { "User(#{name})" }           # Go's String()
}

v = u.to_data                    # dyn.Value: a struct "main.User" {Name, tags, boss}
u2 = ~User.from_data(v)          # and back
dyn.sprintf("%+v", [v])       # {Name:ann tags:[a b] boss:<nil>}
t.execute_str(u)                 # text/template: {{.Name}} {{.greet "hi"}}
```

## Why a package of its own: `std/dyn`

The value type started inside text/template, but every reflection-shaped Go package needs the same thing: templates (`.Field`, `index`, `range`), slog's `Any`, expvar's `Var`s, testing/quick's generated arguments, fmt's `%v` of a user type. A small std package they all import keeps one representation and one derive.

- **Not `reflect`:** Go's reflect is a run-time type system alx doesn't have (S3); a package by that path would read as a port of it. `reflect` stays "excluded" in STDLIB.md.
- **Not `encoding/...`:** it isn't a wire format; json's `Value` stays the parse tree of JSON text, and `dyn.Value.from_json` converts one into the other (a dyn Value is a superset: Go kinds and type names, pointers, functions, method-carrying structs).
- **`dyn`** ("dynamic value") is short, isn't a Go package name, and doesn't collide with the names programs give locals (`data`, `value`, `v`), which matters because a package's name is how code refers to it.

## The value (std/dyn/value.alx)

- A `Value` is a kind (Go's reflect kinds folded: every int width is `KIND_INT`; plus `KIND_PTR`), the Go type name as text (`"map[string]int"`, `"*main.T"`, `"template.HTML"`), scalars inline, and lists, maps, structs, pointers and functions as nodes of a `Doc` arena. alx values can't contain themselves (port-issues #2, #80), so a tree is stored flat, as json.Value is. Values are immutable and cheap to copy; a container's constructor copies its elements into one arena.
- The type name decides what Go's reflection would: printing (`sprint` / `sprintf` / `sprintln` are Go's fmt over Values, std/dyn/fmt.alx), `%T`, comparison rules and error messages.
- A nil pointer is a pointer node without an element; a nil interface is a nil pointer whose type name isn't `*...`; a value read out of a container through `interface {}` remembers that (`Value.it`, only for error text: Go says "in type interface {}").
- A struct node may carry a `Data` object: the original alx value, whose exposed methods a consumer calls by name (`data_sig`, `data_call`, `data_string`).
- Functions are `Func { Sig, ([Value]) -> ~Value }`; `Sig.parse` reads Go's signature text (`Func.of("func(int, ...string) (string, error)", f)`), so callers can check arity and convert arguments as Go's reflection would. `fail` in a function is Go's error result.
- Reading back: `to_int`, `to_uint`, `to_float`, `to_complex`, `to_bool`, `to_str` (numbers convert between kinds only when exact: JSON's 3.0 is an Int, 3.5 isn't), `want_list`, `want_map`, `want_struct`, `get_field` / `field_or_nil` (a map with string keys stands for a struct, as JSON objects do), `variant_name`, `deref`, `absent?`. Errors are `dyn: cannot use string value as int`, `dyn: main.P has no field y`, `dyn: Shape has no variant "Box"`.

## The derive (compiler/src/derive_data.rs)

Expansion by source text, like `#[derive(Json)]` (json-derive.md): the parser records the declaration, its options and its methods; at the end of the module the derive writes ordinary defs and parses them in. Adds `import alxdyn "dyn"` if the file has no `import "dyn"`.

| Generated | |
|---|---|
| `data_add(b: Doc) -> Int`, `to_data -> Value` | the value as a tree |
| `T.from_data(v) -> ~T` | back from a tree, when every field can come back |
| `data_sig(name) -> Sig?`, `data_call(name, args) -> ~Value`, `data_string -> Str?` | the `dyn.Data` interface, when the type has public methods or `to_s` |
| `data_funcs -> Map[Str, Func]` | the public methods as functions (a template FuncMap) |

- **Field types:** Int and the sized integers (Go's int, int8 ... uint64; Rune is int32, Byte uint8), Float, Bool, Str, Complex, `T?` (a pointer, or a typed nil), `[T]`, `[T; N]`, `Map[K, V]`, function types (to a `Func`, signature from the alx type), `Error` (Go's `*errors.errorString`: its message), `dyn.Value` (any value, passed through), and local or imported types that derive Data. Enums: a payload-free variant of a type without methods is a string of the enum's type; otherwise a struct `{variant, fields...}`.
- **from_data** is generated when every (non-skipped) field type can come back; a function field can't (a closure calling through the Value's `Func` would hold the closure `to_data` wraps it in: a closure containing itself, port-issues #71), and a skipped field needs a literal zero (numbers, Str, Bool, arrays, maps, options). A type whose fields can't come back simply has no `from_data` (types that contain it lose theirs too: a fixpoint over the module's derived types).
- **Methods** exposed: public, non-`!`, non-static, non-generic, with parameters of scalar types, `[scalar]` or `dyn.Value` and a result the derive can convert. `to_s` is also `data_string` (Go's String()).
- **Not done:** generic types (as for Json: expansion is per declaration); methods with other parameter types are not exposed. (A derived type with methods may hold `dyn.Value` fields since R13: the `Data` interface is boxed when it contains itself.) A function field still has no `from_data`: that needs the derive to wrap a `Func` back into a closure, which R12 now allows but nobody has written.

## One attribute form: `#[field(...)]` (compiler/src/derive.rs)

Go's struct tag in alx syntax, one key per derive: `#[field(json: "id,omitempty", data: "ID", xml: "id,attr", db: "user_id", asn1: "explicit,tag:0")]`.

- `derive::apply_field_attr` parses it into the field's `Opts`: every `key: "value"` is kept in `Opts.tags` (in order) for the derive that owns the key; the keys a derive here reads are applied at once. A key given twice, a non-string value or a non-identifier key is an error at the attribute.
- `json:` is Go's json tag: `"name"`, `"-"` (skip), `"name,omitempty"`, `",omitempty"`.
- `data:` is `"Name"` (the field's, variant's or method's name in the tree) or `"-"` (left out).
- The per-derive shorthands stay: `#[json("name")]`, `#[json(omit_empty)]`, `#[json(skip)]`, `#[data("Name")]`, `#[data(skip)]`, and they combine with `#[field(...)]` on the same field.
- Before a type, `#[data("pkg.T")]` gives the Go type name consumers see (default: the alx name).

A new derive reads its key from `Opts.tags` and parses Go's tag syntax for it; nothing in the parser changes. `#[derive(Xml)]` (D65, xml-derive.md) reads `xml:` this way.

## Tests

- `std/dyn/value_test.alx`: constructors, type names, fmt, from_json, the conversions, functions.
- `std/dyn/derives/derive_test.alx`: every field kind both ways, enums, options, errors, methods through `Data`.
- `std/encoding/json/derives/derives_test.alx` "field tags": `#[field(json: ...)]`, foreign keys ignored.
- Acceptance case `tmplfixes` (all backends and the Rust oracle): a derived type with methods and function fields in a template; to_data / from_data round trip; one `#[field(...)]` read by Json and Data.
- text/template and html/template run on it (their derives tests, generated vectors and Go's test tables).

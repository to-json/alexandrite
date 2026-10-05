# S5: `#[derive(Xml)]` and `encoding/xml`

The second typed per-format derive of GO-VS-RUBY S5 (after `Json`), and the port of Go's `encoding/xml` built on it. Decision row D65 (and D62 for the interface-default fixes it needed). The user-facing description is the header of `std/encoding/xml/xml.alx`.

```ruby
import "encoding/xml"

#[derive(Xml)]
struct Person {
  #[field(xml: "urn:p person")] xml_name: xml.Name   # Go's XMLName
  #[field(xml: "id,attr")] id: Int
  #[field(xml: "name>first")] first: Str
  #[field(xml: "tag")] tags: [Str]
  #[field(xml: ",comment")] note: Str
  #[field(xml: ",embed")] addr: Address              # Go's anonymous field
}

s = ~p.to_xml                     # to_xml_indent(prefix, indent)
q = ~Person.from_xml(s)
b = ~xml.marshal(p)               # Go's Marshal; xml.unmarshal(data, v) -> ~T
```

## Where Go's reflection went

Go's encoding/xml does three things at run time with reflection. Here each has a place:

| Go | alx |
|---|---|
| typeinfo.go: tag parsing, XMLName, embedded-struct flattening, path conflicts (TagPathError) | the derive, at compile time (`compiler/src/derive_xml.rs`, `Infos::type_info`, a line-by-line port); its errors are compile errors |
| marshal.go / read.go per *field* (marshalStruct, marshalAttr, unmarshalPath, copyValue, the chardata/comment/innerxml rules) | code the derive writes per field, specialised to the field's type (`Int`, sized ints, `Float`, `Bool`, `Str`, `[Byte]`, `T?` as a pointer, `[T]`, `[T; N]`, `dyn.Value` as `interface{}`, named types) |
| marshal.go / read.go per *type* (Marshaler, TextMarshaler, the struct walk) | a protocol every named field type has (below) |

## The protocol

```ruby
def xml_enc(e: xml.Encoder, tmpl: xml.StartElement?, fname: xml.Name) -> ~Unit   # marshalValue
def xml_attr(name: xml.Name) -> ~xml.Attr?                                        # marshalAttr
def xml_chardata -> ~[Byte]?                                                     # chardata of a field
def xml_dec!(d: xml.Decoder, start: xml.StartElement) -> ~Unit                    # unmarshal
def xml_attr_dec!(a: xml.Attr) -> ~Unit                                          # unmarshalAttr
def xml_text_dec!(b: [Byte]) -> ~Unit                                            # copyValue of chardata
```

- **Derived types** get all six, plus `xml_path!` (Go's unmarshalPath), `to_xml`, `to_xml_indent` and `T.from_xml`. When the struct itself declares Go's methods, they take precedence in Go's order: `marshal_xml` then `marshal_text` for elements, `marshal_xml_attr` then `marshal_text` for attributes, `unmarshal_xml!` then `unmarshal_text!` for elements, `unmarshal_xml_attr!` then `unmarshal_text!` for attributes. The derive sees them in the struct body.
- **Hand-written types** (and std types with `marshal_text`, such as `time.Time`) get the protocol from the default methods of `xml.Marshaler`, `Unmarshaler`, `MarshalerAttr`, `UnmarshalerAttr`, `TextMarshaler` and `TextUnmarshaler` (std/encoding/xml/protocol.alx) once they have the required method, as Go's interfaces are satisfied. Their declaration order is Go's precedence; D62 made that deterministic. A type only needs the methods for the roles it is used in (generic instantiation): a Marshaler-only type can't be a field of a type that is also unmarshaled.
- `xml.Name` and `xml.Attr` implement the protocol by hand (a Name field records the element's name; an Attr in an `,any,attr` slice is the attribute itself).
- `marshal`, `unmarshal`, `Encoder.encode!`, `Decoder.decode!` are generic over any value with the protocol. Go's `Marshal` of a bare `int` or `[]string` has no equivalent (no methods on builtins); `marshal_value(dyn.Value)` covers dynamic data, and a `dyn.Value` field is Go's `interface{}` field.

## The tags

`#[field(xml: "...")]` is read from `Opts.tags` (derive.rs parses `#[field(...)]` once for every derive). Go's syntax and validation exactly: name, `"ns name"`, `attr`, `chardata`, `cdata`, `innerxml`, `comment`, `omitempty`, `any`, `any,attr`, `a>b>c` (a leading `>` takes the field name), `-`. The field named `xml_name` is Go's `XMLName`: its tag names and checks the element; of type `xml.Name` it also records the name (any other type only carries the tag, like Go's `struct{}`). One extension: `,embed` marks Go's anonymous struct field (alx has none); the embedded type must be a struct of the same module that derives Xml, `T?` is Go's `*T` (made when one of its fields is set, skipped when absent).

## Differences from Go (and why)

- Default element and attribute names are the alx field names (lower case); tags carry Go's names where they matter. `,any` can't take a name (Go's validation), so Go's `Any string ",any"` matches `<any>` here.
- An XMLName on a field's type is seen (Go's lookupXMLName) only for types of the same module: the derive can't read other modules' declarations.
- Invalid tags, chains with attr, conflicting paths (TagPathError) and unsupported field types (maps, functions, tuples) are compile errors at the struct, not run-time errors.
- No float32; messages of strconv errors name alx's functions; tokens own their bytes; end of input is none (Go's io.EOF), and `decode!` with no element fails with `XmlError.Eof`.
- `MAX_UNMARSHAL_DEPTH` is 1000 (Go 10000): the C backend's frames (port-issues #81) overflowed an 8 MB stack at about 3000 levels of an Unmarshaler recursing through `decode_element!`. Derived types can't recurse (recursive types don't exist), so the limit only matters there.
- The type name in "xml: T.MarshalXML wrote invalid XML" is the alx name for derived types and empty for hand-written ones (no run-time type names); a hand-written Marshaler at the top level without a field name or template gets an empty start name.

## Tests

- `std/encoding/xml/xml_test.alx`, `encode_test.alx`: Go's xml_test.go and the token half of marshal_test.go.
- `std/encoding/xml/vec_token_test.alx`: 1,089 cases generated from Go's Decoder (RawToken, Token, Token with Strict off and HTML auto-close) over Go's test inputs, malformed and truncated documents and random mutations: every token with InputOffset and InputPos, then EOF or the error.
- `std/encoding/xml/derives/marshal_test.alx`: the marshalTests table over translated types (TestMarshal and TestUnmarshal), `read_test.alx`: read_test.go, atom_test.go, the derive-using tests of xml_test.go and marshal_test.go, and the five examples (two of them hand-written Marshaler / TextMarshaler types using the interface defaults).
- `std/encoding/xml/derives/vec_marshal_test.alx`: 1,248 cases generated from Go: Marshal of random values of four struct shapes, Marshal(Unmarshal(output)), and Unmarshal of perturbed, truncated and hand-made malformed documents.
- Acceptance: `xmlderive` (S4, every backend and the Rust oracle) and `xmlderive.bad1`–`bad3` (the derive's compile errors); `ifacedefaults` (S4) for D62.

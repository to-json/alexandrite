//! `#[derive(Json)]`: compile-time generation of `encoding/json` code.
//!
//! A derive is expansion by source text. The parser records the declaration
//! (name, type parameters, fields with their `#[json(...)]` options); when the
//! whole module has been read, this module writes ordinary Alexandrite `def`s
//! for it as text, and the parser parses them back in as methods of the type.
//! The checker, the lowering and every backend see plain code and need no
//! knowledge of derives. Every generated token carries the span of the type's
//! declaration, so an error in generated code points at the `struct`/`enum`.
//!
//! What gets generated (`J` is the local name of the `encoding/json` import):
//!
//!   def json_enc(e: J.Encoder)                      writes the value to `e`
//!   def self.json_dec(d: J.Decoder) -> ~T<J.JsonError>   reads one value
//!   def to_json -> ~Str<J.JsonError>                compact JSON
//!   def to_json_indent(prefix, indent) -> ~Str<..>  like Go's MarshalIndent
//!   def self.from_json(_s: Str) -> ~T<J.JsonError>   one whole document
//!
//! Encodings (see `std/encoding/json/json.alx` for the full table):
//! structs are objects; Int/sized ints/Float/Bool/Str are numbers, booleans
//! and strings; `T?` is the value or `null`; `[T]` an array; `Map[Str, V]` an
//! object; tuples are arrays; a payload-free enum variant is a string, a
//! variant with fields is `{"Variant": payload}` (one unnamed field: the
//! value itself; several: an array; named fields: an object).

use crate::ast::*;
use crate::diag::{Diag, Span};
use std::collections::{HashMap, HashSet};

/// The options on a field or variant (or a method, for derive(Data)).
///
/// Every derive reads one attribute form, Go's struct tag in alx syntax
/// (GO-VS-RUBY S5): `#[field(json: "id,omitempty", data: "ID", xml: "id,attr")]`,
/// each derive reading its own key; a key no derive here knows is kept in
/// `tags` for the derive that does. The per-derive shorthands
/// `#[json(...)]` and `#[data(...)]` say the same.
#[derive(Default, Clone, Debug)]
pub struct Opts {
    /// derive(Json): `json: "name"`, `json: "-"`, `json: ",omitempty"`.
    pub rename: Option<String>,
    pub skip: bool,
    pub omit_empty: bool,
    /// encoding/json/v2's tag options (derive_json2.rs): omitzero, string,
    /// case:ignore (1) / case:strict (2), embed, format:...
    pub omit_zero: bool,
    pub string_tag: bool,
    pub casing: u8,
    pub embed: bool,
    pub format: Option<String>,
    /// derive(Data): `data: "Name"` / `data: "-"`.
    pub drename: Option<String>,
    pub dskip: bool,
    /// Every `key: "value"` of `#[field(...)]`, in order.
    pub tags: Vec<(String, String)>,
    /// derive(Asn1): `asn1: "optional,explicit,tag:0"` (or `#[asn1(...)]`).
    pub asn1: crate::derive_asn1::A1,
}

impl Opts {
    /// The value of `#[field(key: "...")]` (`db` for derive(Row)).
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Whether `text` is an attribute that sets field options: `json(...)`,
/// `data(...)` or `field(...)`.
pub fn is_field_attr(text: &str) -> bool {
    let t = text.trim_start();
    ["json", "data", "asn1", "field"].iter().any(|k| t.strip_prefix(k).is_some_and(|r| r.trim_start().starts_with('(')))
}

/// Apply one field-options attribute (`#[json(...)]`, `#[data(...)]`,
/// `#[field(...)]`) to `o`.
pub fn apply_field_attr(text: &str, sp: Span, o: &mut Opts) -> Result<(), Diag> {
    let t = text.trim();
    if t.starts_with("json") {
        return apply_json_attr(text, sp, o);
    }
    if t.starts_with("data") {
        return apply_data_attr(text, sp, o);
    }
    if t.starts_with("asn1") {
        return crate::derive_asn1::apply_asn1_attr(text, sp, &mut o.asn1);
    }
    let inner = t.strip_prefix("field").map(str::trim_start).and_then(|r| r.strip_prefix('(')).and_then(|r| r.trim_end().strip_suffix(')'));
    let Some(inner) = inner else {
        return Err(Diag::new(sp, format!("unknown attribute `#[{text}]`")));
    };
    let note = "write Go's struct tag as `#[field(json: \"id,omitempty\", data: \"ID\", xml: \"id,attr\")]`";
    for a in split_args(inner) {
        let a = a.trim();
        if a.is_empty() {
            continue;
        }
        let Some((k, v)) = a.split_once(':') else {
            return Err(Diag::new(sp, format!("`{a}` in `#[field(...)]` isn't `key: \"value\"`")).note(note));
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Diag::new(sp, format!("`{k}` isn't a tag key")).note(note));
        }
        let v = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).ok_or_else(|| Diag::new(sp, format!("the `{k}` tag's value must be a string")).note(note))?;
        let v = unescape(v);
        if o.tags.iter().any(|(k2, _)| k2 == k) {
            return Err(Diag::new(sp, format!("the `{k}` tag is given twice")));
        }
        match k {
            "json" => apply_json_tag(&v, sp, o)?,
            "data" => apply_data_tag(&v, o),
            "asn1" => crate::derive_asn1::apply_asn1_attr(&format!("asn1({v})"), sp, &mut o.asn1)?,
            _ => {}
        }
        o.tags.push((k.to_string(), v));
    }
    Ok(())
}

/// Go's json tag: `name`, `-`, then options: omitempty, and encoding/json/v2's
/// omitzero, string, case:ignore|strict, embed, format:F (last).
fn apply_json_tag(v: &str, sp: Span, o: &mut Opts) -> Result<(), Diag> {
    if v == "-" {
        o.skip = true;
        return Ok(());
    }
    let (head, fmt) = match v.find(",format:") {
        Some(i) => (&v[..i], Some(v[i + ",format:".len()..].to_string())),
        None => (v, None),
    };
    let mut parts = head.split(',');
    let name = parts.next().unwrap_or("");
    if !name.is_empty() {
        o.rename = Some(name.to_string());
    }
    for p in parts {
        match p.trim() {
            "omitempty" | "omit_empty" => o.omit_empty = true,
            "omitzero" => o.omit_zero = true,
            "string" => o.string_tag = true,
            "embed" | "inline" => o.embed = true,
            "case:ignore" | "nocase" => o.casing = 1,
            "case:strict" | "strictcase" => o.casing = 2,
            "" => {}
            // Go ignores options it doesn't know (a struct tag is free text)
            _ => {}
        }
    }
    if let Some(f) = fmt {
        if f.is_empty() || f.contains(',') {
            return Err(Diag::new(sp, format!("bad `format:` in json tag `{v}`")).note("format:F comes last: `format:base64`"));
        }
        o.format = Some(f);
    }
    Ok(())
}

/// The data tag: `Name` (the name in the value tree) or `-` (left out).
fn apply_data_tag(v: &str, o: &mut Opts) {
    if v == "-" {
        o.dskip = true;
    } else if !v.is_empty() {
        o.drename = Some(v.to_string());
    }
}

/// `#[data("Name")]` / `#[data(skip)]`.
pub fn apply_data_attr(text: &str, sp: Span, o: &mut Opts) -> Result<(), Diag> {
    let (r, s) = parse_data_attr(text, sp)?;
    if r.is_some() {
        o.drename = r;
    }
    o.dskip |= s;
    Ok(())
}

/// Parse `#[data("Name")]` / `#[data(skip)]`: (rename, skip).
pub fn parse_data_attr(text: &str, sp: Span) -> Result<(Option<String>, bool), Diag> {
    let inner = text.trim().strip_prefix("data").map(str::trim_start).and_then(|r| r.strip_prefix('(')).and_then(|r| r.trim_end().strip_suffix(')'));
    let Some(inner) = inner else {
        return Err(Diag::new(sp, format!("unknown attribute `#[{text}]`")).note("known: #[data(\"Name\")], #[data(skip)]"));
    };
    let (mut rename, mut skip) = (None, false);
    for a in split_args(inner) {
        let a = a.trim();
        if a.is_empty() {
            continue;
        }
        if let Some(s) = a.strip_prefix('"') {
            rename = Some(unescape(s.strip_suffix('"').ok_or_else(|| Diag::new(sp, "unterminated string in `#[data(...)]`"))?));
        } else if a == "skip" {
            skip = true;
        } else {
            return Err(Diag::new(sp, format!("unknown data option `{a}`")).note("options: \"Name\" (rename), skip"));
        }
    }
    Ok((rename, skip))
}

/// Parse the text of one `#[json(...)]` attribute into `o`.
pub fn apply_json_attr(text: &str, sp: Span, o: &mut Opts) -> Result<(), Diag> {
    let inner = text.trim().strip_prefix("json").map(str::trim_start).and_then(|r| r.strip_prefix('(')).and_then(|r| r.trim_end().strip_suffix(')'));
    let Some(inner) = inner else {
        return Err(Diag::new(sp, format!("unknown attribute `#[{text}]`")).note("known: #[json(\"name\")], #[json(omit_empty)], #[json(skip)] on fields; #[derive(Json)] on types; #[pure] on defs"));
    };
    for a in split_args(inner) {
        let a = a.trim();
        if a.is_empty() {
            continue;
        }
        if let Some(s) = a.strip_prefix('"') {
            let s = s.strip_suffix('"').ok_or_else(|| Diag::new(sp, "unterminated string in `#[json(...)]`"))?;
            o.rename = Some(unescape(s));
        } else {
            match a {
                "skip" => o.skip = true,
                "omit_empty" | "omitempty" => o.omit_empty = true,
                _ => return Err(Diag::new(sp, format!("unknown json option `{a}`")).note("options: \"name\" (rename), omit_empty, skip")),
            }
        }
    }
    Ok(())
}

/// Split on commas outside of strings.
fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let (mut in_str, mut esc) = (false, false);
    for c in s.chars() {
        if in_str {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == ',' {
            out.push(std::mem::take(&mut cur));
        } else {
            if c == '"' {
                in_str = true;
            }
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Is this attribute `#[json(transparent)]` (before a type)? Other `json(...)`
/// attributes there are an error.
pub fn is_transparent_attr(text: &str, sp: Span) -> Result<bool, Diag> {
    let t = text.trim();
    let Some(r) = t.strip_prefix("json") else { return Ok(false) };
    let inner = r.trim_start().strip_prefix('(').and_then(|r| r.trim_end().strip_suffix(')')).map(str::trim);
    match inner {
        Some("transparent") => Ok(true),
        _ => Err(Diag::new(sp, format!("unknown attribute `#[{text}]` on a type")).note("before a type: #[json(transparent)] (a one-field struct encoded as its field); #[json(...)] options go on fields")),
    }
}

/// The names in `#[derive(A, B)]`.
pub fn derive_names(text: &str, sp: Span) -> Result<Option<Vec<String>>, Diag> {
    let Some(rest) = text.trim().strip_prefix("derive") else { return Ok(None) };
    let inner = rest.trim_start().strip_prefix('(').and_then(|r| r.trim_end().strip_suffix(')')).ok_or_else(|| Diag::new(sp, "write `#[derive(Json)]`"))?;
    let names: Vec<String> = inner.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    for n in &names {
        // Eq and Show are structural already (D13, D14): accepted, nothing to generate.
        if !matches!(n.as_str(), "Json" | "Asn1" | "Data" | "Gob" | "Xml" | "Arbitrary" | "Row" | "Eq" | "Show") {
            return Err(Diag::new(sp, format!("can't derive `{n}`")).note("derivable: Json (generates to_json / from_json), Asn1 (encoding/asn1: to_asn1 / from_asn1), Data (a dynamic dyn.Value tree: to_data / from_data), Xml (encoding/xml: to_xml / from_xml and the Marshal protocol), Arbitrary (generates `self.arbitrary` for testing/quick), Row (database/sql: `Type.from_sql_row`); Eq and Show are accepted and need nothing: structs, tuples and enums already compare and print field by field"));
        }
    }
    Ok(Some(names))
}

#[derive(Clone, Debug)]
pub struct DField {
    pub name: String,
    pub ty: TypeExpr,
    pub opts: Opts,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct DVariant {
    pub name: String,
    pub fields: Vec<DField>,
    pub opts: Opts,
}

#[derive(Clone, Debug)]
pub enum DShape {
    Struct(Vec<DField>),
    Enum(Vec<DVariant>),
}

#[derive(Clone, Debug)]
pub struct DeriveJob {
    /// `Json`, `Asn1`, `Arbitrary` or `Row`.
    pub derive: String,
    /// Type-level `#[asn1(set)]` / `#[asn1(transparent)]` (derive(Asn1)).
    pub type_opts: crate::derive_asn1::A1,
    pub name: String,
    /// `#[data("pkg.T")]`: the Go type name consumers see (json/v2 errors).
    pub go_name: Option<String>,
    /// `#[json(transparent)]`: a struct with one field is encoded as that field.
    pub transparent: bool,
    pub tparams: Vec<String>,
    pub public: bool,
    pub span: Span,
    pub shape: DShape,
}

/// What the module declares, so a field naming one of its types can be checked.
pub struct ModuleTypes {
    /// type name -> is it derived
    pub local: HashMap<String, bool>,
    pub enums: HashSet<String>,
}

pub fn type_src(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named(n, _) => n.clone(),
        TypeExpr::Array(e, _) => format!("[{}]", type_src(e)),
        TypeExpr::Opt(e, _) => format!("{}?", type_src(e)),
        TypeExpr::Fixed(e, n, _) => match &n.kind {
            ExprKind::Int(v) => format!("[{}; {v}]", type_src(e)),
            _ => format!("[{}; _]", type_src(e)),
        },
        TypeExpr::App(n, args, _) => format!("{n}[{}]", args.iter().map(type_src).collect::<Vec<_>>().join(", ")),
        TypeExpr::Result(e, _, _) => format!("~{}", type_src(e)),
        TypeExpr::Handle(n, args, _) if args.is_empty() => format!("@{n}"),
        TypeExpr::Handle(n, args, _) => format!("@{n}[{}]", args.iter().map(type_src).collect::<Vec<_>>().join(", ")),
        TypeExpr::Fn(ps, r, _) => format!("({}) -> {}", ps.iter().map(type_src).collect::<Vec<_>>().join(", "), type_src(r)),
        TypeExpr::Tuple(ts, _) => format!("({})", ts.iter().map(type_src).collect::<Vec<_>>().join(", ")),
    }
}

/// A string as an Alexandrite literal.
fn lit(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '#' => o.push_str("\\#"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => o.push_str(&format!("\\x{:02x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A string as JSON text (Go's rules, with HTML escaping on).
fn json_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => o.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A piece of an encoded value: literal JSON text, or a Str expression.
enum Frag {
    Lit(String),
    Ex(String),
}

/// One interpolated string literal for the pieces.
fn frags_expr(fr: &[Frag]) -> String {
    let mut o = String::from("\"");
    let mut lit_acc = String::new();
    let flush = |o: &mut String, acc: &mut String| {
        if !acc.is_empty() {
            let l = lit(acc);
            o.push_str(&l[1..l.len() - 1]);
            acc.clear();
        }
    };
    for f in fr {
        match f {
            Frag::Lit(t) => lit_acc.push_str(t),
            Frag::Ex(e) => {
                flush(&mut o, &mut lit_acc);
                o.push_str("#{");
                o.push_str(e);
                o.push('}');
            }
        }
    }
    flush(&mut o, &mut lit_acc);
    o.push('"');
    o
}

#[derive(Clone)]
enum Ty<'a> {
    Int(IntKind),
    Float,
    Bool,
    Str,
    Opt(&'a TypeExpr),
    Arr(&'a TypeExpr),
    /// `[T; N]` with N a literal.
    Fixed(&'a TypeExpr, i64),
    /// key, value
    Map(&'a TypeExpr, &'a TypeExpr),
    Tuple(&'a [TypeExpr]),
    Named(&'a str),
}

/// What a map key is: its text, read back.
enum KeyTy {
    Str,
    Int(IntKind),
    Named,
}

struct Gen<'a> {
    alias: &'a str,
    /// In a file that uses encoding/json/v2, a field type v1 has no encoding
    /// for (Complex, ...) fails at run time in v1 instead of failing the derive.
    lenient: bool,
    /// Types only v2 encodes (jsontext.Value under the file's alias).
    v2_only: Vec<String>,
    tparams: &'a [String],
    types: &'a ModuleTypes,
    owner: &'a str,
    out: String,
    n: usize,
}

type GResult<T> = Result<T, String>;

/// A field of a struct's JSON object after Go's embedding rules: its key and
/// the chain of fields from the struct to it (embedded structs, then the field).
#[derive(Clone)]
struct RField<'a> {
    name: String,
    tagged: bool,
    index: Vec<usize>,
    /// (field, the type it belongs to)
    chain: Vec<(&'a DField, String)>,
}

impl<'a> RField<'a> {
    fn leaf(&self) -> &'a DField {
        self.chain.last().unwrap().0
    }
}

/// The embedded struct a field names (`#[field(json: ",embed")]` on a field of
/// a struct type derived in this file, or an optional one), if it is one.
fn embedded<'a>(f: &DField, jobs: &'a [DeriveJob]) -> Option<(&'a DeriveJob, bool)> {
    if !f.opts.embed || f.opts.rename.is_some() {
        return None;
    }
    let (t, opt) = match &f.ty {
        TypeExpr::Opt(t, _) => (&**t, true),
        t => (t, false),
    };
    let TypeExpr::Named(n, _) = t else { return None };
    let j = jobs.iter().find(|j| &j.name == n && j.derive == "Json")?;
    match &j.shape {
        DShape::Struct(_) if !j.transparent => Some((j, opt)),
        _ => None,
    }
}

/// Does t name a type of the program (not a builtin such as Int), maybe optional?
fn names_user_type(t: &TypeExpr) -> bool {
    match t {
        TypeExpr::Opt(e, _) => names_user_type(e),
        TypeExpr::Named(n, _) => IntKind::from_name(n).is_none() && !matches!(n.as_str(), "Float" | "F64" | "Bool" | "Str" | "Complex" | "Unit"),
        _ => false,
    }
}

/// Go's typeFields: the fields of the object for `root`, with embedded
/// structs' fields promoted (breadth first; a shallower field hides deeper
/// ones of the same name, at one depth a tagged one wins, and two equal
/// ones both disappear), in field order.
fn resolve_fields<'a>(root: &'a DeriveJob, jobs: &'a [DeriveJob], lenient: bool) -> Result<Vec<RField<'a>>, String> {
    let mut fields: Vec<RField<'a>> = vec![];
    let mut next: Vec<(&'a DeriveJob, Vec<usize>, Vec<(&'a DField, String)>)> = vec![(root, vec![], vec![])];
    let mut next_count: HashMap<String, usize> = HashMap::new();
    let mut visited: HashSet<String> = HashSet::new();
    while !next.is_empty() {
        let current = std::mem::take(&mut next);
        let count = std::mem::take(&mut next_count);
        for (job, index, chain) in current {
            if !visited.insert(job.name.clone()) {
                continue;
            }
            let DShape::Struct(fs) = &job.shape else { continue };
            for (i, sf) in fs.iter().enumerate() {
                if sf.opts.skip {
                    continue;
                }
                let mut idx = index.clone();
                idx.push(i);
                let mut ch = chain.clone();
                ch.push((sf, job.name.clone()));
                if sf.opts.embed && sf.opts.rename.is_none() {
                    match embedded(sf, jobs) {
                        Some((ej, _)) => {
                            let c = next_count.entry(ej.name.clone()).or_insert(0);
                            *c += 1;
                            if *c == 1 {
                                next.push((ej, idx, ch));
                            }
                            continue;
                        }
                        // Go ignores the option on a field that can't be embedded
                        // (an Int); a named type must be a struct derived here.
                        None if lenient || !names_user_type(&sf.ty) => {}
                        None => return Err(format!("`{}` is embedded, but its type isn't a struct that derives Json in this file", sf.name)),
                    }
                }
                let f = RField { name: sf.opts.rename.clone().unwrap_or_else(|| sf.name.clone()), tagged: sf.opts.rename.is_some(), index: idx, chain: ch };
                fields.push(f.clone());
                if count.get(&job.name).copied().unwrap_or(0) > 1 {
                    fields.push(f);
                }
            }
        }
    }
    fields.sort_by(|a, b| a.name.cmp(&b.name).then(a.index.len().cmp(&b.index.len())).then(b.tagged.cmp(&a.tagged)).then(a.index.cmp(&b.index)));
    let mut out = vec![];
    let mut i = 0;
    while i < fields.len() {
        let mut j = i + 1;
        while j < fields.len() && fields[j].name == fields[i].name {
            j += 1;
        }
        let g = &fields[i..j];
        if g.len() == 1 || g[0].index.len() != g[1].index.len() || g[0].tagged != g[1].tagged {
            out.push(g[0].clone());
        }
        i = j;
    }
    out.sort_by(|a, b| a.index.cmp(&b.index));
    Ok(out)
}

impl<'a> Gen<'a> {
    fn w(&mut self, ind: usize, s: impl AsRef<str>) {
        for _ in 0..ind {
            self.out.push_str("  ");
        }
        self.out.push_str(s.as_ref());
        self.out.push('\n');
    }
    fn fresh(&mut self, base: &str) -> String {
        self.n += 1;
        format!("_{base}{}", self.n)
    }

    fn classify(&self, t: &'a TypeExpr) -> GResult<Ty<'a>> {
        Ok(match t {
            // A module's own `Int`/`Float` (math/big's) shadows the builtin;
            // it brings its own json_enc / json_dec.
            TypeExpr::Named(n, _) if self.shadowed(n) => Ty::Named(n),
            TypeExpr::Named(n, _) => {
                if let Some(k) = IntKind::from_name(n) {
                    Ty::Int(k)
                } else if n == "Float" || n == "F64" {
                    Ty::Float
                } else if n == "Bool" {
                    Ty::Bool
                } else if n == "Str" {
                    Ty::Str
                } else if self.tparams.contains(n) {
                    return Err(format!("the type parameter `{n}` can't be encoded: derive(Json) on a generic type isn't supported yet (use a concrete type)"));
                } else if matches!(n.as_str(), "Float32" | "F32" | "Ptr" | "Unit" | "Complex") {
                    return Err(format!("{n} has no JSON encoding"));
                } else {
                    Ty::Named(n)
                }
            }
            TypeExpr::Opt(e, _) => Ty::Opt(e),
            TypeExpr::Array(e, _) => Ty::Arr(e),
            TypeExpr::App(n, args, _) if n == "Map" && args.len() == 2 => {
                self.key_ty(&args[0])?;
                Ty::Map(&args[0], &args[1])
            }
            TypeExpr::App(n, ..) => return Err(format!("`{n}[...]`: generic types can't be derived yet")),
            TypeExpr::Tuple(ts, _) => Ty::Tuple(ts),
            TypeExpr::Fixed(e, n, _) => match &n.kind {
                ExprKind::Int(v) => Ty::Fixed(e, *v as i64),
                _ => return Err("a fixed-size array `[T; N]` needs a literal N for derive(Json)".to_string()),
            },
            TypeExpr::Result(..) => return Err("a fallible type has no JSON encoding".to_string()),
            TypeExpr::Handle(..) => return Err("a pool handle `@T` has no JSON encoding".to_string()),
            TypeExpr::Fn(..) => return Err("a function type has no JSON encoding".to_string()),
        })
    }

    /// Go's map keys: strings, integers, and TextMarshalers (named types).
    fn key_ty(&self, k: &TypeExpr) -> GResult<KeyTy> {
        match k {
            TypeExpr::Named(n, _) if n == "Str" => Ok(KeyTy::Str),
            TypeExpr::Named(n, _) if IntKind::from_name(n).is_some() && !self.shadowed(n) => Ok(KeyTy::Int(IntKind::from_name(n).unwrap())),
            TypeExpr::Named(n, _) if !matches!(n.as_str(), "Float" | "F64" | "Bool" | "Complex" | "Unit") => Ok(KeyTy::Named),
            k => Err(format!("map keys must be Str, an integer type or a TextMarshaler to be JSON object keys, not {}", type_src(k))),
        }
    }

    fn is_byte(t: &TypeExpr) -> bool {
        matches!(t, TypeExpr::Named(n, _) if n == "Byte" || n == "U8")
    }

    /// The pieces of the Str that encodes `x` (an expression without side effects),
    /// after any statements it needs (temporaries) are written at `ind`. A value
    /// becomes one interpolated string, so a record costs one allocation and one
    /// push into the encoder, however many fields it has. `quoted`: the `,string`
    /// option (scalars inside a JSON string).
    fn enc(&mut self, t: &'a TypeExpr, x: &str, ind: usize, quoted: bool) -> GResult<Vec<Frag>> {
        if self.lenient && (self.classify(t).is_err() || self.v2_only_type(t)) && self.zero(t).is_some() {
            return Ok(vec![Frag::Ex(format!("_e.unsupported!({})", lit(&type_src(t))))]);
        }
        let q = |v: Vec<Frag>| -> Vec<Frag> {
            if quoted {
                let mut o = vec![Frag::Lit("\"".into())];
                o.extend(v);
                o.push(Frag::Lit("\"".into()));
                o
            } else {
                v
            }
        };
        Ok(match self.classify(t)? {
            Ty::Int(_) | Ty::Bool => q(vec![Frag::Ex(x.to_string())]),
            Ty::Float => q(vec![Frag::Ex(format!("_e.f!({x})"))]),
            Ty::Str if quoted => vec![Frag::Ex(format!("_e.q(_e.q({x}))"))],
            Ty::Str => vec![Frag::Ex(format!("_e.q({x})"))],
            Ty::Named(n) if quoted && n == format!("{}.Number", self.alias) => vec![Frag::Ex(format!("{}.enc_qnumber(_e, {x})", self.alias))],
            Ty::Named(_) => vec![Frag::Ex(format!("{}.enc_val(_e, {x})", self.alias))],
            Ty::Opt(inner) => {
                let (o, v) = (self.fresh("o"), self.fresh("v"));
                self.w(ind, format!("{o} = \"null\""));
                self.w(ind, format!("if {v} = {x} {{"));
                let fr = self.enc(inner, &v, ind + 1, quoted)?;
                self.w(ind + 1, format!("{o} = {}", frags_expr(&fr)));
                self.w(ind, "}");
                vec![Frag::Ex(o)]
            }
            Ty::Arr(inner) if Self::is_byte(inner) => vec![Frag::Ex(format!("{}.bytes_text({x})", self.alias))],
            Ty::Arr(inner) | Ty::Fixed(inner, _) => {
                let (a, v) = (self.fresh("a"), self.fresh("x"));
                self.w(ind, format!("{a}: [Str] = []"));
                self.w(ind, format!("for {v} in {x} {{"));
                let fr = self.enc(inner, &v, ind + 1, false)?;
                self.w(ind + 1, format!("{a} << {}", frags_expr(&fr)));
                self.w(ind, "}");
                vec![Frag::Lit("[".into()), Frag::Ex(format!("{a}.join(\",\")")), Frag::Lit("]".into())]
            }
            Ty::Map(kt, inner) => {
                let (ks, ps, k, v, kx) = (self.fresh("ks"), self.fresh("ps"), self.fresh("k"), self.fresh("v"), self.fresh("kt"));
                self.w(ind, format!("{ks}: [Str] = []"));
                self.w(ind, format!("{ps}: [Str] = []"));
                self.w(ind, format!("for {k}, {v} in {x} {{"));
                let ktext = match self.key_ty(kt)? {
                    KeyTy::Str => k.clone(),
                    KeyTy::Int(_) => format!("{k}.to_s"),
                    KeyTy::Named => format!("{}.key_text(_e, {k}, {})", self.alias, lit(&type_src(t))),
                };
                self.w(ind + 1, format!("{kx} = {ktext}"));
                let mut fr = vec![Frag::Ex(format!("_e.q({kx})")), Frag::Lit(":".into())];
                fr.extend(self.enc(inner, &v, ind + 1, false)?);
                self.w(ind + 1, format!("{ks} << {kx}"));
                self.w(ind + 1, format!("{ps} << {}", frags_expr(&fr)));
                self.w(ind, "}");
                vec![Frag::Ex(format!("{}.sorted_object({ks}, {ps})", self.alias))]
            }
            Ty::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                self.w(ind, format!("{} = {x}", names.join(", ")));
                let mut fr = vec![Frag::Lit("[".into())];
                for (k, (t, n)) in ts.iter().zip(&names).enumerate() {
                    if k > 0 {
                        fr.push(Frag::Lit(",".into()));
                    }
                    fr.extend(self.enc(t, n, ind, false)?);
                }
                fr.push(Frag::Lit("]".into()));
                fr
            }
        })
    }

    fn v2_only_type(&self, t: &TypeExpr) -> bool {
        matches!(t, TypeExpr::Named(n, _) if self.v2_only.contains(n))
    }

    /// Does the module declare its own type named like a builtin (`Int`)?
    fn shadowed(&self, n: &str) -> bool {
        crate::check::SHADOWABLE.contains(&n) && self.types.local.contains_key(n)
    }

    /// A test for "empty" (Go's omitempty), or None if the type is never empty.
    fn empty(&self, t: &TypeExpr, x: &str) -> Option<String> {
        match t {
            TypeExpr::Named(n, _) if self.shadowed(n) => None,
            TypeExpr::Named(n, _) if IntKind::from_name(n).is_some() => Some(format!("{x} == 0")),
            TypeExpr::Named(n, _) if n == "Float" || n == "F64" => Some(format!("{x} == 0.0")),
            TypeExpr::Named(n, _) if n == "Bool" => Some(format!("!{x}")),
            TypeExpr::Named(n, _) if n == "Str" => Some(format!("{x}.size == 0")),
            TypeExpr::Array(..) => Some(format!("{x}.size == 0")),
            TypeExpr::Fixed(_, n, _) => match &n.kind {
                ExprKind::Int(0) => Some("true".into()),
                _ => None,
            },
            TypeExpr::App(n, ..) if n == "Map" => Some(format!("{x}.size == 0")),
            TypeExpr::Opt(..) => Some(format!("{x}.none?")),
            // Go's RawMessage is a []byte, Number a string, any an interface
            TypeExpr::Named(n, _) if *n == format!("{}.RawMessage", self.alias) => Some(format!("{x}.data.size == 0")),
            TypeExpr::Named(n, _) if *n == format!("{}.Number", self.alias) => Some(format!("{x}.to_s.size == 0")),
            TypeExpr::Named(n, _) if *n == format!("{}.Value", self.alias) => Some(format!("{x}.null?")),
            _ => None,
        }
    }

    /// A test for "zero" (Go's omitzero: reflect's IsZero, or the type's own
    /// IsZero method, `zero?` in alx).
    fn is_zero(&mut self, t: &TypeExpr, x: &str) -> String {
        match t {
            TypeExpr::Named(n, _) if self.shadowed(n) => format!("{}.is_zero({x})", self.alias),
            TypeExpr::Named(n, _) if IntKind::from_name(n).is_some() => format!("{x} == 0"),
            // (Go 1.27 omits -0 too)
            TypeExpr::Named(n, _) if n == "Float" || n == "F64" => format!("{x} == 0.0"),
            TypeExpr::Named(n, _) if n == "Bool" => format!("!{x}"),
            TypeExpr::Named(n, _) if n == "Str" => format!("{x}.size == 0"),
            TypeExpr::Named(..) => format!("{}.is_zero({x})", self.alias),
            TypeExpr::Array(..) => format!("{x}.size == 0"),
            TypeExpr::App(n, ..) if n == "Map" => format!("{x}.size == 0"),
            TypeExpr::Opt(..) => format!("{x}.none?"),
            TypeExpr::Fixed(e, ..) => {
                let w = self.fresh("w");
                let inner = self.is_zero(e, &w);
                format!("{x}.all? {{ |{w}| {inner} }}")
            }
            TypeExpr::Tuple(ts, _) => {
                if ts.is_empty() {
                    return "true".into();
                }
                let parts: Vec<String> = (0..ts.len()).map(|i| self.is_zero(&ts[i], &format!("{x}[{i}]"))).collect();
                format!("({})", parts.join(" && "))
            }
            _ => "false".into(),
        }
    }

    /// The pieces of `{"k":v,...}` for named fields held in `vals` (expressions),
    /// each behind `guards` (optional embedded structs that must be present:
    /// (variable, expression)).
    fn enc_object(&mut self, fields: &[(&'a DField, String, String)], guards: &[Vec<(String, String)>], ind: usize) -> GResult<Vec<Frag>> {
        let key_frag = |k: &str| -> Frag {
            let q = json_quote(k);
            if q.contains("\\u00") {
                Frag::Ex(format!("_e.q({})", lit(k)))
            } else {
                Frag::Lit(q)
            }
        };
        let cond_of = |g: &mut Gen<'a>, f: &'a DField, v: &str| -> Option<String> {
            let mut cs = vec![];
            if f.opts.omit_empty {
                if let Some(c) = g.empty(&f.ty, v) {
                    cs.push(c);
                }
            }
            if f.opts.omit_zero {
                cs.push(g.is_zero(&f.ty, v));
            }
            if cs.is_empty() {
                None
            } else {
                Some(cs.join(" || "))
            }
        };
        let dynamic = fields.iter().enumerate().any(|(k, (f, _, v))| !guards[k].is_empty() || cond_of(self, f, v).is_some());
        if !dynamic {
            if fields.is_empty() {
                return Ok(vec![Frag::Lit("{}".into())]);
            }
            let mut fr = vec![];
            for (pos, (f, key, v)) in fields.iter().enumerate() {
                fr.push(Frag::Lit(if pos == 0 { "{" } else { "," }.into()));
                fr.push(key_frag(key));
                fr.push(Frag::Lit(":".into()));
                fr.extend(self.enc(&f.ty, v, ind, f.opts.string_tag)?);
            }
            fr.push(Frag::Lit("}".into()));
            return Ok(fr);
        }
        let ps = self.fresh("ps");
        self.w(ind, format!("{ps}: [Str] = []"));
        for (k, (f, key, v)) in fields.iter().enumerate() {
            let mut ind2 = ind;
            for (var, ex) in &guards[k] {
                self.w(ind2, format!("if {var} = {ex} {{"));
                ind2 += 1;
            }
            let cond = cond_of(self, f, v);
            if let Some(c) = &cond {
                self.w(ind2, format!("if !({c}) {{"));
                ind2 += 1;
            }
            let mut fr = vec![key_frag(key), Frag::Lit(":".into())];
            fr.extend(self.enc(&f.ty, v, ind2, f.opts.string_tag)?);
            self.w(ind2, format!("{ps} << {}", frags_expr(&fr)));
            while ind2 > ind {
                ind2 -= 1;
                self.w(ind2, "}");
            }
        }
        Ok(vec![Frag::Lit("{".into()), Frag::Ex(format!("{ps}.join(\",\")")), Frag::Lit("}".into())])
    }

    /// Statements that read a value of type `t` from `_d` into a new local
    /// `dest`; `old` is the value it replaces (Go: null and a type error leave
    /// it; objects merge into it). `quoted`: the `,string` option.
    fn dec(&mut self, t: &'a TypeExpr, dest: &str, old: &str, ind: usize, quoted: bool) -> GResult<()> {
        let a = self.alias.to_string();
        if self.lenient && (self.classify(t).is_err() || self.v2_only_type(t)) {
            if let Some(z) = self.zero(t) {
                self.w(ind, format!("{dest}: {} = {z}", type_src(t)));
                self.w(ind, "_d.skip!");
                self.w(ind, format!("_d.fail!({a}.JsonError.UnsupportedType({}))", lit(&type_src(t))));
                return Ok(());
            }
        }
        match self.classify(t)? {
            Ty::Int(k) => {
                let (sm, um) = if quoted { ("qsint!", "quint!") } else { ("sint!", "uint!") };
                match k {
                    IntKind::I64 if !quoted => self.w(ind, format!("{dest} = _d.int!({old})")),
                    IntKind::I64 => self.w(ind, format!("{dest} = _d.qsint!(64, {old})")),
                    IntKind::U64 => self.w(ind, format!("{dest} = _d.{um}(64, {old})")),
                    k if k.signed() => self.w(ind, format!("{dest} = _d.{sm}({}, {old}.to_i).as_{}", k.bits(), k.method())),
                    k => self.w(ind, format!("{dest} = _d.{um}({}, {old}.to_u64).as_{}", k.bits(), k.method())),
                }
            }
            Ty::Float => self.w(ind, format!("{dest} = _d.{}({old})", if quoted { "qfloat!" } else { "float!" })),
            Ty::Bool => self.w(ind, format!("{dest} = _d.{}({old})", if quoted { "qbool!" } else { "bool!" })),
            Ty::Str => self.w(ind, format!("{dest} = _d.{}({old})", if quoted { "qstr!" } else { "str!" })),
            Ty::Named(n) if quoted && n == format!("{a}.Number") => self.w(ind, format!("{dest} = {a}.dec_qnumber(_d, {old})")),
            Ty::Named(_) if old.ends_with(".zero_value()") => {
                let z = self.fresh("z");
                self.w(ind, format!("{z}: {} = {old}", type_src(t)));
                self.w(ind, format!("{dest} = {a}.dec_val(_d, {z})"));
            }
            Ty::Named(_) => self.w(ind, format!("{dest} = {a}.dec_val(_d, {old})")),
            Ty::Opt(inner) => {
                let (v, ov) = (self.fresh("v"), self.fresh("ov"));
                let z = self.zero(inner).ok_or_else(|| format!("{} has no zero value to decode into", type_src(inner)))?;
                self.w(ind, format!("{dest}: {} = {old}", type_src(t)));
                self.w(ind, if quoted { "if _d.take_null! || _d.qnull! {" } else { "if _d.take_null! {" });
                self.w(ind + 1, format!("{dest} = none"));
                self.w(ind, "} else {");
                self.w(ind + 1, format!("{ov}: {} = {old} || {z}", type_src(inner)));
                self.dec(inner, &v, &ov, ind + 1, quoted)?;
                self.w(ind + 1, format!("{dest} = {v}"));
                self.w(ind, "}");
            }
            Ty::Arr(inner) if Self::is_byte(inner) => self.w(ind, format!("{dest} = _d.bytes!({old}, {})", lit(&type_src(t)))),
            Ty::Arr(inner) => {
                let (v, st, acc) = (self.fresh("v"), self.fresh("st"), self.fresh("a"));
                let z = self.zero(inner).ok_or_else(|| format!("{} has no zero value to decode into", type_src(inner)))?;
                self.w(ind, format!("{dest}: {} = {old}", type_src(t)));
                self.w(ind, format!("{st} = _d.open!(91, {})", lit(&type_src(t))));
                self.w(ind, format!("if {st} == 1 {{"));
                self.w(ind + 1, format!("{acc}: {} = []", type_src(t)));
                self.w(ind + 1, "while _d.more! {");
                self.dec(inner, &v, &z, ind + 2, false)?;
                self.w(ind + 2, format!("{acc} << {v}"));
                self.w(ind + 1, "}");
                self.w(ind + 1, format!("{dest} = {acc}"));
                self.w(ind, format!("}} else if {st} == 0 {{"));
                self.w(ind + 1, format!("{dest} = []"));
                self.w(ind, "}");
            }
            Ty::Fixed(inner, n) => {
                let (v, st, acc, i) = (self.fresh("v"), self.fresh("st"), self.fresh("a"), self.fresh("i"));
                let z = self.zero(inner).ok_or_else(|| format!("{} has no zero value to decode into", type_src(inner)))?;
                self.w(ind, format!("{dest}: {} = {old}", type_src(t)));
                self.w(ind, format!("{st} = _d.open!(91, {})", lit(&type_src(t))));
                self.w(ind, format!("if {st} == 1 {{"));
                let ze = self.fresh("z");
                self.w(ind + 1, format!("{ze}: {} = {z}", type_src(inner)));
                self.w(ind + 1, format!("{acc}: {} = [{ze}; {n}]", type_src(t)));
                self.w(ind + 1, format!("{i} = 0"));
                self.w(ind + 1, "while _d.more! {");
                self.w(ind + 2, format!("if {i} < {n} {{"));
                self.dec(inner, &v, &z, ind + 3, false)?;
                self.w(ind + 3, format!("{acc}[{i}] = {v}"));
                self.w(ind + 2, "} else {");
                self.w(ind + 3, "_d.skip!");
                self.w(ind + 2, "}");
                self.w(ind + 2, format!("{i} += 1"));
                self.w(ind + 1, "}");
                self.w(ind + 1, format!("{dest} = {acc}"));
                self.w(ind, "}");
            }
            Ty::Map(kt, inner) => {
                let (k, v, st, m) = (self.fresh("k"), self.fresh("v"), self.fresh("st"), self.fresh("m"));
                let z = self.zero(inner).ok_or_else(|| format!("{} has no zero value to decode into", type_src(inner)))?;
                self.w(ind, format!("{dest}: {} = {old}", type_src(t)));
                self.w(ind, format!("{st} = _d.open!(123, {})", lit(&type_src(t))));
                self.w(ind, format!("if {st} == 1 {{"));
                self.w(ind + 1, format!("{m}: {} = {old}", type_src(t)));
                self.w(ind + 1, "while _d.next_key! {");
                let ind3 = match self.key_ty(kt)? {
                    KeyTy::Str => {
                        self.w(ind + 2, format!("{k} = _d.key"));
                        ind + 2
                    }
                    KeyTy::Int(ik) => {
                        let k0 = self.fresh("k");
                        self.w(ind + 2, format!("{k0} = _d.int_key!({}, {}, {})", ik.bits(), ik.signed(), lit(&type_src(kt))));
                        self.w(ind + 2, format!("if {k} = {k0}?.as_{} {{", ik.method()));
                        ind + 3
                    }
                    KeyTy::Named => {
                        let kz = self.zero(kt).ok_or_else(|| format!("{} has no zero value to decode into", type_src(kt)))?;
                        self.w(ind + 2, format!("if {k} = {a}.dec_key(_d, {kz}) {{"));
                        ind + 3
                    }
                };
                self.dec(inner, &v, &z, ind3, false)?;
                self.w(ind3, format!("{m}[{k}] = {v}"));
                if ind3 > ind + 2 {
                    self.w(ind + 2, "}");
                }
                self.w(ind + 1, "}");
                self.w(ind + 1, format!("{dest} = {m}"));
                self.w(ind, format!("}} else if {st} == 0 {{"));
                self.w(ind + 1, format!("{dest} = {{}}"));
                self.w(ind, "}");
            }
            Ty::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                self.w(ind, "_d.open_tuple!");
                for (k, (t, n)) in ts.iter().zip(&names).enumerate() {
                    self.w(ind, format!("_d.tuple_next!({k}, {})", ts.len()));
                    let z = self.zero(t).ok_or_else(|| format!("{} has no zero value to decode into", type_src(t)))?;
                    self.dec(t, n, &z, ind, false)?;
                }
                self.w(ind, format!("_d.tuple_end!({})", ts.len()));
                self.w(ind, format!("{dest} = ({})", names.join(", ")));
            }
        }
        Ok(())
    }

    /// A spelled zero value for a type.
    fn zero(&self, t: &TypeExpr) -> Option<String> {
        Some(match t {
            TypeExpr::Named(n, _) if self.shadowed(n) => format!("{n}.json_zero"),
            TypeExpr::Named(n, _) if IntKind::from_name(n).is_some() => match IntKind::from_name(n).unwrap() {
                IntKind::I64 => "0".into(),
                k => format!("0.as_{}", k.method()),
            },
            TypeExpr::Named(n, _) if n == "Float" || n == "F64" => "0.0".into(),
            TypeExpr::Named(n, _) if n == "Bool" => "false".into(),
            TypeExpr::Named(n, _) if n == "Str" => "\"\"".into(),
            TypeExpr::Named(n, _) if n == "Complex" => "Complex.new(0.0, 0.0)".into(),
            // a struct of this file that doesn't derive Json (a hand-written Marshaler)
            TypeExpr::Named(n, _) if self.types.local.get(n.as_str()) == Some(&false) && !self.types.enums.contains(n.as_str()) => format!("{n}.new"),
            // a type of this file that derives Json
            TypeExpr::Named(n, _) if self.types.local.get(n.as_str()) == Some(&true) => format!("{n}.json_zero"),
            // another package's type: its json_zero when it was derived, else `T.new`
            TypeExpr::Named(n, _) if !self.tparams.contains(n) => format!("{}.zero_value()", self.alias),
            TypeExpr::Array(..) => "[]".into(),
            TypeExpr::Fixed(e, n, _) => match &n.kind {
                ExprKind::Int(v) => format!("[{}; {v}]", self.zero(e)?),
                _ => return None,
            },
            TypeExpr::App(n, ..) if n == "Map" => "{}".into(),
            TypeExpr::Opt(..) => "none".into(),
            TypeExpr::Tuple(ts, _) => format!("({})", ts.iter().map(|t| self.zero(t)).collect::<Option<Vec<_>>>()?.join(", ")),
            _ => return None,
        })
    }
}

/// The one field of a `#[json(transparent)]` struct.
pub fn transparent_field(fields: &[DField]) -> Result<&DField, String> {
    let live: Vec<&DField> = fields.iter().filter(|f| !f.opts.skip).collect();
    match live.as_slice() {
        [f] => Ok(f),
        _ => Err("#[json(transparent)] needs exactly one (non-skipped) field".to_string()),
    }
}

fn check_keys<'a>(fields: impl Iterator<Item = &'a DField>, what: &str) -> Result<(), String> {
    let mut seen: HashSet<String> = HashSet::new();
    for f in fields {
        if f.opts.skip {
            continue;
        }
        let k = f.opts.rename.clone().unwrap_or_else(|| f.name.clone());
        if !seen.insert(k.clone()) {
            return Err(format!("{what}: two fields are named \"{k}\" in JSON"));
        }
    }
    Ok(())
}

/// The source text of `struct Name { defs }` holding the derived methods.
/// `jobs`: every derive of the file (embedded structs are looked up there).
pub fn json_source(job: &DeriveJob, alias: &str, types: &ModuleTypes, lenient: bool, v2_only: &[String], jobs: &[DeriveJob]) -> Result<String, Diag> {
    let fail = |m: String| Diag::new(job.span, format!("derive(Json) on `{}`: {m}", job.name));
    let mut g = Gen { alias, lenient, v2_only: v2_only.to_vec(), tparams: &job.tparams, types, owner: &job.name, out: String::new(), n: 0 };
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Json yet; derive it on a concrete wrapper, or write json_enc / json_dec by hand".to_string()));
    }
    let (pubk, name, a) = ("pub ", job.name.clone(), alias.to_string());
    let resolved = match &job.shape {
        DShape::Struct(_) if !job.transparent => resolve_fields(job, jobs, lenient).map_err(&fail)?,
        _ => vec![],
    };
    g.w(0, format!("struct {name} {{"));
    // ---- encoding
    g.w(1, format!("{pubk}def json_str(_e: {a}.Encoder) -> Str {{"));
    match &job.shape {
        DShape::Struct(fields) if job.transparent => {
            let f = transparent_field(fields).map_err(&fail)?;
            let fr = g.enc(&f.ty, &format!("self.{}", f.name), 2, false).map_err(&fail)?;
            g.w(2, frags_expr(&fr));
        }
        DShape::Struct(_) => {
            let mut fs = vec![];
            let mut guards = vec![];
            for rf in &resolved {
                let mut cur = "self".to_string();
                let mut gs = vec![];
                for (sf, _) in &rf.chain[..rf.chain.len() - 1] {
                    if matches!(sf.ty, TypeExpr::Opt(..)) {
                        let v = g.fresh("p");
                        gs.push((v.clone(), format!("{cur}.{}", sf.name)));
                        cur = v;
                    } else {
                        cur = format!("{cur}.{}", sf.name);
                    }
                }
                fs.push((rf.leaf(), rf.name.clone(), format!("{cur}.{}", rf.leaf().name)));
                guards.push(gs);
            }
            let fr = g.enc_object(&fs, &guards, 2).map_err(&fail)?;
            g.w(2, frags_expr(&fr));
        }
        DShape::Enum(vs) => {
            check_keys(vs.iter().map(|v| DField { name: v.name.clone(), ty: TypeExpr::Named("Str".into(), job.span), opts: v.opts.clone(), span: job.span }).collect::<Vec<_>>().iter(), "variants").map_err(&fail)?;
            if vs.is_empty() {
                g.w(2, "\"null\"");
            } else {
                g.w(2, "_r = \"null\"");
                g.w(2, "case self {");
                for v in vs {
                    let vname = v.opts.rename.clone().unwrap_or_else(|| v.name.clone());
                    let key = json_quote(&vname);
                    if v.fields.is_empty() {
                        g.w(3, format!("{} => {{", v.name));
                        g.w(4, format!("_r = {}", lit(&key)));
                        g.w(4, "nil");
                        g.w(3, "}");
                        continue;
                    }
                    let vals: Vec<String> = (0..v.fields.len()).map(|k| format!("_f{k}")).collect();
                    g.w(3, format!("{}({}) => {{", v.name, vals.join(", ")));
                    let mut fr = vec![Frag::Lit(format!("{{{key}:"))];
                    let named = v.fields.iter().any(|f| f.name.parse::<usize>().is_err());
                    if named {
                        if v.fields.iter().any(|f| f.name.parse::<usize>().is_ok()) {
                            return Err(fail(format!("variant `{}` mixes named and unnamed fields", v.name)));
                        }
                        check_keys(v.fields.iter(), &format!("variant {}", v.name)).map_err(&fail)?;
                        let fs: Vec<(&DField, String, String)> = v.fields.iter().zip(&vals).filter(|(f, _)| !f.opts.skip).map(|(f, x)| (f, f.opts.rename.clone().unwrap_or_else(|| f.name.clone()), x.clone())).collect();
                        let guards = vec![vec![]; fs.len()];
                        fr.extend(g.enc_object(&fs, &guards, 4).map_err(&fail)?);
                    } else if v.fields.len() == 1 {
                        fr.extend(g.enc(&v.fields[0].ty, &vals[0], 4, false).map_err(&fail)?);
                    } else {
                        fr.push(Frag::Lit("[".into()));
                        for (k, f) in v.fields.iter().enumerate() {
                            if k > 0 {
                                fr.push(Frag::Lit(",".into()));
                            }
                            fr.extend(g.enc(&f.ty, &vals[k], 4, false).map_err(&fail)?);
                        }
                        fr.push(Frag::Lit("]".into()));
                    }
                    fr.push(Frag::Lit("}".into()));
                    g.w(4, format!("_r = {}", frags_expr(&fr)));
                    g.w(4, "nil");
                    g.w(3, "}");
                }
                g.w(2, "}");
                g.w(2, "_r");
            }
        }
    }
    g.w(1, "}");
    g.w(1, format!("{pubk}def json_enc(_e: {a}.Encoder) {{"));
    g.w(2, "_e.raw!(self.json_str(_e))");
    g.w(1, "}");
    // ---- the zero value (what json_dec decodes into; also after an error)
    g.w(1, format!("{pubk}def self.json_zero -> {name} {{"));
    match &job.shape {
        DShape::Struct(_) => g.w(2, format!("{name}.new")),
        DShape::Enum(vs) => {
            if let Some(v) = vs.first() {
                let zs: Option<Vec<String>> = v.fields.iter().map(|f| g.zero(&f.ty)).collect();
                let Some(zs) = zs else { return Err(fail(format!("variant `{}` has a field without a zero value", v.name))) };
                if zs.is_empty() {
                    g.w(2, format!("{name}.{}", v.name));
                } else {
                    g.w(2, format!("{name}.{}({})", v.name, zs.join(", ")));
                }
            }
        }
    }
    g.w(1, "}");
    g.w(1, format!("{pubk}def json_derived -> Bool {{ true }}"));
    // ---- omitzero
    if let DShape::Struct(fields) = &job.shape {
        let zs: Vec<String> = fields.iter().filter(|f| !f.opts.skip).map(|f| g.is_zero(&f.ty, &format!("self.{}", f.name))).collect();
        g.w(1, format!("{pubk}def json_is_zero -> Bool {{ {} }}", if zs.is_empty() { "true".to_string() } else { zs.join(" && ") }));
    }
    // ---- decoding
    // Each field (and each enum variant) reads in a helper def of its own, so the
    // frame of `json_merge` stays small however many fields there are.
    let mut helpers = String::new();
    match &job.shape {
        DShape::Struct(fields) => {
            g.w(1, format!("{pubk}def json_mergeable -> Bool {{ true }}"));
            g.w(1, format!("{pubk}def self.json_dec(_d: {a}.Decoder) -> {name} {{"));
            g.w(2, format!("{name}.new.json_merge(_d)"));
            g.w(1, "}");
            // helpers: one per own field
            for f in fields.iter().filter(|f| !f.opts.skip) {
                if embedded(f, jobs).is_some() && !job.transparent {
                    continue;
                }
                let saved = std::mem::take(&mut g.out);
                g.w(1, format!("def self.json_f_{}(_d: {a}.Decoder, _o: {}) -> {} {{", f.name, type_src(&f.ty), type_src(&f.ty)));
                g.dec(&f.ty, "_v", "_o", 2, f.opts.string_tag).map_err(&fail)?;
                g.w(2, "_v");
                g.w(1, "}");
                helpers.push_str(&std::mem::replace(&mut g.out, saved));
            }
            g.w(1, format!("{pubk}def json_merge(_d: {a}.Decoder) -> {name} {{"));
            g.w(2, "_r = self");
            if job.transparent {
                let f = transparent_field(fields).map_err(&fail)?;
                g.w(2, "_d.root!(\"\")");
                g.w(2, format!("_r.{} = {name}.json_f_{}(_d, _r.{})", f.name, f.name, f.name));
            } else {
                // `#[data("pkg.T")]` names the type in errors (Go's name; "" for
                // a Go anonymous struct)
                let shown = job.go_name.clone().unwrap_or_else(|| name.clone());
                g.w(2, format!("_d.root!({})", lit(&shown)));
                g.w(2, format!("if _d.open!(123, {}) == 1 {{", lit(&shown)));
                g.w(3, "while _d.next_key! {");
                let names: Vec<String> = resolved.iter().map(|rf| lit(&rf.name)).collect();
                g.w(4, "_i = case _d.key {");
                for (i, nm) in names.iter().enumerate() {
                    g.w(5, format!("{nm} => {i}"));
                }
                g.w(5, format!("_ => {a}.fold_index(_d.key, [{}])", names.join(", ")));
                g.w(4, "}");
                g.w(4, "case _i {");
                for (i, rf) in resolved.iter().enumerate() {
                    g.w(5, format!("{i} => {{"));
                    let leaf = rf.leaf();
                    let owner = &rf.chain.last().unwrap().1;
                    // the chain of embedded structs: copies, the field, then write back
                    let mut cur = "_r".to_string();
                    let mut back: Vec<(String, String, String)> = vec![];
                    for (sf, _) in &rf.chain[..rf.chain.len() - 1] {
                        let t = g.fresh("em");
                        let inner = match &sf.ty {
                            TypeExpr::Opt(t2, _) => Some(type_src(t2)),
                            _ => None,
                        };
                        match inner {
                            Some(tn) => g.w(6, format!("{t} = {cur}.{} || {tn}.new", sf.name)),
                            None => g.w(6, format!("{t} = {cur}.{}", sf.name)),
                        }
                        back.push((cur.clone(), sf.name.clone(), t.clone()));
                        cur = t;
                    }
                    g.w(6, format!("{cur}.{} = {owner}.json_f_{}(_d, {cur}.{})", leaf.name, leaf.name, leaf.name));
                    for (parent, fname, t) in back.iter().rev() {
                        g.w(6, format!("{parent}.{fname} = {t}"));
                    }
                    g.w(6, "nil");
                    g.w(5, "}");
                }
                g.w(5, "_ => _d.unknown!");
                g.w(4, "}");
                g.w(3, "}");
                g.w(2, "}");
            }
            g.w(2, "_r");
            g.w(1, "}");
        }
        DShape::Enum(vs) => {
            if vs.is_empty() {
                return Err(fail("an enum without variants has no JSON form".to_string()));
            }
            g.w(1, format!("{pubk}def self.json_dec(_d: {a}.Decoder) -> {name} {{"));
            g.w(2, format!("_d.root!({})", lit(&name)));
            g.w(2, "_vn = _d.variant!");
            g.w(2, "_wr = _d.wrapped");
            g.w(2, format!("_r = {name}.json_zero"));
            g.w(2, "case _vn {");
            for v in vs {
                let vname = v.opts.rename.clone().unwrap_or_else(|| v.name.clone());
                g.w(3, format!("{} => {{", lit(&vname)));
                let ctx = format!("{name}.{}", v.name);
                if v.fields.is_empty() {
                    g.w(4, "_d.unit_variant!(_wr)");
                    g.w(4, format!("_r = {name}.{}", v.name));
                    g.w(4, "nil");
                    g.w(3, "}");
                    continue;
                }
                let hn = format!("json_v_{}", v.name);
                g.w(4, format!("_r = {name}.{hn}(_d, _wr)"));
                g.w(4, "nil");
                g.w(3, "}");
                let saved = std::mem::take(&mut g.out);
                g.w(1, format!("def self.{hn}(_d: {a}.Decoder, _wr: Bool) -> {name} {{"));
                g.w(2, format!("_d.at!({})", lit(&ctx)));
                g.w(2, "_d.need_payload!(_wr)");
                let named = v.fields.iter().any(|f| f.name.parse::<usize>().is_err());
                let mut args: Vec<String> = vec![];
                if named {
                    // Named fields: an object. Each field gets a temporary holding its zero.
                    let mut temps: Vec<String> = vec![];
                    for f in v.fields.iter() {
                        let t = g.fresh("p");
                        match g.zero(&f.ty) {
                            Some(z) => g.w(2, format!("{t}: {} = {z}", type_src(&f.ty))),
                            None => return Err(fail(format!("field `{}` of variant `{}` has no zero value", f.name, v.name))),
                        }
                        temps.push(t);
                    }
                    g.w(2, "if _d.open_obj! {");
                    g.w(3, "while _d.next_key! {");
                    g.w(4, "case _d.key {");
                    for (f, t) in v.fields.iter().zip(&temps) {
                        if f.opts.skip {
                            continue;
                        }
                        let key = f.opts.rename.clone().unwrap_or_else(|| f.name.clone());
                        g.w(5, format!("{} => {{", lit(&key)));
                        let x = g.fresh("v");
                        g.dec(&f.ty, &x, t, 6, f.opts.string_tag).map_err(&fail)?;
                        g.w(6, format!("{t} = {x}"));
                        g.w(6, "nil");
                        g.w(5, "}");
                    }
                    g.w(5, "_ => _d.unknown!");
                    g.w(4, "}");
                    g.w(3, "}");
                    g.w(2, "}");
                    args = temps;
                } else if v.fields.len() == 1 {
                    let x = g.fresh("v");
                    let z = g.zero(&v.fields[0].ty).ok_or_else(|| fail(format!("variant `{}` has a field without a zero value", v.name)))?;
                    g.dec(&v.fields[0].ty, &x, &z, 2, false).map_err(&fail)?;
                    args.push(x);
                } else {
                    g.w(2, "_d.open_tuple!");
                    for (k, f) in v.fields.iter().enumerate() {
                        g.w(2, format!("_d.tuple_next!({k}, {})", v.fields.len()));
                        let x = g.fresh("v");
                        let z = g.zero(&f.ty).ok_or_else(|| fail(format!("variant `{}` has a field without a zero value", v.name)))?;
                        g.dec(&f.ty, &x, &z, 2, false).map_err(&fail)?;
                        args.push(x);
                    }
                    g.w(2, format!("_d.tuple_end!({})", v.fields.len()));
                }
                g.w(2, format!("{name}.{}({})", v.name, args.join(", ")));
                g.w(1, "}");
                helpers.push_str(&std::mem::replace(&mut g.out, saved));
            }
            g.w(3, "_ => {");
            g.w(4, format!("_d.bad_variant!({}, _vn)", lit(&name)));
            g.w(3, "}");
            g.w(2, "}");
            g.w(2, "_d.end_variant!(_wr)");
            g.w(2, "_r");
            g.w(1, "}");
        }
    }
    g.out.push_str(&helpers);
    // ---- the conveniences
    g.w(1, format!("{pubk}def to_json -> ~Str<{a}.JsonError> {{"));
    g.w(2, format!("{a}.~marshal(self)"));
    g.w(1, "}");
    g.w(1, format!("{pubk}def to_json_indent(_prefix: Str, _indent: Str) -> ~Str<{a}.JsonError> {{"));
    g.w(2, format!("{a}.~marshal_indent(self, _prefix, _indent)"));
    g.w(1, "}");
    g.w(1, format!("{pubk}def self.from_json(_s: Str) -> ~{name}<{a}.JsonError> {{"));
    g.w(2, format!("{a}.~unmarshal_into(_s, {name}.json_zero)"));
    g.w(1, "}");
    g.w(0, "}");
    let _ = (g.alias, g.owner);
    Ok(g.out)
}

/// `#[derive(Row)]`: `def self.from_sql_row(r: S.RawRow) -> ~T`, where `S` is
/// the local name of the `database/sql` import. Columns map to fields by
/// name: `#[field(db: "name")]`, else the field's name; case-insensitively,
/// as sqlx does. A column no field takes is an error (sqlx's "missing
/// destination name"); `#[field(db: "-")]` leaves a field out; fields no
/// column names stay zero. Each value converts with `S.convert_column`, the
/// conversion `rows.scan` uses (Go's convertAssign).
pub fn row_source(job: &DeriveJob, alias: &str) -> Result<String, Diag> {
    let fail = |m: String| Diag::new(job.span, format!("derive(Row) on `{}`: {m}", job.name));
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Row yet".to_string()));
    }
    let DShape::Struct(fields) = &job.shape else {
        return Err(fail("only a struct can derive Row (its fields are the columns)".to_string()));
    };
    let name = &job.name;
    let mut o = String::new();
    o.push_str(&format!("struct {name} {{\n  pub def self.from_sql_row(_row: {alias}.RawRow) -> ~{name} {{\n"));
    o.push_str(&format!("    _r = {name}.new\n    _i = 0\n    while _i < _row.columns.size {{\n      _c = {alias}.lower_ascii(_row.columns[_i])\n"));
    let mut seen: Vec<String> = vec![];
    let mut first = true;
    for (k, f) in fields.iter().enumerate() {
        let key = f.opts.tag("db").unwrap_or(&f.name).to_string();
        if key == "-" {
            continue;
        }
        let low = key.to_ascii_lowercase();
        if seen.contains(&low) {
            return Err(fail(format!("two fields take the column `{key}`")));
        }
        seen.push(low.clone());
        let kw = if first { "if" } else { "} else if" };
        first = false;
        o.push_str(&format!("      {kw} _c == {} {{\n        _v{k}: {} = ~{alias}.convert_column(_row, _i)\n        _r.{} = _v{k}\n", lit(&low), type_src(&f.ty), f.name));
    }
    let missing = format!("fail {alias}.SqlError.MissingDestination(name: _row.columns[_i], type_name: {})", lit(name));
    if first {
        o.push_str(&format!("      {missing}\n"));
    } else {
        o.push_str(&format!("      }} else {{\n        {missing}\n      }}\n"));
    }
    o.push_str("      _i += 1\n    }\n    _r\n  }\n}\n");
    Ok(o)
}

// ---------------------------------------------------------------- Arbitrary

/// `#[derive(Arbitrary)]`: `def self.arbitrary(r: R.Rand, size: Int) -> T`,
/// the random values testing/quick checks with (Go's quick.sizedValue for a
/// struct: each field gets size / fields; an enum picks a variant
/// uniformly). `q` and `r` are the local names of testing/quick and
/// math/rand.
pub fn arbitrary_source(job: &DeriveJob, q: &str, r: &str) -> Result<String, Diag> {
    let fail = |m: String| Diag::new(job.span, format!("derive(Arbitrary) on `{}`: {m}", job.name));
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Arbitrary yet; write `def self.arbitrary(r, size)` by hand".to_string()));
    }
    let name = &job.name;
    let args = |fields: &[DField], size: &str| -> Result<String, Diag> {
        let named = fields.iter().any(|f| f.name.parse::<usize>().is_err());
        let mut parts = vec![];
        for f in fields {
            let g = arb_gen(&f.ty, q, r).map_err(&fail)?;
            let v = format!("{g}.generate(_r, {size})");
            parts.push(if named { format!("{}: {v}", f.name) } else { v });
        }
        Ok(parts.join(", "))
    };
    let mut o = format!("struct {name} {{\n  pub def self.arbitrary(_r: {r}.Rand, _size: Int) -> {name} {{\n");
    match &job.shape {
        DShape::Struct(fields) => {
            o.push_str(&format!("    _s = {q}.field_size(_size, {})\n", fields.len()));
            o.push_str(&format!("    {name}.new({})\n", args(fields, "_s")?));
        }
        DShape::Enum(vs) => {
            if vs.is_empty() {
                return Err(fail("an enum without variants has no values".to_string()));
            }
            o.push_str("    _rr = _r\n");
            o.push_str(&format!("    _k = _rr.intn!({})\n", vs.len()));
            o.push_str("    case _k {\n");
            for (k, v) in vs.iter().enumerate() {
                let arm = if k + 1 == vs.len() { "_".to_string() } else { k.to_string() };
                if v.fields.is_empty() {
                    o.push_str(&format!("      {arm} => {name}.{}\n", v.name));
                } else {
                    o.push_str(&format!("      {arm} => {{\n        _s = {q}.field_size(_size, {})\n        {name}.{}({})\n      }}\n", v.fields.len(), v.name, args(&v.fields, "_s")?));
                }
            }
            o.push_str("    }\n");
        }
    }
    o.push_str("  }\n}\n");
    Ok(o)
}

/// The testing/quick generator expression for a field of type `t`.
fn arb_gen(t: &TypeExpr, q: &str, r: &str) -> Result<String, String> {
    Ok(match t {
        TypeExpr::Named(n, _) => match n.as_str() {
            "Int" => format!("{q}.int"),
            "I64" => format!("{q}.i64"),
            "I32" => format!("{q}.i32"),
            "I16" => format!("{q}.i16"),
            "I8" => format!("{q}.i8"),
            "U64" => format!("{q}.u64"),
            "U32" => format!("{q}.u32"),
            "U16" => format!("{q}.u16"),
            "U8" | "Byte" => format!("{q}.u8"),
            "Rune" => format!("{q}.rune"),
            "Float" | "F64" => format!("{q}.float"),
            "Complex" => format!("{q}.complex"),
            "Bool" => format!("{q}.bool"),
            "Str" => format!("{q}.str"),
            _ if n.chars().next().is_some_and(|c| c.is_uppercase()) || n.contains('.') => format!("{q}.Arb[{n}].gen"),
            _ => return Err(format!("no generator for a field of type `{n}`")),
        },
        TypeExpr::Array(e, _) => format!("{q}.slice_of({})", arb_gen(e, q, r)?),
        TypeExpr::Opt(e, _) => format!("{q}.opt_of({})", arb_gen(e, q, r)?),
        TypeExpr::App(n, args, _) if n == "Map" && args.len() == 2 => format!("{q}.map_of({}, {})", arb_gen(&args[0], q, r)?, arb_gen(&args[1], q, r)?),
        TypeExpr::Tuple(ts, _) if ts.len() == 2 => format!("{q}.tuple2({}, {})", arb_gen(&ts[0], q, r)?, arb_gen(&ts[1], q, r)?),
        TypeExpr::Tuple(ts, _) if ts.len() == 3 => format!("{q}.tuple3({}, {}, {})", arb_gen(&ts[0], q, r)?, arb_gen(&ts[1], q, r)?, arb_gen(&ts[2], q, r)?),
        _ => return Err(format!("no generator for a field of type `{}`; write `def self.arbitrary(r, size)` by hand", type_src(t))),
    })
}

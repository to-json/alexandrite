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
    /// derive(Data): `data: "Name"` / `data: "-"`.
    pub drename: Option<String>,
    pub dskip: bool,
    /// Every `key: "value"` of `#[field(...)]`, in order.
    pub tags: Vec<(String, String)>,
}

/// Whether `text` is an attribute that sets field options: `json(...)`,
/// `data(...)` or `field(...)`.
pub fn is_field_attr(text: &str) -> bool {
    let t = text.trim_start();
    ["json", "data", "field"].iter().any(|k| t.strip_prefix(k).is_some_and(|r| r.trim_start().starts_with('(')))
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
            _ => {}
        }
        o.tags.push((k.to_string(), v));
    }
    Ok(())
}

/// Go's json tag: `name`, `-`, `name,omitempty`, `,omitempty`.
fn apply_json_tag(v: &str, sp: Span, o: &mut Opts) -> Result<(), Diag> {
    if v == "-" {
        o.skip = true;
        return Ok(());
    }
    let mut parts = v.split(',');
    let name = parts.next().unwrap_or("");
    if !name.is_empty() {
        o.rename = Some(name.to_string());
    }
    for p in parts {
        match p.trim() {
            "omitempty" | "omit_empty" => o.omit_empty = true,
            "" => {}
            p => return Err(Diag::new(sp, format!("unknown json tag option `{p}`")).note("options: omitempty")),
        }
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

/// The names in `#[derive(A, B)]`.
pub fn derive_names(text: &str, sp: Span) -> Result<Option<Vec<String>>, Diag> {
    let Some(rest) = text.trim().strip_prefix("derive") else { return Ok(None) };
    let inner = rest.trim_start().strip_prefix('(').and_then(|r| r.trim_end().strip_suffix(')')).ok_or_else(|| Diag::new(sp, "write `#[derive(Json)]`"))?;
    let names: Vec<String> = inner.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    for n in &names {
        // Eq and Show are structural already (D13, D14): accepted, nothing to generate.
        if !matches!(n.as_str(), "Json" | "Data" | "Xml" | "Arbitrary" | "Eq" | "Show") {
            return Err(Diag::new(sp, format!("can't derive `{n}`")).note("derivable: Json (generates to_json / from_json), Data (a dynamic dyn.Value tree: to_data / from_data), Xml (encoding/xml: to_xml / from_xml and the Marshal protocol), Arbitrary (generates `self.arbitrary` for testing/quick); Eq and Show are accepted and need nothing: structs, tuples and enums already compare and print field by field"));
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
    /// `Json` or `Arbitrary`.
    pub derive: String,
    pub name: String,
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
    Map(&'a TypeExpr),
    Tuple(&'a [TypeExpr]),
    Named(&'a str),
}

struct Gen<'a> {
    alias: &'a str,
    tparams: &'a [String],
    types: &'a ModuleTypes,
    owner: &'a str,
    out: String,
    n: usize,
}

type GResult<T> = Result<T, String>;

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
                } else if let Some(false) = self.types.local.get(n.as_str()) {
                    return Err(format!("`{n}` doesn't derive Json: add `#[derive(Json)]` to it"));
                } else if matches!(n.as_str(), "Float32" | "F32" | "Ptr" | "Unit") {
                    return Err(format!("{n} has no JSON encoding"));
                } else {
                    Ty::Named(n)
                }
            }
            TypeExpr::Opt(e, _) => Ty::Opt(e),
            TypeExpr::Array(e, _) => Ty::Arr(e),
            TypeExpr::App(n, args, _) if n == "Map" && args.len() == 2 => {
                match &args[0] {
                    TypeExpr::Named(k, _) if k == "Str" => {}
                    k => return Err(format!("map keys must be Str to be JSON object keys, not {}", type_src(k))),
                }
                Ty::Map(&args[1])
            }
            TypeExpr::App(n, ..) => return Err(format!("`{n}[...]`: generic types can't be derived yet")),
            TypeExpr::Tuple(ts, _) => Ty::Tuple(ts),
            TypeExpr::Fixed(..) => return Err("fixed-size arrays `[T; N]` aren't supported by derive(Json); use a slice `[T]`".to_string()),
            TypeExpr::Result(..) => return Err("a fallible type has no JSON encoding".to_string()),
            TypeExpr::Handle(..) => return Err("a pool handle `@T` has no JSON encoding".to_string()),
            TypeExpr::Fn(..) => return Err("a function type has no JSON encoding".to_string()),
        })
    }

    /// The pieces of the Str that encodes `x` (an expression without side effects),
    /// after any statements it needs (temporaries) are written at `ind`. A value
    /// becomes one interpolated string, so a record costs one allocation and one
    /// push into the encoder, however many fields it has.
    fn enc(&mut self, t: &'a TypeExpr, x: &str, ind: usize) -> GResult<Vec<Frag>> {
        Ok(match self.classify(t)? {
            Ty::Int(_) | Ty::Bool => vec![Frag::Ex(x.to_string())],
            Ty::Float => vec![Frag::Ex(format!("_e.f!({x})"))],
            Ty::Str => vec![Frag::Ex(format!("_e.q({x})"))],
            Ty::Named(_) => vec![Frag::Ex(format!("{x}.json_str(_e)"))],
            Ty::Opt(inner) => {
                let (o, v) = (self.fresh("o"), self.fresh("v"));
                self.w(ind, format!("{o} = \"null\""));
                self.w(ind, format!("if {v} = {x} {{"));
                let fr = self.enc(inner, &v, ind + 1)?;
                self.w(ind + 1, format!("{o} = {}", frags_expr(&fr)));
                self.w(ind, "}");
                vec![Frag::Ex(o)]
            }
            Ty::Arr(inner) => {
                let (a, v) = (self.fresh("a"), self.fresh("x"));
                self.w(ind, format!("{a}: [Str] = []"));
                self.w(ind, format!("for {v} in {x} {{"));
                let fr = self.enc(inner, &v, ind + 1)?;
                self.w(ind + 1, format!("{a} << {}", frags_expr(&fr)));
                self.w(ind, "}");
                vec![Frag::Lit("[".into()), Frag::Ex(format!("{a}.join(\",\")")), Frag::Lit("]".into())]
            }
            Ty::Map(inner) => {
                let (a, k, v) = (self.fresh("a"), self.fresh("k"), self.fresh("v"));
                self.w(ind, format!("{a}: [Str] = []"));
                self.w(ind, format!("for {k}, {v} in {x} {{"));
                let mut fr = vec![Frag::Ex(format!("_e.q({k})")), Frag::Lit(":".into())];
                fr.extend(self.enc(inner, &v, ind + 1)?);
                self.w(ind + 1, format!("{a} << {}", frags_expr(&fr)));
                self.w(ind, "}");
                vec![Frag::Lit("{".into()), Frag::Ex(format!("{a}.join(\",\")")), Frag::Lit("}".into())]
            }
            Ty::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                self.w(ind, format!("{} = {x}", names.join(", ")));
                let mut fr = vec![Frag::Lit("[".into())];
                for (k, (t, n)) in ts.iter().zip(&names).enumerate() {
                    if k > 0 {
                        fr.push(Frag::Lit(",".into()));
                    }
                    fr.extend(self.enc(t, n, ind)?);
                }
                fr.push(Frag::Lit("]".into()));
                fr
            }
        })
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
            TypeExpr::App(n, ..) if n == "Map" => Some(format!("{x}.size == 0")),
            TypeExpr::Opt(..) => Some(format!("{x}.none?")),
            _ => None,
        }
    }

    /// The pieces of `{"k":v,...}` for named fields held in `vals` (expressions).
    fn enc_object(&mut self, fields: &'a [DField], vals: &[String], ind: usize) -> GResult<Vec<Frag>> {
        let live: Vec<usize> = (0..fields.len()).filter(|&k| !fields[k].opts.skip).collect();
        let dynamic = live.iter().any(|&k| fields[k].opts.omit_empty && self.empty(&fields[k].ty, &vals[k]).is_some());
        if !dynamic {
            if live.is_empty() {
                return Ok(vec![Frag::Lit("{}".into())]);
            }
            let mut fr = vec![];
            for (pos, &k) in live.iter().enumerate() {
                let key = json_quote(fields[k].opts.rename.as_deref().unwrap_or(&fields[k].name));
                let open = if pos == 0 { "{" } else { "," };
                fr.push(Frag::Lit(format!("{open}{key}:")));
                fr.extend(self.enc(&fields[k].ty, &vals[k], ind)?);
            }
            fr.push(Frag::Lit("}".into()));
            return Ok(fr);
        }
        let ps = self.fresh("ps");
        self.w(ind, format!("{ps}: [Str] = []"));
        for &k in &live {
            let key = json_quote(fields[k].opts.rename.as_deref().unwrap_or(&fields[k].name));
            let cond = if fields[k].opts.omit_empty { self.empty(&fields[k].ty, &vals[k]) } else { None };
            let ind2 = if let Some(c) = &cond {
                self.w(ind, format!("if !({c}) {{"));
                ind + 1
            } else {
                ind
            };
            let mut fr = vec![Frag::Lit(format!("{key}:"))];
            fr.extend(self.enc(&fields[k].ty, &vals[k], ind2)?);
            self.w(ind2, format!("{ps} << {}", frags_expr(&fr)));
            if cond.is_some() {
                self.w(ind, "}");
            }
        }
        Ok(vec![Frag::Lit("{".into()), Frag::Ex(format!("{ps}.join(\",\")")), Frag::Lit("}".into())])
    }

    /// Statements that read a value of type `t` from `d` into a new local `dest`.
    fn dec(&mut self, t: &'a TypeExpr, dest: &str, ind: usize) -> GResult<()> {
        match self.classify(t)? {
            Ty::Int(k) => match k {
                IntKind::I64 => self.w(ind, format!("{dest} = _d.int!")),
                IntKind::U64 => self.w(ind, format!("{dest} = _d.uint!(64)")),
                k if k.signed() => self.w(ind, format!("{dest} = _d.sint!({}).as_{}", k.bits(), k.method())),
                k => self.w(ind, format!("{dest} = _d.uint!({}).as_{}", k.bits(), k.method())),
            },
            Ty::Float => self.w(ind, format!("{dest} = _d.float!")),
            Ty::Bool => self.w(ind, format!("{dest} = _d.bool!")),
            Ty::Str => self.w(ind, format!("{dest} = _d.str!")),
            Ty::Named(n) => self.w(ind, format!("{dest} = {n}.json_dec(_d)")),
            Ty::Opt(inner) => {
                let (isnull, v) = (self.fresh("z"), self.fresh("v"));
                self.w(ind, format!("{dest}: {} = none", type_src(t)));
                self.w(ind, format!("{isnull} = _d.take_null!"));
                self.w(ind, format!("if !{isnull} {{"));
                self.dec(inner, &v, ind + 1)?;
                self.w(ind + 1, format!("{dest} = {v}"));
                self.w(ind, "}");
            }
            Ty::Arr(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("{dest}: {} = []", type_src(t)));
                self.w(ind, "if _d.open_arr! {");
                self.w(ind + 1, "while _d.more! {");
                self.dec(inner, &v, ind + 2)?;
                self.w(ind + 2, format!("{dest} << {v}"));
                self.w(ind + 1, "}");
                self.w(ind, "}");
            }
            Ty::Map(inner) => {
                let (k, v) = (self.fresh("k"), self.fresh("v"));
                self.w(ind, format!("{dest}: {} = {{}}", type_src(t)));
                self.w(ind, "if _d.open_obj! {");
                self.w(ind + 1, "while _d.next_key! {");
                self.w(ind + 2, format!("{k} = _d.key"));
                self.dec(inner, &v, ind + 2)?;
                self.w(ind + 2, format!("{dest}[{k}] = {v}"));
                self.w(ind + 1, "}");
                self.w(ind, "}");
            }
            Ty::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                self.w(ind, "_d.open_tuple!");
                for (k, (t, n)) in ts.iter().zip(&names).enumerate() {
                    self.w(ind, format!("_d.tuple_next!({k}, {})", ts.len()));
                    self.dec(t, n, ind)?;
                }
                self.w(ind, format!("_d.tuple_end!({})", ts.len()));
                self.w(ind, format!("{dest} = ({})", names.join(", ")));
            }
        }
        Ok(())
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

/// The source text of `struct Name[T] { defs }` holding the derived methods.
pub fn json_source(job: &DeriveJob, alias: &str, types: &ModuleTypes) -> Result<String, Diag> {
    let fail = |m: String| Diag::new(job.span, format!("derive(Json) on `{}`: {m}", job.name));
    let mut g = Gen { alias, tparams: &job.tparams, types, owner: &job.name, out: String::new(), n: 0 };
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Json yet; derive it on a concrete wrapper, or write json_enc / json_dec by hand".to_string()));
    }
    let (pubk, name, a) = ("pub ", job.name.clone(), alias.to_string());
    let tp = String::new();
    let _ = &tp;
    g.w(0, format!("struct {name} {{"));
    // ---- encoding
    g.w(1, format!("{pubk}def json_str(_e: {a}.Encoder) -> Str {{"));
    match &job.shape {
        DShape::Struct(fields) => {
            check_keys(fields.iter(), "").map_err(&fail)?;
            let vals: Vec<String> = fields.iter().map(|f| format!("self.{}", f.name)).collect();
            let fr = g.enc_object(fields, &vals, 2).map_err(&fail)?;
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
                        fr.extend(g.enc_object(&v.fields, &vals, 4).map_err(&fail)?);
                    } else if v.fields.len() == 1 {
                        fr.extend(g.enc(&v.fields[0].ty, &vals[0], 4).map_err(&fail)?);
                    } else {
                        fr.push(Frag::Lit("[".into()));
                        for (k, f) in v.fields.iter().enumerate() {
                            if k > 0 {
                                fr.push(Frag::Lit(",".into()));
                            }
                            fr.extend(g.enc(&f.ty, &vals[k], 4).map_err(&fail)?);
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
    // ---- the value json_dec returns after an error
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
    // ---- decoding
    // Each field (and each enum variant) reads in a helper def of its own, so the
    // frame of `json_dec` stays small however many fields there are.
    let mut helpers = String::new();
    g.w(1, format!("{pubk}def self.json_dec(_d: {a}.Decoder) -> {name} {{"));
    match &job.shape {
        DShape::Struct(fields) => {
            g.w(2, format!("_r = {name}.new"));
            g.w(2, "if _d.open_obj! {");
            g.w(3, "while _d.next_key! {");
            g.w(4, "case _d.key {");
            for f in fields.iter().filter(|f| !f.opts.skip) {
                let key = f.opts.rename.clone().unwrap_or_else(|| f.name.clone());
                let hn = format!("json_f_{}", f.name);
                g.w(5, format!("{} => {{", lit(&key)));
                g.w(6, format!("_d.at!({})", lit(&format!("{name}.{}", f.name))));
                g.w(6, format!("_r.{} = {name}.{hn}(_d)", f.name));
                g.w(6, "nil");
                g.w(5, "}");
                let saved = std::mem::take(&mut g.out);
                g.w(1, format!("def self.{hn}(_d: {a}.Decoder) -> {} {{", type_src(&f.ty)));
                g.dec(&f.ty, "_v", 2).map_err(&fail)?;
                g.w(2, "_v");
                g.w(1, "}");
                helpers.push_str(&std::mem::replace(&mut g.out, saved));
            }
            g.w(5, "_ => _d.skip!");
            g.w(4, "}");
            g.w(3, "}");
            g.w(2, "}");
            g.w(2, "_r");
        }
        DShape::Enum(vs) => {
            if vs.is_empty() {
                return Err(fail("an enum without variants has no JSON form".to_string()));
            }
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
                        g.dec(&f.ty, &x, 6).map_err(&fail)?;
                        g.w(6, format!("{t} = {x}"));
                        g.w(6, "nil");
                        g.w(5, "}");
                    }
                    g.w(5, "_ => _d.skip!");
                    g.w(4, "}");
                    g.w(3, "}");
                    g.w(2, "}");
                    args = temps;
                } else if v.fields.len() == 1 {
                    let x = g.fresh("v");
                    g.dec(&v.fields[0].ty, &x, 2).map_err(&fail)?;
                    args.push(x);
                } else {
                    g.w(2, "_d.open_tuple!");
                    for (k, f) in v.fields.iter().enumerate() {
                        g.w(2, format!("_d.tuple_next!({k}, {})", v.fields.len()));
                        let x = g.fresh("v");
                        g.dec(&f.ty, &x, 2).map_err(&fail)?;
                        args.push(x);
                    }
                    g.w(2, format!("_d.tuple_end!({})", v.fields.len()));
                }
                // Staged (required) fields: unwrap, or fail when missing.
                let mut call_args = vec![];
                let mut open = 0;
                for a in &args {
                    if let Some(t) = a.strip_suffix('!') {
                        let u = g.fresh("u");
                        g.w(2 + open, format!("if {u} = {t} {{"));
                        open += 1;
                        call_args.push(u);
                    } else {
                        call_args.push(a.clone());
                    }
                }
                if open == 0 {
                    g.w(2, format!("{name}.{}({})", v.name, call_args.join(", ")));
                } else {
                    g.w(2 + open, format!("return {name}.{}({})", v.name, call_args.join(", ")));
                    for k in (0..open).rev() {
                        g.w(2 + k, "}");
                    }
                    g.w(2, format!("fail _d.missing_field({})", lit(&ctx)));
                }
                g.w(1, "}");
                helpers.push_str(&std::mem::replace(&mut g.out, saved));
            }
            g.w(3, "_ => {");
            g.w(4, format!("_d.bad_variant!({}, _vn)", lit(&name)));
            g.w(3, "}");
            g.w(2, "}");
            g.w(2, "_d.end_variant!(_wr)");
            g.w(2, "_r");
        }
    }
    g.w(1, "}");
    g.out.push_str(&helpers);
    // ---- the conveniences
    g.w(1, format!("{pubk}def to_json -> ~Str<{a}.JsonError> {{"));
    g.w(2, format!("_e = {a}.new_encoder()"));
    g.w(2, "self.json_enc(_e)");
    g.w(2, "_e.~finish!");
    g.w(1, "}");
    g.w(1, format!("{pubk}def to_json_indent(_prefix: Str, _indent: Str) -> ~Str<{a}.JsonError> {{"));
    g.w(2, "_s = self.~to_json");
    g.w(2, format!("{a}.~indent(_s, _prefix, _indent)"));
    g.w(1, "}");
    g.w(1, format!("{pubk}def self.from_json(_s: Str) -> ~{name}<{a}.JsonError> {{"));
    g.w(2, format!("{a}.~check(_s)"));
    g.w(2, format!("_d = {a}.new_decoder(_s)"));
    g.w(2, format!("_v = {name}.json_dec(_d)"));
    g.w(2, "_d.~done!");
    g.w(2, "_v");
    g.w(1, "}");
    g.w(0, "}");
    let _ = (g.alias, g.owner);
    Ok(g.out)
}

impl<'a> Gen<'a> {
    /// A spelled zero value for a primitive/container type.
    fn zero(&self, t: &TypeExpr) -> Option<String> {
        Some(match t {
            TypeExpr::Named(n, _) if self.shadowed(n) => format!("{n}.json_zero"),
            TypeExpr::Named(n, _) if IntKind::from_name(n).is_some() => "0".into(),
            TypeExpr::Named(n, _) if n == "Float" || n == "F64" => "0.0".into(),
            TypeExpr::Named(n, _) if n == "Bool" => "false".into(),
            TypeExpr::Named(n, _) if n == "Str" => "\"\"".into(),
            TypeExpr::Named(n, _) if !self.tparams.contains(n) => format!("{n}.json_zero"),
            TypeExpr::Array(..) => "[]".into(),
            TypeExpr::App(n, ..) if n == "Map" => "{}".into(),
            TypeExpr::Opt(..) => "none".into(),
            TypeExpr::Tuple(ts, _) => format!("({})", ts.iter().map(|t| self.zero(t)).collect::<Option<Vec<_>>>()?.join(", ")),
            _ => return None,
        })
    }
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

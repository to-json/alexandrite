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
//!   def self.from_json(s: Str) -> ~T<J.JsonError>   one whole document
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

/// Options from `#[json(...)]` on a field or variant.
#[derive(Default, Clone, Debug)]
pub struct Opts {
    pub rename: Option<String>,
    pub skip: bool,
    pub omit_empty: bool,
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
        if !matches!(n.as_str(), "Json" | "Eq" | "Show") {
            return Err(Diag::new(sp, format!("can't derive `{n}`")).note("derivable: Json (generates to_json / from_json); Eq and Show are accepted and need nothing: structs, tuples and enums already compare and print field by field"));
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
        TypeExpr::Fixed(e, _, _) => format!("[{}; _]", type_src(e)),
        TypeExpr::App(n, args, _) => format!("{n}[{}]", args.iter().map(type_src).collect::<Vec<_>>().join(", ")),
        TypeExpr::Result(e, _, _) => format!("~{}", type_src(e)),
        TypeExpr::Handle(n, _) => format!("@{n}"),
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
            TypeExpr::Named(n, _) => {
                if let Some(k) = IntKind::from_name(n) {
                    Ty::Int(k)
                } else if n == "Float" {
                    Ty::Float
                } else if n == "Bool" {
                    Ty::Bool
                } else if n == "Str" {
                    Ty::Str
                } else if self.tparams.contains(n) {
                    return Err(format!("the type parameter `{n}` can't be encoded: derive(Json) on a generic type isn't supported yet (use a concrete type)"));
                } else if let Some(false) = self.types.local.get(n.as_str()) {
                    return Err(format!("`{n}` doesn't derive Json: add `#[derive(Json)]` to it"));
                } else if matches!(n.as_str(), "Float32" | "F32" | "F64" | "Ptr" | "Unit") {
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

    /// Statements that write `x` (an expression without side effects) to `e`.
    fn enc(&mut self, t: &'a TypeExpr, x: &str, ind: usize) -> GResult<()> {
        match self.classify(t)? {
            Ty::Int(k) => match k {
                IntKind::I64 => self.w(ind, format!("e.int!({x})")),
                IntKind::U64 => self.w(ind, format!("e.uint!({x})")),
                _ => self.w(ind, format!("e.int!({x}.to_i)")),
            },
            Ty::Float => self.w(ind, format!("e.float!({x})")),
            Ty::Bool => self.w(ind, format!("e.bool!({x})")),
            Ty::Str => self.w(ind, format!("e.str!({x})")),
            Ty::Named(_) => self.w(ind, format!("{x}.json_enc(e)")),
            Ty::Opt(inner) => {
                let v = self.fresh("o");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc(inner, &v, ind + 1)?;
                self.w(ind, "} else {");
                self.w(ind + 1, "e.raw!(\"null\")");
                self.w(ind, "}");
            }
            Ty::Arr(inner) => {
                let (i, v) = (self.fresh("i"), self.fresh("x"));
                self.w(ind, "e.raw!(\"[\")");
                self.w(ind, format!("{i} = 0"));
                self.w(ind, format!("for {v} in {x} {{"));
                self.w(ind + 1, format!("e.raw!(\",\") if {i} > 0"));
                self.w(ind + 1, format!("{i} += 1"));
                self.enc(inner, &v, ind + 1)?;
                self.w(ind, "}");
                self.w(ind, "e.raw!(\"]\")");
            }
            Ty::Map(inner) => {
                let (i, k, v) = (self.fresh("i"), self.fresh("k"), self.fresh("v"));
                self.w(ind, "e.raw!(\"{\")");
                self.w(ind, format!("{i} = 0"));
                self.w(ind, format!("for {k}, {v} in {x} {{"));
                self.w(ind + 1, format!("e.raw!(\",\") if {i} > 0"));
                self.w(ind + 1, format!("{i} += 1"));
                self.w(ind + 1, format!("e.str!({k})"));
                self.w(ind + 1, "e.raw!(\":\")");
                self.enc(inner, &v, ind + 1)?;
                self.w(ind, "}");
                self.w(ind, "e.raw!(\"}\")");
            }
            Ty::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                self.w(ind, format!("{} = {x}", names.join(", ")));
                self.w(ind, "e.raw!(\"[\")");
                for (k, (t, n)) in ts.iter().zip(&names).enumerate() {
                    if k > 0 {
                        self.w(ind, "e.raw!(\",\")");
                    }
                    self.enc(t, n, ind)?;
                }
                self.w(ind, "e.raw!(\"]\")");
            }
        }
        Ok(())
    }

    /// A test for "empty" (Go's omitempty), or None if the type is never empty.
    fn empty(&self, t: &TypeExpr, x: &str) -> Option<String> {
        match t {
            TypeExpr::Named(n, _) if IntKind::from_name(n).is_some() => Some(format!("{x} == 0")),
            TypeExpr::Named(n, _) if n == "Float" => Some(format!("{x} == 0.0")),
            TypeExpr::Named(n, _) if n == "Bool" => Some(format!("!{x}")),
            TypeExpr::Named(n, _) if n == "Str" => Some(format!("{x}.size == 0")),
            TypeExpr::Array(..) => Some(format!("{x}.size == 0")),
            TypeExpr::App(n, ..) if n == "Map" => Some(format!("{x}.size == 0")),
            TypeExpr::Opt(..) => Some(format!("{x}.none?")),
            _ => None,
        }
    }

    /// Statements that write `{"k":v,...}` for named fields held in `vals`
    /// (expressions) to `e`.
    fn enc_object(&mut self, fields: &'a [DField], vals: &[String], ind: usize) -> GResult<()> {
        let live: Vec<usize> = (0..fields.len()).filter(|&k| !fields[k].opts.skip).collect();
        let dynamic = live.iter().any(|&k| fields[k].opts.omit_empty && self.empty(&fields[k].ty, &vals[k]).is_some());
        if !dynamic {
            if live.is_empty() {
                self.w(ind, "e.raw!(\"{}\")");
                return Ok(());
            }
            for (pos, &k) in live.iter().enumerate() {
                let key = json_quote(fields[k].opts.rename.as_deref().unwrap_or(&fields[k].name));
                let open = if pos == 0 { "{" } else { "," };
                self.w(ind, format!("e.raw!({})", lit(&format!("{open}{key}:"))));
                self.enc(&fields[k].ty, &vals[k], ind)?;
            }
            self.w(ind, "e.raw!(\"}\")");
            return Ok(());
        }
        let n = self.fresh("n");
        self.w(ind, "e.raw!(\"{\")");
        self.w(ind, format!("{n} = 0"));
        for &k in &live {
            let key = json_quote(fields[k].opts.rename.as_deref().unwrap_or(&fields[k].name));
            let cond = if fields[k].opts.omit_empty { self.empty(&fields[k].ty, &vals[k]) } else { None };
            let ind2 = if let Some(c) = &cond {
                self.w(ind, format!("if !({c}) {{"));
                ind + 1
            } else {
                ind
            };
            self.w(ind2, format!("e.raw!({n} == 0 ? {} : {})", lit(&format!("{key}:")), lit(&format!(",{key}:"))));
            self.w(ind2, format!("{n} += 1"));
            self.enc(&fields[k].ty, &vals[k], ind2)?;
            if cond.is_some() {
                self.w(ind, "}");
            }
        }
        self.w(ind, "e.raw!(\"}\")");
        Ok(())
    }

    /// Statements that read a value of type `t` from `d` into a new local `dest`.
    fn dec(&mut self, t: &'a TypeExpr, dest: &str, ind: usize) -> GResult<()> {
        match self.classify(t)? {
            Ty::Int(k) => match k {
                IntKind::I64 => self.w(ind, format!("{dest} = d.~int!")),
                IntKind::U64 => self.w(ind, format!("{dest} = d.~uint!(64)")),
                k if k.signed() => self.w(ind, format!("{dest} = d.~sint!({}).as_{}", k.bits(), k.method())),
                k => self.w(ind, format!("{dest} = d.~uint!({}).as_{}", k.bits(), k.method())),
            },
            Ty::Float => self.w(ind, format!("{dest} = d.~float!")),
            Ty::Bool => self.w(ind, format!("{dest} = d.~bool!")),
            Ty::Str => self.w(ind, format!("{dest} = d.~str!")),
            Ty::Named(n) => self.w(ind, format!("{dest} = {n}.~json_dec(d)")),
            Ty::Opt(inner) => {
                let (isnull, v) = (self.fresh("z"), self.fresh("v"));
                self.w(ind, format!("{dest}: {} = none", type_src(t)));
                self.w(ind, format!("{isnull} = d.~take_null!"));
                self.w(ind, format!("if !{isnull} {{"));
                self.dec(inner, &v, ind + 1)?;
                self.w(ind + 1, format!("{dest} = {v}"));
                self.w(ind, "}");
            }
            Ty::Arr(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("{dest}: {} = []", type_src(t)));
                self.w(ind, "if d.~open_arr! {");
                self.w(ind + 1, "while d.~more! {");
                self.dec(inner, &v, ind + 2)?;
                self.w(ind + 2, format!("{dest} << {v}"));
                self.w(ind + 1, "}");
                self.w(ind, "}");
            }
            Ty::Map(inner) => {
                let (k, v) = (self.fresh("k"), self.fresh("v"));
                self.w(ind, format!("{dest}: {} = {{}}", type_src(t)));
                self.w(ind, "if d.~open_obj! {");
                self.w(ind + 1, "while d.~next_key! {");
                self.w(ind + 2, format!("{k} = d.key"));
                self.dec(inner, &v, ind + 2)?;
                self.w(ind + 2, format!("{dest}[{k}] = {v}"));
                self.w(ind + 1, "}");
                self.w(ind, "}");
            }
            Ty::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                self.w(ind, "d.~open_tuple!");
                for (k, (t, n)) in ts.iter().zip(&names).enumerate() {
                    self.w(ind, format!("d.~tuple_next!({k}, {})", ts.len()));
                    self.dec(t, n, ind)?;
                }
                self.w(ind, format!("d.~tuple_end!({})", ts.len()));
                self.w(ind, format!("{dest} = ({})", names.join(", ")));
            }
        }
        Ok(())
    }

    /// Does a missing key leave this type without a zero value we can spell
    /// in a temporary? (A named type inside a variant's named fields.)
    fn needs_stage(&self, t: &TypeExpr) -> bool {
        matches!(t, TypeExpr::Named(n, _) if IntKind::from_name(n).is_none() && !matches!(n.as_str(), "Float" | "Bool" | "Str"))
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
    let (pubk, name, a) = (if job.public { "pub " } else { "" }, job.name.clone(), alias.to_string());
    let tp = String::new();
    let _ = &tp;
    g.w(0, format!("struct {name} {{"));
    // ---- encoding
    g.w(1, format!("{pubk}def json_enc(e: {a}.Encoder) {{"));
    match &job.shape {
        DShape::Struct(fields) => {
            check_keys(fields.iter(), "").map_err(&fail)?;
            let vals: Vec<String> = fields.iter().map(|f| format!("self.{}", f.name)).collect();
            g.enc_object(fields, &vals, 2).map_err(&fail)?;
        }
        DShape::Enum(vs) => {
            check_keys(vs.iter().map(|v| DField { name: v.name.clone(), ty: TypeExpr::Named("Str".into(), job.span), opts: v.opts.clone(), span: job.span }).collect::<Vec<_>>().iter(), "variants").map_err(&fail)?;
            if vs.is_empty() {
                g.w(2, "e.raw!(\"null\")");
            } else {
                g.w(2, "case self {");
                for v in vs {
                    let vname = v.opts.rename.clone().unwrap_or_else(|| v.name.clone());
                    let key = json_quote(&vname);
                    if v.fields.is_empty() {
                        g.w(3, format!("{} => e.str!({})", v.name, lit(&vname)));
                        continue;
                    }
                    let vals: Vec<String> = (0..v.fields.len()).map(|k| format!("_f{k}")).collect();
                    g.w(3, format!("{}({}) => {{", v.name, vals.join(", ")));
                    g.w(4, format!("e.raw!({})", lit(&format!("{{{key}:"))));
                    let named = v.fields.iter().any(|f| f.name.parse::<usize>().is_err());
                    if named {
                        if v.fields.iter().any(|f| f.name.parse::<usize>().is_ok()) {
                            return Err(fail(format!("variant `{}` mixes named and unnamed fields", v.name)));
                        }
                        check_keys(v.fields.iter(), &format!("variant {}", v.name)).map_err(&fail)?;
                        g.enc_object(&v.fields, &vals, 4).map_err(&fail)?;
                    } else if v.fields.len() == 1 {
                        g.enc(&v.fields[0].ty, &vals[0], 4).map_err(&fail)?;
                    } else {
                        g.w(4, "e.raw!(\"[\")");
                        for (k, f) in v.fields.iter().enumerate() {
                            if k > 0 {
                                g.w(4, "e.raw!(\",\")");
                            }
                            g.enc(&f.ty, &vals[k], 4).map_err(&fail)?;
                        }
                        g.w(4, "e.raw!(\"]\")");
                    }
                    g.w(4, "e.raw!(\"}\")");
                    g.w(3, "}");
                }
                g.w(2, "}");
            }
        }
    }
    g.w(1, "}");
    // ---- decoding
    g.w(1, format!("{pubk}def self.json_dec(d: {a}.Decoder) -> ~{name}<{a}.JsonError> {{"));
    match &job.shape {
        DShape::Struct(fields) => {
            g.w(2, format!("r = {name}.new"));
            g.w(2, "if d.~open_obj! {");
            g.w(3, "while d.~next_key! {");
            g.w(4, "case d.key {");
            for f in fields.iter().filter(|f| !f.opts.skip) {
                let key = f.opts.rename.clone().unwrap_or_else(|| f.name.clone());
                g.w(5, format!("{} => {{", lit(&key)));
                g.w(6, format!("d.at!({})", lit(&format!("{name}.{}", f.name))));
                let v = g.fresh("v");
                g.dec(&f.ty, &v, 6).map_err(&fail)?;
                g.w(6, format!("r.{} = {v}", f.name));
                g.w(5, "}");
            }
            g.w(5, "_ => d.~skip!");
            g.w(4, "}");
            g.w(3, "}");
            g.w(2, "}");
            g.w(2, "r");
        }
        DShape::Enum(vs) => {
            if vs.is_empty() {
                return Err(fail("an enum without variants has no JSON form".to_string()));
            }
            g.w(2, "vname = d.~variant!");
            g.w(2, "wrapped = d.wrapped");
            g.w(2, "r = case vname {");
            for v in vs {
                let vname = v.opts.rename.clone().unwrap_or_else(|| v.name.clone());
                g.w(3, format!("{} => {{", lit(&vname)));
                let ctx = format!("{name}.{}", v.name);
                if v.fields.is_empty() {
                    g.w(4, "d.~unit_variant!(wrapped)");
                    g.w(4, format!("{name}.{}", v.name));
                } else {
                    g.w(4, format!("d.at!({})", lit(&ctx)));
                    g.w(4, "d.~need_payload!(wrapped)");
                    let named = v.fields.iter().any(|f| f.name.parse::<usize>().is_err());
                    let mut args: Vec<String> = vec![];
                    if named {
                        // Named fields: an object. Each field gets a temporary;
                        // a named (non-primitive) type is staged as `T?` and required.
                        let mut stages: Vec<(String, bool)> = vec![];
                        g.w(4, "if d.~open_obj! {");
                        let mut tmp: Vec<String> = vec![];
                        for f in v.fields.iter() {
                            let t = g.fresh("p");
                            let staged = g.needs_stage(&f.ty);
                            if staged {
                                g.w(5, format!("{t}: {}? = none", type_src(&f.ty)));
                            } else {
                                match g.zero(&f.ty) {
                                    Some(z) => g.w(5, format!("{t}: {} = {z}", type_src(&f.ty))),
                                    None => return Err(fail(format!("field `{}` of variant `{}` has no zero value", f.name, v.name))),
                                }
                            }
                            stages.push((t.clone(), staged));
                            tmp.push(t);
                        }
                        g.w(5, "while d.~next_key! {");
                        g.w(6, "case d.key {");
                        for (f, (t, staged)) in v.fields.iter().zip(&stages) {
                            if f.opts.skip {
                                continue;
                            }
                            let key = f.opts.rename.clone().unwrap_or_else(|| f.name.clone());
                            g.w(7, format!("{} => {{", lit(&key)));
                            let x = g.fresh("v");
                            g.dec(&f.ty, &x, 8).map_err(&fail)?;
                            let _ = staged;
                            g.w(8, format!("{t} = {x}"));
                            g.w(7, "}");
                        }
                        g.w(7, "_ => d.~skip!");
                        g.w(6, "}");
                        g.w(5, "}");
                        g.w(4, "}");
                        for (t, staged) in &stages {
                            if *staged {
                                args.push(format!("{t}!"));
                            } else {
                                args.push(t.clone());
                            }
                        }
                    } else if v.fields.len() == 1 {
                        let x = g.fresh("v");
                        g.dec(&v.fields[0].ty, &x, 4).map_err(&fail)?;
                        args.push(x);
                    } else {
                        g.w(4, "d.~open_tuple!");
                        for (k, f) in v.fields.iter().enumerate() {
                            g.w(4, format!("d.~tuple_next!({k}, {})", v.fields.len()));
                            let x = g.fresh("v");
                            g.dec(&f.ty, &x, 4).map_err(&fail)?;
                            args.push(x);
                        }
                        g.w(4, format!("d.~tuple_end!({})", v.fields.len()));
                    }
                    // Staged (required) fields: unwrap with an error when missing.
                    let mut call_args = vec![];
                    let mut open = 0;
                    for (k, a) in args.iter().enumerate() {
                        if let Some(t) = a.strip_suffix('!') {
                            let u = g.fresh("u");
                            g.w(4 + open, format!("if {u} = {t} {{"));
                            open += 1;
                            call_args.push(u);
                            let _ = k;
                        } else {
                            call_args.push(a.clone());
                        }
                    }
                    g.w(4 + open, format!("{name}.{}({})", v.name, call_args.join(", ")));
                    for k in (0..open).rev() {
                        g.w(4 + k, format!("}} else {{ fail d.missing_field({}) }}", lit(&ctx)));
                    }
                }
                g.w(3, "}");
            }
            g.w(3, format!("_ => fail d.bad_variant({}, vname)", lit(&name)));
            g.w(2, "}");
            g.w(2, "d.~end_variant!(wrapped)");
            g.w(2, "r");
        }
    }
    g.w(1, "}");
    // ---- the conveniences
    g.w(1, format!("{pubk}def to_json -> ~Str<{a}.JsonError> {{"));
    g.w(2, format!("e = {a}.new_encoder()"));
    g.w(2, "self.json_enc(e)");
    g.w(2, "e.~finish!");
    g.w(1, "}");
    g.w(1, format!("{pubk}def to_json_indent(prefix: Str, indent: Str) -> ~Str<{a}.JsonError> {{"));
    g.w(2, "s = self.~to_json");
    g.w(2, format!("{a}.~indent(s, prefix, indent)"));
    g.w(1, "}");
    g.w(1, format!("{pubk}def self.from_json(s: Str) -> ~{name}<{a}.JsonError> {{"));
    g.w(2, format!("d = {a}.new_decoder(s)"));
    g.w(2, format!("v = {name}.~json_dec(d)"));
    g.w(2, "d.~done!");
    g.w(2, "v");
    g.w(1, "}");
    g.w(0, "}");
    let _ = (g.alias, g.owner);
    Ok(g.out)
}

impl<'a> Gen<'a> {
    /// A spelled zero value for a primitive/container type.
    fn zero(&self, t: &TypeExpr) -> Option<String> {
        Some(match t {
            TypeExpr::Named(n, _) if IntKind::from_name(n).is_some() => "0".into(),
            TypeExpr::Named(n, _) if n == "Float" => "0.0".into(),
            TypeExpr::Named(n, _) if n == "Bool" => "false".into(),
            TypeExpr::Named(n, _) if n == "Str" => "\"\"".into(),
            TypeExpr::Array(..) => "[]".into(),
            TypeExpr::App(n, ..) if n == "Map" => "{}".into(),
            TypeExpr::Opt(..) => "none".into(),
            TypeExpr::Tuple(ts, _) => format!("({})", ts.iter().map(|t| self.zero(t)).collect::<Option<Vec<_>>>()?.join(", ")),
            _ => return None,
        })
    }
}

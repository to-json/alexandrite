//! `#[derive(Json)]`, part two: the encoding/json/v2 methods (GO-VS-RUBY D70).
//!
//! The same derive as derive.rs (v1), the same field analysis and tags
//! (`derive::Opts`, parsed once by `apply_field_attr`), written only when the
//! file imports `encoding/json/v2` or `encoding/json/jsontext` (so programs
//! using only v1 don't compile v2). The methods stream through a jsontext
//! Encoder / Decoder and call the per-kind functions of package v2 (its
//! arshal.alx, fields.alx), passing Go's spelling of each field's type for
//! error messages:
//!
//!   def json_v2_type -> Str
//!   def json_v2_enc(_e: JT.Encoder, _o: JT.Options) -> ~Unit<Error>
//!   def json_v2_dec(_d: JT.Decoder, _o: JT.Options) -> ~T<Error>
//!   def json_v2_is_zero -> Bool
//!   structs also: json_v2_fields_enc, self.json_v2_lookup, self.json_v2_has,
//!   json_v2_field_dec (what an embedding struct calls)

use crate::ast::*;
use crate::derive::{type_src, DShape, DeriveJob, DField, ModuleTypes};
use crate::diag::Diag;
use std::collections::HashMap;

/// A string as an alx literal.
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

pub struct Ctx<'a> {
    /// local names of the v2 and jsontext imports
    pub j: &'a str,
    pub t: &'a str,
    pub types: &'a ModuleTypes,
    /// Go type names given with `#[data("pkg.T")]`
    pub go_names: &'a HashMap<String, String>,
}

struct G<'a> {
    c: &'a Ctx<'a>,
    tparams: &'a [String],
    out: String,
    n: usize,
}

type GR<T> = Result<T, String>;

#[derive(Clone, Copy)]
enum K<'a> {
    Int(IntKind),
    Float,
    Bool,
    Str,
    Complex,
    Bytes,
    FixedBytes(u64),
    Opt(&'a TypeExpr),
    Arr(&'a TypeExpr),
    Fixed(&'a TypeExpr, u64),
    Map(&'a TypeExpr, &'a TypeExpr),
    Tuple(&'a [TypeExpr]),
    Raw,
    Named(&'a str),
}

impl<'a> G<'a> {
    fn w(&mut self, ind: usize, s: impl AsRef<str>) {
        for _ in 0..ind {
            self.out.push_str("  ");
        }
        self.out.push_str(s.as_ref());
        self.out.push('\n');
    }
    fn fresh(&mut self, b: &str) -> String {
        self.n += 1;
        format!("_{b}{}", self.n)
    }

    fn shadowed(&self, n: &str) -> bool {
        crate::check::SHADOWABLE.contains(&n) && self.c.types.local.contains_key(n)
    }

    fn kind(&self, t: &'a TypeExpr) -> GR<K<'a>> {
        Ok(match t {
            TypeExpr::Named(n, _) if self.shadowed(n) => K::Named(n),
            TypeExpr::Named(n, _) => {
                if let Some(k) = IntKind::from_name(n) {
                    K::Int(k)
                } else {
                    match n.as_str() {
                        "Float" | "F64" => K::Float,
                        "Bool" => K::Bool,
                        "Str" => K::Str,
                        "Complex" => K::Complex,
                        _ if n.ends_with(".Value") && n.split('.').next() == Some(self.c.t) => K::Raw,
                        _ if self.tparams.contains(n) => return Err(format!("the type parameter `{n}` can't be encoded: derive(Json) on a generic type isn't supported yet")),
                        // A local type that doesn't derive Json provides the
                        // protocol itself (json_v2_enc / json_v2_dec / json_zero).
                        _ => K::Named(n),
                    }
                }
            }
            TypeExpr::Opt(e, _) => K::Opt(e),
            TypeExpr::Array(e, _) => match &**e {
                TypeExpr::Named(n, _) if IntKind::from_name(n) == Some(IntKind::U8) => K::Bytes,
                _ => K::Arr(e),
            },
            TypeExpr::Fixed(e, n, _) => {
                let len = match &n.kind {
                    ExprKind::Int(v) => *v as u64,
                    _ => return Err("a fixed array needs a literal length for derive(Json)".into()),
                };
                match &**e {
                    TypeExpr::Named(n, _) if IntKind::from_name(n) == Some(IntKind::U8) => K::FixedBytes(len),
                    _ => K::Fixed(e, len),
                }
            }
            TypeExpr::App(n, a, _) if n == "Map" && a.len() == 2 => K::Map(&a[0], &a[1]),
            TypeExpr::App(n, ..) => return Err(format!("`{n}[...]`: generic types can't be derived yet")),
            TypeExpr::Tuple(ts, _) => K::Tuple(ts),
            TypeExpr::Result(..) => return Err("a fallible type has no JSON encoding".into()),
            TypeExpr::Handle(..) => return Err("a pool handle `@T` has no JSON encoding".into()),
            TypeExpr::Fn(..) => return Err("a function type has no JSON encoding".into()),
        })
    }

    /// Go's spelling of a type, for error messages.
    fn go(&self, t: &'a TypeExpr) -> GR<String> {
        Ok(match self.kind(t)? {
            K::Int(k) => match k {
                IntKind::I64 => "int".into(),
                IntKind::I8 => "int8".into(),
                IntKind::I16 => "int16".into(),
                IntKind::I32 => "int32".into(),
                IntKind::U8 => "uint8".into(),
                IntKind::U16 => "uint16".into(),
                IntKind::U32 => "uint32".into(),
                IntKind::U64 => "uint64".into(),
            },
            K::Float => "float64".into(),
            K::Bool => "bool".into(),
            K::Str => "string".into(),
            K::Complex => "complex128".into(),
            K::Bytes => "[]uint8".into(),
            K::FixedBytes(n) => format!("[{n}]uint8"),
            K::Opt(e) => format!("*{}", self.go(e)?),
            K::Arr(e) => format!("[]{}", self.go(e)?),
            K::Fixed(e, n) => format!("[{n}]{}", self.go(e)?),
            K::Map(k, v) => format!("map[{}]{}", self.go(k)?, self.go(v)?),
            K::Tuple(ts) => format!("({})", ts.iter().map(|t| self.go(t)).collect::<GR<Vec<_>>>()?.join(", ")),
            K::Raw => "jsontext.Value".into(),
            K::Named(n) => self.c.go_names.get(n).cloned().unwrap_or_else(|| n.to_string()),
        })
    }

    /// An expression for the zero value of t.
    fn zero(&self, t: &'a TypeExpr) -> GR<String> {
        Ok(match self.kind(t)? {
            K::Int(_) => "0".into(),
            K::Float => "0.0".into(),
            K::Bool => "false".into(),
            K::Str => "\"\"".into(),
            K::Complex => "Complex.new(0.0, 0.0)".into(),
            K::Bytes | K::Arr(_) => "[]".into(),
            K::FixedBytes(n) => format!("[0; {n}]"),
            K::Fixed(e, n) => format!("[{}; {n}]", self.zero(e)?),
            K::Map(..) => "{}".into(),
            K::Opt(_) => "none".into(),
            K::Tuple(ts) => format!("({})", ts.iter().map(|t| self.zero(t)).collect::<GR<Vec<_>>>()?.join(", ")),
            K::Raw => format!("{}.Value.new(raw: [])", self.c.t),
            K::Named(n) => format!("{n}.json_zero"),
        })
    }

    /// A Bool expression: is x (of type t) Go's zero value (omitzero)?
    fn is_zero(&self, t: &'a TypeExpr, x: &str) -> GR<String> {
        Ok(match self.kind(t)? {
            K::Int(_) => format!("{x} == 0"),
            K::Float => format!("({x} == 0.0 && 1.0 / {x} > 0.0)"),
            K::Bool => format!("!{x}"),
            K::Str => format!("{x}.size == 0"),
            K::Complex => format!("({x}.real == 0.0 && {x}.imag == 0.0)"),
            K::Bytes | K::Arr(_) | K::Map(..) => format!("{x}.size == 0"),
            K::FixedBytes(_) => format!("{x}.all? {{ |_w| _w == 0 }}"),
            K::Fixed(e, _) => {
                let w = format!("_w{}", x.len());
                format!("{x}.all? {{ |{w}| {} }}", self.is_zero(e, &w)?)
            }
            K::Opt(_) => format!("{x}.none?"),
            K::Tuple(ts) => {
                if ts.is_empty() {
                    "true".into()
                } else {
                    let mut parts = vec![];
                    for (i, e) in ts.iter().enumerate() {
                        parts.push(self.is_zero(e, &format!("{x}[{i}]"))?);
                    }
                    format!("({})", parts.join(" && "))
                }
            }
            K::Raw => format!("{x}.raw.size == 0"),
            K::Named(_) => format!("{x}.json_v2_is_zero"),
        })
    }

    /// Is x empty for omitempty's fast path (no need to marshal it)?
    fn empty_fast(&self, t: &'a TypeExpr, x: &str) -> GR<Option<String>> {
        Ok(match self.kind(t)? {
            K::Str | K::Arr(_) | K::Map(..) => Some(format!("{x}.size == 0")),
            K::Opt(_) => Some(format!("{x}.none?")),
            K::FixedBytes(0) | K::Fixed(_, 0) => Some("true".into()),
            _ => None,
        })
    }

    /// A lambda `(E, X, O) -> ~Unit<Error>` marshaling a value of type t.
    fn enc_lambda(&mut self, t: &'a TypeExpr, ind: usize) -> GR<String> {
        let (e, x, o) = (self.fresh("e"), self.fresh("x"), self.fresh("o"));
        let saved = std::mem::take(&mut self.out);
        self.enc(t, &x, &e, &o, ind + 1)?;
        self.w(ind + 1, "nil");
        let body = std::mem::replace(&mut self.out, saved);
        let pad = "  ".repeat(ind);
        Ok(format!("->({e}: {jt}.Encoder, {x}: {ty}, {o}: {jt}.Options) -> ~Unit<Error> {{\n{body}{pad}}}", jt = self.c.t, ty = type_src(t)))
    }

    /// A lambda `(D, X, O) -> ~X<Error>` unmarshaling into a value of type t.
    fn dec_lambda(&mut self, t: &'a TypeExpr, ind: usize) -> GR<String> {
        let (d, c, o, r) = (self.fresh("d"), self.fresh("c"), self.fresh("o"), self.fresh("r"));
        let saved = std::mem::take(&mut self.out);
        self.dec(t, &c, &d, &o, &r, ind + 1)?;
        self.w(ind + 1, &r);
        let body = std::mem::replace(&mut self.out, saved);
        let pad = "  ".repeat(ind);
        Ok(format!("->({d}: {jt}.Decoder, {c}: {ty}, {o}: {jt}.Options) -> ~{ty}<Error> {{\n{body}{pad}}}", jt = self.c.t, ty = type_src(t)))
    }

    /// Statements marshaling x (of type t) to encoder e with options o.
    fn enc(&mut self, t: &'a TypeExpr, x: &str, e: &str, o: &str, ind: usize) -> GR<()> {
        let j = self.c.j.to_string();
        let jt = self.c.t.to_string();
        let gt = lit(&self.go(t)?);
        match self.kind(t)? {
            K::Int(k) if k.signed() => self.w(ind, format!("~{j}.marshal_int({e}, {x}.as_i64, {o}, {gt})")),
            K::Int(_) => self.w(ind, format!("~{j}.marshal_uint({e}, {x}.as_u64, {o}, {gt})")),
            K::Float => self.w(ind, format!("~{j}.marshal_float({e}, {x}, {o}, {gt}, 64)")),
            K::Bool => self.w(ind, format!("~{j}.marshal_bool({e}, {x}, {o}, {gt})")),
            K::Str => self.w(ind, format!("~{j}.marshal_string({e}, {x}, {o}, {gt})")),
            K::Complex => self.w(ind, format!("~{j}.marshal_unsupported({e}, {gt})")),
            K::Bytes => self.w(ind, format!("~{j}.marshal_bytes({e}, {x}, {o}, {gt})")),
            K::FixedBytes(_) => {
                let s = self.fresh("s");
                self.w(ind, format!("{s}: [Byte] = []"));
                self.w(ind, format!("for _b in {x} {{ {s} << _b }}"));
                self.w(ind, format!("~{j}.marshal_bytes({e}, {s}, {o}, {gt})"));
            }
            K::Opt(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc(inner, &v, e, o, ind + 1)?;
                self.w(ind, "} else {");
                self.w(ind + 1, format!("~{e}.write_token!({jt}.Token.null)"));
                self.w(ind, "}");
            }
            K::Arr(inner) => {
                let f = self.enc_lambda(inner, ind)?;
                self.w(ind, format!("~{j}.marshal_slice({e}, {x}, {o}, {gt}, {f})"));
            }
            K::Fixed(inner, _) => {
                let s = self.fresh("s");
                self.w(ind, format!("{s}: [{}] = []", type_src(inner)));
                self.w(ind, format!("for _y in {x} {{ {s} << _y }}"));
                let f = self.enc_lambda(inner, ind)?;
                self.w(ind, format!("~{j}.marshal_array({e}, {s}, {o}, {gt}, {f})"));
            }
            K::Map(kt, vt) => {
                let uniq = match self.kind(kt)? {
                    K::Int(_) | K::Bool => 1,
                    K::Str => 2,
                    K::Float => 0,
                    _ => return Err(format!("map keys must be Str, an integer type, Bool or Float, not {}", type_src(kt))),
                };
                let fk = self.enc_lambda(kt, ind)?;
                let fv = self.enc_lambda(vt, ind)?;
                let tk = lit(&self.go(kt)?);
                self.w(ind, format!("~{j}.marshal_map({e}, {x}, {o}, {gt}, {tk}, {uniq}, {fk}, {fv})"));
            }
            K::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                let tag = self.fresh("tag");
                self.w(ind, format!("if {tag} = {j}.tuple_tag_error({e}, {o}, {gt}) {{ fail {tag} }}"));
                if !names.is_empty() {
                    self.w(ind, format!("{} = {x}", names.join(", ")));
                }
                self.w(ind, format!("~{e}.write_token!({jt}.Token.begin_array)"));
                let o2 = self.fresh("o");
                self.w(ind, format!("{o2} = {j}.inner_opts({o})"));
                for (t, n) in ts.iter().zip(&names) {
                    self.enc(t, n, e, &o2, ind)?;
                }
                self.w(ind, format!("~{e}.write_token!({jt}.Token.end_array)"));
            }
            K::Raw => self.w(ind, format!("~{j}.marshal_raw({e}, {x}, {o}, {gt})")),
            K::Named(_) => self.w(ind, format!("{x}.~json_v2_enc({e}, {o})")),
        }
        Ok(())
    }

    /// Statements decoding a value of type t (current value: expression cur)
    /// from d into a new local `dest`.
    fn dec(&mut self, t: &'a TypeExpr, cur: &str, d: &str, o: &str, dest: &str, ind: usize) -> GR<()> {
        let j = self.c.j.to_string();
        let jt = self.c.t.to_string();
        let gt = lit(&self.go(t)?);
        match self.kind(t)? {
            K::Int(k) if k == IntKind::I64 => self.w(ind, format!("{dest} = ~{j}.unmarshal_int({d}, {cur}, {o}, {gt}, 64)")),
            K::Int(k) if k.signed() => self.w(ind, format!("{dest} = ~{j}.unmarshal_int({d}, {cur}.as_i64, {o}, {gt}, {}).as_{}", k.bits(), k.method())),
            K::Int(k) => self.w(ind, format!("{dest} = ~{j}.unmarshal_uint({d}, {cur}.as_u64, {o}, {gt}, {}).as_{}", k.bits(), k.method())),
            K::Float => self.w(ind, format!("{dest} = ~{j}.unmarshal_float({d}, {cur}, {o}, {gt}, 64)")),
            K::Bool => self.w(ind, format!("{dest} = ~{j}.unmarshal_bool({d}, {cur}, {o}, {gt})")),
            K::Str => self.w(ind, format!("{dest} = ~{j}.unmarshal_string({d}, {cur}, {o}, {gt})")),
            K::Complex => self.w(ind, format!("{dest} = ~{j}.unmarshal_unsupported({d}, {cur}, {gt})")),
            K::Bytes => self.w(ind, format!("{dest} = ~{j}.unmarshal_bytes({d}, {cur}, {o}, {gt}, -1)")),
            K::FixedBytes(n) => {
                let s = self.fresh("s");
                self.w(ind, format!("{s} = ~{j}.unmarshal_bytes({d}, [], {o}, {gt}, {n})"));
                self.w(ind, format!("{dest}: [Byte; {n}] = [0; {n}]"));
                self.w(ind, format!("for _i in 0...{n} {{ {dest}[_i] = {s}[_i] }}"));
            }
            K::Opt(inner) => {
                let b = self.fresh("b");
                let v = self.fresh("v");
                self.w(ind, format!("{dest}: {} = none", type_src(t)));
                self.w(ind, format!("if {d}.peek_kind! == {jt}.Kind.Null {{"));
                self.w(ind + 1, format!("~{j}.read_token({d})"));
                self.w(ind, "} else {");
                self.w(ind + 1, format!("{b} = {cur} || {}", self.zero(inner)?));
                self.dec(inner, &b, d, o, &v, ind + 1)?;
                self.w(ind + 1, format!("{dest} = {v}"));
                self.w(ind, "}");
            }
            K::Arr(inner) => {
                let z = self.zero(inner)?;
                let f = self.dec_lambda(inner, ind)?;
                self.w(ind, format!("{dest} = ~{j}.unmarshal_slice({d}, {cur}, {o}, {gt}, {z}, {f})"));
            }
            K::Fixed(inner, n) => {
                let z = self.zero(inner)?;
                let s = self.fresh("s");
                let c2 = self.fresh("cs");
                self.w(ind, format!("{c2}: [{}] = []", type_src(inner)));
                self.w(ind, format!("for _y in {cur} {{ {c2} << _y }}"));
                let f = self.dec_lambda(inner, ind)?;
                self.w(ind, format!("{s} = ~{j}.unmarshal_array({d}, {c2}, {o}, {gt}, {z}, {f})"));
                self.w(ind, format!("{dest}: {} = {}", type_src(t), self.zero(t)?));
                self.w(ind, format!("for _i in 0...{n} {{ {dest}[_i] = {s}[_i] }}"));
            }
            K::Map(kt, vt) => {
                let uniq = match self.kind(kt)? {
                    K::Int(_) | K::Bool => 1,
                    K::Str => 2,
                    K::Float => 0,
                    _ => return Err(format!("map keys must be Str, an integer type, Bool or Float, not {}", type_src(kt))),
                };
                let (zk, zv) = (self.zero(kt)?, self.zero(vt)?);
                let fk = self.dec_lambda(kt, ind)?;
                let fv = self.dec_lambda(vt, ind)?;
                self.w(ind, format!("{dest} = ~{j}.unmarshal_map({d}, {cur}, {o}, {gt}, {uniq}, {zk}, {zv}, {fk}, {fv})"));
            }
            K::Tuple(ts) => {
                let names: Vec<String> = ts.iter().map(|_| self.fresh("t")).collect();
                let curs: Vec<String> = ts.iter().map(|_| self.fresh("u")).collect();
                self.w(ind, format!("{dest}: {} = {}", type_src(t), self.zero(t)?));
                if !curs.is_empty() {
                    self.w(ind, format!("{} = {cur}", curs.join(", ")));
                }
                self.w(ind, format!("if ~{j}.begin_tuple({d}, {o}, {gt}) {{"));
                let o2 = self.fresh("o");
                self.w(ind + 1, format!("{o2} = {j}.inner_opts({o})"));
                for (k, t) in ts.iter().enumerate() {
                    self.w(ind + 1, format!("~{j}.tuple_elem({d}, {gt})"));
                    self.dec(t, &curs[k], d, &o2, &names[k], ind + 1)?;
                }
                self.w(ind + 1, format!("~{j}.end_tuple({d}, {gt})"));
                self.w(ind + 1, format!("{dest} = ({})", names.join(", ")));
                self.w(ind, "}");
            }
            K::Raw => self.w(ind, format!("{dest} = ~{j}.unmarshal_raw({d}, {cur}, {o}, {gt})")),
            K::Named(_) => self.w(ind, format!("{dest} = {cur}.~json_v2_dec({d}, {o})")),
        }
        Ok(())
    }

    /// The field options expression for field f under struct options fo.
    fn fopts(&self, f: &DField, fo: &str) -> String {
        if !f.opts.string_tag && f.opts.format.is_none() {
            return fo.to_string();
        }
        format!("{}.field_opts({fo}, {}, {})", self.c.j, f.opts.string_tag, lit(f.opts.format.as_deref().unwrap_or("")))
    }
}

fn json_name(f: &DField) -> String {
    f.opts.rename.clone().unwrap_or_else(|| f.name.clone())
}

/// Is this field an embedded fallback (a jsontext.Value or Map[Str, V] tagged embed)?
fn fallback_kind<'a>(g: &G<'a>, f: &'a DField) -> GR<Option<K<'a>>> {
    if !f.opts.embed {
        return Ok(None);
    }
    match g.kind(&f.ty)? {
        k @ K::Raw => Ok(Some(k)),
        k @ K::Map(kt, _) if matches!(g.kind(kt)?, K::Str) => Ok(Some(k)),
        K::Named(_) => Ok(None),
        _ => Err(format!("field `{}` tagged embed must be a struct that derives Json, a jsontext.Value or a Map[Str, V]", f.name)),
    }
}

/// The source text of `struct Name { defs }` with the v2 methods.
pub fn json2_source(job: &DeriveJob, go_name: &str, c: &Ctx) -> Result<String, Diag> {
    let fail = |m: String| Diag::new(job.span, format!("derive(Json) on `{}` (encoding/json/v2): {m}", job.name));
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Json yet".into()));
    }
    let mut g = G { c, tparams: &job.tparams, out: String::new(), n: 0 };
    let (j, jt, name) = (c.j, c.t, job.name.as_str());
    // A transparent struct is its field, also in Go's type names.
    let tfield = match (&job.shape, job.transparent) {
        (DShape::Struct(fields), true) => Some(crate::derive::transparent_field(fields).map_err(fail)?),
        _ => None,
    };
    let go_name = match (tfield, &job.go_name) {
        (Some(f), None) => g.go(&f.ty).map_err(fail)?,
        _ => go_name.to_string(),
    };
    let gt = lit(&go_name);
    g.w(0, format!("struct {name} {{"));
    g.w(1, format!("pub def json_v2_type -> Str {{ {gt} }}"));
    match &job.shape {
        DShape::Struct(_) if tfield.is_some() => transparent_methods(&mut g, job, tfield.unwrap()).map_err(fail)?,
        DShape::Struct(fields) => struct_methods(&mut g, job, fields, &gt).map_err(fail)?,
        DShape::Enum(vs) => enum_methods(&mut g, job, vs, &gt).map_err(fail)?,
    }
    // Hooks of WithMarshalers / WithUnmarshalers, then the type's own code.
    let _ = (j, jt);
    g.w(0, "}");
    Ok(g.out)
}

fn hook_prelude(g: &mut G, name: &str) {
    let (j, jt) = (g.c.j.to_string(), g.c.t.to_string());
    g.w(2, "if _o.marshalers.size > 0 {");
    g.w(3, format!("return nil if ~{j}.call_marshalers(_e, {jt}.Arg.O(self), _o)"));
    g.w(2, "}");
    let _ = name;
}

fn unhook_prelude(g: &mut G, name: &str) {
    let (j, jt) = (g.c.j.to_string(), g.c.t.to_string());
    g.w(2, "if _o.unmarshalers.size > 0 {");
    g.w(3, format!("if _h = ~{j}.call_unmarshalers(_d, {jt}.Arg.O(self), _o) {{"));
    g.w(4, "case _h {");
    g.w(5, "O(_m) => {");
    g.w(6, "case _m {");
    g.w(7, format!("{name}(_x) => {{ return _x }}"));
    g.w(7, "_ => {}");
    g.w(6, "}");
    g.w(5, "}");
    g.w(5, "_ => {}");
    g.w(4, "}");
    g.w(3, "}");
    g.w(2, "}");
}

fn transparent_methods<'a>(g: &mut G<'a>, job: &'a DeriveJob, f: &'a DField) -> GR<()> {
    let jt = g.c.t.to_string();
    let name = job.name.as_str();
    let z = g.is_zero(&f.ty, &format!("self.{}", f.name))?;
    g.w(1, format!("pub def json_v2_is_zero -> Bool {{ {z} }}"));
    g.w(1, format!("pub def json_v2_enc(_e: {jt}.Encoder, _o: {jt}.Options) -> ~Unit<Error> {{"));
    hook_prelude(g, name);
    g.enc(&f.ty, &format!("self.{}", f.name), "_e", "_o", 2)?;
    g.w(2, "nil");
    g.w(1, "}");
    g.w(1, format!("pub def json_v2_dec(_d: {jt}.Decoder, _o: {jt}.Options) -> ~{name}<Error> {{"));
    unhook_prelude(g, name);
    g.dec(&f.ty, &format!("self.{}", f.name), "_d", "_o", "_v", 2)?;
    g.w(2, "_r = self");
    g.w(2, format!("_r.{} = _v", f.name));
    g.w(2, "_r");
    g.w(1, "}");
    Ok(())
}

fn struct_methods<'a>(g: &mut G<'a>, job: &'a DeriveJob, fields: &'a [DField], gt: &str) -> GR<()> {
    let (j, jt) = (g.c.j.to_string(), g.c.t.to_string());
    let name = job.name.as_str();
    let live: Vec<&DField> = fields.iter().filter(|f| !f.opts.skip).collect();
    let mut fallback: Option<&DField> = None;
    for f in &live {
        if fallback_kind(g, f)?.is_some() {
            if fallback.is_some() {
                return Err("only one embedded fallback (a jsontext.Value or Map[Str, V] tagged embed) per struct".into());
            }
            fallback = Some(f);
        }
    }
    let members: Vec<&DField> = live.iter().copied().filter(|f| !(f.opts.embed && fallback_kind(g, f).ok().flatten().is_some())).collect();
    let direct: Vec<&DField> = members.iter().copied().filter(|f| !f.opts.embed).collect();
    let embedded: Vec<&DField> = members.iter().copied().filter(|f| f.opts.embed).collect();
    let mut seen = std::collections::HashSet::new();
    for f in &direct {
        if !seen.insert(json_name(f)) {
            return Err(format!("two fields are named \"{}\" in JSON", json_name(f)));
        }
    }
    // ---- json_v2_is_zero
    let mut zs = vec![];
    for f in &live {
        zs.push(g.is_zero(&f.ty, &format!("self.{}", f.name))?);
    }
    g.w(1, format!("pub def json_v2_is_zero -> Bool {{ {} }}", if zs.is_empty() { "true".to_string() } else { zs.join(" && ") }));
    // ---- encoding
    g.w(1, format!("pub def json_v2_enc(_e: {jt}.Encoder, _o: {jt}.Options) -> ~Unit<Error> {{"));
    hook_prelude(g, name);
    let fmt_field = live.iter().find(|f| f.opts.format.is_some()).map(|f| f.opts.drename.clone().unwrap_or_else(|| f.name.clone()));
    if let Some(ff) = &fmt_field {
        g.w(2, format!("fail {j}.unsupported_format_enc(_e, {gt}, {}) if !_o.get({jt}.F_FORMAT_TAG_SUPPORTED)", lit(ff)));
    }
    g.w(2, format!("_fo = ~{j}.begin_struct(_e, _o, {gt}, {})", !embedded.is_empty()));
    g.w(2, format!("_w = self.~json_v2_fields_enc(_e, _fo, {j}.no_names, {j}.no_names)"));
    if let Some(f) = fallback {
        let names: Vec<String> = direct.iter().map(|f| lit(&json_name(f))).collect();
        let casings: Vec<String> = direct.iter().map(|f| f.opts.casing.to_string()).collect();
        let (names, casings) = if names.is_empty() { (format!("{j}.no_names"), format!("{j}.no_casings")) } else { (format!("[{}]", names.join(", ")), format!("[{}]", casings.join(", "))) };
        g.w(2, format!("_fb = {j}.Fallback.new(seen: _w, names: {names}, casings: {casings}, checked: {})", !embedded.is_empty()));
        match g.kind(&f.ty)? {
            K::Raw => g.w(2, format!("~{j}.marshal_fallback_raw(_e, _fo, self.{}, _fb)", f.name)),
            K::Map(_, vt) => {
                let fv = g.enc_lambda(vt, 2)?;
                g.w(2, format!("~{j}.marshal_fallback_map(_e, _fo, self.{}, _fb, {fv})", f.name));
            }
            _ => unreachable!(),
        }
    }
    g.w(2, format!("~{j}.end_struct(_e)"));
    g.w(1, "}");
    // ---- the members (also what an embedding struct writes)
    // `_w`: the names written so far (the last one is what an omitempty
    // member that is taken back out restores).
    // `_hide`: names of an embedding struct's own fields, which shadow these.
    g.w(1, format!("pub def json_v2_fields_enc(_e: {jt}.Encoder, _fo: {jt}.Options, _w0: [Str], _hide: [Str]) -> ~[Str]<Error> {{"));
    g.w(2, "_w = _w0");
    let own: Vec<String> = direct.iter().map(|f| lit(&json_name(f))).collect();
    if !embedded.is_empty() {
        if own.is_empty() {
            g.w(2, "_hide2 = _hide");
        } else {
            g.w(2, format!("_hide2 = _hide + [{}]", own.join(", ")));
        }
    }
    g.w(2, format!("_oz = _fo.get({jt}.F_OMIT_ZERO_STRUCT_FIELDS)"));
    for f in &members {
        let x = format!("self.{}", f.name);
        if f.opts.embed {
            g.w(2, format!("_w = {x}.~json_v2_fields_enc(_e, _fo, _w, _hide2)"));
            continue;
        }
        let zero = g.is_zero(&f.ty, &x)?;
        let mut cond = if f.opts.omit_zero { format!("!({zero})") } else { format!("!(_oz && {zero})") };
        cond = format!("!_hide.include?({}) && {cond}", lit(&json_name(f)));
        let fast = if f.opts.omit_empty { g.empty_fast(&f.ty, &x)? } else { None };
        if let Some(fe) = &fast {
            cond = format!("{cond} && !({fe})");
        }
        let jn = json_name(f);
        g.w(2, format!("if {cond} {{"));
        g.w(3, format!("~{j}.write_name(_e, {})", lit(&jn)));
        let fo = g.fopts(f, "_fo");
        let ov = g.fresh("fo");
        g.w(3, format!("{ov} = {fo}"));
        g.enc(&f.ty, &x, "_e", &ov, 3)?;
        if f.opts.omit_empty {
            g.w(3, format!("_w << {} if !{j}.unwrite_if_empty(_e, {j}.last_name(_w))", lit(&jn)));
        } else {
            g.w(3, format!("_w << {}", lit(&jn)));
        }
        g.w(2, "}");
    }
    g.w(2, "_w");
    g.w(1, "}");
    // ---- name lookup: (canonical name, ambiguous?)
    g.w(1, format!("pub def self.json_v2_lookup(_name: Str, _o: {jt}.Options) -> (Str, Bool) {{"));
    g.w(2, "_c = \"\"");
    if !direct.is_empty() {
        g.w(2, "case _name {");
        for f in &direct {
            let jn = lit(&json_name(f));
            g.w(3, format!("{jn} => {{ _c = {jn} }}"));
        }
        g.w(3, "_ => {}");
        g.w(2, "}");
        let names: Vec<String> = direct.iter().map(|f| lit(&json_name(f))).collect();
        let casings: Vec<String> = direct.iter().map(|f| f.opts.casing.to_string()).collect();
        g.w(2, "if _c == \"\" {");
        g.w(3, format!("_names = [{}]", names.join(", ")));
        g.w(3, format!("_i = {j}.fold_match(_name, _names, [{}], _o)", casings.join(", ")));
        g.w(3, "return (\"\", true) if _i == -2");
        g.w(3, "_c = _names[_i] if _i >= 0");
        g.w(2, "}");
    }
    for f in &embedded {
        let K::Named(en) = g.kind(&f.ty)? else { return Err(format!("field `{}` tagged embed must be a struct that derives Json", f.name)) };
        g.w(2, "if _c == \"\" {");
        g.w(3, format!("_c2, _a2 = {en}.json_v2_lookup(_name, _o)"));
        g.w(3, "return (\"\", true) if _a2");
        g.w(3, "_c = _c2");
        g.w(2, "}");
    }
    g.w(2, "(_c, false)");
    g.w(1, "}");
    g.w(1, "pub def self.json_v2_has(_c: Str) -> Bool {");
    let mut hs: Vec<String> = direct.iter().map(|f| format!("_c == {}", lit(&json_name(f)))).collect();
    for f in &embedded {
        let K::Named(en) = g.kind(&f.ty)? else { unreachable!() };
        hs.push(format!("{en}.json_v2_has(_c)"));
    }
    g.w(2, if hs.is_empty() { "false".to_string() } else { hs.join(" || ") });
    g.w(1, "}");
    // ---- one field
    g.w(1, format!("pub def json_v2_field_dec(_canon: Str, _d: {jt}.Decoder, _fo: {jt}.Options) -> ~{name}<Error> {{"));
    g.w(2, "_r = self");
    g.w(2, "case _canon {");
    for f in &direct {
        let jn = lit(&json_name(f));
        g.w(3, format!("{jn} => {{"));
        let fo = g.fopts(f, "_fo");
        let ov = g.fresh("fo");
        g.w(4, format!("{ov} = {fo}"));
        let v = g.fresh("v");
        g.dec(&f.ty, &format!("_r.{}", f.name), "_d", &ov, &v, 4)?;
        g.w(4, format!("_r.{} = {v}", f.name));
        g.w(4, "nil");
        g.w(3, "}");
    }
    g.w(3, "_ => {");
    for f in &embedded {
        let K::Named(en) = g.kind(&f.ty)? else { unreachable!() };
        g.w(4, format!("if {en}.json_v2_has(_canon) {{"));
        g.w(5, format!("_r.{} = _r.{}.~json_v2_field_dec(_canon, _d, _fo)", f.name, f.name));
        g.w(4, "}");
    }
    g.w(4, "nil");
    g.w(3, "}");
    g.w(2, "}");
    g.w(2, "_r");
    g.w(1, "}");
    // ---- decoding
    g.w(1, format!("pub def json_v2_dec(_d: {jt}.Decoder, _o: {jt}.Options) -> ~{name}<Error> {{"));
    unhook_prelude(g, name);
    g.w(2, format!("return {name}.json_zero if !~{j}.begin_struct_dec(_d, _o, {gt})"));
    if let Some(ff) = &fmt_field {
        g.w(2, format!("fail {j}.unsupported_format_dec(_d, {gt}, {}) if !_o.get({jt}.F_FORMAT_TAG_SUPPORTED)", lit(ff)));
    }
    g.w(2, format!("_fo = {j}.inner_opts(_o)"));
    g.w(2, "_r = self");
    g.w(2, "_seen: Map[Str, Bool] = {}");
    g.w(2, format!("_chk = !_fo.get({jt}.F_ALLOW_DUPLICATE_NAMES)"));
    g.w(2, format!("while {j}.more_members(_d) {{"));
    g.w(3, format!("_name, _raw = ~{j}.read_name(_d)"));
    g.w(3, format!("_canon, _amb = {name}.json_v2_lookup(_name, _fo)"));
    g.w(3, format!("fail {j}.ambiguous_error(_d, {gt}) if _amb"));
    g.w(3, "if _canon == \"\" {");
    g.w(4, format!("~{j}.unknown_member(_d, _fo, {gt}, _name, _raw, {})", fallback.is_some()));
    if let Some(f) = fallback {
        match g.kind(&f.ty)? {
            K::Raw => {
                g.w(4, "_q, _qe = " .to_string() + &format!("{jt}.append_quote([], _name)"));
                g.w(4, format!("_r.{n} = ~{j}.unmarshal_fallback_raw(_d, _r.{n}, Str.from_bytes(_q))", n = f.name));
            }
            K::Map(_, vt) => {
                let zv = g.zero(vt)?;
                let fv = g.dec_lambda(vt, 4)?;
                g.w(4, format!("_r.{n} = ~{j}.unmarshal_fallback_map(_d, _fo, _r.{n}, _name, {zv}, {fv})", n = f.name));
            }
            _ => unreachable!(),
        }
    }
    g.w(3, "} else {");
    g.w(4, "if _chk {");
    g.w(5, format!("fail {j}.duplicate_field_error(_d, _raw) if _seen.has_key?(_canon)"));
    g.w(5, "_seen[_canon] = true");
    g.w(4, "}");
    g.w(4, "_r = _r.~json_v2_field_dec(_canon, _d, _fo)");
    g.w(3, "}");
    g.w(2, "}");
    g.w(2, format!("~{j}.end_struct_dec(_d)"));
    g.w(2, "_r");
    g.w(1, "}");
    Ok(())
}

fn enum_methods<'a>(g: &mut G<'a>, job: &'a DeriveJob, vs: &'a [crate::derive::DVariant], gt: &str) -> GR<()> {
    let (j, jt) = (g.c.j.to_string(), g.c.t.to_string());
    let name = job.name.as_str();
    if vs.is_empty() {
        return Err("an enum without variants has no JSON form".into());
    }
    // ---- is_zero: the first variant with zero fields
    let v0 = &vs[0];
    if v0.fields.is_empty() {
        g.w(1, format!("pub def json_v2_is_zero -> Bool {{ case self {{ {} => true; _ => false }} }}", v0.name));
    } else {
        let bs: Vec<String> = (0..v0.fields.len()).map(|k| format!("_z{k}")).collect();
        let mut zs = vec![];
        for (k, f) in v0.fields.iter().enumerate() {
            zs.push(g.is_zero(&f.ty, &bs[k])?);
        }
        g.w(1, format!("pub def json_v2_is_zero -> Bool {{ case self {{ {}({}) => {}; _ => false }} }}", v0.name, bs.join(", "), zs.join(" && ")));
    }
    // ---- encoding
    g.w(1, format!("pub def json_v2_enc(_e: {jt}.Encoder, _o: {jt}.Options) -> ~Unit<Error> {{"));
    hook_prelude(g, name);
    g.w(2, "case self {");
    for v in vs {
        let vn = lit(&v.opts.rename.clone().unwrap_or_else(|| v.name.clone()));
        if v.fields.is_empty() {
            g.w(3, format!("{} => {{ ~{j}.unit_variant(_e, _o, {gt}, {vn}) }}", v.name));
            continue;
        }
        let bs: Vec<String> = (0..v.fields.len()).map(|k| format!("_f{k}")).collect();
        g.w(3, format!("{}({}) => {{", v.name, bs.join(", ")));
        g.w(4, format!("_vo = ~{j}.begin_variant(_e, _o, {gt}, {vn})"));
        let named = v.fields.iter().any(|f| f.name.parse::<usize>().is_err());
        if named {
            g.w(4, format!("~_e.write_token!({jt}.Token.begin_object)"));
            g.w(4, format!("_fo = {j}.inner_opts(_vo)"));
            for (k, f) in v.fields.iter().enumerate() {
                if f.opts.skip {
                    continue;
                }
                g.w(4, format!("~{j}.write_name(_e, {})", lit(&json_name(f))));
                g.enc(&f.ty, &bs[k], "_e", "_fo", 4)?;
            }
            g.w(4, format!("~_e.write_token!({jt}.Token.end_object)"));
        } else if v.fields.len() == 1 {
            g.enc(&v.fields[0].ty, &bs[0], "_e", "_vo", 4)?;
        } else {
            g.w(4, format!("~_e.write_token!({jt}.Token.begin_array)"));
            g.w(4, format!("_ao = {j}.inner_opts(_vo)"));
            for (k, f) in v.fields.iter().enumerate() {
                g.enc(&f.ty, &bs[k], "_e", "_ao", 4)?;
            }
            g.w(4, format!("~_e.write_token!({jt}.Token.end_array)"));
        }
        g.w(4, format!("~{j}.end_struct(_e)"));
        g.w(3, "}");
    }
    g.w(2, "}");
    g.w(2, "nil");
    g.w(1, "}");
    // ---- decoding
    g.w(1, format!("pub def json_v2_dec(_d: {jt}.Decoder, _o: {jt}.Options) -> ~{name}<Error> {{"));
    unhook_prelude(g, name);
    g.w(2, format!("_vn, _wr = ~{j}.read_variant(_d, _o, {gt})"));
    g.w(2, format!("return {name}.json_zero if _vn == \"\" && !_wr"));
    g.w(2, format!("_vo = {j}.inner_opts(_o)"));
    g.w(2, "case _vn {");
    for v in vs {
        let vn = lit(&v.opts.rename.clone().unwrap_or_else(|| v.name.clone()));
        g.w(3, format!("{vn} => {{"));
        if v.fields.is_empty() {
            g.w(4, format!("fail {j}.variant_shape_error(_d, {gt}, _vn) if _wr"));
            g.w(4, format!("return {name}.{}", v.name));
            g.w(3, "}");
            continue;
        }
        g.w(4, format!("fail {j}.variant_shape_error(_d, {gt}, _vn) if !_wr"));
        let named = v.fields.iter().any(|f| f.name.parse::<usize>().is_err());
        let mut args = vec![];
        if named {
            let mut temps = vec![];
            for f in &v.fields {
                let t = g.fresh("p");
                g.w(4, format!("{t}: {} = {}", type_src(&f.ty), g.zero(&f.ty)?));
                temps.push(t);
            }
            g.w(4, format!("if ~{j}.begin_struct_dec(_d, _vo, {gt}) {{"));
            g.w(5, format!("_fo = {j}.inner_opts(_vo)"));
            g.w(5, format!("while {j}.more_members(_d) {{"));
            g.w(6, format!("_name, _raw = ~{j}.read_name(_d)"));
            g.w(6, "case _name {");
            for (f, t) in v.fields.iter().zip(&temps) {
                if f.opts.skip {
                    continue;
                }
                g.w(7, format!("{} => {{", lit(&json_name(f))));
                let x = g.fresh("v");
                g.dec(&f.ty, t, "_d", "_fo", &x, 8)?;
                g.w(8, format!("{t} = {x}"));
                g.w(8, "nil");
                g.w(7, "}");
            }
            g.w(7, format!("_ => {{ ~{j}.unknown_member(_d, _fo, {gt}, _name, _raw, false) }}"));
            g.w(6, "}");
            g.w(5, "}");
            g.w(5, format!("~{j}.end_struct_dec(_d)"));
            g.w(4, "}");
            args = temps;
        } else if v.fields.len() == 1 {
            let x = g.fresh("v");
            let z = g.zero(&v.fields[0].ty)?;
            g.dec(&v.fields[0].ty, &z, "_d", "_vo", &x, 4)?;
            args.push(x);
        } else {
            let tt = TypeExpr::Tuple(v.fields.iter().map(|f| f.ty.clone()).collect(), job.span);
            let tt: &'a TypeExpr = Box::leak(Box::new(tt));
            let x = g.fresh("v");
            let z = g.zero(tt)?;
            g.dec(tt, &z, "_d", "_vo", &x, 4)?;
            for k in 0..v.fields.len() {
                args.push(format!("{x}[{k}]"));
            }
        }
        g.w(4, format!("~{j}.end_variant(_d, {gt})"));
        g.w(4, format!("return {name}.{}({})", v.name, args.join(", ")));
        g.w(3, "}");
    }
    g.w(3, format!("_ => {{ fail {j}.unknown_variant(_d, {gt}, _vn) }}"));
    g.w(2, "}");
    g.w(1, "}");
    Ok(())
}

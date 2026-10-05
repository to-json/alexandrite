//! `#[derive(Gob)]`: compile-time generation of `encoding/gob` code
//! (GO-VS-RUBY D66, docs/notes/gob-derive.md).
//!
//! Like `#[derive(Json)]` (derive.rs) and `#[derive(Data)]`
//! (derive_data.rs), this is expansion by source text: the parser records
//! the declaration, and at the end of the module this module writes ordinary
//! `def`s that the parser reads back in as methods.
//!
//! What gets generated (`G` is the local name of the `encoding/gob` import),
//! the gob protocol that `std/encoding/gob` calls:
//!
//!   def self.gob_rtype(b: G.Types) -> Int     the Go type, described (types.alx)
//!   def gob_type(b: G.Types) -> Int           the same, through a value
//!   def gob_type_name -> Str                  Go's Register name ("main.T";
//!                                             an enum value: its variant's)
//!   def self.gob_zero -> T                    the value decoding starts from
//!   def gob_enc(e: G.Enc)                     the value (a struct: its fields
//!                                             and terminator; an enum: an
//!                                             interface value)
//!   def self.gob_compat(d, w) -> ~Bool        Go's compatibleType against wire type w
//!   def gob_dec(d: G.Dec, w: Int) -> ~T       a value of wire type w, into a copy of self
//!   def gob_top(d: G.Dec, w: Int) -> ~T       the same as a whole value (Go's
//!                                             decodeValue: the singleton and
//!                                             "no fields matched" checks)
//!
//! Mapping (Go's types): Int is int, I8..I32 int8..int32 (Rune int32), U8
//! (Byte) uint8 .. U64 uint64, Float float64, Bool, Str string, Complex
//! complex128, [Byte] []byte, [T] []T, [T; N] [N]T, Map[K, V] map[K]V, T? *T;
//! a struct is a Go struct named by `#[data("pkg.T")]` (default `main.T`)
//! with fields named by `#[field(gob: "Name")]` (default: the field's name in
//! CamelCase, `user_id` -> `UserId`; `gob: "-"` leaves it out); an enum is a
//! Go interface whose variants are the concrete types sent in it (a variant
//! with one unnamed field: that field's type; otherwise a struct named after
//! the variant, its fields CamelCase or F0, F1 ...), registered as
//! `pkg.Variant` or `#[field(gob: "name")]` on the variant.

use crate::ast::*;
use crate::derive::{type_src, DShape, DeriveJob, DField};
use crate::diag::Diag;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct GobJob {
    pub job: DeriveJob,
    /// `#[data("pkg.Name")]` before the type: its Go type name.
    pub go_name: Option<String>,
}

/// A local type, as the derive sees it.
#[derive(Clone, Debug)]
pub struct LocalType {
    pub is_enum: bool,
    pub derived: bool,
}

fn lit(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '#' => o.push_str("\\#"),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// `user_id` -> `UserId`, `x` -> `X`, `0` -> `F0`.
pub fn go_field_name(n: &str) -> String {
    if n.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return format!("F{n}");
    }
    let mut out = String::new();
    for part in n.split('_').filter(|p| !p.is_empty()) {
        let mut cs = part.chars();
        if let Some(c) = cs.next() {
            out.extend(c.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    if out.is_empty() { n.to_string() } else { out }
}

fn tag<'a>(f: &'a DField) -> Option<&'a str> {
    f.opts.tags.iter().find(|(k, _)| k == "gob").map(|(_, v)| v.as_str())
}

/// The scalar types: (Go name, wire id constant, encode with `{x}`, decode
/// expression (with `{n}`: the name for range errors), zero).
fn scalar(n: &str) -> Option<(&'static str, &'static str, &'static str, &'static str, &'static str)> {
    Some(match n {
        "Int" | "I64" => ("int", "T_INT", "_e.int!({x})", "_d.~int!", "0"),
        "I8" => ("int8", "T_INT", "_e.int!({x}.to_i64)", "_d.~i8!({n})", "0"),
        "I16" => ("int16", "T_INT", "_e.int!({x}.to_i64)", "_d.~i16!({n})", "0"),
        "I32" | "Rune" => ("int32", "T_INT", "_e.int!({x}.to_i64)", "_d.~i32!({n})", "0"),
        "U8" | "Byte" => ("uint8", "T_UINT", "_e.uint64!({x}.to_u64)", "_d.~u8!({n})", "0"),
        "U16" => ("uint16", "T_UINT", "_e.uint64!({x}.to_u64)", "_d.~u16!({n})", "0"),
        "U32" => ("uint32", "T_UINT", "_e.uint64!({x}.to_u64)", "_d.~u32!({n})", "0"),
        "U64" => ("uint64", "T_UINT", "_e.uint64!({x})", "_d.~uint!", "0"),
        "Float" | "F64" => ("float64", "T_FLOAT", "_e.float!({x})", "_d.~float!", "0.0"),
        "Bool" => ("bool", "T_BOOL", "_e.bool!({x})", "_d.~bool!", "false"),
        "Str" => ("string", "T_STRING", "_e.str!({x})", "_d.~str!", "\"\""),
        "Complex" => ("complex128", "T_COMPLEX", "_e.complex!({x})", "_d.~complex!", "Complex.new(0.0, 0.0)"),
        _ => return None,
    })
}

fn is_bytes(t: &TypeExpr) -> bool {
    matches!(t, TypeExpr::Array(e, _) if matches!(&**e, TypeExpr::Named(n, _) if n == "Byte" || n == "U8"))
}

struct G<'a> {
    a: &'a str,
    locals: &'a HashMap<String, LocalType>,
    out: String,
    n: usize,
}

type GR<T> = Result<T, String>;

impl<'a> G<'a> {
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

    /// A local enum (an interface on the wire).
    fn is_enum(&self, t: &TypeExpr) -> bool {
        matches!(t, TypeExpr::Named(n, _) if self.locals.get(n).is_some_and(|l| l.is_enum))
    }

    /// Check a field type can be encoded.
    fn check(&self, t: &TypeExpr) -> GR<()> {
        match t {
            TypeExpr::Named(n, _) => {
                if scalar(n).is_some() || n.contains('.') {
                    return Ok(());
                }
                match self.locals.get(n) {
                    Some(l) if l.derived => Ok(()),
                    Some(_) => Err(format!("field type `{n}` doesn't derive Gob; add #[derive(Gob)] to it")),
                    // another file of the package (or a hand-written protocol type): its methods say
                    None => Ok(()),
                }
            }
            TypeExpr::Array(e, _) | TypeExpr::Opt(e, _) => self.check(e),
            TypeExpr::Fixed(e, n, _) => match &n.kind {
                ExprKind::Int(_) => self.check(e),
                _ => Err("an array's length must be a literal".into()),
            },
            TypeExpr::App(n, args, _) if n == "Map" && args.len() == 2 => {
                self.check(&args[0])?;
                self.check(&args[1])
            }
            _ => Err(format!("can't encode a field of type `{}` with gob (Go has no such type)", type_src(t))),
        }
    }

    /// An expression for t's RType, with `_t` a Types variable.
    fn rt(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Named(n, _) => match scalar(n) {
                Some(s) => format!("_t.basic!({})", lit(s.0)),
                None => format!("{n}.gob_rtype(_t)"),
            },
            _ if is_bytes(t) => "_t.bytes!".into(),
            TypeExpr::Array(e, _) => format!("_t.slice!({})", self.rt(e)),
            TypeExpr::Opt(e, _) if self.is_enum(e) => self.rt(e),
            TypeExpr::Opt(e, _) => format!("_t.ptr!({})", self.rt(e)),
            TypeExpr::Fixed(e, n, _) => {
                let len = match &n.kind {
                    ExprKind::Int(v) => *v,
                    _ => 0,
                };
                format!("_t.array!({}, {len})", self.rt(e))
            }
            TypeExpr::App(_, args, _) => format!("_t.map!({}, {})", self.rt(&args[0]), self.rt(&args[1])),
            _ => "0".into(),
        }
    }

    /// The zero of t.
    fn zero(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Named(n, _) => match scalar(n) {
                Some(s) => s.4.to_string(),
                None => format!("{n}.gob_zero"),
            },
            TypeExpr::Array(..) => "[]".into(),
            TypeExpr::Opt(..) => "none".into(),
            TypeExpr::Fixed(e, n, _) => {
                let len = match &n.kind {
                    ExprKind::Int(v) => *v,
                    _ => 0,
                };
                format!("[{}; {len}]", self.zero(e))
            }
            _ => "{}".into(),
        }
    }

    /// Encode x of type t, all of it (Go's sendZero: elements, singletons).
    fn enc_all(&mut self, t: &TypeExpr, x: &str, ind: usize, nil_msg: &str) {
        match t {
            TypeExpr::Named(n, _) => match scalar(n) {
                Some(s) => self.w(ind, s.2.replace("{x}", x)),
                None => self.w(ind, format!("{x}.gob_enc(_e)")),
            },
            _ if is_bytes(t) => self.w(ind, format!("_e.bytes!({x})")),
            TypeExpr::Array(e, _) | TypeExpr::Fixed(e, _, _) => {
                self.w(ind, format!("_e.uint!({x}.size)"));
                let v = self.fresh("x");
                self.w(ind, format!("for {v} in {x} {{"));
                self.enc_all(e, &v, ind + 1, "gob: encodeArray: nil element");
                self.w(ind, "}");
            }
            TypeExpr::App(_, args, _) => {
                self.w(ind, format!("_e.uint!({x}.size)"));
                let (k, v) = (self.fresh("k"), self.fresh("v"));
                self.w(ind, format!("for {k}, {v} in {x} {{"));
                self.enc_all(&args[0], &k, ind + 1, "gob: encodeReflectValue: nil element");
                self.enc_all(&args[1], &v, ind + 1, "gob: encodeReflectValue: nil element");
                self.w(ind, "}");
            }
            TypeExpr::Opt(e, _) => {
                let v = self.fresh("o");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc_all(e, &v, ind + 1, nil_msg);
                self.w(ind, "} else {");
                if self.is_enum(e) {
                    // a nil interface
                    self.w(ind + 1, "_e.uint!(0)");
                } else {
                    self.w(ind + 1, format!("_e.fail!({})", lit(nil_msg)));
                }
                self.w(ind, "}");
            }
            _ => {}
        }
    }

    /// The field k of a struct: sent unless zero (Go leaves out zero fields).
    fn enc_field(&mut self, t: &TypeExpr, x: &str, k: usize, ind: usize) {
        let field = format!("_n = _e.field!({k}, _n)");
        match t {
            TypeExpr::Named(n, _) => match scalar(n) {
                Some(_) => {
                    let cond = match n.as_str() {
                        "Bool" => x.to_string(),
                        "Str" => format!("{x}.size > 0"),
                        "Float" | "F64" => format!("{x} != 0.0"),
                        "Complex" => format!("{x} != Complex.new(0.0, 0.0)"),
                        _ => format!("{x} != 0"),
                    };
                    self.w(ind, format!("if {cond} {{"));
                    self.w(ind + 1, field);
                    self.enc_all(t, x, ind + 1, "");
                    self.w(ind, "}");
                }
                // structs, interfaces and GobEncoders are always sent
                None => {
                    self.w(ind, field);
                    self.enc_all(t, x, ind, "");
                }
            },
            TypeExpr::Array(..) | TypeExpr::App(..) => {
                self.w(ind, format!("if {x}.size > 0 {{"));
                self.w(ind + 1, field);
                self.enc_all(t, x, ind + 1, "");
                self.w(ind, "}");
            }
            TypeExpr::Fixed(..) => {
                self.w(ind, field);
                self.enc_all(t, x, ind, "");
            }
            TypeExpr::Opt(e, _) => {
                let v = self.fresh("o");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc_field(e, &v, k, ind + 1);
                self.w(ind, "}");
            }
            _ => {}
        }
    }

    /// A Bool expression: is wire type `w` compatible with t (may use `~`).
    fn compat(&self, t: &TypeExpr, w: &str) -> String {
        match t {
            TypeExpr::Named(n, _) => match scalar(n) {
                Some(s) => format!("{w} == {}.{}", self.a, s.1),
                None if self.is_enum(t) => format!("{w} == {}.T_INTERFACE", self.a),
                None => format!("{n}.~gob_compat(_d, {w})"),
            },
            _ if is_bytes(t) => format!("{w} == {}.T_BYTES", self.a),
            TypeExpr::Array(e, _) => {
                let ew = format!("_d.elem_of({w}, {}.K_SLICE)", self.a);
                format!("({ew} >= 0 && {})", self.compat(e, &ew))
            }
            TypeExpr::Fixed(e, n, _) => {
                let len = match &n.kind {
                    ExprKind::Int(v) => *v,
                    _ => 0,
                };
                let ew = format!("_d.array_elem({w}, {len})");
                format!("({ew} >= 0 && {})", self.compat(e, &ew))
            }
            TypeExpr::App(_, args, _) => {
                let kw = format!("_d.key_of({w})");
                let ew = format!("_d.elem_of({w}, {}.K_MAP)", self.a);
                format!("({kw} >= 0 && {} && {})", self.compat(&args[0], &kw), self.compat(&args[1], &ew))
            }
            TypeExpr::Opt(e, _) => self.compat(e, w),
            _ => "false".into(),
        }
    }

    /// Decode a value of wire type `w` (compatible with t) into `dest`.
    fn dec(&mut self, t: &TypeExpr, w: &str, dest: &str, name: &str, ind: usize) {
        match t {
            TypeExpr::Named(n, _) => match scalar(n) {
                Some(s) => self.w(ind, format!("{dest} = {}", s.3.replace("{n}", &lit(name)))),
                None => self.w(ind, format!("{dest} = {dest}.~gob_dec(_d, {w})")),
            },
            _ if is_bytes(t) => self.w(ind, format!("{dest} = _d.~bytes!")),
            TypeExpr::Array(e, _) => {
                let (n, xs, i, v, ew) = (self.fresh("n"), self.fresh("xs"), self.fresh("i"), self.fresh("v"), self.fresh("w"));
                self.w(ind, format!("{ew} = _d.elem_of({w}, {}.K_SLICE)", self.a));
                self.w(ind, format!("{n} = _d.~slice_len!({})", self.go_str_lit(e)));
                self.w(ind, format!("{xs}: {} = []", type_src(t)));
                self.w(ind, format!("for {i} in 0...{n} {{"));
                self.w(ind + 1, format!("_d.~elem_check!({n})"));
                self.w(ind + 1, format!("{v}: {} = {}", type_src(e), self.zero(e)));
                self.dec(e, &ew, &v, &format!("element of {name}"), ind + 1);
                self.w(ind + 1, format!("{xs} << {v}"));
                self.w(ind, "}");
                self.w(ind, format!("{dest} = {xs}"));
            }
            TypeExpr::Fixed(e, len, _) => {
                let len = match &len.kind {
                    ExprKind::Int(v) => *v,
                    _ => 0,
                };
                let (xs, i, v, ew) = (self.fresh("xs"), self.fresh("i"), self.fresh("v"), self.fresh("w"));
                self.w(ind, format!("{ew} = _d.array_elem({w}, {len})"));
                self.w(ind, format!("_d.~array_len!({len})"));
                // [U8; N]: Go sends each element as a uint; one call reads them all
                if matches!(&**e, TypeExpr::Named(n, _) if n == "U8" || n == "Byte") {
                    self.w(ind, format!("{xs}: {} = {}", type_src(t), self.zero(t)));
                    self.w(ind, format!("copy({xs}, _d.~u8s!({len}, {}))", lit(&format!("element of {name}"))));
                    self.w(ind, format!("{dest} = {xs}"));
                    let _ = (i, v, ew);
                    return;
                }
                self.w(ind, format!("{xs}: {} = {}", type_src(t), self.zero(t)));
                self.w(ind, format!("for {i} in 0...{len} {{"));
                self.w(ind + 1, format!("_d.~elem_check!({len})"));
                // straight into the element: a fixed-array local assigned to
                // another is copied into the loop's region, which the store
                // into xs then outlives (port-issues #88)
                self.dec(e, &ew, &format!("{xs}[{i}]"), &format!("element of {name}"), ind + 1);
                let _ = v;
                self.w(ind, "}");
                self.w(ind, format!("{dest} = {xs}"));
            }
            TypeExpr::App(_, args, _) => {
                let (n, m, i, k, v, kw, ew) = (self.fresh("n"), self.fresh("m"), self.fresh("i"), self.fresh("k"), self.fresh("v"), self.fresh("w"), self.fresh("w"));
                self.w(ind, format!("{kw} = _d.key_of({w})"));
                self.w(ind, format!("{ew} = _d.elem_of({w}, {}.K_MAP)", self.a));
                self.w(ind, format!("{n} = _d.~count!"));
                self.w(ind, format!("{m} = {dest}"));
                self.w(ind, format!("for {i} in 0...{n} {{"));
                self.w(ind + 1, format!("{k}: {} = {}", type_src(&args[0]), self.zero(&args[0])));
                self.dec(&args[0], &kw, &k, name, ind + 1);
                self.w(ind + 1, format!("{v}: {} = {}", type_src(&args[1]), self.zero(&args[1])));
                self.dec(&args[1], &ew, &v, name, ind + 1);
                self.w(ind + 1, format!("{m}[{k}] = {v}"));
                self.w(ind, "}");
                self.w(ind, format!("{dest} = {m}"));
            }
            TypeExpr::Opt(e, _) if matches!(**e, TypeExpr::Fixed(..)) => {
                // a fresh array straight into dest (no fixed-array local copies, port-issues #88)
                self.dec(e, w, dest, name, ind);
            }
            TypeExpr::Opt(e, _) => {
                let (o, x) = (self.fresh("o"), self.fresh("x"));
                self.w(ind, format!("{o}: {} = {}", type_src(e), self.zero(e)));
                self.w(ind, format!("if {x} = {dest} {{"));
                self.w(ind + 1, format!("{o} = {x}"));
                self.w(ind, "}");
                if self.is_enum(e) {
                    // a nil interface is none
                    let (nm, r) = (self.fresh("name"), self.fresh("r"));
                    let en = type_src(e);
                    self.w(ind, format!("{nm} = _d.~iface_name!"));
                    self.w(ind, format!("if {nm} == \"\" {{"));
                    self.w(ind + 1, format!("{dest} = none"));
                    self.w(ind, "} else {");
                    self.w(ind + 1, format!("{r} = {o}.~gob_dec_named(_d, {nm})"));
                    self.w(ind + 1, format!("{dest} = {r}"));
                    self.w(ind, "}");
                    let _ = en;
                } else {
                    self.dec(e, w, &o, name, ind);
                    self.w(ind, format!("{dest} = {o}"));
                }
            }
            _ => {}
        }
    }

    /// A string literal or expression for t's Go String() (for messages;
    /// `_t` is the decoder's Types).
    fn go_str_lit(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Named(n, _) if scalar(n).is_some() => lit(scalar(n).unwrap().0),
            _ => format!("_t.str_of({})", self.rt(t)),
        }
    }
}

/// One struct-shaped value: the fields (wire name, type, and where it lives).
struct Shape {
    names: Vec<String>,
    tys: Vec<TypeExpr>,
    /// self.field, or a local for a variant
    dests: Vec<String>,
}

fn struct_shape(fields: &[DField], var: bool) -> Shape {
    let mut s = Shape { names: vec![], tys: vec![], dests: vec![] };
    for (k, f) in fields.iter().enumerate() {
        let name = match tag(f) {
            Some("-") => continue,
            Some(t) if !t.is_empty() => t.split(',').next().unwrap_or(t).to_string(),
            _ => go_field_name(&f.name),
        };
        s.names.push(name);
        s.tys.push(f.ty.clone());
        s.dests.push(if var { format!("_s{k}") } else { format!("_r.{}", f.name) });
    }
    s
}

pub fn source(gj: &GobJob, alias: &str, locals: &HashMap<String, LocalType>) -> Result<String, Diag> {
    let job = &gj.job;
    let fail = |m: String| Diag::new(job.span, format!("derive(Gob) on `{}`: {m}", job.name));
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Gob yet; derive it on a concrete wrapper".to_string()));
    }
    let a = alias;
    let name = &job.name;
    let go_name = gj.go_name.clone().unwrap_or_else(|| format!("main.{name}"));
    let qual = match go_name.rfind('.') {
        Some(i) => go_name[..i].to_string(),
        None => "main".to_string(),
    };
    let mut g = G { a, locals, out: String::new(), n: 0 };
    match &job.shape {
        DShape::Struct(fs) => {
            for f in fs {
                if tag(f) != Some("-") {
                    g.check(&f.ty).map_err(&fail)?;
                }
            }
        }
        DShape::Enum(vs) => {
            for v in vs {
                for f in &v.fields {
                    g.check(&f.ty).map_err(&fail)?;
                }
            }
        }
    }
    g.w(0, format!("struct {name} {{"));
    g.w(1, format!("def gob_type(_b: {a}.Types) -> Int {{ {name}.gob_rtype(_b) }}"));
    match &job.shape {
        DShape::Struct(fs) => {
            let sh = struct_shape(fs, false);
            let sent_none = !fs.is_empty() && sh.names.is_empty();
            // gob_rtype
            g.w(1, format!("def self.gob_rtype(_b: {a}.Types) -> Int {{"));
            g.w(2, "_t = _b");
            g.w(2, format!("if _r = _t.find({}) {{", lit(&go_name)));
            g.w(3, "return _r");
            g.w(2, "}");
            let rts: Vec<String> = sh.tys.iter().map(|t| g.rt(t)).collect();
            let names: Vec<String> = sh.names.iter().map(|n| lit(n)).collect();
            g.w(2, format!("_fn: [Str] = [{}]", names.join(", ")));
            g.w(2, format!("_ft: [Int] = [{}]", rts.join(", ")));
            g.w(2, format!("_t.strct!({}, _fn, _ft)", lit(&go_name)));
            g.w(1, "}");
            g.w(1, format!("def gob_type_name -> Str {{ {} }}", lit(&go_name)));
            g.w(1, format!("def self.gob_zero -> {name} {{ {name}.new }}"));
            // gob_enc
            g.w(1, format!("def gob_enc(_e0: {a}.Encoder) {{").replace(".Encoder)", ".Enc)"));
            g.w(2, "_e = _e0");
            if sent_none {
                g.w(2, format!("_e.fail!({})", lit(&format!("gob: type {go_name} has no exported fields"))));
            }
            g.w(2, "_n = -1");
            let mut k = 0;
            for f in fs {
                if tag(f) == Some("-") {
                    continue;
                }
                g.enc_field(&f.ty, &format!("self.{}", f.name), k, 2);
                k += 1;
            }
            g.w(2, "_e.uint!(0)");
            g.w(2, "nil");
            g.w(1, "}");
            gen_struct_dec(&mut g, name, &go_name, &sh, None, fs.len());
        }
        DShape::Enum(vs) => {
            g.w(1, format!("def self.gob_rtype(_b: {a}.Types) -> Int {{"));
            g.w(2, "_t = _b");
            g.w(2, format!("_t.iface!({})", lit(&go_name)));
            g.w(1, "}");
            // variant names
            let vnames: Vec<String> = vs.iter().map(|v| v.opts.tags.iter().find(|(k, _)| k == "gob").map(|(_, t)| t.clone()).unwrap_or_else(|| format!("{qual}.{}", v.name))).collect();
            g.w(1, "def gob_type_name -> Str {");
            g.w(2, "case self {");
            for (v, vn) in vs.iter().zip(&vnames) {
                let pat = if v.fields.is_empty() { v.name.clone() } else { format!("{}({})", v.name, v.fields.iter().map(|_| "_").collect::<Vec<_>>().join(", ")) };
                g.w(3, format!("{pat} => {}", lit(vn)));
            }
            g.w(2, "}");
            g.w(1, "}");
            // zero: the first variant, zero fields
            let v0 = &vs[0];
            let z = if v0.fields.is_empty() {
                format!("{name}.{}", v0.name)
            } else if v0.fields.iter().all(|f| f.name.chars().next().is_some_and(|c| c.is_ascii_digit())) {
                format!("{name}.{}({})", v0.name, v0.fields.iter().map(|f| g.zero(&f.ty)).collect::<Vec<_>>().join(", "))
            } else {
                format!("{name}.{}({})", v0.name, v0.fields.iter().map(|f| format!("{}: {}", f.name, g.zero(&f.ty))).collect::<Vec<_>>().join(", "))
            };
            g.w(1, format!("def self.gob_zero -> {name} {{ {z} }}"));
            // the concrete types
            for (i, v) in vs.iter().enumerate() {
                if v.fields.len() == 1 && v.fields[0].name == "0" {
                    continue;
                }
                let sh = struct_shape(&v.fields, true);
                let vgo = format!("{qual}.{}", v.name);
                g.w(1, format!("def self.gob_vrt{i}(_b: {a}.Types) -> Int {{"));
                g.w(2, "_t = _b");
                g.w(2, format!("if _r = _t.find({}) {{", lit(&vgo)));
                g.w(3, "return _r");
                g.w(2, "}");
                let rts: Vec<String> = sh.tys.iter().map(|t| g.rt(t)).collect();
                let names: Vec<String> = sh.names.iter().map(|n| lit(n)).collect();
                g.w(2, format!("_fn: [Str] = [{}]", names.join(", ")));
                g.w(2, format!("_ft: [Int] = [{}]", rts.join(", ")));
                g.w(2, format!("_t.strct!({}, _fn, _ft)", lit(&vgo)));
                g.w(1, "}");
            }
            // gob_enc: an interface value
            g.w(1, format!("def gob_enc(_e0: {a}.Enc) {{"));
            g.w(2, "_e = _e0");
            g.w(2, "_t = _e.types");
            g.w(2, "case self {");
            for (i, v) in vs.iter().enumerate() {
                let vals: Vec<String> = (0..v.fields.len()).map(|k| format!("_f{k}")).collect();
                let pat = if v.fields.is_empty() { v.name.clone() } else { format!("{}({})", v.name, vals.join(", ")) };
                g.w(3, format!("{pat} => {{"));
                g.w(4, format!("_e.str!({a}.name_of({}))", lit(&vnames[i])));
                if v.fields.len() == 1 && v.fields[0].name == "0" {
                    let t = v.fields[0].ty.clone();
                    let rt = g.rt(&t);
                    g.w(4, format!("_rt = {rt}"));
                    g.w(4, "_e.iface_begin!(_rt)");
                    g.w(4, "_e.uint!(0) unless _e.struct?(_rt)");
                    g.enc_all(&t, "_f0", 4, "gob: cannot encode nil pointer inside interface");
                } else {
                    g.w(4, format!("_e.iface_begin!({name}.gob_vrt{i}(_t))"));
                    g.w(4, "_n = -1");
                    let mut k = 0;
                    for (j, f) in v.fields.iter().enumerate() {
                        if tag(f) == Some("-") {
                            continue;
                        }
                        g.enc_field(&f.ty, &format!("_f{j}"), k, 4);
                        k += 1;
                    }
                    g.w(4, "_e.uint!(0)");
                }
                g.w(4, "_e.iface_end!");
                g.w(4, "nil");
                g.w(3, "}");
            }
            g.w(2, "}");
            g.w(1, "}");
            // compat, top, dec
            g.w(1, format!("def self.gob_compat(_d: {a}.Dec, _w: Int) -> ~Bool {{ _w == {a}.T_INTERFACE }}"));
            g.w(1, format!("def gob_top(_d0: {a}.Dec, _wid: Int) -> ~{name} {{"));
            g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
            g.w(2, format!("fail _d.mismatch({}, _wid, true) if _wid != {a}.T_INTERFACE", lit(&go_name)));
            g.w(2, "_d.~single!");
            g.w(2, "self.~gob_dec(_d, _wid)");
            g.w(1, "}");
            g.w(1, format!("def gob_dec(_d0: {a}.Dec, _wid: Int) -> ~{name} {{"));
            g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
            g.w(2, "_name = _d.~iface_name!");
            g.w(2, format!("return {name}.gob_zero if _name == \"\""));
            g.w(2, "self.~gob_dec_named(_d, _name)");
            g.w(1, "}");
            // after the name
            g.w(1, format!("def gob_dec_named(_d0: {a}.Dec, _name: Str) -> ~{name} {{"));
            g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
            g.w(2, format!("_ty = {a}.type_of(_name)"));
            for (i, vn) in vnames.iter().enumerate() {
                g.w(2, format!("if _ty == {} {{", lit(vn)));
                g.w(3, "_cid = _d.~iface_value_id!");
                g.w(3, format!("return {name}.~gob_vdec{i}(_d, _cid)"));
                g.w(2, "}");
            }
            g.w(2, "fail _d.not_registered(_name)");
            g.w(1, "}");
            // each variant's concrete value (Go's decodeValue)
            for (i, v) in vs.iter().enumerate() {
                if v.fields.len() == 1 && v.fields[0].name == "0" {
                    let t = v.fields[0].ty.clone();
                    g.w(1, format!("def self.gob_vdec{i}(_d0: {a}.Dec, _cid: Int) -> ~{name} {{"));
                    g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
                    g.w(2, format!("_v: {} = {}", type_src(&t), g.zero(&t)));
                    match &t {
                        TypeExpr::Named(n, _) if scalar(n).is_none() && !g.is_enum(&t) => {
                            g.w(2, "_v = _v.~gob_top(_d, _cid)");
                        }
                        _ => {
                            let c = g.compat(&t, "_cid");
                            g.w(2, format!("_ok = {c}"));
                            g.w(2, format!("fail _d.mismatch(_t.str_of({}), _cid, false) unless _ok", g.rt(&t)));
                            g.w(2, "_d.~single!");
                            g.dec(&t, "_cid", "_v", &vnames[i], 2);
                        }
                    }
                    g.w(2, format!("{name}.{}(_v)", v.name));
                    g.w(1, "}");
                } else {
                    let sh = struct_shape(&v.fields, true);
                    let vgo = format!("{qual}.{}", v.name);
                    let ctor = if v.fields.is_empty() {
                        format!("{name}.{}", v.name)
                    } else if v.fields.iter().all(|f| f.name.chars().next().is_some_and(|c| c.is_ascii_digit())) {
                        format!("{name}.{}({})", v.name, (0..v.fields.len()).map(|k| format!("_s{k}")).collect::<Vec<_>>().join(", "))
                    } else {
                        format!("{name}.{}({})", v.name, v.fields.iter().enumerate().map(|(k, f)| format!("{}: _s{k}", f.name)).collect::<Vec<_>>().join(", "))
                    };
                    let inits: Vec<(String, String, String)> = v.fields.iter().enumerate().map(|(k, f)| (format!("_s{k}"), type_src(&f.ty), g.zero(&f.ty))).collect();
                    gen_struct_dec(&mut g, name, &vgo, &sh, Some((i, ctor, inits)), v.fields.len());
                }
            }
        }
    }
    g.w(0, "}");
    Ok(g.out)
}

/// The decoding defs of a struct (or of an enum variant's struct: `var` is
/// (index, constructor, locals)).
fn gen_struct_dec(g: &mut G, name: &str, go: &str, sh: &Shape, var: Option<(usize, String, Vec<(String, String, String)>)>, nfields: usize) {
    let a = g.a.to_string();
    let sfx = match &var {
        Some((i, _, _)) => format!("{i}"),
        None => String::new(),
    };
    // the plan: for each wire field, the local one (or -1); Go's compileDec
    g.w(1, format!("def self.gob_plan{sfx}(_d0: {a}.Dec, _wid: Int) -> ~[Int] {{"));
    g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
    g.w(2, format!("if _c = _d.plan({}, _wid) {{", lit(go)));
    g.w(3, "return _c");
    g.w(2, "}");
    g.w(2, format!("_ws = _d.~wire_struct(_wid, {})", lit(go)));
    g.w(2, "_p: [Int] = []");
    g.w(2, "for _k in 0..._ws.fnames.size {");
    g.w(3, "_fname = _ws.fnames[_k]");
    g.w(3, "_f = _ws.fids[_k]");
    g.w(3, format!("fail {a}.gerr(\"gob: empty name for remote field of type \" + _ws.name) if _fname == \"\""));
    g.w(3, "_i = -1");
    for (k, n) in sh.names.iter().enumerate() {
        g.w(3, format!("if _fname == {} {{", lit(n)));
        g.w(4, format!("_i = {k}"));
        let c = g.compat(&sh.tys[k], "_f");
        g.w(4, format!("_ok = {c}"));
        let t = sh.tys[k].clone();
        g.w(4, format!("fail _d.wrong_type(_t.str_of({}), _wid, {}) unless _ok", g.rt(&t), lit(n)));
        g.w(3, "}");
    }
    g.w(3, "_d.~ignorable!(_f) if _i < 0");
    g.w(3, "_p << _i");
    g.w(2, "}");
    g.w(2, format!("_d.store_plan!({}, _wid, _p)", lit(go)));
    g.w(2, "_p");
    g.w(1, "}");
    // the fields
    let (head, ret) = match &var {
        None => (format!("def gob_dec(_d0: {a}.Dec, _wid: Int) -> ~{name} {{"), "_r".to_string()),
        Some((i, ctor, _)) => (format!("def self.gob_vbody{i}(_d0: {a}.Dec, _wid: Int) -> ~{name} {{"), ctor.clone()),
    };
    g.w(1, head);
    g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
    g.w(2, format!("_p = {name}.~gob_plan{sfx}(_d, _wid)"));
    match &var {
        None => g.w(2, "_r = self"),
        Some((_, _, inits)) => {
            for (v, t, z) in inits {
                g.w(2, format!("{v}: {t} = {z}"));
            }
        }
    }
    g.w(2, "_fnum = -1");
    g.w(2, "loop {");
    g.w(3, "_fnum = _d.~next_field!(_fnum, _p.size)");
    g.w(3, "break if _fnum < 0");
    g.w(3, "_w = _d.field_id(_wid, _fnum)");
    g.w(3, "_q = _p[_fnum]");
    for (k, n) in sh.names.iter().enumerate() {
        g.w(3, format!("{}if _q == {k} {{", if k == 0 { "" } else { "} els" }));
        let t = sh.tys[k].clone();
        let d = sh.dests[k].clone();
        g.dec(&t, "_w", &d, n, 4);
    }
    if sh.names.is_empty() {
        g.w(3, "_d.~skip!(_w)");
    } else {
        g.w(3, "} else {");
        g.w(4, "_d.~skip!(_w)");
        g.w(3, "}");
    }
    g.w(2, "}");
    g.w(2, ret);
    g.w(1, "}");
    // decodeValue: the whole value
    let (thead, call) = match &var {
        None => (format!("def gob_top(_d0: {a}.Dec, _wid: Int) -> ~{name} {{"), "self.~gob_dec(_d, _wid)".to_string()),
        Some((i, _, _)) => (format!("def self.gob_vdec{i}(_d0: {a}.Dec, _wid: Int) -> ~{name} {{"), format!("{name}.~gob_vbody{i}(_d, _wid)")),
    };
    g.w(1, thead);
    g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
    g.w(2, format!("_p = {name}.~gob_plan{sfx}(_d, _wid)"));
    if nfields > 0 {
        g.w(2, format!("fail {a}.gerr({}) if _wid >= 64 && _p.size > 0 && _p.all? {{ it < 0 }}", lit(&format!("gob: type mismatch: no fields matched compiling decoder for {}", crate::derive_gob::short(go)))));
    }
    g.w(2, call);
    g.w(1, "}");
    if var.is_none() {
        g.w(1, format!("def self.gob_compat(_d0: {a}.Dec, _w: Int) -> ~Bool {{"));
        g.w(2, "_d = _d0");
    g.w(2, "_t = _d.types");
        g.w(2, "return false if _d.ext_kind(_w) != 0");
        g.w(2, format!("{name}.~gob_plan(_d, _w)"));
        g.w(2, "true");
        g.w(1, "}");
    }
}

pub fn short(s: &str) -> &str {
    match s.rfind('.') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

//! `#[derive(Asn1)]`: compile-time generation of `encoding/asn1` code (D65).
//!
//! The same mechanism as `#[derive(Json)]` (derive.rs): the parser records the
//! struct and its fields' `#[asn1(...)]` options, and at the end of the module
//! this file writes ordinary Alexandrite defs as text, which the parser reads
//! back in as methods of the type. `A` is the local name of `encoding/asn1`:
//!
//!   def asn1_enc(p: A.Params) -> ~[Byte]<A.Asn1Error>      the element (Go's makeField)
//!   def self.asn1_dec(b, off, p, depth) -> ~(T, Int)<..>   one element (Go's parseField)
//!   def self.asn1_utag -> Int                             SEQUENCE, or SET for set types
//!   def to_asn1 / to_asn1_with_params(s)                   Go's Marshal / MarshalWithParams
//!   def self.from_asn1(b) / from_asn1_with_params(b, s)    Go's Unmarshal (value, rest)
//!
//! The options are Go's struct tag options (`optional`, `explicit`, `tag: N`,
//! `default: N`, `set`, `utf8`, ...) plus `enumerated`, `flag`, `raw_content`
//! and `skip` for what Go expresses with the types Enumerated, Flag and
//! RawContent. Field codecs are the typed `enc_*` / `dec_*` functions of the
//! package, called with a `Params` literal the derive spells from the options,
//! so the per-field work Go does with reflection at run time is decided here.

use crate::ast::*;
use crate::derive::{type_src, DField, DShape, DeriveJob, ModuleTypes};
use crate::diag::{Diag, Span};
use std::collections::HashMap;

/// Options from `#[asn1(...)]` on a field, or on the type.
#[derive(Default, Clone, Debug)]
pub struct A1 {
    pub optional: bool,
    pub explicit: bool,
    pub application: bool,
    pub private: bool,
    pub default: Option<i64>,
    pub tag: Option<i64>,
    pub string_type: i64,
    pub time_type: i64,
    pub set: bool,
    pub omit_empty: bool,
    pub enumerated: bool,
    pub flag: bool,
    pub raw_content: bool,
    pub skip: bool,
    pub transparent: bool,
}

/// Parse the text of one `#[asn1(...)]` attribute into `o`.
pub fn apply_asn1_attr(text: &str, sp: Span, o: &mut A1) -> Result<(), Diag> {
    let inner = text.trim().strip_prefix("asn1").map(str::trim_start).and_then(|r| r.strip_prefix('(')).and_then(|r| r.trim_end().strip_suffix(')'));
    let Some(inner) = inner else {
        return Err(Diag::new(sp, format!("unknown attribute `#[{text}]`")).note("write #[asn1(optional, explicit, tag: 0)]"));
    };
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let num = |s: &str| -> Result<i64, Diag> { s.trim().parse::<i64>().map_err(|_| Diag::new(sp, format!("`{part}`: expected an integer"))) };
        if let Some(v) = part.strip_prefix("tag").map(str::trim_start).and_then(|r| r.strip_prefix(':')) {
            let n = num(v)?;
            if n < 0 {
                return Err(Diag::new(sp, "an ASN.1 tag can't be negative"));
            }
            o.tag = Some(n);
            continue;
        }
        if let Some(v) = part.strip_prefix("default").map(str::trim_start).and_then(|r| r.strip_prefix(':')) {
            o.default = Some(num(v)?);
            continue;
        }
        match part {
            "optional" => o.optional = true,
            "explicit" => {
                o.explicit = true;
                o.tag.get_or_insert(0);
            }
            "application" => {
                o.application = true;
                o.tag.get_or_insert(0);
            }
            "private" => {
                o.private = true;
                o.tag.get_or_insert(0);
            }
            "generalized" => o.time_type = 24,
            "utc" => o.time_type = 23,
            "ia5" => o.string_type = 22,
            "printable" => o.string_type = 19,
            "numeric" => o.string_type = 18,
            "utf8" => o.string_type = 12,
            "set" => o.set = true,
            "omit_empty" | "omitempty" => o.omit_empty = true,
            "enumerated" => o.enumerated = true,
            "flag" => o.flag = true,
            "raw_content" => o.raw_content = true,
            "skip" => o.skip = true,
            "transparent" => o.transparent = true,
            _ => {
                return Err(Diag::new(sp, format!("unknown asn1 option `{part}`")).note("options: optional, explicit, application, private, tag: N, default: N, set, omit_empty, utf8, ia5, printable, numeric, utc, generalized, enumerated, flag, raw_content, skip; on a type: set, transparent"));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Ty<'a> {
    Bool,
    Int(IntKind),
    Str,
    Bytes,
    Big,
    Time,
    BitString,
    Oid,
    Raw,
    Any,
    Named(&'a str),
    List(&'a TypeExpr),
    Opt(&'a TypeExpr),
}

struct Gen<'a> {
    a: &'a str,
    imports: &'a HashMap<String, String>,
    types: &'a ModuleTypes,
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
                if let Some((pkg, name)) = n.split_once('.') {
                    match (self.imports.get(pkg).map(String::as_str), name) {
                        (Some("math/big"), "Int") => Ty::Big,
                        (Some("time"), "Time") => Ty::Time,
                        (Some("encoding/asn1"), "BitString") => Ty::BitString,
                        (Some("encoding/asn1"), "ObjectIdentifier") => Ty::Oid,
                        (Some("encoding/asn1"), "RawValue") => Ty::Raw,
                        (Some("encoding/asn1"), "Any") => Ty::Any,
                        (Some("math/big"), _) | (Some("time"), _) => return Err(format!("`{n}` has no ASN.1 encoding")),
                        _ => Ty::Named(n),
                    }
                } else if let Some(k) = IntKind::from_name(n) {
                    if !k.signed() {
                        return Err(format!("`{n}`: unsigned integers have no ASN.1 encoding in Go's encoding/asn1; use Int"));
                    }
                    Ty::Int(k)
                } else if n == "Bool" {
                    Ty::Bool
                } else if n == "Str" {
                    Ty::Str
                } else if let Some(false) = self.types.local.get(n.as_str()) {
                    return Err(format!("`{n}` doesn't derive Asn1: add `#[derive(Asn1)]` to it"));
                } else if self.types.enums.contains(n.as_str()) {
                    return Err(format!("`{n}` is an enum: derive(Asn1) handles structs only"));
                } else if matches!(n.as_str(), "Float" | "F64" | "F32" | "Ptr" | "Unit") {
                    return Err(format!("{n} has no ASN.1 encoding"));
                } else {
                    Ty::Named(n)
                }
            }
            TypeExpr::Array(e, _) => match &**e {
                TypeExpr::Named(n, _) if n == "Byte" || n == "U8" => Ty::Bytes,
                _ => Ty::List(e),
            },
            TypeExpr::Opt(e, _) => Ty::Opt(e),
            TypeExpr::App(n, ..) => return Err(format!("`{n}[...]` has no ASN.1 encoding")),
            TypeExpr::Tuple(..) => return Err("a tuple has no ASN.1 encoding; use a struct".to_string()),
            TypeExpr::Fixed(..) => return Err("fixed-size arrays `[T; N]` aren't supported by derive(Asn1); use a slice `[T]`".to_string()),
            TypeExpr::Result(..) => return Err("a fallible type has no ASN.1 encoding".to_string()),
            TypeExpr::Handle(..) => return Err("a pool handle `@T` has no ASN.1 encoding".to_string()),
            TypeExpr::Fn(..) => return Err("a function type has no ASN.1 encoding".to_string()),
        })
    }

    /// The `Params` literal for options `o`.
    fn params(&self, o: &A1) -> String {
        let mut ps = vec![];
        let b = |ps: &mut Vec<String>, c: bool, n: &str| {
            if c {
                ps.push(format!("{n}: true"));
            }
        };
        b(&mut ps, o.optional, "optional");
        b(&mut ps, o.explicit, "explicit");
        b(&mut ps, o.application, "application");
        b(&mut ps, o.private, "private");
        if let Some(d) = o.default {
            ps.push(format!("default_value: {d}"));
        }
        if let Some(t) = o.tag {
            ps.push(format!("tag: {t}"));
        }
        if o.string_type != 0 {
            ps.push(format!("string_type: {}", o.string_type));
        }
        if o.time_type != 0 {
            ps.push(format!("time_type: {}", o.time_type));
        }
        b(&mut ps, o.set, "set");
        b(&mut ps, o.omit_empty, "omit_empty");
        if ps.is_empty() {
            format!("{}.Params.new", self.a)
        } else {
            format!("{}.Params.new({})", self.a, ps.join(", "))
        }
    }

    /// The kind `check_seq_of` expects for elements of type `t`.
    fn kind(&self, t: &'a TypeExpr) -> GResult<String> {
        let a = self.a;
        Ok(match self.classify(t)? {
            Ty::Bool => format!("{a}.K_BOOL"),
            Ty::Int(_) | Ty::Big => format!("{a}.K_INT"),
            Ty::Str => format!("{a}.K_STR"),
            Ty::Bytes => format!("{a}.K_BYTES"),
            Ty::Time => format!("{a}.K_TIME"),
            Ty::BitString => format!("{a}.K_BIT_STRING"),
            Ty::Oid => format!("{a}.K_OID"),
            Ty::Raw | Ty::Any => format!("{a}.K_RAW"),
            Ty::Named(n) => format!("{n}.asn1_utag"),
            Ty::List(_) => format!("{a}.K_SEQUENCE"),
            Ty::Opt(_) => return Err("a slice of optionals `[T?]` has no ASN.1 encoding".to_string()),
        })
    }

    /// An expression (after statements written at `ind`) for the element of
    /// `x`, of type `t`, with params `p` (a Params expression) and options `o`.
    fn enc(&mut self, t: &'a TypeExpr, x: &str, p: &str, o: &A1, ind: usize) -> GResult<String> {
        let a = self.a.to_string();
        Ok(match self.classify(t)? {
            Ty::Bool if o.flag => format!("{a}.~enc_flag({x}, {p})"),
            Ty::Bool => format!("{a}.~enc_bool({x}, {p})"),
            Ty::Int(_) if o.enumerated => format!("{a}.~enc_enumerated({x}.to_i, {p})"),
            Ty::Int(IntKind::I64) => format!("{a}.~enc_int({x}, {p})"),
            Ty::Int(_) => format!("{a}.~enc_int({x}.to_i, {p})"),
            Ty::Str => format!("{a}.~enc_str({x}, {p})"),
            Ty::Bytes => format!("{a}.~enc_bytes({x}, {p})"),
            Ty::Big => format!("{a}.~enc_big({x}, {p})"),
            Ty::Time => format!("{a}.~enc_time({x}, {p})"),
            Ty::BitString => format!("{a}.~enc_bit_string({x}, {p})"),
            Ty::Oid => format!("{a}.~enc_oid({x}, {p})"),
            Ty::Raw => format!("{a}.~enc_raw({x}, {p})"),
            Ty::Any => format!("{a}.~enc_any({x}, {p})"),
            Ty::Named(_) => format!("{x}.~asn1_enc({p})"),
            Ty::List(e) => {
                let (es, v) = (self.fresh("es"), self.fresh("x"));
                self.w(ind, format!("{es}: [[Byte]] = []"));
                self.w(ind, format!("for {v} in {x} {{"));
                let np = format!("{a}.Params.new");
                let inner = self.enc(e, &v, &np, &A1::default(), ind + 1)?;
                self.w(ind + 1, format!("{es} << {inner}"));
                self.w(ind, "}");
                format!("{a}.~enc_list({es}, false, {p})")
            }
            Ty::Opt(_) => return Err("nested optionals have no ASN.1 encoding".to_string()),
        })
    }

    /// A test that is true when `x` (of type `t`) is not its zero value, for
    /// optional fields (Go omits an optional zero value), or None if the
    /// value is never omitted.
    fn nonzero(&self, t: &'a TypeExpr, x: &str) -> GResult<Option<String>> {
        let a = self.a;
        Ok(match self.classify(t)? {
            Ty::Bool => Some(x.to_string()),
            Ty::Int(_) => Some(format!("{x} != 0")),
            Ty::Str | Ty::Bytes | Ty::List(_) => Some(format!("{x}.size != 0")),
            Ty::Big => None,
            Ty::Time => Some(format!("!{x}.is_zero")),
            Ty::BitString => Some(format!("!{a}.zero_bit_string?({x})")),
            Ty::Oid => Some(format!("{x}.arcs.size != 0")),
            Ty::Raw => Some(format!("!{a}.zero_raw?({x})")),
            Ty::Any => Some(format!("!{a}.zero_any?({x})")),
            Ty::Named(n) => Some(format!("!({x} == {n}.new)")),
            Ty::Opt(_) => None,
        })
    }

    /// Statements decoding a value of type `t` from `src` at the offset in
    /// the variable `off` (updated) into the new local `dest`.
    fn dec(&mut self, t: &'a TypeExpr, dest: &str, src: &str, off: &str, p: &str, o: &A1, depth: &str, ind: usize) -> GResult<()> {
        let a = self.a.to_string();
        let call = |f: &str| format!("{dest}, {off} = {a}.~{f}({src}, {off}, {p}, {depth})");
        match self.classify(t)? {
            Ty::Bool if o.flag => self.w(ind, call("dec_flag")),
            Ty::Bool => self.w(ind, call("dec_bool")),
            Ty::Int(k) => {
                let tmp = self.fresh("i");
                let c = |f: &str| format!("{tmp}, {off} = {a}.~{f}({src}, {off}, {p}, {depth})");
                if o.enumerated {
                    self.w(ind, c("dec_enumerated"));
                } else {
                    match k {
                        IntKind::I64 => self.w(ind, c("dec_int")),
                        IntKind::I32 => self.w(ind, c("dec_int32")),
                        k => self.w(ind, format!("{tmp}, {off} = {a}.~dec_int_range({src}, {off}, {p}, {depth}, {}, {})", k.min(), k.max())),
                    }
                }
                if k == IntKind::I64 {
                    self.w(ind, format!("{dest} = {tmp}"));
                } else {
                    self.w(ind, format!("{dest} = {tmp}.as_{}", k.method()));
                }
            }
            Ty::Str => self.w(ind, call("dec_str")),
            Ty::Bytes => self.w(ind, call("dec_bytes")),
            Ty::Big => self.w(ind, call("dec_big")),
            Ty::Time => self.w(ind, call("dec_time")),
            Ty::BitString => self.w(ind, call("dec_bit_string")),
            Ty::Oid => self.w(ind, call("dec_oid")),
            Ty::Raw => self.w(ind, call("dec_raw")),
            Ty::Any => self.w(ind, call("dec_any")),
            Ty::Named(n) => self.w(ind, format!("{dest}, {off} = {n}.~asn1_dec({src}, {off}, {p}, {depth})")),
            Ty::List(e) => {
                let kind = self.kind(e)?;
                let (h, s, q, x) = (self.fresh("h"), self.fresh("s"), self.fresh("q"), self.fresh("e"));
                self.w(ind, format!("{h} = {a}.~head({src}, {off}, {p}, {a}.K_SEQUENCE, \"\", {depth})"));
                self.w(ind, format!("{off} = {h}.next"));
                self.w(ind, format!("{dest}: {} = []", type_src(t)));
                self.w(ind, format!("if {h}.present {{"));
                self.w(ind + 1, format!("{s} = {src}[{h}.body...{h}.end]"));
                self.w(ind + 1, format!("{a}.~check_seq_of({s}, {kind})"));
                self.w(ind + 1, format!("{q} = 0"));
                self.w(ind + 1, format!("while {q} < {s}.size {{"));
                let np = format!("{a}.Params.new");
                let hd = format!("{h}.depth");
                self.dec(e, &x, &s, &q, &np, &A1::default(), &hd, ind + 2)?;
                self.w(ind + 2, format!("{dest} << {x}"));
                self.w(ind + 1, "}");
                self.w(ind, "}");
            }
            Ty::Opt(_) => return Err("nested optionals have no ASN.1 encoding".to_string()),
        }
        Ok(())
    }

    /// The encoding of one field into the body `_b`.
    fn enc_field(&mut self, f: &'a DField, x: &str, ind: usize) -> GResult<()> {
        let o = &f.opts.asn1;
        let p = self.params(o);
        let mut conds: Vec<String> = vec![];
        let ty = self.classify(&f.ty)?;
        if o.omit_empty && matches!(ty, Ty::Bytes | Ty::List(_)) {
            conds.push(format!("{x}.size != 0"));
        }
        if o.optional {
            if let (Some(d), Ty::Int(_)) = (o.default, ty) {
                conds.push(format!("{x} != {d}"));
            } else if let Some(c) = self.nonzero(&f.ty, x)? {
                conds.push(c);
            }
        }
        let mut ind2 = ind;
        if !conds.is_empty() {
            self.w(ind, format!("if {} {{", conds.join(" && ")));
            ind2 += 1;
        }
        if let Ty::Opt(inner) = ty {
            let v = self.fresh("v");
            self.w(ind2, format!("if {v} = {x} {{"));
            let e = self.enc(inner, &v, &p, o, ind2 + 1)?;
            self.w(ind2 + 1, format!("_b = _b + {e}"));
            self.w(ind2, "}");
        } else {
            self.check_opts(&f.ty, o)?;
            let e = self.enc(&f.ty, x, &p, o, ind2)?;
            self.w(ind2, format!("_b = _b + {e}"));
        }
        if !conds.is_empty() {
            self.w(ind, "}");
        }
        Ok(())
    }

    fn check_opts(&self, t: &'a TypeExpr, o: &A1) -> GResult<()> {
        let ty = self.classify(t)?;
        if o.flag && !matches!(ty, Ty::Bool) {
            return Err("`flag` is for Bool fields".to_string());
        }
        if o.enumerated && !matches!(ty, Ty::Int(_)) {
            return Err("`enumerated` is for integer fields".to_string());
        }
        Ok(())
    }
}

/// The source text of `struct Name { defs }` holding the derived methods.
pub fn asn1_source(job: &DeriveJob, alias: &str, imports: &HashMap<String, String>, types: &ModuleTypes) -> Result<String, Diag> {
    let name = job.name.clone();
    let fail = |m: String| Diag::new(job.span, format!("derive(Asn1) on `{name}`: {m}"));
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Asn1".to_string()));
    }
    let DShape::Struct(fields) = &job.shape else {
        return Err(fail("derive(Asn1) handles structs only (Go's encoding/asn1 has no CHOICE)".to_string()));
    };
    let mut g = Gen { a: alias, imports, types, out: String::new(), n: 0 };
    let a = alias.to_string();
    let to = &job.type_opts;
    let wrapper = to.set || to.transparent;
    let live: Vec<&DField> = fields.iter().filter(|f| !f.opts.asn1.skip).collect();
    for (k, f) in live.iter().enumerate() {
        if f.opts.asn1.raw_content {
            if k != 0 || !matches!(&f.ty, TypeExpr::Array(e, _) if matches!(&**e, TypeExpr::Named(n, _) if n == "Byte" || n == "U8")) {
                return Err(fail(format!("`raw_content` goes on the first field, a [Byte] (field `{}`)", f.name)));
            }
        }
    }
    if wrapper && (live.len() != 1 || !matches!(&live[0].ty, TypeExpr::Array(..))) {
        return Err(fail("a `set` or `transparent` type has exactly one field, a slice".to_string()));
    }
    g.w(0, format!("struct {name} {{"));
    // ---- the universal tag
    g.w(1, "pub def self.asn1_utag -> Int {");
    g.w(2, if to.set { format!("{a}.TAG_SET") } else { format!("{a}.TAG_SEQUENCE") });
    g.w(1, "}");
    // ---- encoding
    g.w(1, format!("pub def asn1_enc(_p: {a}.Params) -> ~[Byte]<{a}.Asn1Error> {{"));
    if wrapper {
        let f = live[0];
        let TypeExpr::Array(e, _) = &f.ty else { unreachable!() };
        let (es, v) = (g.fresh("es"), g.fresh("x"));
        g.w(2, format!("{es}: [[Byte]] = []"));
        g.w(2, format!("for {v} in self.{} {{", f.name));
        let np = format!("{a}.Params.new");
        let inner = g.enc(e, &v, &np, &A1::default(), 3).map_err(&fail)?;
        g.w(3, format!("{es} << {inner}"));
        g.w(2, "}");
        g.w(2, format!("{a}.~enc_list({es}, {}, _p)", to.set));
    } else {
        let mut first = 0;
        if let Some(f) = live.first().filter(|f| f.opts.asn1.raw_content) {
            g.w(2, format!("if self.{}.size > 0 {{", f.name));
            g.w(3, format!("return {a}.~enc_struct({a}.raw_content_body(self.{}), _p)", f.name));
            g.w(2, "}");
            first = 1;
        }
        g.w(2, "_b: [Byte] = []");
        for f in &live[first..] {
            g.enc_field(f, &format!("self.{}", f.name), 2).map_err(|m| fail(format!("field `{}`: {m}", f.name)))?;
        }
        g.w(2, format!("{a}.~enc_struct(_b, _p)"));
    }
    g.w(1, "}");
    // ---- decoding
    g.w(1, format!("pub def self.asn1_dec(_b: [Byte], _off: Int, _p: {a}.Params, _depth: Int) -> ~({name}, Int)<{a}.Asn1Error> {{"));
    let kind = if to.set { format!("{a}.K_SET") } else { format!("{a}.K_SEQUENCE") };
    g.w(2, format!("_h = {a}.~head(_b, _off, _p, {kind}, {}, _depth)", lit(&name)));
    g.w(2, format!("_r = {name}.new"));
    g.w(2, "if _h.present {");
    if wrapper {
        let f = live[0];
        let TypeExpr::Array(e, _) = &f.ty else { unreachable!() };
        let k = g.kind(e).map_err(&fail)?;
        g.w(3, "_s = _b[_h.body..._h.end]");
        g.w(3, format!("{a}.~check_seq_of(_s, {k})"));
        g.w(3, "_q = 0");
        g.w(3, "while _q < _s.size {");
        let np = format!("{a}.Params.new");
        g.dec(e, "_x", "_s", "_q", &np, &A1::default(), "_h.depth", 4).map_err(&fail)?;
        g.w(4, format!("_r.{} << _x", f.name));
        g.w(3, "}");
    } else {
        let mut first = 0;
        if let Some(f) = live.first().filter(|f| f.opts.asn1.raw_content) {
            g.w(3, format!("_r.{} = _b[_h.start..._h.end] + []", f.name));
            first = 1;
        }
        g.w(3, "_in = _b[_h.body..._h.end]");
        g.w(3, "_o = 0");
        g.w(3, "_d = _h.depth");
        for f in &live[first..] {
            let o = &f.opts.asn1;
            let t = g.fresh("t");
            let r: GResult<()> = (|| {
                if let TypeExpr::Opt(inner, _) = &f.ty {
                    let mut oo = o.clone();
                    oo.optional = true;
                    let p = g.params(&oo);
                    let s = g.fresh("o");
                    g.w(3, format!("{s} = _o"));
                    g.dec(inner, &t, "_in", "_o", &p, o, "_d", 3)?;
                    g.w(3, format!("_r.{} = {t} if _o != {s}", f.name));
                } else {
                    g.check_opts(&f.ty, o)?;
                    let p = g.params(o);
                    g.dec(&f.ty, &t, "_in", "_o", &p, o, "_d", 3)?;
                    g.w(3, format!("_r.{} = {t}", f.name));
                }
                Ok(())
            })();
            r.map_err(|m| fail(format!("field `{}`: {m}", f.name)))?;
        }
    }
    g.w(2, "}");
    g.w(2, "(_r, _h.next)");
    g.w(1, "}");
    // ---- the conveniences
    g.w(1, format!("pub def to_asn1 -> ~[Byte]<{a}.Asn1Error> {{"));
    g.w(2, format!("self.~asn1_enc({a}.Params.new)"));
    g.w(1, "}");
    g.w(1, format!("pub def to_asn1_with_params(_s: Str) -> ~[Byte]<{a}.Asn1Error> {{"));
    g.w(2, format!("self.~asn1_enc({a}.parse_field_parameters(_s))"));
    g.w(1, "}");
    g.w(1, format!("pub def self.from_asn1(_b: [Byte]) -> ~({name}, [Byte])<{a}.Asn1Error> {{"));
    g.w(2, format!("_v, _o = {name}.~asn1_dec(_b, 0, {a}.Params.new, 0)"));
    g.w(2, "(_v, _b[_o..._b.size])");
    g.w(1, "}");
    g.w(1, format!("pub def self.from_asn1_with_params(_b: [Byte], _s: Str) -> ~({name}, [Byte])<{a}.Asn1Error> {{"));
    g.w(2, format!("_v, _o = {name}.~asn1_dec(_b, 0, {a}.parse_field_parameters(_s), 0)"));
    g.w(2, "(_v, _b[_o..._b.size])");
    g.w(1, "}");
    g.w(0, "}");
    Ok(g.out)
}

fn lit(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('#', "\\#"))
}

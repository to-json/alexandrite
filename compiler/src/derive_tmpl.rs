//! `#[derive(Template)]`: how a struct or enum becomes text/template data.
//!
//! Like `#[derive(Json)]` (derive.rs), this is expansion by source text: the
//! parser records the declaration (fields with their `#[template(...)]`
//! options, and the type's methods), and at the end of the module this
//! module writes ordinary `def`s that the parser reads back in as methods.
//!
//! What gets generated (`T` is the local name of the `text/template` import):
//!
//!   def tmpl_add(_b: T.Doc) -> Int     the value as a node of `_b`: a struct
//!                                      (fields in order, renamed by options)
//!   def to_tmpl -> T.Value             the value as template data
//!
//! and, when the type has public methods the template can call (Go's
//! exported methods, `.Method args`), the `T.Data` protocol:
//!
//!   def tmpl_sig(name) -> T.Sig?       a method's Go signature
//!   def tmpl_call(name, args) -> ~T.Value
//!   def tmpl_string -> Str?            `to_s`, which fmt prints (Go's String)
//!   def tmpl_funcs -> Map[Str, T.Func] the methods as a FuncMap
//!
//! Field and result types: integers (Go's int, int8 ... uint64), Float,
//! Bool, Str, Complex, `T?` (a pointer: the value or a typed nil), `[T]`,
//! `[T; N]`, `Map[K, V]`, function types, `Error` (its message), T.Value
//! (any value), and types that derive Template themselves. Method parameters:
//! the scalar types, T.Value and `[scalar]`; a method with another parameter
//! type, a `!` method, a static or generic one isn't exposed.

use crate::ast::*;
use crate::derive::{type_src, DShape, DeriveJob};
use crate::diag::{Diag, Span};
use std::collections::HashMap;

/// A method of a derived type, as the derive sees it.
#[derive(Clone, Debug)]
pub struct TMethod {
    pub name: String,
    pub params: Vec<(String, TypeExpr)>,
    pub ret: Option<TypeExpr>,
    pub fallible: bool,
    pub public: bool,
    /// `#[template("Name")]` before the def: the name templates use.
    pub rename: Option<String>,
    pub skip: bool,
}

#[derive(Clone, Debug)]
pub struct TmplJob {
    pub job: DeriveJob,
    /// `#[template("pkg.Name")]` before the type: its Go type name.
    pub go_name: Option<String>,
    pub methods: Vec<TMethod>,
}

/// Parse `#[template("Name")]` / `#[template(skip)]`: (rename, skip).
pub fn parse_attr(text: &str, sp: Span) -> Result<(Option<String>, bool), Diag> {
    let inner = text.trim().strip_prefix("template").map(str::trim_start).and_then(|r| r.strip_prefix('(')).and_then(|r| r.trim_end().strip_suffix(')'));
    let Some(inner) = inner else {
        return Err(Diag::new(sp, format!("unknown attribute `#[{text}]`")).note("known: #[template(\"Name\")], #[template(skip)]"));
    };
    let (mut rename, mut skip) = (None, false);
    for a in inner.split(',') {
        let a = a.trim();
        if a.is_empty() {
            continue;
        }
        if let Some(s) = a.strip_prefix('"') {
            rename = Some(s.strip_suffix('"').ok_or_else(|| Diag::new(sp, "unterminated string in `#[template(...)]`"))?.to_string());
        } else if a == "skip" {
            skip = true;
        } else {
            return Err(Diag::new(sp, format!("unknown template option `{a}`")).note("options: \"Name\" (rename), skip"));
        }
    }
    Ok((rename, skip))
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

struct G<'a> {
    a: &'a str,
    /// Go names of this module's derived types.
    go_names: &'a HashMap<String, String>,
    /// Types of this module that don't derive Template.
    underived: &'a HashMap<String, bool>,
    tparams: &'a [String],
    out: String,
    n: usize,
    /// The doc being built into (`_b`, or a lambda's own).
    b: String,
}

type GR<T> = Result<T, String>;

fn is_value(n: &str) -> bool {
    n == "Value" || n.ends_with(".Value") && !n.starts_with("json.") && !n.starts_with("alxjson.")
}

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

    /// The Go type name a template sees for an alx type.
    fn go(&self, t: &TypeExpr) -> GR<String> {
        Ok(match t {
            TypeExpr::Named(n, _) => match n.as_str() {
                "Int" => "int".into(),
                "I64" => "int64".into(),
                "I32" | "Rune" => "int32".into(),
                "I16" => "int16".into(),
                "I8" => "int8".into(),
                "U64" => "uint64".into(),
                "U32" => "uint32".into(),
                "U16" => "uint16".into(),
                "U8" | "Byte" => "uint8".into(),
                "Float" | "F64" => "float64".into(),
                "Bool" => "bool".into(),
                "Str" => "string".into(),
                "Complex" => "complex128".into(),
                "Error" => "error".into(),
                n if is_value(n) => "interface {}".into(),
                n if self.tparams.iter().any(|p| p == n) => return Err(format!("the type parameter `{n}`: derive(Template) on a generic type isn't supported yet")),
                n => self.go_names.get(n).cloned().unwrap_or_else(|| n.to_string()),
            },
            TypeExpr::Opt(e, _) => format!("*{}", self.go(e)?),
            TypeExpr::Array(e, _) => format!("[]{}", self.go(e)?),
            TypeExpr::Fixed(e, n, _) => match &n.kind {
                ExprKind::Int(v) => format!("[{v}]{}", self.go(e)?),
                _ => return Err("a fixed-size array's length must be a literal for derive(Template)".into()),
            },
            TypeExpr::App(n, args, _) if n == "Map" && args.len() == 2 => format!("map[{}]{}", self.go(&args[0])?, self.go(&args[1])?),
            TypeExpr::Fn(ps, r, _) => {
                let ps: GR<Vec<String>> = ps.iter().map(|p| self.go(p)).collect();
                let (rt, fallible) = match &**r {
                    TypeExpr::Result(t, _, _) => (t.as_ref(), true),
                    t => (t, false),
                };
                let rg = self.go(rt)?;
                if fallible {
                    format!("func({}) ({rg}, error)", ps?.join(", "))
                } else {
                    format!("func({}) {rg}", ps?.join(", "))
                }
            }
            t => return Err(format!("`{}` can't be template data", type_src(t))),
        })
    }

    /// Statements (at `ind`) and an expression: the node of `x` (of type t)
    /// in the doc `_b`.
    fn node(&mut self, t: &TypeExpr, x: &str, ind: usize) -> GR<String> {
        Ok(match t {
            TypeExpr::Named(n, _) => {
                if let Some(k) = IntKind::from_name(n) {
                    let g = self.go(t)?;
                    if k.signed() {
                        let v = if k == IntKind::I64 { x.to_string() } else { format!("{x}.to_i") };
                        format!("{}.int({v}, {})", self.b, lit(&g))
                    } else {
                        let v = if k == IntKind::U64 { x.to_string() } else { format!("{x}.to_u64") };
                        format!("{}.uint({v}, {})", self.b, lit(&g))
                    }
                } else {
                    match n.as_str() {
                        "Float" | "F64" => format!("{}.float({x}, \"float64\")", self.b),
                        "Bool" => format!("{}.bool({x})", self.b),
                        "Str" => format!("{}.str({x}, \"string\")", self.b),
                        "Complex" => format!("{}.complex({x})", self.b),
                        "Error" => format!("{}.str({x}.message, \"*errors.errorString\")", self.b),
                        n if is_value(n) => format!("{}.add({x})", self.b),
                        n if self.tparams.iter().any(|p| p == n) => return Err(format!("the type parameter `{n}`: derive(Template) on a generic type isn't supported yet")),
                        n if self.underived.get(n) == Some(&false) => return Err(format!("`{n}` doesn't derive Template: add `#[derive(Template)]` to it")),
                        _ => format!("{x}.tmpl_add({})", self.b),
                    }
                }
            }
            TypeExpr::Opt(e, _) => {
                let (o, v) = (self.fresh("o"), self.fresh("v"));
                let g = self.go(t)?;
                self.w(ind, format!("{o} = 0"));
                self.w(ind, format!("if {v} = {x} {{"));
                let inner = self.node(e, &v, ind + 1)?;
                self.w(ind + 1, format!("{o} = {}.ptr({inner}, {})", self.b, lit(&g)));
                self.w(ind, "} else {");
                self.w(ind + 1, format!("{o} = {}.nil_of({})", self.b, lit(&g)));
                self.w(ind, "}");
                o
            }
            TypeExpr::Array(e, _) | TypeExpr::Fixed(e, _, _) => {
                let (l, v) = (self.fresh("l"), self.fresh("x"));
                let g = self.go(t)?;
                self.w(ind, format!("{l}: [Int] = []"));
                self.w(ind, format!("for {v} in {x} {{"));
                let inner = self.node(e, &v, ind + 1)?;
                self.w(ind + 1, format!("{l} << {inner}"));
                self.w(ind, "}");
                format!("{}.list({}, {l})", self.b, lit(&g))
            }
            TypeExpr::App(n, args, _) if n == "Map" && args.len() == 2 => {
                let (ks, vs, k, v) = (self.fresh("ks"), self.fresh("vs"), self.fresh("k"), self.fresh("v"));
                let g = self.go(t)?;
                self.w(ind, format!("{ks}: [Int] = []"));
                self.w(ind, format!("{vs}: [Int] = []"));
                self.w(ind, format!("for {k}, {v} in {x} {{"));
                let kn = self.node(&args[0], &k, ind + 1)?;
                self.w(ind + 1, format!("{ks} << {kn}"));
                let vn = self.node(&args[1], &v, ind + 1)?;
                self.w(ind + 1, format!("{vs} << {vn}"));
                self.w(ind, "}");
                format!("{}.map({}, {ks}, {vs})", self.b, lit(&g))
            }
            TypeExpr::Fn(ps, r, _) => {
                let fv = self.fresh("fv");
                self.w(ind, format!("{fv} = {x}"));
                let (rt, fallible) = match &**r {
                    TypeExpr::Result(t, _, _) => (t.as_ref().clone(), true),
                    t => (t.clone(), false),
                };
                let sig = self.sig(ps, &rt, fallible)?;
                let mut args = vec![];
                for (k, p) in ps.iter().enumerate() {
                    args.push(self.arg(p, &format!("_a[{k}]"))?);
                }
                let tilde = if fallible { "~" } else { "" };
                let a = self.a;
                let body = self.result_value(&rt, &format!("{fv}.{tilde}call({})", args.join(", ")), ind + 1)?;
                let lam = format!("{a}.Func.make({sig}, ->(_a: [{a}.Value]) -> ~{a}.Value {{\n{body}}})");
                format!("{}.func({lam}, \"\")", self.b)
            }
            t => return Err(format!("`{}` can't be template data", type_src(t))),
        })
    }

    /// Statements (a lambda's body) evaluating `call` and converting its
    /// result to a Value in a fresh doc.
    fn result_value(&mut self, rt: &TypeExpr, call: &str, ind: usize) -> GR<String> {
        let saved = std::mem::take(&mut self.out);
        let nb = self.fresh("b");
        let outer = std::mem::replace(&mut self.b, nb);
        let r = self.fresh("r");
        self.w(ind, format!("{r} = {call}"));
        self.w(ind, format!("{} = {}.Doc.make", self.b, self.a));
        let e = self.node(rt, &r, ind)?;
        self.w(ind, format!("{}.value({e})", self.b));
        self.b = outer;
        Ok(std::mem::replace(&mut self.out, saved))
    }

    /// The expression converting the (already checked) Value `v` to a
    /// parameter of type t.
    fn arg(&self, t: &TypeExpr, v: &str) -> GR<String> {
        Ok(match t {
            TypeExpr::Named(n, _) => {
                if let Some(k) = IntKind::from_name(n) {
                    match k {
                        IntKind::I64 => format!("{v}.int_val"),
                        k if k.signed() => format!("{v}.int_val.as_{}", k.method()),
                        IntKind::U64 => format!("{v}.uint_val"),
                        k => format!("{v}.uint_val.as_{}", k.method()),
                    }
                } else {
                    match n.as_str() {
                        "Float" | "F64" => format!("{v}.float_val"),
                        "Bool" => format!("{v}.bool_val"),
                        "Str" => format!("{v}.str_val"),
                        "Complex" => format!("{v}.complex_val"),
                        n if is_value(n) => v.to_string(),
                        n => return Err(format!("a parameter of type `{n}` can't come from a template")),
                    }
                }
            }
            TypeExpr::Array(e, _) => {
                let inner = self.arg(e, "it")?;
                format!("{v}.elems.map {{ {inner} }}")
            }
            t => return Err(format!("a parameter of type `{}` can't come from a template", type_src(t))),
        })
    }

    /// `T.Sig.new(...)` for parameters ps and result rt.
    fn sig(&self, ps: &[TypeExpr], rt: &TypeExpr, fallible: bool) -> GR<String> {
        let mut pg = vec![];
        for p in ps {
            let g = match p {
                TypeExpr::Named(n, _) if is_value(n) => "interface {}".to_string(),
                p => self.go(p)?,
            };
            pg.push(lit(&g));
        }
        let mut rs = vec![lit(&self.go(rt)?)];
        if fallible {
            rs.push(lit("error"));
        }
        Ok(format!("{}.Sig.new(params: [{}], variadic: false, results: [{}])", self.a, pg.join(", "), rs.join(", ")))
    }
}

/// The source text of `struct Name { defs }` holding the derived methods.
pub fn source(tj: &TmplJob, alias: &str, go_names: &HashMap<String, String>, underived: &HashMap<String, bool>) -> Result<String, Diag> {
    let job = &tj.job;
    let fail = |m: String| Diag::new(job.span, format!("derive(Template) on `{}`: {m}", job.name));
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Template yet; derive it on a concrete wrapper".to_string()));
    }
    let mut g = G { a: alias, go_names, underived, tparams: &job.tparams, out: String::new(), n: 0, b: "_b".to_string() };
    let name = &job.name;
    let a = alias;
    let go_name = tj.go_name.clone().unwrap_or_else(|| name.clone());
    // The methods a template can call.
    let mut methods: Vec<(TMethod, String)> = vec![];
    for m in &tj.methods {
        if !m.public || m.skip || m.name.ends_with('!') || m.params.first().map(|p| p.0 != "self").unwrap_or(true) {
            continue;
        }
        let Some(rt) = &m.ret else { continue };
        let ps: Vec<TypeExpr> = m.params[1..].iter().map(|p| p.1.clone()).collect();
        let ok = ps.iter().enumerate().all(|(k, p)| g.arg(p, &format!("_a[{k}]")).is_ok()) && g.go(rt).is_ok();
        if !ok {
            continue;
        }
        let sig = g.sig(&ps, rt, m.fallible).map_err(&fail)?;
        methods.push((m.clone(), sig));
    }
    let has_to_s = tj.methods.iter().any(|m| m.name.rsplit('.').next() == Some("to_s") && m.params.len() == 1 && m.params[0].0 == "self" && matches!(&m.ret, Some(TypeExpr::Named(n, _)) if n == "Str") && !m.fallible);
    let data = !methods.is_empty() || has_to_s;
    g.w(0, format!("struct {name} {{"));
    g.w(1, format!("pub def tmpl_add(_b: {a}.Doc) -> Int {{"));
    match &job.shape {
        DShape::Struct(fields) => {
            let mut names = vec![];
            g.w(2, "_v: [Int] = []");
            for f in fields {
                if f.opts.tskip {
                    continue;
                }
                names.push(lit(f.opts.trename.as_deref().unwrap_or(&f.name)));
                let e = g.node(&f.ty, &format!("self.{}", f.name), 2).map_err(&fail)?;
                g.w(2, format!("_v << {e}"));
            }
            g.w(2, format!("_n: [Str] = [{}]", names.join(", ")));
            if data {
                g.w(2, format!("_b.strct_obj({}, _n, _v, self)", lit(&go_name)));
            } else {
                g.w(2, format!("_b.strct({}, _n, _v)", lit(&go_name)));
            }
        }
        DShape::Enum(vs) => {
            g.w(2, "_r = 0");
            g.w(2, "case self {");
            for v in vs {
                let vname = v.opts.trename.clone().unwrap_or_else(|| v.name.clone());
                if v.fields.is_empty() {
                    g.w(3, format!("{} => {{", v.name));
                    if data {
                        g.w(4, format!("_r = _b.strct_obj({}, [\"variant\"], [_b.str({}, \"string\")], self)", lit(&go_name), lit(&vname)));
                    } else {
                        g.w(4, format!("_r = _b.str({}, {})", lit(&vname), lit(&go_name)));
                    }
                    g.w(4, "nil");
                    g.w(3, "}");
                    continue;
                }
                let vals: Vec<String> = (0..v.fields.len()).map(|k| format!("_f{k}")).collect();
                g.w(3, format!("{}({}) => {{", v.name, vals.join(", ")));
                let mut names = vec![lit("variant")];
                g.w(4, format!("_v: [Int] = [_b.str({}, \"string\")]", lit(&vname)));
                for (k, f) in v.fields.iter().enumerate() {
                    names.push(lit(f.opts.trename.as_deref().unwrap_or(&f.name)));
                    let e = g.node(&f.ty, &vals[k], 4).map_err(&fail)?;
                    g.w(4, format!("_v << {e}"));
                }
                g.w(4, format!("_n: [Str] = [{}]", names.join(", ")));
                if data {
                    g.w(4, format!("_r = _b.strct_obj({}, _n, _v, self)", lit(&go_name)));
                } else {
                    g.w(4, format!("_r = _b.strct({}, _n, _v)", lit(&go_name)));
                }
                g.w(4, "nil");
                g.w(3, "}");
            }
            g.w(2, "}");
            g.w(2, "_r");
        }
    }
    g.w(1, "}");
    g.w(1, format!("pub def to_tmpl -> {a}.Value {{"));
    g.w(2, format!("_b = {a}.Doc.make"));
    g.w(2, "_b.value(self.tmpl_add(_b))");
    g.w(1, "}");
    if data {
        g.w(1, format!("pub def tmpl_sig(_name: Str) -> {a}.Sig? {{"));
        g.w(2, "case _name {");
        for (m, sig) in &methods {
            let mname = m.name.rsplit('.').next().unwrap_or(&m.name);
            let tname = m.rename.clone().unwrap_or_else(|| mname.to_string());
            g.w(3, format!("{} => {sig}", lit(&tname)));
        }
        g.w(3, "_ => none");
        g.w(2, "}");
        g.w(1, "}");
        g.w(1, format!("pub def tmpl_call(_name: Str, _a: [{a}.Value]) -> ~{a}.Value {{"));
        g.w(2, format!("_b = {a}.Doc.make"));
        g.w(2, "case _name {");
        for (m, _) in &methods {
            let mname = m.name.rsplit('.').next().unwrap_or(&m.name).to_string();
            let tname = m.rename.clone().unwrap_or_else(|| mname.clone());
            g.w(3, format!("{} => {{", lit(&tname)));
            let mut args = vec![];
            for (k, p) in m.params[1..].iter().enumerate() {
                args.push(g.arg(&p.1, &format!("_a[{k}]")).map_err(&fail)?);
            }
            let tilde = if m.fallible { "~" } else { "" };
            g.w(4, format!("_r = self.{tilde}{mname}({})", args.join(", ")));
            let e = g.node(m.ret.as_ref().unwrap(), "_r", 4).map_err(&fail)?;
            g.w(4, format!("return _b.value({e})"));
            g.w(3, "}");
        }
        g.w(3, "_ => {}");
        g.w(2, "}");
        g.w(2, "fail Failure.Msg(\"template: no method \" + _name)");
        g.w(1, "}");
        g.w(1, "pub def tmpl_string -> Str? {");
        if has_to_s {
            g.w(2, "self.to_s");
        } else {
            g.w(2, "none");
        }
        g.w(1, "}");
        g.w(1, format!("pub def tmpl_funcs -> Map[Str, {a}.Func] {{"));
        g.w(2, format!("_m: Map[Str, {a}.Func] = {{}}"));
        g.w(2, "_s = self");
        for (m, sig) in &methods {
            let mname = m.name.rsplit('.').next().unwrap_or(&m.name);
            let tname = lit(&m.rename.clone().unwrap_or_else(|| mname.to_string()));
            g.w(2, format!("_m[{tname}] = {a}.Func.make({sig}, ->(_a: [{a}.Value]) -> ~{a}.Value {{ _s.~tmpl_call({tname}, _a) }})"));
        }
        g.w(2, "_m");
        g.w(1, "}");
    }
    g.w(0, "}");
    let _ = type_src;
    Ok(g.out)
}

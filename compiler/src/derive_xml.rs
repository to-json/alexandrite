//! `#[derive(Xml)]`: compile-time generation of `encoding/xml` code.
//!
//! Expansion by source text, like `#[derive(Json)]` (derive.rs): the parser
//! records the struct (its fields with their `#[field(xml: "...")]` tags,
//! and the names of its own methods), and at the end of the module this
//! module writes ordinary defs that the parser reads back in as methods.
//!
//! The work Go's encoding/xml does at run time with reflection is split in
//! two. Go's typeinfo.go (tag parsing, embedded-struct flattening, the field
//! path conflicts, the XMLName rules) runs here, at compile time, and its
//! errors are compile errors. Go's marshal.go / read.go walk (marshalValue,
//! marshalAttr, marshalStruct, unmarshal, unmarshalAttr, unmarshalPath,
//! copyValue) becomes code specialised to each field's type. What the walk
//! does per *type* rather than per field (Marshaler, TextMarshaler, the
//! struct default) is behind a protocol every named field type has, from
//! this derive or from the default methods of the interfaces in
//! std/encoding/xml/protocol.alx (`X` is the local name of the import):
//!
//!   def xml_enc(e: X.Encoder, tmpl: X.StartElement?, fname: X.Name) -> ~Unit
//!   def xml_attr(name: X.Name) -> ~X.Attr?
//!   def xml_chardata -> ~[Byte]?
//!   def xml_dec!(d: X.Decoder, start: X.StartElement) -> ~Unit
//!   def xml_attr_dec!(a: X.Attr) -> ~Unit
//!   def xml_text_dec!(b: [Byte]) -> ~Unit
//!
//! plus, for a struct, `xml_path!` (Go's unmarshalPath) and the
//! conveniences `to_xml`, `to_xml_indent` and `T.from_xml`.
//!
//! The design and the differences from Go: docs/notes/xml-derive.md.

#![allow(non_snake_case)]

use crate::ast::*;
use crate::derive::{DShape, DeriveJob, DField};
use crate::diag::Diag;
use std::collections::HashMap;

/// A struct to derive Xml for, and the names of the methods it declares
/// (a hand-written `marshal_xml` etc. takes precedence, as in Go).
#[derive(Clone, Debug)]
pub struct XmlJob {
    pub job: DeriveJob,
    pub methods: Vec<String>,
}

const F_ELEMENT: u32 = 1;
const F_ATTR: u32 = 2;
const F_CDATA: u32 = 4;
const F_CHARDATA: u32 = 8;
const F_INNERXML: u32 = 16;
const F_COMMENT: u32 = 32;
const F_ANY: u32 = 64;
const F_OMIT: u32 = 128;
const F_MODE: u32 = F_ELEMENT | F_ATTR | F_CDATA | F_CHARDATA | F_INNERXML | F_COMMENT | F_ANY;

/// One step through an embedded struct field.
#[derive(Clone, Debug)]
struct Seg {
    field: String,
    opt: bool,
    ty: String,
}

/// Go's fieldInfo: where the field is (`path` through embedded structs,
/// then `field`), its XML name, name space, flags and parent elements.
#[derive(Clone, Debug)]
struct FInfo {
    path: Vec<Seg>,
    field: String,
    ty: TypeExpr,
    name: String,
    xmlns: String,
    flags: u32,
    parents: Vec<String>,
    tag: String,
}

#[derive(Clone, Debug, Default)]
struct TInfo {
    xmlname: Option<FInfo>,
    fields: Vec<FInfo>,
}

fn xml_tag(f: &DField) -> String {
    f.opts.tags.iter().find(|(k, _)| k == "xml").map(|(_, v)| v.clone()).unwrap_or_default()
}

/// `,embed` in the tag: the field's struct is flattened into this one (Go's
/// anonymous struct field).
fn embedded(tag: &str) -> bool {
    tag.split(',').skip(1).any(|t| t.trim() == "embed")
}

struct Infos<'a> {
    jobs: &'a HashMap<String, &'a XmlJob>,
}

impl<'a> Infos<'a> {
    /// Go's getTypeInfo.
    fn type_info(&self, job: &DeriveJob, depth: usize) -> Result<TInfo, String> {
        if depth > 32 {
            return Err(format!("`{}`: embedded structs nest too deeply", job.name));
        }
        let mut tinfo = TInfo::default();
        let DShape::Struct(fields) = &job.shape else { return Err("derive(Xml) works on structs".into()) };
        for f in fields {
            let tag = xml_tag(f);
            if tag == "-" {
                continue;
            }
            if embedded(&tag) {
                let (inner_ty, opt) = match &f.ty {
                    TypeExpr::Named(n, _) => (n.clone(), false),
                    TypeExpr::Opt(e, _) => match &**e {
                        TypeExpr::Named(n, _) => (n.clone(), true),
                        _ => return Err(format!("field `{}`: `embed` takes a struct (or an optional struct) of this module that derives Xml", f.name)),
                    },
                    _ => return Err(format!("field `{}`: `embed` takes a struct (or an optional struct) of this module that derives Xml", f.name)),
                };
                let Some(inner_job) = self.jobs.get(&inner_ty) else {
                    return Err(format!("field `{}`: `embed` needs `{inner_ty}` to be a struct of this module that derives Xml", f.name));
                };
                let inner = self.type_info(&inner_job.job, depth + 1)?;
                let seg = Seg { field: f.name.clone(), opt, ty: inner_ty.clone() };
                if tinfo.xmlname.is_none() {
                    if let Some(mut x) = inner.xmlname.clone() {
                        // Go keeps the embedded field's index path unprefixed
                        // here, so the inherited XMLName's tag counts but its
                        // value is neither read nor set: no value.
                        x.path.insert(0, seg.clone());
                        x.ty = TypeExpr::Named("<inherited>".into(), f.span);
                        tinfo.xmlname = Some(x);
                    }
                }
                for mut fi in inner.fields {
                    fi.path.insert(0, seg.clone());
                    add_field_info(&job.name, &mut tinfo, fi)?;
                }
                continue;
            }
            let fi = self.field_info(&job.name, f, &tag)?;
            if f.name == "xml_name" {
                tinfo.xmlname = Some(fi);
                continue;
            }
            add_field_info(&job.name, &mut tinfo, fi)?;
        }
        Ok(tinfo)
    }

    /// Go's structFieldInfo.
    fn field_info(&self, tname: &str, f: &DField, tag0: &str) -> Result<FInfo, String> {
        let mut fi = FInfo { path: vec![], field: f.name.clone(), ty: f.ty.clone(), name: String::new(), xmlns: String::new(), flags: 0, parents: vec![], tag: tag0.to_string() };
        let mut tag = tag0.to_string();
        if let Some((ns, t)) = tag.clone().split_once(' ') {
            fi.xmlns = ns.to_string();
            tag = t.to_string();
        }
        let tokens: Vec<String> = tag.split(',').map(|s| s.to_string()).collect();
        let bad = || format!("xml: invalid tag in field {} of type {tname}: {}", f.name, go_quote(tag0));
        if tokens.len() == 1 {
            fi.flags = F_ELEMENT;
        } else {
            tag = tokens[0].clone();
            for flag in &tokens[1..] {
                match flag.as_str() {
                    "attr" => fi.flags |= F_ATTR,
                    "cdata" => fi.flags |= F_CDATA,
                    "chardata" => fi.flags |= F_CHARDATA,
                    "innerxml" => fi.flags |= F_INNERXML,
                    "comment" => fi.flags |= F_COMMENT,
                    "any" => fi.flags |= F_ANY,
                    "omitempty" => fi.flags |= F_OMIT,
                    _ => {}
                }
            }
            let mode = fi.flags & F_MODE;
            let mut valid = true;
            match mode {
                0 => fi.flags |= F_ELEMENT,
                m if [F_ATTR, F_CDATA, F_CHARDATA, F_INNERXML, F_COMMENT, F_ANY, F_ANY | F_ATTR].contains(&m) => {
                    if f.name == "xml_name" || (!tag.is_empty() && mode != F_ATTR) {
                        valid = false;
                    }
                }
                _ => valid = false,
            }
            if fi.flags & F_MODE == F_ANY {
                fi.flags |= F_ELEMENT;
            }
            if fi.flags & F_OMIT != 0 && fi.flags & (F_ELEMENT | F_ATTR) == 0 {
                valid = false;
            }
            if !valid {
                return Err(bad());
            }
        }
        if !fi.xmlns.is_empty() && tag.is_empty() {
            return Err(format!("xml: namespace without name in field {} of type {tname}: {}", f.name, go_quote(tag0)));
        }
        if f.name == "xml_name" {
            fi.name = tag;
            return Ok(fi);
        }
        if tag.is_empty() {
            if let Some(x) = self.lookup_xml_name(&f.ty) {
                fi.xmlns = x.xmlns;
                fi.name = x.name;
            } else {
                fi.name = f.name.clone();
            }
            return Ok(fi);
        }
        let mut parents: Vec<String> = tag.split('>').map(|s| s.to_string()).collect();
        if parents[0].is_empty() {
            parents[0] = f.name.clone();
        }
        if parents.last().is_some_and(|p| p.is_empty()) {
            return Err(format!("xml: trailing '>' in field {} of type {tname}", f.name));
        }
        fi.name = parents.last().cloned().unwrap_or_default();
        if parents.len() > 1 {
            if fi.flags & F_ELEMENT == 0 {
                return Err(format!("xml: {tag} chain not valid with {} flag", tokens[1..].join(",")));
            }
            parents.pop();
            fi.parents = parents;
        }
        if fi.flags & F_ELEMENT != 0 {
            if let Some(x) = self.lookup_xml_name(&f.ty) {
                if x.name != fi.name {
                    return Err(format!("xml: name {} in tag of {tname}.{} conflicts with name {} in {}.XMLName", go_quote(&fi.name), f.name, go_quote(&x.name), type_name_of(&f.ty)));
                }
            }
        }
        Ok(fi)
    }

    /// Go's lookupXMLName: the xml_name tag of a struct of this module.
    fn lookup_xml_name(&self, t: &TypeExpr) -> Option<FInfo> {
        let n = match t {
            TypeExpr::Named(n, _) => n,
            TypeExpr::Opt(e, _) => match &**e {
                TypeExpr::Named(n, _) => n,
                _ => return None,
            },
            _ => return None,
        };
        let job = self.jobs.get(n)?;
        let DShape::Struct(fields) = &job.job.shape else { return None };
        let f = fields.iter().find(|f| f.name == "xml_name")?;
        let tag = xml_tag(f);
        match self.field_info(n, f, &tag) {
            Ok(fi) if !fi.name.is_empty() => Some(fi),
            _ => None,
        }
    }
}

fn type_name_of(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Opt(e, _) => type_name_of(e),
        t => crate::derive::type_src(t),
    }
}

/// Go's addFieldInfo: conflicting paths are errors (Go's TagPathError), or
/// the shallower field wins.
fn add_field_info(tname: &str, tinfo: &mut TInfo, newf: FInfo) -> Result<(), String> {
    let mut conflicts = vec![];
    'outer: for (i, oldf) in tinfo.fields.iter().enumerate() {
        if oldf.flags & F_MODE != newf.flags & F_MODE {
            continue;
        }
        if !oldf.xmlns.is_empty() && !newf.xmlns.is_empty() && oldf.xmlns != newf.xmlns {
            continue;
        }
        let minl = newf.parents.len().min(oldf.parents.len());
        for p in 0..minl {
            if oldf.parents[p] != newf.parents[p] {
                continue 'outer;
            }
        }
        if oldf.parents.len() > newf.parents.len() {
            if oldf.parents[newf.parents.len()] == newf.name {
                conflicts.push(i);
            }
        } else if oldf.parents.len() < newf.parents.len() {
            if newf.parents[oldf.parents.len()] == oldf.name {
                conflicts.push(i);
            }
        } else if newf.name == oldf.name && newf.xmlns == oldf.xmlns {
            conflicts.push(i);
        }
    }
    if conflicts.is_empty() {
        tinfo.fields.push(newf);
        return Ok(());
    }
    let depth = |f: &FInfo| f.path.len();
    for &i in &conflicts {
        if depth(&tinfo.fields[i]) < depth(&newf) {
            return Ok(());
        }
    }
    for &i in &conflicts {
        let oldf = &tinfo.fields[i];
        if depth(oldf) == depth(&newf) {
            return Err(format!("{tname} field {} with tag {} conflicts with field {} with tag {}", go_quote(&oldf.field), go_quote(&oldf.tag), go_quote(&newf.field), go_quote(&newf.tag)));
        }
    }
    for &i in conflicts.iter().rev() {
        tinfo.fields.remove(i);
    }
    tinfo.fields.push(newf);
    Ok(())
}

/// Go's %q of an ASCII-ish string.
fn go_quote(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

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

/// How a field type is written and read.
#[derive(Clone, Debug)]
enum K<'a> {
    Int(IntKind),
    Float,
    Bool,
    Str,
    Bytes,
    Opt(&'a TypeExpr),
    Slice(&'a TypeExpr),
    Fixed(&'a TypeExpr, String),
    Dyn,
    Named(String),
}

struct Gen<'a> {
    x: &'a str,
    tname: &'a str,
    out: String,
    n: usize,
}

type GR<T> = Result<T, String>;

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

    fn kind<'t>(&self, t: &'t TypeExpr) -> GR<K<'t>> {
        Ok(match t {
            TypeExpr::Named(n, _) => {
                if let Some(k) = IntKind::from_name(n) {
                    K::Int(k)
                } else if n == "Float" || n == "F64" {
                    K::Float
                } else if n == "Bool" {
                    K::Bool
                } else if n == "Str" {
                    K::Str
                } else if n == "dyn.Value" {
                    K::Dyn
                } else if matches!(n.as_str(), "F32" | "Float32" | "Ptr" | "Unit" | "Complex" | "Error") {
                    return Err(format!("xml: unsupported type: {n}"));
                } else {
                    K::Named(n.clone())
                }
            }
            TypeExpr::Opt(e, _) => K::Opt(e),
            TypeExpr::Array(e, _) => match &**e {
                TypeExpr::Named(n, _) if n == "Byte" || n == "U8" => K::Bytes,
                _ => K::Slice(e),
            },
            TypeExpr::Fixed(e, n, _) => {
                if matches!(&**e, TypeExpr::Named(n, _) if n == "Byte" || n == "U8") {
                    return Err("xml: fixed-size byte arrays aren't supported; use [Byte]".into());
                }
                let n = match &n.kind {
                    ExprKind::Int(v) => v.to_string(),
                    _ => "N".to_string(),
                };
                K::Fixed(e, n)
            }
            t => return Err(format!("xml: unsupported type: {}", crate::derive::type_src(t))),
        })
    }

    fn ts(&self, t: &TypeExpr) -> String {
        crate::derive::type_src(t)
    }

    fn is_name_type(&self, t: &TypeExpr) -> bool {
        matches!(t, TypeExpr::Named(n, _) if *n == format!("{}.Name", self.x) || n == "xml.Name")
    }

    /// A zero value of t (for a typed declaration).
    fn zero(&self, t: &TypeExpr) -> GR<String> {
        Ok(match self.kind(t)? {
            K::Int(_) => "0".into(),
            K::Float => "0.0".into(),
            K::Bool => "false".into(),
            K::Str => "\"\"".into(),
            K::Bytes | K::Slice(_) => "[]".into(),
            K::Opt(_) => "none".into(),
            K::Fixed(e, n) => format!("[{}; {n}]", self.zero(e)?),
            K::Dyn => "dyn.Value.invalid".into(),
            K::Named(n) => format!("{n}.new"),
        })
    }

    /// The text of a simple value (an expression of type Str).
    fn text_of(&self, k: &K, x: &str) -> String {
        match k {
            K::Int(_) => format!("\"#{{{x}}}\""),
            K::Float => format!("{}.float_text({x})", self.x),
            K::Bool => format!("({x} ? \"true\" : \"false\")"),
            K::Str => x.to_string(),
            K::Bytes => format!("Str.from_bytes({x})"),
            _ => unreachable!(),
        }
    }

    /// Go's isEmptyValue, or None (never empty).
    fn empty(&self, t: &TypeExpr, x: &str) -> GR<Option<String>> {
        Ok(match self.kind(t)? {
            K::Int(_) => Some(format!("{x} == 0")),
            K::Float => Some(format!("{x} == 0.0")),
            K::Bool => Some(format!("!{x}")),
            K::Str | K::Bytes | K::Slice(_) => Some(format!("{x}.size == 0")),
            K::Fixed(_, n) => Some(if n == "0" { "true".into() } else { "false".into() }),
            K::Opt(_) => Some(format!("{x}.none?")),
            K::Dyn => Some(format!("{}.dyn_empty?({x})", self.x)),
            K::Named(_) => None,
        })
    }

    fn name_expr(&self, space: &str, local: &str) -> String {
        format!("{}.Name.new(space: {}, local: {})", self.x, lit(space), lit(local))
    }

    // ------------------------------------------------------------ marshal

    /// Go's marshalValue for a field value x of type t, named fname.
    fn enc_value(&mut self, t: &TypeExpr, x: &str, fname: &str, omit: bool, ind: usize) -> GR<()> {
        let mut ind = ind;
        let mut closes = 0;
        if omit {
            if let Some(c) = self.empty(t, x)? {
                self.w(ind, format!("if !({c}) {{"));
                ind += 1;
                closes += 1;
            }
        }
        let k = self.kind(t)?;
        let X = self.x.to_string();
        match k {
            K::Opt(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc_value(inner, &v, fname, false, ind + 1)?;
                self.w(ind, "}");
            }
            K::Slice(inner) | K::Fixed(inner, _) => {
                let v = self.fresh("v");
                self.w(ind, format!("for {v} in {x} {{"));
                self.enc_value(inner, &v, fname, omit, ind + 1)?;
                self.w(ind, "}");
            }
            K::Named(_) => self.w(ind, format!("{x}.~xml_enc(_e, none, {fname})")),
            K::Dyn => self.w(ind, format!("{X}.~marshal_dyn(_e, {x}, none, {fname}, {omit})")),
            k => {
                self.w(ind, format!("_e.~write_start!({X}.StartElement.new(name: {fname}))"));
                if matches!(k, K::Bytes) {
                    self.w(ind, format!("_e.escape_bytes!({x})"));
                } else {
                    let s = self.text_of(&k, x);
                    self.w(ind, format!("_e.escape_string!({s})"));
                }
                self.w(ind, format!("_e.~write_end!({fname})"));
            }
        }
        for _ in 0..closes {
            ind -= 1;
            self.w(ind, "}");
        }
        Ok(())
    }

    /// Go's marshalAttr: appends to the local `_attrs`.
    fn enc_attr(&mut self, t: &TypeExpr, x: &str, name: &str, ind: usize) -> GR<()> {
        let X = self.x.to_string();
        match self.kind(t)? {
            K::Opt(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc_attr(inner, &v, name, ind + 1)?;
                self.w(ind, "}");
            }
            K::Slice(inner) | K::Fixed(inner, _) => {
                let v = self.fresh("v");
                self.w(ind, format!("for {v} in {x} {{"));
                self.enc_attr(inner, &v, name, ind + 1)?;
                self.w(ind, "}");
            }
            K::Named(_) => {
                let a = self.fresh("a");
                self.w(ind, format!("if {a} = {x}.~xml_attr({name}) {{"));
                self.w(ind + 1, format!("_attrs << {a}"));
                self.w(ind, "}");
            }
            K::Dyn => self.w(ind, format!("_attrs = _attrs + {X}.~dyn_attrs({x}, {name})")),
            k => {
                let s = self.text_of(&k, x);
                self.w(ind, format!("_attrs << {X}.Attr.new(name: {name}, value: {s})"));
            }
        }
        Ok(())
    }

    /// A chardata / cdata field's text (Go's marshalStruct, fCDATA / fCharData).
    fn enc_chardata(&mut self, t: &TypeExpr, x: &str, cdata: bool, ind: usize) -> GR<()> {
        let X = self.x.to_string();
        let emit = |s: String| if cdata { format!("_e.write_cdata!({s})") } else { format!("_e.escape_string!({s})") };
        match self.kind(t)? {
            K::Opt(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc_chardata(inner, &v, cdata, ind + 1)?;
                self.w(ind, "}");
            }
            K::Named(_) => {
                let b = self.fresh("b");
                self.w(ind, format!("if {b} = {x}.~xml_chardata {{"));
                self.w(ind + 1, emit(format!("Str.from_bytes({b})")));
                self.w(ind, "}");
            }
            K::Dyn => {
                let s = self.fresh("s");
                self.w(ind, format!("if {s} = {X}.~dyn_text({x}) {{"));
                self.w(ind + 1, emit(s.clone()));
                self.w(ind, "}");
            }
            K::Slice(_) | K::Fixed(..) => {}
            k => {
                let s = self.text_of(&k, x);
                self.w(ind, emit(s));
            }
        }
        Ok(())
    }

    fn enc_comment(&mut self, t: &TypeExpr, x: &str, fieldname: &str, ind: usize) -> GR<()> {
        let X = self.x.to_string();
        let tn = self.tname.to_string();
        match self.kind(t)? {
            K::Str => self.w(ind, format!("_e.~write_comment!({x})")),
            K::Bytes => self.w(ind, format!("_e.~write_comment!(Str.from_bytes({x}))")),
            K::Opt(inner) if matches!(self.kind(inner)?, K::Str | K::Bytes) => {
                let v = self.fresh("v");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc_comment(inner, &v, fieldname, ind + 1)?;
                self.w(ind, "} else {");
                self.w(ind + 1, format!("fail {X}.bad_comment({})", lit(&tn)));
                self.w(ind, "}");
            }
            K::Dyn => self.w(ind, format!("{X}.~dyn_comment(_e, {x}, {})", lit(&tn))),
            _ => return Err(format!("xml: bad type for comment field {fieldname} of {tn}: a comment is Str, [Byte] or an optional of them")),
        }
        Ok(())
    }

    fn enc_inner(&mut self, t: &TypeExpr, x: &str, fname: &str, ind: usize) -> GR<()> {
        let X = self.x.to_string();
        match self.kind(t)? {
            K::Str => self.w(ind, format!("_e.write_raw!({x})")),
            K::Bytes => self.w(ind, format!("_e.write_raw!(Str.from_bytes({x}))")),
            K::Opt(inner) if matches!(self.kind(inner)?, K::Str | K::Bytes) => {
                let v = self.fresh("v");
                self.w(ind, format!("if {v} = {x} {{"));
                self.enc_inner(inner, &v, fname, ind + 1)?;
                self.w(ind, "}");
            }
            K::Dyn => self.w(ind, format!("{X}.~dyn_inner(_e, {x}, {fname})")),
            _ => self.enc_value(t, x, fname, false, ind)?,
        }
        Ok(())
    }

    /// Opens `if _p = self.e {` for each optional embedded struct on the way
    /// to a field (Go's value with dontInitNilPointers: a nil one skips the
    /// field); returns the field's expression and the number of `}` to write.
    fn read_path(&mut self, fi: &FInfo, ind: usize) -> (String, usize) {
        let mut place = "self".to_string();
        let mut opened = 0;
        for seg in &fi.path {
            if seg.opt {
                let p = self.fresh("p");
                self.w(ind + opened, format!("if {p} = {place}.{} {{", seg.field));
                opened += 1;
                place = p;
            } else {
                place = format!("{place}.{}", seg.field);
            }
        }
        (format!("{place}.{}", fi.field), opened)
    }

    fn close(&mut self, ind: usize, opened: usize) {
        for k in (0..opened).rev() {
            self.w(ind + k, "}");
        }
    }

    /// Places a field for writing: optional embedded structs on the way are
    /// taken out (made if missing) into locals and put back after `body`
    /// (Go's value with initNilPointers).
    fn write_path(&mut self, fi: &FInfo, ind: usize, body: &mut dyn FnMut(&mut Gen<'a>, &str, usize) -> GR<()>) -> GR<()> {
        let mut place = "self".to_string();
        let mut back: Vec<(String, String)> = vec![];
        for seg in &fi.path {
            if seg.opt {
                let p = self.fresh("p");
                let outer = format!("{place}.{}", seg.field);
                self.w(ind, format!("{p}: {} = {outer} || {}.new", seg.ty, seg.ty));
                back.push((outer, p.clone()));
                place = p;
            } else {
                place = format!("{place}.{}", seg.field);
            }
        }
        let fp = format!("{place}.{}", fi.field);
        body(self, &fp, ind)?;
        for (outer, p) in back.iter().rev() {
            self.w(ind, format!("{outer} = {p}"));
        }
        Ok(())
    }

    // ------------------------------------------------------------ unmarshal

    /// `place.~call` for a `!` method: a field is taken into a local and put
    /// back (a `!` method is called on a variable).
    fn call_mut(&mut self, place: &str, call: &str, ind: usize) {
        if !place.contains('.') {
            self.w(ind, format!("{place}.~{call}"));
            return;
        }
        let t = self.fresh("m");
        self.w(ind, format!("{t} = {place}"));
        self.w(ind, format!("{t}.~{call}"));
        self.w(ind, format!("{place} = {t}"));
    }

    /// Statements converting the bytes `b` into `place` (Go's copyValue).
    fn convert(&mut self, k: &K, place: &str, b: &str, ind: usize) {
        let X = self.x.to_string();
        match k {
            K::Int(ik) => {
                let v = self.fresh("n");
                match ik {
                    IntKind::I64 => self.w(ind, format!("{place} = {X}.~text_int({b}, 64)")),
                    IntKind::U64 => self.w(ind, format!("{place} = {X}.~text_uint({b}, 64)")),
                    k if k.signed() => {
                        self.w(ind, format!("{v} = {X}.~text_int({b}, {})", k.bits()));
                        self.w(ind, format!("{place} = {v}.as_{}", k.method()));
                    }
                    k => {
                        self.w(ind, format!("{v} = {X}.~text_uint({b}, {})", k.bits()));
                        self.w(ind, format!("{place} = {v}.as_{}", k.method()));
                    }
                }
            }
            K::Float => self.w(ind, format!("{place} = {X}.~text_float({b})")),
            K::Bool => self.w(ind, format!("{place} = {X}.~text_bool({b})")),
            K::Str => self.w(ind, format!("{place} = Str.from_bytes({b})")),
            K::Bytes => self.w(ind, format!("{place} = {b}")),
            _ => unreachable!(),
        }
    }

    /// Go's unmarshal of one element (start) into place.
    fn dec_value(&mut self, t: &TypeExpr, place: &str, start: &str, ind: usize) -> GR<()> {
        let X = self.x.to_string();
        match self.kind(t)? {
            K::Opt(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("{v}: {} = {place} || {}", self.ts(inner), self.zero(inner)?));
                self.dec_value(inner, &v, start, ind)?;
                self.w(ind, format!("{place} = {v}"));
            }
            K::Slice(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("{v}: {} = {}", self.ts(inner), self.zero(inner)?));
                self.dec_value(inner, &v, start, ind)?;
                self.w(ind, format!("{place} << {v}"));
            }
            K::Fixed(inner, n) => {
                self.w(ind, "_d.~check_depth");
                self.w(ind, format!("fail {X}.unknown_type({})", lit(&format!("[{n}]{}", self.ts(inner)))));
            }
            K::Named(_) => self.call_mut(place, &format!("xml_dec!(_d, {start})"), ind),
            K::Dyn => {
                self.w(ind, "_d.~check_depth");
                self.w(ind, "_d.~skip!");
            }
            k => {
                self.w(ind, "_d.~check_depth");
                let b = self.fresh("b");
                self.w(ind, format!("{b} = _d.~read_text!"));
                self.convert(&k, place, &b, ind);
            }
        }
        Ok(())
    }

    /// Go's unmarshalAttr of the attribute `a` into place.
    fn dec_attr(&mut self, t: &TypeExpr, place: &str, a: &str, ind: usize) -> GR<()> {
        let X = self.x.to_string();
        match self.kind(t)? {
            K::Opt(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("{v}: {} = {place} || {}", self.ts(inner), self.zero(inner)?));
                self.dec_attr(inner, &v, a, ind)?;
                self.w(ind, format!("{place} = {v}"));
            }
            K::Slice(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("{v}: {} = {}", self.ts(inner), self.zero(inner)?));
                self.dec_attr(inner, &v, a, ind)?;
                self.w(ind, format!("{place} << {v}"));
            }
            K::Fixed(inner, n) => self.w(ind, format!("fail {X}.cannot_unmarshal({})", lit(&format!("[{n}]{}", self.ts(inner))))),
            K::Named(_) => self.call_mut(place, &format!("xml_attr_dec!({a})"), ind),
            K::Dyn => self.w(ind, format!("fail {X}.cannot_unmarshal(\"interface {{}}\")")),
            k => {
                let b = self.fresh("b");
                self.w(ind, format!("{b} = {a}.value.bytes"));
                self.convert(&k, place, &b, ind);
            }
        }
        Ok(())
    }

    /// Character data into place (Go's TextUnmarshaler check, then copyValue).
    fn dec_text(&mut self, t: &TypeExpr, place: &str, data: &str, fieldname: &str, ind: usize) -> GR<()> {
        let X = self.x.to_string();
        match self.kind(t)? {
            K::Opt(inner) => {
                let v = self.fresh("v");
                self.w(ind, format!("{v}: {} = {place} || {}", self.ts(inner), self.zero(inner)?));
                self.dec_text(inner, &v, data, fieldname, ind)?;
                self.w(ind, format!("{place} = {v}"));
            }
            K::Named(_) => self.call_mut(place, &format!("xml_text_dec!({data})"), ind),
            K::Dyn => self.w(ind, format!("fail {X}.cannot_unmarshal(\"interface {{}}\")")),
            K::Slice(_) | K::Fixed(..) => return Err(format!("field `{fieldname}`: character data goes into a Str, [Byte], a number, Bool, or a type with unmarshal_text!")),
            k => self.convert(&k, place, data, ind),
        }
        Ok(())
    }
}

/// Which of Go's interfaces the struct implements itself.
struct Own {
    marshal: bool,
    unmarshal: bool,
    marshal_attr: bool,
    unmarshal_attr: bool,
    text: bool,
    untext: bool,
}

/// The source text of `struct Name { defs }` holding the derived methods.
pub fn xml_source(xj: &XmlJob, alias: &str, jobs: &HashMap<String, &XmlJob>) -> Result<String, Diag> {
    let job = &xj.job;
    let fail = |m: String| Diag::new(job.span, format!("derive(Xml) on `{}`: {m}", job.name));
    if !job.tparams.is_empty() {
        return Err(fail("generic types can't derive Xml yet; derive it on a concrete wrapper, or implement Marshaler / Unmarshaler by hand".into()));
    }
    let DShape::Struct(_) = &job.shape else {
        return Err(fail("derive(Xml) works on structs (Go has no sum types); give an enum unmarshal_text! / marshal_text and implement the protocol by hand".into()));
    };
    let has = |m: &str| xj.methods.iter().any(|x| x == m);
    let own = Own {
        marshal: has("marshal_xml"),
        unmarshal: has("unmarshal_xml!"),
        marshal_attr: has("marshal_xml_attr"),
        unmarshal_attr: has("unmarshal_xml_attr!"),
        text: has("marshal_text"),
        untext: has("unmarshal_text!"),
    };
    let infos = Infos { jobs };
    let tinfo = infos.type_info(job, 0).map_err(&fail)?;
    let name = job.name.clone();
    let tl = lit(&name);
    let X = alias.to_string();
    let mut g = Gen { x: alias, tname: &name, out: String::new(), n: 0 };
    g.w(0, format!("struct {name} {{"));

    // ---- xml_enc: Go's marshalValue for this type
    g.w(1, format!("pub def xml_enc(_e: {X}.Encoder, _tmpl: {X}.StartElement?, _fname: {X}.Name) -> ~Unit {{"));
    if own.marshal {
        g.w(2, format!("_start = {X}.start_for(_tmpl, {X}.Name.new, _fname, {tl})"));
        g.w(2, "_n = _e.begin_marshaler!");
        g.w(2, "self.~marshal_xml(_e, _start)");
        g.w(2, format!("_e.~end_marshaler!(_n, {tl})"));
    } else if own.text {
        g.w(2, format!("_start = {X}.start_for(_tmpl, {X}.Name.new, _fname, {tl})"));
        g.w(2, "_e.~write_start!(_start)");
        g.w(2, "_text = self.~marshal_text");
        g.w(2, "_e.escape_bytes!(_text)");
        g.w(2, "_e.~write_end!(_start.name)");
    } else {
        enc_struct(&mut g, &tinfo, &name).map_err(&fail)?;
    }
    g.w(1, "}");

    // ---- xml_attr: Go's marshalAttr for this type
    g.w(1, format!("pub def xml_attr(_name: {X}.Name) -> ~{X}.Attr? {{"));
    if own.marshal_attr {
        g.w(2, "_a = self.~marshal_xml_attr(_name)");
        g.w(2, "return none if _a.name.local == \"\"");
        g.w(2, "_a");
    } else if own.text {
        g.w(2, "_text = self.~marshal_text");
        g.w(2, format!("{X}.Attr.new(name: _name, value: Str.from_bytes(_text))"));
    } else {
        g.w(2, format!("fail {X}.XmlError.UnsupportedType({tl})"));
    }
    g.w(1, "}");

    // ---- xml_chardata: the text of a chardata field of this type
    g.w(1, "pub def xml_chardata -> ~[Byte]? {");
    if own.text {
        g.w(2, "self.~marshal_text");
    } else {
        g.w(2, "none");
    }
    g.w(1, "}");

    // ---- xml_dec!: Go's unmarshal for this type
    g.w(1, format!("pub def xml_dec!(_d: {X}.Decoder, _start: {X}.StartElement) -> ~Unit {{"));
    g.w(2, "_d.~check_depth");
    if own.unmarshal {
        g.w(2, "_saved = _d.begin_unmarshaler!");
        g.w(2, "_r = self.unmarshal_xml!(_d, _start)");
        g.w(2, format!("_d.~end_unmarshaler!(_saved, _r.err, {tl}, _start)"));
    } else if own.untext {
        g.w(2, "_text = _d.~element_text!");
        g.w(2, "self.~unmarshal_text!(_text)");
    } else {
        dec_struct(&mut g, &tinfo, &name).map_err(&fail)?;
    }
    g.w(1, "}");

    // ---- xml_attr_dec!: Go's unmarshalAttr for this type
    g.w(1, format!("pub def xml_attr_dec!(_a: {X}.Attr) -> ~Unit {{"));
    if own.unmarshal_attr {
        g.w(2, "self.~unmarshal_xml_attr!(_a)");
    } else if own.untext {
        g.w(2, "self.~unmarshal_text!(_a.value.bytes)");
    } else {
        g.w(2, format!("fail {X}.cannot_unmarshal({tl})"));
    }
    g.w(1, "}");

    // ---- xml_text_dec!: character data into this type
    g.w(1, "pub def xml_text_dec!(_b: [Byte]) -> ~Unit {");
    if own.untext {
        g.w(2, "self.~unmarshal_text!(_b)");
    } else {
        g.w(2, format!("fail {X}.cannot_unmarshal({tl})"));
    }
    g.w(1, "}");

    if !own.unmarshal && !own.untext {
        path_def(&mut g, &tinfo).map_err(&fail)?;
    }

    // ---- conveniences
    g.w(1, "pub def to_xml -> ~Str {");
    g.w(2, format!("_e = {X}.new_buffer_encoder()"));
    g.w(2, "_e.~encode!(self)");
    g.w(2, "_e.~close!");
    g.w(2, "_e.output");
    g.w(1, "}");
    g.w(1, "pub def to_xml_indent(_prefix: Str, _indent: Str) -> ~Str {");
    g.w(2, format!("_e = {X}.new_buffer_encoder()"));
    g.w(2, "_e.indent!(_prefix, _indent)");
    g.w(2, "_e.~encode!(self)");
    g.w(2, "_e.~close!");
    g.w(2, "_e.output");
    g.w(1, "}");
    g.w(1, format!("pub def self.from_xml(_s: Str) -> ~{name} {{"));
    g.w(2, format!("_d = {X}.new_string_decoder(_s)"));
    g.w(2, format!("_d.~decode!({name}.new)"));
    g.w(1, "}");
    g.w(0, "}");
    Ok(g.out)
}

/// The default xml_enc body: Go's marshalValue for a struct, then
/// marshalStruct.
fn enc_struct(g: &mut Gen, tinfo: &TInfo, tname: &str) -> GR<()> {
    let X = g.x.to_string();
    g.w(2, format!("_start = {X}.StartElement.new(name: {X}.Name.new)"));
    g.w(2, "if _t = _tmpl {");
    g.w(3, "_start = _t.copy");
    let mut ex = 2;
    if let Some(xn) = &tinfo.xmlname {
        g.w(2, "} else {");
        if !xn.name.is_empty() {
            g.w(3, format!("_start.name = {}", g.name_expr(&xn.xmlns, &xn.name)));
        } else if g.is_name_type(&xn.ty) {
            let (v, opened) = g.read_path(xn, 3);
            g.w(3 + opened, format!("if {v}.local != \"\" {{"));
            g.w(4 + opened, format!("_start.name = {v}"));
            g.w(3 + opened, "}");
            g.close(3, opened);
        }
        ex = 2;
    }
    g.w(ex, "}");
    g.w(2, "if _start.name.local == \"\" {");
    g.w(3, "_start.name = _fname");
    g.w(2, "}");
    g.w(2, "if _start.name.local == \"\" {");
    g.w(3, format!("_start.name = {X}.Name.new(local: {})", lit(tname)));
    g.w(2, "}");
    // Attributes
    g.w(2, format!("_attrs: [{X}.Attr] = _start.attr.dup"));
    for fi in tinfo.fields.iter().filter(|f| f.flags & F_ATTR != 0) {
        let (v, opened) = g.read_path(fi, 2);
        let ind = 2 + opened;
        let mut close = 0;
        if fi.flags & F_OMIT != 0 {
            if let Some(c) = g.empty(&fi.ty, &v)? {
                g.w(ind, format!("if !({c}) {{"));
                close = 1;
            }
        }
        let nm = g.name_expr(&fi.xmlns, &fi.name);
        g.enc_attr(&fi.ty, &v, &nm, ind + close)?;
        if close == 1 {
            g.w(ind, "}");
        }
        g.close(2, opened);
    }
    g.w(2, "_start.attr = _attrs");
    if let Some(xn) = &tinfo.xmlname {
        if xn.xmlns.is_empty() && xn.name.is_empty() {
            g.w(2, "if _start.name.space == \"\" && _e.top_space != \"\" {");
            g.w(3, format!("_start.attr << {X}.Attr.new(name: {X}.Name.new(local: \"xmlns\"), value: \"\")"));
            g.w(2, "}");
        }
    }
    g.w(2, "_e.~write_start!(_start)");
    // marshalStruct
    let any_parents = tinfo.fields.iter().any(|f| !f.parents.is_empty());
    if any_parents {
        g.w(2, "_ps: [Str] = []");
    }
    let trim = |g: &mut Gen, parents: &[String], ind: usize| {
        if any_parents {
            g.w(ind, format!("_ps = _e.~trim_parents!(_ps, {})", str_list(parents)));
        }
    };
    for fi in tinfo.fields.iter().filter(|f| f.flags & F_ATTR == 0) {
        let (v, opened) = g.read_path(fi, 2);
        let ind = 2 + opened;
        let fname = g.name_expr(&fi.xmlns, &fi.name);
        match fi.flags & F_MODE {
            F_CDATA | F_CHARDATA => {
                trim(g, &fi.parents, ind);
                g.enc_chardata(&fi.ty, &v, fi.flags & F_MODE == F_CDATA, ind)?;
            }
            F_COMMENT => {
                trim(g, &fi.parents, ind);
                g.enc_comment(&fi.ty, &v, &fi.field, ind)?;
            }
            F_INNERXML => {
                g.enc_inner(&fi.ty, &v, &fname, ind)?;
            }
            _ => {
                trim(g, &fi.parents, ind);
                if !fi.parents.is_empty() {
                    let cond = match g.kind(&fi.ty)? {
                        K::Opt(_) => format!(" && !{v}.none?"),
                        K::Dyn => format!(" && !{X}.dyn_nil?({v})"),
                        _ => String::new(),
                    };
                    g.w(ind, format!("if _ps.size < {}{cond} {{", fi.parents.len()));
                    g.w(ind + 1, format!("_ps = _e.~push_parents!(_ps, {})", str_list(&fi.parents)));
                    g.w(ind, "}");
                }
                g.enc_value(&fi.ty, &v, &fname, fi.flags & F_OMIT != 0, ind)?;
            }
        }
        g.close(2, opened);
    }
    if any_parents {
        g.w(2, "_ps = _e.~trim_parents!(_ps, [])");
    }
    g.w(2, "_e.~write_end!(_start.name)");
    g.w(2, "_e.~cached_error");
    Ok(())
}

fn str_list(xs: &[String]) -> String {
    if xs.is_empty() {
        return "[]".into();
    }
    format!("[{}]", xs.iter().map(|s| lit(s)).collect::<Vec<_>>().join(", "))
}

/// The default xml_dec! body: Go's unmarshal for a struct.
fn dec_struct(g: &mut Gen, tinfo: &TInfo, tname: &str) -> GR<()> {
    let X = g.x.to_string();
    if let Some(xn) = &tinfo.xmlname {
        if !xn.name.is_empty() || !xn.xmlns.is_empty() {
            g.w(2, format!("{X}.~wrong_element({}, {}, _start)", lit(&xn.xmlns), lit(&xn.name)));
        }
        if g.is_name_type(&xn.ty) {
            let xn2 = xn.clone();
            g.write_path(&xn2, 2, &mut |g, place, ind| {
                g.w(ind, format!("{place} = _start.name"));
                Ok(())
            })?;
        }
    }
    // Attributes
    let attrs: Vec<&FInfo> = tinfo.fields.iter().filter(|f| f.flags & F_MODE == F_ATTR).collect();
    let any_attr = tinfo.fields.iter().find(|f| f.flags & F_MODE == F_ANY | F_ATTR);
    if !attrs.is_empty() || any_attr.is_some() {
        g.w(2, "for _a in _start.attr {");
        g.w(3, "_h = false");
        for fi in attrs {
            let mut cond = format!("_a.name.local == {}", lit(&fi.name));
            if !fi.xmlns.is_empty() {
                cond.push_str(&format!(" && _a.name.space == {}", lit(&fi.xmlns)));
            }
            g.w(3, format!("if {cond} {{"));
            let fi2 = fi.clone();
            g.write_path(&fi2, 4, &mut |g, place, ind| g.dec_attr(&fi2.ty, place, "_a", ind))?;
            g.w(4, "_h = true");
            g.w(3, "}");
        }
        if let Some(fi) = any_attr {
            g.w(3, "if !_h {");
            let fi2 = fi.clone();
            g.write_path(&fi2, 4, &mut |g, place, ind| g.dec_attr(&fi2.ty, place, "_a", ind))?;
            g.w(3, "}");
        }
        g.w(3, "nil");
        g.w(2, "}");
    }
    let save_data = tinfo.fields.iter().find(|f| matches!(f.flags & F_MODE, F_CDATA | F_CHARDATA)).cloned();
    let save_comment = tinfo.fields.iter().find(|f| f.flags & F_MODE == F_COMMENT).cloned();
    let save_any = tinfo.fields.iter().find(|f| matches!(f.flags & F_MODE, F_ANY | 65)).cloned();
    let save_xml = tinfo.fields.iter().find(|f| f.flags & F_MODE == F_INNERXML).cloned();
    let xml_kind = match &save_xml {
        Some(fi) => match g.kind(&fi.ty)? {
            K::Str => 1,
            K::Bytes => 2,
            _ => 0,
        },
        None => 0,
    };
    if save_data.is_some() {
        g.w(2, "_data: [Byte] = []");
    }
    if save_comment.is_some() {
        g.w(2, "_comment: [Byte] = []");
    }
    if xml_kind != 0 {
        g.w(2, "_xi = _d.begin_inner!");
        g.w(2, "_xo = 0");
        g.w(2, "_xd: [Byte] = []");
    }
    g.w(2, "loop {");
    if xml_kind != 0 {
        g.w(3, "_xo = _d.saved_offset");
    }
    g.w(3, "_t = _d.~need_token!");
    g.w(3, "case _t {");
    g.w(4, "StartElement(_s) => {");
    g.w(5, "_c = self.~xml_path!(_d, [], _s)");
    if let Some(fi) = &save_any {
        g.w(5, "if !_c {");
        let fi2 = fi.clone();
        g.write_path(&fi2, 6, &mut |g, place, ind| g.dec_value(&fi2.ty, place, "_s", ind))?;
        g.w(6, "_c = true");
        g.w(5, "}");
    }
    g.w(5, "_d.~skip! if !_c");
    g.w(5, "nil");
    g.w(4, "}");
    g.w(4, "EndElement(_) => {");
    if xml_kind != 0 {
        g.w(5, "_xd = _d.end_inner!(_xi, _xo)");
    }
    g.w(5, "break");
    g.w(4, "}");
    if save_data.is_some() {
        g.w(4, "CharData(_b) => {");
        g.w(5, "_data = _data + _b");
        g.w(5, "nil");
        g.w(4, "}");
    }
    if save_comment.is_some() {
        g.w(4, "Comment(_b) => {");
        g.w(5, "_comment = _comment + _b");
        g.w(5, "nil");
        g.w(4, "}");
    }
    g.w(4, "_ => nil");
    g.w(3, "}");
    g.w(2, "}");
    if let Some(fi) = &save_data {
        let fi2 = fi.clone();
        g.write_path(&fi2, 2, &mut |g, place, ind| g.dec_text(&fi2.ty, place, "_data", &fi2.field, ind))?;
    }
    if let Some(fi) = &save_comment {
        let k = g.kind(&fi.ty)?;
        if matches!(k, K::Str | K::Bytes) {
            let fi2 = fi.clone();
            g.write_path(&fi2, 2, &mut |g, place, ind| {
                if matches!(k, K::Str) {
                    g.w(ind, format!("{place} = Str.from_bytes(_comment)"));
                } else {
                    g.w(ind, format!("{place} = _comment"));
                }
                Ok(())
            })?;
        }
    }
    if let Some(fi) = &save_xml {
        if xml_kind != 0 {
            let fi2 = fi.clone();
            g.write_path(&fi2, 2, &mut |g, place, ind| {
                if xml_kind == 1 {
                    g.w(ind, format!("{place} = Str.from_bytes(_xd)"));
                } else {
                    g.w(ind, format!("{place} = _xd"));
                }
                Ok(())
            })?;
        }
    }
    let _ = tname;
    g.w(2, "nil");
    Ok(())
}

/// xml_path!: Go's unmarshalPath over the element fields.
fn path_def(g: &mut Gen, tinfo: &TInfo) -> GR<()> {
    let X = g.x.to_string();
    g.w(1, format!("pub def xml_path!(_d: {X}.Decoder, _ps: [Str], _s: {X}.StartElement) -> ~Bool {{"));
    g.w(2, "_np = _ps.size");
    g.w(2, "_rec: [Str] = []");
    g.w(2, "_go = false");
    for fi in tinfo.fields.iter().filter(|f| f.flags & F_ELEMENT != 0) {
        let plen = fi.parents.len();
        let mut cond = String::from("!_go");
        if !fi.xmlns.is_empty() {
            cond.push_str(&format!(" && _s.name.space == {}", lit(&fi.xmlns)));
        }
        if plen == 0 {
            cond.push_str(&format!(" && _np == 0 && _s.name.local == {}", lit(&fi.name)));
            g.w(2, format!("if {cond} {{"));
            let fi2 = fi.clone();
            g.write_path(&fi2, 3, &mut |g, place, ind| g.dec_value(&fi2.ty, place, "_s", ind))?;
            g.w(3, "return true");
            g.w(2, "}");
            continue;
        }
        let pl = str_list(&fi.parents);
        cond.push_str(&format!(" && _np <= {plen} && _ps == {pl}[0..._np]"));
        g.w(2, format!("if {cond} {{"));
        g.w(3, format!("if _np == {plen} && _s.name.local == {} {{", lit(&fi.name)));
        let fi2 = fi.clone();
        g.write_path(&fi2, 4, &mut |g, place, ind| g.dec_value(&fi2.ty, place, "_s", ind))?;
        g.w(4, "return true");
        g.w(3, "}");
        g.w(3, format!("if _np < {plen} && {pl}[_np] == _s.name.local {{"));
        g.w(4, format!("_rec = {pl}[0..._np + 1]"));
        g.w(4, "_go = true");
        g.w(3, "}");
        g.w(2, "}");
    }
    g.w(2, "return false if !_go");
    g.w(2, "loop {");
    g.w(3, "_t = _d.~need_token!");
    g.w(3, "case _t {");
    g.w(4, "StartElement(_s2) => {");
    g.w(5, "_c = self.~xml_path!(_d, _rec, _s2)");
    g.w(5, "_d.~skip! if !_c");
    g.w(5, "nil");
    g.w(4, "}");
    g.w(4, "EndElement(_) => {");
    g.w(5, "return true");
    g.w(4, "}");
    g.w(4, "_ => nil");
    g.w(3, "}");
    g.w(2, "}");
    g.w(2, "true");
    g.w(1, "}");
    Ok(())
}

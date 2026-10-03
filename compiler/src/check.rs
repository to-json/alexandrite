//! Type checking and inference.
//!
//! Every function is checked once per distinct tuple of argument types
//! (monomorphization, Crystal-style). Inside a function, types are inferred
//! with unification. A local whose type is only fixed later in the body
//! (`found = []` ... `found << n`) is handled by re-checking: each pass
//! records the concrete types it learned per binding site and the next pass
//! starts from them. Only the final pass reports errors.

use crate::ast::*;
use crate::consts;
use crate::diag::{Diag, SourceMap, Span};
use crate::tast::*;
use std::collections::HashMap;

type R<T> = Result<T, Diag>;

pub struct ExternSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub fallible: bool,
    pub pure: bool,
    pub symbol: String,
    /// An `extern def` (a C function), not a separately compiled alexandrite package.
    pub ffi: bool,
}

/// The signature of an `extern def`: C-compatible parameter types and result.
pub fn ffi_sig(d: &Def) -> R<ExternSig> {
    fn scalar(t: &TypeExpr, ok_str: bool) -> R<Ty> {
        let bad = |sp: Span| Diag::new(sp, "an `extern def` takes and returns C types: Int, I8..I32, U8..U64, Float, Bool, Ptr, and (parameters only) Str and [Byte]");
        match t {
            TypeExpr::Named(n, sp) => match n.as_str() {
                "Float" | "F64" => Ok(Ty::Float),
                "Bool" => Ok(Ty::Bool),
                "Ptr" => Ok(Ty::Ptr),
                "Str" if ok_str => Ok(Ty::Str),
                "Str" => Err(Diag::new(*sp, "an `extern def` can't return a Str: return a Ptr and copy it with `Str.from_cstr(p)`")),
                n => IntKind::from_name(n).map(Ty::of_kind).ok_or_else(|| bad(*sp)),
            },
            TypeExpr::Array(e, sp) if ok_str => match &**e {
                TypeExpr::Named(n, _) if IntKind::from_name(n) == Some(IntKind::U8) => Ok(Ty::arr(Ty::IntK(IntKind::U8))),
                _ => Err(Diag::new(*sp, "an `extern def` takes only a [Byte] array (a pointer to its first element)")),
            },
            TypeExpr::Named(_, sp) | TypeExpr::Array(_, sp) | TypeExpr::Opt(_, sp) | TypeExpr::Fixed(_, _, sp) | TypeExpr::App(_, _, sp) | TypeExpr::Result(_, _, sp) | TypeExpr::Handle(_, _, sp) | TypeExpr::Fn(_, _, sp) | TypeExpr::Tuple(_, sp) => Err(bad(*sp)),
        }
    }
    let mut params = vec![];
    for p in &d.params {
        let Some(t) = &p.ty else {
            return Err(Diag::new(p.span, format!("`extern def {}`: parameter `{}` needs a type", d.name, p.name)));
        };
        params.push(scalar(t, true)?);
    }
    let ret = match &d.ret {
        None => Ty::Unit,
        Some(TypeExpr::Named(n, _)) if n == "Unit" => Ty::Unit,
        Some(t) => scalar(t, false)?,
    };
    Ok(ExternSig { params, ret, fallible: false, pure: false, symbol: d.ffi.clone().unwrap_or_default(), ffi: true })
}

pub struct DefInfo {
    pub def: Def,
    pub overflow: Overflow,
    pub external: Option<ExternSig>,
    /// The package it belongs to ("" = the main file).
    pub pkg: String,
}

thread_local! {
    /// The package whose code is being checked (names resolve there first).
    static PKG: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// Every package's imports (alias → path), and the public names.
    static USED: std::cell::RefCell<std::collections::HashSet<(String, String)>> = std::cell::RefCell::new(Default::default());
    static PKGS: std::cell::RefCell<(HashMap<String, HashMap<String, String>>, std::collections::HashSet<String>)> = std::cell::RefCell::new(Default::default());
}

pub fn set_packages(public: std::collections::HashSet<String>, imports: HashMap<String, HashMap<String, String>>) {
    USED.with(|u| u.borrow_mut().clear());
    PKGS.with(|p| *p.borrow_mut() = (imports, public));
}

/// Enter package `pkg`; returns the previous one, for `leave_pkg`.
pub fn enter_pkg(pkg: &str) -> String {
    PKG.with(|p| std::mem::replace(&mut *p.borrow_mut(), pkg.to_string()))
}
pub fn leave_pkg(prev: String) {
    PKG.with(|p| *p.borrow_mut() = prev);
}
pub fn current_pkg() -> String {
    PKG.with(|p| p.borrow().clone())
}

/// The package part of a qualified name (`geom.Point` → `geom`).
pub fn pkg_of(name: &str) -> String {
    match name.split_once('.') {
        Some((p, _)) if !p.chars().next().is_some_and(|c| c.is_uppercase()) => p.to_string(),
        _ => String::new(),
    }
}

/// The import path an alias names in the current package.
pub fn import_path(alias: &str) -> Option<String> {
    let pkg = current_pkg();
    let r = PKGS.with(|p| p.borrow().0.get(&pkg).and_then(|m| m.get(alias).cloned()));
    if r.is_some() {
        USED.with(|u| u.borrow_mut().insert((pkg, alias.to_string())));
    }
    r
}

/// Was import `alias` of package `pkg` used?
pub fn import_used(pkg: &str, alias: &str) -> bool {
    USED.with(|u| u.borrow().contains(&(pkg.to_string(), alias.to_string())))
}

pub fn is_public(q: &str) -> bool {
    PKGS.with(|p| p.borrow().1.contains(q))
}

/// Resolve a declared name as written in the current package: `alias.N`
/// through its imports (must be `pub`), else the package's own `N`, else a
/// main-file or builtin `N`.
pub fn resolve_name(n: &str, sp: Span, exists: &dyn Fn(&str) -> bool) -> R<String> {
    if let Some((alias, rest)) = n.split_once('.') {
        if let Some(path) = import_path(alias) {
            let q = format!("{path}.{rest}");
            if exists(&q) {
                if !is_public(&q) {
                    return Err(Diag::new(sp, format!("`{n}` isn't public; its package must declare it `pub`")));
                }
                return Ok(q);
            }
            return Err(Diag::new(sp, format!("package `{alias}` has no `{rest}`")));
        }
    }
    let pkg = current_pkg();
    if !pkg.is_empty() {
        let q = format!("{pkg}.{n}");
        if exists(&q) {
            return Ok(q);
        }
    }
    Ok(n.to_string())
}

pub struct World<'a> {
    pub sm: &'a SourceMap,
    pub defs: Vec<DefInfo>,
    by_name: HashMap<String, usize>,
    instances: HashMap<(usize, Vec<Ty>), FuncId>,
    pub funcs: Vec<Option<TFunc>>,
    /// Signatures of instances still being checked (for recursion).
    sigs: HashMap<FuncId, (Ty, bool, bool)>,
    /// An error inside a callee is final: retry passes of the caller must
    /// not swallow it.
    fatal: Option<Diag>,
    /// User structs by name, fields resolved.
    pub structs: Structs,
    /// Top-level constants: value and declared type (None = untyped, Go).
    pub consts: HashMap<String, (CVal, Option<Ty>)>,
    /// Interfaces: each method's name, parameter types (after self), return type, has a default.
    pub ifaces: HashMap<String, Vec<IfaceMethod>>,
    /// Each interface's implementors so far, with their method instances.
    pub impls: HashMap<String, Vec<(Ty, Vec<FuncId>)>>,
    pub stringers: HashMap<String, FuncId>,
    /// Error types (enums), in tag order: builtins first, then `error` decls.
    pub errors: Vec<Ty>,
    /// Warnings found so far (deduplicated by location).
    pub warnings: Vec<Diag>,
    /// Refinements by (qualified) name: each target type with its methods.
    pub refines: HashMap<String, Vec<(Ty, HashMap<String, usize>)>>,
    /// Array constants read in place (`MONTHS[i]`): name and value, by
    /// `M::Global` index. Set once, before the program runs; never written.
    pub globals: Vec<(String, TExpr)>,
    /// Constants whose values are enum variants (`pub SHA256 = Hash.SHA256`),
    /// or refer to such constants: evaluated once the types are known
    /// (`add_enum_consts`).
    pending_consts: Vec<ConstDef>,
}

#[derive(Clone, Debug)]
pub struct IfaceMethod {
    pub name: String,
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub default: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum CVal {
    Num(ConstVal),
    Str(String),
    Bool(bool),
    /// An array literal of constants (a slice or a fixed array: the
    /// constant's type says which, and is always known for these).
    Arr(Vec<CVal>),
    /// A variant without fields of an enum (`pub SHA256 = Hash.SHA256`):
    /// the enum's qualified name and the variant's index.
    Enum(String, usize),
}

/// The type of a constant's value: `want` if given (checking that the value
/// fits it), else the default (Int, Float, Str, Bool, and slices of those).
fn const_type(v: &CVal, want: Option<&Ty>, sp: Span) -> R<Ty> {
    match (v, want) {
        (CVal::Num(n), None) => Ok(if matches!(n, ConstVal::Float(_)) { Ty::Float } else { Ty::Int }),
        (CVal::Num(_), Some(Ty::Float)) => Ok(Ty::Float),
        (CVal::Num(n), Some(t)) if t.int_kind().is_some() => {
            let k = t.int_kind().unwrap();
            match consts::as_int(n) {
                Some(i) if consts::fit(&i, k).is_some() => Ok(t.clone()),
                Some(i) => Err(Diag::new(sp, format!("constant {i} overflows {}", k.name()))),
                None => Err(Diag::new(sp, format!("constant {} isn't an integer, so it can't be {}", consts::show(n), k.name()))),
            }
        }
        (CVal::Str(_), None | Some(Ty::Str)) => Ok(Ty::Str),
        (CVal::Bool(_), None | Some(Ty::Bool)) => Ok(Ty::Bool),
        (CVal::Arr(vs), Some(t @ Ty::Fixed(_, n))) if vs.len() as u64 != *n => Err(Diag::new(sp, format!("{} elements where {} needs {n}", vs.len(), t.show()))),
        (CVal::Arr(vs), Some(t @ (Ty::Array(el) | Ty::Fixed(el, _)))) => {
            for v in vs {
                const_type(v, Some(el), sp)?;
            }
            Ok(t.clone())
        }
        (CVal::Arr(vs), None) => {
            let Some(first) = vs.first() else {
                return Err(Diag::new(sp, "an empty array constant needs a type: `NAME: [T] = []`"));
            };
            // Untyped numbers join: an Int among Floats is a Float.
            let floats = vs.iter().all(|v| matches!(v, CVal::Num(_))) && vs.iter().any(|v| matches!(v, CVal::Num(ConstVal::Float(_))));
            let el = if floats { Ty::Float } else { const_type(first, None, sp)? };
            for v in vs {
                if const_type(v, Some(&el), sp).ok().as_ref() != Some(&el) {
                    return Err(Diag::new(sp, format!("an array constant's elements must have one type; this one has {} and {}", el.show(), const_type(v, None, sp)?.show())));
                }
            }
            Ok(Ty::arr(el))
        }
        (CVal::Enum(..), _) => Err(Diag::new(sp, "an array constant can't hold enum values")),
        (_, Some(t)) => Err(Diag::new(sp, format!("this constant isn't {}", t.show()))),
    }
}

/// A constant's value as a typed literal (`ty` from `const_type`). Each use
/// of an array constant as a value is a fresh array built from this.
pub fn const_lit(v: &CVal, ty: &Ty, sp: Span) -> TExpr {
    let kind = match (v, ty) {
        (CVal::Num(n), Ty::Float) => TK::Float(consts::to_f64(&match n {
            ConstVal::Int(i) => num_rational::BigRational::from_integer(i.clone()),
            ConstVal::Float(q) => q.clone(),
        })),
        (CVal::Num(n), t) => TK::Int(consts::fit(&consts::as_int(n).unwrap(), t.int_kind().unwrap()).unwrap()),
        (CVal::Str(s), _) => TK::Str(s.clone()),
        (CVal::Bool(b), _) => TK::Bool(*b),
        (CVal::Arr(vs), t) => {
            let el = t.arr_elem().unwrap();
            TK::Array(vs.iter().map(|v| const_lit(v, &el, sp)).collect())
        }
        (CVal::Enum(..), _) => unreachable!("enum constants aren't array elements"),
    };
    TExpr { kind, ty: ty.clone(), span: sp }
}

/// Values of this type share no mutable storage: an element read from a
/// constant array can be handed out as it is.
fn storage_free(t: &Ty) -> bool {
    matches!(t, Ty::Int | Ty::IntK(_) | Ty::Float | Ty::Bool | Ty::Str)
}

pub type Structs = HashMap<String, Ty>;
pub type Consts = HashMap<String, (CVal, Option<Ty>)>;

const PASSES: usize = 4;

impl<'a> World<'a> {
    pub fn new(sm: &'a SourceMap, defs: Vec<DefInfo>) -> R<Self> {
        let mut by_name = HashMap::new();
        for (i, d) in defs.iter().enumerate() {
            if by_name.insert(d.def.name.clone(), i).is_some() {
                return Err(Diag::new(d.def.name_span, format!("`{}` is defined twice", d.def.name)));
            }
        }
        GENERICS.with(|g| g.borrow_mut().clear());
        INSTS.with(|g| g.borrow_mut().clear());
        Ok(World { sm, defs, by_name, instances: HashMap::new(), funcs: vec![], sigs: HashMap::new(), fatal: None, structs: HashMap::new(), consts: HashMap::new(), ifaces: HashMap::new(), impls: HashMap::new(), stringers: HashMap::new(), errors: vec![], refines: HashMap::new(), warnings: vec![], globals: vec![], pending_consts: vec![] })
    }

    /// Evaluate top-level constants, in order (each may use earlier ones).
    pub fn add_consts(&mut self, defs: &[ConstDef]) -> R<()> {
        for d in defs {
            if self.consts.contains_key(&d.name) || self.structs.contains_key(&d.name) {
                return Err(Diag::new(d.span, format!("`{}` is already defined", d.name)));
            }
            // `Enum.Variant`, or a constant naming one: after the types.
            let deferred = match &d.value.kind {
                ExprKind::Call { recv: Some(r), args, block: None, .. } => args.is_empty() && matches!(r.kind, ExprKind::Const(_)),
                ExprKind::Const(c) => {
                    let prev = enter_pkg(&pkg_of(&d.name));
                    let q = resolve_name(c, d.value.span, &|q| self.pending_consts.iter().any(|p| p.name == q));
                    leave_pkg(prev);
                    q.is_ok_and(|q| self.pending_consts.iter().any(|p| p.name == q))
                }
                _ => false,
            };
            if deferred {
                self.pending_consts.push(d.clone());
                continue;
            }
            let prev = enter_pkg(&pkg_of(&d.name));
            let v = self.eval_const(&d.value);
            leave_pkg(prev);
            let v = v?;
            let prev = enter_pkg(&pkg_of(&d.name));
            let ty = match &d.ty {
                Some(te) if matches!(v, CVal::Arr(_)) => {
                    let t = type_from(te, &self.structs, &self.consts)?;
                    Some(const_type(&v, Some(&t), d.value.span)?)
                }
                None if matches!(v, CVal::Arr(_)) => Some(const_type(&v, None, d.value.span)?),
                Some(te) => {
                    let t = type_from(te, &self.structs, &self.consts)?;
                    let ok = match (&v, &t) {
                        (CVal::Num(_), Ty::Float) => true,
                        (CVal::Num(n), t) if t.int_kind().is_some() => {
                            let k = t.int_kind().unwrap();
                            match consts::as_int(n) {
                                Some(i) if consts::fit(&i, k).is_some() => true,
                                Some(i) => return Err(Diag::new(d.value.span, format!("constant {i} overflows {}", k.name()))),
                                None => false,
                            }
                        }
                        (CVal::Str(_), Ty::Str) | (CVal::Bool(_), Ty::Bool) => true,
                        _ => false,
                    };
                    if !ok {
                        return Err(Diag::new(d.value.span, format!("`{}` is declared {} but its value isn't", d.name, t.show())));
                    }
                    Some(t)
                }
                None => None,
            };
            leave_pkg(prev);
            self.consts.insert(d.name.clone(), (v, ty));
        }
        Ok(())
    }

    fn eval_const(&self, e: &Expr) -> R<CVal> {
        eval_const(&self.consts, e)
    }

    /// The constants `add_consts` deferred: enum variants, once the types
    /// are known.
    pub fn add_enum_consts(&mut self) -> R<()> {
        let pending = std::mem::take(&mut self.pending_consts);
        for d in &pending {
            let prev = enter_pkg(&pkg_of(&d.name));
            let ev = self.enum_const(&d.value);
            let want = d.ty.as_ref().map(|te| type_from(te, &self.structs, &self.consts));
            leave_pkg(prev);
            let (v, t) = match ev {
                Some(r) => r?,
                None => return Err(Diag::new(d.value.span, "a constant must be computable at compile time: literals, arrays of them, other constants, operators and enum variants without fields")),
            };
            if let Some(want) = want {
                let want = want?;
                if want != t {
                    return Err(Diag::new(d.value.span, format!("`{}` is declared {} but its value isn't", d.name, want.show())));
                }
            }
            self.consts.insert(d.name.clone(), (v, Some(t)));
        }
        Ok(())
    }

    /// `Enum.Variant` (a variant without fields) or another enum constant
    /// as a constant's value: its value and type. None: not one of those.
    fn enum_const(&self, e: &Expr) -> Option<R<(CVal, Ty)>> {
        match &e.kind {
            ExprKind::Call { recv: Some(r), name, args, block: None, .. } if args.is_empty() => {
                let ExprKind::Const(c) = &r.kind else { return None };
                let q = resolve_name(c, r.span, &|q| self.structs.contains_key(q)).ok()?;
                let t @ Ty::Enum(_, vs) = self.structs.get(&q)? else { return None };
                Some(match vs.iter().position(|(v, _)| v == name) {
                    Some(k) if vs[k].1.is_empty() => Ok((CVal::Enum(q.clone(), k), t.clone())),
                    Some(_) => Err(Diag::new(e.span, format!("`{c}.{name}` has fields: only a variant without fields can be a constant"))),
                    None => Err(Diag::new(e.span, format!("{c} has no variant `{name}`"))),
                })
            }
            ExprKind::Const(c) => {
                let q = resolve_name(c, e.span, &|q| self.consts.contains_key(q)).ok()?;
                match self.consts.get(&q)? {
                    (v @ CVal::Enum(..), Some(t)) => Some(Ok((v.clone(), t.clone()))),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The global holding array constant `name` (made on first use).
    fn const_global(&mut self, name: &str, v: &CVal, ty: &Ty) -> usize {
        if let Some(k) = self.globals.iter().position(|(n, _)| n == name) {
            return k;
        }
        self.globals.push((name.to_string(), const_lit(v, ty, Span::default())));
        self.globals.len() - 1
    }

    /// Declare structs and enums (in any order; a value type can't contain itself).
    pub fn add_structs(&mut self, defs: &[StructDef], enums: &[EnumDef]) -> R<()> {
        #[derive(Clone, Copy)]
        enum Def<'d> {
            S(&'d StructDef),
            E(&'d EnumDef),
        }
        let mut by_name: HashMap<&str, Def> = HashMap::new();
        for d in defs.iter().filter(|d| !d.tparams.is_empty()) {
            GENERICS.with(|g| g.borrow_mut().insert(d.name.clone(), GenDef::S(d.clone())));
        }
        for d in enums.iter().filter(|d| !d.tparams.is_empty()) {
            GENERICS.with(|g| g.borrow_mut().insert(d.name.clone(), GenDef::E(d.clone())));
        }
        DECLARED.with(|d| d.borrow_mut().extend(defs.iter().map(|x| x.name.clone()).chain(enums.iter().map(|x| x.name.clone()))));
        let defs: Vec<&StructDef> = defs.iter().filter(|d| d.tparams.is_empty()).collect();
        let enums: Vec<&EnumDef> = enums.iter().filter(|d| d.tparams.is_empty()).collect();
        let all = defs.iter().copied().map(|d| (d.name.as_str(), d.span, Def::S(d))).chain(enums.iter().copied().map(|d| (d.name.as_str(), d.span, Def::E(d))));
        for (name, span, d) in all {
            if by_name.contains_key(name) || self.structs.contains_key(name) || self.consts.contains_key(name) || ["Int", "Float", "Bool", "Str", "Array", "Map", "Math", "Test", "Enumerator", "Error", "Ptr", "F64"].contains(&name) {
                return Err(Diag::new(span, format!("`{name}` is already defined")));
            }
            by_name.insert(name, d);
        }
        fn fields_of(fs: &[(String, TypeExpr, Span)], by_name: &HashMap<&str, Def>, done: &mut Structs, visiting: &mut Vec<String>, consts: &Consts) -> R<Vec<(String, Ty)>> {
            let mut out = vec![];
            for (f, te, _) in fs {
                let mut names = vec![];
                type_names(te, &mut names);
                for n in names {
                    let q = resolve_name(&n, Span::default(), &|q| by_name.contains_key(q)).unwrap_or(n);
                    if by_name.contains_key(q.as_str()) {
                        let prev = enter_pkg(&pkg_of(&q));
                        let r = resolve(&q, by_name, done, visiting, consts);
                        leave_pkg(prev);
                        r?;
                    }
                }
                out.push((f.clone(), type_from(te, done, consts)?));
            }
            Ok(out)
        }
        fn resolve(name: &str, by_name: &HashMap<&str, Def>, done: &mut Structs, visiting: &mut Vec<String>, consts: &Consts) -> R<Ty> {
            if let Some(t) = done.get(name) {
                return Ok(t.clone());
            }
            let d = by_name[name];
            let span = match d {
                Def::S(s) => s.span,
                Def::E(e) => e.span,
            };
            if visiting.iter().any(|v| v == name) {
                return Err(Diag::new(span, format!("`{name}` contains itself; it is a value, so it can't (recursive types come with the memory model)")));
            }
            visiting.push(name.to_string());
            let prev = enter_pkg(&pkg_of(name));
            let t = match d {
                Def::S(s) => Ty::Struct(name.to_string(), fields_of(&s.fields, by_name, done, visiting, consts)?),
                Def::E(e) => {
                    let mut vs = vec![];
                    for (v, fs, _) in &e.variants {
                        vs.push((v.clone(), fields_of(fs, by_name, done, visiting, consts)?));
                    }
                    Ty::Enum(name.to_string(), vs)
                }
            };
            leave_pkg(prev);
            visiting.pop();
            done.insert(name.to_string(), t.clone());
            Ok(t)
        }
        let names: Vec<&str> = by_name.keys().copied().collect();
        for n in names {
            resolve(n, &by_name, &mut self.structs, &mut vec![], &self.consts)?;
        }
        for e in enums.iter().filter(|e| e.error) {
            let t = self.structs[&e.name].clone();
            self.errors.push(t);
        }
        Ok(())
    }

    /// Resolve refinement targets (after structs).
    pub fn add_refines(&mut self, defs: &[RefineDef]) -> R<()> {
        for r in defs {
            let prev = enter_pkg(&pkg_of(&r.name));
            let t = type_from(&r.target, &self.structs, &self.consts);
            leave_pkg(prev);
            let t = t?;
            let word = texpr_word(&r.target);
            let mut ms = HashMap::new();
            for m in &r.methods {
                ms.insert(m.clone(), self.by_name[&refine_def_name(&r.name, &word, m)]);
            }
            let entry = self.refines.entry(r.name.clone()).or_default();
            match entry.iter_mut().find(|(x, _)| *x == t) {
                Some((_, old)) => old.extend(ms),
                None => entry.push((t, ms)),
            }
        }
        Ok(())
    }

    /// The builtin error types.
    pub fn add_builtin_errors(&mut self) {
        let s = |v: &str| (v.to_string(), vec![("path".to_string(), Ty::Str)]);
        let builtins = [
            ("IoError", vec![s("NotFound"), s("Failed")]),
            ("ArithError", vec![("Overflow".to_string(), vec![]), ("DivZero".to_string(), vec![]), ("Domain".to_string(), vec![("message".to_string(), Ty::Str)])]),
            ("IndexError", vec![("OutOfBounds".to_string(), vec![]), ("SliceBounds".to_string(), vec![]), ("NoElement".to_string(), vec![("message".to_string(), Ty::Str)])]),
            ("Failure", vec![("Msg".to_string(), vec![("message".to_string(), Ty::Str)])]),
            ("TaskError", vec![("Panicked".to_string(), vec![("message".to_string(), Ty::Str)])]),
        ];
        for (n, vs) in builtins {
            let t = Ty::Enum(n.into(), vs);
            self.structs.insert(n.into(), t.clone());
            self.errors.push(t);
        }
    }

    /// The index of error type `t`, if it is one.
    pub fn error_index(&self, t: &Ty) -> Option<usize> {
        self.errors.iter().position(|e| e == t)
    }

    /// Instances of each error type's `message` method.
    pub fn message_instances(&mut self) -> R<HashMap<usize, FuncId>> {
        let mut out = HashMap::new();
        for (k, t) in self.errors.clone().iter().enumerate() {
            let Some(tn) = t.type_name() else { continue };
            if let Some(&def) = self.by_name.get(&method_name(tn, "message")) {
                let fid = self.instance(def, vec![t.clone()], Span::default())?;
                out.insert(k, fid);
            }
        }
        Ok(out)
    }

    /// Declare interface names (before structs, whose fields may use them).
    pub fn add_iface_names(&mut self, defs: &[IfaceDef]) -> R<()> {
        for d in defs {
            if self.structs.contains_key(&d.name) || self.consts.contains_key(&d.name) {
                return Err(Diag::new(d.span, format!("`{}` is already defined", d.name)));
            }
            self.structs.insert(d.name.clone(), Ty::Iface(d.name.clone()));
        }
        Ok(())
    }

    /// Resolve interface method signatures (after structs).
    pub fn add_iface_sigs(&mut self, defs: &[IfaceDef]) -> R<()> {
        for d in defs {
            let prev = enter_pkg(&pkg_of(&d.name));
            let r = self.add_iface_sig(d);
            leave_pkg(prev);
            r?;
        }
        Ok(())
    }

    fn add_iface_sig(&mut self, d: &IfaceDef) -> R<()> {
        {
            let mut ms = vec![];
            for (name, params, ret, default, span) in &d.methods {
                let mut ps = vec![];
                for p in params {
                    let Some(t) = &p.ty else {
                        return Err(Diag::new(p.span, format!("interface methods spell their parameter types: `{}: Type`", p.name)));
                    };
                    ps.push(type_from(t, &self.structs, &self.consts)?);
                }
                let ret = match ret {
                    Some(t) => type_from(t, &self.structs, &self.consts)?,
                    None => Ty::Unit,
                };
                ms.push(IfaceMethod { name: name.clone(), params: ps, ret, default: *default, span: *span });
            }
            self.ifaces.insert(d.name.clone(), ms);
        }
        Ok(())
    }

    /// Whether a value of type `t` contains (not behind a handle) a value
    /// of interface `iface`.
    fn contains_iface(&self, t: &Ty, iface: &str, seen: &mut Vec<String>) -> bool {
        match t {
            Ty::Iface(j) if j == iface => true,
            Ty::Iface(j) => {
                if seen.contains(j) {
                    return false;
                }
                seen.push(j.clone());
                let impls: Vec<Ty> = self.impls.get(j).map(|v| v.iter().map(|(t, _)| t.clone()).collect()).unwrap_or_default();
                impls.iter().any(|x| self.contains_iface(x, iface, seen))
            }
            Ty::Struct(_, fs) => fs.iter().any(|(_, f)| self.contains_iface(f, iface, seen)),
            Ty::Enum(_, vs) => vs.iter().any(|(_, fs)| fs.iter().any(|(_, f)| self.contains_iface(f, iface, seen))),
            Ty::Tuple(ts) => ts.iter().any(|x| self.contains_iface(x, iface, seen)),
            Ty::Opt(x) | Ty::Array(x) | Ty::Fixed(x, _) | Ty::Result(x) | Ty::Mutex(x) | Ty::Chan(x) => self.contains_iface(x, iface, seen),
            Ty::Map(k, v) => self.contains_iface(k, iface, seen) || self.contains_iface(v, iface, seen),
            _ => false,
        }
    }

    /// Make `t` an implementor of interface `iface` (checking it has every
    /// method), returning its tag.
    fn implement(&mut self, iface: &str, t: &Ty, sp: Span) -> R<usize> {
        if let Some(k) = self.impls.get(iface).and_then(|v| v.iter().position(|(x, _)| x == t)) {
            return Ok(k);
        }
        let Some(tn) = t.type_name().filter(|_| matches!(t, Ty::Struct(..) | Ty::Enum(..))) else {
            return Err(Diag::new(sp, format!("{} can't satisfy {iface}: only structs and enums have methods", t.show())));
        };
        let tn = tn.to_string();
        // An interface value holds its implementors by value: one that holds
        // the interface itself would be infinitely large.
        if self.contains_iface(t, iface, &mut vec![]) {
            return Err(Diag::new(sp, format!("{tn} can't be a {iface}: it holds a {iface} value itself, and an interface value contains its implementors")).note(format!("make {tn} generic over what it holds (`struct {tn}[T] {{ inner: T }}`, like Rust's BufReader<R>), or keep the inner value in a Pool and hold an @handle")));
        }
        let methods = self.ifaces[iface].clone();
        let impls = self.impls.entry(iface.to_string()).or_default();
        impls.push((t.clone(), vec![]));
        let k = impls.len() - 1;
        let mut fids = vec![];
        for m in &methods {
            let own = self.by_name.get(&method_name(&tn, &m.name)).copied();
            let def = match (own, m.default) {
                (Some(d), _) => d,
                (None, true) => self.by_name[&method_name(iface, &m.name)],
                (None, false) => {
                    self.impls.get_mut(iface).unwrap().pop();
                    return Err(Diag::new(sp, format!("{tn} doesn't satisfy {iface}: it has no method `{}`", m.name)).note(format!("{iface} needs: {}", methods.iter().filter(|m| !m.default).map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "))));
                }
            };
            let nparams = self.defs[def].def.params.len();
            if nparams != m.params.len() + 1 {
                self.impls.get_mut(iface).unwrap().pop();
                return Err(Diag::new(sp, format!("{tn}.{} takes {} argument(s), but {iface}.{} takes {}", m.name, nparams - 1, m.name, m.params.len())));
            }
            // A `!` method gets its receiver in a one-element slice.
            let me = if m.name.ends_with('!') { Ty::arr(t.clone()) } else { t.clone() };
            let args: Vec<Ty> = std::iter::once(me).chain(m.params.iter().cloned()).collect();
            let fid = self.instance(def, args, sp)?;
            let (ret, fallible) = match &self.funcs[fid] {
                Some(f) => (f.ret.clone(), f.fallible),
                None => self.sigs.get(&fid).map(|s| (s.0.clone(), s.1)).unwrap_or((Ty::Unit, false)),
            };
            // An infallible method satisfies a fallible interface method (it always succeeds).
            let lifted = !fallible && m.ret == Ty::Result(Box::new(ret.clone()));
            let ret = if fallible { Ty::Result(Box::new(ret)) } else { ret };
            // Covariant results: a `Digest` satisfies `def clone -> Hash` when
            // Digest is a Hash (the call through the interface wraps it).
            let covariant = match (&ret, &m.ret) {
                (Ty::Struct(..) | Ty::Enum(..), Ty::Iface(j)) => {
                    let j = j.clone();
                    self.implement(&j, &ret, sp).is_ok()
                }
                _ => false,
            };
            if ret != m.ret && !lifted && !covariant {
                self.impls.get_mut(iface).unwrap().pop();
                return Err(Diag::new(sp, format!("{tn}.{} returns {}, but {iface}.{} returns {}", m.name, ret.show(), m.name, m.ret.show())));
            }
            fids.push(fid);
        }
        self.impls.get_mut(iface).unwrap()[k].1 = fids;
        Ok(k)
    }

    /// Bind a def's type parameters for checking an instance with `args`:
    /// a generic type's method takes them from `self`; a generic def from
    /// its declared parameter types. Returns what to restore.
    pub fn bind(&mut self, def: usize, args: &[Ty], sp: Span) -> R<Vec<(String, Option<Ty>)>> {
        let d = &self.defs[def].def;
        let mut b: Vec<(String, Ty)> = vec![];
        if let (Some((owner, _)), Some(self_t)) = (d.name.rsplit_once('.'), args.first()) {
            if let Some(g) = generic(owner) {
                let st = self_t.arr_elem().filter(|_| d.name.ends_with('!')).unwrap_or_else(|| self_t.clone());
                if let Some((_, targs)) = inst_args(&st) {
                    b.extend(g.tparams().iter().map(|p| p.name.clone()).zip(targs));
                }
            }
        }
        if !d.tparams.is_empty() {
            let mut out = HashMap::new();
            for (p, t) in d.params.iter().zip(args) {
                if let Some(te) = &p.ty {
                    bind_tparams(te, t, &d.tparams, &mut out);
                }
            }
            // Parameters the arguments don't decide come after them, in order
            // (from the type the result is wanted as: `x: R[Int] = R.empty`).
            let mut extra = args.iter().skip(d.params.len());
            for tp in &d.tparams {
                if !out.contains_key(&tp.name) {
                    if let Some(t) = extra.next() {
                        out.insert(tp.name.clone(), t.clone());
                    }
                }
            }
            for tp in &d.tparams {
                let Some(t) = out.get(&tp.name) else {
                    return Err(Diag::new(tp.span, format!("can't infer `{}` from the arguments of `{}`; declare the type the result is wanted as", tp.name, d.name)));
                };
                check_bound(tp, t, sp)?;
                b.push((tp.name.clone(), t.clone()));
            }
            let tps = d.tparams.clone();
            for tp in &tps {
                if let Some(Bound::Iface(i)) = &tp.bound {
                    let t = out[&tp.name].clone();
                    // `io.Reader` through an import alias; `Reader` in its own package.
                    let prev = enter_pkg(&self.defs[def].pkg);
                    let key = match i.split_once('.') {
                        Some((alias, n)) => import_path(alias).map_or_else(|| i.clone(), |p| format!("{p}.{n}")),
                        None => resolve_name(i, tp.span, &|q| self.ifaces.contains_key(q)).unwrap_or_else(|_| i.clone()),
                    };
                    leave_pkg(prev);
                    if !self.ifaces.contains_key(&key) {
                        return Err(Diag::new(tp.span, format!("`{i}` isn't an interface")));
                    }
                    self.implement(&key, &t, sp)?;
                }
            }
        }
        Ok(b.into_iter().map(|(n, t)| {
            let old = self.structs.insert(n.clone(), t);
            (n, old)
        }).collect())
    }

    pub fn unbind(&mut self, saved: Vec<(String, Option<Ty>)>) {
        for (n, old) in saved.into_iter().rev() {
            match old {
                Some(t) => self.structs.insert(n, t),
                None => self.structs.remove(&n),
            };
        }
    }

    /// An interface default named `m` that a value of type `tn` can use.
    fn default_for(&self, tn: &str, m: &str) -> Option<usize> {
        for (iname, ms) in &self.ifaces {
            if ms.iter().any(|x| x.name == m && x.default) && ms.iter().filter(|x| !x.default).all(|x| self.by_name.contains_key(&method_name(tn, &x.name))) {
                return self.by_name.get(&method_name(iname, m)).copied();
            }
        }
        None
    }

    pub fn check_main(&mut self, main: &[Stmt], overflow: Overflow, span: Span) -> R<FuncId> {
        let id = self.funcs.len();
        self.funcs.push(None);
        let f = self.check_fn(id, None, &[], main, overflow, span)?;
        self.funcs[id] = Some(f);
        Ok(id)
    }

    /// Check every def with fully annotated (or no) parameters: the
    /// exported surface of a library.
    pub fn check_exports(&mut self) -> R<Vec<(String, FuncId)>> {
        let mut out = vec![];
        for i in 0..self.defs.len() {
            let d = &self.defs[i].def;
            if !d.public {
                continue;
            }
            let prev = enter_pkg(&self.defs[i].pkg);
            let mut tys = vec![];
            for p in &d.params {
                match &p.ty {
                    Some(t) => tys.push(type_from(t, &self.structs, &self.consts)?),
                    None => {
                        return Err(Diag::new(p.span, format!("cannot export `{}`: parameter `{}` needs a type (headers need explicit types)", d.name, p.name)));
                    }
                }
            }
            leave_pkg(prev);
            let name = d.name.clone();
            let id = self.instance(i, tys, d.name_span)?;
            out.push((name, id));
        }
        Ok(out)
    }

    fn instance(&mut self, def: usize, args: Vec<Ty>, call_span: Span) -> R<FuncId> {
        if let Some(id) = self.instances.get(&(def, args.clone())) {
            return Ok(*id);
        }
        let id = self.funcs.len();
        self.funcs.push(None);
        self.instances.insert((def, args.clone()), id);
        let info = &self.defs[def];
        if let Some(ext) = &info.external {
            let f = TFunc {
                cname: if ext.ffi { format!("f{}_{}", id, cname(&info.def.name)) } else { ext.symbol.clone() },
                src_name: info.def.name.clone(),
                params: (0..ext.params.len()).collect(),
                locals: ext.params.iter().enumerate().map(|(i, t)| Local { name: format!("p{i}"), ty: t.clone(), reassigned: 0, mutated: false, pushed: false, user: false }).collect(),
                ret: ext.ret.clone(),
                fallible: ext.fallible,
                pure: ext.pure,
                io: ext.ffi || !ext.pure,
                body: vec![],
                overflow: info.overflow,
                span: info.def.span,
                external: !ext.ffi,
                ffi: ext.ffi.then(|| ext.symbol.clone()),
                is_main: false,
                lambdas: vec![],
                lambda_info: vec![],
                errs: if ext.fallible { vec!["Error".into()] } else { vec![] },
            };
            self.funcs[id] = Some(f);
            return Ok(id);
        }
        let def_ast = info.def.clone();
        let overflow = info.overflow;
        let prev_pkg = enter_pkg(&info.pkg);
        let r = self.instance_in_pkg(id, def, args, def_ast, overflow, call_span);
        leave_pkg(prev_pkg);
        r
    }

    fn instance_in_pkg(&mut self, id: FuncId, def: usize, args: Vec<Ty>, def_ast: Def, overflow: Overflow, call_span: Span) -> R<FuncId> {
        let saved = match self.bind(def, &args, call_span) {
            Ok(s) => s,
            Err(e) => {
                self.instances.remove(&(def, args.clone()));
                return Err(e);
            }
        };
        if let Some(ret) = &def_ast.ret {
            let r = type_from(ret, &self.structs, &self.consts);
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    self.unbind(saved);
                    return Err(e);
                }
            };
            self.sigs.insert(id, (r, def_ast.fallible, def_ast.pure));
        }
        let f = self.check_fn(id, Some(&def_ast), &args, &def_ast.body, overflow, def_ast.span);
        self.unbind(saved);
        let f = match f {
            Ok(f) => f,
            Err(e) => {
                self.fatal.get_or_insert(e.clone());
                return Err(e);
            }
        };
        self.sigs.insert(id, (f.ret.clone(), f.fallible, f.pure));
        self.funcs[id] = Some(f);
        Ok(id)
    }

    fn check_fn(&mut self, id: FuncId, def: Option<&Def>, args: &[Ty], body: &[Stmt], overflow: Overflow, span: Span) -> R<TFunc> {
        let mut hints: HashMap<(NodeId, u32), Ty> = HashMap::new();
        let mut last_err = None;
        for pass in 0..PASSES {
            let strict = pass == PASSES - 1;
            let mut cx = FnCx::new(self, def, overflow, &hints, strict);
            let r = cx.run(def, args, body);
            let learned = cx.learned();
            let unresolved = cx.unresolved;
            drop(cx);
            if let Some(e) = &self.fatal {
                return Err(e.clone());
            }
            match r {
                Ok(mut f) if !unresolved => {
                    f.cname = match def {
                        Some(d) => format!("f{}_{}", id, cname(&d.name)),
                        None => "alx_main".into(),
                    };
                    f.span = span;
                    return Ok(f);
                }
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
            if strict {
                break;
            }
            let before = hints.len();
            hints.extend(learned);
            if hints.len() == before && pass > 0 && last_err.is_some() {
                // Nothing new learned: the next pass would fail the same way.
                let mut cx = FnCx::new(self, def, overflow, &hints, true);
                return match cx.run(def, args, body) {
                    Ok(f) if !cx.unresolved => Ok(f),
                    Ok(_) => Err(Diag::new(span, "could not infer all types in this function")),
                    Err(e) => Err(e),
                };
            }
        }
        Err(last_err.unwrap_or_else(|| Diag::new(span, "could not infer all types in this function")))
    }
}

pub fn eval_const(consts: &Consts, e: &Expr) -> R<CVal> {
    let num = |e: &Expr, v: CVal| match v {
        CVal::Num(n) => Ok(n),
        _ => Err(Diag::new(e.span, "expected a numeric constant")),
    };
    Ok(match &e.kind {
        ExprKind::Int(v) => CVal::Num(ConstVal::Int((*v).into())),
        ExprKind::BigInt(t) => CVal::Num(ConstVal::Int(t.parse().map_err(|_| Diag::new(e.span, "malformed integer"))?)),
        ExprKind::Float(v, t) => CVal::Num(consts::parse_float(t).map_or(ConstVal::Float(num_rational::BigRational::from_float(*v).unwrap_or_default()), ConstVal::Float)),
        ExprKind::Str(s) => CVal::Str(s.clone()),
        ExprKind::Bool(b) => CVal::Bool(*b),
        ExprKind::Const(c) => match consts.get(&resolve_name(c, e.span, &|q| consts.contains_key(q))?) {
            Some((v, _)) => v.clone(),
            None => return Err(Diag::new(e.span, format!("`{c}` isn't a constant defined above"))),
        },
        ExprKind::Neg(x) => CVal::Num(consts::neg(&num(x, eval_const(consts, x)?)?)),
        ExprKind::BitNot(x) => match num(x, eval_const(consts, x)?)? {
            ConstVal::Int(i) => CVal::Num(ConstVal::Int(-i - 1)),
            _ => return Err(Diag::new(e.span, "`^` needs an integer constant")),
        },
        ExprKind::Binary(op, a, b) => {
            let (x, y) = (num(a, eval_const(consts, a)?)?, num(b, eval_const(consts, b)?)?);
            match consts::fold(*op, &x, &y) {
                Ok(Some(v)) => CVal::Num(v),
                Ok(None) => return Err(Diag::new(e.span, format!("`{}` isn't a constant operation", op.text()))),
                Err(m) => return Err(Diag::new(e.span, m)),
            }
        }
        ExprKind::Call { recv: Some(r), name, args, block: None, .. } if name == "<<" && args.len() == 1 => {
            let (x, y) = (num(r, eval_const(consts, r)?)?, num(&args[0], eval_const(consts, &args[0])?)?);
            CVal::Num(consts::fold(BinOp::Shl, &x, &y).map_err(|m| Diag::new(e.span, m))?.unwrap())
        }
        ExprKind::Array(items) => CVal::Arr(items.iter().map(|x| eval_const(consts, x)).collect::<R<_>>()?),
        ExprKind::ArrayRepeat(v, n) => {
            let v = eval_const(consts, v)?;
            let n = match eval_const(consts, n)? {
                CVal::Num(n) => consts::as_int(&n).and_then(|i| usize::try_from(i).ok()).filter(|n| *n <= 1 << 20),
                _ => None,
            };
            let n = n.ok_or_else(|| Diag::new(e.span, "an array constant's length must be an integer constant from 0 to 1048576"))?;
            CVal::Arr(vec![v; n])
        }
        _ => return Err(Diag::new(e.span, "a constant must be computable at compile time: literals, arrays of them, other constants and operators")),
    })
}

pub fn cname(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '?' => o.push_str("_q"),
            '!' => o.push_str("_b"),
            '.' => o.push_str("__"),
            '+' => o.push_str("_add"),
            '-' => o.push_str("_sub"),
            '*' => o.push_str("_mul"),
            '/' => o.push_str("_div"),
            '%' => o.push_str("_rem"),
            '=' => o.push_str("_eq"),
            '<' => o.push_str("_lt"),
            '>' => o.push_str("_gt"),
            '[' => o.push_str("_idx"),
            ',' | ' ' => o.push('_'),
            ']' => {}
            '@' => o.push_str("_r_"),
            '#' => o.push_str("_m_"),
            c => o.push(c),
        }
    }
    o
}

/// Values `puts` and interpolation can show.
pub fn printable(t: &Ty) -> bool {
    match t {
        Ty::Int | Ty::IntK(_) | Ty::Float | Ty::Str | Ty::Bool | Ty::Var(_) | Ty::Iface(_) | Ty::Error | Ty::Handle(_) => true,
        Ty::Opt(t) | Ty::Array(t) | Ty::Fixed(t, _) => printable(t),
        Ty::Map(k, v) => printable(k) && printable(v),
        Ty::Struct(_, fs) => fs.iter().all(|(_, t)| printable(t)),
        Ty::Tuple(ts) => ts.iter().all(printable),
        _ => false,
    }
}

pub fn map_key_ok(t: &Ty) -> bool {
    matches!(t, Ty::Str | Ty::Bool | Ty::Var(_)) || t.int_kind().is_some()
}

fn type_names(t: &TypeExpr, out: &mut Vec<String>) {
    match t {
        TypeExpr::Named(n, _) => out.push(n.clone()),
        TypeExpr::Array(t, _) | TypeExpr::Opt(t, _) | TypeExpr::Fixed(t, _, _) => type_names(t, out),
        TypeExpr::App(_, ts, _) => ts.iter().for_each(|t| type_names(t, out)),
        // A handle names its type without containing it.
        TypeExpr::Handle(..) => {}
        TypeExpr::Fn(ps, r, _) => {
            ps.iter().for_each(|t| type_names(t, out));
            type_names(r, out);
        }
        TypeExpr::Result(t, _, _) => type_names(t, out),
        TypeExpr::Tuple(ts, _) => ts.iter().for_each(|t| type_names(t, out)),
    }
}

/// A generic struct or enum, instantiated on use.
#[derive(Clone)]
pub enum GenDef {
    S(StructDef),
    E(EnumDef),
}

impl GenDef {
    pub fn tparams(&self) -> &[TParam] {
        match self {
            GenDef::S(d) => &d.tparams,
            GenDef::E(d) => &d.tparams,
        }
    }
}

thread_local! {
    /// Generic types by name.
    pub static GENERICS: std::cell::RefCell<HashMap<String, GenDef>> = std::cell::RefCell::new(HashMap::new());
    /// Generic instances: `Stack[Int]` → (`Stack`, [Int]).
    pub static INSTS: std::cell::RefCell<HashMap<String, (String, Vec<Ty>)>> = std::cell::RefCell::new(HashMap::new());
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Every struct and enum name (for `@T` handles to types not resolved yet).
    pub static DECLARED: std::cell::RefCell<std::collections::HashSet<String>> = std::cell::RefCell::new(Default::default());
}

pub fn generic(n: &str) -> Option<GenDef> {
    GENERICS.with(|g| g.borrow().get(n).cloned())
}

/// The type arguments of a generic instance type (`Stack[Int]` → [Int]).
pub fn inst_args(t: &Ty) -> Option<(String, Vec<Ty>)> {
    let n = match t {
        Ty::Struct(n, _) | Ty::Enum(n, _) => n,
        _ => return None,
    };
    INSTS.with(|m| m.borrow().get(n).cloned())
}

/// `Name[args]` for a generic struct or enum.
pub fn instantiate(n: &str, g: &GenDef, targs: Vec<Ty>, sp: Span, structs: &Structs, consts: &Consts) -> R<Ty> {
    let tps = g.tparams();
    if tps.len() != targs.len() {
        return Err(Diag::new(sp, format!("`{n}` takes {} type argument(s), got {}", tps.len(), targs.len())));
    }
    for (tp, t) in tps.iter().zip(&targs) {
        check_bound(tp, t, sp)?;
    }
    if DEPTH.with(|d| d.get()) > 16 {
        return Err(Diag::new(sp, format!("`{n}` contains itself; it is a value, so it can't (recursive types come with the memory model)")));
    }
    let name = format!("{n}[{}]", targs.iter().map(Ty::show).collect::<Vec<_>>().join(", "));
    let mut env = structs.clone();
    for (tp, t) in tps.iter().zip(&targs) {
        env.insert(tp.name.clone(), t.clone());
    }
    DEPTH.with(|d| d.set(d.get() + 1));
    // The field types are the generic's package's names (`list.List[Int]`
    // made in another package still finds list's private `Node`).
    let prev = enter_pkg(&pkg_of(n));
    let fields = |fs: &[(String, TypeExpr, Span)]| fs.iter().map(|(f, te, _)| Ok((f.clone(), type_from(te, &env, consts)?))).collect::<R<Vec<_>>>();
    let r = match g {
        GenDef::S(d) => fields(&d.fields).map(|fs| Ty::Struct(name.clone(), fs)),
        GenDef::E(d) => d.variants.iter().map(|(v, fs, _)| Ok((v.clone(), fields(fs)?))).collect::<R<Vec<_>>>().map(|vs| Ty::Enum(name.clone(), vs)),
    };
    leave_pkg(prev);
    DEPTH.with(|d| d.set(d.get() - 1));
    let t = r?;
    INSTS.with(|m| m.borrow_mut().insert(name, (n.to_string(), targs)));
    Ok(t)
}

/// A type argument meets its parameter's bound (`like Int`; interfaces are
/// checked where the instance is made, by `World::bind`).
pub fn check_bound(tp: &TParam, t: &Ty, sp: Span) -> R<()> {
    match &tp.bound {
        Some(Bound::Like(k)) => {
            let ok = match k.as_str() {
                "Int" => t.int_kind().is_some() || matches!(t, Ty::Var(_)),
                "Float" => matches!(t, Ty::Float | Ty::Var(_)),
                _ => return Err(Diag::new(tp.span, format!("`like {k}`: only `like Int` and `like Float` are bounds"))),
            };
            if !ok {
                return Err(Diag::new(sp, format!("{} needs `{}` to be like {k}, not {}", tp.name, tp.name, t.show())));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Bind type parameters by matching a declared type against an actual one.
pub fn bind_tparams(te: &TypeExpr, t: &Ty, tps: &[TParam], out: &mut HashMap<String, Ty>) {
    match (te, t) {
        (TypeExpr::Named(n, _), t) if tps.iter().any(|p| p.name == *n) => {
            out.entry(n.clone()).or_insert_with(|| t.clone());
        }
        (TypeExpr::Array(e, _), Ty::Array(x)) | (TypeExpr::Opt(e, _), Ty::Opt(x)) | (TypeExpr::Fixed(e, _, _), Ty::Fixed(x, _)) => bind_tparams(e, x, tps, out),
        // A function argument binds through its parameter and result types:
        // `def new[H](h: () -> H)` takes H from `->() -> sha256.Digest { .. }`.
        (TypeExpr::Fn(ps, r, _), Ty::Fn(xs, y)) if ps.len() == xs.len() => {
            for (p, x) in ps.iter().zip(xs) {
                bind_tparams(p, x, tps, out);
            }
            bind_tparams(r, y, tps, out);
        }
        (TypeExpr::Result(e, _, _), Ty::Result(x)) => bind_tparams(e, x, tps, out),
        (TypeExpr::Tuple(es, _), Ty::Tuple(xs)) if es.len() == xs.len() => {
            for (e, x) in es.iter().zip(xs) {
                bind_tparams(e, x, tps, out);
            }
        }
        (TypeExpr::App(n, args, _), Ty::Map(k, v)) if n == "Map" && args.len() == 2 => {
            bind_tparams(&args[0], k, tps, out);
            bind_tparams(&args[1], v, tps, out);
        }
        (TypeExpr::Handle(n, args, _), Ty::Handle(h)) if !args.is_empty() => {
            if let Some((base, targs)) = INSTS.with(|m| m.borrow().get(h).cloned()) {
                if base == *n || base.ends_with(&format!(".{n}")) {
                    for (a, x) in args.iter().zip(&targs) {
                        bind_tparams(a, x, tps, out);
                    }
                }
            }
        }
        (TypeExpr::App(n, args, _), t) => {
            if let Some((base, targs)) = inst_args(t) {
                // `pkg.Name[T]` written in another package names the
                // instance's base qualified by its import path
                // (`net/textproto.Reader`); inside that package it is
                // written unqualified (`Reader[R]`).
                if base == *n || base.ends_with(&format!("/{n}")) || base.ends_with(&format!(".{n}")) {
                    for (a, x) in args.iter().zip(&targs) {
                        bind_tparams(a, x, tps, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Whether a generic first parameter written `te` can take a receiver of type
/// `t` for method sugar: the outer shape (slice, map, optional, function) must
/// agree, so `xs.equal(ys)` doesn't pick `maps.equal`. A bare type parameter
/// takes anything.
fn sugar_shape_fits(te: &TypeExpr, t: &Ty) -> bool {
    match te {
        TypeExpr::Array(..) => matches!(t, Ty::Array(_)),
        TypeExpr::Fixed(..) => matches!(t, Ty::Fixed(..)),
        TypeExpr::Opt(..) => matches!(t, Ty::Opt(_)),
        TypeExpr::Fn(..) => matches!(t, Ty::Fn(..)),
        TypeExpr::App(n, ..) if n == "Map" => matches!(t, Ty::Map(..)),
        TypeExpr::App(n, ..) if n == "Chan" => matches!(t, Ty::Chan(_)),
        _ => true,
    }
}

/// Does the current package declare type `n` itself?
fn in_current_pkg(n: &str, structs: &Structs) -> bool {
    let p = current_pkg();
    !p.is_empty() && structs.contains_key(&format!("{p}.{n}"))
}

/// Builtin type names a package may declare a type of its own with
/// (math/big's `Int` and `Float`). Inside that package the name means its
/// own type; the builtins stay reachable as `I64` and `F64`. (A main file
/// can't: its type names aren't qualified, so they would collide with the
/// builtins'. `alx test` checks such a package under its import path.)
pub const SHADOWABLE: [&str; 2] = ["Int", "Float"];

/// Does `n` name a builtin type that the current code shadows with its own?
pub fn shadowed(n: &str, structs: &Structs) -> bool {
    SHADOWABLE.contains(&n) && in_current_pkg(n, structs)
}

pub fn type_from(t: &TypeExpr, structs: &Structs, consts: &Consts) -> R<Ty> {
    let type_from = |t| type_from(t, structs, consts);
    match t {
        // A package's own type wins over a main-file type of the same name.
        TypeExpr::Named(n, sp) if shadowed(n, structs) || (!matches!(n.as_str(), "Float" | "F64" | "Bool" | "Str" | "Error" | "Unit" | "Ptr") && IntKind::from_name(n).is_none() && (!structs.contains_key(n) || in_current_pkg(n, structs))) => {
            let q = resolve_name(n, *sp, &|q| structs.contains_key(q) || generic(q).is_some())?;
            if generic(&q).is_some() && !structs.contains_key(&q) {
                return Err(Diag::new(*sp, format!("`{n}` is generic: give its type arguments (`{n}[...]`)")));
            }
            structs.get(&q).cloned().ok_or_else(|| Diag::new(*sp, format!("unknown type `{n}`")))
        }
        TypeExpr::Named(n, sp) => match n.as_str() {
            "Float" | "F64" => Ok(Ty::Float),
            "Bool" => Ok(Ty::Bool),
            "Str" => Ok(Ty::Str),
            "Error" => Ok(Ty::Error),
            "Unit" => Ok(Ty::Unit),
            "Ptr" => Ok(Ty::Ptr),
            _ => match IntKind::from_name(n) {
                Some(k) => Ok(Ty::of_kind(k)),
                None => structs.get(n).cloned().ok_or_else(|| Diag::new(*sp, format!("unknown type `{n}`"))),
            },
        },
        TypeExpr::Array(t, _) => Ok(Ty::arr(type_from(t)?)),
        TypeExpr::Result(t, _, _) => Ok(Ty::Result(Box::new(type_from(t)?))),
        TypeExpr::Handle(n, args, sp) if !args.is_empty() => {
            // `@Node[T]`: named like the instance (`Node[Int]`), which isn't
            // made here: a handle only names its type (the instance may be
            // the struct being made, `struct Node[T] { next: @Node[T]? }`).
            let g = resolve_name(n, *sp, &|q| generic(q).is_some())?;
            let Some(gd) = generic(&g) else {
                return Err(Diag::new(*sp, format!("`{n}` isn't a generic type")));
            };
            if gd.tparams().len() != args.len() {
                return Err(Diag::new(*sp, format!("`{n}` takes {} type argument(s), got {}", gd.tparams().len(), args.len())));
            }
            let targs = args.iter().map(type_from).collect::<R<Vec<_>>>()?;
            Ok(Ty::Handle(format!("{g}[{}]", targs.iter().map(Ty::show).collect::<Vec<_>>().join(", "))))
        }
        TypeExpr::Handle(n, _, sp) => {
            let q = resolve_name(n, *sp, &|q| structs.contains_key(q) || generic(q).is_some() || DECLARED.with(|d| d.borrow().contains(q)))?;
            if !(structs.contains_key(&q) || DECLARED.with(|d| d.borrow().contains(&q))) {
                return Err(Diag::new(*sp, format!("unknown type `{n}`")));
            }
            Ok(Ty::Handle(q))
        }
        TypeExpr::Fn(ps, r, _) => Ok(Ty::Fn(ps.iter().map(type_from).collect::<R<Vec<_>>>()?, Box::new(type_from(r)?))),
        TypeExpr::Tuple(ts, _) => Ok(Ty::Tuple(ts.iter().map(type_from).collect::<R<Vec<_>>>()?)),
        TypeExpr::Opt(t, _) => Ok(Ty::Opt(Box::new(type_from(t)?))),
        TypeExpr::App(n, args, sp) => match (n.as_str(), args.as_slice()) {
            ("Map", [k, v]) => {
                let kt = type_from(k)?;
                if !map_key_ok(&kt) {
                    return Err(Diag::new(*sp, format!("a Map key must be an integer type, Str or Bool, not {}", kt.show())));
                }
                Ok(Ty::Map(Box::new(kt), Box::new(type_from(v)?)))
            }
            ("Map", _) => Err(Diag::new(*sp, "`Map` takes two types: `Map[K, V]`")),
            ("Chan", [t]) => Ok(Ty::Chan(Box::new(type_from(t)?))),
            ("Pool", [t]) => Ok(Ty::Pool(Box::new(type_from(t)?))),
            ("Mutex", [t]) => Ok(Ty::Mutex(Box::new(type_from(t)?))),
            ("Atomic", [t]) => match type_from(t)? {
                t @ (Ty::Int | Ty::Bool) => Ok(Ty::Atomic(Box::new(t))),
                t => Err(Diag::new(*sp, format!("`Atomic` holds an Int or a Bool, not {}; guard other values with a `Mutex`", t.show()))),
            },
            ("Task", [t]) => Ok(Ty::Task(Box::new(type_from(t)?))),
            (g, _) if generic(&resolve_name(g, *sp, &|q| generic(q).is_some())?).is_some() => {
                let g = resolve_name(g, *sp, &|q| generic(q).is_some())?;
                let targs = args.iter().map(type_from).collect::<R<Vec<_>>>()?;
                instantiate(&g, &generic(&g).unwrap(), targs, *sp, structs, consts)
            }
            _ => Err(Diag::new(*sp, format!("unknown generic type `{n}`"))),
        },
        TypeExpr::Fixed(t, n, _) => {
            let el = type_from(t)?;
            let n_sp = n.span;
            let n = match eval_const(consts, n)? {
                CVal::Num(v) => consts::as_int(&v).and_then(|i| u64::try_from(i).ok()),
                _ => None,
            };
            let n = n.filter(|n| *n <= 1 << 32).ok_or_else(|| Diag::new(n_sp, "an array length must be a non-negative integer constant"))?;
            Ok(Ty::Fixed(Box::new(el), n))
        }
    }
}

struct FnCx<'w, 'a> {
    w: &'w mut World<'a>,
    locals: Vec<Local>,
    scopes: Vec<HashMap<String, LocalId>>,
    subst: Vec<Option<Ty>>,
    hints: &'w HashMap<(NodeId, u32), Ty>,
    sites: Vec<((NodeId, u32), Ty)>,
    strict: bool,
    unresolved: bool,
    /// The first constant that doesn't fit its final type (found while zonking).
    const_err: std::cell::RefCell<Option<Diag>>,
    pure_decl: bool,
    fn_name: String,
    is_main: bool,
    ret: Ty,
    /// Set when an impure operation is seen (reset per block to compute block purity).
    impure: bool,
    fallible_decl: bool,
    /// Depth of `try` directly enclosing the expression being checked.
    under_try: bool,
    /// The def being checked declares its error set (`~T<A | B>`).
    declared_errs: bool,
    /// `#![overflow(wrap)]`: arithmetic under `~` can't fail with ArithError.
    wrap: bool,
    /// The error set of the last fallible `!` call (its result is held in a Seq).
    mut_errs: Option<Vec<String>>,
    /// Kind of each enclosing loop-ish construct, innermost last.
    loops: Vec<LoopKind>,
    n_params: usize,
    /// Where each source-declared local was first assigned.
    decl_spans: Vec<(LocalId, Span)>,
    /// Active refinements: (scope depth where `using` appeared, name).
    usings: Vec<(usize, String)>,
    /// Error types this function can fail with ("Error" = any).
    errs: std::collections::BTreeSet<String>,
    /// Inside a lambda's body (no `~` there yet).
    in_lambda: bool,
    /// Inside a `lock` block: `self.loops`' length outside it (leaving the
    /// block early would keep the lock held).
    lock_floor: Option<usize>,
    /// The `!`-call cells of the function being checked, made empty on
    /// entry (None inside lambdas, task and generator bodies, which become
    /// functions of their own).
    cells: Option<Vec<(LocalId, Ty)>>,
    /// Lambda literals checked so far: (block span start, fn type, captures).
    lambdas: Vec<(u32, Ty, Vec<LocalId>)>,
    lambda_info: Vec<(u32, Vec<LocalId>, (usize, usize))>,
    /// The type the expression being checked is wanted as (for inferring
    /// a generic constructor's type arguments).
    want_hint: Option<Ty>,
    /// `R[Str].empty`: the type arguments a static method's call gives (consumed by `call_def`).
    owner_targs: Vec<Ty>,
    /// In a method: `Some(true)` for a `!` method (`self` is a one-element
    /// slice holding the receiver), `Some(false)` for one taking a copy.
    method: Option<bool>,
    /// The types this generic def's type parameters are bound to (by name):
    /// their methods are callable here even when not `pub`, since the caller
    /// handed the type over for exactly that (`heap.push(h, x)` calls the
    /// caller's `h.push!`).
    targ_types: Vec<String>,
    /// The next block checked has its value discarded (`each`, `step`).
    block_unused: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum LoopKind {
    While,
    Block,
    Gen,
}

/// Method names offered as suggestions, by receiver kind.
const SEQ_METHODS: &[&str] = &[
    "select", "filter", "reject", "find", "map", "flat_map", "take_while", "drop", "take", "each_with_index", "lazy", "sum", "max", "min", "max_by", "min_by",
    "first", "to_a", "each", "reduce", "inject", "all?", "any?", "count", "include?", "sort", "size", "length",
];
const ARRAY_EXTRA: &[&str] = &["each_index", "each_cons", "pmap", "last", "<<", "dup", "reverse"];
const INT_METHODS: &[&str] = &["to_s", "to_f", "to_i", "to_u8", "to_i32", "to_u32", "to_u64", "as_u8", "as_i32", "as_u32", "as_u64", "even?", "odd?", "digits", "step"];
const FLOAT_METHODS: &[&str] = &["to_s", "to_f", "to_i", "abs", "sqrt"];
/// `fmt` functions the compiler provides (std/fmt/fmt.alx documents them).
const FMT_BUILTINS: &[&str] = &["sprintf", "printf", "sprint", "sprintln", "print", "println", "errorf"];
const STR_METHODS: &[&str] = &["chars", "bytes", "runes", "size", "length", "reverse", "delete", "split", "to_i", "to_s", "strip", "lstrip", "rstrip", "lines", "start_with?", "end_with?", "include?", "byteindex"];

fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + (a[i - 1] != b[j - 1]) as usize);
        }
        prev = cur;
    }
    prev[b.len()]
}

impl<'w, 'a> FnCx<'w, 'a> {
    fn new(w: &'w mut World<'a>, def: Option<&Def>, _overflow: Overflow, hints: &'w HashMap<(NodeId, u32), Ty>, strict: bool) -> Self {
        let targ_types = def.map_or_else(Vec::new, |d| d.tparams.iter().filter_map(|tp| w.structs.get(&tp.name).and_then(|t| t.type_name().map(str::to_string))).collect());
        FnCx {
            w,
            locals: vec![],
            scopes: vec![HashMap::new()],
            subst: vec![],
            hints,
            sites: vec![],
            strict,
            unresolved: false,
            const_err: std::cell::RefCell::new(None),
            pure_decl: def.is_some_and(|d| d.pure),
            fn_name: def.map_or("main".into(), |d| d.name.clone()),
            want_hint: None,
            owner_targs: vec![],
            errs: Default::default(),
            decl_spans: vec![],
            usings: def.map_or_else(Vec::new, |d| d.using.iter().map(|u| (0, resolve_name(u, Span::default(), &|_| true).unwrap_or_else(|_| u.clone()))).collect()),
            in_lambda: false,
            lock_floor: None,
            cells: None,
            lambdas: vec![],
            lambda_info: vec![],
            method: def.filter(|d| d.params.first().is_some_and(|p| p.name == "self")).map(|d| d.name.ends_with('!')),
            block_unused: false,
            targ_types,
            is_main: def.is_none(),
            ret: Ty::Unit,
            impure: false,
            fallible_decl: def.is_some_and(|d| d.fallible),
            under_try: false,
            declared_errs: def.is_some_and(|d| d.errs.is_some()),
            wrap: _overflow == Overflow::Wrap,
            mut_errs: None,
            loops: vec![],
            n_params: 0,
        }
    }

    // ---------- types ----------

    fn fresh(&mut self) -> Ty {
        self.subst.push(None);
        Ty::Var(self.subst.len() as u32 - 1)
    }

    /// A type variable tied to a binding site: starts from what earlier
    /// passes learned there.
    fn site(&mut self, node: NodeId, slot: u32) -> Ty {
        if let Some(t) = self.hints.get(&(node, slot)) {
            return t.clone();
        }
        let v = self.fresh();
        self.sites.push(((node, slot), v.clone()));
        v
    }

    fn learned(&self) -> Vec<((NodeId, u32), Ty)> {
        self.sites.iter().map(|(k, t)| (*k, self.resolve(t))).filter(|(_, t)| !t.has_var()).collect()
    }

    fn resolve(&self, t: &Ty) -> Ty {
        match t {
            Ty::Var(v) => match &self.subst[*v as usize] {
                Some(t) => self.resolve(t),
                None => t.clone(),
            },
            Ty::Array(t) => Ty::arr(self.resolve(t)),
            Ty::Fixed(t, n) => Ty::Fixed(Box::new(self.resolve(t)), *n),
            Ty::Map(k, v) => Ty::Map(Box::new(self.resolve(k)), Box::new(self.resolve(v))),
            Ty::Fn(ps, r) => Ty::Fn(ps.iter().map(|t| self.resolve(t)).collect(), Box::new(self.resolve(r))),
            Ty::Result(t) => Ty::Result(Box::new(self.resolve(t))),
            Ty::Task(t) => Ty::Task(Box::new(self.resolve(t))),
            Ty::Pool(t) => Ty::Pool(Box::new(self.resolve(t))),
            Ty::Mutex(t) => Ty::Mutex(Box::new(self.resolve(t))),
            Ty::Atomic(t) => Ty::Atomic(Box::new(self.resolve(t))),
            Ty::Chan(t) => Ty::Chan(Box::new(self.resolve(t))),
            Ty::Seq(t, l) => Ty::seq(self.resolve(t), *l),
            Ty::Gen(t) => Ty::Gen(Box::new(self.resolve(t))),
            Ty::Opt(t) => Ty::Opt(Box::new(self.resolve(t))),
            Ty::Yielder(t) => Ty::Yielder(Box::new(self.resolve(t))),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve(t)).collect()),
            t => t.clone(),
        }
    }

    fn unify(&mut self, a: &Ty, b: &Ty) -> bool {
        let (a, b) = (self.resolve(a), self.resolve(b));
        match (&a, &b) {
            (Ty::Var(x), Ty::Var(y)) if x == y => true,
            (Ty::Var(x), t) | (t, Ty::Var(x)) => {
                if occurs(*x, t) {
                    return false;
                }
                self.subst[*x as usize] = Some(t.clone());
                true
            }
            (Ty::Never, _) | (_, Ty::Never) => true,
            (Ty::Array(x), Ty::Array(y)) | (Ty::Gen(x), Ty::Gen(y)) | (Ty::Yielder(x), Ty::Yielder(y)) | (Ty::Opt(x), Ty::Opt(y)) => self.unify(x, y),
            (Ty::Seq(x, _), Ty::Seq(y, _)) => self.unify(x, y),
            (Ty::Fixed(x, n), Ty::Fixed(y, m)) if n == m => self.unify(x, y),
            (Ty::Map(k1, v1), Ty::Map(k2, v2)) => self.unify(k1, k2) && self.unify(v1, v2),
            (Ty::Result(x), Ty::Result(y)) | (Ty::Task(x), Ty::Task(y)) | (Ty::Chan(x), Ty::Chan(y)) | (Ty::Pool(x), Ty::Pool(y)) | (Ty::Mutex(x), Ty::Mutex(y)) | (Ty::Atomic(x), Ty::Atomic(y)) => self.unify(x, y),
            (Ty::Fn(p1, r1), Ty::Fn(p2, r2)) if p1.len() == p2.len() => {
                let pairs: Vec<_> = p1.iter().cloned().zip(p2.iter().cloned()).collect();
                pairs.iter().all(|(x, y)| self.unify(x, y)) && self.unify(r1, r2)
            }
            (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                let pairs: Vec<_> = xs.iter().cloned().zip(ys.iter().cloned()).collect();
                pairs.iter().all(|(x, y)| self.unify(x, y))
            }
            _ => a == b,
        }
    }

    fn expect(&mut self, got: &Ty, want: &Ty, sp: Span, what: &str) -> R<()> {
        if self.unify(got, want) {
            return Ok(());
        }
        Err(Diag::new(sp, format!("{what}: expected {}, got {}", self.resolve(want).show(), self.resolve(got).show())))
    }

    /// Unknown type where a concrete one is needed: fine in early passes.
    fn unknown(&mut self, sp: Span, what: &str) -> R<Ty> {
        if self.strict {
            return Err(Diag::new(sp, format!("cannot infer the type of {what}; add a type annotation")));
        }
        self.unresolved = true;
        Ok(self.fresh())
    }

    // ---------- scopes ----------

    fn lookup(&self, name: &str) -> Option<LocalId> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn declare(&mut self, name: &str, ty: Ty) -> LocalId {
        self.locals.push(Local { name: name.to_string(), ty, reassigned: 0, mutated: false, pushed: false, user: false });
        let id = self.locals.len() - 1;
        self.scopes.last_mut().unwrap().insert(name.to_string(), id);
        id
    }

    // ---------- function ----------

    fn run(&mut self, def: Option<&Def>, args: &[Ty], body: &[Stmt]) -> R<TFunc> {
        let mut params = vec![];
        if let Some(d) = def {
            for (p, a) in d.params.iter().zip(args) {
                params.push(self.declare(&p.name, a.clone()));
            }
            self.n_params = params.len();
            self.ret = match &d.ret {
                Some(t) => type_from(t, &self.w.structs, &self.w.consts)?,
                None => self.fresh(),
            };
        }
        // Only a declared return type makes the last expression a value
        // that must exist; otherwise a trailing `case`/`if` whose arms
        // disagree is a statement, and the function returns nothing.
        self.cells = Some(vec![]);
        let r = self.body_as(body, def.is_some_and(|d| d.ret.is_some()));
        let cells = self.cells.take().unwrap_or_default();
        let (mut stmts, tail_ty) = r?;
        // The cells exist from the start, in the frame (a cell made inside a
        // loop would die with the iteration it was made in).
        if !cells.is_empty() {
            let mut init: Vec<TStmt> = cells
                .into_iter()
                .map(|(c, t)| {
                    let sp = def.map_or(Span::default(), |d| d.span);
                    let empty = self.mk(TK::Array(vec![]), t.clone(), sp);
                    TStmt::Expr(self.mk(TK::Assign(c, Box::new(empty)), t, sp))
                })
                .collect();
            init.append(&mut stmts);
            stmts = init;
        }
        if !self.is_main {
            let sp = def.map_or(Span::default(), |d| d.span);
            // The value of the last statement is the return value.
            if let Some(TStmt::Expr(e)) = stmts.last_mut() {
                let e2 = self.materialize(e.clone());
                let rt = self.ret.clone();
                let e2 = self.coerce(e2, &rt)?;
                *e = e2;
                let t = e.ty.clone();
                let rt = self.ret.clone();
                if !matches!(rt, Ty::Unit) && !self.unify(&t, &rt) {
                    return Err(Diag::new(e.span, format!("`{}` returns {}, but its last expression is {}", self.fn_name, self.resolve(&rt).show(), self.resolve(&t).show())));
                }
            } else {
                let rt = self.ret.clone();
                if !self.unify(&rt, &Ty::Unit) && !matches!(tail_ty, Ty::Never) {
                    return Err(Diag::new(sp, format!("`{}` must end with an expression of type {}", self.fn_name, self.resolve(&rt).show())));
                }
            }
        }
        let ret = self.resolve(&self.ret.clone());
        if ret.has_var() {
            self.unknown(def.map_or(Span::default(), |d| d.name_span), "this function's return value")?;
        }
        let locals: Vec<Local> = self.locals.iter().map(|l| Local { ty: self.resolve(&l.ty), ..l.clone() }).collect();
        for l in &locals {
            if l.ty.has_var() {
                self.unresolved = true;
                if self.strict {
                    return Err(Diag::new(def.map_or(Span::default(), |d| d.span), format!("cannot infer the type of `{}`; add a type annotation", l.name)));
                }
            }
        }
        let body = self.zonk_stmts(stmts);
        if let Some(e) = self.const_err.borrow_mut().take() {
            return Err(e);
        }
        let lambdas = self.lambdas.iter().map(|(lo, t, c)| (*lo, self.resolve(t), c.clone())).collect();
        // A declared error set must cover what the body can fail with.
        if let Some(Some(declared)) = def.map(|d| d.errs.clone()) {
            let declared: Vec<String> = declared.iter().map(|n| resolve_name(n, Span::default(), &|q| self.w.structs.contains_key(q)).unwrap_or_else(|_| n.clone())).collect();
            if !declared.iter().any(|e| e == "Error") {
                for e in &self.errs {
                    if !declared.contains(e) {
                        let d = def.unwrap();
                        return Err(Diag::new(d.name_span, format!("`{}` can fail with {e}, which its `~T<{}>` doesn't list", d.name, declared.join(" | "))).note("add it, or declare `~T<Error>` (any error)"));
                    }
                }
            }
        }
        let errs = self.errs.iter().cloned().collect();
        // Locals assigned but never read (Go's "declared and not used").
        let mut read = std::collections::HashSet::new();
        fn reads(e: &TExpr, out: &mut std::collections::HashSet<LocalId>) {
            match &e.kind {
                TK::Local(l) | TK::IndexAssign(l, ..) | TK::PlaceAssign(l, ..) => {
                    out.insert(*l);
                }
                _ => {}
            }
            if let TK::Select(arms, _) = &e.kind {
                for a in arms {
                    if let TSelArm::Recv { bind: Some(l), .. } = a {
                        out.insert(*l);
                    }
                }
            }
            if let TK::M(_, _, _, Some(b)) = &e.kind {
                for s in &b.body {
                    crate::prove::stmt_exprs(s, &mut |x| reads(x, out));
                }
            }
            crate::prove::each_child(e, &mut |x| reads(x, out));
        }
        for s in &body {
            crate::prove::stmt_exprs(s, &mut |x| reads(x, &mut read));
            if let TStmt::MultiAssign(ls, _) = s {
                let _ = ls;
            }
        }
        for (id, sp) in &self.decl_spans {
            let l = &self.locals[*id];
            if l.user && !read.contains(id) && !l.name.starts_with('_') && l.name != "self" {
                let d = Diag::new(*sp, format!("`{}` is assigned but never used", l.name)).note("use it, or name it `_` or `_name`");
                if !self.w.warnings.iter().any(|w| w.span == d.span) {
                    self.w.warnings.push(d);
                }
            }
        }
        Ok(TFunc {
            lambdas,
            lambda_info: self.lambda_info.clone(),
            errs,
            cname: String::new(),
            src_name: self.fn_name.clone(),
            params,
            locals,
            ret: if self.is_main { Ty::Unit } else { ret },
            fallible: self.fallible_decl,
            pure: self.pure_decl,
            io: self.impure,
            body,
            overflow: Overflow::Abort,
            span: Span::default(),
            external: false,
            ffi: None,
            is_main: self.is_main,
        })
    }

    /// Replace inference variables with their solutions throughout the tree.
    fn zonk_stmts(&self, ss: Vec<TStmt>) -> Vec<TStmt> {
        ss.into_iter().map(|s| self.zonk_stmt(s)).collect()
    }
    fn zonk_stmt(&self, s: TStmt) -> TStmt {
        match s {
            TStmt::Expr(e) => TStmt::Expr(self.zonk(e)),
            TStmt::MultiAssign(ls, es) => TStmt::MultiAssign(ls, es.into_iter().map(|e| self.zonk(e)).collect()),
            TStmt::While(c, b) => TStmt::While(self.zonk(c), self.zonk_stmts(b)),
            TStmt::If(c, a, b) => TStmt::If(self.zonk(c), self.zonk_stmts(a), self.zonk_stmts(b)),
            TStmt::Break(e, sp) => TStmt::Break(e.map(|e| self.zonk(e)), sp),
            TStmt::Return(e, sp) => TStmt::Return(e.map(|e| self.zonk(e)), sp),
            TStmt::Fail(e, sp) => TStmt::Fail(self.zonk(e), sp),
            TStmt::Defer(e) => TStmt::Defer(self.zonk(e)),
            s => s,
        }
    }
    fn zonk(&self, e: TExpr) -> TExpr {
        let mut ty = self.resolve(&e.ty);
        if let TK::Const(v) = &e.kind {
            // An untyped constant takes its final type here, and must fit it.
            if matches!(ty, Ty::Var(_)) {
                ty = if matches!(v, ConstVal::Float(_)) { Ty::Float } else { Ty::Int };
            }
            let kind = match ty.int_kind() {
                Some(k) => match consts::as_int(v).and_then(|i| consts::fit(&i, k).map(|b| (i, b))) {
                    Some((_, bits)) => TK::Int(bits),
                    None => {
                        let mut err = self.const_err.borrow_mut();
                        if err.is_none() {
                            *err = Some(match consts::as_int(v) {
                                Some(i) => Diag::new(e.span, format!("constant {i} overflows {}", k.name())).note(if k == IntKind::I64 { "it can still take part in constant arithmetic; only its final value must fit".to_string() } else { format!("{} holds {}..{}", k.name(), k.min(), k.max()) }),
                                None => Diag::new(e.span, format!("constant {} isn't an integer", consts::show(v))),
                            });
                        }
                        TK::Int(0)
                    }
                },
                None => TK::Float(consts::to_f64(&match v {
                    ConstVal::Int(i) => num_rational::BigRational::from_integer(i.clone()),
                    ConstVal::Float(q) => q.clone(),
                })),
            };
            return TExpr { kind, ty, span: e.span };
        }
        if let Ty::Map(k, _) = &ty {
            if !map_key_ok(k) {
                let mut err = self.const_err.borrow_mut();
                if err.is_none() {
                    *err = Some(Diag::new(e.span, format!("a Map key must be an integer type, Str or Bool, not {}", k.show())));
                }
            }
        }
        let b = |x: Box<TExpr>| Box::new(self.zonk(*x));
        let kind = match e.kind {
            TK::Assign(l, v) => TK::Assign(l, b(v)),
            TK::IndexAssign(l, i, v) => TK::IndexAssign(l, b(i), b(v)),
            TK::Bin(op, x, y) => TK::Bin(op, b(x), b(y)),
            TK::Neg(x) => TK::Neg(b(x)),
            TK::Not(x) => TK::Not(b(x)),
            TK::Ternary(c, x, y) => TK::Ternary(b(c), b(x), b(y)),
            TK::Range(x, y, ex) => TK::Range(b(x), b(y), ex),
            TK::Index(x, y) => TK::Index(b(x), b(y)),
            TK::Slice(x, lo, hi, ex) => TK::Slice(b(x), lo.map(b), hi.map(b), ex),
            TK::Call(f, args) => TK::Call(f, args.into_iter().map(|a| self.zonk(a)).collect()),
            TK::M(m, r, args, blk) => TK::M(
                m,
                r.map(b),
                args.into_iter().map(|a| self.zonk(a)).collect(),
                blk.map(|bl| Box::new(TBlock { body: self.zonk_stmts(bl.body), ..*bl })),
            ),
            TK::Try(x) => TK::Try(b(x)),
            TK::Puts(x) => TK::Puts(b(x)),
            TK::Panic(x) => TK::Panic(b(x)),
            TK::Array(xs) => TK::Array(xs.into_iter().map(|a| self.zonk(a)).collect()),
            TK::Format(ps, xs) => TK::Format(ps, xs.into_iter().map(|a| self.zonk(a)).collect()),
            TK::Seq(ss) => TK::Seq(self.zonk_stmts(ss)),
            TK::Some(x) => TK::Some(b(x)),
            TK::Select(arms, d) => TK::Select(
                arms.into_iter()
                    .map(|a| match a {
                        TSelArm::Recv { ch, bind, body } => TSelArm::Recv { ch: self.zonk(ch), bind, body: self.zonk_stmts(body) },
                        TSelArm::Send { ch, val, body } => TSelArm::Send { ch: self.zonk(ch), val: self.zonk(val), body: self.zonk_stmts(body) },
                    })
                    .collect(),
                d.map(|d| self.zonk_stmts(d)),
            ),
            TK::PlaceAssign(l, steps, op, v) => TK::PlaceAssign(
                l,
                steps
                    .into_iter()
                    .map(|st| match st {
                        TStep::Index(i) => TStep::Index(self.zonk(i)),
                        f => f,
                    })
                    .collect(),
                op,
                b(v),
            ),
            k => k,
        };
        TExpr { kind, ty, span: e.span }
    }

    // ---------- statements ----------

    /// Returns the statements and the type of the last one's value.
    fn body(&mut self, stmts: &[Stmt]) -> R<(Vec<TStmt>, Ty)> {
        self.body_as(stmts, true)
    }

    /// `used`: whether the last statement's value is required (otherwise it
    /// may be discarded, like every earlier statement's). A `case` or `if`
    /// whose value may be discarded can have arms of different types; it
    /// then has no value.
    fn body_as(&mut self, stmts: &[Stmt], used: bool) -> R<(Vec<TStmt>, Ty)> {
        let mut out = vec![];
        let mut last = Ty::Unit;
        for (i, s) in stmts.iter().enumerate() {
            let used = used && i + 1 == stmts.len();
            let t = match &s.kind {
                // A trailing `if ... else ...` is the block's value (Ruby).
                StmtKind::If(c, a, b) if i + 1 == stmts.len() && !b.is_empty() => TStmt::Expr(self.if_value(c, a, b, s.span, used)?),
                StmtKind::Expr(Expr { kind: ExprKind::Case(subject, arms), span, .. }) => TStmt::Expr(self.case(subject.as_deref(), arms, *span, used)?),
                StmtKind::Expr(Expr { kind: ExprKind::If(c, a, b), span, .. }) => TStmt::Expr(self.if_value(c, a, b, *span, used)?),
                _ => self.stmt(s)?,
            };
            last = match &t {
                TStmt::Expr(e) => e.ty.clone(),
                TStmt::Next(_) | TStmt::Break(..) | TStmt::Return(..) | TStmt::Fail(..) => Ty::Never,
                _ => Ty::Unit,
            };
            out.push(t);
        }
        Ok((out, last))
    }

    fn stmt(&mut self, s: &Stmt) -> R<TStmt> {
        Ok(match &s.kind {
            StmtKind::Expr(e) => {
                let te = self.expr(e)?;
                if let TK::M(M::Reverse, Some(r), ..) = &te.kind {
                    if matches!(self.resolve(&r.ty), Ty::Array(_)) {
                        let d = Diag::new(te.span, "`reverse` makes a new slice, and this one is dropped").note("`xs.reverse!` reverses in place");
                        self.w.warnings.push(d);
                    }
                }
                TStmt::Expr(te)
            }
            StmtKind::PlaceMultiAssign(targets, values) => {
                if targets.len() != values.len() {
                    return Err(Diag::new(s.span, format!("{} places but {} values", targets.len(), values.len())));
                }
                // Every value first (into hidden locals), then the stores.
                let mut out = vec![];
                let mut tmps = vec![];
                for v in values {
                    let tv = self.value(v)?;
                    let n = self.locals.len();
                    let name = format!("__multi{n}");
                    let id = self.declare(&name, tv.ty.clone());
                    out.push(TStmt::Expr(self.mk(TK::Assign(id, Box::new(tv.clone())), tv.ty.clone(), v.span)));
                    tmps.push(name);
                }
                for (t, name) in targets.iter().zip(&tmps) {
                    let rhs = Expr { id: NodeId::MAX, kind: ExprKind::Name(name.clone()), span: t.span };
                    let asg = Expr { id: NodeId::MAX, kind: ExprKind::Assign(Box::new(t.clone()), Box::new(rhs)), span: t.span };
                    let e = self.expr(&asg)?;
                    out.push(TStmt::Expr(e));
                }
                out.push(TStmt::Expr(self.mk(TK::Unit, Ty::Unit, s.span)));
                TStmt::Expr(self.mk(TK::Seq(out), Ty::Unit, s.span))
            }
            StmtKind::MultiAssign(targets, values) => {
                // `a, b = pair`: take a tuple apart.
                if values.len() == 1 && targets.len() > 1 {
                    let v = self.value(&values[0])?;
                    let Ty::Tuple(ts) = self.resolve(&v.ty) else {
                        return Err(Diag::new(values[0].span, format!("{} names but one value, a {} (only a tuple can be taken apart)", targets.len(), self.resolve(&v.ty).show())));
                    };
                    if ts.len() != targets.len() {
                        return Err(Diag::new(s.span, format!("{} names for a tuple of {}", targets.len(), ts.len())));
                    }
                    let tt = Ty::Tuple(ts.clone());
                    let n = self.locals.len();
                    let tmp = self.declare(&format!("_tuple{n}"), tt.clone());
                    let take = self.mk(TK::Assign(tmp, Box::new(v)), tt.clone(), s.span);
                    let mut ids = vec![];
                    let mut parts = vec![];
                    for (k, ((name, nsp), t)) in targets.iter().zip(&ts).enumerate() {
                        if name == "_" {
                            continue;
                        }
                        let whole = self.mk(TK::Local(tmp), tt.clone(), *nsp);
                        parts.push(self.mk(TK::M(M::TupleGet(k), Some(Box::new(whole)), vec![], None), t.clone(), *nsp));
                        ids.push(self.assign_local(name, *nsp, t, sp_node(nsp))?);
                    }
                    let unit = self.mk(TK::Unit, Ty::Unit, s.span);
                    return Ok(TStmt::Expr(self.mk(TK::Seq(vec![TStmt::Expr(take), TStmt::MultiAssign(ids, parts), TStmt::Expr(unit)]), Ty::Unit, s.span)));
                }
                if targets.len() != values.len() {
                    return Err(Diag::new(s.span, format!("{} names but {} values", targets.len(), values.len())));
                }
                let vals = values.iter().map(|v| self.value(v)).collect::<R<Vec<_>>>()?;
                let mut ids = vec![];
                for ((name, sp), v) in targets.iter().zip(&vals) {
                    ids.push(self.assign_local(name, *sp, &v.ty, sp_node(sp))?);
                }
                TStmt::MultiAssign(ids, vals)
            }
            StmtKind::Decl(name, nsp, te, e) => {
                let ty = type_from(te, &self.w.structs, &self.w.consts)?;
                let v = self.value_as(e, &ty)?;
                if !self.unify(&v.ty, &ty) {
                    return Err(Diag::new(e.span, format!("`{name}` is declared {}, but this is {}", ty.show(), self.resolve(&v.ty).show())));
                }
                let id = match self.lookup(name) {
                    Some(id) => {
                        let lt = self.locals[id].ty.clone();
                        if !self.unify(&lt, &ty) {
                            return Err(Diag::new(*nsp, format!("`{name}` is already {}", self.resolve(&lt).show())));
                        }
                        self.locals[id].reassigned += 1;
                        id
                    }
                    None => {
                        let id = self.declare(name, ty.clone());
                        self.locals[id].user = true;
                        self.decl_spans.push((id, *nsp));
                        id
                    }
                };
                TStmt::Expr(self.mk(TK::Assign(id, Box::new(v)), ty, s.span))
            }
            StmtKind::While(c, body) => {
                let opt = self.opt_let(c)?;
                let c = match &opt {
                    Some((cond, ..)) => cond.clone(),
                    None => self.cond(c)?,
                };
                self.loops.push(LoopKind::While);
                self.scopes.push(HashMap::new());
                let bind = opt.map(|(_, name, tmp, ty)| self.opt_bind(&name, tmp, &ty, s.span));
                let r = self.body_as(body, false);
                self.pop_scope();
                self.loops.pop();
                let (mut b, _) = r?;
                if let Some(bind) = bind {
                    b.insert(0, bind);
                }
                TStmt::While(c, b)
            }
            StmtKind::If(c, a, b) => {
                if let Some((cond, name, tmp, ty)) = self.opt_let(c)? {
                    self.scopes.push(HashMap::new());
                    let bind = self.opt_bind(&name, tmp, &ty, s.span);
                    let r = self.body_as(a, false);
                    self.pop_scope();
                    let (mut ta, _) = r?;
                    ta.insert(0, bind);
                    let (tb, _) = self.body_as(b, false)?;
                    return Ok(TStmt::If(cond, ta, tb));
                }
                let c = self.cond(c)?;
                let (a, _) = self.body_as(a, false)?;
                let (b, _) = self.body_as(b, false)?;
                TStmt::If(c, a, b)
            }
            StmtKind::Defer(e) => {
                let v = self.expr(e)?;
                TStmt::Defer(v)
            }
            StmtKind::Next => {
                self.not_leaving_lock(s.span, "next")?;
                if self.loops.is_empty() {
                    return Err(Diag::new(s.span, "`next` outside a loop or block"));
                }
                TStmt::Next(s.span)
            }
            StmtKind::Break(v) => {
                self.not_leaving_lock(s.span, "break")?;
                if self.loops.is_empty() {
                    return Err(Diag::new(s.span, "`break` outside a loop or block"));
                }
                TStmt::Break(v.as_ref().map(|v| self.value(v)).transpose()?, s.span)
            }
            StmtKind::Using(n) => {
                let q = resolve_name(n, s.span, &|q| self.w.refines.contains_key(q))?;
                if !self.w.refines.contains_key(&q) {
                    return Err(Diag::new(s.span, format!("no refinement `{n}`; declare it with `refine {n} for Type {{ ... }}`")));
                }
                self.usings.push((self.scopes.len(), q));
                TStmt::Expr(self.mk(TK::Unit, Ty::Unit, s.span))
            }
            StmtKind::Fail(v) => {
                if self.lock_floor.is_some() {
                    return Err(Diag::new(s.span, "`fail` inside a `lock` block would leave the lock held; return a value from the block and fail after it"));
                }
                if self.in_lambda && !self.fallible_decl {
                    return Err(Diag::new(s.span, "`fail` inside a lambda that isn't fallible: give it a `~T` result"));
                }
                if !self.is_main && !self.fallible_decl {
                    return Err(Diag::new(s.span, format!("`fail` in `{}`, which isn't fallible: declare it `-> ~T`", self.fn_name)));
                }
                let e = self.value(v)?;
                let e = self.to_error(e)?;
                TStmt::Fail(e, s.span)
            }
            StmtKind::Return(v) => {
                if self.lock_floor.is_some() {
                    return Err(Diag::new(s.span, "`return` inside a `lock` block would leave the lock held; return after it"));
                }
                if self.is_main {
                    return Err(Diag::new(s.span, "`return` at the top level"));
                }
                let rt = self.ret.clone();
                let v = v.as_ref().map(|v| self.value_as(v, &rt)).transpose()?;
                let v = v.map(|v| self.coerce(v, &rt)).transpose()?;
                let t = v.as_ref().map_or(Ty::Unit, |v| v.ty.clone());
                self.expect(&t, &rt, s.span, "return value")?;
                TStmt::Return(v, s.span)
            }
        })
    }

    fn cond(&mut self, c: &Expr) -> R<TExpr> {
        let e = self.value(c)?;
        if !self.unify(&e.ty, &Ty::Bool) {
            return Err(Diag::new(c.span, format!("condition must be Bool, got {}", self.resolve(&e.ty).show())));
        }
        Ok(e)
    }

    fn assign_local(&mut self, name: &str, sp: Span, ty: &Ty, _site: NodeId) -> R<LocalId> {
        if let Some(id) = self.lookup(name) {
            let lt = self.locals[id].ty.clone();
            if !self.unify(&lt, ty) {
                return Err(Diag::new(sp, format!("`{name}` is {}, cannot assign {}", self.resolve(&lt).show(), self.resolve(ty).show())));
            }
            self.locals[id].reassigned += 1;
            return Ok(id);
        }
        if let Some(Ty::Struct(_, fs)) = self.self_struct() {
            if self.lookup("self").is_some() && fs.iter().any(|(f, _)| f == name) {
                return Err(Diag::new(sp, format!("`{name}` is a field: write `self.{name} = ...` to set it (in a `!` method), or pick another name for a local")));
            }
        }
        let id = self.declare(name, ty.clone());
        self.locals[id].user = true;
        self.decl_spans.push((id, sp));
        Ok(id)
    }

    // ---------- expressions ----------

    /// An expression used as a value: pipelines are materialized.
    fn value(&mut self, e: &Expr) -> R<TExpr> {
        let t = self.expr(e)?;
        Ok(self.materialize(t))
    }

    fn materialize(&mut self, t: TExpr) -> TExpr {
        match self.resolve(&t.ty) {
            Ty::Seq(el, _) => TExpr { span: t.span, ty: Ty::arr(*el), kind: TK::M(M::ToA, Some(Box::new(t)), vec![], None) },
            _ => t,
        }
    }

    fn mk(&self, kind: TK, ty: Ty, span: Span) -> TExpr {
        TExpr { kind, ty, span }
    }

    fn expr(&mut self, e: &Expr) -> R<TExpr> {
        let sp = e.span;
        Ok(match &e.kind {
            ExprKind::Int(v) => self.mk(TK::Const(ConstVal::Int((*v).into())), Ty::Int, sp),
            ExprKind::BigInt(t) => self.mk(TK::Const(ConstVal::Int(t.parse().map_err(|_| Diag::new(sp, "malformed integer"))?)), Ty::Int, sp),
            ExprKind::Float(v, t) => match consts::parse_float(t) {
                Some(q) => self.mk(TK::Const(ConstVal::Float(q)), Ty::Float, sp),
                None => self.mk(TK::Float(*v), Ty::Float, sp),
            },
            ExprKind::Str(s) => self.mk(TK::Str(s.clone()), Ty::Str, sp),
            ExprKind::Tuple(items) => {
                let vals = items.iter().map(|x| self.value(x)).collect::<R<Vec<_>>>()?;
                let t = Ty::Tuple(vals.iter().map(|v| v.ty.clone()).collect());
                self.mk(TK::M(M::TupleNew, None, vals, None), t, sp)
            }
            ExprKind::Bool(b) => self.mk(TK::Bool(*b), Ty::Bool, sp),
            ExprKind::Nil => self.mk(TK::Unit, Ty::Unit, sp),
            ExprKind::Sym(s) => return Err(Diag::new(sp, format!("symbol `:{s}` can only be used as a block (`&:{s}`) or with `reduce`"))),
            ExprKind::Name(n) if n == "self" && self.method == Some(true) => {
                let id = self.lookup(n).expect("self is declared");
                let t = self.locals[id].ty.clone();
                let el = self.resolve(&t).arr_elem().expect("self is a [T]");
                let a = self.mk(TK::Local(id), t, sp);
                let z = self.mk(TK::Int(0), Ty::Int, sp);
                self.mk(TK::Index(Box::new(a), Box::new(z)), el, sp)
            }
            ExprKind::Name(n) => match self.lookup(n) {
                Some(id) => self.mk(TK::Local(id), self.locals[id].ty.clone(), sp),
                None => {
                    // A function passed as a value where one is wanted:
                    // `sort_func(xs, by_len)` is `sort_func(xs, ->(a, b) { by_len(a, b) })`.
                    // (A bare name of a function with parameters can't be a call.)
                    let q = resolve_name(n, sp, &|q| self.w.by_name.contains_key(q)).unwrap_or_else(|_| n.clone());
                    let def = self.w.by_name.get(&q).map(|&d| self.w.defs[d].def.clone());
                    // A field of the receiver shadows a function of the same name.
                    let def = def.filter(|_| self.self_field(e).is_none());
                    if let Some(d) = def.filter(|d| !d.params.is_empty() && d.tparams.is_empty() && d.params.iter().all(|p| p.ty.is_some())) {
                        {
                            let id = NodeId::MAX;
                            let names: Vec<String> = (0..d.params.len()).map(|i| format!("__arg{i}")).collect();
                            let call = Expr {
                                id,
                                kind: ExprKind::Call { recv: None, name: n.clone(), name_span: sp, args: names.iter().map(|a| Expr { id, kind: ExprKind::Name(a.clone()), span: sp }).collect(), block: None, block_sym: None },
                                span: sp,
                            };
                            let body = Block { id, params: names.iter().map(|a| (a.clone(), sp)).collect(), body: vec![Stmt { kind: StmtKind::Expr(call), span: sp }], span: sp };
                            let params = names.into_iter().zip(&d.params).map(|(name, p)| Param { name, ty: p.ty.clone(), span: sp }).collect();
                            let lam = Expr { id, kind: ExprKind::Lambda(params, d.ret.clone(), Box::new(body)), span: sp };
                            return self.expr(&lam);
                        }
                    }
                    return self.call(None, n, sp, &[], None, None, sp);
                }
            },
            ExprKind::Const(c) => match self.w.consts.get(&resolve_name(c, sp, &|q| self.w.consts.contains_key(q))?).cloned() {
                Some((v, ty)) => return self.const_value(v, ty, sp),
                None => return Err(Diag::new(sp, format!("`{c}` is not a value; call a method on it (`{c}.new`, ...)"))),
            },
            ExprKind::Call { recv: Some(r), name, args, block: None, block_sym: None, .. } if matches!(name.as_str(), "size" | "length") && args.is_empty() && self.const_array(r).is_some() => {
                let Some((_, CVal::Arr(vs), _)) = self.const_array(r) else { unreachable!() };
                self.mk(TK::Int(vs.len() as i64), Ty::Int, sp)
            }
            ExprKind::Call { recv, name, name_span, args, block, block_sym } => {
                return self.call(recv.as_deref(), name, *name_span, args, block.as_deref(), block_sym.as_ref(), sp);
            }
            ExprKind::Index(a, i) => {
                if let Some(r) = self.const_read(e)? {
                    return Ok(r);
                }
                let a = self.value(a)?;
                let at = self.resolve(&a.ty);
                if let Some(sn) = at.type_name() {
                    if let Some(&def) = self.w.by_name.get(&method_name(sn, "[]")) {
                        let k = self.value(i)?;
                        return self.call_def(def, "[]", sp, vec![a, k], sp);
                    }
                }
                // `t[0]`: a tuple's element (the index is a literal).
                if let Ty::Tuple(ts) = &at {
                    let k = match &i.kind {
                        ExprKind::Int(k) if (*k as usize) < ts.len() && *k >= 0 => *k as usize,
                        _ => return Err(Diag::new(i.span, format!("a tuple's index is a literal from 0 to {}", ts.len() - 1))),
                    };
                    let t = ts[k].clone();
                    return Ok(self.mk(TK::M(M::TupleGet(k), Some(Box::new(a)), vec![], None), t, sp));
                }
                if let Ty::Pool(t) = &at {
                    let h = self.value(i)?;
                    let want = Ty::Handle(t.show());
                    self.expect(&h.ty, &want, h.span, "pool handle")?;
                    return Ok(self.mk(TK::M(M::PoolGet, Some(Box::new(a)), vec![h], None), (**t).clone(), sp));
                }
                if let Ty::Map(kt, vt) = &at {
                    let k = self.value(i)?;
                    let k = self.coerce(k, kt)?;
                    self.expect(&k.ty, kt, k.span, "map key")?;
                    return Ok(self.mk(TK::M(M::MapGet, Some(Box::new(a)), vec![k], None), Ty::Opt(vt.clone()), sp));
                }
                if let ExprKind::SliceRange(lo, hi, excl) = &i.kind {
                    let ty = match at {
                        Ty::Array(t) | Ty::Fixed(t, _) => Ty::Array(t),
                        Ty::Str => Ty::Str,
                        Ty::Var(_) => self.unknown(a.span, "the sliced value")?,
                        t => return Err(Diag::new(a.span, format!("cannot slice {}", t.show()))),
                    };
                    let lo = lo.as_deref().map(|x| self.index_value(x)).transpose()?.map(Box::new);
                    let hi = hi.as_deref().map(|x| self.index_value(x)).transpose()?.map(Box::new);
                    return Ok(self.mk(TK::Slice(Box::new(a), lo, hi, *excl), ty, sp));
                }
                let i = self.index_value(i)?;
                let el = match at {
                    Ty::Array(t) | Ty::Fixed(t, _) => *t,
                    // Go: indexing a string gives a byte.
                    Ty::Str => Ty::IntK(IntKind::U8),
                    Ty::Var(_) => self.unknown(a.span, "the indexed value")?,
                    t => return Err(Diag::new(a.span, format!("cannot index into {}", t.show()))),
                };
                self.mk(TK::Index(Box::new(a), Box::new(i)), el, sp)
            }
            ExprKind::Lambda(params, ret, blk) => return self.lambda(params, ret.as_ref(), blk, sp),
            ExprKind::Spawn(blk) => return self.spawn(blk, sp),
            ExprKind::Select(arms, default) => return self.select(arms, default.as_deref(), sp),
            ExprKind::TypeApp(c, _) => return Err(Diag::new(sp, format!("`{c}[...]` is a type; call `.new` on it"))),
            ExprKind::SliceRange(..) => return Err(Diag::new(sp, "a range with an open end only works inside `[ ]`, to slice")),
            ExprKind::ArrayRepeat(v, n) => {
                let v = self.value(v)?;
                let n = self.index_value(n)?;
                let el = v.ty.clone();
                self.mk(TK::M(M::ArrayNew, None, vec![n, v], None), Ty::arr(el), sp)
            }
            ExprKind::Assign(target, _) | ExprKind::OpAssign(_, target, _) if self.is_map_index(target) => return self.map_assign(e),
            ExprKind::Assign(target, v) if matches!(&target.kind, ExprKind::Index(a, _) if matches!(self.peek_ty(a), Some(Ty::Pool(_)))) => {
                let ExprKind::Index(a, i) = &target.kind else { unreachable!() };
                let p = self.value(a)?;
                let Ty::Pool(t) = self.resolve(&p.ty) else { unreachable!() };
                let h = self.value(i)?;
                self.expect(&h.ty, &Ty::Handle(t.show()), h.span, "pool handle")?;
                let v = self.value_as(v, &t)?;
                self.expect(&v.ty, &t, v.span, "pool value")?;
                self.impure = true;
                return Ok(self.mk(TK::M(M::PoolSet, Some(Box::new(p)), vec![h, v], None), Ty::Unit, sp));
            }
            ExprKind::MapLit(pairs) => {
                let (kt, vt) = (self.fresh(), self.fresh());
                let mut args = vec![];
                for (k, v) in pairs {
                    let k = self.value(k)?;
                    let k = self.coerce(k, &kt)?;
                    self.expect(&k.ty, &kt, k.span, "map key")?;
                    let v = self.value(v)?;
                    let v = self.coerce(v, &vt)?;
                    self.expect(&v.ty, &vt, v.span, "map value")?;
                    args.push(k);
                    args.push(v);
                }
                let kr = self.resolve(&kt);
                if !map_key_ok(&kr) {
                    return Err(Diag::new(sp, format!("a Map key must be an integer type, Str or Bool, not {}", kr.show())));
                }
                self.mk(TK::M(M::MapNew, None, args, None), Ty::Map(Box::new(kt), Box::new(vt)), sp)
            }
            ExprKind::Assign(target, v) => match &target.kind {
                ExprKind::Name(n) if n == "self" && self.method.is_some() => {
                    let (id, steps, pty) = self.place(target)?;
                    let v = self.value(v)?;
                    let v = self.coerce(v, &pty)?;
                    self.expect(&v.ty, &pty, v.span, "assignment")?;
                    self.mk(TK::PlaceAssign(id, steps, None, Box::new(v)), pty, sp)
                }
                ExprKind::Name(n) => {
                    let v = self.value(v)?;
                    let v = match self.lookup(n) {
                        Some(id) => {
                            let lt = self.locals[id].ty.clone();
                            self.coerce(v, &lt)?
                        }
                        None => v,
                    };
                    let id = self.assign_local(n, target.span, &v.ty.clone(), target.id)?;
                    let ty = v.ty.clone();
                    self.mk(TK::Assign(id, Box::new(v)), ty, sp)
                }
                ExprKind::Index(arr, idx) if matches!(arr.kind, ExprKind::Name(_)) => {
                    let ExprKind::Name(an) = &arr.kind else { unreachable!() };
                    let Some(id) = self.lookup(an) else {
                        return Err(Diag::new(arr.span, format!("unknown local `{an}`")));
                    };
                    if self.pure_decl && id < self.param_count() {
                        return Err(Diag::new(sp, format!("`#[pure] def {}` can't mutate its argument `{an}`", self.fn_name)));
                    }
                    self.locals[id].mutated = true;
                    let idx = self.index_value(idx)?;
                    let v = self.value(v)?;
                    let el = match self.resolve(&self.locals[id].ty.clone()) {
                        Ty::Array(t) | Ty::Fixed(t, _) => *t,
                        Ty::Str => return Err(Diag::new(sp, "strings are immutable; build a new one (`bytes`, `Str.from_bytes`)")),
                        Ty::Var(_) => self.unknown(arr.span, "the array")?,
                        t => return Err(Diag::new(arr.span, format!("cannot assign into {}", t.show()))),
                    };
                    let v = self.coerce(v, &el)?;
                    self.expect(&v.ty, &el, v.span, "element assignment")?;
                    let ty = v.ty.clone();
                    self.mk(TK::IndexAssign(id, Box::new(idx), Box::new(v)), ty, sp)
                }
                _ => {
                    let (id, steps, pty) = self.place(target)?;
                    let v = self.value(v)?;
                    let v = self.coerce(v, &pty)?;
                    self.expect(&v.ty, &pty, v.span, "assignment")?;
                    self.mk(TK::PlaceAssign(id, steps, None, Box::new(v)), pty, sp)
                }
            },
            ExprKind::OpAssign(op, target, v) if !matches!(target.kind, ExprKind::Name(_)) => {
                let (id, steps, pty) = self.place(target)?;
                let cur = self.place_read(id, &steps, &pty, target.span);
                let rhs = self.value(v)?;
                let bin = self.binary(*op, cur, rhs, sp)?;
                if !self.unify(&bin.ty, &pty) {
                    return Err(Diag::new(sp, format!("`{}=` on a {} place gives {}", op.text(), self.resolve(&pty).show(), self.resolve(&bin.ty).show())));
                }
                let TK::Bin(_, _, rhs) = bin.kind else { unreachable!() };
                self.mk(TK::PlaceAssign(id, steps, Some(*op), rhs), pty, sp)
            }
            ExprKind::OpAssign(op, target, v) => {
                let ExprKind::Name(n) = &target.kind else { unreachable!() };
                let Some(id) = self.lookup(n) else {
                    if self.self_field(target).is_some() {
                        return Err(Diag::new(target.span, format!("`{n}` is a field: write `self.{n} {}= ...` (in a `!` method)", op.text())));
                    }
                    return Err(Diag::new(target.span, format!("`{n}` is not defined yet")));
                };
                let cur = self.mk(TK::Local(id), self.locals[id].ty.clone(), target.span);
                let rhs = self.value(v)?;
                let bin = self.binary(*op, cur, rhs, sp)?;
                let ty = bin.ty.clone();
                self.assign_local(n, target.span, &ty, target.id)?;
                self.mk(TK::Assign(id, Box::new(bin)), ty, sp)
            }
            ExprKind::Binary(op, l, r) => {
                let l = self.value(l)?;
                let r = self.value(r)?;
                return self.binary(*op, l, r, sp);
            }
            ExprKind::Neg(x) => {
                let x = self.value(x)?;
                if let TK::Const(v) = &x.kind {
                    let ty = x.ty.clone();
                    let e = self.mk(TK::Const(consts::neg(v)), ty.clone(), sp);
                    return self.coerce(e, &ty);
                }
                let t = self.resolve(&x.ty);
                if t == Ty::Float || t.int_kind().is_some() {
                    return Ok(self.mk(TK::Neg(Box::new(x)), t, sp));
                }
                self.expect(&x.ty, &Ty::Int, x.span, "negation")?;
                self.mk(TK::Neg(Box::new(x)), Ty::Int, sp)
            }
            ExprKind::BitNot(x) => {
                let x = self.value(x)?;
                if let TK::Const(ConstVal::Int(i)) = &x.kind {
                    let ty = x.ty.clone();
                    let e = self.mk(TK::Const(ConstVal::Int(-i - 1)), ty.clone(), sp);
                    return self.coerce(e, &ty);
                }
                let t = self.resolve(&x.ty);
                let Some(k) = t.int_kind() else {
                    return Err(Diag::new(x.span, format!("`^` needs an integer, got {}", t.show())));
                };
                // ^x = x XOR all-ones (of the type's width).
                let mask = if k.signed() || k == IntKind::U64 { -1 } else { k.max() as i64 };
                let m = self.mk(TK::Int(mask), t.clone(), sp);
                self.mk(TK::Bin(BinOp::BitXor, Box::new(x), Box::new(m)), t, sp)
            }
            ExprKind::Not(x) => {
                let x = self.value(x)?;
                self.expect(&x.ty, &Ty::Bool, x.span, "`!`")?;
                self.mk(TK::Not(Box::new(x)), Ty::Bool, sp)
            }
            ExprKind::Range(lo, hi, excl) => {
                let lo = self.value(lo)?;
                let hi = self.value(hi)?;
                self.expect(&lo.ty, &Ty::Int, lo.span, "range start")?;
                self.expect(&hi.ty, &Ty::Int, hi.span, "range end")?;
                self.mk(TK::Range(Box::new(lo), Box::new(hi), *excl), Ty::Range, sp)
            }
            ExprKind::Ternary(c, a, b) => {
                let c = self.cond(c)?;
                let a = self.value(a)?;
                let b = self.value(b)?;
                // An untyped constant takes the other branch's type (`r == 0 ? -1 : r`).
                let (ta, tb) = (self.resolve(&a.ty), self.resolve(&b.ty));
                let a = if matches!(a.kind, TK::Const(_)) && ta != tb { self.coerce(a.clone(), &tb).unwrap_or(a) } else { a };
                let b = if matches!(b.kind, TK::Const(_)) && ta != tb { self.coerce(b.clone(), &ta).unwrap_or(b) } else { b };
                let (a, b) = self.join(a, b)?;
                if !self.unify(&a.ty, &b.ty) {
                    return Err(Diag::new(sp, format!("branches have different types: {} and {}", self.resolve(&a.ty).show(), self.resolve(&b.ty).show())));
                }
                let ty = a.ty.clone();
                self.mk(TK::Ternary(Box::new(c), Box::new(a), Box::new(b)), ty, sp)
            }
            ExprKind::Try(x) => {
                if self.lock_floor.is_some() {
                    return Err(Diag::new(sp, "`~` inside a `lock` block could leave the lock held; handle the error inside (`rescue`, `unwrap_or`) or after the block"));
                }
                let saved = std::mem::replace(&mut self.under_try, true);
                self.mut_errs = None;
                let inner = self.value(x);
                self.under_try = saved;
                let inner = inner?;
                if self.in_lambda && !self.fallible_decl {
                    return Err(Diag::new(sp, "`~` inside a lambda that isn't fallible: give it a `~T` result (`->(x: Int) -> ~Int { ... }`), or handle the result (`unwrap_or`, `rescue`)"));
                }
                if !self.is_main && !self.fallible_decl {
                    return Err(Diag::new(sp, format!("`~` in `{}`, which isn't fallible: declare it `-> ~T`", self.fn_name)));
                }
                // What can fail under the `~`: the operation itself (a held
                // Result, a fallible call, File.read), and every builtin
                // under it that would otherwise panic (call arguments too).
                let mut errs = std::collections::BTreeSet::new();
                let mut mut_call = false;
                let held = if let Ty::Result(t) = self.resolve(&inner.ty) {
                    match (&inner.kind, self.mut_errs.take()) {
                        (TK::Seq(..), Some(es)) if !es.is_empty() => {
                            errs.extend(es);
                            mut_call = true;
                        }
                        _ => {
                            errs.insert("Error".to_string());
                        }
                    }
                    Some(*t)
                } else {
                    match &inner.kind {
                        TK::Call(f, _) if is_fallible_expr(&inner, self) => {
                            // (A call to a def still being checked, in a def with a declared set: the
                            // callee's failures are covered by the declaration, which is the contract.)
                            match &self.w.funcs[*f] {
                                Some(f) => errs.extend(f.errs.clone()),
                                None if self.declared_errs => {}
                                None => {
                                    errs.insert("Error".to_string());
                                }
                            }
                        }
                        TK::M(M::FileRead, ..) => {
                            errs.insert("IoError".into());
                        }
                        _ => {}
                    }
                    None
                };
                let fallible_op = held.is_some() || is_fallible_expr(&inner, self);
                if !mut_call {
                    let selfl = if self.method == Some(true) { self.lookup("self") } else { None };
                    try_faults(&inner, &mut errs, selfl);
                }
                if self.wrap && fallible_op {
                    errs.remove("ArithError");
                }
                if errs.is_empty() && !fallible_op {
                    return Err(Diag::new(sp, "`~` needs something that can fail: a fallible call, a `~T` value, arithmetic, indexing or slicing, or another builtin that can panic (`first`, `unwrap`, `to_u8`, ...)"));
                }
                self.errs.extend(errs);
                if let Some(t) = held {
                    return Ok(self.mk(TK::Try(Box::new(inner)), t, sp));
                }
                let ty = inner.ty.clone();
                self.mk(TK::Try(Box::new(inner)), ty, sp)
            }
            ExprKind::Interp(parts) => {
                let mut pieces = vec![];
                let mut vals = vec![];
                for p in parts {
                    match p {
                        InterpPart::Lit(s) => pieces.push(FmtPiece::Lit(s.clone())),
                        InterpPart::Expr(x) => {
                            let v = self.value(x)?;
                            let v = self.show_value(v)?;
                            let t = self.resolve(&v.ty);
                            if !printable(&t) {
                                return Err(Diag::new(x.span, format!("can't interpolate a {} yet", t.show())));
                            }
                            pieces.push(FmtPiece::Str(vals.len()));
                            vals.push(v);
                        }
                    }
                }
                self.mk(TK::Format(pieces, vals), Ty::Str, sp)
            }
            ExprKind::Case(subject, arms) => return self.case(subject.as_deref(), arms, sp, true),
            ExprKind::None => {
                let t = self.fresh();
                self.mk(TK::None, Ty::Opt(Box::new(t)), sp)
            }
            ExprKind::OptCall(call) => {
                let ExprKind::Call { recv: Some(recv), name, name_span, args, block, block_sym } = &call.kind else { unreachable!() };
                let o = self.value(recv)?;
                let ot = self.resolve(&o.ty);
                if !matches!(ot, Ty::Opt(_)) {
                    return Err(Diag::new(recv.span, format!("`?.` needs a T? on its left, but this is {}", ot.show())).note("use `.` for a value that is always there"));
                }
                let (tmp, pre) = self.opt_tmp(o, sp);
                let inner = self.opt_get(tmp, &ot, sp);
                let r = self.method(inner, name, *name_span, args, block.as_deref(), block_sym.as_ref(), sp)?;
                let rt = self.resolve(&r.ty);
                let (some, ty) = match rt {
                    Ty::Opt(_) => (r, rt),
                    t => {
                        let ty = Ty::Opt(Box::new(t));
                        (self.mk(TK::Some(Box::new(r)), ty.clone(), sp), ty)
                    }
                };
                let none = self.mk(TK::None, ty.clone(), sp);
                let present = self.opt_present(tmp, &ot, sp);
                let t = self.mk(TK::Ternary(Box::new(present), Box::new(some), Box::new(none)), ty.clone(), sp);
                self.mk(TK::Seq(vec![pre, TStmt::Expr(t)]), ty, sp)
            }
            ExprKind::If(c, a, b) => return self.if_value(c, a, b, sp, true),
            ExprKind::KwArg(n, nsp, _) => return Err(Diag::new(*nsp, format!("keyword argument `{n}:` outside `Struct.new`")).note("keyword arguments name struct fields: `Body.new(x: 1.0, mass: m)`")),
            ExprKind::Array(items) => {
                let el = self.site(e.id, 0);
                let mut out = vec![];
                for it in items {
                    let v = self.value(it)?;
                    self.expect(&v.ty, &el, v.span, "array element")?;
                    out.push(v);
                }
                self.mk(TK::Array(out), Ty::arr(el), sp)
            }
        })
    }

    /// An assignable place: a local, then `[i]` and `.field` steps.
    fn place(&mut self, e: &Expr) -> R<(LocalId, Vec<TStep>, Ty)> {
        if let Some(f) = self.self_field(e) {
            return self.place(&f);
        }
        match &e.kind {
            ExprKind::Name(n) if n == "self" && self.method.is_some() => {
                let id = self.lookup(n).expect("self is declared");
                if self.method == Some(false) {
                    let m = self.fn_name.rsplit('.').next().unwrap_or_default().to_string();
                    return Err(Diag::new(e.span, format!("`{m}` gets a copy of `self`; a method that changes it is named with `!` (`def {m}!`)")));
                }
                self.locals[id].mutated = true;
                let t = self.resolve(&self.locals[id].ty.clone());
                let el = t.arr_elem().expect("self is a [T]");
                let z = self.mk(TK::Int(0), Ty::Int, e.span);
                Ok((id, vec![TStep::Index(z)], el))
            }
            ExprKind::Name(n) => {
                let Some(id) = self.lookup(n) else {
                    return Err(Diag::new(e.span, format!("`{n}` is not defined yet")));
                };
                if self.pure_decl && id < self.param_count() {
                    return Err(Diag::new(e.span, format!("`#[pure] def {}` can't mutate its argument `{n}`", self.fn_name)));
                }
                self.locals[id].mutated = true;
                Ok((id, vec![], self.locals[id].ty.clone()))
            }
            ExprKind::Index(a, i) => {
                let (id, mut steps, t) = self.place(a)?;
                let el = match self.resolve(&t) {
                    Ty::Array(t) | Ty::Fixed(t, _) => *t,
                    Ty::Str => return Err(Diag::new(a.span, "strings are immutable; build a new one (`bytes`, `Str.from_bytes`)")),
                    Ty::Var(_) => self.unknown(a.span, "the array")?,
                    t => return Err(Diag::new(a.span, format!("cannot index into {}", t.show()))),
                };
                let i = self.index_value(i)?;
                steps.push(TStep::Index(i));
                Ok((id, steps, el))
            }
            ExprKind::Call { recv: Some(r), name, name_span, .. } => {
                let (id, mut steps, t) = self.place(r)?;
                let t = self.resolve(&t);
                let Some((k, ft)) = t.field(name) else {
                    return Err(match &t {
                        Ty::Var(_) => Diag::new(r.span, "cannot infer the type of this value; add a type annotation"),
                        Ty::Struct(sn, fs) => self.no_field(sn, fs, name, *name_span),
                        t => Diag::new(*name_span, format!("cannot assign to `.{name}` of {}", t.show())),
                    });
                };
                steps.push(TStep::Field(k));
                Ok((id, steps, ft))
            }
            ExprKind::Const(c) if self.const_array(e).is_some() => Err(Diag::new(e.span, format!("`{c}` is a constant and can't be changed")).note(format!("copy it into a variable to get an array of your own: `xs = {c}`"))),
            _ => Err(Diag::new(e.span, "cannot assign to this")),
        }
    }

    /// Reading a place, as an expression (for `place op= v`).
    fn place_read(&mut self, id: LocalId, steps: &[TStep], pty: &Ty, sp: Span) -> TExpr {
        let mut cur = self.mk(TK::Local(id), self.locals[id].ty.clone(), sp);
        for st in steps {
            let t = self.resolve(&cur.ty);
            cur = match st {
                TStep::Index(i) => {
                    let el = match t {
                        Ty::Array(e) | Ty::Fixed(e, _) => *e,
                        _ => pty.clone(),
                    };
                    self.mk(TK::Index(Box::new(cur), Box::new(i.clone())), el, sp)
                }
                TStep::Field(k) => {
                    let ft = match &t {
                        Ty::Struct(_, fs) => fs[*k].1.clone(),
                        _ => pty.clone(),
                    };
                    self.mk(TK::M(M::TupleGet(*k), Some(Box::new(cur)), vec![], None), ft, sp)
                }
            };
        }
        cur
    }

    fn no_field(&self, sn: &str, fs: &[(String, Ty)], name: &str, sp: Span) -> Diag {
        let mut msg = format!("`{sn}` has no field `{name}`");
        if let Some((best, _)) = fs.iter().filter(|(f, _)| lev(f, name) <= 2).min_by_key(|(f, _)| lev(f, name)) {
            msg.push_str(&format!("; did you mean `{best}`?"));
        } else {
            msg.push_str(&format!("; its fields are {}", fs.iter().map(|(f, _)| format!("`{f}`")).collect::<Vec<_>>().join(", ")));
        }
        Diag::new(sp, msg)
    }

    /// A constant where a type is wanted takes that type (Go's untyped
    /// constants), and must fit it. Non-constants are never converted:
    /// an Int variable needs `.to_f` or `.to_u8`.
    fn coerce(&mut self, e: TExpr, want: &Ty) -> R<TExpr> {
        let want = self.resolve(want);
        // `c ? 1 : 0` where a U64 is wanted: constant branches take the type
        // (Go's untyped constants, through the conditional).
        if let TK::Ternary(..) = &e.kind {
            if self.resolve(&e.ty) != want && (want.int_kind().is_some() || want == Ty::Float) {
                let sp = e.span;
                let TK::Ternary(c, a, b) = e.kind else { unreachable!() };
                let a = self.coerce(*a, &want)?;
                let b = self.coerce(*b, &want)?;
                let ty = if self.resolve(&a.ty) == want && self.resolve(&b.ty) == want { want.clone() } else { a.ty.clone() };
                return Ok(self.mk(TK::Ternary(c, Box::new(a), Box::new(b)), ty, sp));
            }
        }
        // A tuple literal coerces element by element (`(1, "a")` as `(U8, Str)`).
        if let (TK::M(M::TupleNew, None, items, None), Ty::Tuple(ts)) = (&e.kind, &want) {
            if items.len() == ts.len() && self.resolve(&e.ty) != want {
                let sp = e.span;
                let mut vs = vec![];
                for (x, t) in items.clone().into_iter().zip(ts.clone()) {
                    vs.push(self.coerce(x, &t)?);
                }
                let ty = Ty::Tuple(vs.iter().map(|v| v.ty.clone()).collect());
                return Ok(self.mk(TK::M(M::TupleNew, None, vs, None), ty, sp));
            }
        }
        // An array literal coerces element by element (`[128, 65]` as `[Byte]`,
        // structs as `[Iface]`).
        if let (TK::Array(items), Ty::Array(et)) = (&e.kind, &want) {
            if self.resolve(&e.ty) != want {
                let sp = e.span;
                let mut vs = vec![];
                for x in items.clone() {
                    vs.push(self.coerce(x, et)?);
                }
                if vs.iter().all(|v| self.resolve(&v.ty) == **et) {
                    return Ok(self.mk(TK::Array(vs), want.clone(), sp));
                }
            }
        }
        if want == Ty::Error {
            let et = self.resolve(&e.ty);
            if let Some(k) = self.w.error_index(&et) {
                let sp = e.span;
                return Ok(self.mk(TK::M(M::ToError(k), Some(Box::new(e)), vec![], None), Ty::Error, sp));
            }
        }
        // A struct or enum where an interface is wanted is wrapped (Go's implicit conversion).
        if let Ty::Iface(iname) = &want {
            let et = self.resolve(&e.ty);
            if matches!(et, Ty::Struct(..) | Ty::Enum(..)) && !et.has_var() {
                let k = self.w.implement(iname, &et, e.span)?;
                let sp = e.span;
                return Ok(self.mk(TK::M(M::ToIface(k), Some(Box::new(e)), vec![], None), want.clone(), sp));
            }
        }
        // A T where a T? is wanted is present.
        if let Ty::Opt(inner) = &want {
            let et = self.resolve(&e.ty);
            if !matches!(et, Ty::Opt(_) | Ty::Var(_)) {
                let e = self.coerce(e, inner)?;
                if self.unify(&e.ty, inner) {
                    let sp = e.span;
                    return Ok(self.mk(TK::Some(Box::new(e)), want.clone(), sp));
                }
                return Ok(e);
            }
        }
        if let (TK::M(M::MapNew, None, ..), Ty::Map(kt, vt)) = (&e.kind, &want) {
            let TK::M(_, _, args, _) = e.kind else { unreachable!() };
            let (kt, vt) = ((**kt).clone(), (**vt).clone());
            let mut out = vec![];
            for (j, x) in args.into_iter().enumerate() {
                let t = if j % 2 == 0 { &kt } else { &vt };
                let x = self.coerce(x, t)?;
                self.expect(&x.ty, t, x.span, if j % 2 == 0 { "map key" } else { "map value" })?;
                out.push(x);
            }
            return Ok(self.mk(TK::M(M::MapNew, None, out, None), want, e.span));
        }
        if let Ty::Fixed(el, n) = &want {
            // A literal (`[1, 2, 3]`, `[0; 4]`) of the right length becomes a fixed array.
            let len = match &e.kind {
                TK::Array(items) => Some(items.len() as u64),
                TK::M(M::ArrayNew, None, a, _) => match &a[0].kind {
                    TK::Const(ConstVal::Int(i)) => u64::try_from(i.clone()).ok(),
                    TK::Int(i) => u64::try_from(*i).ok(),
                    _ => return Err(Diag::new(e.span, format!("a {} needs a constant length here", want.show()))),
                },
                _ => None,
            };
            if let Some(len) = len {
                if len != *n {
                    return Err(Diag::new(e.span, format!("{len} elements where {} needs {n}", want.show())));
                }
                let el = (**el).clone();
                let kind = match e.kind {
                    TK::Array(items) => {
                        let items = items.into_iter().map(|x| self.coerce(x, &el)).collect::<R<Vec<_>>>()?;
                        for x in &items {
                            self.expect(&x.ty, &el, x.span, "array element")?;
                        }
                        TK::Array(items)
                    }
                    TK::M(m, None, mut a, b) => {
                        let fill = a.pop().unwrap();
                        let fill = self.coerce(fill, &el)?;
                        self.expect(&fill.ty, &el, fill.span, "array element")?;
                        a.push(fill);
                        TK::M(m, None, a, b)
                    }
                    _ => unreachable!(),
                };
                return Ok(self.mk(kind, want, e.span));
            }
        }
        if let (TK::Array(_), Ty::Array(el)) = (&e.kind, &want) {
            let TK::Array(items) = e.kind else { unreachable!() };
            let el = (**el).clone();
            let items = items.into_iter().map(|x| self.coerce(x, &el)).collect::<R<Vec<_>>>()?;
            for x in &items {
                self.expect(&x.ty, &el, x.span, "array element")?;
            }
            return Ok(self.mk(TK::Array(items), want, e.span));
        }
        let TK::Const(v) = &e.kind else { return Ok(e) };
        match &want {
            Ty::Float => {
                let q = match v {
                    ConstVal::Int(i) => num_rational::BigRational::from_integer(i.clone()),
                    ConstVal::Float(q) => q.clone(),
                };
                Ok(self.mk(TK::Const(ConstVal::Float(q)), Ty::Float, e.span))
            }
            t if t.int_kind().is_some() => {
                let k = t.int_kind().unwrap();
                let Some(i) = consts::as_int(v) else {
                    return Err(Diag::new(e.span, format!("constant {} isn't an integer, so it can't be {}", consts::show(v), k.name())));
                };
                if consts::fit(&i, k).is_none() {
                    return Err(Diag::new(e.span, format!("constant {i} overflows {}", k.name())).note(format!("{} holds {}..{}", k.name(), k.min(), k.max())));
                }
                Ok(self.mk(TK::Const(ConstVal::Int(i)), want.clone(), e.span))
            }
            _ => Ok(e),
        }
    }

    /// Go's zero value of a type, as an expression.
    fn zero_of(&mut self, t: &Ty, sp: Span) -> Option<TExpr> {
        let kind = match t {
            Ty::Int | Ty::IntK(_) => TK::Int(0),
            Ty::Opt(_) => TK::None,
            Ty::Float => TK::Float(0.0),
            Ty::Bool => TK::Bool(false),
            Ty::Str => TK::Str(String::new()),
            Ty::Array(_) => TK::Array(vec![]),
            Ty::Map(..) => TK::M(M::MapNew, None, vec![], None),
            Ty::Pool(_) => TK::M(M::PoolNew, None, vec![], None),
            Ty::Enum(_, vs) => {
                // The first variant, with zero fields.
                let mut slots = vec![];
                for (_, fs) in vs.clone() {
                    for (_, ft) in fs {
                        slots.push(self.zero_of(&ft, sp)?);
                    }
                }
                TK::M(M::VariantNew(0), None, slots, None)
            }
            Ty::Tuple(ts) => {
                let vals = ts.clone().iter().map(|t| self.zero_of(t, sp)).collect::<Option<Vec<_>>>()?;
                TK::M(M::TupleNew, None, vals, None)
            }
            Ty::Fixed(el, n) => {
                let z = self.zero_of(el, sp)?;
                TK::M(M::ArrayNew, None, vec![self.mk(TK::Int(*n as i64), Ty::Int, sp), z], None)
            }
            Ty::Struct(_, fs) => {
                let vals = fs.clone().iter().map(|(_, ft)| self.zero_of(ft, sp)).collect::<Option<Vec<_>>>()?;
                TK::M(M::StructNew, None, vals, None)
            }
            _ => return None,
        };
        Some(self.mk(kind, t.clone(), sp))
    }

    /// `case`: lowered to a chain of conditionals over a temporary.
    /// `used`: the case's value is required. If not (a statement), its arms
    /// may be of different types (it then has no value), as an `if`'s may.
    fn case(&mut self, subject: Option<&Expr>, arms: &[CaseArm], sp: Span, used: bool) -> R<TExpr> {
        let mut pre = vec![];
        let subj = match subject {
            Some(s) => {
                let v = self.value(s)?;
                let ty = v.ty.clone();
                let id = self.declare(&format!("_case{}", sp.lo), ty.clone());
                pre.push(TStmt::Expr(self.mk(TK::Assign(id, Box::new(v)), ty.clone(), sp)));
                Some((id, ty))
            }
            None => None,
        };
        let mut conds: Vec<(TExpr, TExpr)> = vec![];
        let mut default: Option<TExpr> = None;
        let enum_ty = subj.as_ref().map(|(_, t)| self.resolve(t)).filter(|t| matches!(t, Ty::Enum(..)));
        let mut covered: Vec<bool> = match &enum_ty {
            Some(Ty::Enum(_, vs)) => vec![false; vs.len()],
            _ => vec![],
        };
        for arm in arms {
            if arm.pats.is_empty() {
                if default.is_some() {
                    return Err(Diag::new(arm.span, "a second `_` arm can never match"));
                }
                default = Some(self.arm_body_with(vec![], &arm.body, arm.span, used)?);
                continue;
            }
            if default.is_some() {
                return Err(Diag::new(arm.span, "this arm comes after `_`, so it can never match"));
            }
            let mut cond: Option<TExpr> = None;
            let mut binds: Vec<(String, Ty, TExpr)> = vec![];
            for pat in &arm.pats {
                let local = |cx: &mut Self| subj.as_ref().map(|(id, ty)| cx.mk(TK::Local(*id), ty.clone(), arm.span));
                let c = match pat {
                    Pat::Variant(full, bs, vsp) => {
                        // `Type.Variant`: the type part narrows (and checks) it.
                        let (tq, vn) = match full.rsplit_once('.') {
                            Some((t, v)) => (Some(t.to_string()), v.to_string()),
                            None => (None, full.clone()),
                        };
                        let vn = &vn;
                        let type_is = |t: &Ty, q: &str| t.type_name().is_some_and(|n| n == q || n.ends_with(&format!(".{q}")));
                        if let (Some(q), Some(et)) = (&tq, &enum_ty) {
                            if !type_is(et, q) {
                                return Err(Diag::new(*vsp, format!("`{full}`: the case is over {}, not {q}", et.show())));
                            }
                        }
                        let variant = match &enum_ty {
                            Some(Ty::Enum(en, vs)) => match vs.iter().position(|(v, _)| v == vn) {
                                Some(k) => Some((k, vs[k].1.clone())),
                                None => return Err(Diag::new(*vsp, format!("`{vn}` is not a variant of {en}; its variants are {}", vs.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>().join(", ")))),
                            },
                            _ => None,
                        };
                        match variant {
                            Some((k, fields)) => {
                                let et = enum_ty.clone().unwrap();
                                covered[k] = true;
                                let l = local(self).unwrap();
                                let tag = self.mk(TK::M(M::EnumTag, Some(Box::new(l)), vec![], None), Ty::Int, *vsp);
                                let kv = self.mk(TK::Int(k as i64), Ty::Int, *vsp);
                                if let Some(bs) = bs {
                                    if bs.len() != fields.len() {
                                        return Err(Diag::new(*vsp, format!("`{vn}` has {} field(s) ({}), the pattern binds {}", fields.len(), fields.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>().join(", "), bs.len())));
                                    }
                                    if arm.pats.len() > 1 {
                                        return Err(Diag::new(*vsp, "an arm with alternatives (`|`) can't bind fields"));
                                    }
                                    let base = et.enum_slot(k);
                                    for (j, ((b, _), (_, ft))) in bs.iter().zip(&fields).enumerate() {
                                        if b != "_" {
                                            let l = local(self).unwrap();
                                            let get = self.mk(TK::M(M::TupleGet(base + j), Some(Box::new(l)), vec![], None), ft.clone(), *vsp);
                                            binds.push((b.clone(), ft.clone(), get));
                                        }
                                    }
                                }
                                self.mk(TK::Bin(BinOp::Eq, Box::new(tag), Box::new(kv)), Ty::Bool, *vsp)
                            }
                            None if subj.as_ref().is_some_and(|(_, t)| self.resolve(t) == Ty::Error) => {
                                // Over an Error: a variant of any error type, or an error type itself.
                                let l = local(self).unwrap();
                                let errors = self.w.errors.clone();
                                if let Some(k) = errors.iter().position(|t| tq.is_none() && t.type_name() == Some(vn.as_str())).or_else(|| if tq.is_none() { errors.iter().position(|t| type_is(t, vn)) } else { None }) {
                                    if bs.is_some() {
                                        return Err(Diag::new(*vsp, format!("`{vn}` is an error type; match its variants to bind fields")));
                                    }
                                    self.mk(TK::M(M::ErrIs(k), Some(Box::new(l)), vec![], None), Ty::Bool, *vsp)
                                } else {
                                    let mut hits: Vec<(usize, usize)> = errors.iter().enumerate().filter(|(_, t)| tq.as_deref().is_none_or(|q| type_is(t, q))).filter_map(|(k, t)| match t {
                                        Ty::Enum(_, vs) => vs.iter().position(|(v, _)| v == vn).map(|j| (k, j)),
                                        _ => None,
                                    }).collect();
                                    // Several packages may name a variant alike: the
                                    // current package's own error types win.
                                    if hits.len() > 1 {
                                        let pkg = current_pkg();
                                        let own: Vec<(usize, usize)> = hits.iter().copied().filter(|&(k, _)| errors[k].type_name().is_some_and(|n| pkg_of(n) == pkg)).collect();
                                        if !own.is_empty() {
                                            hits = own;
                                        }
                                    }
                                    let (k, j) = match hits.as_slice() {
                                        [one] => *one,
                                        [] => return Err(Diag::new(*vsp, format!("no error type has a variant `{vn}`"))),
                                        _ => return Err(Diag::new(*vsp, format!("several error types have a variant `{vn}`; match the type first"))),
                                    };
                                    let et = errors[k].clone();
                                    let Ty::Enum(_, vs) = &et else { unreachable!() };
                                    let fields = vs[j].1.clone();
                                    let is = self.mk(TK::M(M::ErrIs(k), Some(Box::new(l.clone())), vec![], None), Ty::Bool, *vsp);
                                    let as_t = self.mk(TK::M(M::ErrAs(k), Some(Box::new(l)), vec![], None), et.clone(), *vsp);
                                    if let Some(bs) = bs {
                                        if bs.len() != fields.len() {
                                            return Err(Diag::new(*vsp, format!("`{vn}` has {} field(s), the pattern binds {}", fields.len(), bs.len())));
                                        }
                                        if arm.pats.len() > 1 {
                                            return Err(Diag::new(*vsp, "an arm with alternatives (`|`) can't bind fields"));
                                        }
                                        let base = et.enum_slot(j);
                                        for (m, ((b, _), (_, ft))) in bs.iter().zip(&fields).enumerate() {
                                            if b != "_" {
                                                let get = self.mk(TK::M(M::TupleGet(base + m), Some(Box::new(as_t.clone())), vec![], None), ft.clone(), *vsp);
                                                binds.push((b.clone(), ft.clone(), get));
                                            }
                                        }
                                    }
                                    let tag = self.mk(TK::M(M::EnumTag, Some(Box::new(as_t)), vec![], None), Ty::Int, *vsp);
                                    let jv = self.mk(TK::Int(j as i64), Ty::Int, *vsp);
                                    let eq = self.mk(TK::Bin(BinOp::Eq, Box::new(tag), Box::new(jv)), Ty::Bool, *vsp);
                                    self.mk(TK::Bin(BinOp::And, Box::new(is), Box::new(eq)), Ty::Bool, *vsp)
                                }
                            }
                            None if bs.is_none() => {
                                let e = Expr { kind: ExprKind::Const(vn.clone()), span: *vsp, id: NodeId::MAX };
                                match local(self) {
                                    Some(l) => {
                                        let r = self.value(&e)?;
                                        self.binary(BinOp::Eq, l, r, *vsp)?
                                    }
                                    None => self.cond(&e)?,
                                }
                            }
                            None => return Err(Diag::new(*vsp, format!("`{vn}(...)` is a variant pattern, but the subject isn't an enum"))),
                        }
                    }
                    Pat::Value(e) => match local(self) {
                        Some(l) => {
                            let r = self.value(e)?;
                            self.binary(BinOp::Eq, l, r, e.span)?
                        }
                        None => self.cond(e)?,
                    },
                    Pat::Range(lo, hi, excl) => {
                        let l = local(self).ok_or_else(|| Diag::new(lo.span, "a range pattern needs a `case` subject"))?;
                        let lov = self.value(lo)?;
                        let ge = self.binary(BinOp::Ge, l.clone(), lov, lo.span)?;
                        let hiv = self.value(hi)?;
                        let lt = self.binary(if *excl { BinOp::Lt } else { BinOp::Le }, l, hiv, hi.span)?;
                        self.binary(BinOp::And, ge, lt, lo.span.to(hi.span))?
                    }
                };
                cond = Some(match cond {
                    None => c,
                    Some(prev) => {
                        let s = prev.span.to(c.span);
                        self.binary(BinOp::Or, prev, c, s)?
                    }
                });
            }
            let body = self.arm_body_with(binds, &arm.body, arm.span, used)?;
            conds.push((cond.unwrap(), body));
        }
        // Over an enum: every variant must be covered (or `_` given); the
        // last arm then needs no test.
        if let (Some(Ty::Enum(en, vs)), None) = (&enum_ty, &default) {
            let missing: Vec<&str> = vs.iter().zip(&covered).filter(|(_, c)| !**c).map(|((v, _), _)| v.as_str()).collect();
            if !missing.is_empty() {
                return Err(Diag::new(sp, format!("this `case` on {en} doesn't cover {}; add {} or `_`", missing.join(", "), if missing.len() == 1 { "an arm for it" } else { "arms for them" })));
            }
            if let Some((_, last)) = conds.pop() {
                default = Some(last);
            }
        }
        // Without `_` nothing may match: the arms are statements and the case has no value.
        let mut has_default = default.is_some();
        let unitize = |cx: &mut Self, b: TExpr| -> TExpr {
            let span = b.span;
            let TK::Seq(mut ss) = b.kind else { unreachable!() };
            ss.push(TStmt::Expr(cx.mk(TK::Unit, Ty::Unit, span)));
            cx.mk(TK::Seq(ss), Ty::Unit, span)
        };
        // An arm that is a T? makes the others' plain T values present.
        if has_default {
            let all: Vec<Ty> = conds.iter().map(|(_, b)| self.resolve(&b.ty)).chain(default.iter().map(|d| self.resolve(&d.ty))).collect();
            if let Some(ot) = all.iter().find(|t| matches!(t, Ty::Opt(_))).cloned() {
                let lift = |cx: &mut Self, b: TExpr| -> R<TExpr> {
                    let t = cx.resolve(&b.ty);
                    if matches!(t, Ty::Opt(_) | Ty::Var(_) | Ty::Never) { Ok(b) } else { cx.coerce_branch(b, &ot) }
                };
                let mut lifted = vec![];
                for (c, b) in conds {
                    lifted.push((c, lift(self, b)?));
                }
                conds = lifted;
                default = Some(lift(self, default.unwrap())?);
            }
        }
        // Arms that disagree: an error if the value is wanted, else a statement (no value).
        // The case's type is the first arm's that has a value: an arm that
        // panics or returns (a Never) fits any type, and mustn't make the
        // whole case a Never.
        let mut case_ty = None;
        if let Some(d) = default.as_ref().filter(|_| has_default) {
            // The last arm's type, unless it is a Never: then the first
            // other arm's that has a value.
            let ty = if matches!(self.resolve(&d.ty), Ty::Never) {
                conds.iter().map(|(_, b)| b.ty.clone()).find(|t| !matches!(self.resolve(t), Ty::Never)).unwrap_or_else(|| d.ty.clone())
            } else {
                d.ty.clone()
            };
            case_ty = Some(ty.clone());
            for (_, b) in &conds {
                if !self.unify(&b.ty, &ty) {
                    if used {
                        return Err(Diag::new(b.span, format!("this arm is {}, but the others are {}", self.resolve(&b.ty).show(), self.resolve(&ty).show())));
                    }
                    has_default = false;
                    break;
                }
            }
        }
        let mut acc = match default {
            Some(d) if has_default => d,
            Some(d) => unitize(self, d),
            None => self.mk(TK::Unit, Ty::Unit, sp),
        };
        let ty = match case_ty {
            Some(t) if has_default => t,
            _ => acc.ty.clone(),
        };
        for (c, b) in conds.into_iter().rev() {
            let b = if has_default { b } else { unitize(self, b) };
            let s = c.span.to(b.span);
            acc = self.mk(TK::Ternary(Box::new(c), Box::new(b), Box::new(acc)), ty.clone(), s);
        }
        if pre.is_empty() {
            return Ok(acc);
        }
        pre.push(TStmt::Expr(acc));
        Ok(self.mk(TK::Seq(pre), ty, sp))
    }

    /// Two branches' values: a T meeting a T? becomes present (`if c { x } else { none }`).
    fn join(&mut self, a: TExpr, b: TExpr) -> R<(TExpr, TExpr)> {
        let (ra, rb) = (self.resolve(&a.ty), self.resolve(&b.ty));
        Ok(match (&ra, &rb) {
            (Ty::Opt(_), t) if !matches!(t, Ty::Opt(_) | Ty::Var(_) | Ty::Never) => {
                let b = self.coerce_branch(b, &ra)?;
                (a, b)
            }
            (t, Ty::Opt(_)) if !matches!(t, Ty::Opt(_) | Ty::Var(_) | Ty::Never) => {
                let a = self.coerce_branch(a, &rb)?;
                (a, b)
            }
            // `c ? x : 0` with x a U8: the literal takes the other branch's type.
            (Ty::Int, t) if t.int_kind().is_some() && matches!(a.kind, TK::Const(_)) => {
                let a = self.coerce(a, &rb)?;
                (a, b)
            }
            (t, Ty::Int) if t.int_kind().is_some() && matches!(b.kind, TK::Const(_)) => {
                let b = self.coerce(b, &ra)?;
                (a, b)
            }
            _ => (a, b),
        })
    }

    /// Coerce a branch's value (the last expression of its statements).
    fn coerce_branch(&mut self, e: TExpr, want: &Ty) -> R<TExpr> {
        match e.kind {
            TK::Seq(mut ss) => {
                if let Some(TStmt::Expr(last)) = ss.pop() {
                    let v = self.coerce(last, want)?;
                    ss.push(TStmt::Expr(v));
                }
                let sp = e.span;
                Ok(self.mk(TK::Seq(ss), want.clone(), sp))
            }
            _ => self.coerce(e, want),
        }
    }

    /// Evaluate an optional into a fresh local: (local, its assignment).
    /// Inside a method, a bare field name `f` means `self.f`.
    fn self_field(&self, e: &Expr) -> Option<Expr> {
        let (name, name_span) = match &e.kind {
            ExprKind::Call { recv: None, name, args, block: None, name_span, .. } if args.is_empty() => (name, name_span),
            ExprKind::Name(n) if n != "self" => (n, &e.span),
            _ => return None,
        };
        if self.method.is_none() || self.lookup(name).is_some() {
            return None;
        }
        let Some(Ty::Struct(_, fs)) = self.self_struct() else { return None };
        if !fs.iter().any(|(f, _)| f == name) {
            return None;
        }
        let me = Expr { kind: ExprKind::Name("self".into()), span: *name_span, id: NodeId::MAX };
        Some(Expr { kind: ExprKind::Call { recv: Some(Box::new(me)), name: name.clone(), name_span: *name_span, args: vec![], block: None, block_sym: None }, span: e.span, id: e.id })
    }

    /// A struct with a `to_s` method prints through it (Go's Stringer).
    /// Instantiate `to_s` for every struct or enum inside `t` that has one.
    fn stringers(&mut self, t: &Ty, sp: Span) -> R<()> {
        let t = self.resolve(t);
        if t.has_var() || self.w.stringers.contains_key(&t.show()) {
            return Ok(());
        }
        if let Some(tn) = t.type_name() {
            if let Some(&def) = self.w.by_name.get(&method_name(tn, "to_s")) {
                let fid = self.w.instance(def, vec![t.clone()], sp)?;
                self.w.stringers.insert(t.show(), fid);
                return Ok(());
            }
        }
        match &t {
            Ty::Array(e) | Ty::Fixed(e, _) | Ty::Opt(e) => self.stringers(e, sp),
            Ty::Map(k, v) => {
                self.stringers(k, sp)?;
                self.stringers(v, sp)
            }
            Ty::Tuple(ts) => ts.iter().try_for_each(|x| self.stringers(x, sp)),
            Ty::Struct(_, fs) => fs.clone().iter().try_for_each(|(_, x)| self.stringers(x, sp)),
            Ty::Enum(_, vs) => vs.clone().iter().flat_map(|(_, fs)| fs.iter().map(|(_, x)| x.clone())).collect::<Vec<_>>().iter().try_for_each(|x| self.stringers(x, sp)),
            _ => Ok(()),
        }
    }

    fn show_value(&mut self, v: TExpr) -> R<TExpr> {
        self.stringers(&v.ty.clone(), v.span)?;
        if let Some(sn) = self.resolve(&v.ty).type_name().map(str::to_string) {
            if let Some(&def) = self.w.by_name.get(&method_name(&sn, "to_s")) {
                let sp = v.span;
                return self.call_def(def, "to_s", sp, vec![v], sp);
            }
        }
        Ok(v)
    }

    /// `a == b` field by field (Go's comparable structs).
    fn struct_eq(&mut self, l: TExpr, r: TExpr, sp: Span) -> R<TExpr> {
        let t = self.resolve(&l.ty);
        let fts: Vec<Ty> = match &t {
            Ty::Struct(_, fs) => fs.iter().map(|(_, t)| t.clone()).collect(),
            Ty::Tuple(ts) => ts.clone(),
            // Unused slots hold zero values, so comparing them all works.
            Ty::Enum(_, vs) => std::iter::once(Ty::Int).chain(vs.iter().flat_map(|(_, fs)| fs.iter().map(|(_, t)| t.clone()))).collect(),
            _ => unreachable!(),
        };
        let (lid, s1) = self.opt_tmp(l, sp);
        let (rid, s2) = self.opt_tmp(r, sp);
        let mut acc: Option<TExpr> = None;
        for (k, ft) in fts.iter().enumerate() {
            if matches!(self.resolve(ft), Ty::Map(..)) {
                return Err(Diag::new(sp, format!("`==` on {}: its field of type {} can't be compared; define `def ==(other)`", t.show(), ft.show())));
            }
            let a = self.mk(TK::Local(lid), t.clone(), sp);
            let b = self.mk(TK::Local(rid), t.clone(), sp);
            let fa = self.mk(TK::M(M::TupleGet(k), Some(Box::new(a)), vec![], None), ft.clone(), sp);
            let fb = self.mk(TK::M(M::TupleGet(k), Some(Box::new(b)), vec![], None), ft.clone(), sp);
            let eq = if matches!(self.resolve(ft), Ty::Opt(_)) { self.opt_eq(fa, fb, sp)? } else { self.binary(BinOp::Eq, fa, fb, sp)? };
            acc = Some(match acc {
                None => eq,
                Some(p) => self.mk(TK::Bin(BinOp::And, Box::new(p), Box::new(eq)), Ty::Bool, sp),
            });
        }
        let e = acc.unwrap_or_else(|| self.mk(TK::Bool(true), Ty::Bool, sp));
        Ok(self.mk(TK::Seq(vec![s1, s2, TStmt::Expr(e)]), Ty::Bool, sp))
    }

    /// `a == b` on slices: `a.size == b.size && (0...a.size).all? { a[i] == b[i] }`.
    fn arr_eq(&mut self, l: TExpr, r: TExpr, sp: Span) -> R<TExpr> {
        let t = self.resolve(&l.ty);
        let el = match &t {
            Ty::Array(e) | Ty::Fixed(e, _) => (**e).clone(),
            _ => unreachable!(),
        };
        let (lid, s1) = self.opt_tmp(l, sp);
        let (rid, s2) = self.opt_tmp(r, sp);
        let side = |cx: &mut Self, id: LocalId| cx.mk(TK::Local(id), t.clone(), sp);
        let (la, lb) = (side(self, lid), side(self, rid));
        let size = |cx: &mut Self, x: TExpr| cx.mk(TK::M(M::Size, Some(Box::new(x)), vec![], None), Ty::Int, sp);
        let (na, nb) = (size(self, la.clone()), size(self, lb.clone()));
        let same_size = self.binary(BinOp::Eq, na.clone(), nb, sp)?;
        let i = self.declare(&format!("_eqi{}_{}", sp.lo, self.locals.len()), Ty::Int);
        let li = self.mk(TK::Local(i), Ty::Int, sp);
        let ea = self.mk(TK::Index(Box::new(la), Box::new(li.clone())), el.clone(), sp);
        let eb = self.mk(TK::Index(Box::new(lb), Box::new(li)), el, sp);
        let body = self.binary(BinOp::Eq, ea, eb, sp)?;
        let zero = self.mk(TK::Int(0), Ty::Int, sp);
        let range = self.mk(TK::Range(Box::new(zero), Box::new(na), true), Ty::Range, sp);
        let blk = TBlock { params: vec![i], destructure: false, body: vec![TStmt::Expr(body)], pure: false, span: sp, own: (i, i + 1) };
        let all = self.mk(TK::M(M::All, Some(Box::new(range)), vec![], Some(Box::new(blk))), Ty::Bool, sp);
        let both = self.mk(TK::Bin(BinOp::And, Box::new(same_size), Box::new(all)), Ty::Bool, sp);
        Ok(self.mk(TK::Seq(vec![s1, s2, TStmt::Expr(both)]), Ty::Bool, sp))
    }

    /// Two T? are equal if both are absent or both hold equal values.
    fn opt_eq(&mut self, a: TExpr, b: TExpr, sp: Span) -> R<TExpr> {
        let ot = a.ty.clone();
        let (aid, s1) = self.opt_tmp(a, sp);
        let (bid, s2) = self.opt_tmp(b, sp);
        let (pa, pb) = (self.opt_present(aid, &ot, sp), self.opt_present(bid, &ot, sp));
        let both = self.mk(TK::Bin(BinOp::Eq, Box::new(pa.clone()), Box::new(pb)), Ty::Bool, sp);
        let (ga, gb) = (self.opt_get(aid, &ot, sp), self.opt_get(bid, &ot, sp));
        let inner = self.binary(BinOp::Eq, ga, gb, sp)?;
        let t = self.mk(TK::Bool(true), Ty::Bool, sp);
        let vals = self.mk(TK::Ternary(Box::new(pa), Box::new(inner), Box::new(t)), Ty::Bool, sp);
        let e = self.mk(TK::Bin(BinOp::And, Box::new(both), Box::new(vals)), Ty::Bool, sp);
        Ok(self.mk(TK::Seq(vec![s1, s2, TStmt::Expr(e)]), Ty::Bool, sp))
    }

    /// `->(x: T) -> R { ... }`: parameter types come from annotations or
    /// from the function type it is wanted as.
    fn lambda(&mut self, params: &[Param], ret: Option<&TypeExpr>, blk: &Block, sp: Span) -> R<TExpr> {
        let hint = match self.want_hint.clone().map(|t| self.resolve(&t)) {
            Some(Ty::Fn(ps, r)) if ps.len() == params.len() => Some((ps, *r)),
            _ => None,
        };
        let mut ptys = vec![];
        for (i, p) in params.iter().enumerate() {
            ptys.push(match (&p.ty, &hint) {
                (Some(t), _) => type_from(t, &self.w.structs, &self.w.consts)?,
                (None, Some((ps, _))) => ps[i].clone(),
                (None, None) => return Err(Diag::new(p.span, format!("give `{}` a type (`{}: Int`), or use the lambda where a function type is expected", p.name, p.name))),
            });
        }
        // `return` inside returns from the lambda.
        let rvar = match (ret, &hint) {
            (Some(t), _) => type_from(t, &self.w.structs, &self.w.consts)?,
            (None, Some((_, r))) => r.clone(),
            (None, None) => self.fresh(),
        };
        // A lambda of type `(..) -> ~T` is fallible like a def: `~` and `fail`
        // inside return the error, and its body produces a T.
        let fallible = matches!(self.resolve(&rvar), Ty::Result(_));
        let body_ret = match self.resolve(&rvar) {
            Ty::Result(t) => *t,
            _ => rvar.clone(),
        };
        let saved_fallible = std::mem::replace(&mut self.fallible_decl, fallible);
        let saved_declared = std::mem::replace(&mut self.declared_errs, false);
        let saved_lambda = std::mem::replace(&mut self.in_lambda, true);
        let saved_lock = self.lock_floor.take();
        let saved_cells = self.cells.take();
        let saved_ret = std::mem::replace(&mut self.ret, body_ret.clone());
        let saved_main = std::mem::replace(&mut self.is_main, false);
        let saved = self.want_hint.take();
        let saved_loops = std::mem::take(&mut self.loops);
        let r = if ptys.is_empty() {
            let own_start = self.locals.len();
            self.scopes.push(HashMap::new());
            let r = self.body(&blk.body);
            self.pop_scope();
            r.map(|(body, _)| {
                let ty = match body.last() {
                    Some(TStmt::Expr(e)) => e.ty.clone(),
                    _ => Ty::Unit,
                };
                (TBlock { params: vec![], destructure: false, body, pure: false, span: blk.span, own: (own_start, self.locals.len()) }, ty)
            })
        } else {
            self.block_n(blk, &ptys, false)
        };
        self.loops = saved_loops;
        self.want_hint = saved;
        self.ret = saved_ret;
        self.is_main = saved_main;
        self.in_lambda = saved_lambda;
        self.fallible_decl = saved_fallible;
        self.declared_errs = saved_declared;
        self.lock_floor = saved_lock;
        self.cells = saved_cells;
        let (mut tb, bt) = r?;
        if ret.is_none() && hint.is_none() && !matches!(self.resolve(&bt), Ty::Never) && !self.unify(&rvar, &bt) {
            return Err(Diag::new(sp, format!("this lambda returns {} and {}", self.resolve(&rvar).show(), self.resolve(&bt).show())));
        }
        let rt = self.resolve(&rvar);
        let bt_want = self.resolve(&body_ret);
        if bt_want != Ty::Unit {
            if let Some(TStmt::Expr(last)) = tb.body.pop() {
                let last = self.coerce(last, &bt_want)?;
                self.expect(&last.ty, &bt_want, last.span, "lambda result")?;
                tb.body.push(TStmt::Expr(last));
            } else {
                return Err(Diag::new(sp, format!("this lambda should return {}", rt.show())));
            }
        }
        // Captured: locals used inside that were declared outside.
        let mut used = vec![];
        for s in &tb.body {
            crate::lower::collect_locals_stmt(s, &mut used);
        }
        used.sort_unstable();
        used.dedup();
        let caps: Vec<LocalId> = used.into_iter().filter(|l| *l < tb.own.0 || *l >= tb.own.1).filter(|l| !tb.params.contains(l)).collect();
        let ty = Ty::Fn(ptys, Box::new(rt));
        let lo = blk.span.lo;
        self.lambdas.retain(|(l, _, _)| *l != lo);
        self.lambdas.push((lo, ty.clone(), caps.clone()));
        self.lambda_info.retain(|(l, _, _)| *l != lo);
        let own_lo = tb.own.0.min(tb.params.iter().copied().min().unwrap_or(tb.own.0));
        self.lambda_info.push((lo, tb.params.clone(), (own_lo, tb.own.1)));
        let cap_es = caps.iter().map(|l| self.mk(TK::Local(*l), self.locals[*l].ty.clone(), sp)).collect();
        Ok(self.mk(TK::M(M::Lambda, None, cap_es, Some(Box::new(tb))), ty, sp))
    }

    /// A block passed where `def`'s last parameter takes a function: a
    /// lambda whose parameter types come from that parameter's type, with
    /// the def's type parameters bound by the arguments before it.
    fn block_as_lambda(&mut self, def: usize, before: &[TExpr], b: &Block) -> R<TExpr> {
        let d = self.w.defs[def].def.clone();
        let mut env: HashMap<String, Ty> = HashMap::new();
        for (p, a) in d.params.iter().zip(before) {
            if let Some(te) = &p.ty {
                let at = self.resolve(&a.ty);
                bind_tparams(te, &at, &d.tparams, &mut env);
            }
        }
        // A generic type's method (`r.do { |v| }` on a `Ring[Int]`): its
        // type's parameters come from the receiver.
        if let (Some((owner, _)), Some(a0)) = (d.name.rsplit_once('.'), before.first()) {
            if let Some(g) = generic(owner).filter(|_| d.params.first().is_some_and(|p| p.name == "self")) {
                let st = self.resolve(&a0.ty);
                let st = st.arr_elem().filter(|_| d.name.ends_with('!')).unwrap_or(st);
                if let Some((_, targs)) = inst_args(&st) {
                    for (tp, t) in g.tparams().iter().zip(targs) {
                        env.entry(tp.name.clone()).or_insert(t);
                    }
                }
            }
        }
        let prev = enter_pkg(&self.w.defs[def].pkg);
        let want = match d.params.last().and_then(|p| p.ty.as_ref()) {
            Some(te) => subst_type(te, &env, &self.w.structs, &self.w.consts),
            None => None,
        };
        leave_pkg(prev);
        let Some(Ty::Fn(ps, _)) = want.clone() else {
            return Err(Diag::new(b.span, format!("can't tell this block's parameter types from `{}`'s arguments; pass a lambda with typed parameters", d.name)));
        };
        let mut blk = b.clone();
        if blk.params.is_empty() && ps.len() == 1 {
            blk.params = vec![("it".into(), b.span)];
        }
        let params = blk.params.iter().map(|(n, s)| Param { name: n.clone(), ty: None, span: *s }).collect();
        let lam = Expr { id: NodeId::MAX, kind: ExprKind::Lambda(params, None, Box::new(blk)), span: b.span };
        let saved = self.want_hint.replace(want.unwrap());
        let r = self.expr(&lam);
        self.want_hint = saved;
        r
    }

    /// `next`/`break` mustn't leave a `lock` block (only loops inside it).
    fn not_leaving_lock(&self, sp: Span, what: &str) -> R<()> {
        match self.lock_floor {
            Some(f) if self.loops.len() <= f + 1 => Err(Diag::new(sp, format!("`{what}` would leave the `lock` block with the lock held; let the block finish"))),
            _ => Ok(()),
        }
    }

    /// `Mutex.new(v)` / `Atomic.new(v)`, with the value type given or not.
    fn sync_new(&mut self, c: &str, t: Option<Ty>, csp: Span, args: &[Expr], sp: Span) -> R<TExpr> {
        let [a] = args else {
            return Err(Diag::new(sp, format!("`{c}.new` takes the initial value")));
        };
        let v = match &t {
            Some(t) => {
                let v = self.value_as(a, t)?;
                self.expect(&v.ty, t, v.span, "initial value")?;
                v
            }
            None => self.value(a)?,
        };
        let vt = self.resolve(&v.ty);
        self.impure = true;
        if c == "Mutex" {
            return Ok(self.mk(TK::M(M::MutexNew, None, vec![v], None), Ty::Mutex(Box::new(vt)), sp));
        }
        if !matches!(vt, Ty::Int | Ty::Bool) {
            return Err(Diag::new(csp, format!("`Atomic` holds an Int or a Bool, not {}; guard other values with a `Mutex`", vt.show())));
        }
        Ok(self.mk(TK::M(M::AtomicNew, None, vec![v], None), Ty::Atomic(Box::new(vt)), sp))
    }

    fn fn_call(&mut self, f: TExpr, args: &[Expr], sp: Span) -> R<TExpr> {
        let Ty::Fn(ps, r) = self.resolve(&f.ty) else { unreachable!() };
        if args.len() != ps.len() {
            return Err(Diag::new(sp, format!("this {} takes {} argument(s), got {}", f.ty.show(), ps.len(), args.len())));
        }
        let mut targs = vec![];
        for (a, p) in args.iter().zip(&ps) {
            let v = self.value_as(a, p)?;
            self.expect(&v.ty, p, v.span, "argument")?;
            targs.push(v);
        }
        self.impure = true;
        Ok(self.mk(TK::M(M::FnCall, Some(Box::new(f)), targs, None), *r, sp))
    }

    /// `spawn { body }`: the body runs on its own task, with the locals it
    /// uses moved in (sharing.rs rejects using shared storage afterwards).
    /// It may use `~`: its errors become the task's result.
    fn spawn(&mut self, blk: &Block, sp: Span) -> R<TExpr> {
        let saved_errs = std::mem::take(&mut self.errs);
        let saved_fallible = std::mem::replace(&mut self.fallible_decl, true);
        let saved_main = std::mem::replace(&mut self.is_main, false);
        let saved_lambda = std::mem::replace(&mut self.in_lambda, false);
        let saved_lock = self.lock_floor.take();
        let saved_cells = self.cells.take();
        let saved_loops = std::mem::take(&mut self.loops);
        let rvar = self.fresh();
        let saved_ret = std::mem::replace(&mut self.ret, rvar.clone());
        let own_start = self.locals.len();
        self.scopes.push(HashMap::new());
        let r = self.body(&blk.body);
        self.pop_scope();
        self.ret = saved_ret;
        self.loops = saved_loops;
        self.in_lambda = saved_lambda;
        self.lock_floor = saved_lock;
        self.cells = saved_cells;
        self.is_main = saved_main;
        self.fallible_decl = saved_fallible;
        let body_errs = std::mem::replace(&mut self.errs, saved_errs);
        let (body, _) = r?;
        let bt = match body.last() {
            Some(TStmt::Expr(e)) => e.ty.clone(),
            _ => Ty::Unit,
        };
        // A held `~T` as the task's value isn't lowered (the C backend
        // mistyped it, the JIT lost it): the body's errors already make the
        // task's result fallible, so ask for the `~`.
        // (`spawn { srv.serve(ln) }`, a held ~Unit, is common and works.)
        if let (Ty::Result(inner), Some(TStmt::Expr(e))) = (&bt, body.last()) {
            if **inner != Ty::Unit {
            return Err(Diag::new(e.span, "a `spawn` body can't end in a held `~T`: propagate it with `~` (`spawn { ~f(x) }`), so its error becomes the task's"));
            }
        }
        let tb = TBlock { params: vec![], destructure: false, body, pure: false, span: blk.span, own: (own_start, self.locals.len()) };
        let mut used = vec![];
        for s in &tb.body {
            crate::lower::collect_locals_stmt(s, &mut used);
        }
        used.sort_unstable();
        used.dedup();
        let caps: Vec<LocalId> = used.into_iter().filter(|l| *l < tb.own.0 || *l >= tb.own.1).collect();
        let cap_es = caps.iter().map(|l| self.mk(TK::Local(*l), self.locals[*l].ty.clone(), sp)).collect();
        // A body that can fail produces a ~T.
        let produced = if body_errs.is_empty() { bt } else { Ty::Result(Box::new(bt)) };
        self.impure = true;
        Ok(self.mk(TK::M(M::Spawn, None, cap_es, Some(Box::new(tb))), Ty::Task(Box::new(produced)), sp))
    }

    /// `select { when v = ch.recv => ...; when ch.send(x) => ...; else => ... }`
    fn select(&mut self, arms: &[SelArm], default: Option<&[Stmt]>, sp: Span) -> R<TExpr> {
        let mut out = vec![];
        for a in arms {
            let arm = match &a.op {
                SelOp::Recv(bind, ch) => {
                    let c = self.value(ch)?;
                    let Ty::Chan(t) = self.resolve(&c.ty) else {
                        return Err(Diag::new(ch.span, format!("`select` receives from a channel, not {}", self.resolve(&c.ty).show())));
                    };
                    self.scopes.push(HashMap::new());
                    let id = bind.as_ref().map(|(n, _)| self.declare(n, Ty::Opt(t.clone())));
                    let r = self.body_as(&a.body, false);
                    self.pop_scope();
                    TSelArm::Recv { ch: c, bind: id, body: r?.0 }
                }
                SelOp::Send(ch, v) => {
                    let c = self.value(ch)?;
                    let Ty::Chan(t) = self.resolve(&c.ty) else {
                        return Err(Diag::new(ch.span, format!("`select` sends on a channel, not {}", self.resolve(&c.ty).show())));
                    };
                    let v = self.value(v)?;
                    let v = self.coerce(v, &t)?;
                    self.expect(&v.ty, &t, v.span, "sent value")?;
                    self.scopes.push(HashMap::new());
                    let r = self.body_as(&a.body, false);
                    self.pop_scope();
                    TSelArm::Send { ch: c, val: v, body: r?.0 }
                }
            };
            out.push(arm);
        }
        let d = match default {
            Some(b) => {
                self.scopes.push(HashMap::new());
                let r = self.body_as(b, false);
                self.pop_scope();
                Some(r?.0)
            }
            None => None,
        };
        self.impure = true;
        Ok(self.mk(TK::Select(out, d), Ty::Unit, sp))
    }

    /// A value checked against a known type: array and map literals check
    /// each element against it (so `[Circle.new, Square.new]` can be `[Shape]`).
    fn value_as(&mut self, e: &Expr, want: &Ty) -> R<TExpr> {
        let w = self.resolve(want);
        // A tuple literal checks each element against its slot (`T?` too).
        if let ExprKind::Tuple(items) = &e.kind {
            let slots = match &w {
                Ty::Tuple(ts) => Some(ts.clone()),
                Ty::Opt(inner) => match self.resolve(inner) {
                    Ty::Tuple(ts) => Some(ts),
                    _ => None,
                },
                _ => None,
            };
            if let Some(ts) = slots.filter(|ts| ts.len() == items.len()) {
                let mut vs = vec![];
                for (x, t) in items.iter().zip(&ts) {
                    let v = self.value_as(x, t)?;
                    self.expect(&v.ty, t, v.span, "tuple element")?;
                    vs.push(v);
                }
                let tv = self.mk(TK::M(M::TupleNew, None, vs, None), Ty::Tuple(ts), e.span);
                return self.coerce(tv, &w);
            }
        }
        if let (ExprKind::Array(items), Some(el)) = (&e.kind, w.arr_elem()) {
            let mut vs = vec![];
            for x in items {
                let v = self.value_as(x, &el)?;
                self.expect(&v.ty, &el, v.span, "array element")?;
                vs.push(v);
            }
            if let Ty::Fixed(_, n) = &w {
                if vs.len() as u64 != *n {
                    return Err(Diag::new(e.span, format!("{} elements where {} needs {n}", vs.len(), w.show())));
                }
            }
            return Ok(self.mk(TK::Array(vs), w, e.span));
        }
        if let (ExprKind::ArrayRepeat(v, n), Ty::Array(el)) = (&e.kind, &w) {
            // `[0; 800]` as a `[U8]`: the fill takes the element type.
            let v = self.value_as(v, el)?;
            self.expect(&v.ty, el, v.span, "array element")?;
            let n = self.index_value(n)?;
            return Ok(self.mk(TK::M(M::ArrayNew, None, vec![n, v], None), w.clone(), e.span));
        }
        let saved = self.want_hint.replace(w.clone());
        let v = self.value(e);
        self.want_hint = saved;
        let v = v?;
        self.coerce(v, &w)
    }

    /// `fail x`: a Str becomes `Failure.Msg(x)`; an error type's value is wrapped.
    fn to_error(&mut self, e: TExpr) -> R<TExpr> {
        let sp = e.span;
        let t = self.resolve(&e.ty);
        match &t {
            Ty::Error => Ok(e),
            Ty::Str => {
                let ft = self.w.structs["Failure"].clone();
                let v = self.mk(TK::M(M::VariantNew(0), None, vec![e], None), ft.clone(), sp);
                let k = self.w.error_index(&ft).unwrap();
                Ok(self.mk(TK::M(M::ToError(k), Some(Box::new(v)), vec![], None), Ty::Error, sp))
            }
            _ => match self.w.error_index(&t) {
                Some(k) => Ok(self.mk(TK::M(M::ToError(k), Some(Box::new(e)), vec![], None), Ty::Error, sp)),
                None => Err(Diag::new(sp, format!("`fail` takes an error (a value of an `error` type) or a message Str, not {}", t.show()))),
            },
        }
        .inspect(|_| {
            let name = match &t {
                Ty::Str => "Failure".to_string(),
                Ty::Error => "Error".to_string(),
                t => t.type_name().unwrap_or("Error").to_string(),
            };
            self.errs.insert(name);
        })
    }

    /// `geom` in `geom.area(x)`: an imported package's name (not a local).
    fn pkg_alias(&self, e: &Expr) -> Option<String> {
        let a = match &e.kind {
            ExprKind::Name(a) => a,
            ExprKind::Call { recv: None, name, args, block: None, .. } if args.is_empty() => name,
            _ => return None,
        };
        if self.lookup(a).is_some() {
            return None;
        }
        import_path(a)
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        let depth = self.scopes.len();
        self.usings.retain(|(d, _)| *d <= depth);
    }

    /// A method from an active refinement, innermost `using` first.
    fn refined(&self, t: &Ty, name: &str) -> Option<usize> {
        for (_, r) in self.usings.iter().rev() {
            if let Some(entries) = self.w.refines.get(r) {
                if let Some(def) = entries.iter().find(|(x, _)| x == t).and_then(|(_, ms)| ms.get(name)) {
                    return Some(*def);
                }
            }
        }
        None
    }

    /// The struct type of `self` in a method.
    fn self_struct(&self) -> Option<Ty> {
        let id = self.lookup("self")?;
        let t = self.resolve(&self.locals[id].ty);
        match self.method {
            Some(true) => t.arr_elem(),
            _ => Some(t),
        }
    }

    /// `recv.m!(args)`: call a mutating method on a place, through a
    /// one-element slice, and write the receiver back.
    fn mutating_call(&mut self, recv: &Expr, def: usize, name: &str, name_span: Span, args: &[Expr], sp: Span) -> R<TExpr> {
        let (id, steps, pty) = self.place(recv).map_err(|d| {
            if matches!(recv.kind, ExprKind::Name(_) | ExprKind::Index(..) | ExprKind::Call { .. }) { d } else { Diag::new(recv.span, format!("`{name}` changes its receiver; call it on a variable")) }
        })?;
        // Arguments are evaluated before the receiver is read: an argument
        // that calls something (`p.push!(p.new_node!)`) may change the
        // receiver, and the call must see that change.
        let mut pre = vec![];
        let mut vals = vec![];
        for a in args {
            let v = self.value(a)?;
            if has_call(&v) {
                let (aid, st) = self.opt_tmp(v.clone(), v.span);
                pre.push(st);
                vals.push(self.mk(TK::Local(aid), v.ty.clone(), v.span));
            } else {
                vals.push(v);
            }
        }
        let cur = self.place_read(id, &steps, &pty, recv.span);
        let (tid, s1) = self.call_cell(cur, &pty, recv.span, true);
        let s1 = if pre.is_empty() {
            s1
        } else {
            pre.push(s1);
            TStmt::Expr(self.mk(TK::Seq(pre), Ty::Unit, sp))
        };
        let mut targs = vec![self.mk(TK::Local(tid), Ty::arr(pty.clone()), recv.span)];
        targs.extend(vals);
        // The receiver is written back after the call, so a fallible call
        // is held (a `~T`) here; an enclosing `~` propagates it after.
        let saved = std::mem::replace(&mut self.under_try, false);
        let call = self.call_def(def, name, name_span, targs, sp);
        self.under_try = saved;
        let call = call?;
        if let TK::Call(f, _) = &call.kind {
            self.mut_errs = self.w.funcs[*f].as_ref().map(|f| f.errs.clone());
        }
        let rty = call.ty.clone();
        if matches!(self.resolve(&rty), Ty::Unit) {
            let tl = self.mk(TK::Local(tid), Ty::arr(pty.clone()), sp);
            let z = self.mk(TK::Int(0), Ty::Int, sp);
            let back = self.mk(TK::Index(Box::new(tl), Box::new(z)), pty.clone(), sp);
            let wb = if steps.is_empty() { self.mk(TK::Assign(id, Box::new(back)), pty, sp) } else { self.mk(TK::PlaceAssign(id, steps, None, Box::new(back)), pty, sp) };
            let u = self.mk(TK::Unit, Ty::Unit, sp);
            return Ok(self.mk(TK::Seq(vec![s1, TStmt::Expr(call), TStmt::Expr(wb), TStmt::Expr(u)]), Ty::Unit, sp));
        }
        let (rid, s2) = self.opt_tmp(call, sp);
        let tl = self.mk(TK::Local(tid), Ty::arr(pty.clone()), sp);
        let z = self.mk(TK::Int(0), Ty::Int, sp);
        let back = self.mk(TK::Index(Box::new(tl), Box::new(z)), pty.clone(), sp);
        let wb = if steps.is_empty() { self.mk(TK::Assign(id, Box::new(back)), pty, sp) } else { self.mk(TK::PlaceAssign(id, steps, None, Box::new(back)), pty, sp) };
        let r = self.mk(TK::Local(rid), rty.clone(), sp);
        Ok(self.mk(TK::Seq(vec![s1, s2, TStmt::Expr(wb), TStmt::Expr(r)]), rty, sp))
    }

    /// The one-element slice a `!` call gets its receiver in: one per call
    /// site, made on first use and reused after (a loop allocates it once).
    ///
    /// `share`: the call's arguments are evaluated before the cell is filled
    /// (`mutating_call`), so no other `!` call on the same type can run while
    /// the cell is in use, and call sites with the same receiver type share
    /// one cell. That keeps frames small in recursive methods with many `!`
    /// calls (std/regexp/syntax's printer and compiler).
    fn call_cell(&mut self, cur: TExpr, pty: &Ty, sp: Span, share: bool) -> (LocalId, TStmt) {
        let n = self.locals.len();
        let at = Ty::arr(pty.clone());
        if self.cells.is_none() {
            // A lambda or task body: a fresh cell per call.
            let arr = self.mk(TK::Array(vec![cur]), at, sp);
            return self.opt_tmp(arr, sp);
        }
        let shared = if share { self.cells.as_ref().unwrap().iter().find(|(_, t)| *t == at).map(|(c, _)| *c) } else { None };
        let cell = match shared {
            Some(c) => c,
            None => self.declare(&format!("__cell{n}"), at.clone()),
        };
        let l = |ck: &mut Self| ck.mk(TK::Local(cell), at.clone(), sp);
        let size = { let c = l(self); self.mk(TK::M(M::Size, Some(Box::new(c)), vec![], None), Ty::Int, sp) };
        let zero = self.mk(TK::Int(0), Ty::Int, sp);
        let fresh = self.mk(TK::Bin(BinOp::Eq, Box::new(size), Box::new(zero)), Ty::Bool, sp);
        if shared.is_none() {
            self.cells.as_mut().unwrap().push((cell, at.clone()));
        }
        self.locals[cell].mutated = true;
        self.locals[cell].pushed = true;
        let c = l(self);
        let make = self.mk(TK::M(M::Push, Some(Box::new(c)), vec![cur.clone()], None), Ty::Unit, sp);
        let z = self.mk(TK::Int(0), Ty::Int, sp);
        let reuse = self.mk(TK::IndexAssign(cell, Box::new(z), Box::new(cur)), pty.clone(), sp);
        (cell, TStmt::If(fresh, vec![TStmt::Expr(make)], vec![TStmt::Expr(reuse)]))
    }

    /// `w.m!(args)` where `w` is an interface value in a place: dispatch on
    /// a one-element slice holding the value, then write it back.
    fn mutating_iface_call(&mut self, recv: &Expr, iname: &str, k: usize, m: &IfaceMethod, args: &[Expr], sp: Span) -> R<TExpr> {
        let (id, steps, pty) = self.place(recv)?;
        let cur = self.place_read(id, &steps, &pty, recv.span);
        let (tid, s1) = self.call_cell(cur, &pty, recv.span, false);
        if args.len() != m.params.len() {
            return Err(Diag::new(sp, format!("`{iname}.{}` takes {} argument(s), got {}", m.name, m.params.len(), args.len())));
        }
        let mut targs = vec![];
        for (a, pt) in args.iter().zip(&m.params) {
            let v = self.value(a)?;
            let v = self.coerce(v, pt)?;
            self.expect(&v.ty, pt, v.span, "argument")?;
            targs.push(v);
        }
        self.impure = true;
        let tl = self.mk(TK::Local(tid), Ty::arr(pty.clone()), sp);
        let call = self.mk(TK::M(M::IfaceCall(k), Some(Box::new(tl)), targs, None), m.ret.clone(), sp);
        let rty = call.ty.clone();
        let (rid, s2) = self.opt_tmp(call, sp);
        let tl = self.mk(TK::Local(tid), Ty::arr(pty.clone()), sp);
        let z = self.mk(TK::Int(0), Ty::Int, sp);
        let back = self.mk(TK::Index(Box::new(tl), Box::new(z)), pty.clone(), sp);
        let wb = if steps.is_empty() { self.mk(TK::Assign(id, Box::new(back)), pty, sp) } else { self.mk(TK::PlaceAssign(id, steps, None, Box::new(back)), pty, sp) };
        let r = self.mk(TK::Local(rid), rty.clone(), sp);
        Ok(self.mk(TK::Seq(vec![s1, s2, TStmt::Expr(wb), TStmt::Expr(r)]), rty, sp))
    }

    /// The type of a local or a field path, without checking anything.
    fn peek_ty(&self, e: &Expr) -> Option<Ty> {
        if let Some(f) = self.self_field(e) {
            return self.peek_ty(&f);
        }
        match &e.kind {
            ExprKind::Name(n) if n == "self" && self.method.is_some() => self.self_struct(),
            ExprKind::Name(n) => self.lookup(n).map(|id| self.resolve(&self.locals[id].ty)),
            ExprKind::Call { recv: Some(r), name, args, block: None, .. } if args.is_empty() => self.peek_ty(r)?.field(name).map(|(_, t)| self.resolve(&t)),
            ExprKind::Index(a, i) if !matches!(i.kind, ExprKind::SliceRange(..)) => self.peek_ty(a)?.arr_elem().map(|t| self.resolve(&t)),
            _ => None,
        }
    }

    fn is_map_index(&self, target: &Expr) -> bool {
        match &target.kind {
            ExprKind::Index(a, i) if !matches!(i.kind, ExprKind::SliceRange(..)) => matches!(self.peek_ty(a), Some(Ty::Map(..))),
            _ => false,
        }
    }

    /// `m[k] = v`, and `m[k] op= v` (a missing key reads as V's zero, as in Go).
    fn map_assign(&mut self, e: &Expr) -> R<TExpr> {
        let sp = e.span;
        let (op, target, v) = match &e.kind {
            ExprKind::Assign(t, v) => (None, t, v),
            ExprKind::OpAssign(op, t, v) => (Some(*op), t, v),
            _ => unreachable!(),
        };
        let ExprKind::Index(a, i) = &target.kind else { unreachable!() };
        if let ExprKind::Name(n) = &a.kind {
            if let Some(id) = self.lookup(n) {
                if self.pure_decl && id < self.param_count() {
                    return Err(Diag::new(sp, format!("`#[pure] def {}` can't mutate its argument `{n}`", self.fn_name)));
                }
                self.locals[id].mutated = true;
            }
        }
        let m = self.value(a)?;
        let Ty::Map(kt, vt) = self.resolve(&m.ty) else { unreachable!() };
        let k = self.value(i)?;
        let k = self.coerce(k, &kt)?;
        self.expect(&k.ty, &kt, k.span, "map key")?;
        let rhs = self.value(v)?;
        let Some(op) = op else {
            let rhs = self.coerce(rhs, &vt)?;
            self.expect(&rhs.ty, &vt, rhs.span, "map value")?;
            return Ok(self.mk(TK::M(M::MapSet, Some(Box::new(m)), vec![k, rhs], None), Ty::Unit, sp));
        };
        // Evaluate the map and key once.
        let (mid, s1) = self.opt_tmp(m, a.span);
        let (kid, s2) = self.opt_tmp(k, i.span);
        let mt = Ty::Map(kt.clone(), vt.clone());
        let ml = |cx: &mut Self| cx.mk(TK::Local(mid), mt.clone(), a.span);
        let kl = |cx: &mut Self| cx.mk(TK::Local(kid), (*kt).clone(), i.span);
        let Some(zero) = self.zero_of(&vt, sp) else {
            return Err(Diag::new(sp, format!("`{}=` on a map needs a value type with a zero value, not {}", op.text(), vt.show())));
        };
        let (m1, k1) = (ml(self), kl(self));
        let cur = self.mk(TK::M(M::MapGetOr, Some(Box::new(m1)), vec![k1, zero], None), (*vt).clone(), sp);
        let new = self.binary(op, cur, rhs, sp)?;
        let new = self.coerce(new, &vt)?;
        self.expect(&new.ty, &vt, sp, "map value")?;
        let (m2, k2) = (ml(self), kl(self));
        let set = self.mk(TK::M(M::MapSet, Some(Box::new(m2)), vec![k2, new], None), Ty::Unit, sp);
        Ok(self.mk(TK::Seq(vec![s1, s2, TStmt::Expr(set)]), Ty::Unit, sp))
    }

    fn opt_tmp(&mut self, v: TExpr, sp: Span) -> (LocalId, TStmt) {
        let ty = v.ty.clone();
        let id = self.declare(&format!("_opt{}_{}", sp.lo, self.locals.len()), ty.clone());
        (id, TStmt::Expr(self.mk(TK::Assign(id, Box::new(v)), ty, sp)))
    }
    fn opt_present(&mut self, id: LocalId, ty: &Ty, sp: Span) -> TExpr {
        let l = self.mk(TK::Local(id), ty.clone(), sp);
        self.mk(TK::M(M::OptPresent, Some(Box::new(l)), vec![], None), Ty::Bool, sp)
    }
    fn opt_get(&mut self, id: LocalId, ty: &Ty, sp: Span) -> TExpr {
        let Ty::Opt(inner) = self.resolve(ty) else { unreachable!() };
        let l = self.mk(TK::Local(id), ty.clone(), sp);
        self.mk(TK::M(M::OptGet, Some(Box::new(l)), vec![], None), *inner, sp)
    }

    /// `if x = e` / `while x = e` where `e` is a T?: the condition tests
    /// presence; the returned binding gives `x` the value in the branch.
    fn opt_let(&mut self, c: &Expr) -> R<Option<(TExpr, String, LocalId, Ty)>> {
        let ExprKind::Assign(t, rhs) = &c.kind else { return Ok(None) };
        let ExprKind::Name(n) = &t.kind else { return Ok(None) };
        let v = self.value(rhs)?;
        let vt = self.resolve(&v.ty);
        if !matches!(vt, Ty::Opt(_)) {
            return Err(Diag::new(c.span, format!("`if {n} = ...` unwraps a T?, but this is {}", vt.show())).note("to compare, use `==`"));
        }
        let (tmp, pre) = self.opt_tmp(v, c.span);
        let present = self.opt_present(tmp, &vt, c.span);
        let cond = self.mk(TK::Seq(vec![pre, TStmt::Expr(present)]), Ty::Bool, c.span);
        Ok(Some((cond, n.clone(), tmp, vt)))
    }

    /// Declare the unwrapped binding at the start of a branch.
    fn opt_bind(&mut self, name: &str, tmp: LocalId, ty: &Ty, sp: Span) -> TStmt {
        let v = self.opt_get(tmp, ty, sp);
        let id = self.declare(name, v.ty.clone());
        let vty = v.ty.clone();
        TStmt::Expr(self.mk(TK::Assign(id, Box::new(v)), vty, sp))
    }

    /// `if` as a value: the branches' values if they agree, else nil.
    /// `used`: the value is required, so the branches must agree; otherwise
    /// (a statement) they may differ and the `if` then has no value.
    fn if_value(&mut self, c: &Expr, a: &[Stmt], b: &[Stmt], sp: Span, used: bool) -> R<TExpr> {
        let (c, ta) = match self.opt_let(c)? {
            Some((cond, name, tmp, ty)) => {
                self.scopes.push(HashMap::new());
                let bind = self.opt_bind(&name, tmp, &ty, sp);
                let ta = self.seq_body(a, sp, used);
                self.pop_scope();
                let mut ta = ta?;
                if let TK::Seq(ss) = &mut ta.kind {
                    ss.insert(0, bind);
                }
                (cond, ta)
            }
            None => {
                let c = self.cond(c)?;
                // Branches share the enclosing scope, as statement `if`s do (Ruby).
                (c, self.seq_body(a, sp, used)?)
            }
        };
        let tb = self.seq_body(b, sp, used)?;
        // A discarded value isn't joined: coercing one branch to the other's
        // type (an Int branch to the other's Error?) would mistype it.
        let (ta, tb) = if used { self.join(ta, tb)? } else { (ta, tb) };
        let (ra, rb) = (self.resolve(&ta.ty), self.resolve(&tb.ty));
        let (ta, tb, ty) = if !b.is_empty() && self.unify(&ra, &rb) {
            let ty = if matches!(ra, Ty::Never) { rb } else { ra };
            (ta, tb, ty)
        } else if used && !b.is_empty() {
            let at = match &tb.kind {
                TK::Seq(ss) => match ss.last() {
                    Some(TStmt::Expr(e)) => e.span,
                    _ => tb.span,
                },
                _ => tb.span,
            };
            return Err(Diag::new(at, format!("this branch is {}, but the other is {}", rb.show(), ra.show())).note("an `if` used as a value needs branches of one type; as a statement, they may differ"));
        } else {
            let unit = |cx: &mut Self, x: TExpr| {
                let span = x.span;
                let TK::Seq(mut ss) = x.kind else { unreachable!() };
                ss.push(TStmt::Expr(cx.mk(TK::Unit, Ty::Unit, span)));
                cx.mk(TK::Seq(ss), Ty::Unit, span)
            };
            (unit(self, ta), unit(self, tb), Ty::Unit)
        };
        Ok(self.mk(TK::Ternary(Box::new(c), Box::new(ta), Box::new(tb)), ty, sp))
    }

    fn seq_body(&mut self, body: &[Stmt], sp: Span, used: bool) -> R<TExpr> {
        let (mut ss, ty) = self.body_as(body, used)?;
        if let Some(TStmt::Expr(e)) = ss.last_mut() {
            let m = self.materialize(e.clone());
            *e = m;
        }
        let ty = match ss.last() {
            Some(TStmt::Expr(e)) => e.ty.clone(),
            _ if matches!(ty, Ty::Never) => Ty::Never,
            _ => Ty::Unit,
        };
        Ok(self.mk(TK::Seq(ss), ty, sp))
    }

    /// An arm's body, with pattern bindings declared first.
    fn arm_body_with(&mut self, binds: Vec<(String, Ty, TExpr)>, body: &[Stmt], sp: Span, used: bool) -> R<TExpr> {
        self.scopes.push(HashMap::new());
        let mut pre = vec![];
        for (n, t, v) in binds {
            let id = self.declare(&n, t.clone());
            pre.push(TStmt::Expr(self.mk(TK::Assign(id, Box::new(v)), t, sp)));
        }
        let r = self.body_as(body, used);
        self.pop_scope();
        let (ss, ty) = r?;
        let mut ss: Vec<TStmt> = pre.into_iter().chain(ss).collect();
        if let Some(TStmt::Expr(e)) = ss.last_mut() {
            let m = self.materialize(e.clone());
            *e = m;
        }
        let ty = match ss.last() {
            Some(TStmt::Expr(e)) => e.ty.clone(),
            _ if matches!(ty, Ty::Never) => Ty::Never,
            _ => Ty::Unit,
        };
        Ok(self.mk(TK::Seq(ss), ty, sp))
    }

    /// An index: any integer type (a constant defaults to Int).
    fn index_value(&mut self, e: &Expr) -> R<TExpr> {
        let i = self.value(e)?;
        let i = self.coerce(i, &Ty::Int)?;
        let t = self.resolve(&i.ty);
        if t.int_kind().is_none() {
            self.expect(&i.ty, &Ty::Int, i.span, "index")?;
        }
        Ok(i)
    }

    /// The array constant `e` names (`C`, `pkg.C`), if it does: its
    /// qualified name, value and type.
    fn const_array(&self, e: &Expr) -> Option<(String, CVal, Ty)> {
        let q = match &e.kind {
            ExprKind::Const(c) => resolve_name(c, e.span, &|q| self.w.consts.contains_key(q)).ok()?,
            ExprKind::Call { recv: Some(r), name, args, block: None, .. } if args.is_empty() => Some(format!("{}.{name}", self.pkg_alias(r)?)).filter(|q| is_public(q))?,
            _ => return None,
        };
        match self.w.consts.get(&q) {
            Some((v @ CVal::Arr(_), Some(t))) => Some((q, v.clone(), t.clone())),
            _ => None,
        }
    }

    /// `C[i]`, `C[i][j]`, ... of an array constant `C`, when the element read
    /// shares no storage: read in place from the constant's global, so a
    /// lookup in a loop allocates nothing. (Anything else gets a fresh copy.)
    fn const_read(&mut self, e: &Expr) -> R<Option<TExpr>> {
        let mut idxs = vec![];
        let mut base = e;
        while let ExprKind::Index(a, i) = &base.kind {
            if matches!(i.kind, ExprKind::SliceRange(..)) {
                return Ok(None);
            }
            idxs.push(&**i);
            base = a;
        }
        let Some((name, v, ty)) = self.const_array(base) else { return Ok(None) };
        let mut el = ty.clone();
        for _ in &idxs {
            match el.arr_elem() {
                Some(t) => el = t,
                None => return Ok(None),
            }
        }
        if !storage_free(&el) {
            return Ok(None);
        }
        let k = self.w.const_global(&name, &v, &ty);
        let mut cur = self.mk(TK::M(M::Global(k), None, vec![], None), ty, base.span);
        for i in idxs.into_iter().rev() {
            let el = cur.ty.arr_elem().unwrap();
            let i = self.index_value(i)?;
            let sp = cur.span.to(i.span);
            cur = self.mk(TK::Index(Box::new(cur), Box::new(i)), el, sp);
        }
        Ok(Some(cur))
    }

    /// A top-level constant used as a value.
    fn const_value(&mut self, v: CVal, ty: Option<Ty>, sp: Span) -> R<TExpr> {
        Ok(match (v, ty) {
            // A fresh array per use: changing it never changes the constant.
            (v @ CVal::Arr(_), Some(t)) => const_lit(&v, &t, sp),
            (CVal::Arr(_), None) => unreachable!("array constants are typed"),
            (CVal::Enum(_, k), Some(et @ Ty::Enum(..))) => {
                let Ty::Enum(_, vs) = &et else { unreachable!() };
                let mut slots = vec![];
                for (_, fs) in vs {
                    for (_, ft) in fs {
                        slots.push(self.zero_of(ft, sp).ok_or_else(|| Diag::new(sp, format!("{} has no zero value", ft.show())))?);
                    }
                }
                self.mk(TK::M(M::VariantNew(k), None, slots, None), et.clone(), sp)
            }
            (CVal::Enum(..), _) => unreachable!("enum constants are typed"),
            (CVal::Str(s), _) => self.mk(TK::Str(s), Ty::Str, sp),
            (CVal::Bool(b), _) => self.mk(TK::Bool(b), Ty::Bool, sp),
            (CVal::Num(n), None) => {
                let ty = if matches!(n, ConstVal::Float(_)) { Ty::Float } else { Ty::Int };
                self.mk(TK::Const(n), ty, sp)
            }
            (CVal::Num(n), Some(t)) => {
                // A typed constant is a plain value of its type.
                let e = self.mk(TK::Const(n), t.clone(), sp);
                let e = self.coerce(e, &t)?;
                let TK::Const(n) = &e.kind else { unreachable!() };
                let kind = match t.int_kind() {
                    Some(k) => TK::Int(consts::fit(&consts::as_int(n).unwrap(), k).unwrap()),
                    None => TK::Float(consts::to_f64(&match n {
                        ConstVal::Int(i) => num_rational::BigRational::from_integer(i.clone()),
                        ConstVal::Float(q) => q.clone(),
                    })),
                };
                self.mk(kind, t, sp)
            }
        })
    }

    fn param_count(&self) -> usize {
        self.n_params
    }

    fn binary(&mut self, op: BinOp, l: TExpr, r: TExpr, sp: Span) -> R<TExpr> {
        if op == BinOp::Or {
            if let Ty::Opt(inner) = self.resolve(&l.ty) {
                // `opt || default`: the value if present (the default is evaluated only if not).
                let lt = l.ty.clone();
                let (tmp, pre) = self.opt_tmp(l, sp);
                let r = self.coerce(r, &inner)?;
                let rt = self.resolve(&r.ty);
                let (value, ty) = if matches!(rt, Ty::Opt(_)) {
                    if !self.unify(&r.ty, &lt) {
                        return Err(Diag::new(r.span, format!("`||` default is {}, but the value is {}", rt.show(), self.resolve(&lt).show())));
                    }
                    (self.mk(TK::Local(tmp), lt.clone(), sp), lt.clone())
                } else {
                    if !self.unify(&r.ty, &inner) {
                        return Err(Diag::new(r.span, format!("`||` default is {}, but the value is {}", rt.show(), inner.show())));
                    }
                    (self.opt_get(tmp, &lt, sp), (*inner).clone())
                };
                let present = self.opt_present(tmp, &lt, sp);
                let t = self.mk(TK::Ternary(Box::new(present), Box::new(value), Box::new(r)), ty.clone(), sp);
                return Ok(self.mk(TK::Seq(vec![pre, TStmt::Expr(t)]), ty, sp));
            }
        }
        if matches!(op, BinOp::And | BinOp::Or) {
            self.expect(&l.ty, &Ty::Bool, l.span, &format!("`{}`", op.text()))?;
            self.expect(&r.ty, &Ty::Bool, r.span, &format!("`{}`", op.text()))?;
            return Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), Ty::Bool, sp));
        }
        // Operators on structs: their methods (`def +(o)`, `def ==(o)`, `def <=>(o)`).
        let lres = self.resolve(&l.ty);
        if let Some(sn) = lres.type_name() {
            let sn = sn.to_string();
            let sn = &sn;
            let m = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Rem => "%",
                BinOp::Eq | BinOp::Ne => "==",
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => "<=>",
                _ => "",
            };
            if let Some(&def) = self.w.by_name.get(&method_name(sn, m)) {
                let call = self.call_def(def, m, sp, vec![l, r], sp)?;
                return Ok(match op {
                    BinOp::Ne => {
                        self.expect(&call.ty, &Ty::Bool, sp, "`==`")?;
                        self.mk(TK::Not(Box::new(call)), Ty::Bool, sp)
                    }
                    BinOp::Eq => {
                        self.expect(&call.ty, &Ty::Bool, sp, "`==`")?;
                        call
                    }
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        self.expect(&call.ty, &Ty::Int, sp, "`<=>`")?;
                        let z = self.mk(TK::Int(0), Ty::Int, sp);
                        self.mk(TK::Bin(op, Box::new(call), Box::new(z)), Ty::Bool, sp)
                    }
                    _ => call,
                });
            }
            if !m.is_empty() && !matches!(op, BinOp::Eq | BinOp::Ne) {
                return Err(Diag::new(sp, format!("`{}` on {sn}: define `def {m}(other)` inside `struct {sn}`", op.text())));
            }
        }
        if let (BinOp::Eq | BinOp::Ne, Ty::Handle(_) | Ty::Ptr) = (op, &lres) {
            if !self.unify(&l.ty, &r.ty) {
                return Err(Diag::new(sp, format!("`{}` between {} and {}", op.text(), lres.show(), self.resolve(&r.ty).show())));
            }
            return Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), Ty::Bool, sp));
        }
        // `a + b` on slices: a new slice holding both (Ruby's Array#+, Go's
        // append(a[:len(a):len(a)], b...)).
        if op == BinOp::Add && matches!(lres, Ty::Array(_)) {
            if !self.unify(&l.ty, &r.ty) {
                return Err(Diag::new(sp, format!("`+` between {} and {}", lres.show(), self.resolve(&r.ty).show())));
            }
            let t = l.ty.clone();
            return Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), t, sp));
        }
        // `s * n`: s repeated n times (Ruby's String#*, Go's strings.Repeat).
        if op == BinOp::Mul && lres == Ty::Str {
            let r = self.coerce(r, &Ty::Int)?;
            self.expect(&r.ty, &Ty::Int, r.span, "the count in `Str * n`")?;
            return Ok(self.mk(TK::M(M::StrHelper(7), Some(Box::new(l)), vec![r], None), Ty::Str, sp));
        }
        // Slices and fixed arrays compare element by element (Ruby's Array#==).
        if matches!(op, BinOp::Eq | BinOp::Ne) && matches!(lres, Ty::Array(_) | Ty::Fixed(..)) {
            if !self.unify(&l.ty, &r.ty) {
                return Err(Diag::new(sp, format!("`{}` between {} and {}", op.text(), lres.show(), self.resolve(&r.ty).show())));
            }
            let eq = self.arr_eq(l, r, sp)?;
            return Ok(if op == BinOp::Ne { self.mk(TK::Not(Box::new(eq)), Ty::Bool, sp) } else { eq });
        }
        if matches!(op, BinOp::Eq | BinOp::Ne) && matches!(lres, Ty::Struct(..) | Ty::Tuple(_) | Ty::Enum(..)) {
            if !self.unify(&l.ty, &r.ty) {
                return Err(Diag::new(sp, format!("`{}` between {} and {}", op.text(), lres.show(), self.resolve(&r.ty).show())));
            }
            let eq = self.struct_eq(l, r, sp)?;
            return Ok(if op == BinOp::Ne { self.mk(TK::Not(Box::new(eq)), Ty::Bool, sp) } else { eq });
        }
        // Constants fold exactly (Go).
        if let (TK::Const(a), TK::Const(b)) = (&l.kind, &r.kind) {
            match consts::fold(op, a, b) {
                Err(m) => return Err(Diag::new(sp, m)),
                Ok(Some(v)) => {
                    let (lt, rt) = (self.resolve(&l.ty), self.resolve(&r.ty));
                    let ty = if lt == rt && lt.int_kind().is_some() && lt != Ty::Int {
                        lt
                    } else if matches!(v, ConstVal::Float(_)) {
                        Ty::Float
                    } else {
                        Ty::Int
                    };
                    let e = self.mk(TK::Const(v), ty.clone(), sp);
                    // An Int result stays exact until it gets its final type
                    // (`v: U64 = 1 << 63`); zonk checks that it fits.
                    return if ty == Ty::Int { Ok(e) } else { self.coerce(e, &ty) };
                }
                Ok(None) => {}
            }
        }
        let is_const = |e: &TExpr| matches!(e.kind, TK::Const(_));
        let shift = matches!(op, BinOp::Shl | BinOp::Shr);
        // A constant operand takes the other operand's type.
        let (l, r) = if shift {
            let r = if is_const(&r) { self.coerce(r, &Ty::Int)? } else { r };
            let l = if is_const(&l) { self.coerce(l, &Ty::Int)? } else { l };
            (l, r)
        } else if is_const(&l) && !is_const(&r) {
            let rt = self.resolve(&r.ty);
            (if matches!(rt, Ty::Var(_)) { l } else { self.coerce(l, &rt)? }, r)
        } else if is_const(&r) && !is_const(&l) {
            let lt = self.resolve(&l.ty);
            let r = if matches!(lt, Ty::Var(_)) { r } else { self.coerce(r, &lt)? };
            (l, r)
        } else {
            (l, r)
        };
        let (lt, rt) = (self.resolve(&l.ty), self.resolve(&r.ty));
        if shift {
            let k = match &lt {
                Ty::Var(_) => {
                    let t = self.unknown(l.span, "the shifted value")?;
                    return Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), t, sp));
                }
                t => t.int_kind().ok_or_else(|| Diag::new(l.span, format!("`{}` needs an integer, got {}", op.text(), t.show())))?,
            };
            if rt.int_kind().is_none() && !matches!(rt, Ty::Var(_)) {
                return Err(Diag::new(r.span, format!("a shift count must be an integer, got {}", rt.show())));
            }
            return Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), Ty::of_kind(k), sp));
        }
        // Float arithmetic and comparison: an Int operand must be a literal.
        if lt == Ty::Float || rt == Ty::Float {
            for (e, t) in [(&l, &lt), (&r, &rt)] {
                if t.int_kind().is_some() {
                    return Err(Diag::new(e.span, format!("`{}` mixes Float and {}: convert with `.to_f`", op.text(), t.show())).note("only numeric constants convert implicitly (as in Go)"));
                }
                if !matches!(t, Ty::Float | Ty::Var(_)) {
                    return Err(Diag::new(e.span, format!("`{}` needs numbers, got {}", op.text(), t.show())));
                }
            }
            if !(op.is_arith() || matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)) || matches!(op, BinOp::Rem | BinOp::Pow) {
                return Err(Diag::new(sp, format!("`{}` isn't defined on Float", op.text())));
            }
            self.unify(&l.ty, &Ty::Float);
            self.unify(&r.ty, &Ty::Float);
            let ty = if op.is_arith() { Ty::Float } else { Ty::Bool };
            return Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), ty, sp));
        }
        // `opt == value`: the plain side is wrapped (equal if opt holds it).
        let (l, r, lt, rt) = if matches!(op, BinOp::Eq | BinOp::Ne) {
            match (self.resolve(&lt), self.resolve(&rt)) {
                (Ty::Opt(inner), other) if !matches!(other, Ty::Opt(_) | Ty::Var(_)) => {
                    let r = self.coerce(r, &inner)?;
                    let rsp = r.span;
                    let r = self.mk(TK::Some(Box::new(r)), Ty::Opt(inner.clone()), rsp);
                    let rt = r.ty.clone();
                    (l, r, lt, rt)
                }
                (other, Ty::Opt(inner)) if !matches!(other, Ty::Opt(_) | Ty::Var(_)) => {
                    let l = self.coerce(l, &inner)?;
                    let lsp = l.span;
                    let l = self.mk(TK::Some(Box::new(l)), Ty::Opt(inner.clone()), lsp);
                    let lt = l.ty.clone();
                    (l, r, lt, rt)
                }
                _ => (l, r, lt, rt),
            }
        } else {
            (l, r, lt, rt)
        };
        let mismatch = |cx: &Self| {
            let (a, b) = (cx.resolve(&l.ty), cx.resolve(&r.ty));
            let hint = match (a.int_kind(), b.int_kind()) {
                (Some(_), Some(k)) => format!("convert one side, e.g. `.to_{}`", k.method()),
                _ => String::new(),
            };
            let d = Diag::new(sp, format!("mismatched types {} and {} for `{}`", a.show(), b.show(), op.text()));
            if hint.is_empty() { d } else { d.note(hint) }
        };
        let ty = match op {
            BinOp::Eq | BinOp::Ne => {
                if !self.unify(&lt, &rt) {
                    return Err(mismatch(self));
                }
                // Optionals: equal if both are none or both hold equal values.
                if matches!(self.resolve(&lt), Ty::Opt(_)) {
                    let eq = self.opt_eq(l, r, sp)?;
                    return Ok(if op == BinOp::Ne { self.mk(TK::Not(Box::new(eq)), Ty::Bool, sp) } else { eq });
                }
                if matches!(self.resolve(&lt), Ty::Struct(..) | Ty::Tuple(_) | Ty::Array(_)) {
                    return Err(Diag::new(sp, format!("`{}` on {} values isn't supported yet; compare their fields", op.text(), self.resolve(&lt).show())));
                }
                Ty::Bool
            }
            _ => {
                let known = if !matches!(lt, Ty::Var(_)) { lt.clone() } else { rt.clone() };
                let operand = match known {
                    Ty::Var(_) => self.unknown(sp, "this operand")?,
                    Ty::Str if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) => Ty::Str,
                    t => t,
                };
                if !matches!(operand, Ty::Var(_)) {
                    if !self.unify(&l.ty, &operand) || !self.unify(&r.ty, &operand) {
                        return Err(mismatch(self));
                    }
                    let k = operand.int_kind();
                    if k.is_none() && !(operand == Ty::Str && matches!(op, BinOp::Add | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)) {
                        return Err(Diag::new(sp, format!("`{}` needs numbers, got {}", op.text(), operand.show())));
                    }
                    if op == BinOp::Pow && operand != Ty::Int {
                        return Err(Diag::new(sp, format!("`**` is only defined on Int, got {}", operand.show())));
                    }
                }
                if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) { Ty::Bool } else { operand }
            }
        };
        Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), ty, sp))
    }

    // ---------- calls ----------

    #[allow(clippy::too_many_arguments)]
    fn call(&mut self, recv: Option<&Expr>, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, bsym: Option<&(String, Span)>, sp: Span) -> R<TExpr> {
        // `geom.area(x)`, `geom.PI`, `geom.Point.new(...)`: through an import.
        if let Some(r) = recv {
            if let Some(path) = self.pkg_alias(r) {
                let q = format!("{path}.{name}");
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    if self.w.consts.contains_key(&q) && args.is_empty() {
                        if !is_public(&q) {
                            return Err(Diag::new(name_span, format!("`{name}` isn't public; its package must declare it `pub`")));
                        }
                        let (v, ty) = self.w.consts[&q].clone();
                        return self.const_value(v, ty, sp);
                    }
                    return Err(Diag::new(name_span, format!("`{name}` is a type (or isn't in that package); call a method on it")));
                }
                if path == "fmt" && crate::front::is_std("fmt") && FMT_BUILTINS.contains(&name) {
                    return self.fmt_call(name, args, sp);
                }
                if !self.w.by_name.contains_key(&q) {
                    return Err(Diag::new(name_span, format!("package `{}` has no `{name}`", path)));
                }
                return self.global_call(&q, name_span, args, block, bsym, sp);
            }
            if let ExprKind::Call { recv: Some(r2), name: tn, args: a2, block: None, name_span: tsp, .. } = &r.kind {
                if a2.is_empty() && tn.chars().next().is_some_and(|c| c.is_uppercase()) {
                    // (`geom.ORIGIN.size` is a method of the constant's value.)
                    if let Some(path) = self.pkg_alias(r2).filter(|p| !self.w.consts.contains_key(&format!("{p}.{tn}"))) {
                        let alias_q = format!("{}.{tn}", match &r2.kind { ExprKind::Name(a) => a.clone(), ExprKind::Call { name, .. } => name.clone(), _ => unreachable!() });
                        let _ = path;
                        return self.const_call(&alias_q, *tsp, name, name_span, args, block, sp);
                    }
                }
            }
        }
        // `Stack[Int].new`: a generic type with explicit arguments.
        if let Some(Expr { kind: ExprKind::TypeApp(c, tes), span: csp, .. }) = recv {
            if c == "Pool" && tes.len() == 1 && name == "new" && args.is_empty() {
                let t = type_from(&tes[0], &self.w.structs, &self.w.consts)?;
                if t.type_name().is_none() {
                    return Err(Diag::new(*csp, format!("a pool holds a struct or enum (its handles are `@Name`), not {}", t.show())));
                }
                return Ok(self.mk(TK::M(M::PoolNew, None, vec![], None), Ty::Pool(Box::new(t)), sp));
            }
            if (c == "Mutex" || c == "Atomic") && tes.len() == 1 && name == "new" {
                let t = type_from(&tes[0], &self.w.structs, &self.w.consts)?;
                return self.sync_new(c, Some(t), *csp, args, sp);
            }
            if c == "Chan" && tes.len() == 1 && name == "new" {
                let t = type_from(&tes[0], &self.w.structs, &self.w.consts)?;
                let cap = match args {
                    [] => self.mk(TK::Int(0), Ty::Int, sp),
                    [a] => {
                        let v = self.value(a)?;
                        let v = self.coerce(v, &Ty::Int)?;
                        self.expect(&v.ty, &Ty::Int, v.span, "channel capacity")?;
                        v
                    }
                    _ => return Err(Diag::new(sp, "`Chan[T].new` takes a capacity (or nothing: unbuffered)")),
                };
                self.impure = true;
                return Ok(self.mk(TK::M(M::ChanNew, None, vec![cap], None), Ty::Chan(Box::new(t)), sp));
            }
            let c = &resolve_name(c, *csp, &|q| generic(q).is_some())?;
            let Some(g) = generic(c) else {
                return Err(Diag::new(*csp, format!("`{c}` isn't a generic type")));
            };
            let targs = tes.iter().map(|t| type_from(t, &self.w.structs, &self.w.consts)).collect::<R<Vec<_>>>()?;
            let t = instantiate(c, &g, targs, *csp, &self.w.structs, &self.w.consts)?;
            return self.const_call_named(c, Some(t), *csp, name, name_span, args, block, sp);
        }
        // `Mutex.new(v)`, `Atomic.new(v)` (and `Mutex[T].new(v)` above).
        // (A constant of the current package is keyed by its qualified name.)
        let is_const = |cx: &Self, c: &str, csp: Span| resolve_name(c, csp, &|q| cx.w.consts.contains_key(q)).is_ok_and(|q| cx.w.consts.contains_key(&q));
        if let Some(Expr { kind: ExprKind::Const(c), span: csp, .. }) = recv {
            if (c == "Mutex" || c == "Atomic") && name == "new" && !is_const(self, c, *csp) {
                return self.sync_new(c, None, *csp, args, sp);
            }
        }
        // Constant receivers: Int.sqrt, Array.new, File.read, Enumerator.new.
        if let Some(Expr { kind: ExprKind::Const(c), span: csp, .. }) = recv {
            if !is_const(self, c, *csp) {
                return self.const_call(c, *csp, name, name_span, args, block, sp);
            }
        }
        let Some(recv) = recv else {
            return self.global_call(name, name_span, args, block, bsym, sp);
        };
        // `a.b << x`, `grid[i] << x`: push through a place (read the slice
        // header, push, write it back).
        if matches!(name, "<<" | "push") && args.len() == 1 && block.is_none() {
            let is_local = matches!(&recv.kind, ExprKind::Name(n) if self.lookup(n).is_some() && !(n == "self" && self.method.is_some()));
            if !is_local && matches!(self.peek_ty(recv), Some(Ty::Array(_))) {
                let (id, steps, pty) = self.place(recv)?;
                let cur = self.place_read(id, &steps, &pty, recv.span);
                let (tid, s1) = self.opt_tmp(cur, recv.span);
                self.locals[tid].pushed = true;
                self.locals[tid].mutated = true;
                let el = self.resolve(&pty).arr_elem().unwrap();
                let x = self.value(&args[0])?;
                let x = self.coerce(x, &el)?;
                self.expect(&x.ty, &el, x.span, "pushed value")?;
                let tl = self.mk(TK::Local(tid), pty.clone(), recv.span);
                let push = self.mk(TK::M(M::Push, Some(Box::new(tl)), vec![x], None), Ty::Unit, sp);
                let back = self.mk(TK::Local(tid), pty.clone(), sp);
                let wb = if steps.is_empty() { self.mk(TK::Assign(id, Box::new(back)), pty, sp) } else { self.mk(TK::PlaceAssign(id, steps, None, Box::new(back)), pty, sp) };
                return Ok(self.mk(TK::Seq(vec![s1, TStmt::Expr(push), TStmt::Expr(wb)]), Ty::Unit, sp));
            }
        }
        if name.ends_with('!') {
            let st = match &recv.kind {
                ExprKind::Name(n) if n == "self" && self.method.is_some() => self.self_struct(),
                _ => self.peek_ty(recv),
            };
            if let Some(Ty::Iface(iname)) = &st {
                // `w.write!(x)` on an interface value: the value is a place too.
                let ms = self.w.ifaces[iname].clone();
                if let Some(k) = ms.iter().position(|m| m.name == name) {
                    return self.mutating_iface_call(recv, iname, k, &ms[k], args, sp);
                }
            }
            if let Some(sn) = st.as_ref().and_then(|t| t.type_name()) {
                if let Some(&def) = self.w.by_name.get(&method_name(sn, name)) {
                    if block.is_some() {
                        return Err(Diag::new(sp, format!("`{name}` doesn't take a block")));
                    }
                    return self.mutating_call(recv, def, name, name_span, args, sp);
                }
            }
        }
        let r = self.expr(recv)?;
        self.method(r, name, name_span, args, block, bsym, sp)
    }

    fn global_call(&mut self, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, bsym: Option<&(String, Span)>, sp: Span) -> R<TExpr> {
        if let Some(id) = self.lookup(name) {
            if let Ty::Fn(..) = self.resolve(&self.locals[id].ty) {
                let f = self.mk(TK::Local(id), self.locals[id].ty.clone(), name_span);
                return self.fn_call(f, args, sp);
            }
        }
        match name {
            "puts" | "print" | "p" => {
                if self.pure_decl {
                    return Err(Diag::new(sp, format!("`#[pure] def {}` can't do I/O: `{name}`", self.fn_name)));
                }
                self.impure = true;
                if args.len() != 1 {
                    return Err(Diag::new(sp, format!("`{name}` takes one argument")));
                }
                let v = self.value(&args[0])?;
                let v = self.show_value(v)?;
                if name == "print" {
                    // No newline (Ruby's print, Go's fmt.Print): the text as "#{v}".
                    let t = self.resolve(&v.ty);
                    if !printable(&t) {
                        return Err(Diag::new(args[0].span, format!("can't print a {} yet", t.show())));
                    }
                    let text = self.mk(TK::Format(vec![FmtPiece::Str(0)], vec![v]), Ty::Str, sp);
                    return Ok(self.mk(TK::M(M::PrintStr, None, vec![text], None), Ty::Unit, sp));
                }
                return Ok(self.mk(TK::Puts(Box::new(v)), Ty::Unit, sp));
            }
            "panic" if self.w.by_name.get("panic").is_none() => {
                if args.len() != 1 {
                    return Err(Diag::new(sp, "`panic` takes one argument: `panic(message)`"));
                }
                let m = self.value(&args[0])?;
                let m = self.show_value(m)?;
                return Ok(self.mk(TK::Panic(Box::new(m)), Ty::Never, sp));
            }
            "assert" if self.w.by_name.get("assert").is_none() => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Diag::new(sp, "`assert` takes a condition and maybe a message: `assert cond, \"why\"`"));
                }
                let c = self.value(&args[0])?;
                self.expect(&c.ty, &Ty::Bool, c.span, "`assert` condition")?;
                let text = format!("assertion failed: {} at {}", self.w.sm.snippet(args[0].span).trim(), self.w.sm.loc(sp));
                let msg = self.mk(TK::Str(text), Ty::Str, sp);
                // `assert cond, why`: the message goes after the source text.
                let msg = match args.get(1) {
                    Some(w) => {
                        let w = self.value(w)?;
                        let w = self.show_value(w)?;
                        let sep = self.mk(TK::Str(": ".into()), Ty::Str, sp);
                        self.mk(TK::Format(vec![FmtPiece::Str(0), FmtPiece::Str(1), FmtPiece::Str(2)], vec![msg, sep, w]), Ty::Str, sp)
                    }
                    None => msg,
                };
                let fail = self.mk(TK::Panic(Box::new(msg)), Ty::Unit, sp);
                let neg = self.mk(TK::Not(Box::new(c)), Ty::Bool, sp);
                return Ok(self.mk(TK::Seq(vec![TStmt::If(neg, vec![TStmt::Expr(fail)], vec![])]), Ty::Unit, sp));
            }
            "assert_eq" if self.w.by_name.get("assert_eq").is_none() => {
                if args.len() != 2 {
                    return Err(Diag::new(sp, "`assert_eq` takes two arguments: `assert_eq actual, expected`"));
                }
                let a = self.value(&args[0])?;
                let b = self.value(&args[1])?;
                let b = self.coerce(b, &a.ty)?;
                let (aid, s1) = self.opt_tmp(a.clone(), sp);
                let (bid, s2) = self.opt_tmp(b.clone(), sp);
                let la = self.mk(TK::Local(aid), a.ty.clone(), sp);
                let lb = self.mk(TK::Local(bid), b.ty.clone(), sp);
                let eq = self.binary(BinOp::Eq, la.clone(), lb.clone(), sp)?;
                let mut vals = vec![];
                for (v, x) in [(la, &args[0]), (lb, &args[1])] {
                    let v = self.show_value(v)?;
                    let t = self.resolve(&v.ty);
                    if !printable(&t) {
                        return Err(Diag::new(x.span, format!("can't show a {} in `assert_eq` yet", t.show())));
                    }
                    vals.push(v);
                }
                let pieces = vec![FmtPiece::Lit(format!("assert_eq failed at {}: got ", self.w.sm.loc(sp))), FmtPiece::Str(0), FmtPiece::Lit(", want ".into()), FmtPiece::Str(1)];
                let msg = self.mk(TK::Format(pieces, vals), Ty::Str, sp);
                let fail = self.mk(TK::Panic(Box::new(msg)), Ty::Unit, sp);
                let neg = self.mk(TK::Not(Box::new(eq)), Ty::Bool, sp);
                return Ok(self.mk(TK::Seq(vec![s1, s2, TStmt::If(neg, vec![TStmt::Expr(fail)], vec![])]), Ty::Unit, sp));
            }
            "loop" => {
                let Some(b) = block else { return Err(Diag::new(sp, "`loop` needs a block")) };
                self.loops.push(LoopKind::While);
                self.scopes.push(HashMap::new());
                let (body, _) = self.body_as(&b.body, false)?;
                self.pop_scope();
                self.loops.pop();
                // A loop nothing breaks out of never finishes (it returns, fails or panics).
                let ty = if breaks_out(&body) { Ty::Unit } else { Ty::Never };
                let blk = TBlock { params: vec![], destructure: false, body, pure: true, span: b.span , own: (0, 0) };
                return Ok(self.mk(TK::M(M::Loop, None, vec![], Some(Box::new(blk))), ty, sp));
            }
            "it" => return Err(Diag::new(sp, "`it` can only be used inside a block")),
            "format" | "sprintf" => return self.format(name, args, sp),
            "copy" if args.len() == 2 && !self.w.by_name.contains_key("copy") => {
                let dst = self.value(&args[0])?;
                let src = self.value(&args[1])?;
                let el = match self.resolve(&dst.ty) {
                    Ty::Array(t) | Ty::Fixed(t, _) => *t,
                    t => return Err(Diag::new(dst.span, format!("`copy` copies into a slice, not {}", t.show()))),
                };
                match self.resolve(&src.ty) {
                    Ty::Array(t) | Ty::Fixed(t, _) => self.expect(&t, &el, src.span, "copied elements")?,
                    t => return Err(Diag::new(src.span, format!("`copy` copies from a slice, not {}", t.show()))),
                }
                let dst = TExpr { ty: Ty::arr(el.clone()), ..dst };
                let src = TExpr { ty: Ty::arr(el), ..src };
                return Ok(self.mk(TK::M(M::CopyInto, None, vec![dst, src], None), Ty::Int, sp));
            }
            _ => {}
        }
        let _ = bsym;
        // Inside a method, a bare name is a field or method of `self`.
        if self.method.is_some() && self.lookup("self").is_some() {
            let st = self.self_struct();
            let tn = st.as_ref().and_then(|t| t.type_name()).map(str::to_string);
            if let Some(sn) = &tn {
                let fs: Vec<(String, Ty)> = match &st {
                    Some(Ty::Struct(_, fs)) => fs.clone(),
                    _ => vec![],
                };
                let is_field = args.is_empty() && block.is_none() && fs.iter().any(|(f, _)| f == name);
                // A method of `self` wins, unless only a function of that
                // name outside the struct takes these arguments.
                let own_fits = self.w.by_name.get(&method_name(sn, name)).is_some_and(|&d| self.w.defs[d].def.params.len() == args.len() + 1 || block.is_some());
                let outer_fits = resolve_name(name, name_span, &|q| self.w.by_name.contains_key(q)).ok().and_then(|q| self.w.by_name.get(&q).copied()).is_some_and(|d| self.w.defs[d].def.params.len() == args.len());
                let own = self.w.by_name.contains_key(&method_name(sn, name)) && (own_fits || !outer_fits);
                if is_field || own || self.w.default_for(sn, name).is_some() {
                    let me = Expr { kind: ExprKind::Name("self".into()), span: name_span, id: NodeId::MAX };
                    return self.call(Some(&me), name, name_span, args, block, bsym, sp);
                }
            }
        }
        let resolved = resolve_name(name, name_span, &|q| self.w.by_name.contains_key(q))?;
        let name = resolved.as_str();
        let Some(&def) = self.w.by_name.get(name) else {
            let mut msg = format!("undefined local variable or method `{name}`");
            let cands: Vec<String> = self.scopes.iter().flat_map(|s| s.keys().cloned()).chain(self.w.by_name.keys().cloned()).collect();
            if let Some(best) = cands.iter().filter(|c| c.as_str() != name && lev(c, name) <= 2).min_by_key(|c| lev(c, name)) {
                msg.push_str(&format!("; did you mean `{best}`?"));
            }
            return Err(Diag::new(name_span, msg));
        };
        let d = self.w.defs[def].def.clone();
        // A block for a last parameter of function type: `index_func(xs) { |x| ... }`.
        if let Some(b) = block {
            if d.params.len() != args.len() + 1 || !matches!(d.params.last().and_then(|p| p.ty.as_ref()), Some(TypeExpr::Fn(..))) {
                return Err(Diag::new(sp, format!("`{name}` doesn't take a block")));
            }
            let mut targs = vec![];
            for a in args {
                targs.push(self.value(a)?);
            }
            let f = self.block_as_lambda(def, &targs, b)?;
            targs.push(f);
            return self.call_def(def, name, name_span, targs, sp);
        }
        if args.len() != d.params.len() {
            return Err(Diag::new(name_span, format!("`{name}` takes {} argument(s), got {}", d.params.len(), args.len())));
        }
        let mut targs = vec![];
        for (a, p) in args.iter().zip(&d.params) {
            // A declared type (when it doesn't mention type parameters) guides literals.
            let v = match (&p.ty, self.w.defs[def].external.is_none()) {
                (Some(t), true) => match {
                    let prev = enter_pkg(&self.w.defs[def].pkg);
                    let r = type_from(t, &self.w.structs, &self.w.consts);
                    leave_pkg(prev);
                    r
                } {
                    Ok(t) => self.value_as(a, &t)?,
                    Err(_) => self.value(a)?,
                },
                _ => self.value(a)?,
            };
            targs.push(v);
        }
        self.call_def(def, name, name_span, targs, sp)
    }

    /// Call def `def` with checked arguments (a method's include `self`).
    fn call_def(&mut self, def: usize, name: &str, name_span: Span, args: Vec<TExpr>, sp: Span) -> R<TExpr> {
        let owner_targs = std::mem::take(&mut self.owner_targs);
        let d = self.w.defs[def].def.clone();
        let via_targ = d.name.rsplit_once('.').is_some_and(|(owner, _)| self.targ_types.iter().any(|t| t == owner));
        if !d.public && self.w.defs[def].pkg != current_pkg() && !via_targ {
            let shown = d.name.rsplit('/').next().unwrap_or(&d.name);
            return Err(Diag::new(name_span, format!("`{shown}` isn't public; its package must declare it `pub def`")));
        }
        let ext = self.w.defs[def].external.as_ref().map(|e| e.params.clone());
        if args.len() != d.params.len() {
            let own = usize::from(d.params.first().is_some_and(|p| p.name == "self"));
            return Err(Diag::new(name_span, format!("`{name}` takes {} argument(s), got {}", d.params.len() - own, args.len() - own)));
        }
        // The callee's signature means what it means in its own package.
        let prev = enter_pkg(&self.w.defs[def].pkg);
        let mut args = args;
        if !owner_targs.is_empty() {
            // `R[Byte].make(2, 7)`: the arguments adapt to the given types first.
            let env: HashMap<String, Ty> = d.tparams.iter().map(|tp| tp.name.clone()).zip(owner_targs.iter().cloned()).collect();
            for (a, p) in args.iter_mut().zip(&d.params) {
                if let Some(want) = p.ty.as_ref().and_then(|te| subst_type(te, &env, &self.w.structs, &self.w.consts)) {
                    match self.coerce(a.clone(), &want) {
                        Ok(v) => *a = v,
                        Err(e) => {
                            leave_pkg(prev);
                            return Err(e);
                        }
                    }
                }
            }
        }
        let mut arg_tys: Vec<Ty> = args.iter().map(|a| self.resolve(&a.ty)).collect();
        if !owner_targs.is_empty() {
            let mut out = HashMap::new();
            for (p, t) in d.params.iter().zip(&arg_tys) {
                if let Some(te) = &p.ty {
                    bind_tparams(te, t, &d.tparams, &mut out);
                }
            }
            for (tp, want) in d.tparams.iter().zip(&owner_targs) {
                if let Some(got) = out.get(&tp.name).filter(|got| *got != want && !got.has_var()) {
                    leave_pkg(prev);
                    return Err(Diag::new(name_span, format!("`{name}`: `{}` is {}, but the arguments make it {}", tp.name, want.show(), got.show())));
                }
            }
        }
        let extra = self.result_targs(&d, &arg_tys, &owner_targs);
        arg_tys.extend(extra.iter().cloned());
        let saved = match self.w.bind(def, &arg_tys, sp) {
            Ok(s) => s,
            Err(e) => {
                leave_pkg(prev);
                return Err(e);
            }
        };
        let r = self.call_def_args(name_span, &d, ext, args);
        self.w.unbind(saved);
        leave_pkg(prev);
        let targs = r?;
        self.call_def_inst(def, name, name_span, targs, extra, sp, &d)
    }

    /// Type parameters of a generic def that its arguments don't decide,
    /// taken from the type its result is wanted as (in tparam order).
    /// A static method's own type's arguments (`R[Str].empty`) come first.
    fn result_targs(&mut self, d: &Def, arg_tys: &[Ty], owner_targs: &[Ty]) -> Vec<Ty> {
        if d.tparams.is_empty() {
            return vec![];
        }
        let mut out = HashMap::new();
        for (p, t) in d.params.iter().zip(arg_tys) {
            if let Some(te) = &p.ty {
                bind_tparams(te, t, &d.tparams, &mut out);
            }
        }
        if d.tparams.iter().all(|tp| out.contains_key(&tp.name)) {
            return vec![];
        }
        let mut from_ret: HashMap<String, Ty> = d.tparams.iter().map(|tp| tp.name.clone()).zip(owner_targs.iter().cloned()).collect();
        if let (Some(want), Some(ret)) = (self.want_hint.clone(), d.ret.as_ref()) {
            let want = self.resolve(&want);
            bind_tparams(ret, &want, &d.tparams, &mut from_ret);
        }
        let mut extra = vec![];
        for tp in &d.tparams {
            if out.contains_key(&tp.name) {
                continue;
            }
            match from_ret.get(&tp.name) {
                Some(t) if !t.has_var() => extra.push(t.clone()),
                _ => return vec![],
            }
        }
        extra
    }

    fn call_def_args(&mut self, name_span: Span, d: &Def, ext: Option<Vec<Ty>>, args: Vec<TExpr>) -> R<Vec<TExpr>> {
        let name = &d.name;
        let mut targs = vec![];
        for (i, (v, p)) in args.into_iter().zip(&d.params).enumerate() {
            let want = match (&ext, &p.ty) {
                (Some(ts), _) => Some(ts[i].clone()),
                (None, Some(t)) => Some(type_from(t, &self.w.structs, &self.w.consts)?),
                (None, None) => None,
            };
            let v = match &want {
                Some(w) => self.coerce(v, w)?,
                None => v,
            };
            if let Some(want) = want {
                if !self.unify(&v.ty, &want) {
                    return Err(Diag::new(name_span, format!("`{name}` expects {}, got {}", want.show(), self.resolve(&v.ty).show())));
                }
            }
            targs.push(v);
        }
        Ok(targs)
    }

    #[allow(clippy::too_many_arguments)]
    fn call_def_inst(&mut self, def: usize, name: &str, name_span: Span, targs: Vec<TExpr>, extra: Vec<Ty>, sp: Span, d: &Def) -> R<TExpr> {
        let mut arg_tys: Vec<Ty> = targs.iter().map(|a| self.resolve(&a.ty)).collect();
        if arg_tys.iter().any(Ty::has_var) {
            let t = self.unknown(sp, "this call's arguments")?;
            return Ok(self.mk(TK::Unit, t, sp));
        }
        let callee_pure = d.pure || self.w.defs[def].external.as_ref().is_some_and(|e| e.pure);
        if self.pure_decl && !callee_pure {
            return Err(Diag::new(name_span, format!("`#[pure] def {}` calls `{name}`, which isn't pure", self.fn_name)));
        }
        arg_tys.extend(extra);
        let fid = self.w.instance(def, arg_tys, sp)?;
        let (ret, fallible, io) = match &self.w.funcs[fid] {
            Some(f) => (f.ret.clone(), f.fallible, f.io),
            None => match self.w.sigs.get(&fid) {
                // Recursive call to an instance still being checked: assume
                // I/O unless it is declared pure.
                Some(s) => (s.0.clone(), s.1, !s.2),
                None => return Err(Diag::new(sp, format!("`{name}` is recursive: declare its return type (`-> T`)"))),
            },
        };
        if io {
            if self.pure_decl {
                return Err(Diag::new(name_span, format!("`#[pure] def {}` calls `{name}`, which does I/O", self.fn_name)));
            }
            self.impure = true;
        }
        let call = self.mk(TK::Call(fid, targs), ret, sp);
        self.fallible_use(call, fallible, name)
    }

    /// A fallible call must be under `try`; at the top level `try` is implicit.
    fn fallible_use(&mut self, e: TExpr, fallible: bool, name: &str) -> R<TExpr> {
        if !fallible || self.under_try {
            return Ok(e);
        }
        // Without `~`, a fallible call is a `~T` value.
        let _ = name;
        let t = Ty::Result(Box::new(e.ty.clone()));
        Ok(TExpr { ty: t, ..e })
    }

    fn const_call(&mut self, c: &str, csp: Span, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, sp: Span) -> R<TExpr> {
        let resolved = resolve_name(c, csp, &|q| self.w.structs.contains_key(q) || generic(q).is_some())?;
        let c = resolved.as_str();
        if let (Some(GenDef::S(_)), false, true) = (generic(c), self.w.structs.contains_key(c), name != "new") {
            // `Ring.make(3, v)`: a static method of a generic type, generic over its parameters.
            let q = method_name(c, name);
            if let Some(&def) = self.w.by_name.get(&q) {
                if self.w.defs[def].def.params.first().is_none_or(|p| p.name != "self") {
                    // (`named` only marks `c` as a type here; the def infers its parameters.)
                    return self.const_call_named(c, Some(Ty::Unit), csp, name, name_span, args, block, sp);
                }
            }
        }
        if let (Some(g), false) = (generic(c), self.w.structs.contains_key(c)) {
            // `Stack.new(items: [1])`: infer the type arguments from the values
            // (or from the type the value is wanted as).
            let fields: Vec<(String, TypeExpr, Span)> = match (&g, name) {
                (GenDef::S(d), "new") => d.fields.clone(),
                (GenDef::E(d), v) => match d.variants.iter().find(|(n, _, _)| n == v) {
                    Some((_, fs, _)) => fs.clone(),
                    None => return Err(Diag::new(name_span, format!("{c} has no variant `{v}`"))),
                },
                _ => return Err(Diag::new(name_span, format!("no method `{name}` on {c}"))),
            };
            let mut out = HashMap::new();
            if let Some((base, targs)) = self.want_hint.clone().and_then(|t| inst_args(&self.resolve(&t))) {
                if base == c {
                    for (tp, t) in g.tparams().iter().zip(targs) {
                        out.insert(tp.name.clone(), t);
                    }
                }
            }
            for (i, a) in args.iter().enumerate() {
                let (te, v) = match &a.kind {
                    ExprKind::KwArg(n, _, v) => match fields.iter().find(|(f, _, _)| f == n) {
                        Some((_, te, _)) => (te, v.as_ref()),
                        None => continue,
                    },
                    _ => match fields.get(i) {
                        Some((_, te, _)) => (te, a),
                        None => continue,
                    },
                };
                let v = self.value(v)?;
                let t = self.resolve(&v.ty);
                bind_tparams(te, &t, g.tparams(), &mut out);
            }
            let mut targs = vec![];
            for tp in g.tparams() {
                match out.get(&tp.name) {
                    Some(t) if !t.has_var() => targs.push(t.clone()),
                    _ => return Err(Diag::new(sp, format!("can't infer `{}` for {c}; write `{c}[...].{name}` or declare the type", tp.name))),
                }
            }
            let t = instantiate(c, &g, targs, sp, &self.w.structs, &self.w.consts)?;
            return self.const_call_named(c, Some(t), csp, name, name_span, args, block, sp);
        }
        self.const_call_named(c, None, csp, name, name_span, args, block, sp)
    }

    #[allow(clippy::too_many_arguments)]
    fn const_call_named(&mut self, c: &str, named: Option<Ty>, csp: Span, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, sp: Span) -> R<TExpr> {
        let argv = |cx: &mut Self| args.iter().map(|a| cx.value(a)).collect::<R<Vec<_>>>();
        let named = named.or_else(|| self.w.structs.get(c).cloned());
        // `Point.from_json(s)`: a static method (`def self.from_json`), a def of the
        // type without a receiver.
        if named.is_some() {
            let q = method_name(c, name);
            if let Some(&def) = self.w.by_name.get(&q) {
                if self.w.defs[def].def.params.first().is_none_or(|p| p.name != "self") {
                    if block.is_some() {
                        return Err(Diag::new(sp, format!("`{c}.{name}` doesn't take a block")));
                    }
                    let d = self.w.defs[def].def.clone();
                    if args.len() != d.params.len() {
                        return Err(Diag::new(name_span, format!("`{c}.{name}` takes {} argument(s), got {}", d.params.len(), args.len())));
                    }
                    let mut targs = vec![];
                    for (a, p) in args.iter().zip(&d.params) {
                        let v = match &p.ty {
                            Some(t) => {
                                let prev = enter_pkg(&self.w.defs[def].pkg);
                                let r = type_from(t, &self.w.structs, &self.w.consts);
                                leave_pkg(prev);
                                match r {
                                    Ok(t) => self.value_as(a, &t)?,
                                    Err(_) => self.value(a)?,
                                }
                            }
                            None => self.value(a)?,
                        };
                        targs.push(v);
                    }
                    if let Some((base, ts)) = named.as_ref().and_then(inst_args) {
                        if base == c {
                            self.owner_targs = ts;
                        }
                    }
                    return self.call_def(def, &q, name_span, targs, sp);
                }
            }
        }
        if let Some(et @ Ty::Enum(..)) = named.clone() {
            let Ty::Enum(_, vs) = &et else { unreachable!() };
            let Some(k) = vs.iter().position(|(v, _)| v == name) else {
                return Err(Diag::new(name_span, format!("{c} has no variant `{name}`; its variants are {}", vs.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>().join(", "))));
            };
            if block.is_some() {
                return Err(Diag::new(sp, "a variant doesn't take a block"));
            }
            let fields = &vs[k].1;
            let mut given: Vec<Option<TExpr>> = vec![None; fields.len()];
            let kw = args.iter().any(|a| matches!(a.kind, ExprKind::KwArg(..)));
            if !kw && args.len() != fields.len() {
                return Err(Diag::new(sp, format!("`{c}.{name}` takes {} value(s) ({}), got {}", fields.len(), fields.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>().join(", "), args.len())));
            }
            for (i, a) in args.iter().enumerate() {
                let (j, v) = match &a.kind {
                    ExprKind::KwArg(n, nsp, v) => match fields.iter().position(|(f, _)| f == n) {
                        Some(j) => (j, v.as_ref()),
                        None => return Err(Diag::new(*nsp, format!("`{c}.{name}` has no field `{n}`"))),
                    },
                    _ if kw => return Err(Diag::new(a.span, format!("`{c}.{name}`: mix of positional and keyword arguments; use one or the other"))),
                    _ => (i, a),
                };
                let ft = fields[j].1.clone();
                let v = self.value(v)?;
                let v = self.coerce(v, &ft)?;
                self.expect(&v.ty, &ft, v.span, &format!("`{c}.{name}` field `{}`", fields[j].0))?;
                given[j] = Some(v);
            }
            let mut slots = vec![];
            for (vk, (_, fs)) in vs.iter().enumerate() {
                for (j, (f, ft)) in fs.iter().enumerate() {
                    let v = if vk == k { given[j].take() } else { None };
                    slots.push(match v {
                        Some(v) => v,
                        None => self.zero_of(ft, sp).ok_or_else(|| Diag::new(sp, format!("`{c}.{name}` needs `{f}`: {} has no zero value", ft.show())))?,
                    });
                }
            }
            return Ok(self.mk(TK::M(M::VariantNew(k), None, slots, None), et.clone(), sp));
        }
        if let (Some(st @ Ty::Struct(..)), "new") = (named.clone(), name) {
            let Ty::Struct(_, fields) = &st else { unreachable!() };
            if args.is_empty() || args.iter().any(|a| matches!(a.kind, ExprKind::KwArg(..))) {
                // Keywords: any subset of fields, in any order; the rest are zero (Go).
                let mut given: Vec<Option<TExpr>> = vec![None; fields.len()];
                for a in args {
                    let ExprKind::KwArg(n, nsp, v) = &a.kind else {
                        return Err(Diag::new(a.span, format!("`{c}.new`: mix of positional and keyword arguments; use one or the other")));
                    };
                    let Some(k) = fields.iter().position(|(f, _)| f == n) else {
                        return Err(self.no_field(c, fields, n, *nsp));
                    };
                    if given[k].is_some() {
                        return Err(Diag::new(*nsp, format!("field `{n}` given twice")));
                    }
                    let ft = &fields[k].1;
                    let v = self.value(v)?;
                    let v = self.coerce(v, ft)?;
                    if !self.unify(&v.ty, ft) {
                        return Err(Diag::new(v.span, format!("`{c}.{n}` is {}, got {}", ft.show(), self.resolve(&v.ty).show())));
                    }
                    given[k] = Some(v);
                }
                let mut vals = vec![];
                for (k, g) in given.into_iter().enumerate() {
                    vals.push(match g {
                        Some(v) => v,
                        None => self.zero_of(&fields[k].1, sp).ok_or_else(|| Diag::new(sp, format!("`{c}.{}` has no zero value ({}); give it", fields[k].0, fields[k].1.show())))?,
                    });
                }
                return Ok(self.mk(TK::M(M::StructNew, None, vals, None), st, sp));
            }
            if args.len() != fields.len() {
                return Err(Diag::new(sp, format!("`{c}.new` takes {} arguments ({}), got {}", fields.len(), fields.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>().join(", "), args.len())));
            }
            let mut vals = vec![];
            for (a, (f, ft)) in args.iter().zip(fields) {
                let v = self.value(a)?;
                let v = self.coerce(v, ft)?;
                if !self.unify(&v.ty, ft) {
                    return Err(Diag::new(a.span, format!("`{c}.{f}` is {}, got {}", ft.show(), self.resolve(&v.ty).show())));
                }
                vals.push(v);
            }
            return Ok(self.mk(TK::M(M::StructNew, None, vals, None), st, sp));
        }
        match (c, name) {
            ("Math", "sqrt") => {
                let a = argv(self)?;
                if a.len() != 1 {
                    return Err(Diag::new(sp, "`Math.sqrt` takes one argument"));
                }
                let x = self.coerce(a[0].clone(), &Ty::Float)?;
                self.expect(&x.ty, &Ty::Float, x.span, "Math.sqrt")?;
                Ok(self.mk(TK::M(M::Sqrt, None, vec![x], None), Ty::Float, sp))
            }
            ("Int", "sqrt") => {
                let a = argv(self)?;
                if a.len() != 1 {
                    return Err(Diag::new(sp, "`Int.sqrt` takes one argument"));
                }
                self.expect(&a[0].ty, &Ty::Int, a[0].span, "Int.sqrt")?;
                Ok(self.mk(TK::M(M::IntSqrt, None, a, None), Ty::Int, sp))
            }
            ("Str", "from_bytes") => {
                let mut a = argv(self)?;
                if a.len() != 1 {
                    return Err(Diag::new(sp, "`Str.from_bytes` takes one argument, a [Byte]"));
                }
                if matches!(a[0].kind, TK::Array(_)) {
                    let x = a.remove(0);
                    a.insert(0, self.coerce(x, &Ty::arr(Ty::IntK(IntKind::U8)))?);
                }
                match self.resolve(&a[0].ty) {
                    Ty::Array(t) | Ty::Fixed(t, _) if *t == Ty::IntK(IntKind::U8) => {}
                    t => return Err(Diag::new(a[0].span, format!("`Str.from_bytes` takes a [Byte], not {}", t.show()))),
                }
                Ok(self.mk(TK::M(M::FromBytes, None, a, None), Ty::Str, sp))
            }
            ("Array", "new") => {
                let a = argv(self)?;
                if a.len() != 2 {
                    return Err(Diag::new(sp, "`Array.new(size, value)` takes two arguments"));
                }
                self.expect(&a[0].ty, &Ty::Int, a[0].span, "Array.new size")?;
                let el = a[1].ty.clone();
                Ok(self.mk(TK::M(M::ArrayNew, None, a, None), Ty::arr(el), sp))
            }
            ("Ptr", "null") => {
                if !args.is_empty() {
                    return Err(Diag::new(sp, "`Ptr.null` takes no arguments"));
                }
                Ok(self.mk(TK::M(M::PtrNull, None, vec![], None), Ty::Ptr, sp))
            }
            ("Str", "from_cstr") | ("Str", "from_ptr") => {
                let a = argv(self)?;
                let (m, want) = if name == "from_cstr" { (M::StrFromCstr, 1) } else { (M::StrFromPtr, 2) };
                if a.len() != want {
                    let sig = if want == 1 { "`Str.from_cstr(p)` takes a Ptr" } else { "`Str.from_ptr(p, n)` takes a Ptr and a byte count" };
                    return Err(Diag::new(sp, sig));
                }
                self.expect(&a[0].ty, &Ty::Ptr, a[0].span, &format!("`Str.{name}` pointer"))?;
                if want == 2 {
                    self.expect(&a[1].ty, &Ty::Int, a[1].span, "`Str.from_ptr` length")?;
                }
                self.impure = true;
                Ok(self.mk(TK::M(m, None, a, None), Ty::Str, sp))
            }
            ("C", "errno") => {
                if !args.is_empty() {
                    return Err(Diag::new(sp, "`C.errno` takes no arguments"));
                }
                self.impure = true;
                Ok(self.mk(TK::M(M::CErrno, None, vec![], None), Ty::Int, sp))
            }
            ("C", "strerror") => {
                let a = argv(self)?;
                if a.len() != 1 {
                    return Err(Diag::new(sp, "`C.strerror` takes one argument, the errno value"));
                }
                self.expect(&a[0].ty, &Ty::Int, a[0].span, "`C.strerror` argument")?;
                self.impure = true;
                Ok(self.mk(TK::M(M::CStrerror, None, a, None), Ty::Str, sp))
            }
            ("Time", "now_ns") => {
                if !args.is_empty() {
                    return Err(Diag::new(sp, "`Time.now_ns` takes no arguments"));
                }
                self.impure = true;
                Ok(self.mk(TK::M(M::NowNs, None, vec![], None), Ty::Int, sp))
            }
            ("Test", "exit") => {
                let a = argv(self)?;
                if a.len() != 1 {
                    return Err(Diag::new(sp, "`Test.exit` takes one argument, the status"));
                }
                self.expect(&a[0].ty, &Ty::Int, a[0].span, "Test.exit status")?;
                self.impure = true;
                Ok(self.mk(TK::M(M::Exit, None, a, None), Ty::Unit, sp))
            }
            ("Test", "begin_capture") | ("Test", "end_capture") => {
                self.impure = true;
                let (m, ty) = if name == "begin_capture" { (M::CapBegin, Ty::Unit) } else { (M::CapEnd, Ty::Str) };
                Ok(self.mk(TK::M(m, None, vec![], None), ty, sp))
            }
            ("File", "read") => {
                if self.pure_decl {
                    return Err(Diag::new(csp, format!("`#[pure] def {}` can't do I/O: `File.read`", self.fn_name)));
                }
                self.impure = true;
                let a = argv(self)?;
                if a.len() != 1 {
                    return Err(Diag::new(sp, "`File.read` takes one argument"));
                }
                self.expect(&a[0].ty, &Ty::Str, a[0].span, "File.read path")?;
                let e = self.mk(TK::M(M::FileRead, None, a, None), Ty::Str, sp);
                self.fallible_use(e, true, "File.read")
            }
            ("Enumerator", "new") => {
                let Some(b) = block else { return Err(Diag::new(sp, "`Enumerator.new` needs a block `{ |y| ... }`")) };
                let el = self.site(b.id, 0);
                let own_start = self.locals.len();
                self.scopes.push(HashMap::new());
                let y = match b.params.as_slice() {
                    [(n, _)] => self.declare(n, Ty::Yielder(Box::new(el.clone()))),
                    _ => return Err(Diag::new(b.span, "`Enumerator.new` blocks take one parameter, the yielder: `{ |y| ... }`")),
                };
                self.loops.push(LoopKind::Gen);
                let saved = std::mem::replace(&mut self.impure, false);
                let saved_cells = self.cells.take();
                let r = self.body_as(&b.body, false);
                self.cells = saved_cells;
                let (body, _) = r?;
                let pure = !self.impure;
                self.impure = saved || self.impure;
                self.loops.pop();
                self.pop_scope();
                let blk = TBlock { params: vec![y], destructure: false, body, pure, span: b.span, own: (own_start, self.locals.len()) };
                Ok(self.mk(TK::M(M::EnumNew, None, vec![], Some(Box::new(blk))), Ty::Gen(Box::new(el)), sp))
            }
            _ => {
                let _ = name_span;
                Err(Diag::new(name_span, format!("no method `{name}` on `{c}`")))
            }
        }
    }

    /// Check a block whose parameters bind `elem` (destructured if it is a
    /// tuple and the block names several params). Returns the block and the
    /// type of its value.
    fn block(&mut self, b: &Block, elem: &Ty) -> R<(TBlock, Ty)> {
        self.block_n(b, std::slice::from_ref(elem), true)
    }

    fn block_n(&mut self, b: &Block, params: &[Ty], allow_destructure: bool) -> R<(TBlock, Ty)> {
        let used = !std::mem::take(&mut self.block_unused);
        let own_start = self.locals.len();
        self.scopes.push(HashMap::new());
        let mut ids = vec![];
        let mut destructure = false;
        let elem = self.resolve(&params[0]);
        if b.params.is_empty() {
            if params.len() == 1 {
                ids.push(self.declare("it", elem.clone()));
            } else {
                for (i, t) in params.iter().enumerate() {
                    ids.push(self.declare(&format!("_{}", i + 1), t.clone()));
                }
            }
        } else if b.params.len() == params.len() {
            for ((n, _), t) in b.params.iter().zip(params) {
                ids.push(self.declare(n, t.clone()));
            }
        } else if allow_destructure && params.len() == 1 && b.params.len() > 1 {
            match &elem {
                Ty::Tuple(ts) if ts.len() == b.params.len() => {
                    for ((n, _), t) in b.params.iter().zip(ts.clone()) {
                        ids.push(self.declare(n, t));
                    }
                    destructure = true;
                }
                Ty::Var(_) => {
                    for (n, _) in &b.params {
                        let v = self.unknown(b.span, "the block's element")?;
                        ids.push(self.declare(n, v));
                    }
                    destructure = true;
                }
                t => {
                    self.pop_scope();
                    return Err(Diag::new(b.span, format!("block takes {} parameters but each element is {}", b.params.len(), t.show())));
                }
            }
        } else {
            self.pop_scope();
            return Err(Diag::new(b.span, format!("block takes {} parameter(s), expected {}", b.params.len(), params.len())));
        }
        self.loops.push(LoopKind::Block);
        let saved = std::mem::replace(&mut self.impure, false);
        let r = self.body_as(&b.body, used);
        let pure = !self.impure;
        self.impure = saved || self.impure;
        self.loops.pop();
        self.pop_scope();
        let (mut body, ty) = r?;
        let own = (own_start, self.locals.len());
        // The block's value: materialize unless it feeds flat_map (caller decides).
        if let Some(TStmt::Expr(e)) = body.last_mut() {
            let ty = e.ty.clone();
            return Ok((TBlock { params: ids, destructure, body, pure, span: b.span, own }, ty));
        }
        Ok((TBlock { params: ids, destructure, body, pure, span: b.span, own }, if matches!(ty, Ty::Never) { Ty::Never } else { Ty::Unit }))
    }

    /// `&:name` as a block: `{ |x| x.name }`.
    fn sym_block(&mut self, sym: &(String, Span), elem: &Ty) -> R<(TBlock, Ty)> {
        self.scopes.push(HashMap::new());
        let p = self.declare("_sym", elem.clone());
        let recv = self.mk(TK::Local(p), elem.clone(), sym.1);
        let body = self.method(recv, &sym.0, sym.1, &[], None, None, sym.1);
        self.pop_scope();
        let body = body?;
        let ty = body.ty.clone();
        Ok((TBlock { params: vec![p], destructure: false, body: vec![TStmt::Expr(body)], pure: true, span: sym.1 , own: (0, 0) }, ty))
    }

    /// Block given as `{ ... }` or `&:sym`.
    fn any_block(&mut self, block: Option<&Block>, bsym: Option<&(String, Span)>, elem: &Ty, sp: Span, name: &str) -> R<(TBlock, Ty)> {
        match (block, bsym) {
            (Some(b), _) => self.block(b, elem),
            (None, Some(s)) => self.sym_block(s, elem),
            _ => Err(Diag::new(sp, format!("`{name}` needs a block"))),
        }
    }

    fn elem_of(&mut self, t: &Ty, sp: Span) -> R<Option<(Ty, bool)>> {
        Ok(match self.resolve(t) {
            Ty::Range => Some((Ty::Int, false)),
            Ty::Array(t) | Ty::Fixed(t, _) => Some((*t, false)),
            Ty::Map(k, v) => Some((Ty::Tuple(vec![*k, *v]), false)),
            Ty::Chan(t) => Some((*t, false)),
            Ty::Pool(t) => Some((Ty::Tuple(vec![Ty::Handle(t.show()), *t]), false)),
            Ty::Seq(t, l) => Some((*t, l)),
            Ty::Gen(t) => Some((*t, false)),
            Ty::Var(_) => {
                let _ = sp;
                None
            }
            _ => None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    /// A method call; when the receiver's type has no such method, a
    /// function of an imported standard package whose first parameter takes
    /// the receiver (S2's method sugar: `s.has_prefix(p)` for
    /// `strings.has_prefix(s, p)`).
    fn method(&mut self, recv: TExpr, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, bsym: Option<&(String, Span)>, sp: Span) -> R<TExpr> {
        match self.method_builtin(recv.clone(), name, name_span, args, block, bsym, sp) {
            Err(d) if d.msg.starts_with(&format!("no method `{name}`")) && bsym.is_none() => {
                let rt = self.resolve(&recv.ty);
                let Some(def) = self.std_sugar(&rt, name)? else { return Err(d) };
                let mut targs = vec![recv];
                for a in args {
                    targs.push(self.value(a)?);
                }
                // `xs.index_func { |x| ... }`: the block is the last argument.
                if let Some(b) = block {
                    if self.w.defs[def].def.params.len() != targs.len() + 1 {
                        return Err(d);
                    }
                    let f = self.block_as_lambda(def, &targs, b)?;
                    targs.push(f);
                }
                self.call_def(def, name, name_span, targs, sp)
            }
            r => r,
        }
    }

    /// The imported standard-package function `name` whose first parameter
    /// takes a `t`.
    fn std_sugar(&self, t: &Ty, name: &str) -> R<Option<usize>> {
        let pkg = current_pkg();
        let mut imports: Vec<(String, String)> = PKGS.with(|p| p.borrow().0.get(&pkg).map(|m| m.iter().map(|(a, p)| (a.clone(), p.clone())).collect()).unwrap_or_default());
        imports.sort();
        for (alias, path) in imports.iter().filter(|(_, p)| crate::front::is_std(p)) {
            let Some(&def) = self.w.by_name.get(&format!("{path}.{name}")) else { continue };
            let d = &self.w.defs[def].def;
            if !d.public {
                continue;
            }
            let fits = match d.params.first().and_then(|p| p.ty.as_ref()) {
                None => false,
                Some(te) if !d.tparams.is_empty() => sugar_shape_fits(te, t),
                Some(te) => type_from(te, &self.w.structs, &self.w.consts).is_ok_and(|pt| pt == *t),
            };
            if fits {
                USED.with(|u| u.borrow_mut().insert((pkg.clone(), alias.clone())));
                return Ok(Some(def));
            }
        }
        Ok(None)
    }

    fn method_builtin(&mut self, recv: TExpr, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, bsym: Option<&(String, Span)>, sp: Span) -> R<TExpr> {
        let rt = self.resolve(&recv.ty);
        if let Ty::Var(_) = rt {
            if self.strict {
                return Err(Diag::new(recv.span, format!("cannot infer the type of the receiver of `{name}`; add a type annotation")));
            }
            self.unresolved = true;
            let t = self.fresh();
            return Ok(self.mk(TK::Unit, t, sp));
        }
        if let Some(def) = self.refined(&rt, name) {
            if block.is_some() {
                return Err(Diag::new(sp, format!("`{name}` doesn't take a block")));
            }
            let mut targs = vec![recv];
            for a in args {
                targs.push(self.value(a)?);
            }
            return self.call_def(def, name, name_span, targs, sp);
        }
        let argv = |cx: &mut Self| args.iter().map(|a| cx.value(a)).collect::<R<Vec<_>>>();
        let mk_m = |cx: &mut Self, m: M, r: TExpr, a: Vec<TExpr>, b: Option<TBlock>, ty: Ty| cx.mk(TK::M(m, Some(Box::new(r)), a, b.map(Box::new)), ty, sp);
        if let Ty::Map(kt, vt) = &rt {
            let (kt, vt) = ((**kt).clone(), (**vt).clone());
            let key_arg = |cx: &mut Self, a: &[Expr]| -> R<TExpr> {
                let k = cx.value(&a[0])?;
                let k = cx.coerce(k, &kt)?;
                cx.expect(&k.ty, &kt, k.span, "map key")?;
                Ok(k)
            };
            match (name, args.len()) {
                ("size" | "length", 0) => return Ok(mk_m(self, M::MapSize, recv, vec![], None, Ty::Int)),
                ("empty?", 0) => {
                    let n = mk_m(self, M::MapSize, recv, vec![], None, Ty::Int);
                    let z = self.mk(TK::Int(0), Ty::Int, sp);
                    return Ok(self.mk(TK::Bin(BinOp::Eq, Box::new(n), Box::new(z)), Ty::Bool, sp));
                }
                ("key?" | "has_key?" | "include?", 1) => {
                    let k = key_arg(self, args)?;
                    return Ok(mk_m(self, M::MapHas, recv, vec![k], None, Ty::Bool));
                }
                ("delete", 1) => {
                    if let TK::Local(id) = recv.kind {
                        if self.pure_decl && id < self.param_count() {
                            return Err(Diag::new(sp, format!("`#[pure] def {}` can't mutate its argument `{}`", self.fn_name, self.locals[id].name)));
                        }
                        self.locals[id].mutated = true;
                    }
                    let k = key_arg(self, args)?;
                    return Ok(mk_m(self, M::MapDel, recv, vec![k], None, Ty::Opt(Box::new(vt))));
                }
                ("fetch", 2) => {
                    let k = key_arg(self, args)?;
                    let d = self.value(&args[1])?;
                    let d = self.coerce(d, &vt)?;
                    self.expect(&d.ty, &vt, d.span, "default")?;
                    return Ok(mk_m(self, M::MapGetOr, recv, vec![k, d], None, vt));
                }
                ("keys", 0) => return Ok(mk_m(self, M::MapKeys, recv, vec![], None, Ty::arr(kt))),
                ("values", 0) => return Ok(mk_m(self, M::MapValues, recv, vec![], None, Ty::arr(vt))),
                ("dup" | "clone", 0) => {
                    // A new map with the same entries (MapNew with a receiver copies it).
                    return Ok(mk_m(self, M::MapNew, recv, vec![], None, rt.clone()));
                }
                _ => {}
            }
        }
        // Fixed arrays: their length is constant; everything else reads them as a slice.
        if let Ty::Fixed(el, n) = &rt {
            match name {
                "size" | "length" => return Ok(self.mk(TK::Int(*n as i64), Ty::Int, sp)),
                "<<" | "push" | "pop" | "concat" => return Err(Diag::new(name_span, format!("`{name}` would change the length of a {}; use a slice `[T]`", rt.show()))),
                _ => {
                    let view = TExpr { ty: Ty::Array(el.clone()), ..recv };
                    return self.method(view, name, name_span, args, block, bsym, sp);
                }
            }
        }
        // Tuples
        if let Ty::Tuple(ts) = &rt {
            let k = match name {
                "first" => Some(0),
                "last" => Some(ts.len() - 1),
                _ => None,
            };
            if let Some(k) = k {
                return Ok(mk_m(self, M::TupleGet(k), recv, vec![], None, ts[k].clone()));
            }
        }
        // Yielder: y << v
        if let Ty::Yielder(el) = &rt {
            if name == "<<" {
                let a = argv(self)?;
                self.expect(&a[0].ty, el, a[0].span, "yielded value")?;
                return Ok(mk_m(self, M::Yield, recv, a, None, Ty::Unit));
            }
        }
        // Ptr: `p.null?`
        if rt == Ty::Ptr {
            return match (name, args.len()) {
                ("null?", 0) => {
                    let z = self.mk(TK::M(M::PtrNull, None, vec![], None), Ty::Ptr, sp);
                    Ok(self.mk(TK::Bin(BinOp::Eq, Box::new(recv), Box::new(z)), Ty::Bool, sp))
                }
                _ => Err(Diag::new(name_span, format!("no method `{name}` on Ptr; it has `null?` and `==`"))),
            };
        }
        // Optionals
        if let Ty::Opt(inner) = &rt {
            let inner = (**inner).clone();
            return match name {
                "present?" => Ok(mk_m(self, M::OptPresent, recv, vec![], None, Ty::Bool)),
                "some?" => Ok(mk_m(self, M::OptPresent, recv, vec![], None, Ty::Bool)),
                "none?" => {
                    let p = mk_m(self, M::OptPresent, recv, vec![], None, Ty::Bool);
                    Ok(self.mk(TK::Not(Box::new(p)), Ty::Bool, sp))
                }
                "unwrap" => Ok(mk_m(self, M::Unwrap, recv, vec![], None, inner)),
                "unwrap_or" => {
                    let a = argv(self)?;
                    if a.len() != 1 {
                        return Err(Diag::new(sp, "`unwrap_or(default)` takes one argument"));
                    }
                    self.binary(BinOp::Or, recv, a.into_iter().next().unwrap(), sp)
                }
                _ => Err(Diag::new(name_span, format!("`{name}` on a {}: it may be absent", rt.show())).note("unwrap it first (`if x = ...`, `x || default`, `x.unwrap`), or chain with `?.`")),
            };
        }
        // Struct fields
        if let Ty::Struct(sn, fs) = &rt {
            if let Some((k, ft)) = rt.field(name) {
                if !args.is_empty() || block.is_some() {
                    return Err(Diag::new(sp, format!("`{sn}.{name}` is a field, not a method")));
                }
                return Ok(mk_m(self, M::TupleGet(k), recv, vec![], None, ft));
            }
            let base = rt.type_name().unwrap_or(sn);
            if let Some(&def) = self.w.by_name.get(&method_name(base, name)).or(self.w.default_for(base, name).as_ref()) {
                if name.ends_with('!') {
                    return Err(Diag::new(recv.span, format!("`{name}` changes its receiver; call it on a variable")));
                }
                let mut targs = vec![recv];
                for a in args {
                    targs.push(self.value(a)?);
                }
                // A block for a last parameter of function type.
                if let Some(b) = block {
                    let d = &self.w.defs[def].def;
                    if d.params.len() != targs.len() + 1 || !matches!(d.params.last().and_then(|p| p.ty.as_ref()), Some(TypeExpr::Fn(..))) {
                        return Err(Diag::new(sp, format!("`{name}` doesn't take a block")));
                    }
                    let f = self.block_as_lambda(def, &targs, b)?;
                    targs.push(f);
                }
                return self.call_def(def, name, name_span, targs, sp);
            }
            return Err(self.no_field(sn, fs, name, name_span));
        }
        if rt == Ty::Error {
            return match (name, args.len()) {
                ("message" | "to_s", 0) => Ok(mk_m(self, M::ErrMessage, recv, vec![], None, Ty::Str)),
                ("wrap", 1) => {
                    let c = self.value(&args[0])?;
                    self.expect(&c.ty, &Ty::Str, c.span, "`wrap` context")?;
                    Ok(mk_m(self, M::ErrWrap, recv, vec![c], None, Ty::Error))
                }
                _ => Err(Diag::new(name_span, format!("no method `{name}` on Error; it has message, wrap"))),
            };
        }
        if let Ty::Result(t) = &rt {
            let t = (**t).clone();
            return match (name, args.len()) {
                ("ok", 0) => Ok(mk_m(self, M::ResOk, recv, vec![], None, Ty::Opt(Box::new(t)))),
                ("err", 0) => Ok(mk_m(self, M::ResErr, recv, vec![], None, Ty::Opt(Box::new(Ty::Error)))),
                ("ok?", 0) => Ok(mk_m(self, M::ResIsOk, recv, vec![], None, Ty::Bool)),
                ("err?", 0) => {
                    let ok = mk_m(self, M::ResIsOk, recv, vec![], None, Ty::Bool);
                    Ok(self.mk(TK::Not(Box::new(ok)), Ty::Bool, sp))
                }
                ("unwrap", 0) => Ok(mk_m(self, M::ResUnwrap, recv, vec![], None, t)),
                ("unwrap_or", 1) => {
                    let d = self.value(&args[0])?;
                    let d = self.coerce(d, &t)?;
                    self.expect(&d.ty, &t, d.span, "`unwrap_or` default")?;
                    Ok(mk_m(self, M::ResUnwrapOr, recv, vec![d], None, t))
                }
                ("rescue", 0) => {
                    let (blk, bt) = self.any_block(block, bsym, &Ty::Error, sp, name)?;
                    self.expect(&bt, &t, blk.span, "`rescue` value")?;
                    Ok(mk_m(self, M::ResRescue, recv, vec![], Some(blk), t))
                }
                _ => Err(Diag::new(name_span, format!("no method `{name}` on {}; handle it with `~`, ok, err, unwrap, unwrap_or, rescue", rt.show()))),
            };
        }
        if let Ty::Chan(t) = &rt {
            let t = (**t).clone();
            self.impure = true;
            match (name, args.len()) {
                ("send" | "<<", 1) => {
                    let v = self.value(&args[0])?;
                    let v = self.coerce(v, &t)?;
                    self.expect(&v.ty, &t, v.span, "sent value")?;
                    return Ok(mk_m(self, M::ChanSend, recv, vec![v], None, Ty::Unit));
                }
                ("recv", 0) => return Ok(mk_m(self, M::ChanRecv, recv, vec![], None, Ty::Opt(Box::new(t)))),
                ("close", 0) => return Ok(mk_m(self, M::ChanClose, recv, vec![], None, Ty::Unit)),
                ("size" | "length", 0) => return Ok(mk_m(self, M::ChanLen, recv, vec![], None, Ty::Int)),
                _ if self.elem_of(&rt, sp)?.is_some() && SEQ_METHODS.contains(&name) => {}
                _ => return Err(Diag::new(name_span, format!("no method `{name}` on {}; it has send (<<), recv, close, size, and the Enumerable methods", rt.show()))),
            }
        }
        if let Ty::Mutex(t) = &rt {
            let t = (**t).clone();
            self.impure = true;
            return match (name, args.len()) {
                ("lock", 0) => {
                    let Some(b) = block else {
                        return Err(Diag::new(sp, "`lock` needs a block: `m.lock { |v| ... }`"));
                    };
                    let saved = self.lock_floor.replace(self.loops.len());
                    let r = self.block(b, &t);
                    self.lock_floor = saved;
                    let (blk, bt) = r?;
                    Ok(mk_m(self, M::Lock, recv, vec![], Some(blk), bt))
                }
                _ => Err(Diag::new(name_span, format!("no method `{name}` on {}; reach the value with `lock {{ |v| ... }}`", rt.show()))),
            };
        }
        if let Ty::Atomic(t) = &rt {
            let t = (**t).clone();
            self.impure = true;
            let val = |ck: &mut Self, a: &Expr| -> R<TExpr> {
                let v = ck.value_as(a, &t)?;
                ck.expect(&v.ty, &t, v.span, "atomic value")?;
                Ok(v)
            };
            return match (name, args.len()) {
                ("load", 0) => Ok(mk_m(self, M::AtomicLoad, recv, vec![], None, t)),
                ("store", 1) => {
                    let v = val(self, &args[0])?;
                    Ok(mk_m(self, M::AtomicStore, recv, vec![v], None, Ty::Unit))
                }
                ("add", 1) if t == Ty::Int => {
                    let v = val(self, &args[0])?;
                    Ok(mk_m(self, M::AtomicAdd, recv, vec![v], None, Ty::Int))
                }
                ("swap", 1) => {
                    let v = val(self, &args[0])?;
                    Ok(mk_m(self, M::AtomicSwap, recv, vec![v], None, t))
                }
                ("compare_and_swap", 2) => {
                    let o = val(self, &args[0])?;
                    let n = val(self, &args[1])?;
                    Ok(mk_m(self, M::AtomicCas, recv, vec![o, n], None, Ty::Bool))
                }
                _ => Err(Diag::new(name_span, format!("no method `{name}` on {}; it has load, store, {}swap and compare_and_swap", rt.show(), if t == Ty::Int { "add, " } else { "" }))),
            };
        }
        if let Ty::Pool(t) = &rt {
            let t = (**t).clone();
            let ht = Ty::Handle(t.show());
            match (name, args.len()) {
                ("add" | "<<", 1) => {
                    let v = self.value_as(&args[0], &t)?;
                    self.expect(&v.ty, &t, v.span, "pool value")?;
                    self.impure = true;
                    return Ok(mk_m(self, M::PoolAdd, recv, vec![v], None, ht));
                }
                ("remove", 1) => {
                    let h = self.value(&args[0])?;
                    self.expect(&h.ty, &ht, h.span, "pool handle")?;
                    self.impure = true;
                    return Ok(mk_m(self, M::PoolRemove, recv, vec![h], None, Ty::Opt(Box::new(t))));
                }
                ("size" | "length", 0) => return Ok(mk_m(self, M::PoolSize, recv, vec![], None, Ty::Int)),
                ("get", 1) => {
                    let h = self.value(&args[0])?;
                    self.expect(&h.ty, &ht, h.span, "pool handle")?;
                    return Ok(mk_m(self, M::PoolLookup, recv, vec![h], None, Ty::Opt(Box::new(t))));
                }
                _ if SEQ_METHODS.contains(&name) => {}
                _ => return Err(Diag::new(name_span, format!("no method `{name}` on {}; it has add, remove, size, get, [h], and the Enumerable methods over (handle, value)", rt.show()))),
            }
        }
        if let (Ty::Task(t), "wait", 0) = (&rt, name, args.len()) {
            let inner = self.resolve(t);
            let ok = match inner {
                Ty::Result(x) => *x,
                x => x,
            };
            self.impure = true;
            return Ok(mk_m(self, M::TaskWait, recv, vec![], None, Ty::Result(Box::new(ok))));
        }
        if let (Ty::Fn(..), "call") = (&rt, name) {
            return self.fn_call(recv, args, sp);
        }
        if let Ty::Iface(iname) = &rt {
            let ms = self.w.ifaces[iname].clone();
            let Some(k) = ms.iter().position(|m| m.name == name) else {
                return Err(Diag::new(name_span, format!("no method `{name}` on {iname}; it has {}", ms.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "))));
            };
            let m = &ms[k];
            if args.len() != m.params.len() {
                return Err(Diag::new(name_span, format!("`{iname}.{name}` takes {} argument(s), got {}", m.params.len(), args.len())));
            }
            let mut targs = vec![];
            for (a, pt) in args.iter().zip(&m.params) {
                let v = self.value(a)?;
                let v = self.coerce(v, pt)?;
                self.expect(&v.ty, pt, v.span, &format!("`{iname}.{name}` argument"))?;
                targs.push(v);
            }
            self.impure = true;
            return Ok(mk_m(self, M::IfaceCall(k), recv, targs, None, m.ret.clone()));
        }
        if let Ty::Enum(en, vs) = &rt {
            let base = rt.type_name().unwrap_or(en);
            if let Some(&def) = self.w.by_name.get(&method_name(base, name)).or(self.w.default_for(base, name).as_ref()) {
                if name.ends_with('!') {
                    return Err(Diag::new(recv.span, format!("`{name}` changes its receiver; call it on a variable")));
                }
                let mut targs = vec![recv];
                for a in args {
                    targs.push(self.value(a)?);
                }
                return self.call_def(def, name, name_span, targs, sp);
            }
            if name == "to_i" && vs.iter().all(|(_, fs)| fs.is_empty()) {
                // Payload-free enums are Go's iota constants.
                return Ok(mk_m(self, M::EnumTag, recv, vec![], None, Ty::Int));
            }
            return Err(Diag::new(name_span, format!("no method `{name}` on {en}")));
        }
        // math block: libm calls (`x.__sin`, `x.__pow(y)`) and bit casts, for std/math.
        if rt == Ty::Float || rt == Ty::IntK(IntKind::U64) {
            if let Some(r) = self.math_intrinsic(&rt, &recv, name, name_span, args, sp) {
                return r;
            }
        }
        // Float methods
        if rt == Ty::Float {
            match name {
                "to_f" => return Ok(recv),
                "to_i" | "to_int" | "truncate" => return Ok(mk_m(self, M::FloatToI, recv, vec![], None, Ty::Int)),
                "abs" => return Ok(mk_m(self, M::FloatAbs, recv, vec![], None, Ty::Float)),
                "sqrt" => return Ok(self.mk(TK::M(M::Sqrt, None, vec![recv], None), Ty::Float, sp)),
                "to_s" => return Ok(mk_m(self, M::FloatToS, recv, vec![], None, Ty::Str)),
                _ => {}
            }
        }
        // Conversions between numeric types: `to_u8` checks, `as_u8` wraps (Go's `uint8(x)`).
        if rt.int_kind().is_some() || rt == Ty::Float {
            let conv = name.strip_prefix("to_").map(|n| (n, false)).or_else(|| name.strip_prefix("as_").map(|n| (n, true)));
            if let Some((n, wrap)) = conv {
                let k = match n {
                    "i" => Some(IntKind::I64),
                    "byte" => Some(IntKind::U8),
                    "rune" => Some(IntKind::I32),
                    n => [IntKind::I8, IntKind::I16, IntKind::I32, IntKind::I64, IntKind::U8, IntKind::U16, IntKind::U32, IntKind::U64].into_iter().find(|k| k.method() == n),
                };
                if let Some(k) = k {
                    if wrap && rt == Ty::Float {
                        return Err(Diag::new(name_span, format!("`as_{n}` wraps integers; for a Float use `to_{n}`")));
                    }
                    let recv = self.coerce(recv, &if rt == Ty::Float { Ty::Float } else { Ty::Int })?;
                    if n == "i" && rt == Ty::Float {
                        return Ok(mk_m(self, M::FloatToI, recv, vec![], None, Ty::Int));
                    }
                    return Ok(mk_m(self, M::Conv(k, wrap), recv, vec![], None, Ty::of_kind(k)));
                }
            }
        }
        // Methods on every integer type.
        if let Some(k) = rt.int_kind() {
            match name {
                "to_f" => {
                    let recv = self.coerce(recv, &Ty::Int)?;
                    return Ok(mk_m(self, M::ToF, recv, vec![], None, Ty::Float));
                }
                "to_s" => {
                    let recv = self.coerce(recv, &Ty::Int)?;
                    return Ok(mk_m(self, M::ToS, recv, vec![], None, Ty::Str));
                }
                "even?" if k != IntKind::I64 => return Ok(mk_m(self, M::Even, recv, vec![], None, Ty::Bool)),
                "odd?" if k != IntKind::I64 => return Ok(mk_m(self, M::Odd, recv, vec![], None, Ty::Bool)),
                "<<" => {
                    let a = argv(self)?;
                    if a.len() != 1 {
                        return Err(Diag::new(sp, "`<<` takes one operand"));
                    }
                    return self.binary(BinOp::Shl, recv, a.into_iter().next().unwrap(), sp);
                }
                _ => {}
            }
        }
        // Int methods
        if rt == Ty::Int {
            match name {
                "even?" => return Ok(mk_m(self, M::Even, recv, vec![], None, Ty::Bool)),
                "odd?" => return Ok(mk_m(self, M::Odd, recv, vec![], None, Ty::Bool)),
                "digits" => return Ok(mk_m(self, M::Digits, recv, vec![], None, Ty::arr(Ty::Int))),
                "step" => {
                    let a = argv(self)?;
                    if a.len() != 2 {
                        return Err(Diag::new(sp, "`step(limit, by)` takes two arguments"));
                    }
                    for x in &a {
                        self.expect(&x.ty, &Ty::Int, x.span, "step")?;
                    }
                    let Some(b) = block else { return Err(Diag::new(sp, "`step` needs a block")) };
                    self.block_unused = true;
                    let (blk, _) = self.block(b, &Ty::Int)?;
                    return Ok(mk_m(self, M::Step, recv, a, Some(blk), Ty::Unit));
                }
                _ => {}
            }
        }
        // Str methods
        if rt == Ty::Str {
            match name {
                "to_s" => return Ok(recv),
                "to_i" => return Ok(mk_m(self, M::ToI, recv, vec![], None, Ty::Int)),
                "size" | "length" => return Ok(mk_m(self, M::Size, recv, vec![], None, Ty::Int)),
                "reverse" => return Ok(mk_m(self, M::Reverse, recv, vec![], None, Ty::Str)),
                // Ruby's conveniences (Go: strings.TrimSpace, HasPrefix, ...).
                "strip" | "lstrip" | "rstrip" | "lines" => {
                    if !args.is_empty() {
                        return Err(Diag::new(sp, format!("`{name}` takes no arguments")));
                    }
                    let (code, ty) = match name {
                        "strip" => (0, Ty::Str),
                        "lstrip" => (1, Ty::Str),
                        "rstrip" => (2, Ty::Str),
                        _ => (6, Ty::arr(Ty::Str)),
                    };
                    return Ok(mk_m(self, M::StrHelper(code), recv, vec![], None, ty));
                }
                "start_with?" | "end_with?" | "include?" => {
                    let a = argv(self)?;
                    if a.len() != 1 {
                        return Err(Diag::new(sp, format!("`{name}` takes one string")));
                    }
                    self.expect(&a[0].ty, &Ty::Str, a[0].span, name)?;
                    let code = match name {
                        "start_with?" => 3,
                        "end_with?" => 4,
                        _ => 5,
                    };
                    return Ok(mk_m(self, M::StrHelper(code), recv, a, None, Ty::Bool));
                }
                "byteindex" => {
                    // Ruby's String#byteindex(sub, offset = 0): a byte offset or -1
                    // (nil in Ruby). std/regexp needed a fast substring search.
                    let a = argv(self)?;
                    if a.is_empty() || a.len() > 2 {
                        return Err(Diag::new(sp, "`byteindex(sub, from = 0)` takes one or two arguments"));
                    }
                    self.expect(&a[0].ty, &Ty::Str, a[0].span, name)?;
                    if a.len() == 2 {
                        self.expect(&a[1].ty, &Ty::Int, a[1].span, name)?;
                    }
                    return Ok(mk_m(self, M::ByteIndex, recv, a, None, Ty::Int));
                }
                "chars" => return Ok(mk_m(self, M::Chars, recv, vec![], None, Ty::seq(Ty::Str, false))),
                "bytes" => return Ok(mk_m(self, M::Bytes, recv, vec![], None, Ty::seq(Ty::IntK(IntKind::U8), false))),
                "runes" => return Ok(mk_m(self, M::Runes, recv, vec![], None, Ty::seq(Ty::IntK(IntKind::I32), false))),
                "delete" | "split" => {
                    let a = argv(self)?;
                    if a.len() != 1 {
                        return Err(Diag::new(sp, format!("`{name}` takes one argument")));
                    }
                    self.expect(&a[0].ty, &Ty::Str, a[0].span, name)?;
                    return Ok(if name == "delete" {
                        mk_m(self, M::Delete, recv, a, None, Ty::Str)
                    } else {
                        mk_m(self, M::Split, recv, a, None, Ty::arr(Ty::Str))
                    });
                }
                _ => {}
            }
        }
        // A function value's `dup`: its captures copied (so it can go to a task).
        if matches!(rt, Ty::Fn(..)) && matches!(name, "dup" | "clone") && args.is_empty() {
            return Ok(mk_m(self, M::Dup, recv, vec![], None, rt.clone()));
        }
        // Arrays: push, size, indexing helpers.
        if let Ty::Array(el) = &rt {
            let el = (**el).clone();
            match name {
                "dup" | "clone" => return Ok(mk_m(self, M::Dup, recv, vec![], None, rt.clone())),
                // A new slice, last element first (Ruby's; slices.reverse
                // reverses in place, as Go's does).
                "reverse" => return Ok(mk_m(self, M::Reverse, recv, vec![], None, rt.clone())),
                // In place (Ruby's reverse!; slices.reverse(xs) is Go's).
                "reverse!" => {
                    let (id, s1) = self.opt_tmp(recv, sp);
                    let at = self.mk(TK::Local(id), rt.clone(), sp);
                    let rev = mk_m(self, M::Reverse, at.clone(), vec![], None, rt.clone());
                    let copy = self.mk(TK::M(M::CopyInto, None, vec![at, rev], None), Ty::Int, sp);
                    let unit = self.mk(TK::Unit, Ty::Unit, sp);
                    return Ok(self.mk(TK::Seq(vec![s1, TStmt::Expr(copy), TStmt::Expr(unit)]), Ty::Unit, sp));
                }
                "<<" | "push" => {
                    let mut a = argv(self)?;
                    if a.len() != 1 {
                        return Err(Diag::new(sp, format!("`{name}` takes one value")));
                    }
                    let x = a.pop().unwrap();
                    a.push(self.coerce(x, &el)?);
                    self.expect(&a[0].ty, &el, a[0].span, "pushed value")?;
                    if let TK::Local(id) = recv.kind {
                        if self.pure_decl && id < self.param_count() {
                            return Err(Diag::new(sp, format!("`#[pure] def {}` can't mutate its argument `{}`", self.fn_name, self.locals[id].name)));
                        }
                        self.locals[id].mutated = true;
                        self.locals[id].pushed = true;
                    } else {
                        return Err(Diag::new(recv.span, "can only push onto a local array"));
                    }
                    return Ok(mk_m(self, M::Push, recv, a, None, Ty::Unit));
                }
                "size" | "length" => return Ok(mk_m(self, M::Size, recv, vec![], None, Ty::Int)),
                "last" => return Ok(mk_m(self, M::Last, recv, vec![], None, el)),
                "each_index" => return Ok(mk_m(self, M::EachIndex, recv, vec![], None, Ty::seq(Ty::Int, false))),
                "each_cons" => {
                    let a = argv(self)?;
                    if a.len() != 1 {
                        return Err(Diag::new(sp, "`each_cons(n)` takes one argument"));
                    }
                    self.expect(&a[0].ty, &Ty::Int, a[0].span, "each_cons")?;
                    return Ok(mk_m(self, M::EachCons, recv, a, None, Ty::seq(Ty::arr(el), false)));
                }
                "pmap" => {
                    let (blk, out) = self.any_block(block, bsym, &el, sp, name)?;
                    if !blk.pure {
                        return Err(Diag::new(blk.span, "`pmap` blocks must be pure: no I/O"));
                    }
                    return Ok(mk_m(self, M::Pmap, recv, vec![], Some(blk), Ty::arr(out)));
                }
                _ => {}
            }
        }
        // Pipelines over Range / Array / Seq / Enumerator.
        if let Some((el, lazy)) = self.elem_of(&rt, sp)? {
            let seq = |t: Ty| Ty::seq(t, lazy);
            match name {
                "find" | "detect" => {
                    let (blk, bt) = self.any_block(block, bsym, &el, sp, name)?;
                    self.expect(&bt, &Ty::Bool, blk.span, &format!("`{name}` block"))?;
                    let sel = mk_m(self, M::Select, recv, vec![], Some(blk), seq(el.clone()));
                    return Ok(mk_m(self, M::Find, sel, vec![], None, Ty::Opt(Box::new(el))));
                }
                "select" | "filter" | "reject" | "take_while" => {
                    let (blk, bt) = self.any_block(block, bsym, &el, sp, name)?;
                    self.expect(&bt, &Ty::Bool, blk.span, &format!("`{name}` block"))?;
                    let m = match name {
                        "reject" => M::Reject,
                        "take_while" => M::TakeWhile,
                        _ => M::Select,
                    };
                    return Ok(mk_m(self, m, recv, vec![], Some(blk), seq(el)));
                }
                "map" => {
                    let (mut blk, bt) = self.any_block(block, bsym, &el, sp, name)?;
                    let bt = self.materialize_block_tail(&mut blk, bt);
                    return Ok(mk_m(self, M::Map, recv, vec![], Some(blk), seq(bt)));
                }
                "flat_map" => {
                    let (blk, bt) = self.any_block(block, bsym, &el, sp, name)?;
                    let inner = match self.elem_of(&bt, sp)? {
                        Some((t, _)) => t,
                        None => return Err(Diag::new(blk.span, format!("`flat_map` block must return a collection, got {}", self.resolve(&bt).show()))),
                    };
                    return Ok(mk_m(self, M::FlatMap, recv, vec![], Some(blk), seq(inner)));
                }
                "step" => {
                    let a = argv(self)?;
                    if a.len() != 1 || block.is_some() {
                        return Err(Diag::new(sp, "`step(n)` takes one argument (every n-th element; use it in a chain or `for`)"));
                    }
                    self.expect(&a[0].ty, &Ty::Int, a[0].span, name)?;
                    return Ok(mk_m(self, M::StepBy, recv, a, None, seq(el)));
                }
                "drop" | "take" => {
                    let a = argv(self)?;
                    if a.len() != 1 {
                        return Err(Diag::new(sp, format!("`{name}(n)` takes one argument")));
                    }
                    self.expect(&a[0].ty, &Ty::Int, a[0].span, name)?;
                    return Ok(mk_m(self, if name == "drop" { M::Drop } else { M::Take }, recv, a, None, seq(el)));
                }
                "each_with_index" => {
                    if block.is_some() {
                        return Err(Diag::new(sp, "`each_with_index` with a block: use `.each_with_index.each { |x, i| ... }`"));
                    }
                    return Ok(mk_m(self, M::EachWithIndex, recv, vec![], None, seq(Ty::Tuple(vec![el, Ty::Int]))));
                }
                "lazy" => return Ok(mk_m(self, M::Lazy, recv, vec![], None, Ty::seq(el, true))),
                // `xs.join(sep)`: the elements as `%v` with `sep` between (Go's strings.Join).
                "join" if matches!(rt, Ty::Array(_)) && args.len() <= 1 && block.is_none() => {
                    if !printable(&el) {
                        return Err(Diag::new(sp, format!("`join` shows each element, and {} can't be shown", el.show())));
                    }
                    let sep = match args.first() {
                        Some(a) => {
                            let v = self.value(a)?;
                            self.expect(&v.ty, &Ty::Str, v.span, "`join` separator")?;
                            v
                        }
                        None => self.mk(TK::Str(String::new()), Ty::Str, sp),
                    };
                    return Ok(mk_m(self, M::Join, recv, vec![sep], None, Ty::Str));
                }
                "sum" => {
                    let (blk, t) = match (block, bsym) {
                        (None, None) => (None, el.clone()),
                        _ => {
                            let (b, t) = self.any_block(block, bsym, &el, sp, name)?;
                            (Some(b), t)
                        }
                    };
                    let t = self.resolve(&t);
                    let st = if t == Ty::Float { Ty::Float } else { Ty::Int };
                    self.expect(&t, &st, sp, "`sum` elements")?;
                    return Ok(mk_m(self, M::Sum, recv, vec![], blk, st));
                }
                "max" | "min" => {
                    if !matches!(self.resolve(&el), Ty::Int | Ty::IntK(_) | Ty::Float | Ty::Str) {
                        return Err(Diag::new(sp, format!("`{name}` needs Int, Float or Str elements, got {}", self.resolve(&el).show())));
                    }
                    return Ok(mk_m(self, if name == "max" { M::Max } else { M::Min }, recv, vec![], None, el));
                }
                "max_by" | "min_by" => {
                    let (blk, kt) = self.any_block(block, bsym, &el, sp, name)?;
                    let k = self.resolve(&kt);
                    if !(k.int_kind().is_some() || matches!(k, Ty::Float | Ty::Str)) {
                        self.expect(&kt, &Ty::Int, blk.span, &format!("`{name}` key"))?;
                    }
                    return Ok(mk_m(self, if name == "max_by" { M::MaxBy } else { M::MinBy }, recv, vec![], Some(blk), el));
                }
                "first" => {
                    if !args.is_empty() {
                        return Err(Diag::new(sp, "`first(n)` isn't supported yet; use `take(n)`"));
                    }
                    return Ok(mk_m(self, M::First, recv, vec![], None, el));
                }
                "to_a" => return Ok(mk_m(self, M::ToA, recv, vec![], None, Ty::arr(el))),
                "each" => {
                    self.block_unused = block.is_some();
                    let (blk, _) = self.any_block(block, bsym, &el, sp, name)?;
                    return Ok(mk_m(self, M::Each, recv, vec![], Some(blk), Ty::Unit));
                }
                "reduce" | "inject" => {
                    let a = args;
                    let (mut init, op) = match (a, block, bsym) {
                        ([Expr { kind: ExprKind::Sym(s), span, .. }], None, None) => (None, Some((s.clone(), *span))),
                        ([init, Expr { kind: ExprKind::Sym(s), span, .. }], None, None) => (Some(self.value(init)?), Some((s.clone(), *span))),
                        ([], Some(_), None) => (None, None),
                        ([init], Some(_), None) => (Some(self.value(init)?), None),
                        _ => return Err(Diag::new(sp, "`reduce` takes a symbol (`reduce(:*)`) or a block")),
                    };
                    // With a block and a start value, the accumulator is the
                    // start value's type (Ruby's inject: `bytes.reduce(0) { |n, b|
                    // n * 10 + b.to_i }` sums into an Int). A numeric constant
                    // over Floats is a Float. The symbol form folds elements.
                    let mut acc_ty = el.clone();
                    if let Some(i) = init.take() {
                        let i = if op.is_none() && !(matches!(i.kind, TK::Const(_) | TK::Int(_)) && self.resolve(&el) == Ty::Float) {
                            acc_ty = self.resolve(&i.ty);
                            i
                        } else {
                            let c = self.coerce(i, &el)?;
                            self.expect(&c.ty, &el, c.span, "reduce initial value")?;
                            c
                        };
                        init = Some(i);
                    }
                    let blk = match op {
                        Some((s, ssp)) => {
                            let bop = match s.as_str() {
                                "+" => BinOp::Add,
                                "*" => BinOp::Mul,
                                "-" => BinOp::Sub,
                                _ => return Err(Diag::new(ssp, format!("`reduce(:{s})` isn't supported; use a block"))),
                            };
                            self.scopes.push(HashMap::new());
                            let a = self.declare("_acc", el.clone());
                            let x = self.declare("_x", el.clone());
                            self.pop_scope();
                            let la = self.mk(TK::Local(a), el.clone(), sp);
                            let lx = self.mk(TK::Local(x), el.clone(), sp);
                            let body = self.binary(bop, la, lx, sp)?;
                            TBlock { params: vec![a, x], destructure: false, body: vec![TStmt::Expr(body)], pure: true, span: ssp , own: (0, 0) }
                        }
                        None => {
                            let (b, t) = self.block_n(block.unwrap(), &[acc_ty.clone(), el.clone()], false)?;
                            self.expect(&t, &acc_ty, b.span, "reduce block")?;
                            b
                        }
                    };
                    return Ok(mk_m(self, M::Reduce, recv, init.into_iter().collect(), Some(blk), acc_ty));
                }
                "all?" | "any?" => {
                    let (blk, bt) = self.any_block(block, bsym, &el, sp, name)?;
                    self.expect(&bt, &Ty::Bool, blk.span, &format!("`{name}` block"))?;
                    return Ok(mk_m(self, if name == "all?" { M::All } else { M::Any }, recv, vec![], Some(blk), Ty::Bool));
                }
                "count" | "size" | "length" if block.is_none() && bsym.is_none() && args.is_empty() => {
                    return Ok(mk_m(self, M::Count, recv, vec![], None, Ty::Int));
                }
                "count" if args.is_empty() => {
                    let (blk, bt) = self.any_block(block, bsym, &el, sp, name)?;
                    self.expect(&bt, &Ty::Bool, blk.span, "`count` block")?;
                    let sel = mk_m(self, M::Select, recv, vec![], Some(blk), seq(el.clone()));
                    return Ok(mk_m(self, M::Count, sel, vec![], None, Ty::Int));
                }
                "include?" => {
                    let mut a = argv(self)?;
                    if a.len() == 1 {
                        let x = a.remove(0);
                        a.push(self.coerce(x, &el)?);
                    }
                    self.expect(&a[0].ty, &el, a[0].span, "include?")?;
                    return Ok(mk_m(self, M::Include, recv, a, None, Ty::Bool));
                }
                "sort" => {
                    if !matches!(self.resolve(&el), Ty::Int | Ty::Str) {
                        return Err(Diag::new(sp, "`sort` needs Int or Str elements"));
                    }
                    return Ok(mk_m(self, M::Sort, recv, vec![], None, Ty::arr(el)));
                }
                "each_index" | "each_cons" | "pmap" | "last" => {
                    // Array-only: materialize, then retry.
                    let arr = self.materialize(recv);
                    if matches!(self.resolve(&arr.ty), Ty::Array(_)) {
                        return self.method(arr, name, name_span, args, block, bsym, sp);
                    }
                    return Err(Diag::new(name_span, format!("`{name}` needs an Array")));
                }
                _ => {}
            }
        }
        Err(self.no_method(&rt, name, name_span))
    }

    /// `format("...", args)`: Go's fmt verbs. The format string must be a
    /// literal, so every directive is checked against its argument here.
    fn format(&mut self, name: &str, args: &[Expr], sp: Span) -> R<TExpr> {
        let Some(Expr { kind: ExprKind::Str(fmt), span: fsp, .. }) = args.first() else {
            return Err(Diag::new(sp, format!("`{name}` takes a format string literal first")));
        };
        let vals = args[1..].iter().map(|a| self.value(a)).collect::<R<Vec<_>>>()?;
        let mut pieces = vec![];
        let mut lit = String::new();
        let mut k = 0;
        let mut it = fmt.chars().peekable();
        while let Some(c) = it.next() {
            if c != '%' {
                lit.push(c);
                continue;
            }
            let mut spec = String::new();
            while let Some(&d) = it.peek() {
                spec.push(d);
                it.next();
                if d.is_ascii_alphabetic() || d == '%' {
                    break;
                }
            }
            if spec == "%" {
                lit.push('%');
                continue;
            }
            if !lit.is_empty() {
                pieces.push(FmtPiece::Lit(std::mem::take(&mut lit)));
            }
            let Some(v) = vals.get(k) else {
                return Err(Diag::new(*fsp, format!("`%{spec}` has no argument: {} given", vals.len())));
            };
            let t = self.resolve(&v.ty);
            // Flags, width, precision, verb: `%-+ 08.3f`.
            let (mut left, mut zero, mut plus, mut space) = (false, false, false, false);
            let mut rest = spec.as_str();
            while let Some(c) = rest.chars().next().filter(|c| "-+0 #".contains(*c)) {
                match c {
                    '-' => left = true,
                    '+' => plus = true,
                    '0' => zero = true,
                    ' ' => space = true,
                    _ => {}
                }
                rest = &rest[1..];
            }
            let wlen = rest.chars().take_while(char::is_ascii_digit).count();
            let width: u32 = rest[..wlen].parse().unwrap_or(0).min(1000);
            rest = &rest[wlen..];
            let mut prec: Option<u32> = None;
            if let Some(r) = rest.strip_prefix('.') {
                let n = r.chars().take_while(char::is_ascii_digit).count();
                prec = Some(r[..n].parse().unwrap_or(0).min(40));
                rest = &r[n..];
            }
            let verb = rest;
            let bad = |what: &str| Diag::new(v.span, format!("`%{spec}` needs {what}, got {}", t.show()));
            let numeric = t.int_kind().is_some() || t == Ty::Float;
            let piece = match verb {
                "d" | "i" => {
                    if t.int_kind().is_none() {
                        return Err(bad("an integer"));
                    }
                    FmtPiece::Int(k)
                }
                "x" | "X" | "o" | "b" => {
                    if t.int_kind().is_none() {
                        return Err(bad("an integer"));
                    }
                    let base = match verb {
                        "o" => 8,
                        "b" => 2,
                        _ => 16,
                    };
                    FmtPiece::Base(k, base, verb == "X")
                }
                "c" => {
                    if t.int_kind().is_none() {
                        return Err(bad("a Rune"));
                    }
                    FmtPiece::Char(k)
                }
                "s" => {
                    if t != Ty::Str {
                        return Err(bad("a Str (use `%v` for any value)"));
                    }
                    FmtPiece::Str(k)
                }
                "q" => {
                    if t != Ty::Str {
                        return Err(bad("a Str"));
                    }
                    FmtPiece::Quote(k)
                }
                "v" => {
                    if !printable(&t) {
                        return Err(bad("a value that can be shown"));
                    }
                    FmtPiece::Str(k)
                }
                "T" => FmtPiece::Lit(t.show()),
                "t" => {
                    if t != Ty::Bool {
                        return Err(bad("a Bool"));
                    }
                    FmtPiece::Str(k)
                }
                "f" | "F" => {
                    if !matches!(t, Ty::Int | Ty::Float) {
                        return Err(bad("a number"));
                    }
                    FmtPiece::Fixed(k, prec.unwrap_or(6))
                }
                "e" | "E" => {
                    if !matches!(t, Ty::Int | Ty::Float) {
                        return Err(bad("a number"));
                    }
                    FmtPiece::Exp(k, prec.unwrap_or(6), verb == "E")
                }
                _ => return Err(Diag::new(*fsp, format!("unsupported directive `%{spec}`")).note("supported so far (Go's fmt verbs): %v, %d, %s, %q, %t, %f, %e, %E, %x, %X, %o, %b, %c, %T, %%, with flags `-+0 `, a width and a precision")),
            };
            // Integer precision: minimum digits; Go ignores the 0 flag then.
            let int_verb = matches!(verb, "d" | "i" | "x" | "X" | "o" | "b");
            let piece = match prec {
                Some(n) if int_verb => {
                    zero = false;
                    FmtPiece::Digits { inner: Box::new(piece), n }
                }
                _ => piece,
            };
            let piece = if width > 0 || plus || space {
                FmtPiece::Padded { inner: Box::new(piece), width, left, zero: zero && !left && numeric, plus: plus && numeric, space: space && numeric && !plus }
            } else {
                piece
            };
            // `%T` shows no argument.
            if verb == "T" {
                pieces.push(piece);
                k += 1;
                continue;
            }
            pieces.push(piece);
            k += 1;
        }
        if !lit.is_empty() {
            pieces.push(FmtPiece::Lit(lit));
        }
        if k != vals.len() {
            return Err(Diag::new(sp, format!("`{name}`: {} directive(s) but {} argument(s)", k, vals.len())));
        }
        let vals = vals
            .into_iter()
            .enumerate()
            .map(|(i, v)| if pieces.iter().any(|p| p.wants_float() && p.arg() == Some(i)) { self.coerce(v, &Ty::Float) } else { Ok(v) })
            .collect::<R<Vec<_>>>()?;
        Ok(self.mk(TK::Format(pieces, vals), Ty::Str, sp))
    }

    /// `fmt.sprintf` and friends (std/fmt): builtins over `format`.
    fn fmt_call(&mut self, name: &str, args: &[Expr], sp: Span) -> R<TExpr> {
        let text = match name {
            "sprintf" | "printf" | "errorf" => self.format(name, args, sp)?,
            _ => {
                // sprint / sprintln / print / println: every operand as `%v`.
                let line = name.ends_with("ln");
                let vals = args.iter().map(|a| self.value(a)).collect::<R<Vec<_>>>()?;
                let mut pieces = vec![];
                for (i, v) in vals.iter().enumerate() {
                    let t = self.resolve(&v.ty);
                    if !printable(&t) {
                        return Err(Diag::new(v.span, format!("`fmt.{name}` can't show a {}", t.show())));
                    }
                    if i > 0 {
                        let prev = self.resolve(&vals[i - 1].ty);
                        if line || (prev != Ty::Str && t != Ty::Str) {
                            pieces.push(FmtPiece::Lit(" ".into()));
                        }
                    }
                    pieces.push(FmtPiece::Str(i));
                }
                if line {
                    pieces.push(FmtPiece::Lit("\n".into()));
                }
                self.mk(TK::Format(pieces, vals), Ty::Str, sp)
            }
        };
        match name {
            "printf" | "print" | "println" => {
                if self.pure_decl {
                    return Err(Diag::new(sp, format!("`#[pure] def {}` can't do I/O: `fmt.{name}`", self.fn_name)));
                }
                self.impure = true;
                Ok(self.mk(TK::M(M::PrintStr, None, vec![text], None), Ty::Unit, sp))
            }
            "errorf" => self.to_error(text),
            _ => Ok(text),
        }
    }

    fn materialize_block_tail(&mut self, blk: &mut TBlock, t: Ty) -> Ty {
        if let Some(TStmt::Expr(e)) = blk.body.last_mut() {
            let m = self.materialize(e.clone());
            let ty = m.ty.clone();
            *e = m;
            return ty;
        }
        t
    }

    fn no_method(&self, rt: &Ty, name: &str, sp: Span) -> Diag {
        let mut cands: Vec<&str> = vec![];
        match rt {
            Ty::Int | Ty::IntK(_) => cands.extend(INT_METHODS),
            Ty::Float => cands.extend(FLOAT_METHODS),
            Ty::Str => cands.extend(STR_METHODS),
            Ty::Array(_) => {
                cands.extend(SEQ_METHODS);
                cands.extend(ARRAY_EXTRA);
            }
            Ty::Range | Ty::Seq(..) | Ty::Gen(_) => cands.extend(SEQ_METHODS),
            Ty::Tuple(_) => cands.extend(["first", "last"]),
            _ => {}
        }
        let mut msg = format!("no method `{name}` on {}", rt.show());
        if cands.contains(&name) {
            msg = format!("`{name}` exists on {}, but not with these arguments or this block", rt.show());
        } else if let Some(best) = cands.iter().filter(|c| **c != name && lev(c, name) <= 2).min_by_key(|c| lev(c, name)) {
            msg.push_str(&format!("; did you mean `{best}`?"));
        }
        Diag::new(sp, msg)
    }
}

fn occurs(v: u32, t: &Ty) -> bool {
    match t {
        Ty::Var(x) => *x == v,
        Ty::Array(t) | Ty::Fixed(t, _) | Ty::Seq(t, _) | Ty::Gen(t) | Ty::Yielder(t) | Ty::Opt(t) => occurs(v, t),
        Ty::Tuple(ts) => ts.iter().any(|t| occurs(v, t)),
        Ty::Map(k, x) => occurs(v, k) || occurs(v, x),
        Ty::Fn(ps, r) => ps.iter().any(|t| occurs(v, t)) || occurs(v, r),
        Ty::Result(t) | Ty::Task(t) | Ty::Chan(t) | Ty::Pool(t) | Ty::Mutex(t) | Ty::Atomic(t) => occurs(v, t),
        _ => false,
    }
}

fn sp_node(sp: &Span) -> NodeId {
    sp.lo
}

/// Can `try` apply to this: a fallible call, File.read, or arithmetic.
fn is_fallible_expr(e: &TExpr, cx: &FnCx) -> bool {
    match &e.kind {
        TK::Call(f, _) => cx.w.funcs[*f].as_ref().map_or_else(|| cx.w.sigs.get(f).is_some_and(|s| s.1), |f| f.fallible),
        TK::M(M::FileRead, ..) => true,
        _ => false,
    }
}

/// Whether evaluating `e` calls a function (and so may change state).
fn has_call(e: &TExpr) -> bool {
    if matches!(e.kind, TK::Call(..) | TK::Seq(..)) {
        return true;
    }
    let mut found = false;
    crate::prove::each_child(e, &mut |x| found = found || has_call(x));
    found
}

/// An integer type, or one not known yet.
fn int_like(t: &Ty) -> bool {
    t.int_kind().is_some() || matches!(t, Ty::Var(_))
}

/// The builtin errors that operations under a `~` can fail with instead of
/// panicking: arithmetic (ArithError) and the panicking builtins of
/// `prove::fault`. A nested `~` and blocks that become functions of
/// their own aren't covered by this one.
fn try_faults(e: &TExpr, out: &mut std::collections::BTreeSet<String>, selfl: Option<LocalId>) {
    match &e.kind {
        // Reading `self` in a `!` method: its one-element slice, always there.
        TK::Index(a, i) if matches!((&a.kind, &i.kind), (TK::Local(l), TK::Int(0)) if Some(*l) == selfl) => return,
        TK::Try(_) => return,
        _ if crate::prove::own_function_block(e) => {
            if let TK::M(_, r, args, _) = &e.kind {
                r.iter().map(|r| &**r).chain(args).for_each(|x| try_faults(x, out, selfl));
            }
            return;
        }
        TK::Bin(op, ..) | TK::PlaceAssign(_, _, Some(op), _) if op.is_arith() && int_like(&e.ty) => {
            out.insert("ArithError".into());
        }
        TK::Neg(_) if int_like(&e.ty) => {
            out.insert("ArithError".into());
        }
        // A negative count panics.
        TK::Bin(BinOp::Shl | BinOp::Shr, _, c) if c.ty.int_kind().is_none_or(|k| k.signed()) && !matches!(c.kind, TK::Int(n) if n >= 0) => {
            out.insert("ArithError".into());
        }
        _ => {}
    }
    if let Some(f) = crate::prove::fault(e) {
        out.insert(f.err.to_string());
    }
    crate::prove::each_child(e, &mut |x| try_faults(x, out, selfl));
}

// ---- math block: intrinsics behind std/math ----
impl<'w, 'a> FnCx<'w, 'a> {
    /// `x.__sin`, `x.__pow(y)`, `x.__fma(y, z)` (libm on Floats), `x.__bits`
    /// (Float -> U64) and `u.__from_bits` (U64 -> Float). None: not one of these.
    fn math_intrinsic(&mut self, rt: &Ty, recv: &TExpr, name: &str, name_span: Span, args: &[Expr], sp: Span) -> Option<R<TExpr>> {
        let n = name.strip_prefix("__")?;
        let (m, ty, arity) = match (rt, n) {
            (Ty::Float, "bits") => (M::FloatBits, Ty::IntK(IntKind::U64), 1),
            (Ty::IntK(IntKind::U64), "from_bits") => (M::FloatFromBits, Ty::Float, 1),
            (Ty::IntK(IntKind::U64), "mulhi") => (M::UMulHi, Ty::IntK(IntKind::U64), 2),
            (Ty::Float, _) => {
                let f = crate::lir::MathFn::by_name(n)?;
                (M::Math(f), Ty::Float, f.arity())
            }
            _ => return None,
        };
        Some((|| {
            if args.len() + 1 != arity {
                return Err(Diag::new(name_span, format!("`{name}` takes {} argument(s), got {}", arity - 1, args.len())));
            }
            let mut targs = vec![];
            for a in args {
                let want = if m == M::UMulHi { Ty::IntK(IntKind::U64) } else { Ty::Float };
                let v = self.value(a)?;
                let v = self.coerce(v, &want)?;
                self.expect(&v.ty, &want, v.span, name)?;
                targs.push(v);
            }
            Ok(self.mk(TK::M(m, Some(Box::new(recv.clone())), targs, None), ty, sp))
        })())
    }
}

/// A declared type with type parameters replaced by their bindings.
fn subst_type(te: &TypeExpr, env: &HashMap<String, Ty>, structs: &Structs, consts: &Consts) -> Option<Ty> {
    let go = |t: &TypeExpr| subst_type(t, env, structs, consts);
    Some(match te {
        TypeExpr::Named(n, _) if env.contains_key(n) => env[n].clone(),
        TypeExpr::Array(e, _) => Ty::arr(go(e)?),
        TypeExpr::Opt(e, _) => Ty::Opt(Box::new(go(e)?)),
        TypeExpr::Tuple(es, _) => Ty::Tuple(es.iter().map(go).collect::<Option<Vec<_>>>()?),
        TypeExpr::Fn(ps, r, _) => Ty::Fn(ps.iter().map(go).collect::<Option<Vec<_>>>()?, Box::new(go(r)?)),
        TypeExpr::App(n, args, _) if n == "Map" && args.len() == 2 => Ty::Map(Box::new(go(&args[0])?), Box::new(go(&args[1])?)),
        _ => type_from(te, structs, consts).ok()?,
    })
}

/// Whether a `break` in these statements leaves the loop they're the body
/// of (not counting breaks of loops and blocks nested inside).
fn breaks_out(ss: &[TStmt]) -> bool {
    ss.iter().any(|s| match s {
        TStmt::Break(..) => true,
        // A nested loop's breaks are its own (its condition can't break).
        TStmt::While(..) => false,
        TStmt::If(c, a, b) => expr_breaks_out(c) || breaks_out(a) || breaks_out(b),
        _ => {
            let mut hit = false;
            crate::prove::stmt_exprs(s, &mut |e| hit |= expr_breaks_out(e));
            hit
        }
    })
}

fn expr_breaks_out(e: &TExpr) -> bool {
    match &e.kind {
        TK::Seq(ss) => breaks_out(ss),
        TK::Select(arms, d) => {
            arms.iter().any(|a| match a {
                TSelArm::Recv { body, .. } | TSelArm::Send { body, .. } => breaks_out(body),
            }) || d.as_ref().is_some_and(|d| breaks_out(d))
        }
        // A block's `break` leaves the block's own call.
        TK::M(_, recv, args, Some(_)) => recv.as_ref().is_some_and(|r| expr_breaks_out(r)) || args.iter().any(expr_breaks_out),
        _ => {
            let mut hit = false;
            crate::prove::each_child(e, &mut |c| hit |= expr_breaks_out(c));
            hit
        }
    }
}

/// A closure value holds its captures, so a closure that captures a value
/// holding the closure's own type would be infinitely large (and can't be
/// laid out): `s.f = -> () { s.n }` where `s: S` has a field of that type.
pub fn self_containing_closures(funcs: &[TFunc], ifaces: &HashMap<String, Vec<(Ty, Vec<FuncId>)>>) -> Result<(), Diag> {
    let sites: Vec<(Ty, Vec<Ty>)> = funcs.iter().flat_map(|f| f.lambdas.iter().map(move |(_, t, caps)| (t.clone(), caps.iter().map(|c| f.locals[*c].ty.clone()).collect()))).collect();
    fn holds(t: &Ty, want: &Ty, sites: &[(Ty, Vec<Ty>)], ifaces: &HashMap<String, Vec<(Ty, Vec<FuncId>)>>, seen: &mut Vec<Ty>) -> bool {
        if t == want {
            return true;
        }
        let go = |x: &Ty, seen: &mut Vec<Ty>| holds(x, want, sites, ifaces, seen);
        match t {
            Ty::Struct(_, fs) => fs.iter().any(|(_, f)| go(f, seen)),
            Ty::Enum(_, vs) => vs.iter().any(|(_, fs)| fs.iter().any(|(_, f)| go(f, seen))),
            Ty::Tuple(ts) => ts.iter().any(|x| go(x, seen)),
            Ty::Opt(x) | Ty::Array(x) | Ty::Fixed(x, _) | Ty::Result(x) | Ty::Mutex(x) | Ty::Chan(x) => go(x, seen),
            Ty::Map(k, v) => go(k, seen) || go(v, seen),
            Ty::Fn(..) | Ty::Iface(_) => {
                if seen.contains(t) {
                    return false;
                }
                seen.push(t.clone());
                match t {
                    Ty::Iface(n) => ifaces.get(n).is_some_and(|v| v.iter().any(|(x, _)| go(x, seen))),
                    _ => sites.iter().filter(|(ft, _)| ft == t).any(|(_, caps)| caps.iter().any(|c| go(c, seen))),
                }
            }
            _ => false,
        }
    }
    for f in funcs {
        for (lo, t, caps) in &f.lambdas {
            for c in caps {
                let ct = &f.locals[*c].ty;
                if holds(ct, t, &sites, ifaces, &mut vec![]) {
                    let sp = Span { file: f.span.file, lo: *lo, hi: *lo + 1 };
                    return Err(Diag::new(sp, format!("this closure captures `{}` ({}), which holds a closure of this same type ({}): a closure contains what it captures, so it would contain itself", f.locals[*c].name, ct.show(), t.show()))
                        .note("pass the value as a parameter instead, or keep it in a Pool and capture its @handle"));
                }
            }
        }
    }
    Ok(())
}

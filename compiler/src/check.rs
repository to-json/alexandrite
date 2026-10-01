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
}

pub struct DefInfo {
    pub def: Def,
    pub overflow: Overflow,
    pub external: Option<ExternSig>,
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
}

#[derive(Clone, Debug)]
pub enum CVal {
    Num(ConstVal),
    Str(String),
    Bool(bool),
}

pub type Structs = HashMap<String, Ty>;

const PASSES: usize = 4;

impl<'a> World<'a> {
    pub fn new(sm: &'a SourceMap, defs: Vec<DefInfo>) -> R<Self> {
        let mut by_name = HashMap::new();
        for (i, d) in defs.iter().enumerate() {
            if by_name.insert(d.def.name.clone(), i).is_some() {
                return Err(Diag::new(d.def.name_span, format!("`{}` is defined twice", d.def.name)));
            }
        }
        Ok(World { sm, defs, by_name, instances: HashMap::new(), funcs: vec![], sigs: HashMap::new(), fatal: None, structs: HashMap::new(), consts: HashMap::new() })
    }

    /// Evaluate top-level constants, in order (each may use earlier ones).
    pub fn add_consts(&mut self, defs: &[ConstDef]) -> R<()> {
        for d in defs {
            if self.consts.contains_key(&d.name) || self.structs.contains_key(&d.name) {
                return Err(Diag::new(d.span, format!("`{}` is already defined", d.name)));
            }
            let v = self.eval_const(&d.value)?;
            let ty = match &d.ty {
                Some(te) => {
                    let t = type_from(te, &self.structs)?;
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
            self.consts.insert(d.name.clone(), (v, ty));
        }
        Ok(())
    }

    fn eval_const(&self, e: &Expr) -> R<CVal> {
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
            ExprKind::Const(c) => match self.consts.get(c) {
                Some((v, _)) => v.clone(),
                None => return Err(Diag::new(e.span, format!("`{c}` isn't a constant defined above"))),
            },
            ExprKind::Neg(x) => CVal::Num(consts::neg(&num(x, self.eval_const(x)?)?)),
            ExprKind::BitNot(x) => match num(x, self.eval_const(x)?)? {
                ConstVal::Int(i) => CVal::Num(ConstVal::Int(-i - 1)),
                _ => return Err(Diag::new(e.span, "`^` needs an integer constant")),
            },
            ExprKind::Binary(op, a, b) => {
                let (x, y) = (num(a, self.eval_const(a)?)?, num(b, self.eval_const(b)?)?);
                match consts::fold(*op, &x, &y) {
                    Ok(Some(v)) => CVal::Num(v),
                    Ok(None) => return Err(Diag::new(e.span, format!("`{}` isn't a constant operation", op.text()))),
                    Err(m) => return Err(Diag::new(e.span, m)),
                }
            }
            ExprKind::Call { recv: Some(r), name, args, block: None, .. } if name == "<<" && args.len() == 1 => {
                let (x, y) = (num(r, self.eval_const(r)?)?, num(&args[0], self.eval_const(&args[0])?)?);
                CVal::Num(consts::fold(BinOp::Shl, &x, &y).map_err(|m| Diag::new(e.span, m))?.unwrap())
            }
            _ => return Err(Diag::new(e.span, "a constant must be computable at compile time: literals, other constants and operators")),
        })
    }

    /// Declare structs (in any order; a struct can't contain itself).
    pub fn add_structs(&mut self, defs: &[StructDef]) -> R<()> {
        let by_name: HashMap<&str, &StructDef> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
        for d in defs {
            if self.structs.contains_key(&d.name) || ["Int", "Float", "Bool", "Str", "Array", "Math", "File", "Enumerator"].contains(&d.name.as_str()) {
                return Err(Diag::new(d.span, format!("`{}` is already defined", d.name)));
            }
        }
        fn resolve(name: &str, by_name: &HashMap<&str, &StructDef>, done: &mut Structs, visiting: &mut Vec<String>) -> R<Ty> {
            if let Some(t) = done.get(name) {
                return Ok(t.clone());
            }
            let d = by_name[name];
            if visiting.iter().any(|v| v == name) {
                return Err(Diag::new(d.span, format!("struct `{name}` contains itself; a struct is a value, so it can't")));
            }
            visiting.push(name.to_string());
            let mut fields = vec![];
            for (f, te, _) in &d.fields {
                let t = match te {
                    TypeExpr::Named(n, _) if by_name.contains_key(n.as_str()) => resolve(n, by_name, done, visiting)?,
                    _ => type_from(te, done)?,
                };
                fields.push((f.clone(), t));
            }
            visiting.pop();
            let t = Ty::Struct(name.to_string(), fields);
            done.insert(name.to_string(), t.clone());
            Ok(t)
        }
        for d in defs {
            resolve(&d.name, &by_name, &mut self.structs, &mut vec![])?;
        }
        Ok(())
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
            let mut tys = vec![];
            for p in &d.params {
                match &p.ty {
                    Some(t) => tys.push(type_from(t, &self.structs)?),
                    None => {
                        return Err(Diag::new(p.span, format!("cannot export `{}`: parameter `{}` needs a type (headers need explicit types)", d.name, p.name)));
                    }
                }
            }
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
                cname: ext.symbol.clone(),
                src_name: info.def.name.clone(),
                params: (0..ext.params.len()).collect(),
                locals: ext.params.iter().enumerate().map(|(i, t)| Local { name: format!("p{i}"), ty: t.clone(), reassigned: 0, mutated: false, pushed: false }).collect(),
                ret: ext.ret.clone(),
                fallible: ext.fallible,
                pure: ext.pure,
                io: !ext.pure,
                body: vec![],
                overflow: info.overflow,
                span: info.def.span,
                external: true,
                is_main: false,
            };
            self.funcs[id] = Some(f);
            return Ok(id);
        }
        let def_ast = info.def.clone();
        let overflow = info.overflow;
        if let Some(ret) = &def_ast.ret {
            self.sigs.insert(id, (type_from(ret, &self.structs)?, def_ast.fallible, def_ast.pure));
        }
        let _ = call_span;
        let f = match self.check_fn(id, Some(&def_ast), &args, &def_ast.body, overflow, def_ast.span) {
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

pub fn cname(s: &str) -> String {
    s.replace('?', "_q").replace('!', "_b")
}

pub fn type_from(t: &TypeExpr, structs: &Structs) -> R<Ty> {
    match t {
        TypeExpr::Named(n, sp) => match n.as_str() {
            "Float" => Ok(Ty::Float),
            "Bool" => Ok(Ty::Bool),
            "Str" => Ok(Ty::Str),
            _ => match IntKind::from_name(n) {
                Some(k) => Ok(Ty::of_kind(k)),
                None => structs.get(n).cloned().ok_or_else(|| Diag::new(*sp, format!("unknown type `{n}`"))),
            },
        },
        TypeExpr::Array(t, _) => Ok(Ty::arr(type_from(t, structs)?)),
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
    /// Kind of each enclosing loop-ish construct, innermost last.
    loops: Vec<LoopKind>,
    n_params: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum LoopKind {
    While,
    Block,
    Gen,
}

/// Method names offered as suggestions, by receiver kind.
const SEQ_METHODS: &[&str] = &[
    "select", "filter", "reject", "map", "flat_map", "take_while", "drop", "take", "each_with_index", "lazy", "sum", "max", "min", "max_by", "min_by",
    "first", "to_a", "each", "reduce", "inject", "all?", "any?", "count", "include?", "sort", "size", "length",
];
const ARRAY_EXTRA: &[&str] = &["each_index", "each_cons", "pmap", "last", "reverse", "<<"];
const INT_METHODS: &[&str] = &["to_s", "to_f", "to_i", "to_u8", "to_i32", "to_u32", "to_u64", "as_u8", "as_i32", "as_u32", "as_u64", "even?", "odd?", "digits", "step"];
const FLOAT_METHODS: &[&str] = &["to_s", "to_f", "to_i", "abs"];
const STR_METHODS: &[&str] = &["chars", "bytes", "size", "length", "reverse", "delete", "split", "to_i", "to_s"];

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
            is_main: def.is_none(),
            ret: Ty::Unit,
            impure: false,
            fallible_decl: def.is_some_and(|d| d.fallible),
            under_try: false,
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
            Ty::Seq(t, l) => Ty::seq(self.resolve(t), *l),
            Ty::Gen(t) => Ty::Gen(Box::new(self.resolve(t))),
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
            (Ty::Array(x), Ty::Array(y)) | (Ty::Gen(x), Ty::Gen(y)) | (Ty::Yielder(x), Ty::Yielder(y)) => self.unify(x, y),
            (Ty::Seq(x, _), Ty::Seq(y, _)) => self.unify(x, y),
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
        self.locals.push(Local { name: name.to_string(), ty, reassigned: 0, mutated: false, pushed: false });
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
                Some(t) => type_from(t, &self.w.structs)?,
                None => self.fresh(),
            };
        }
        let (mut stmts, tail_ty) = self.body(body)?;
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
        Ok(TFunc {
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
            TK::Call(f, args) => TK::Call(f, args.into_iter().map(|a| self.zonk(a)).collect()),
            TK::M(m, r, args, blk) => TK::M(
                m,
                r.map(b),
                args.into_iter().map(|a| self.zonk(a)).collect(),
                blk.map(|bl| Box::new(TBlock { body: self.zonk_stmts(bl.body), ..*bl })),
            ),
            TK::Try(x) => TK::Try(b(x)),
            TK::Puts(x) => TK::Puts(b(x)),
            TK::Array(xs) => TK::Array(xs.into_iter().map(|a| self.zonk(a)).collect()),
            TK::Format(ps, xs) => TK::Format(ps, xs.into_iter().map(|a| self.zonk(a)).collect()),
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
        let mut out = vec![];
        let mut last = Ty::Unit;
        for s in stmts {
            let t = self.stmt(s)?;
            last = match &t {
                TStmt::Expr(e) => e.ty.clone(),
                TStmt::Next(_) | TStmt::Break(..) | TStmt::Return(..) => Ty::Never,
                _ => Ty::Unit,
            };
            out.push(t);
        }
        Ok((out, last))
    }

    fn stmt(&mut self, s: &Stmt) -> R<TStmt> {
        Ok(match &s.kind {
            StmtKind::Expr(e) => TStmt::Expr(self.expr(e)?),
            StmtKind::MultiAssign(targets, values) => {
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
                let ty = type_from(te, &self.w.structs)?;
                let v = self.value(e)?;
                let v = self.coerce(v, &ty)?;
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
                    None => self.declare(name, ty.clone()),
                };
                TStmt::Expr(self.mk(TK::Assign(id, Box::new(v)), ty, s.span))
            }
            StmtKind::While(c, body) => {
                let c = self.cond(c)?;
                self.loops.push(LoopKind::While);
                self.scopes.push(HashMap::new());
                let (b, _) = self.body(body)?;
                self.scopes.pop();
                self.loops.pop();
                TStmt::While(c, b)
            }
            StmtKind::If(c, a, b) => {
                let c = self.cond(c)?;
                let (a, _) = self.body(a)?;
                let (b, _) = self.body(b)?;
                TStmt::If(c, a, b)
            }
            StmtKind::Next => {
                if self.loops.is_empty() {
                    return Err(Diag::new(s.span, "`next` outside a loop or block"));
                }
                TStmt::Next(s.span)
            }
            StmtKind::Break(v) => {
                if self.loops.is_empty() {
                    return Err(Diag::new(s.span, "`break` outside a loop or block"));
                }
                TStmt::Break(v.as_ref().map(|v| self.value(v)).transpose()?, s.span)
            }
            StmtKind::Return(v) => {
                if self.is_main {
                    return Err(Diag::new(s.span, "`return` at the top level"));
                }
                let v = v.as_ref().map(|v| self.value(v)).transpose()?;
                let rt = self.ret.clone();
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
        Ok(self.declare(name, ty.clone()))
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
            ExprKind::Bool(b) => self.mk(TK::Bool(*b), Ty::Bool, sp),
            ExprKind::Nil => self.mk(TK::Unit, Ty::Unit, sp),
            ExprKind::Sym(s) => return Err(Diag::new(sp, format!("symbol `:{s}` can only be used as a block (`&:{s}`) or with `reduce`"))),
            ExprKind::Name(n) => match self.lookup(n) {
                Some(id) => self.mk(TK::Local(id), self.locals[id].ty.clone(), sp),
                None => return self.call(None, n, sp, &[], None, None, sp),
            },
            ExprKind::Const(c) => match self.w.consts.get(c).cloned() {
                Some((v, ty)) => return self.const_value(v, ty, sp),
                None => return Err(Diag::new(sp, format!("`{c}` is not a value; call a method on it (`{c}.new`, ...)"))),
            },
            ExprKind::Call { recv, name, name_span, args, block, block_sym } => {
                return self.call(recv.as_deref(), name, *name_span, args, block.as_deref(), block_sym.as_ref(), sp);
            }
            ExprKind::Index(a, i) => {
                let a = self.value(a)?;
                let i = self.index_value(i)?;
                let el = match self.resolve(&a.ty) {
                    Ty::Array(t) => *t,
                    Ty::Var(_) => self.unknown(a.span, "the indexed value")?,
                    t => return Err(Diag::new(a.span, format!("cannot index into {}", t.show()))),
                };
                self.mk(TK::Index(Box::new(a), Box::new(i)), el, sp)
            }
            ExprKind::Assign(target, v) => match &target.kind {
                ExprKind::Name(n) => {
                    let v = self.value(v)?;
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
                        Ty::Array(t) => *t,
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
                if !self.unify(&a.ty, &b.ty) {
                    return Err(Diag::new(sp, format!("branches have different types: {} and {}", self.resolve(&a.ty).show(), self.resolve(&b.ty).show())));
                }
                let ty = a.ty.clone();
                self.mk(TK::Ternary(Box::new(c), Box::new(a), Box::new(b)), ty, sp)
            }
            ExprKind::Try(x) => {
                let saved = std::mem::replace(&mut self.under_try, true);
                let inner = self.value(x);
                self.under_try = saved;
                let inner = inner?;
                if !self.is_main && !self.fallible_decl {
                    return Err(Diag::new(sp, format!("`try` in `{}`, which isn't fallible: declare it `-> T!`", self.fn_name)));
                }
                if !is_fallible_expr(&inner, self) {
                    return Err(Diag::new(sp, "`try` needs a fallible call or arithmetic"));
                }
                let ty = inner.ty.clone();
                self.mk(TK::Try(Box::new(inner)), ty, sp)
            }
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
        match &e.kind {
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
                    Ty::Array(t) => *t,
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
                        Ty::Array(e) => *e,
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
            Ty::Float => TK::Float(0.0),
            Ty::Bool => TK::Bool(false),
            Ty::Str => TK::Str(String::new()),
            Ty::Array(_) => TK::Array(vec![]),
            Ty::Struct(_, fs) => {
                let vals = fs.clone().iter().map(|(_, ft)| self.zero_of(ft, sp)).collect::<Option<Vec<_>>>()?;
                TK::M(M::StructNew, None, vals, None)
            }
            _ => return None,
        };
        Some(self.mk(kind, t.clone(), sp))
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

    /// A top-level constant used as a value.
    fn const_value(&mut self, v: CVal, ty: Option<Ty>, sp: Span) -> R<TExpr> {
        Ok(match (v, ty) {
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
        if matches!(op, BinOp::And | BinOp::Or) {
            self.expect(&l.ty, &Ty::Bool, l.span, &format!("`{}`", op.text()))?;
            self.expect(&r.ty, &Ty::Bool, r.span, &format!("`{}`", op.text()))?;
            return Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), Ty::Bool, sp));
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
                    return self.coerce(e, &ty);
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
                    if k.is_none() && operand != Ty::Str {
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
        // Constant receivers: Int.sqrt, Array.new, File.read, Enumerator.new.
        if let Some(Expr { kind: ExprKind::Const(c), span: csp, .. }) = recv {
            if !self.w.consts.contains_key(c) {
                return self.const_call(c, *csp, name, name_span, args, block, sp);
            }
        }
        let Some(recv) = recv else {
            return self.global_call(name, name_span, args, block, bsym, sp);
        };
        let r = self.expr(recv)?;
        self.method(r, name, name_span, args, block, bsym, sp)
    }

    fn global_call(&mut self, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, bsym: Option<&(String, Span)>, sp: Span) -> R<TExpr> {
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
                return Ok(self.mk(TK::Puts(Box::new(v)), Ty::Unit, sp));
            }
            "loop" => {
                let Some(b) = block else { return Err(Diag::new(sp, "`loop` needs a block")) };
                self.loops.push(LoopKind::While);
                self.scopes.push(HashMap::new());
                let (body, _) = self.body(&b.body)?;
                self.scopes.pop();
                self.loops.pop();
                let blk = TBlock { params: vec![], destructure: false, body, pure: true, span: b.span , own: (0, 0) };
                return Ok(self.mk(TK::M(M::Loop, None, vec![], Some(Box::new(blk))), Ty::Unit, sp));
            }
            "it" => return Err(Diag::new(sp, "`it` can only be used inside a block")),
            "format" | "sprintf" => return self.format(name, args, sp),
            _ => {}
        }
        let _ = bsym;
        let Some(&def) = self.w.by_name.get(name) else {
            let mut msg = format!("undefined local variable or method `{name}`");
            let cands: Vec<String> = self.scopes.iter().flat_map(|s| s.keys().cloned()).chain(self.w.by_name.keys().cloned()).collect();
            if let Some(best) = cands.iter().filter(|c| lev(c, name) <= 2).min_by_key(|c| lev(c, name)) {
                msg.push_str(&format!("; did you mean `{best}`?"));
            }
            return Err(Diag::new(name_span, msg));
        };
        if block.is_some() {
            return Err(Diag::new(sp, format!("`{name}` doesn't take a block")));
        }
        let d = self.w.defs[def].def.clone();
        let ext = self.w.defs[def].external.as_ref().map(|e| e.params.clone());
        if args.len() != d.params.len() {
            return Err(Diag::new(name_span, format!("`{name}` takes {} argument(s), got {}", d.params.len(), args.len())));
        }
        let mut targs = vec![];
        for (i, (a, p)) in args.iter().zip(&d.params).enumerate() {
            let v = self.value(a)?;
            let want = match (&ext, &p.ty) {
                (Some(ts), _) => Some(ts[i].clone()),
                (None, Some(t)) => Some(type_from(t, &self.w.structs)?),
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
        let arg_tys: Vec<Ty> = targs.iter().map(|a| self.resolve(&a.ty)).collect();
        if arg_tys.iter().any(Ty::has_var) {
            let t = self.unknown(sp, "this call's arguments")?;
            return Ok(self.mk(TK::Unit, t, sp));
        }
        let callee_pure = d.pure || self.w.defs[def].external.as_ref().is_some_and(|e| e.pure);
        if self.pure_decl && !callee_pure {
            return Err(Diag::new(name_span, format!("`#[pure] def {}` calls `{name}`, which isn't pure", self.fn_name)));
        }
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
        if self.is_main {
            let (ty, sp) = (e.ty.clone(), e.span);
            return Ok(self.mk(TK::Try(Box::new(e)), ty, sp));
        }
        Err(Diag::new(e.span, format!("`{name}` can fail: handle it with `try`")))
    }

    fn const_call(&mut self, c: &str, csp: Span, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, sp: Span) -> R<TExpr> {
        let argv = |cx: &mut Self| args.iter().map(|a| cx.value(a)).collect::<R<Vec<_>>>();
        if let (Some(st), "new") = (self.w.structs.get(c).cloned(), name) {
            let Ty::Struct(_, fields) = &st else { unreachable!() };
            if args.iter().any(|a| matches!(a.kind, ExprKind::KwArg(..))) {
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
            ("Array", "new") => {
                let a = argv(self)?;
                if a.len() != 2 {
                    return Err(Diag::new(sp, "`Array.new(size, value)` takes two arguments"));
                }
                self.expect(&a[0].ty, &Ty::Int, a[0].span, "Array.new size")?;
                let el = a[1].ty.clone();
                Ok(self.mk(TK::M(M::ArrayNew, None, a, None), Ty::arr(el), sp))
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
                let (body, _) = self.body(&b.body)?;
                let pure = !self.impure;
                self.impure = saved || self.impure;
                self.loops.pop();
                self.scopes.pop();
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
                    self.scopes.pop();
                    return Err(Diag::new(b.span, format!("block takes {} parameters but each element is {}", b.params.len(), t.show())));
                }
            }
        } else {
            self.scopes.pop();
            return Err(Diag::new(b.span, format!("block takes {} parameter(s), expected {}", b.params.len(), params.len())));
        }
        self.loops.push(LoopKind::Block);
        let saved = std::mem::replace(&mut self.impure, false);
        let r = self.body(&b.body);
        let pure = !self.impure;
        self.impure = saved || self.impure;
        self.loops.pop();
        self.scopes.pop();
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
        self.scopes.pop();
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
            Ty::Array(t) => Some((*t, false)),
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
    fn method(&mut self, recv: TExpr, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, bsym: Option<&(String, Span)>, sp: Span) -> R<TExpr> {
        let rt = self.resolve(&recv.ty);
        if let Ty::Var(_) = rt {
            if self.strict {
                return Err(Diag::new(recv.span, format!("cannot infer the type of the receiver of `{name}`; add a type annotation")));
            }
            self.unresolved = true;
            let t = self.fresh();
            return Ok(self.mk(TK::Unit, t, sp));
        }
        let argv = |cx: &mut Self| args.iter().map(|a| cx.value(a)).collect::<R<Vec<_>>>();
        let mk_m = |cx: &mut Self, m: M, r: TExpr, a: Vec<TExpr>, b: Option<TBlock>, ty: Ty| cx.mk(TK::M(m, Some(Box::new(r)), a, b.map(Box::new)), ty, sp);
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
        // Struct fields
        if let Ty::Struct(sn, fs) = &rt {
            if let Some((k, ft)) = rt.field(name) {
                if !args.is_empty() || block.is_some() {
                    return Err(Diag::new(sp, format!("`{sn}.{name}` is a field, not a method")));
                }
                return Ok(mk_m(self, M::TupleGet(k), recv, vec![], None, ft));
            }
            return Err(self.no_field(sn, fs, name, name_span));
        }
        // Float methods
        if rt == Ty::Float {
            match name {
                "to_f" => return Ok(recv),
                "to_i" | "to_int" | "truncate" => return Ok(mk_m(self, M::FloatToI, recv, vec![], None, Ty::Int)),
                "abs" => return Ok(mk_m(self, M::FloatAbs, recv, vec![], None, Ty::Float)),
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
                "chars" => return Ok(mk_m(self, M::Chars, recv, vec![], None, Ty::seq(Ty::Str, false))),
                "bytes" => return Ok(mk_m(self, M::Bytes, recv, vec![], None, Ty::seq(Ty::Int, false))),
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
        // Arrays: push, size, indexing helpers.
        if let Ty::Array(el) = &rt {
            let el = (**el).clone();
            match name {
                "<<" | "push" => {
                    let a = argv(self)?;
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
                "reverse" => return Ok(mk_m(self, M::Reverse, recv, vec![], None, Ty::arr(el))),
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
                    self.expect(&kt, &Ty::Int, blk.span, &format!("`{name}` key"))?;
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
                    let (blk, _) = self.any_block(block, bsym, &el, sp, name)?;
                    return Ok(mk_m(self, M::Each, recv, vec![], Some(blk), Ty::Unit));
                }
                "reduce" | "inject" => {
                    let a = args;
                    let (init, op) = match (a, block, bsym) {
                        ([Expr { kind: ExprKind::Sym(s), span, .. }], None, None) => (None, Some((s.clone(), *span))),
                        ([init, Expr { kind: ExprKind::Sym(s), span, .. }], None, None) => (Some(self.value(init)?), Some((s.clone(), *span))),
                        ([], Some(_), None) => (None, None),
                        ([init], Some(_), None) => (Some(self.value(init)?), None),
                        _ => return Err(Diag::new(sp, "`reduce` takes a symbol (`reduce(:*)`) or a block")),
                    };
                    if let Some(i) = &init {
                        self.expect(&i.ty, &el, i.span, "reduce initial value")?;
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
                            self.scopes.pop();
                            let la = self.mk(TK::Local(a), el.clone(), sp);
                            let lx = self.mk(TK::Local(x), el.clone(), sp);
                            let body = self.binary(bop, la, lx, sp)?;
                            TBlock { params: vec![a, x], destructure: false, body: vec![TStmt::Expr(body)], pure: true, span: ssp , own: (0, 0) }
                        }
                        None => {
                            let (b, t) = self.block_n(block.unwrap(), &[el.clone(), el.clone()], false)?;
                            self.expect(&t, &el, b.span, "reduce block")?;
                            b
                        }
                    };
                    return Ok(mk_m(self, M::Reduce, recv, init.into_iter().collect(), Some(blk), el));
                }
                "all?" | "any?" => {
                    let (blk, bt) = self.any_block(block, bsym, &el, sp, name)?;
                    self.expect(&bt, &Ty::Bool, blk.span, &format!("`{name}` block"))?;
                    return Ok(mk_m(self, if name == "all?" { M::All } else { M::Any }, recv, vec![], Some(blk), Ty::Bool));
                }
                "count" | "size" | "length" if block.is_none() => {
                    return Ok(mk_m(self, M::Count, recv, vec![], None, Ty::Int));
                }
                "include?" => {
                    let a = argv(self)?;
                    self.expect(&a[0].ty, &el, a[0].span, "include?")?;
                    return Ok(mk_m(self, M::Include, recv, a, None, Ty::Bool));
                }
                "sort" => {
                    if !matches!(self.resolve(&el), Ty::Int | Ty::Str) {
                        return Err(Diag::new(sp, "`sort` needs Int or Str elements"));
                    }
                    return Ok(mk_m(self, M::Sort, recv, vec![], None, Ty::arr(el)));
                }
                "each_index" | "each_cons" | "pmap" | "last" | "reverse" => {
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
            let bad = |what: &str| Diag::new(v.span, format!("`%{spec}` needs {what}, got {}", t.show()));
            let piece = match spec.as_str() {
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
                    let base = match spec.as_str() {
                        "o" => 8,
                        "b" => 2,
                        _ => 16,
                    };
                    FmtPiece::Base(k, base, spec == "X")
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
                "v" => {
                    if !matches!(t, Ty::Int | Ty::IntK(_) | Ty::Float | Ty::Str | Ty::Bool) {
                        return Err(bad("an Int, Float, Str or Bool"));
                    }
                    FmtPiece::Str(k)
                }
                "t" => {
                    if t != Ty::Bool {
                        return Err(bad("a Bool"));
                    }
                    FmtPiece::Str(k)
                }
                _ if spec.ends_with('f') && (spec == "f" || (spec.starts_with('.') && spec[1..spec.len() - 1].parse::<u32>().is_ok())) => {
                    if !matches!(t, Ty::Int | Ty::Float) {
                        return Err(bad("a number"));
                    }
                    let prec = if spec == "f" { 6 } else { spec[1..spec.len() - 1].parse::<u32>().unwrap().min(40) };
                    FmtPiece::Fixed(k, prec)
                }
                _ => return Err(Diag::new(*fsp, format!("unsupported directive `%{spec}`")).note("supported so far (Go's fmt verbs): %v, %d, %s, %t, %f, %.Nf, %x, %X, %o, %b, %c, %%")),
            };
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
            .map(|(i, v)| if pieces.iter().any(|p| matches!(p, FmtPiece::Fixed(j, _) if *j == i)) { self.coerce(v, &Ty::Float) } else { Ok(v) })
            .collect::<R<Vec<_>>>()?;
        Ok(self.mk(TK::Format(pieces, vals), Ty::Str, sp))
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
        if let Some(best) = cands.iter().filter(|c| lev(c, name) <= 2).min_by_key(|c| lev(c, name)) {
            msg.push_str(&format!("; did you mean `{best}`?"));
        }
        Diag::new(sp, msg)
    }
}

fn occurs(v: u32, t: &Ty) -> bool {
    match t {
        Ty::Var(x) => *x == v,
        Ty::Array(t) | Ty::Seq(t, _) | Ty::Gen(t) | Ty::Yielder(t) => occurs(v, t),
        Ty::Tuple(ts) => ts.iter().any(|t| occurs(v, t)),
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
        TK::Bin(op, ..) => op.is_arith(),
        TK::Neg(_) => true,
        _ => false,
    }
}

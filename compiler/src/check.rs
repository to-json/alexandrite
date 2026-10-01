//! Type checking and inference.
//!
//! Every function is checked once per distinct tuple of argument types
//! (monomorphization, Crystal-style). Inside a function, types are inferred
//! with unification. A local whose type is only fixed later in the body
//! (`found = []` ... `found << n`) is handled by re-checking: each pass
//! records the concrete types it learned per binding site and the next pass
//! starts from them. Only the final pass reports errors.

use crate::ast::*;
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
}

const PASSES: usize = 4;

impl<'a> World<'a> {
    pub fn new(sm: &'a SourceMap, defs: Vec<DefInfo>) -> R<Self> {
        let mut by_name = HashMap::new();
        for (i, d) in defs.iter().enumerate() {
            if by_name.insert(d.def.name.clone(), i).is_some() {
                return Err(Diag::new(d.def.name_span, format!("`{}` is defined twice", d.def.name)));
            }
        }
        Ok(World { sm, defs, by_name, instances: HashMap::new(), funcs: vec![], sigs: HashMap::new(), fatal: None })
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
                    Some(t) => tys.push(type_from(t)?),
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
            self.sigs.insert(id, (type_from(ret)?, def_ast.fallible, def_ast.pure));
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

pub fn type_from(t: &TypeExpr) -> R<Ty> {
    match t {
        TypeExpr::Named(n, sp) => match n.as_str() {
            "Int" => Ok(Ty::Int),
            "Bool" => Ok(Ty::Bool),
            "Str" => Ok(Ty::Str),
            _ => Err(Diag::new(*sp, format!("unknown type `{n}`"))),
        },
        TypeExpr::Array(t, _) => Ok(Ty::arr(type_from(t)?)),
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
const INT_METHODS: &[&str] = &["to_s", "even?", "odd?", "digits", "step", "abs"];
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
                Some(t) => type_from(t)?,
                None => self.fresh(),
            };
        }
        let (mut stmts, tail_ty) = self.body(body)?;
        if !self.is_main {
            let sp = def.map_or(Span::default(), |d| d.span);
            // The value of the last statement is the return value.
            if let Some(TStmt::Expr(e)) = stmts.last_mut() {
                let e2 = self.materialize(e.clone());
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
        let ty = self.resolve(&e.ty);
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
                let t = v.as_ref().map_or(Ty::Unit, |v| v.ty.clone());
                let rt = self.ret.clone();
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
            ExprKind::Int(v) => self.mk(TK::Int(*v), Ty::Int, sp),
            ExprKind::Str(s) => self.mk(TK::Str(s.clone()), Ty::Str, sp),
            ExprKind::Bool(b) => self.mk(TK::Bool(*b), Ty::Bool, sp),
            ExprKind::Nil => self.mk(TK::Unit, Ty::Unit, sp),
            ExprKind::Sym(s) => return Err(Diag::new(sp, format!("symbol `:{s}` can only be used as a block (`&:{s}`) or with `reduce`"))),
            ExprKind::Name(n) => match self.lookup(n) {
                Some(id) => self.mk(TK::Local(id), self.locals[id].ty.clone(), sp),
                None => return self.call(None, n, sp, &[], None, None, sp),
            },
            ExprKind::Const(c) => return Err(Diag::new(sp, format!("`{c}` is not a value; call a method on it (`{c}.new`, ...)"))),
            ExprKind::Call { recv, name, name_span, args, block, block_sym } => {
                return self.call(recv.as_deref(), name, *name_span, args, block.as_deref(), block_sym.as_ref(), sp);
            }
            ExprKind::Index(a, i) => {
                let a = self.value(a)?;
                let i = self.value(i)?;
                self.expect(&i.ty, &Ty::Int, i.span, "index")?;
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
                ExprKind::Index(arr, idx) => {
                    let ExprKind::Name(an) = &arr.kind else {
                        return Err(Diag::new(arr.span, "can only assign into an element of a local array"));
                    };
                    let Some(id) = self.lookup(an) else {
                        return Err(Diag::new(arr.span, format!("unknown local `{an}`")));
                    };
                    if self.pure_decl && id < self.param_count() {
                        return Err(Diag::new(sp, format!("`#[pure] def {}` can't mutate its argument `{an}`", self.fn_name)));
                    }
                    self.locals[id].mutated = true;
                    let idx = self.value(idx)?;
                    self.expect(&idx.ty, &Ty::Int, idx.span, "index")?;
                    let v = self.value(v)?;
                    let el = match self.resolve(&self.locals[id].ty.clone()) {
                        Ty::Array(t) => *t,
                        Ty::Var(_) => self.unknown(arr.span, "the array")?,
                        t => return Err(Diag::new(arr.span, format!("cannot assign into {}", t.show()))),
                    };
                    self.expect(&v.ty, &el, v.span, "element assignment")?;
                    let ty = v.ty.clone();
                    self.mk(TK::IndexAssign(id, Box::new(idx), Box::new(v)), ty, sp)
                }
                _ => return Err(Diag::new(target.span, "cannot assign to this")),
            },
            ExprKind::OpAssign(op, target, v) => {
                let ExprKind::Name(n) = &target.kind else {
                    return Err(Diag::new(target.span, "compound assignment needs a local variable"));
                };
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
                self.expect(&x.ty, &Ty::Int, x.span, "negation")?;
                self.mk(TK::Neg(Box::new(x)), Ty::Int, sp)
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

    fn param_count(&self) -> usize {
        self.n_params
    }

    fn binary(&mut self, op: BinOp, l: TExpr, r: TExpr, sp: Span) -> R<TExpr> {
        let (lt, rt) = (self.resolve(&l.ty), self.resolve(&r.ty));
        let ty = match op {
            BinOp::And | BinOp::Or => {
                self.expect(&lt, &Ty::Bool, l.span, &format!("`{}`", op.text()))?;
                self.expect(&rt, &Ty::Bool, r.span, &format!("`{}`", op.text()))?;
                Ty::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                if !self.unify(&lt, &rt) {
                    return Err(Diag::new(sp, format!("cannot compare {} with {}", lt.show(), rt.show())));
                }
                Ty::Bool
            }
            _ => {
                // Arithmetic and ordering: Int (Str supports ordering).
                let known = if !matches!(lt, Ty::Var(_)) { lt.clone() } else { rt.clone() };
                let operand = match known {
                    Ty::Var(_) => self.unknown(sp, "this operand")?,
                    Ty::Str if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) => Ty::Str,
                    t => t,
                };
                if !matches!(operand, Ty::Var(_)) {
                    for (t, s) in [(&l.ty, l.span), (&r.ty, r.span)] {
                        let t = t.clone();
                        if !self.unify(&t, &operand) || !matches!(self.resolve(&t), Ty::Int | Ty::Str | Ty::Var(_)) {
                            return Err(Diag::new(s, format!("`{}` needs Int operands, got {}", op.text(), self.resolve(&t).show())));
                        }
                    }
                }
                if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
                    Ty::Bool
                } else {
                    operand
                }
            }
        };
        Ok(self.mk(TK::Bin(op, Box::new(l), Box::new(r)), ty, sp))
    }

    // ---------- calls ----------

    #[allow(clippy::too_many_arguments)]
    fn call(&mut self, recv: Option<&Expr>, name: &str, name_span: Span, args: &[Expr], block: Option<&Block>, bsym: Option<&(String, Span)>, sp: Span) -> R<TExpr> {
        // Constant receivers: Int.sqrt, Array.new, File.read, Enumerator.new.
        if let Some(Expr { kind: ExprKind::Const(c), span: csp, .. }) = recv {
            return self.const_call(c, *csp, name, name_span, args, block, sp);
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
                (None, Some(t)) => Some(type_from(t)?),
                (None, None) => None,
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
        match (c, name) {
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
        // Int methods
        if rt == Ty::Int {
            match name {
                "to_s" => return Ok(mk_m(self, M::ToS, recv, vec![], None, Ty::Str)),
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
                    self.expect(&t, &Ty::Int, sp, "`sum` elements")?;
                    return Ok(mk_m(self, M::Sum, recv, vec![], blk, Ty::Int));
                }
                "max" | "min" => {
                    if !matches!(self.resolve(&el), Ty::Int | Ty::Str) {
                        return Err(Diag::new(sp, format!("`{name}` needs Int or Str elements, got {}", self.resolve(&el).show())));
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
            Ty::Int => cands.extend(INT_METHODS),
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

//! Typed tree → LIR.
//!
//! Enumerable chains are fused here: a terminal (`sum`, `max`, `to_a`, ...)
//! walks down its receiver collecting stages (`select`, `map`, `flat_map`,
//! ...) to a source (Range, Array, Enumerator, Str#chars, each_index,
//! each_cons) and emits one loop. Without `.lazy`, a chain whose blocks do
//! I/O is materialized stage by stage instead (Ruby's eager semantics).

use crate::ast::{BinOp, IntKind, Overflow};
use crate::diag::{SourceMap, Span};
use crate::lir::*;
use crate::tast::*;
use std::cell::RefCell;
use std::collections::HashMap;

pub struct Opts {
    pub release: bool,
}

pub fn lower(p: &TProgram, sm: &SourceMap, opts: &Opts) -> LProgram {
    let prog = RefCell::new(LProgram::default());
    for f in &p.funcs {
        let lf = if f.external {
            let mut lw = Lw::new(p, sm, opts, f, f.overflow, ErrPath::Return(vec![]), &prog);
            let params = f.params.iter().map(|l| lw.var_of(*l)).collect();
            LFunc { name: f.cname.clone(), params, vars: lw.vars, ret: lty(&f.ret, f.overflow), fallible: f.fallible, body: vec![], external: true, is_main: false, labels: 0 }
        } else {
            let path = if f.is_main { ErrPath::Die(vec![]) } else { ErrPath::Return(vec![]) };
            let mut lw = Lw::new(p, sm, opts, f, f.overflow, path, &prog);
            let params: Vec<V> = f.params.iter().map(|l| lw.var_of(*l)).collect();
            lw.analyze_facts(&f.body);
            let body = lw.body_with_return(&f.body, !f.is_main && f.ret != Ty::Unit);
            LFunc { name: f.cname.clone(), params, vars: lw.vars, ret: lty(&f.ret, f.overflow), fallible: f.fallible, body, external: false, is_main: f.is_main, labels: lw.labels }
        };
        prog.borrow_mut().funcs.push(lf);
    }
    let mut prog = prog.into_inner();
    prog.uses_pint = p.funcs.iter().any(|f| f.overflow == Overflow::Promote);
    prog
}

pub fn lty(t: &Ty, mode: Overflow) -> LTy {
    match t {
        Ty::Int => {
            if mode == Overflow::Promote {
                LTy::PInt
            } else {
                LTy::I64
            }
        }
        Ty::IntK(k) => LTy::IntK(*k),
        // (present, value)
        Ty::Opt(t) => LTy::Tup(vec![LTy::Bool, lty(t, mode)]),
        Ty::Float => LTy::F64,
        Ty::Struct(_, fs) => LTy::Tup(fs.iter().map(|(_, t)| lty(t, mode)).collect()),
        Ty::Bool => LTy::Bool,
        Ty::Str => LTy::Str,
        Ty::Unit | Ty::Never | Ty::Yielder(_) | Ty::Var(_) => LTy::Unit,
        Ty::Array(t) | Ty::Fixed(t, _) | Ty::Seq(t, _) => LTy::Arr(Box::new(lty(t, mode))),
        Ty::Map(k, v) => crate::mapgen::map_ty(&lty(k, mode), &lty(v, mode)),
        Ty::Tuple(ts) => LTy::Tup(ts.iter().map(|t| lty(t, mode)).collect()),
        Ty::Range => LTy::Range,
        Ty::Gen(t) => LTy::Gen(Box::new(lty(t, mode))),
    }
}

/// What is known about an Int local while its block runs.
#[derive(Clone, Copy, Debug)]
enum Fact {
    Interval(i64, i64),
    /// An index produced by `each_index` over this array local.
    IndexOf(LocalId),
}

struct Lw<'a> {
    p: &'a TProgram,
    sm: &'a SourceMap,
    opts: &'a Opts,
    f: &'a TFunc,
    mode: Overflow,
    vars: Vec<LVar>,
    local_var: HashMap<LocalId, V>,
    out: Vec<Vec<LS>>,
    labels: usize,
    /// Loop targets, each with the defer depth where its body starts.
    next_target: Vec<(Option<Label>, usize)>,
    break_target: Vec<(Label, usize)>,
    /// `defer`red expressions per open block, innermost last.
    defers: Vec<Vec<TExpr>>,
    path: ErrPath,
    /// Inside `try (arith)`: arithmetic fails to the error path.
    try_arith: bool,
    consts: HashMap<LocalId, i64>,
    fixed_len: HashMap<LocalId, i64>,
    facts: HashMap<LocalId, Fact>,
    prog: &'a RefCell<LProgram>,
}

/// A stage of a pipeline with its pre-loop state.
struct Stage<'t> {
    m: M,
    node: &'t TExpr,
    counter: Option<V>,
    limit: Option<V>,
}

impl<'a> Lw<'a> {
    fn new(p: &'a TProgram, sm: &'a SourceMap, opts: &'a Opts, f: &'a TFunc, mode: Overflow, path: ErrPath, prog: &'a RefCell<LProgram>) -> Self {
        Lw {
            p,
            sm,
            opts,
            f,
            mode,
            vars: vec![],
            local_var: HashMap::new(),
            out: vec![vec![]],
            labels: 0,
            next_target: vec![],
            break_target: vec![],
            defers: vec![],
            path,
            try_arith: false,
            consts: HashMap::new(),
            fixed_len: HashMap::new(),
            facts: HashMap::new(),
            prog,
        }
    }

    fn loc(&self, sp: Span) -> String {
        self.sm.loc(sp)
    }

    fn lty(&self, t: &Ty) -> LTy {
        lty(t, self.mode)
    }

    fn new_var(&mut self, name: &str, ty: LTy) -> V {
        self.vars.push(LVar { name: name.to_string(), ty });
        self.vars.len() - 1
    }

    fn tmp(&mut self, ty: LTy) -> V {
        self.new_var("t", ty)
    }

    fn var_of(&mut self, l: LocalId) -> V {
        if let Some(v) = self.local_var.get(&l) {
            return *v;
        }
        let loc = &self.f.locals[l];
        let ty = lty(&loc.ty, self.mode);
        let v = self.new_var(&crate::check::cname(&loc.name), ty);
        self.local_var.insert(l, v);
        v
    }

    fn label(&mut self) -> Label {
        self.labels += 1;
        self.labels - 1
    }

    fn emit(&mut self, s: LS) {
        self.out.last_mut().unwrap().push(s);
    }

    fn sub(&mut self, f: impl FnOnce(&mut Self)) -> Vec<LS> {
        self.out.push(vec![]);
        f(self);
        self.out.pop().unwrap()
    }

    fn sub_val(&mut self, f: impl FnOnce(&mut Self) -> LE) -> (Vec<LS>, LE) {
        self.out.push(vec![]);
        let v = f(self);
        (self.out.pop().unwrap(), v)
    }

    /// Bind a value to a temp unless it is already trivially re-evaluable.
    fn bind(&mut self, e: LE, ty: LTy) -> LE {
        match e {
            LE::Var(_) | LE::I(_) | LE::B(_) | LE::Unit => e,
            _ => {
                let t = self.tmp(ty);
                self.emit(LS::Set(t, e));
                LE::Var(t)
            }
        }
    }

    // ---------- Int representation helpers ----------

    fn promote(&self) -> bool {
        self.mode == Overflow::Promote
    }
    /// An i64-valued builtin result as an Int of this function's mode.
    fn int_out(&self, e: LE) -> LE {
        if self.promote() { LE::ToP(Box::new(e)) } else { e }
    }
    /// An Int of this function's mode, needed as a machine i64.
    fn int_in(&self, e: LE, sp: Span) -> LE {
        if self.promote() { LE::Rt(Rt::PToI64, vec![e, LE::Loc(self.loc(sp))]) } else { e }
    }
    fn ovf(&self, sp: Span) -> Ovf {
        match self.mode {
            Overflow::Wrap => Ovf::Wrap,
            _ if self.f.pure && self.opts.release && !self.try_arith => Ovf::Unchecked,
            _ => Ovf::Panic(self.loc(sp)),
        }
    }

    // ---------- facts for check elision (release) ----------

    fn analyze_facts(&mut self, body: &[TStmt]) {
        for s in body {
            if let TStmt::Expr(TExpr { kind: TK::Assign(l, v), .. }) = s {
                let loc = &self.f.locals[*l];
                if loc.reassigned == 0 {
                    if let Some(c) = self.konst(v) {
                        self.consts.insert(*l, c);
                    }
                    if let TK::M(M::ArrayNew, None, args, _) = &v.kind {
                        if !loc.pushed {
                            if let Some(n) = self.konst(&args[0]) {
                                self.fixed_len.insert(*l, n);
                            }
                        }
                    }
                    if let TK::Array(items) = &v.kind {
                        if !loc.pushed {
                            self.fixed_len.insert(*l, items.len() as i64);
                        }
                    }
                }
            }
        }
    }

    fn konst(&self, e: &TExpr) -> Option<i64> {
        self.interval(e).and_then(|(lo, hi)| (lo == hi).then_some(lo))
    }

    fn interval(&self, e: &TExpr) -> Option<(i64, i64)> {
        match &e.kind {
            TK::Int(v) => Some((*v, *v)),
            TK::Local(l) => match (self.consts.get(l), self.facts.get(l)) {
                (Some(c), _) => Some((*c, *c)),
                (_, Some(Fact::Interval(lo, hi))) => Some((*lo, *hi)),
                (_, Some(Fact::IndexOf(arr))) => self.fixed_len.get(arr).map(|n| (0, n - 1)),
                _ => None,
            },
            TK::Bin(op, a, b) => {
                let (a, b) = (self.interval(a)?, self.interval(b)?);
                let combos = |f: fn(i64, i64) -> Option<i64>| -> Option<(i64, i64)> {
                    let vs = [f(a.0, b.0)?, f(a.0, b.1)?, f(a.1, b.0)?, f(a.1, b.1)?];
                    Some((*vs.iter().min()?, *vs.iter().max()?))
                };
                match op {
                    BinOp::Add => combos(i64::checked_add),
                    BinOp::Sub => combos(i64::checked_sub),
                    BinOp::Mul => combos(i64::checked_mul),
                    _ => None,
                }
            }
            TK::M(M::IntSqrt, None, args, _) => self.interval(&args[0]).filter(|(lo, _)| *lo >= 0).map(|(lo, hi)| (isqrt(lo), isqrt(hi))),
            TK::M(M::Size, Some(r), _, _) => match r.kind {
                _ if matches!(r.ty, Ty::Fixed(..)) => {
                    let Ty::Fixed(_, n) = r.ty else { unreachable!() };
                    Some((n as i64, n as i64))
                }
                TK::Local(l) if matches!(r.ty, Ty::Array(_)) && self.f.locals[l].reassigned == 0 => self.fixed_len.get(&l).map(|n| (*n, *n)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Bounds check needed? `None` = proven in range (release only).
    fn index_check(&self, arr: &TExpr, idx: &TExpr) -> Option<String> {
        let loc = Some(self.loc(idx.span));
        if !self.opts.release {
            return loc;
        }
        let TK::Local(a) = arr.kind else { return loc };
        if let TK::Local(i) = idx.kind {
            if matches!(self.facts.get(&i), Some(Fact::IndexOf(x)) if *x == a) && self.f.locals[i].reassigned == 0 && !self.f.locals[a].pushed {
                return None;
            }
        }
        match (self.fixed_len.get(&a), self.interval(idx)) {
            (Some(n), Some((lo, hi))) if lo >= 0 && hi < *n => None,
            _ => loc,
        }
    }

    // ---------- statements ----------

    fn body_with_return(&mut self, body: &[TStmt], returns_value: bool) -> Vec<LS> {
        self.sub(|lw| {
            lw.defers.push(vec![]);
            for (i, s) in body.iter().enumerate() {
                if returns_value && i == body.len() - 1 {
                    if let TStmt::Expr(e) = s {
                        let mut v = lw.expr(e);
                        if lw.has_defers(0) {
                            let t = lw.lty(&e.ty);
                            v = lw.bind(v, t);
                            lw.emit_defers(0);
                        }
                        lw.emit(LS::Return(Some(v)));
                        continue;
                    }
                }
                lw.stmt(s);
            }
            lw.emit_defers(0);
            lw.defers.pop();
        })
    }

    fn stmts(&mut self, ss: &[TStmt]) {
        self.defers.push(vec![]);
        for s in ss {
            self.stmt(s);
        }
        let depth = self.defers.len() - 1;
        self.emit_defers(depth);
        self.defers.pop();
    }

    fn has_defers(&self, from: usize) -> bool {
        self.defers[from.min(self.defers.len())..].iter().any(|d| !d.is_empty())
    }

    /// Run the deferred code of blocks `from..` (innermost first), as when
    /// leaving them. Errors inside deferred code clean up only outer blocks.
    fn emit_defers(&mut self, from: usize) {
        if !self.has_defers(from) {
            return;
        }
        let saved = self.defers.clone();
        for depth in (from..saved.len()).rev() {
            self.defers.truncate(depth);
            for e in saved[depth].iter().rev() {
                self.stmt(&TStmt::Expr(e.clone()));
            }
        }
        self.defers = saved;
    }

    /// The error path here: the function's, plus the deferred code of every open block.
    fn err_path(&mut self) -> ErrPath {
        let cleanup = if self.has_defers(0) { self.sub(|lw| lw.emit_defers(0)) } else { vec![] };
        match self.path {
            ErrPath::Return(_) => ErrPath::Return(cleanup),
            ErrPath::Die(_) => ErrPath::Die(cleanup),
        }
    }

    /// Lower statements whose last expression is the value, in their own
    /// defer scope (the value is computed before the deferred code runs).
    fn scoped_value(&mut self, ss: &[TStmt], ty: &Ty) -> LE {
        self.defers.push(vec![]);
        let mut val = LE::Unit;
        for (i, s) in ss.iter().enumerate() {
            if i + 1 == ss.len() {
                if let TStmt::Expr(e) = s {
                    val = self.expr(e);
                    continue;
                }
            }
            self.stmt(s);
        }
        let depth = self.defers.len() - 1;
        if self.has_defers(depth) {
            let t = self.lty(ty);
            val = self.bind(val, t);
            self.emit_defers(depth);
        }
        self.defers.pop();
        val
    }

    fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Expr(e) => {
                let v = self.expr(e);
                if !matches!(v, LE::Var(_) | LE::I(_) | LE::B(_) | LE::S(_) | LE::Unit | LE::Field(..) | LE::Cmp(..)) {
                    self.emit(LS::Eval(v));
                }
            }
            TStmt::MultiAssign(ls, es) => {
                let vals: Vec<LE> = es.iter().map(|e| {
                    let v = self.expr(e);
                    let t = self.lty(&e.ty);
                    let t2 = self.tmp(t);
                    self.emit(LS::Set(t2, v));
                    LE::Var(t2)
                }).collect();
                for (l, v) in ls.iter().zip(vals) {
                    let var = self.var_of(*l);
                    self.emit(LS::Set(var, v));
                }
            }
            TStmt::While(c, body) => {
                let l = self.label();
                let inner = self.sub(|lw| {
                    let cv = lw.expr(c);
                    lw.emit(LS::If(LE::Not(Box::new(cv)), vec![LS::Break(l)], vec![]));
                    lw.next_target.push((Some(l), lw.defers.len()));
                    lw.break_target.push((l, lw.defers.len()));
                    lw.stmts(body);
                    lw.break_target.pop();
                    lw.next_target.pop();
                });
                self.emit(LS::Loop(l, inner));
            }
            TStmt::If(c, a, b) => {
                let cv = self.expr(c);
                let a = self.sub(|lw| lw.stmts(a));
                let b = self.sub(|lw| lw.stmts(b));
                self.emit(LS::If(cv, a, b));
            }
            TStmt::Next(_) => match self.next_target.last() {
                Some((Some(l), depth)) => {
                    let (l, depth) = (*l, *depth);
                    self.emit_defers(depth);
                    self.emit(LS::Continue(l));
                }
                _ => unreachable!("`next` outside a loop survived checking"),
            },
            TStmt::Break(_, _) => {
                let (l, depth) = *self.break_target.last().expect("break target");
                self.emit_defers(depth);
                self.emit(LS::Break(l));
            }
            TStmt::Return(v, _) => {
                let mut v = v.as_ref().map(|v| (self.expr(v), self.lty(&v.ty)));
                if self.has_defers(0) {
                    v = v.map(|(x, t)| (self.bind(x, t.clone()), t));
                    self.emit_defers(0);
                }
                self.emit(LS::Return(v.map(|(x, _)| x)));
            }
            TStmt::Defer(e) => {
                if self.defers.is_empty() {
                    self.defers.push(vec![]);
                }
                self.defers.last_mut().unwrap().push(e.clone());
            }
        }
    }

    // ---------- expressions ----------

    fn expr(&mut self, e: &TExpr) -> LE {
        match &e.kind {
            TK::Int(v) => {
                if e.ty == Ty::Int {
                    self.int_out(LE::I(*v))
                } else {
                    LE::I(*v)
                }
            }
            TK::Const(_) => unreachable!("constants are typed by the checker"),
            TK::Float(v) => LE::F(*v),
            TK::PlaceAssign(l, steps, op, v) => self.place_assign(*l, steps, *op, v, e),
            TK::Format(pieces, args) => self.format(pieces, args),
            TK::Seq(ss) => self.scoped_value(ss, &e.ty),
            TK::None => {
                let t = self.lty(&e.ty);
                let LTy::Tup(ts) = &t else { unreachable!() };
                let z = zero_le(&ts[1]);
                LE::Tup(t, vec![LE::B(false), z])
            }
            TK::Some(x) => {
                let t = self.lty(&e.ty);
                let v = self.arg(x);
                LE::Tup(t, vec![LE::B(true), v])
            }
            TK::Str(s) => LE::S(s.clone()),
            TK::Bool(b) => LE::B(*b),
            TK::Unit => LE::Unit,
            TK::Local(l) => LE::Var(self.var_of(*l)),
            TK::Assign(l, v) => {
                let x = self.arg(v);
                let var = self.var_of(*l);
                self.emit(LS::Set(var, x));
                LE::Var(var)
            }
            TK::IndexAssign(l, i, v) => {
                let arr_t = TExpr { kind: TK::Local(*l), ty: self.f.locals[*l].ty.clone(), span: e.span };
                let check = self.index_check(&arr_t, i);
                let iv = self.expr(i);
                let iv = self.int_in_t(iv, &i.ty, i.span);
                let vv = self.expr(v);
                let vt = self.lty(&v.ty);
                let vv = self.bind(vv, vt);
                let var = self.var_of(*l);
                self.emit(LS::SetIndex { arr: var, idx: iv, val: vv.clone(), check });
                vv
            }
            TK::Bin(BinOp::Add, a, b) if e.ty == Ty::Str => {
                let (x, y) = (self.expr(a), self.expr(b));
                LE::Rt(Rt::StrCat, vec![x, y])
            }
            TK::Bin(op, a, b) => self.binary(*op, a, b, e),
            TK::Neg(x) => {
                let v = self.expr(x);
                if e.ty == Ty::Float {
                    return LE::FNeg(Box::new(v));
                }
                if self.promote() {
                    LE::PArith(Op::Sub, Box::new(LE::ToP(Box::new(LE::I(0)))), Box::new(v))
                } else if self.try_arith {
                    let dst = self.tmp(LTy::I64);
                    let path = self.err_path();
                    self.emit(LS::TryArith { dst, op: Op::Sub, a: LE::I(0), b: v, loc: self.loc(e.span), path });
                    LE::Var(dst)
                } else {
                    LE::Neg(Box::new(v), self.ovf(e.span))
                }
            }
            TK::Not(x) => {
                let v = self.expr(x);
                LE::Not(Box::new(v))
            }
            TK::Ternary(c, a, b) => {
                let cv = self.expr(c);
                let (sa, va) = self.sub_val(|lw| lw.expr(a));
                let (sb, vb) = self.sub_val(|lw| lw.expr(b));
                if sa.is_empty() && sb.is_empty() {
                    return LE::Cond(Box::new(cv), Box::new(va), Box::new(vb));
                }
                let t = self.lty(&e.ty);
                let t = self.tmp(t);
                let mut sa = sa;
                sa.push(LS::Set(t, va));
                let mut sb = sb;
                sb.push(LS::Set(t, vb));
                self.emit(LS::If(cv, sa, sb));
                LE::Var(t)
            }
            TK::Range(lo, hi, excl) => {
                let a = self.expr(lo);
                let a = self.int_in(a, lo.span);
                let b = self.expr(hi);
                let b = self.int_in(b, hi.span);
                LE::Range(Box::new(a), Box::new(b), *excl)
            }
            TK::Index(a, i) if a.ty == Ty::Str => {
                let sv = self.expr(a);
                let sv = self.bind(sv, LTy::Str);
                let iv = self.expr(i);
                let iv = self.int_in_t(iv, &i.ty, i.span);
                let iv = self.bind(iv, LTy::I64);
                let n = LE::Rt(Rt::StrLen, vec![sv.clone()]);
                let bad = LE::Cond(Box::new(LE::Cmp(Op::Lt, Box::new(iv.clone()), Box::new(LE::I(0)), LTy::I64)), Box::new(LE::B(true)), Box::new(LE::Cmp(Op::Ge, Box::new(iv.clone()), Box::new(n), LTy::I64)));
                self.emit(LS::If(bad, vec![LS::Panic("index out of bounds".into(), self.loc(i.span))], vec![]));
                LE::Rt(Rt::StrByte, vec![sv, iv, LE::I(0)])
            }
            TK::Slice(a, lo, hi, excl) => {
                let is_str = a.ty == Ty::Str;
                let at = self.lty(&a.ty);
                let av = self.expr(a);
                let av = self.bind(av, at.clone());
                let n = self.tmp(LTy::I64);
                self.emit(LS::Set(n, if is_str { LE::Rt(Rt::StrLen, vec![av.clone()]) } else { LE::Len(Box::new(av.clone())) }));
                let lo_v = match lo {
                    Some(x) => {
                        let v = self.expr(x);
                        let v = self.int_in_t(v, &x.ty, x.span);
                        self.bind(v, LTy::I64)
                    }
                    None => LE::I(0),
                };
                let end = match hi {
                    Some(x) => {
                        let v = self.expr(x);
                        let v = self.int_in_t(v, &x.ty, x.span);
                        let v = if *excl { v } else { LE::Arith(Op::Add, Box::new(v), Box::new(LE::I(1)), Ovf::Unchecked) };
                        self.bind(v, LTy::I64)
                    }
                    None => LE::Var(n),
                };
                let cmp = |op, x: &LE, y: &LE| LE::Cmp(op, Box::new(x.clone()), Box::new(y.clone()), LTy::I64);
                let or = |x: LE, y: LE| LE::Cond(Box::new(x), Box::new(LE::B(true)), Box::new(y));
                let bad = or(cmp(Op::Lt, &lo_v, &LE::I(0)), or(cmp(Op::Lt, &end, &lo_v), cmp(Op::Gt, &end, &LE::Var(n))));
                self.emit(LS::If(bad, vec![LS::Panic("slice bounds out of range".into(), self.loc(e.span))], vec![]));
                let len = self.tmp(LTy::I64);
                self.emit(LS::Set(len, LE::Arith(Op::Sub, Box::new(end), Box::new(lo_v.clone()), Ovf::Unchecked)));
                if is_str {
                    // A non-literal length: always a substring, even when empty.
                    LE::Rt(Rt::StrByte, vec![av, lo_v, LE::Var(len)])
                } else {
                    LE::Slice(at, Box::new(av), Box::new(lo_v), Box::new(LE::Var(len)))
                }
            }
            TK::Index(a, i) => {
                let check = self.index_check(a, i);
                let av = self.expr(a);
                let iv = self.expr(i);
                let iv = self.int_in_t(iv, &i.ty, i.span);
                LE::Index { arr: Box::new(av), idx: Box::new(iv), check }
            }
            TK::Call(fid, args) => {
                let f = &self.p.funcs[*fid];
                let args = args.iter().map(|a| self.arg(a)).collect();
                LE::Call(f.cname.clone(), args)
            }
            TK::Try(inner) => self.try_expr(inner),
            TK::Puts(x) => {
                let v = self.expr(x);
                if matches!(x.ty, Ty::Opt(_) | Ty::Map(..)) {
                    let s = self.to_s(v, &x.ty);
                    self.emit(LS::Puts(s, LTy::Str));
                    return LE::Unit;
                }
                let t = self.lty(&x.ty);
                self.emit(LS::Puts(v, t));
                LE::Unit
            }
            TK::Array(items) => {
                let el = match self.lty(&e.ty) {
                    LTy::Arr(t) => *t,
                    _ => unreachable!(),
                };
                let vs = items.iter().map(|x| self.expr(x)).collect();
                LE::ArrLit(el, vs)
            }
            TK::M(m, recv, args, blk) => self.method(*m, e, recv.as_deref(), args, blk.as_deref()),
        }
    }

    /// A value being stored or passed: fixed arrays (and values holding
    /// them) are copied unless the expression already made a fresh one.
    /// Slices share their storage.
    fn arg(&mut self, a: &TExpr) -> LE {
        let v = self.expr(a);
        let fresh = matches!(a.kind, TK::Array(_) | TK::Call(..) | TK::M(M::ArrayNew | M::StructNew, ..) | TK::Some(_) | TK::None);
        if fresh || !a.ty.is_value_array() {
            return v;
        }
        self.copy_value(v, &a.ty)
    }

    /// A var holding `e` (a new one unless `e` already is a var).
    fn tmp_of(&mut self, e: LE, ty: LTy) -> V {
        match e {
            LE::Var(v) => v,
            e => {
                let t = self.tmp(ty);
                self.emit(LS::Set(t, e));
                t
            }
        }
    }

    /// A copy of an array with its own storage (elements copied as values).
    fn copy_arr(&mut self, v: LE, el: &Ty) -> LE {
        let lt = LTy::Arr(Box::new(self.lty(el)));
        let c = self.tmp(lt);
        self.emit(LS::Set(c, LE::Rt(Rt::ArrCopy, vec![v])));
        if el.is_value_array() {
            let i = self.tmp(LTy::I64);
            let n = self.tmp(LTy::I64);
            self.emit(LS::Set(i, LE::I(0)));
            self.emit(LS::Set(n, LE::Len(Box::new(LE::Var(c)))));
            let l = self.label();
            let body = self.sub(|lw| {
                lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Var(n)), LTy::I64), vec![LS::Break(l)], vec![]));
                let x = LE::Index { arr: Box::new(LE::Var(c)), idx: Box::new(LE::Var(i)), check: None };
                let x = lw.copy_value(x, el);
                lw.emit(LS::SetIndex { arr: c, idx: LE::Var(i), val: x, check: None });
                lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
            });
            self.emit(LS::Loop(l, body));
        }
        LE::Var(c)
    }

    fn map_fns(&mut self, t: &Ty) -> (crate::mapgen::MapFns, LTy, LTy) {
        let Ty::Map(k, v) = t else { unreachable!("not a map: {t:?}") };
        let (k, v) = (self.lty(k), self.lty(v));
        let fns = crate::mapgen::instantiate(&mut self.prog.borrow_mut(), &k, &v);
        (fns, k, v)
    }

    /// Field `f` of map `m`'s header.
    fn map_hdr(m: &LE, f: usize) -> LE {
        LE::Field(Box::new(LE::Index { arr: Box::new(m.clone()), idx: Box::new(LE::I(0)), check: None }), f)
    }

    /// Run `k` on each live entry (key, value) in order. Entries added while
    /// iterating are visited too; deleted ones are skipped.
    fn map_each(&mut self, m: &LE, kt: &Ty, vt: &Ty, mut k: impl FnMut(&mut Self, LE, LE)) {
        use crate::mapgen::{KEYS, LIVE, VALS};
        let (klt, vlt) = (self.lty(kt), self.lty(vt));
        let i = self.tmp(LTy::I64);
        self.emit(LS::Set(i, LE::I(0)));
        let l = self.label();
        let m = m.clone();
        let body = self.sub(|lw| {
            let n = LE::Len(Box::new(Self::map_hdr(&m, KEYS)));
            lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(n), LTy::I64), vec![LS::Break(l)], vec![]));
            let at = |f| LE::Index { arr: Box::new(Self::map_hdr(&m, f)), idx: Box::new(LE::Var(i)), check: None };
            let (kv, vv) = (lw.tmp(klt.clone()), lw.tmp(vlt.clone()));
            let live = at(LIVE);
            let inner = lw.sub(|lw| {
                lw.emit(LS::Set(kv, at(KEYS)));
                lw.emit(LS::Set(vv, at(VALS)));
                k(lw, LE::Var(kv), LE::Var(vv));
            });
            lw.emit(LS::If(live, inner, vec![]));
            lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
        });
        self.emit(LS::Loop(l, body));
    }

    fn map_op(&mut self, m: M, e: &TExpr, recv: Option<&TExpr>, args: &[TExpr]) -> LE {
        use crate::mapgen::{COUNT, VALS};
        let mty = match m {
            M::MapNew => &e.ty,
            _ => &recv.unwrap().ty,
        };
        let (fns, klt, vlt) = self.map_fns(mty);
        let Ty::Map(kt, vt) = mty else { unreachable!() };
        let mlt = self.lty(mty);
        let val_at = |m: &LE, ix: &LE| LE::Index { arr: Box::new(Self::map_hdr(m, VALS)), idx: Box::new(ix.clone()), check: None };
        match m {
            M::MapNew => {
                let mv = self.tmp(mlt);
                let cap = match recv {
                    Some(_) => LE::I(0),
                    None => LE::I(args.len() as i64 / 2),
                };
                self.emit(LS::Set(mv, LE::Call(fns.new.clone(), vec![cap])));
                if let Some(src) = recv {
                    let sv = self.expr(src);
                    let sv = self.bind(sv, self.lty(&src.ty));
                    let set = fns.set.clone();
                    self.map_each(&sv, kt, vt, |lw, k, v| {
                        let v = lw.copy_value(v, vt);
                        lw.emit(LS::Eval(LE::Call(set.clone(), vec![LE::Var(mv), k, v])));
                    });
                }
                for pair in args.chunks(2) {
                    let k = self.expr(&pair[0]);
                    let v = self.arg(&pair[1]);
                    self.emit(LS::Eval(LE::Call(fns.set.clone(), vec![LE::Var(mv), k, v])));
                }
                LE::Var(mv)
            }
            M::MapSet => {
                let mv = self.expr(recv.unwrap());
                let k = self.expr(&args[0]);
                let v = self.arg(&args[1]);
                self.emit(LS::Eval(LE::Call(fns.set.clone(), vec![mv, k, v])));
                LE::Unit
            }
            M::MapGet | M::MapDel | M::MapGetOr => {
                let mv = self.expr(recv.unwrap());
                let mv = self.bind(mv, mlt);
                let k = self.expr(&args[0]);
                let f = if m == M::MapDel { fns.del.clone() } else { fns.find.clone() };
                let ix = self.tmp(LTy::I64);
                self.emit(LS::Set(ix, LE::Call(f, vec![mv.clone(), k])));
                let found = LE::Cmp(Op::Ge, Box::new(LE::Var(ix)), Box::new(LE::I(0)), LTy::I64);
                let r = self.tmp(vlt.clone());
                let dflt = if m == M::MapGetOr { self.expr(&args[1]) } else { zero_le(&vlt) };
                let got = val_at(&mv, &LE::Var(ix));
                let got = if m == M::MapDel { got } else { self.copy_value(got, vt) };
                let _ = klt;
                self.emit(LS::Set(r, LE::Cond(Box::new(found.clone()), Box::new(got), Box::new(dflt))));
                if m == M::MapGetOr {
                    LE::Var(r)
                } else {
                    LE::Tup(self.lty(&e.ty), vec![found, LE::Var(r)])
                }
            }
            M::MapHas => {
                let mv = self.expr(recv.unwrap());
                let k = self.expr(&args[0]);
                LE::Cmp(Op::Ge, Box::new(LE::Call(fns.find.clone(), vec![mv, k])), Box::new(LE::I(0)), LTy::I64)
            }
            M::MapSize => {
                let mv = self.expr(recv.unwrap());
                self.int_out(Self::map_hdr(&mv, COUNT))
            }
            M::MapKeys | M::MapValues => {
                let mv = self.expr(recv.unwrap());
                let mv = self.bind(mv, mlt);
                let keys = m == M::MapKeys;
                let el = if keys { klt } else { vlt };
                let out = self.tmp(LTy::Arr(Box::new(el.clone())));
                self.emit(LS::Set(out, LE::ArrWithCap(el, Box::new(Self::map_hdr(&mv, COUNT)))));
                self.map_each(&mv, kt, vt, |lw, k, v| {
                    let x = if keys { k } else { lw.copy_value(v, vt) };
                    lw.emit(LS::Push(out, x));
                });
                LE::Var(out)
            }
            _ => unreachable!(),
        }
    }

    /// The code point of the `cl`-byte UTF-8 sequence at `s[i]`.
    fn decode_rune(&mut self, s: &LE, i: V, cl: V) -> LE {
        let byte = |k: i64| {
            let at = if k == 0 { LE::Var(i) } else { LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(k)), Ovf::Unchecked) };
            LE::Rt(Rt::StrByte, vec![s.clone(), at, LE::I(0)])
        };
        let prim = |p: Prim, a: LE, b: LE| LE::Prim(p, vec![a, b]);
        let low6 = |k: i64, sh: i64| prim(Prim::Shl, prim(Prim::And, byte(k), LE::I(0x3f)), LE::I(sh));
        let r = self.tmp(LTy::I64);
        let b0 = self.tmp(LTy::I64);
        self.emit(LS::Set(b0, byte(0)));
        let is = |n: i64| LE::Cmp(Op::Eq, Box::new(LE::Var(cl)), Box::new(LE::I(n)), LTy::I64);
        let one = LE::Cond(Box::new(LE::Cmp(Op::Lt, Box::new(LE::Var(b0)), Box::new(LE::I(0x80)), LTy::I64)), Box::new(LE::Var(b0)), Box::new(LE::I(0xfffd)));
        let two = prim(Prim::Or, prim(Prim::Shl, prim(Prim::And, LE::Var(b0), LE::I(0x1f)), LE::I(6)), low6(1, 0));
        let three = prim(Prim::Or, prim(Prim::Or, prim(Prim::Shl, prim(Prim::And, LE::Var(b0), LE::I(0x0f)), LE::I(12)), low6(1, 6)), low6(2, 0));
        let four = prim(Prim::Or, prim(Prim::Or, prim(Prim::Or, prim(Prim::Shl, prim(Prim::And, LE::Var(b0), LE::I(0x07)), LE::I(18)), low6(1, 12)), low6(2, 6)), low6(3, 0));
        self.emit(LS::Set(r, LE::Cond(Box::new(is(1)), Box::new(one), Box::new(LE::Cond(Box::new(is(2)), Box::new(two), Box::new(LE::Cond(Box::new(is(3)), Box::new(three), Box::new(four))))))));
        LE::Var(r)
    }

    fn copy_value(&mut self, v: LE, ty: &Ty) -> LE {
        if !ty.is_value_array() {
            return v;
        }
        match ty {
            Ty::Fixed(el, n) => {
                let lt = self.lty(ty);
                let c = self.tmp(lt);
                self.emit(LS::Set(c, LE::Rt(Rt::ArrCopy, vec![v])));
                if el.is_value_array() {
                    let i = self.tmp(LTy::I64);
                    self.emit(LS::Set(i, LE::I(0)));
                    let l = self.label();
                    let n = *n as i64;
                    let body = self.sub(|lw| {
                        lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::I(n)), LTy::I64), vec![LS::Break(l)], vec![]));
                        let x = LE::Index { arr: Box::new(LE::Var(c)), idx: Box::new(LE::Var(i)), check: None };
                        let x = lw.copy_value(x, el);
                        lw.emit(LS::SetIndex { arr: c, idx: LE::Var(i), val: x, check: None });
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    });
                    self.emit(LS::Loop(l, body));
                }
                LE::Var(c)
            }
            Ty::Struct(_, fs) => {
                let lt = self.lty(ty);
                let t = self.bind(v, lt.clone());
                let vals = fs.iter().enumerate().map(|(k, (_, ft))| self.copy_value(LE::Field(Box::new(t.clone()), k), ft)).collect();
                LE::Tup(lt, vals)
            }
            Ty::Tuple(ts) => {
                let lt = self.lty(ty);
                let t = self.bind(v, lt.clone());
                let vals = ts.iter().enumerate().map(|(k, ft)| self.copy_value(LE::Field(Box::new(t.clone()), k), ft)).collect();
                LE::Tup(lt, vals)
            }
            Ty::Opt(x) => {
                let lt = self.lty(ty);
                let c = self.tmp(lt.clone());
                self.emit(LS::Set(c, v));
                let present = LE::Field(Box::new(LE::Var(c)), 0);
                let body = self.sub(|lw| {
                    let p = lw.copy_value(LE::Field(Box::new(LE::Var(c)), 1), x);
                    lw.emit(LS::Set(c, LE::Tup(lt.clone(), vec![LE::B(true), p])));
                });
                self.emit(LS::If(present, body, vec![]));
                LE::Var(c)
            }
            _ => v,
        }
    }

    fn try_expr(&mut self, inner: &TExpr) -> LE {
        let path = self.err_path();
        match &inner.kind {
            TK::Call(fid, args) => {
                let f = &self.p.funcs[*fid];
                let name = f.cname.clone();
                let ret = self.lty(&f.ret);
                let args = args.iter().map(|a| self.arg(a)).collect();
                let dst = if ret == LTy::Unit { None } else { Some(self.tmp(ret)) };
                self.emit(LS::TryCall { dst, f: name, args, path });
                dst.map_or(LE::Unit, LE::Var)
            }
            TK::M(M::FileRead, None, args, _) => {
                let p = self.expr(&args[0]);
                let dst = self.tmp(LTy::Str);
                self.emit(LS::TryRead { dst, path_arg: p, loc: self.loc(inner.span), path });
                LE::Var(dst)
            }
            _ => {
                let saved = std::mem::replace(&mut self.try_arith, true);
                let v = self.expr(inner);
                self.try_arith = saved;
                v
            }
        }
    }

    fn binary(&mut self, op: BinOp, a: &TExpr, b: &TExpr, e: &TExpr) -> LE {
        if matches!(op, BinOp::And | BinOp::Or) {
            let lop = if op == BinOp::And { Op::And } else { Op::Or };
            let av = self.expr(a);
            let (sb, bv) = self.sub_val(|lw| lw.expr(b));
            if sb.is_empty() {
                return LE::Cmp(lop, Box::new(av), Box::new(bv), LTy::Bool);
            }
            let t = self.tmp(LTy::Bool);
            self.emit(LS::Set(t, av));
            let mut sb = sb;
            sb.push(LS::Set(t, bv));
            let cond = if op == BinOp::And { LE::Var(t) } else { LE::Not(Box::new(LE::Var(t))) };
            self.emit(LS::If(cond, sb, vec![]));
            return LE::Var(t);
        }
        let av = self.expr(a);
        let bv = self.expr(b);
        let at = self.lty(&a.ty);
        let lop = op_of(op);
        if at == LTy::F64 {
            return if op.is_arith() { LE::FArith(lop, Box::new(av), Box::new(bv)) } else { LE::Cmp(lop, Box::new(av), Box::new(bv), at) };
        }
        if !matches!(at, LTy::I64 | LTy::IntK(_) | LTy::PInt) {
            // Str and Bool comparisons.
            return LE::Cmp(lop, Box::new(av), Box::new(bv), at);
        }
        let k = a.ty.int_kind().unwrap_or(IntKind::I64);
        if at == LTy::PInt {
            if op.is_arith() || is_cmp(op) {
                return LE::PArith(lop, Box::new(av), Box::new(bv));
            }
            // Bit operations on a promoted Int work on its 64-bit value.
            let (x, y) = (self.int_in(av, a.span), self.int_in_t(bv, &b.ty, b.span));
            let bk = b.ty.int_kind().unwrap_or(IntKind::I64);
            let r = self.int_op(op, IntKind::I64, bk, x, y, e.span, false);
            return self.int_out(r);
        }
        if is_cmp(op) {
            return self.cmp(lop, av, bv, &at);
        }
        let bk = b.ty.int_kind().unwrap_or(IntKind::I64);
        let bv = if b.ty == Ty::Int { self.int_in(bv, b.span) } else { bv };
        // Release: an I64 operation whose result interval is known can't overflow.
        let proven = k == IntKind::I64 && self.opts.release && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) && self.interval(e).is_some();
        self.int_op(op, k, bk, av, bv, e.span, proven)
    }

    /// A comparison of two values of type `t` (unsigned for U64).
    fn cmp(&mut self, op: Op, a: LE, b: LE, t: &LTy) -> LE {
        match t {
            LTy::IntK(IntKind::U64) if matches!(op, Op::Lt | Op::Le | Op::Gt | Op::Ge) => match op {
                Op::Lt => LE::Prim(Prim::ULt, vec![a, b]),
                Op::Le => LE::Prim(Prim::ULe, vec![a, b]),
                Op::Gt => LE::Prim(Prim::ULt, vec![b, a]),
                _ => LE::Prim(Prim::ULe, vec![b, a]),
            },
            LTy::IntK(_) => LE::Cmp(op, Box::new(a), Box::new(b), LTy::I64),
            LTy::PInt => LE::PArith(op, Box::new(a), Box::new(b)),
            _ => LE::Cmp(op, Box::new(a), Box::new(b), t.clone()),
        }
    }

    /// Go's integer operations on kind `k` (registers hold i64s). `bk` is
    /// the right operand's kind (it differs only for shift counts).
    #[allow(clippy::too_many_arguments)]
    fn int_op(&mut self, op: BinOp, k: IntKind, bk: IntKind, a: LE, b: LE, sp: Span, proven: bool) -> LE {
        let lop = op_of(op);
        let wrap_mode = self.mode == Overflow::Wrap;
        match op {
            BinOp::BitAnd => LE::Prim(Prim::And, vec![a, b]),
            BinOp::BitOr => LE::Prim(Prim::Or, vec![a, b]),
            BinOp::BitXor => LE::Prim(Prim::Xor, vec![a, b]),
            BinOp::AndNot => LE::Prim(Prim::AndNot, vec![a, b]),
            BinOp::Shl | BinOp::Shr => self.shift(op, k, bk, a, b, sp),
            BinOp::AddW | BinOp::SubW | BinOp::MulW => {
                let lop = match op {
                    BinOp::AddW => Op::Add,
                    BinOp::SubW => Op::Sub,
                    _ => Op::Mul,
                };
                wrap_to(k, LE::Arith(lop, Box::new(a), Box::new(b), Ovf::Wrap))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul if wrap_mode => wrap_to(k, LE::Arith(lop, Box::new(a), Box::new(b), Ovf::Wrap)),
            _ if k == IntKind::I64 => {
                if self.try_arith {
                    let dst = self.tmp(LTy::I64);
                    let path = self.err_path();
                    self.emit(LS::TryArith { dst, op: lop, a, b, loc: self.loc(sp), path });
                    return LE::Var(dst);
                }
                if proven {
                    return LE::Arith(lop, Box::new(a), Box::new(b), Ovf::Unchecked);
                }
                LE::Arith(lop, Box::new(a), Box::new(b), self.ovf(sp))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let a = self.bind(a, LTy::I64);
                let b = self.bind(b, LTy::I64);
                if k == IntKind::U64 {
                    let r = self.tmp(LTy::I64);
                    self.emit(LS::Set(r, LE::Arith(lop, Box::new(a.clone()), Box::new(b.clone()), Ovf::Wrap)));
                    let ovf = match op {
                        BinOp::Add => LE::Prim(Prim::ULt, vec![LE::Var(r), a]),
                        BinOp::Sub => LE::Prim(Prim::ULt, vec![a, b]),
                        _ => LE::Cmp(Op::Ne, Box::new(LE::Prim(Prim::UMulHi, vec![a, b])), Box::new(LE::I(0)), LTy::I64),
                    };
                    self.overflow_if(ovf, k, sp);
                    return LE::Var(r);
                }
                // Narrow: the 64-bit result is exact; check it fits.
                let r = self.tmp(LTy::I64);
                self.emit(LS::Set(r, LE::Arith(lop, Box::new(a), Box::new(b), Ovf::Wrap)));
                let out = out_of_range(k, LE::Var(r));
                self.overflow_if(out, k, sp);
                LE::Var(r)
            }
            BinOp::Div | BinOp::Rem => {
                let a = self.bind(a, LTy::I64);
                let b = self.bind(b, LTy::I64);
                let zero = LE::Cmp(Op::Eq, Box::new(b.clone()), Box::new(LE::I(0)), LTy::I64);
                if self.try_arith {
                    let path = self.err_path();
                    self.emit(LS::FailIf { cond: zero, loc: self.loc(sp), path });
                } else {
                    self.emit(LS::If(zero, vec![LS::Panic("division by zero".into(), self.loc(sp))], vec![]));
                }
                if k == IntKind::U64 {
                    return LE::Prim(if op == BinOp::Div { Prim::UDiv } else { Prim::URem }, vec![a, b]);
                }
                let r = self.tmp(LTy::I64);
                self.emit(LS::Set(r, LE::Arith(lop, Box::new(a), Box::new(b), Ovf::Unchecked)));
                if op == BinOp::Div && k.signed() {
                    // MIN / -1 is the one quotient that doesn't fit.
                    if wrap_mode {
                        return wrap_to(k, LE::Var(r));
                    }
                    let out = out_of_range(k, LE::Var(r));
                    self.overflow_if(out, k, sp);
                }
                LE::Var(r)
            }
            _ => unreachable!("`{}` on {}", op.text(), k.name()),
        }
    }

    /// Go's shifts: no overflow; a count of the width or more shifts
    /// everything out; a negative count panics.
    #[allow(clippy::too_many_arguments)]
    fn shift(&mut self, op: BinOp, k: IntKind, ck: IntKind, a: LE, c: LE, sp: Span) -> LE {
        let a = self.bind(a, LTy::I64);
        let c = self.bind(c, LTy::I64);
        if ck.signed() {
            let neg = LE::Cmp(Op::Lt, Box::new(c.clone()), Box::new(LE::I(0)), LTy::I64);
            self.emit(LS::If(neg, vec![LS::Panic("negative shift amount".into(), self.loc(sp))], vec![]));
        }
        let w = k.bits() as i64;
        let big = LE::Prim(Prim::ULe, vec![LE::I(w), c.clone()]);
        if op == BinOp::Shl {
            return LE::Cond(Box::new(big), Box::new(LE::I(0)), Box::new(wrap_to(k, LE::Prim(Prim::Shl, vec![a, c]))));
        }
        if k == IntKind::U64 {
            return LE::Cond(Box::new(big), Box::new(LE::I(0)), Box::new(LE::Prim(Prim::ShrU, vec![a, c])));
        }
        // Registers hold sign- or zero-extended values: an arithmetic shift
        // clamped to 63 is right for every other kind.
        let clamped = LE::Cond(Box::new(LE::Prim(Prim::ULe, vec![LE::I(63), c.clone()])), Box::new(LE::I(63)), Box::new(c));
        LE::Prim(Prim::ShrS, vec![a, clamped])
    }

    fn overflow_if(&mut self, cond: LE, k: IntKind, sp: Span) {
        let loc = self.loc(sp);
        if self.try_arith {
            let path = self.err_path();
            self.emit(LS::FailIf { cond, loc, path });
        } else {
            self.emit(LS::If(cond, vec![LS::Panic(format!("overflow ({})", k.name()), loc)], vec![]));
        }
    }

    /// An Int of this function's mode, needed as a machine i64 (other
    /// integer kinds are already machine values).
    fn int_in_t(&self, e: LE, t: &Ty, sp: Span) -> LE {
        if *t == Ty::Int { self.int_in(e, sp) } else { e }
    }

    /// `x.to_u8` (checked) / `x.as_u8` (wrapping) from an integer or a Float.
    fn convert(&mut self, k: IntKind, wrap: bool, x: &TExpr, sp: Span) -> LE {
        let v = self.expr(x);
        let loc = self.loc(sp);
        let fail = |k: IntKind| LS::Panic(format!("conversion overflow: the value doesn't fit {}", k.name()), loc.clone());
        if x.ty == Ty::Float {
            if k == IntKind::U64 {
                return LE::Rt(Rt::FToU64, vec![v, LE::Loc(loc.clone())]);
            }
            let i = self.tmp(LTy::I64);
            self.emit(LS::Set(i, LE::Rt(Rt::FToI, vec![v, LE::Loc(loc.clone())])));
            if k != IntKind::I64 {
                let out = out_of_range(k, LE::Var(i));
                self.emit(LS::If(out, vec![fail(k)], vec![]));
            }
            let r = LE::Var(i);
            return if k == IntKind::I64 { self.int_out(r) } else { r };
        }
        let sk = x.ty.int_kind().unwrap_or(IntKind::I64);
        let v = self.int_in_t(v, &x.ty, x.span);
        let v = self.bind(v, LTy::I64);
        let r = if wrap {
            wrap_to(k, v)
        } else {
            let out = if sk == IntKind::U64 {
                (k != IntKind::U64).then(|| LE::Prim(Prim::ULt, vec![LE::I(k.max() as i64), v.clone()]))
            } else if k == IntKind::U64 {
                Some(LE::Cmp(Op::Lt, Box::new(v.clone()), Box::new(LE::I(0)), LTy::I64))
            } else if k == IntKind::I64 || (k.min() <= sk.min() && k.max() >= sk.max()) {
                None
            } else {
                Some(out_of_range(k, v.clone()))
            };
            if let Some(out) = out {
                self.emit(LS::If(out, vec![fail(k)], vec![]));
            }
            v
        };
        if k == IntKind::I64 { self.int_out(r) } else { r }
    }

    /// `place = v` / `place op= v`: index expressions are evaluated once.
    fn place_assign(&mut self, l: LocalId, steps: &[TStep], op: Option<crate::ast::BinOp>, v: &TExpr, e: &TExpr) -> LE {
        let var = self.var_of(l);
        let mut lsteps = vec![];
        for (k, st) in steps.iter().enumerate() {
            match st {
                TStep::Index(i) => {
                    let check = if k == 0 {
                        let arr_t = TExpr { kind: TK::Local(l), ty: self.f.locals[l].ty.clone(), span: e.span };
                        self.index_check(&arr_t, i)
                    } else {
                        Some(self.loc(i.span))
                    };
                    let iv = self.expr(i);
                    let iv = self.int_in_t(iv, &i.ty, i.span);
                    let iv = self.bind(iv, LTy::I64);
                    lsteps.push(Step::Index(iv, check));
                }
                TStep::Field(f) => lsteps.push(Step::Field(*f)),
            }
        }
        let pty = self.lty(&e.ty);
        let rhs = self.expr(v);
        let val = match op {
            None => rhs,
            Some(op) => {
                let mut cur = LE::Var(var);
                for st in &lsteps {
                    cur = match st {
                        Step::Index(i, check) => LE::Index { arr: Box::new(cur), idx: Box::new(i.clone()), check: check.clone() },
                        Step::Field(f) => LE::Field(Box::new(cur), *f),
                    };
                }
                self.arith_le(op, cur, rhs, &pty, e.span)
            }
        };
        let val = self.bind(val, pty);
        if lsteps.is_empty() {
            self.emit(LS::Set(var, val.clone()));
        } else {
            self.emit(LS::SetPlace { var, steps: lsteps, val: val.clone() });
        }
        val
    }

    /// `a op b` on already-lowered operands of type `t`.
    fn arith_le(&mut self, op: crate::ast::BinOp, a: LE, b: LE, t: &LTy, sp: Span) -> LE {
        use crate::ast::BinOp as B;
        let lop = op_of(op);
        let _ = B::Add;
        match t {
            LTy::F64 => LE::FArith(lop, Box::new(a), Box::new(b)),
            LTy::PInt if op.is_arith() => LE::PArith(lop, Box::new(a), Box::new(b)),
            LTy::PInt => {
                let (x, y) = (self.int_in(a, sp), self.int_in(b, sp));
                let r = self.int_op(op, IntKind::I64, IntKind::I64, x, y, sp, false);
                self.int_out(r)
            }
            LTy::IntK(k) => self.int_op(op, *k, *k, a, b, sp, false),
            _ => self.int_op(op, IntKind::I64, IntKind::I64, a, b, sp, false),
        }
    }

    /// `format(...)`: a concatenation of literal pieces and formatted arguments.
    fn format(&mut self, pieces: &[FmtPiece], args: &[TExpr]) -> LE {
        let vals: Vec<LE> = args
            .iter()
            .map(|a| {
                let v = self.expr(a);
                let t = self.lty(&a.ty);
                self.bind(v, t)
            })
            .collect();
        let mut parts = vec![];
        for p in pieces {
            parts.push(match p {
                FmtPiece::Lit(s) => LE::S(s.clone()),
                FmtPiece::Int(k) => self.to_s(vals[*k].clone(), &args[*k].ty),
                FmtPiece::Str(k) => self.to_s(vals[*k].clone(), &args[*k].ty),
                FmtPiece::Fixed(k, d) => LE::Rt(Rt::FFmt, vec![vals[*k].clone(), LE::I(*d as i64)]),
                FmtPiece::Base(k, base, upper) => {
                    let t = &args[*k].ty;
                    let v = self.int_in_t(vals[*k].clone(), t, args[*k].span);
                    LE::Rt(Rt::IntFmt, vec![v, LE::I(*base as i64), LE::B(*upper), LE::B(*t == Ty::IntK(IntKind::U64))])
                }
                FmtPiece::Char(k) => {
                    let v = self.int_in_t(vals[*k].clone(), &args[*k].ty, args[*k].span);
                    LE::Rt(Rt::RuneToS, vec![v])
                }
            });
        }
        match parts.len() {
            0 => LE::S(String::new()),
            1 if matches!(parts[0], LE::S(_)) => parts.pop().unwrap(),
            _ => LE::Rt(Rt::StrCat, parts),
        }
    }

    fn to_s(&mut self, v: LE, t: &Ty) -> LE {
        match t {
            Ty::Opt(inner) => {
                let lt = self.lty(t);
                let v = self.bind(v, lt);
                let s = self.to_s(LE::Field(Box::new(v.clone()), 1), inner);
                LE::Cond(Box::new(LE::Field(Box::new(v), 0)), Box::new(s), Box::new(LE::S("none".into())))
            }
            Ty::Str => v,
            Ty::Map(kt, vt) => {
                // Go's `map[k:v k:v]`, in insertion order.
                let lt = self.lty(t);
                let m = self.bind(v, lt);
                let s = self.tmp(LTy::Str);
                let first = self.tmp(LTy::Bool);
                self.emit(LS::Set(s, LE::S("map[".into())));
                self.emit(LS::Set(first, LE::B(true)));
                self.map_each(&m, kt, vt, |lw, k, v| {
                    let sep = LE::Cond(Box::new(LE::Var(first)), Box::new(LE::S(String::new())), Box::new(LE::S(" ".into())));
                    let ks = lw.to_s(k, kt);
                    let vs = lw.to_s(v, vt);
                    lw.emit(LS::Set(s, LE::Rt(Rt::StrCat, vec![LE::Var(s), sep, ks, LE::S(":".into()), vs])));
                    lw.emit(LS::Set(first, LE::B(false)));
                });
                LE::Rt(Rt::StrCat, vec![LE::Var(s), LE::S("]".into())])
            }
            Ty::IntK(IntKind::U64) => LE::Rt(Rt::U64ToS, vec![v]),
            Ty::IntK(_) => LE::Rt(Rt::IntToS, vec![v]),
            Ty::Float => LE::Rt(Rt::FToS, vec![v]),
            Ty::Bool => LE::Cond(Box::new(v), Box::new(LE::S("true".into())), Box::new(LE::S("false".into()))),
            _ => LE::Rt(if self.promote() { Rt::PIntToS } else { Rt::IntToS }, vec![v]),
        }
    }

    // ---------- blocks ----------

    /// Inline a block: bind params to `args`, lower its body, return the
    /// value of its last expression.
    fn inline_block(&mut self, b: &TBlock, args: &[LE], arg_facts: &[Option<Fact>], next_label: Option<Label>) -> LE {
        let pvars: Vec<V> = b.params.iter().map(|p| self.var_of(*p)).collect();
        if b.destructure {
            let tup = args[0].clone();
            for (i, v) in pvars.iter().enumerate() {
                self.emit(LS::Set(*v, LE::Field(Box::new(tup.clone()), i)));
            }
        } else {
            for (i, v) in pvars.iter().enumerate() {
                self.emit(LS::Set(*v, args[i].clone()));
            }
        }
        for (i, p) in b.params.iter().enumerate() {
            match arg_facts.get(i).copied().flatten() {
                Some(fact) if self.f.locals[*p].reassigned == 0 => {
                    self.facts.insert(*p, fact);
                }
                _ => {
                    self.facts.remove(p);
                }
            }
        }
        self.next_target.push((next_label, self.defers.len()));
        let ty = match b.body.last() {
            Some(TStmt::Expr(e)) => e.ty.clone(),
            _ => Ty::Unit,
        };
        let val = self.scoped_value(&b.body, &ty);
        self.next_target.pop();
        for p in &b.params {
            self.facts.remove(p);
        }
        val
    }

    /// Lower all but the last statement of a block; return the last expression.
    fn block_prefix<'t>(&mut self, b: &'t TBlock, args: &[LE]) -> Option<&'t TExpr> {
        let pvars: Vec<V> = b.params.iter().map(|p| self.var_of(*p)).collect();
        if b.destructure {
            for (i, v) in pvars.iter().enumerate() {
                self.emit(LS::Set(*v, LE::Field(Box::new(args[0].clone()), i)));
            }
        } else {
            for (i, v) in pvars.iter().enumerate() {
                self.emit(LS::Set(*v, args[i].clone()));
            }
        }
        let n = b.body.len();
        for s in &b.body[..n.saturating_sub(1)] {
            self.stmt(s);
        }
        match b.body.last() {
            Some(TStmt::Expr(e)) => Some(e),
            Some(s) => {
                self.stmt(s);
                None
            }
            None => None,
        }
    }

    // ---------- methods ----------

    fn method(&mut self, m: M, e: &TExpr, recv: Option<&TExpr>, args: &[TExpr], blk: Option<&TBlock>) -> LE {
        use M::*;
        let sp = e.span;
        match m {
            Sum | Max | Min | MaxBy | MinBy | First | Find | ToA | Each | Reduce | All | Any | Count | Include | Sort => {
                self.pipeline(m, e, recv.unwrap(), args, blk)
            }
            Pmap => self.pmap(e, recv.unwrap(), blk.unwrap()),
            Size => {
                let r = recv.unwrap();
                let v = self.expr(r);
                // `n.to_s.size`: count digits; the string is never observed.
                let v = match v {
                    LE::Rt(Rt::IntToS, a) => return self.int_out(LE::Rt(Rt::NDigits, a)),
                    LE::Rt(Rt::PIntToS, a) => return self.int_out(LE::Rt(Rt::PNDigits, a)),
                    v => v,
                };
                let n = if r.ty == Ty::Str { LE::Rt(Rt::StrLen, vec![v]) } else { LE::Len(Box::new(v)) };
                self.int_out(n)
            }
            Last => {
                let r = recv.unwrap();
                let v = self.expr(r);
                let el = self.lty(&e.ty);
                let av = self.bind(v, LTy::Arr(Box::new(el)));
                let idx = LE::Arith(Op::Sub, Box::new(LE::Len(Box::new(av.clone()))), Box::new(LE::I(1)), Ovf::Unchecked);
                LE::Index { arr: Box::new(av), idx: Box::new(idx), check: Some(self.loc(sp)) }
            }
            Reverse => {
                let v = self.expr(recv.unwrap());
                LE::Rt(Rt::StrRev, vec![v])
            }
            ToS => {
                let r = recv.unwrap();
                let v = self.expr(r);
                self.to_s(v, &r.ty)
            }
            ToI => {
                let v = self.expr(recv.unwrap());
                self.int_out(LE::Rt(Rt::StrToI, vec![v]))
            }
            Delete | Split => {
                let v = self.expr(recv.unwrap());
                let a = self.expr(&args[0]);
                LE::Rt(if m == Delete { Rt::StrDelete } else { Rt::StrSplit }, vec![v, a])
            }
            Even | Odd => {
                let r = recv.unwrap();
                let v = self.expr(r);
                let ev = LE::Rt(if self.promote() && r.ty == Ty::Int { Rt::PEven } else { Rt::Even }, vec![v]);
                if m == Odd { LE::Not(Box::new(ev)) } else { ev }
            }
            Digits => {
                let v = self.expr(recv.unwrap());
                if self.promote() { LE::Rt(Rt::PDigits, vec![v, LE::Loc(self.loc(sp))]) } else { LE::Rt(Rt::Digits, vec![v, LE::Loc(self.loc(sp))]) }
            }
            IntSqrt => {
                let v = self.expr(&args[0]);
                let v = self.int_in(v, sp);
                self.int_out(LE::Rt(Rt::Isqrt, vec![v, LE::Loc(self.loc(sp))]))
            }
            MapNew | MapGet | MapGetOr | MapSet | MapDel | MapHas | MapSize | MapKeys | MapValues => self.map_op(m, e, recv, args),
            FromBytes => {
                let v = self.expr(&args[0]);
                LE::Rt(Rt::StrFromBytes, vec![v])
            }
            Dup => {
                let r = recv.unwrap();
                let v = self.expr(r);
                let el = r.ty.arr_elem().unwrap();
                self.copy_arr(v, &el)
            }
            CopyInto => {
                let lt = self.lty(&args[0].ty);
                let d = self.expr(&args[0]);
                let d = self.tmp_of(d, lt.clone());
                let s = self.expr(&args[1]);
                let s = self.bind(s, lt.clone());
                let n = self.tmp(LTy::I64);
                let (dl, sl) = (LE::Len(Box::new(LE::Var(d))), LE::Len(Box::new(s.clone())));
                self.emit(LS::Set(n, LE::Cond(Box::new(LE::Cmp(Op::Lt, Box::new(dl.clone()), Box::new(sl.clone()), LTy::I64)), Box::new(dl), Box::new(sl))));
                // Through a copy of the source, so overlapping slices work (memmove).
                let t = self.tmp(lt.clone());
                self.emit(LS::Set(t, LE::Rt(Rt::ArrCopy, vec![LE::Slice(lt, Box::new(s), Box::new(LE::I(0)), Box::new(LE::Var(n)))])));
                let el = args[0].ty.arr_elem().unwrap();
                let i = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                let l = self.label();
                let body = self.sub(|lw| {
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Var(n)), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = LE::Index { arr: Box::new(LE::Var(t)), idx: Box::new(LE::Var(i)), check: None };
                    let x = lw.copy_value(x, &el);
                    lw.emit(LS::SetIndex { arr: d, idx: LE::Var(i), val: x, check: None });
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                });
                self.emit(LS::Loop(l, body));
                self.int_out(LE::Var(n))
            }
            ArrayNew if args[1].ty.is_value_array() || matches!(args[1].ty, Ty::Array(_)) => {
                // A fill holding storage is evaluated once per element, so
                // the elements don't share it (`[[0; 3]; 3]` is 3 rows).
                let n = self.expr(&args[0]);
                let n = self.int_in(n, args[0].span);
                let n = self.bind(n, LTy::I64);
                self.emit(LS::If(LE::Cmp(Op::Lt, Box::new(n.clone()), Box::new(LE::I(0)), LTy::I64), vec![LS::Panic("negative array size".into(), self.loc(sp))], vec![]));
                let lt = self.lty(&e.ty);
                let a = self.tmp(lt);
                self.emit(LS::Set(a, LE::ArrWithCap(self.lty(&args[1].ty), Box::new(n.clone()))));
                let i = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                let l = self.label();
                let body = self.sub(|lw| {
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(n.clone()), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = lw.arg(&args[1]);
                    lw.emit(LS::Push(a, x));
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                });
                self.emit(LS::Loop(l, body));
                LE::Var(a)
            }
            ArrayNew => {
                let n = self.expr(&args[0]);
                let n = self.int_in(n, args[0].span);
                let fill = self.expr(&args[1]);
                let el = self.lty(&args[1].ty);
                LE::ArrNew(el, Box::new(n), Box::new(fill), self.loc(sp))
            }
            ToF => {
                let r = recv.unwrap();
                let v = self.expr(r);
                let v = self.int_in_t(v, &r.ty, r.span);
                if r.ty == Ty::IntK(IntKind::U64) { LE::Prim(Prim::UToF, vec![v]) } else { LE::Rt(Rt::IntToF, vec![v]) }
            }
            Conv(k, wrap) => self.convert(k, wrap, recv.unwrap(), sp),
            FloatToI => {
                let v = self.expr(recv.unwrap());
                self.int_out(LE::Rt(Rt::FToI, vec![v, LE::Loc(self.loc(sp))]))
            }
            FloatAbs => LE::Rt(Rt::FAbs, vec![self.expr(recv.unwrap())]),
            FloatToS => LE::Rt(Rt::FToS, vec![self.expr(recv.unwrap())]),
            Sqrt => LE::Rt(Rt::FSqrt, vec![self.expr(&args[0])]),
            StructNew => {
                let t = self.lty(&e.ty);
                let vs = args.iter().map(|a| self.arg(a)).collect();
                LE::Tup(t, vs)
            }
            OptPresent => LE::Field(Box::new(self.expr(recv.unwrap())), 0),
            OptGet => LE::Field(Box::new(self.expr(recv.unwrap())), 1),
            Unwrap => {
                let r = recv.unwrap();
                let v = self.expr(r);
                let t = self.lty(&r.ty);
                let v = self.bind(v, t);
                let absent = LE::Not(Box::new(LE::Field(Box::new(v.clone()), 0)));
                self.emit(LS::If(absent, vec![LS::Panic("unwrap of none".into(), self.loc(sp))], vec![]));
                LE::Field(Box::new(v), 1)
            }
            TupleGet(k) => {
                let v = self.expr(recv.unwrap());
                LE::Field(Box::new(v), k)
            }
            Push => {
                let TK::Local(l) = recv.unwrap().kind else { unreachable!() };
                let var = self.var_of(l);
                let v = self.expr(&args[0]);
                self.emit(LS::Push(var, v));
                LE::Unit
            }
            Yield => {
                let v = self.expr(&args[0]);
                self.emit(LS::Yield(v));
                LE::Unit
            }
            Step => self.step(e, recv.unwrap(), args, blk.unwrap()),
            Loop => {
                let l = self.label();
                let b = blk.unwrap();
                let inner = self.sub(|lw| {
                    lw.next_target.push((Some(l), lw.defers.len()));
                    lw.break_target.push((l, lw.defers.len()));
                    lw.stmts(&b.body);
                    lw.break_target.pop();
                    lw.next_target.pop();
                });
                self.emit(LS::Loop(l, inner));
                LE::Unit
            }
            EnumNew => self.generator(e, blk.unwrap()),
            FileRead => unreachable!("File.read is always under try"),
            _ => unreachable!("stage {m:?} used as a value; the checker materializes these"),
        }
    }

    fn step(&mut self, _e: &TExpr, recv: &TExpr, args: &[TExpr], b: &TBlock) -> LE {
        let start = self.expr(recv);
        let start = self.int_in(start, recv.span);
        let limit = self.expr(&args[0]);
        let limit = self.int_in(limit, args[0].span);
        let by = self.expr(&args[1]);
        let by = self.int_in(by, args[1].span);
        let i = self.tmp(LTy::I64);
        let lim = self.tmp(LTy::I64);
        let byv = self.tmp(LTy::I64);
        self.emit(LS::Set(i, start));
        self.emit(LS::Set(lim, limit));
        self.emit(LS::Set(byv, by));
        self.emit(LS::If(LE::Cmp(Op::Le, Box::new(LE::Var(byv)), Box::new(LE::I(0)), LTy::I64), vec![LS::Panic("`step` needs a positive step".into(), self.loc(args[1].span))], vec![]));
        let fact = match (self.interval(recv), self.interval(&args[0])) {
            (Some((lo, _)), Some((_, hi))) => Some(Fact::Interval(lo, hi)),
            _ => None,
        };
        // `i + by` can't overflow when both are bounded: plain addition.
        let bounded = matches!((self.interval(&args[0]), self.interval(&args[1])), (Some((_, a)), Some((_, b))) if a.checked_add(b).is_some());
        let l = self.label();
        let x = self.tmp(LTy::I64);
        let body = self.sub(|lw| {
            lw.emit(LS::If(LE::Cmp(Op::Gt, Box::new(LE::Var(i)), Box::new(LE::Var(lim)), LTy::I64), vec![LS::Break(l)], vec![]));
            lw.emit(LS::Set(x, LE::Var(i)));
            let next = if bounded { LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::Var(byv)), Ovf::Unchecked) } else { LE::Rt(Rt::SatAdd, vec![LE::Var(i), LE::Var(byv)]) };
            lw.emit(LS::Set(i, next));
            let xv = lw.int_out(LE::Var(x));
            lw.break_target.push((l, lw.defers.len()));
            lw.inline_block(b, &[xv], &[fact], Some(l));
            lw.break_target.pop();
        });
        self.emit(LS::Loop(l, body));
        LE::Unit
    }

    // ---------- pipelines ----------

    fn pipeline(&mut self, term: M, e: &TExpr, recv: &TExpr, args: &[TExpr], blk: Option<&TBlock>) -> LE {
        // Collect stages down to the source.
        let mut stages: Vec<&TExpr> = vec![];
        let mut base = recv;
        while let TK::M(m, Some(r), _, _) = &base.kind {
            if !m.is_stage() || matches!(m, M::Chars | M::Bytes | M::Runes | M::EachIndex | M::EachCons) {
                break;
            }
            stages.push(base);
            base = r;
        }
        stages.reverse();
        let lazy = stages.iter().any(|s| matches!(s.kind, TK::M(M::Lazy, ..)));
        let impure = stages.iter().any(|s| matches!(&s.kind, TK::M(_, _, _, Some(b)) if !b.pure)) || blk.is_some_and(|b| !b.pure);
        if impure && !lazy && !stages.is_empty() && !(stages.len() == 1 && blk.is_none()) {
            // Eager: materialize everything below the last stage first.
            let last = stages[stages.len() - 1];
            let TK::M(_, Some(below), _, _) = &last.kind else { unreachable!() };
            if !stages[..stages.len() - 1].is_empty() || blk.is_some() {
                let below_arr = if stages.len() > 1 { Some(self.materialize(below)) } else { None };
                let last_arr = {
                    let (m, a, b) = match &last.kind {
                        TK::M(m, _, a, b) => (*m, a, b.as_deref()),
                        _ => unreachable!(),
                    };
                    let src = match &below_arr {
                        Some(v) => v.clone(),
                        None => (**below).clone(),
                    };
                    let staged = TExpr { kind: TK::M(m, Some(Box::new(src)), a.clone(), b.cloned().map(Box::new)), ty: last.ty.clone(), span: last.span };
                    self.materialize(&staged)
                };
                return self.pipeline_on(term, e, &last_arr, &[], args, blk);
            }
        }
        self.pipeline_on(term, e, base, &stages, args, blk)
    }

    /// Lower `e` (a Seq) to a fresh array local and return a TExpr naming it.
    fn materialize(&mut self, e: &TExpr) -> TExpr {
        let el = match &e.ty {
            Ty::Seq(t, _) | Ty::Array(t) | Ty::Fixed(t, _) => (**t).clone(),
            _ => Ty::Int,
        };
        let arr_ty = Ty::arr(el);
        let to_a = TExpr { kind: TK::M(M::ToA, Some(Box::new(e.clone())), vec![], None), ty: arr_ty.clone(), span: e.span };
        let v = self.expr(&to_a);
        // Name it with a synthetic local so later code can refer to it.
        let lt = self.lty(&arr_ty);
        let t = self.tmp(lt);
        self.emit(LS::Set(t, v));
        let lid = self.synthetic_local(t, arr_ty.clone());
        TExpr { kind: TK::Local(lid), ty: arr_ty, span: e.span }
    }

    fn synthetic_local(&mut self, v: V, _ty: Ty) -> LocalId {
        // Synthetic locals live past the end of the function's real locals.
        let id = usize::MAX / 2 + self.local_var.len();
        self.local_var.insert(id, v);
        id
    }

    fn pipeline_on(&mut self, term: M, e: &TExpr, base: &TExpr, stages: &[&TExpr], args: &[TExpr], blk: Option<&TBlock>) -> LE {
        use M::*;
        let sp = e.span;
        let outer = self.label();
        let out_ty = self.lty(&e.ty);
        let elem_ty = match &e.ty {
            Ty::Array(t) | Ty::Fixed(t, _) => self.lty(t),
            Ty::Opt(t) if term == Find => self.lty(t),
            _ => out_ty.clone(),
        };
        // Terminal state.
        let acc = match term {
            Sum => {
                let v = self.tmp(out_ty.clone());
                let z = if out_ty == LTy::F64 { LE::F(0.0) } else { self.int_out(LE::I(0)) };
                self.emit(LS::Set(v, z));
                Some(v)
            }
            ToA | Sort => {
                let v = self.tmp(out_ty.clone());
                let cap = self.size_hint(base, stages);
                self.emit(LS::Set(v, LE::ArrWithCap(elem_ty.clone(), Box::new(cap))));
                Some(v)
            }
            Count => {
                let v = self.tmp(LTy::I64);
                self.emit(LS::Set(v, LE::I(0)));
                Some(v)
            }
            All | Any | Include => {
                let v = self.tmp(LTy::Bool);
                self.emit(LS::Set(v, LE::B(term == All)));
                Some(v)
            }
            Max | Min | MaxBy | MinBy | First | Reduce => Some(self.tmp(out_ty.clone())),
            Find => {
                let v = self.tmp(elem_ty.clone());
                self.emit(LS::Set(v, zero_le(&elem_ty)));
                Some(v)
            }
            Each => None,
            _ => unreachable!(),
        };
        let have = if matches!(term, Max | Min | MaxBy | MinBy | First | Find | Reduce) {
            let h = self.tmp(LTy::Bool);
            self.emit(LS::Set(h, LE::B(false)));
            Some(h)
        } else {
            None
        };
        let key = if matches!(term, MaxBy | MinBy) { Some(self.tmp(LTy::I64)) } else { None };
        let key_ty = if let (MaxBy | MinBy, Some(b)) = (term, blk) { b.body.last().map(|s| if let TStmt::Expr(x) = s { self.lty(&x.ty) } else { LTy::I64 }) } else { None };
        if let (Some(k), Some(kt)) = (key, &key_ty) {
            self.vars[k].ty = kt.clone();
        }
        let include_x = if term == Include {
            let x = self.expr(&args[0]);
            let t = self.lty(&args[0].ty);
            Some(self.bind(x, t))
        } else {
            None
        };
        if let (Reduce, Some(init)) = (term, args.first()) {
            let v = self.expr(init);
            self.emit(LS::Set(acc.unwrap(), v));
            self.emit(LS::Set(have.unwrap(), LE::B(true)));
        }
        let elem_lty = self.seq_elem_lty(base, stages);
        let loc = self.loc(sp);
        // Sum bound: at most `count` elements; with an element interval the
        // accumulator's range is known and its additions can't overflow.
        let count = self.count_bound(base, stages);
        let mut k = |lw: &mut Self, x: LE, fact: Option<Fact>, inner: Label| {
            match term {
                Sum => {
                    let v = match blk {
                        Some(b) => lw.inline_block(b, &[x], &[fact], None),
                        None => x,
                    };
                    let a = acc.unwrap();
                    let proven = lw.opts.release && blk.is_none() && match (count, fact.and_then(|f| lw.fact_interval(f))) {
                        (Some(n), Some((lo, hi))) => lo.unsigned_abs().max(hi.unsigned_abs()).checked_mul(n as u64).is_some_and(|m| m < i64::MAX as u64),
                        _ => false,
                    };
                    let sum = if out_ty == LTy::F64 {
                        LE::FArith(Op::Add, Box::new(LE::Var(a)), Box::new(v))
                    } else if lw.promote() {
                        LE::PArith(Op::Add, Box::new(LE::Var(a)), Box::new(v))
                    } else if proven {
                        LE::Arith(Op::Add, Box::new(LE::Var(a)), Box::new(v), Ovf::Unchecked)
                    } else {
                        LE::Arith(Op::Add, Box::new(LE::Var(a)), Box::new(v), lw.ovf(sp))
                    };
                    lw.emit(LS::Set(a, sum));
                }
                ToA | Sort => lw.emit(LS::Push(acc.unwrap(), x)),
                Count => lw.emit(LS::Set(acc.unwrap(), LE::Arith(Op::Add, Box::new(LE::Var(acc.unwrap())), Box::new(LE::I(1)), Ovf::Unchecked))),
                Each => {
                    lw.break_target.push((outer, lw.defers.len()));
                    let v = lw.inline_block(blk.unwrap(), &[x], &[fact], Some(inner));
                    if !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                        lw.emit(LS::Eval(v));
                    }
                    lw.break_target.pop();
                }
                All | Any => {
                    let v = lw.inline_block(blk.unwrap(), &[x], &[], None);
                    let cond = if term == All { LE::Not(Box::new(v)) } else { v };
                    lw.emit(LS::If(cond, vec![LS::Set(acc.unwrap(), LE::B(term == Any)), LS::Break(outer)], vec![]));
                }
                Include => {
                    let t = lw.vars[acc.unwrap()].ty.clone();
                    let _ = t;
                    let et = elem_lty.clone();
                    let c = if et == LTy::PInt { LE::PArith(Op::Eq, Box::new(x), Box::new(include_x.clone().unwrap())) } else { LE::Cmp(Op::Eq, Box::new(x), Box::new(include_x.clone().unwrap()), et) };
                    lw.emit(LS::If(c, vec![LS::Set(acc.unwrap(), LE::B(true)), LS::Break(outer)], vec![]));
                }
                First | Find => {
                    lw.emit(LS::Set(acc.unwrap(), x));
                    lw.emit(LS::Set(have.unwrap(), LE::B(true)));
                    lw.emit(LS::Break(outer));
                }
                Max | Min => {
                    let (a, h) = (acc.unwrap(), have.unwrap());
                    let et = elem_lty.clone();
                    let op = if term == Max { Op::Gt } else { Op::Lt };
                    let better = if et == LTy::PInt { LE::PArith(op, Box::new(x.clone()), Box::new(LE::Var(a))) } else { LE::Cmp(op, Box::new(x.clone()), Box::new(LE::Var(a)), et) };
                    let cond = LE::Cmp(Op::Or, Box::new(LE::Not(Box::new(LE::Var(h)))), Box::new(better), LTy::Bool);
                    lw.emit(LS::If(cond, vec![LS::Set(a, x), LS::Set(h, LE::B(true))], vec![]));
                }
                MaxBy | MinBy => {
                    let (a, h, kv) = (acc.unwrap(), have.unwrap(), key.unwrap());
                    let kx = lw.inline_block(blk.unwrap(), &[x.clone()], &[], None);
                    let kt = key_ty.clone().unwrap_or(LTy::I64);
                    let kx = lw.bind(kx, kt.clone());
                    let op = if term == MaxBy { Op::Gt } else { Op::Lt };
                    let better = if kt == LTy::PInt { LE::PArith(op, Box::new(kx.clone()), Box::new(LE::Var(kv))) } else { LE::Cmp(op, Box::new(kx.clone()), Box::new(LE::Var(kv)), kt) };
                    let cond = LE::Cmp(Op::Or, Box::new(LE::Not(Box::new(LE::Var(h)))), Box::new(better), LTy::Bool);
                    lw.emit(LS::If(cond, vec![LS::Set(a, x), LS::Set(kv, kx), LS::Set(h, LE::B(true))], vec![]));
                }
                Reduce => {
                    let (a, h) = (acc.unwrap(), have.unwrap());
                    let (s, v) = lw.sub_val(|lw| lw.inline_block(blk.unwrap(), &[LE::Var(a), x.clone()], &[], None));
                    let mut s = s;
                    s.push(LS::Set(a, v));
                    lw.emit(LS::If(LE::Var(h), s, vec![LS::Set(a, x), LS::Set(h, LE::B(true))]));
                }
                _ => unreachable!(),
            }
        };
        self.emit_elems(base, stages, outer, Some(outer), &mut k);
        match term {
            Sum | ToA | Count | All | Any | Include => LE::Var(acc.unwrap()),
            Sort => {
                let a = acc.unwrap();
                self.emit(LS::SortInPlace(a, elem_ty));
                LE::Var(a)
            }
            Each => LE::Unit,
            Find => LE::Tup(out_ty.clone(), vec![LE::Var(have.unwrap()), LE::Var(acc.unwrap())]),
            Max | Min | MaxBy | MinBy | First | Reduce => {
                let what = match term {
                    Max => "max",
                    Min => "min",
                    MaxBy => "max_by",
                    MinBy => "min_by",
                    First => "first",
                    _ => "reduce",
                };
                self.emit(LS::If(LE::Not(Box::new(LE::Var(have.unwrap()))), vec![LS::Panic(format!("`{what}` of an empty collection"), loc)], vec![]));
                LE::Var(acc.unwrap())
            }
            _ => unreachable!(),
        }
    }

    fn fact_interval(&self, f: Fact) -> Option<(i64, i64)> {
        match f {
            Fact::Interval(lo, hi) => Some((lo, hi)),
            Fact::IndexOf(a) => self.fixed_len.get(&a).map(|n| (0, n - 1)),
        }
    }

    /// Upper bound on how many elements a pipeline produces.
    fn count_bound(&self, base: &TExpr, stages: &[&TExpr]) -> Option<i64> {
        if stages.iter().any(|s| matches!(s.kind, TK::M(M::FlatMap, ..))) {
            return None;
        }
        match &base.kind {
            TK::Range(lo, hi, excl) => {
                let (a, b) = (self.interval(lo)?.0, self.interval(hi)?.1);
                Some((b - a + if *excl { 0 } else { 1 }).max(0))
            }
            TK::Local(l) => self.fixed_len.get(l).copied(),
            TK::M(M::EachIndex | M::EachCons, Some(a), _, _) => match a.kind {
                TK::Local(l) => self.fixed_len.get(&l).copied(),
                _ => None,
            },
            _ => None,
        }
    }

    fn seq_elem_lty(&self, base: &TExpr, stages: &[&TExpr]) -> LTy {
        let t = stages.last().map_or(&base.ty, |s| &s.ty);
        match t {
            Ty::Seq(t, _) | Ty::Array(t) | Ty::Fixed(t, _) | Ty::Gen(t) => self.lty(t),
            Ty::Range => self.lty(&Ty::Int),
            _ => LTy::Unit,
        }
    }

    /// Capacity to reserve for collecting a pipeline (size rules: worst case).
    fn size_hint(&mut self, base: &TExpr, stages: &[&TExpr]) -> LE {
        if stages.iter().any(|s| matches!(s.kind, TK::M(M::FlatMap | M::TakeWhile, ..))) {
            return LE::I(0);
        }
        match &base.kind {
            TK::Range(lo, hi, excl) => {
                if let (Some(a), Some(b)) = (self.konst(lo), self.konst(hi)) {
                    let n = (b - a + if *excl { 0 } else { 1 }).max(0);
                    return LE::I(n.min(1 << 24));
                }
                LE::I(0)
            }
            TK::Local(_) if matches!(base.ty, Ty::Array(_) | Ty::Fixed(..)) => {
                let v = self.expr(base);
                LE::Len(Box::new(v))
            }
            TK::M(M::Chars | M::Bytes | M::Runes, Some(s), _, _) => {
                let v = self.expr(s);
                LE::Rt(Rt::StrLen, vec![v])
            }
            _ => LE::I(0),
        }
    }

    /// Emit the loops producing each element of `base` + `stages`, calling
    /// `k` with each final element and the innermost loop's label.
    fn emit_elems(&mut self, base: &TExpr, stages: &[&TExpr], outer: Label, own_label: Option<Label>, k: &mut dyn FnMut(&mut Self, LE, Option<Fact>, Label)) {
        // Pre-loop state for counting stages.
        let mut st: Vec<Stage> = vec![];
        for s in stages {
            let TK::M(m, _, args, _) = &s.kind else { unreachable!() };
            let counter = if matches!(m, M::Drop | M::Take | M::EachWithIndex) {
                let c = self.tmp(LTy::I64);
                self.emit(LS::Set(c, LE::I(0)));
                Some(c)
            } else {
                None
            };
            let limit = if matches!(m, M::Drop | M::Take) {
                let n = self.expr(&args[0]);
                let n = self.int_in(n, args[0].span);
                let nv = self.tmp(LTy::I64);
                self.emit(LS::Set(nv, n));
                Some(nv)
            } else {
                None
            };
            st.push(Stage { m: *m, node: s, counter, limit });
        }
        let base_lty = self.lty(&base.ty);
        let promote = self.promote();
        match (&base.kind, &base.ty) {
            (TK::M(M::Chars | M::Bytes | M::Runes, Some(s), _, _), _) => {
                let chars = matches!(base.kind, TK::M(M::Chars, ..));
                let runes = matches!(base.kind, TK::M(M::Runes, ..));
                let sv = self.expr(s);
                let sv = self.bind(sv, LTy::Str);
                let i = self.tmp(LTy::I64);
                let n = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                self.emit(LS::Set(n, LE::Rt(Rt::StrLen, vec![sv.clone()])));
                let l = own_label.unwrap_or_else(|| self.label());
                let body = self.sub(|lw| {
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Var(n)), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = if chars {
                        let cl = lw.tmp(LTy::I64);
                        lw.emit(LS::Set(cl, LE::Rt(Rt::StrChar, vec![sv.clone(), LE::Var(i)])));
                        let x = lw.tmp(LTy::Str);
                        lw.emit(LS::Set(x, LE::Rt(Rt::StrByte, vec![sv.clone(), LE::Var(i), LE::Var(cl)])));
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::Var(cl)), Ovf::Unchecked)));
                        LE::Var(x)
                    } else if runes {
                        let cl = lw.tmp(LTy::I64);
                        lw.emit(LS::Set(cl, LE::Rt(Rt::StrChar, vec![sv.clone(), LE::Var(i)])));
                        let x = lw.decode_rune(&sv, i, cl);
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::Var(cl)), Ovf::Unchecked)));
                        x
                    } else {
                        let x = lw.tmp(LTy::I64);
                        lw.emit(LS::Set(x, LE::Rt(Rt::StrByte, vec![sv.clone(), LE::Var(i), LE::I(0)])));
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                        LE::Var(x)
                    };
                    lw.apply(&st, 0, x, None, l, outer, k);
                });
                self.emit(LS::Loop(l, body));
            }
            (TK::M(M::EachIndex | M::EachCons, Some(a), args, _), _) => {
                let cons = matches!(base.kind, TK::M(M::EachCons, ..));
                let arr_local = if let TK::Local(l) = a.kind { Some(l) } else { None };
                let av = self.expr(a);
                let at = self.lty(&a.ty);
                let av = self.bind(av, at);
                let kk = if cons {
                    let kv = self.expr(&args[0]);
                    let kv = self.int_in(kv, args[0].span);
                    let t = self.tmp(LTy::I64);
                    self.emit(LS::Set(t, kv));
                    Some(t)
                } else {
                    None
                };
                let i = self.tmp(LTy::I64);
                let n = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                self.emit(LS::Set(n, LE::Len(Box::new(av.clone()))));
                let l = own_label.unwrap_or_else(|| self.label());
                let body = self.sub(|lw| {
                    let end = match kk {
                        Some(k) => LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::Var(k)), Ovf::Unchecked),
                        None => LE::Var(i),
                    };
                    let stop = if kk.is_some() { Op::Gt } else { Op::Ge };
                    lw.emit(LS::If(LE::Cmp(stop, Box::new(end), Box::new(LE::Var(n)), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = lw.tmp(if cons { lw.lty(&a.ty) } else { LTy::I64 });
                    if let Some(k) = kk {
                        let at = lw.lty(&a.ty);
                        lw.emit(LS::Set(x, LE::Slice(at, Box::new(av.clone()), Box::new(LE::Var(i)), Box::new(LE::Var(k)))));
                    } else {
                        lw.emit(LS::Set(x, LE::Var(i)));
                    }
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    let fact = if cons { None } else { arr_local.map(Fact::IndexOf) };
                    let xv = if cons { LE::Var(x) } else { lw.int_out(LE::Var(x)) };
                    lw.apply(&st, 0, xv, fact, l, outer, k);
                });
                self.emit(LS::Loop(l, body));
            }
            (_, Ty::Range) => {
                let (lo, hi, excl, fact) = match &base.kind {
                    TK::Range(lo, hi, excl) => {
                        let fact = match (self.interval(lo), self.interval(hi)) {
                            (Some((a, _)), Some((_, b))) => Some(Fact::Interval(a, if *excl { b - 1 } else { b })),
                            _ => None,
                        };
                        let lv = self.expr(lo);
                        let lv = self.int_in(lv, lo.span);
                        let hv = self.expr(hi);
                        let hv = self.int_in(hv, hi.span);
                        (lv, hv, *excl, fact)
                    }
                    _ => {
                        let r = self.expr(base);
                        let r = self.bind(r, LTy::Range);
                        let excl = self.tmp(LTy::Bool);
                        self.emit(LS::Set(excl, LE::RangeField(Box::new(r.clone()), 2)));
                        let hi = LE::Arith(
                            Op::Sub,
                            Box::new(LE::RangeField(Box::new(r.clone()), 1)),
                            Box::new(LE::Cond(Box::new(LE::Var(excl)), Box::new(LE::I(1)), Box::new(LE::I(0)))),
                            Ovf::Unchecked,
                        );
                        (LE::RangeField(Box::new(r), 0), hi, false, None)
                    }
                };
                let i = self.tmp(LTy::I64);
                let h = self.tmp(LTy::I64);
                self.emit(LS::Set(i, lo));
                self.emit(LS::Set(h, hi));
                let l = own_label.unwrap_or_else(|| self.label());
                let bounded = matches!(fact, Some(Fact::Interval(_, b)) if b < i64::MAX);
                let body = self.sub(|lw| {
                    let stop = if excl { Op::Ge } else { Op::Gt };
                    lw.emit(LS::If(LE::Cmp(stop, Box::new(LE::Var(i)), Box::new(LE::Var(h)), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = lw.tmp(LTy::I64);
                    lw.emit(LS::Set(x, LE::Var(i)));
                    if bounded {
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    } else {
                        lw.emit(LS::Set(i, LE::Rt(Rt::SatAdd, vec![LE::Var(i), LE::I(1)])));
                    }
                    // Inclusive range ending at i64::MAX: stop after it.
                    if !excl && !bounded {
                        lw.emit(LS::If(LE::Cmp(Op::Eq, Box::new(LE::Var(x)), Box::new(LE::I(i64::MAX)), LTy::I64), vec![LS::Set(h, LE::I(i64::MIN))], vec![]));
                    }
                    let xv = if promote { LE::ToP(Box::new(LE::Var(x))) } else { LE::Var(x) };
                    lw.apply(&st, 0, xv, fact, l, outer, k);
                });
                self.emit(LS::Loop(l, body));
            }
            (_, Ty::Map(kt, vt)) => {
                let mv = self.expr(base);
                let mv = self.bind(mv, base_lty.clone());
                let tl = LTy::Tup(vec![self.lty(kt), self.lty(vt)]);
                let l = own_label.unwrap_or_else(|| self.label());
                // The pipeline loop is the map walk; `break` leaves it.
                let i = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                let body = self.sub(|lw| {
                    use crate::mapgen::{KEYS, LIVE, VALS};
                    let n = LE::Len(Box::new(Self::map_hdr(&mv, KEYS)));
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(n), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = lw.tmp(tl.clone());
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    let back = LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(-1)), Ovf::Unchecked);
                    let at_back = |f| LE::Index { arr: Box::new(Self::map_hdr(&mv, f)), idx: Box::new(back.clone()), check: None };
                    lw.emit(LS::If(LE::Not(Box::new(at_back(LIVE))), vec![LS::Continue(l)], vec![]));
                    lw.emit(LS::Set(x, LE::Tup(tl.clone(), vec![at_back(KEYS), at_back(VALS)])));
                    lw.apply(&st, 0, LE::Var(x), None, l, outer, k);
                });
                self.emit(LS::Loop(l, body));
            }
            (_, Ty::Array(_) | Ty::Fixed(..)) => {
                let av = self.expr(base);
                let av = self.bind(av, base_lty.clone());
                let el = match &base_lty {
                    LTy::Arr(t) => (**t).clone(),
                    _ => unreachable!(),
                };
                let i = self.tmp(LTy::I64);
                let n = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                self.emit(LS::Set(n, LE::Len(Box::new(av.clone()))));
                let l = own_label.unwrap_or_else(|| self.label());
                let body = self.sub(|lw| {
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Var(n)), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = lw.tmp(el.clone());
                    lw.emit(LS::Set(x, LE::Index { arr: Box::new(av.clone()), idx: Box::new(LE::Var(i)), check: None }));
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    lw.apply(&st, 0, LE::Var(x), None, l, outer, k);
                });
                self.emit(LS::Loop(l, body));
            }
            (_, Ty::Gen(t)) => {
                let gv = self.expr(base);
                let gv = self.bind(gv, base_lty.clone());
                let el = self.lty(t);
                let x = self.tmp(el);
                let l = own_label.unwrap_or_else(|| self.label());
                let body = self.sub(|lw| {
                    lw.emit(LS::NextOrBreak { source: gv.clone(), dst: x, label: l });
                    lw.apply(&st, 0, LE::Var(x), None, l, outer, k);
                });
                self.emit(LS::Loop(l, body));
            }
            (_, t) => unreachable!("pipeline source of type {}", t.show()),
        }
    }

    /// Apply stages `st[i..]` to element `x`, then `k`.
    #[allow(clippy::too_many_arguments)]
    fn apply(&mut self, st: &[Stage], i: usize, x: LE, fact: Option<Fact>, inner: Label, outer: Label, k: &mut dyn FnMut(&mut Self, LE, Option<Fact>, Label)) {
        let Some(s) = st.get(i) else {
            // Hand the element (and its fact) to the terminal.
            return k(self, x, fact, inner);
        };
        let TK::M(m, _, _, blk) = &s.node.kind else { unreachable!() };
        let blk = blk.as_deref();
        match m {
            M::Select | M::Reject => {
                let v = self.inline_block(blk.unwrap(), &[x.clone()], &[fact], None);
                let cond = if *m == M::Select { LE::Not(Box::new(v)) } else { v };
                self.emit(LS::If(cond, vec![LS::Continue(inner)], vec![]));
                self.apply(st, i + 1, x, fact, inner, outer, k);
            }
            M::TakeWhile => {
                let v = self.inline_block(blk.unwrap(), &[x.clone()], &[fact], None);
                self.emit(LS::If(LE::Not(Box::new(v)), vec![LS::Break(outer)], vec![]));
                self.apply(st, i + 1, x, fact, inner, outer, k);
            }
            M::Map => {
                let v = self.inline_block(blk.unwrap(), &[x], &[fact], None);
                let t = self.lty(&match &s.node.ty {
                    Ty::Seq(t, _) => (**t).clone(),
                    t => t.clone(),
                });
                let v = self.bind(v, t);
                self.apply(st, i + 1, v, None, inner, outer, k);
            }
            M::FlatMap => {
                let b = blk.unwrap();
                let tail = self.block_prefix(b, &[x]);
                let Some(tail) = tail else { return };
                // The block's value is itself a sequence: loop over it.
                let mut inner_stages: Vec<&TExpr> = vec![];
                let mut src = tail;
                while let TK::M(mm, Some(r), _, _) = &src.kind {
                    if !mm.is_stage() || matches!(mm, M::Chars | M::Bytes | M::Runes | M::EachIndex | M::EachCons) {
                        break;
                    }
                    inner_stages.push(src);
                    src = r;
                }
                inner_stages.reverse();
                let mut k2 = |lw: &mut Self, y: LE, f2: Option<Fact>, inner2: Label| lw.apply(st, i + 1, y, f2, inner2, outer, k);
                self.emit_elems(src, &inner_stages, outer, None, &mut k2);
            }
            M::Drop => {
                let c = s.counter.unwrap();
                let lim = s.limit.unwrap();
                self.emit(LS::If(
                    LE::Cmp(Op::Lt, Box::new(LE::Var(c)), Box::new(LE::Var(lim)), LTy::I64),
                    vec![LS::Set(c, LE::Arith(Op::Add, Box::new(LE::Var(c)), Box::new(LE::I(1)), Ovf::Unchecked)), LS::Continue(inner)],
                    vec![],
                ));
                self.apply(st, i + 1, x, fact, inner, outer, k);
            }
            M::Take => {
                let c = s.counter.unwrap();
                let lim = s.limit.unwrap();
                self.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(c)), Box::new(LE::Var(lim)), LTy::I64), vec![LS::Break(outer)], vec![]));
                self.emit(LS::Set(c, LE::Arith(Op::Add, Box::new(LE::Var(c)), Box::new(LE::I(1)), Ovf::Unchecked)));
                self.apply(st, i + 1, x, fact, inner, outer, k);
            }
            M::EachWithIndex => {
                let c = s.counter.unwrap();
                let idx = self.int_out(LE::Var(c));
                let tt = self.lty(&match &s.node.ty {
                    Ty::Seq(t, _) => (**t).clone(),
                    t => t.clone(),
                });
                let t = LE::Tup(tt.clone(), vec![x, idx]);
                let t = self.bind(t, tt);
                self.emit(LS::Set(c, LE::Arith(Op::Add, Box::new(LE::Var(c)), Box::new(LE::I(1)), Ovf::Unchecked)));
                self.apply(st, i + 1, t, None, inner, outer, k);
            }
            M::Lazy => self.apply(st, i + 1, x, fact, inner, outer, k),
            _ => unreachable!("stage {m:?}"),
        }
    }

    // ---------- generators and workers ----------

    /// Locals a block uses but doesn't own: they are captured by value.
    fn captures(&self, b: &TBlock) -> Vec<LocalId> {
        let mut used = vec![];
        for s in &b.body {
            collect_locals_stmt(s, &mut used);
        }
        used.sort();
        used.dedup();
        used.into_iter().filter(|l| !(b.own.0..b.own.1).contains(l) && !b.params.contains(l) && *l < self.f.locals.len()).collect()
    }

    fn generator(&mut self, e: &TExpr, b: &TBlock) -> LE {
        let caps = self.captures(b);
        let elem = match self.lty(&e.ty) {
            LTy::Gen(t) => *t,
            _ => unreachable!(),
        };
        let mut g = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Die(vec![]), self.prog);
        g.consts = self.consts.clone();
        g.fixed_len = self.fixed_len.clone();
        let cap_vars: Vec<V> = caps.iter().map(|l| g.var_of(*l)).collect();
        for p in &b.params {
            g.var_of(*p);
        }
        let body = g.sub(|g| g.stmts(&b.body));
        let mut prog = self.prog.borrow_mut();
        let id = prog.gens.len();
        let func = LFunc { name: format!("gen{id}"), params: vec![], vars: g.vars, ret: LTy::Unit, fallible: false, body, external: false, is_main: false, labels: g.labels };
        prog.gens.push(LGen { id, elem, captures: cap_vars, func });
        drop(prog);
        let vals = caps.iter().map(|l| LE::Var(self.var_of(*l))).collect();
        LE::GenNew(id, vals)
    }

    fn pmap(&mut self, e: &TExpr, recv: &TExpr, b: &TBlock) -> LE {
        let caps = self.captures(b);
        if let Some(l) = caps.first() {
            // Checked as a limitation of v0 workers.
            let name = &self.f.locals[*l].name;
            let msg = format!("`pmap` blocks can't capture locals yet (`{name}`)");
            self.emit(LS::Panic(msg, self.loc(b.span)));
        }
        let arr = self.expr(recv);
        let in_ty = match self.lty(&recv.ty) {
            LTy::Arr(t) => *t,
            _ => unreachable!(),
        };
        let out_ty = match self.lty(&e.ty) {
            LTy::Arr(t) => *t,
            _ => unreachable!(),
        };
        let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Return(vec![]), self.prog);
        let param = w.new_var("x", in_ty.clone());
        let (body_stmts, v) = w.sub_val(|w| w.inline_block(b, &[LE::Var(param)], &[], None));
        let mut body = body_stmts;
        body.push(LS::Return(Some(v)));
        let fallible = contains_try(&body);
        let mut prog = self.prog.borrow_mut();
        let id = prog.workers.len();
        let func = LFunc { name: format!("worker{id}"), params: vec![param], vars: std::mem::take(&mut w.vars), ret: out_ty.clone(), fallible, body, external: false, is_main: false, labels: w.labels };
        prog.workers.push(LWorker { id, input: in_ty, func });
        drop(prog);
        let dst = self.tmp(LTy::Arr(Box::new(out_ty)));
        let path = self.err_path();
        self.emit(LS::Pmap { dst, arr, worker: id, path });
        LE::Var(dst)
    }
}

fn isqrt(n: i64) -> i64 {
    if n < 2 {
        return n;
    }
    let mut x = (n as f64).sqrt() as i64;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x
}

fn contains_try(ss: &[LS]) -> bool {
    ss.iter().any(|s| match s {
        LS::TryArith { .. } | LS::TryCall { .. } | LS::TryRead { .. } => true,
        LS::If(_, a, b) => contains_try(a) || contains_try(b),
        LS::Loop(_, b) => contains_try(b),
        LS::Pmap { .. } => true,
        _ => false,
    })
}

fn collect_locals_stmt(s: &TStmt, out: &mut Vec<LocalId>) {
    match s {
        TStmt::Expr(e) => collect_locals(e, out),
        TStmt::MultiAssign(ls, es) => {
            out.extend(ls);
            es.iter().for_each(|e| collect_locals(e, out));
        }
        TStmt::While(c, b) => {
            collect_locals(c, out);
            b.iter().for_each(|s| collect_locals_stmt(s, out));
        }
        TStmt::If(c, a, b) => {
            collect_locals(c, out);
            a.iter().chain(b).for_each(|s| collect_locals_stmt(s, out));
        }
        TStmt::Break(Some(e), _) | TStmt::Return(Some(e), _) | TStmt::Defer(e) => collect_locals(e, out),
        _ => {}
    }
}

fn collect_locals(e: &TExpr, out: &mut Vec<LocalId>) {
    match &e.kind {
        TK::Local(l) => out.push(*l),
        TK::Assign(l, v) => {
            out.push(*l);
            collect_locals(v, out);
        }
        TK::IndexAssign(l, i, v) => {
            out.push(*l);
            collect_locals(i, out);
            collect_locals(v, out);
        }
        TK::Bin(_, a, b) | TK::Range(a, b, _) | TK::Index(a, b) => {
            collect_locals(a, out);
            collect_locals(b, out);
        }
        TK::Neg(x) | TK::Not(x) | TK::Try(x) | TK::Puts(x) | TK::Some(x) => collect_locals(x, out),
        TK::Slice(a, lo, hi, _) => {
            collect_locals(a, out);
            lo.iter().chain(hi.iter()).for_each(|x| collect_locals(x, out));
        }
        TK::Ternary(a, b, c) => {
            collect_locals(a, out);
            collect_locals(b, out);
            collect_locals(c, out);
        }
        TK::Call(_, xs) | TK::Array(xs) | TK::Format(_, xs) => xs.iter().for_each(|x| collect_locals(x, out)),
        TK::Seq(ss) => ss.iter().for_each(|s| collect_locals_stmt(s, out)),
        TK::PlaceAssign(l, steps, _, v) => {
            out.push(*l);
            for st in steps {
                if let TStep::Index(i) = st {
                    collect_locals(i, out);
                }
            }
            collect_locals(v, out);
        }
        TK::M(_, r, xs, b) => {
            if let Some(r) = r {
                collect_locals(r, out);
            }
            xs.iter().for_each(|x| collect_locals(x, out));
            if let Some(b) = b {
                out.extend(&b.params);
                b.body.iter().for_each(|s| collect_locals_stmt(s, out));
            }
        }
        _ => {}
    }
}

fn op_of(op: BinOp) -> Op {
    match op {
        BinOp::Add | BinOp::AddW => Op::Add,
        BinOp::Sub | BinOp::SubW => Op::Sub,
        BinOp::Mul | BinOp::MulW => Op::Mul,
        BinOp::Div => Op::Div,
        BinOp::Rem => Op::Rem,
        BinOp::Pow => Op::Pow,
        BinOp::Eq => Op::Eq,
        BinOp::Ne => Op::Ne,
        BinOp::Lt => Op::Lt,
        BinOp::Le => Op::Le,
        BinOp::Gt => Op::Gt,
        BinOp::Ge => Op::Ge,
        BinOp::And => Op::And,
        BinOp::Or => Op::Or,
        // Bit operations never reach `Op` (they lower to `Prim`).
        _ => Op::Add,
    }
}

fn is_cmp(op: BinOp) -> bool {
    matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
}

/// Truncate to a narrow kind's width (identity for 64-bit kinds).
fn wrap_to(k: IntKind, e: LE) -> LE {
    if k.bits() == 64 { e } else { LE::Prim(Prim::Wrap(k), vec![e]) }
}

/// `r` (an exact 64-bit result) is outside narrow kind `k`'s range.
fn out_of_range(k: IntKind, r: LE) -> LE {
    if k.signed() {
        LE::Cmp(
            Op::Or,
            Box::new(LE::Cmp(Op::Lt, Box::new(r.clone()), Box::new(LE::I(k.min() as i64)), LTy::I64)),
            Box::new(LE::Cmp(Op::Gt, Box::new(r), Box::new(LE::I(k.max() as i64)), LTy::I64)),
            LTy::Bool,
        )
    } else {
        // Unsigned: anything above max, including "negative" results.
        LE::Prim(Prim::ULt, vec![LE::I(k.max() as i64), r])
    }
}

/// The zero value of a LIR type, as an expression.
fn zero_le(t: &LTy) -> LE {
    match t {
        LTy::I64 | LTy::IntK(_) => LE::I(0),
        LTy::F64 => LE::F(0.0),
        LTy::Bool => LE::B(false),
        LTy::Unit => LE::Unit,
        LTy::Str => LE::S(String::new()),
        LTy::PInt => LE::ToP(Box::new(LE::I(0))),
        LTy::Arr(e) => LE::ArrWithCap((**e).clone(), Box::new(LE::I(0))),
        LTy::Tup(ts) => LE::Tup(t.clone(), ts.iter().map(zero_le).collect()),
        LTy::Range => LE::Range(Box::new(LE::I(0)), Box::new(LE::I(0)), false),
        LTy::Gen(_) => panic!("an optional generator has no zero value yet"),
    }
}

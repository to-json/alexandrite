//! Typed tree → LIR.
//!
//! Enumerable chains are fused here: a terminal (`sum`, `max`, `to_a`, ...)
//! walks down its receiver collecting stages (`select`, `map`, `flat_map`,
//! ...) to a source (Range, Array, Enumerator, Str#chars, each_index,
//! each_cons) and emits one loop. Without `.lazy`, a chain whose blocks do
//! I/O is materialized stage by stage instead (Ruby's eager semantics).

use crate::ast::{BinOp, Overflow};
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
            let mut lw = Lw::new(p, sm, opts, f, f.overflow, ErrPath::Return, &prog);
            let params = f.params.iter().map(|l| lw.var_of(*l)).collect();
            LFunc { name: f.cname.clone(), params, vars: lw.vars, ret: lty(&f.ret, f.overflow), fallible: f.fallible, body: vec![], external: true, is_main: false, labels: 0 }
        } else {
            let path = if f.is_main { ErrPath::Die } else { ErrPath::Return };
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
        Ty::Bool => LTy::Bool,
        Ty::Str => LTy::Str,
        Ty::Unit | Ty::Never | Ty::Yielder(_) | Ty::Var(_) => LTy::Unit,
        Ty::Array(t) | Ty::Seq(t, _) => LTy::Arr(Box::new(lty(t, mode))),
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
    next_target: Vec<Option<Label>>,
    break_target: Vec<Label>,
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
            for (i, s) in body.iter().enumerate() {
                if returns_value && i == body.len() - 1 {
                    if let TStmt::Expr(e) = s {
                        let v = lw.expr(e);
                        lw.emit(LS::Return(Some(v)));
                        continue;
                    }
                }
                lw.stmt(s);
            }
        })
    }

    fn stmts(&mut self, ss: &[TStmt]) {
        for s in ss {
            self.stmt(s);
        }
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
                    lw.next_target.push(Some(l));
                    lw.break_target.push(l);
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
                Some(Some(l)) => {
                    let l = *l;
                    self.emit(LS::Continue(l));
                }
                _ => unreachable!("`next` outside a loop survived checking"),
            },
            TStmt::Break(_, _) => {
                let l = *self.break_target.last().expect("break target");
                self.emit(LS::Break(l));
            }
            TStmt::Return(v, _) => {
                let v = v.as_ref().map(|v| self.expr(v));
                self.emit(LS::Return(v));
            }
        }
    }

    // ---------- expressions ----------

    fn expr(&mut self, e: &TExpr) -> LE {
        match &e.kind {
            TK::Int(v) => self.int_out(LE::I(*v)),
            TK::Str(s) => LE::S(s.clone()),
            TK::Bool(b) => LE::B(*b),
            TK::Unit => LE::Unit,
            TK::Local(l) => LE::Var(self.var_of(*l)),
            TK::Assign(l, v) => {
                let mut x = self.expr(v);
                if matches!(v.kind, TK::Local(_)) && matches!(v.ty, Ty::Array(_)) {
                    x = LE::Rt(Rt::ArrCopy, vec![x]);
                }
                let var = self.var_of(*l);
                self.emit(LS::Set(var, x));
                LE::Var(var)
            }
            TK::IndexAssign(l, i, v) => {
                let arr_t = TExpr { kind: TK::Local(*l), ty: self.f.locals[*l].ty.clone(), span: e.span };
                let check = self.index_check(&arr_t, i);
                let iv = self.expr(i);
                let iv = self.int_in(iv, i.span);
                let vv = self.expr(v);
                let vt = self.lty(&v.ty);
                let vv = self.bind(vv, vt);
                let var = self.var_of(*l);
                self.emit(LS::SetIndex { arr: var, idx: iv, val: vv.clone(), check });
                vv
            }
            TK::Bin(op, a, b) => self.binary(*op, a, b, e),
            TK::Neg(x) => {
                let v = self.expr(x);
                if self.promote() {
                    LE::PArith(Op::Sub, Box::new(LE::ToP(Box::new(LE::I(0)))), Box::new(v))
                } else if self.try_arith {
                    let dst = self.tmp(LTy::I64);
                    let path = self.path.clone();
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
            TK::Index(a, i) => {
                let check = self.index_check(a, i);
                let av = self.expr(a);
                let iv = self.expr(i);
                let iv = self.int_in(iv, i.span);
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

    /// Arguments are copied if they are array locals (value semantics).
    fn arg(&mut self, a: &TExpr) -> LE {
        let v = self.expr(a);
        if matches!(a.kind, TK::Local(_)) && matches!(a.ty, Ty::Array(_)) { LE::Rt(Rt::ArrCopy, vec![v]) } else { v }
    }

    fn try_expr(&mut self, inner: &TExpr) -> LE {
        let path = self.path.clone();
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
        let lop = match op {
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
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
        };
        if matches!(op, BinOp::And | BinOp::Or) {
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
        if !op.is_arith() {
            if at == LTy::PInt {
                return LE::PArith(lop, Box::new(av), Box::new(bv));
            }
            return LE::Cmp(lop, Box::new(av), Box::new(bv), at);
        }
        if at == LTy::PInt {
            return LE::PArith(lop, Box::new(av), Box::new(bv));
        }
        if self.try_arith {
            let dst = self.tmp(LTy::I64);
            let path = self.path.clone();
            self.emit(LS::TryArith { dst, op: lop, a: av, b: bv, loc: self.loc(e.span), path });
            return LE::Var(dst);
        }
        // Release: an operation whose result interval is known can't overflow.
        if self.opts.release && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) && self.interval(e).is_some() {
            return LE::Arith(lop, Box::new(av), Box::new(bv), Ovf::Unchecked);
        }
        LE::Arith(lop, Box::new(av), Box::new(bv), self.ovf(e.span))
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
        self.next_target.push(next_label);
        let mut val = LE::Unit;
        for (i, s) in b.body.iter().enumerate() {
            if i == b.body.len() - 1 {
                if let TStmt::Expr(e) = s {
                    val = self.expr(e);
                    continue;
                }
            }
            self.stmt(s);
        }
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
            Sum | Max | Min | MaxBy | MinBy | First | ToA | Each | Reduce | All | Any | Count | Include | Sort => {
                self.pipeline(m, e, recv.unwrap(), args, blk)
            }
            Pmap => self.pmap(e, recv.unwrap(), blk.unwrap()),
            Size => {
                let r = recv.unwrap();
                let v = self.expr(r);
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
                let v = self.expr(recv.unwrap());
                LE::Rt(if self.promote() { Rt::PIntToS } else { Rt::IntToS }, vec![v])
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
                let v = self.expr(recv.unwrap());
                let ev = LE::Rt(if self.promote() { Rt::PEven } else { Rt::Even }, vec![v]);
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
            ArrayNew => {
                let n = self.expr(&args[0]);
                let n = self.int_in(n, args[0].span);
                let fill = self.expr(&args[1]);
                let el = self.lty(&args[1].ty);
                LE::ArrNew(el, Box::new(n), Box::new(fill), self.loc(sp))
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
                    lw.next_target.push(Some(l));
                    lw.break_target.push(l);
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
            lw.break_target.push(l);
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
            if !m.is_stage() || matches!(m, M::Chars | M::Bytes | M::EachIndex | M::EachCons) {
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
            Ty::Seq(t, _) | Ty::Array(t) => (**t).clone(),
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
            Ty::Array(t) => self.lty(t),
            _ => out_ty.clone(),
        };
        // Terminal state.
        let acc = match term {
            Sum => {
                let v = self.tmp(out_ty.clone());
                let z = self.int_out(LE::I(0));
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
            Each => None,
            _ => unreachable!(),
        };
        let have = if matches!(term, Max | Min | MaxBy | MinBy | First | Reduce) {
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
                    let sum = if lw.promote() {
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
                    lw.break_target.push(outer);
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
                First => {
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
            Ty::Seq(t, _) | Ty::Array(t) | Ty::Gen(t) => self.lty(t),
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
            TK::Local(_) if matches!(base.ty, Ty::Array(_)) => {
                let v = self.expr(base);
                LE::Len(Box::new(v))
            }
            TK::M(M::Chars | M::Bytes, Some(s), _, _) => {
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
            (TK::M(M::Chars | M::Bytes, Some(s), _, _), _) => {
                let chars = matches!(base.kind, TK::M(M::Chars, ..));
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
                    } else {
                        let x = lw.tmp(LTy::I64);
                        lw.emit(LS::Set(x, LE::Rt(Rt::StrByte, vec![sv.clone(), LE::Var(i), LE::I(0)])));
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                        lw.int_out(LE::Var(x))
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
            (_, Ty::Array(_)) => {
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
                    if !mm.is_stage() || matches!(mm, M::Chars | M::Bytes | M::EachIndex | M::EachCons) {
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
        let mut g = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Die, self.prog);
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
        let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Return, self.prog);
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
        let path = self.path.clone();
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
        TStmt::Break(Some(e), _) | TStmt::Return(Some(e), _) => collect_locals(e, out),
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
        TK::Neg(x) | TK::Not(x) | TK::Try(x) | TK::Puts(x) => collect_locals(x, out),
        TK::Ternary(a, b, c) => {
            collect_locals(a, out);
            collect_locals(b, out);
            collect_locals(c, out);
        }
        TK::Call(_, xs) | TK::Array(xs) => xs.iter().for_each(|x| collect_locals(x, out)),
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

//! Proven-or-fallible for `#[pure]` functions (DESIGN.md).
//!
//! Every maximal arithmetic expression outside `try` must be proven not to
//! overflow (or divide by zero). Rules, deliberately small for v0:
//! - constant operands that fold without overflow;
//! - `/` and `%` by a constant other than 0 and -1;
//! - the counter axiom: `v += 1` where every assignment to `v` is a constant
//!   or `+= 1` (a 64-bit counter can't overflow in any feasible run).
//! Indexing in a pure function must be under `try` (no interval facts here).

use crate::ast::BinOp;
use crate::diag::{Diag, SourceMap};
use crate::tast::*;
use std::collections::HashSet;

pub fn prove(f: &TFunc, sm: &SourceMap) -> Result<(), Diag> {
    if !f.pure || f.external {
        return Ok(());
    }
    let mut counters: HashSet<LocalId> = (0..f.locals.len()).filter(|i| !f.params.contains(i)).collect();
    for s in &f.body {
        scan_assigns_stmt(s, &mut counters);
    }
    let mut p = Prover { f, sm, counters };
    for s in &f.body {
        p.stmt(s)?;
    }
    Ok(())
}

fn scan_assigns_stmt(s: &TStmt, c: &mut HashSet<LocalId>) {
    match s {
        TStmt::Expr(e) => scan_assigns(e, c),
        TStmt::MultiAssign(ls, es) => {
            for l in ls {
                c.remove(l);
            }
            es.iter().for_each(|e| scan_assigns(e, c));
        }
        TStmt::While(e, b) => {
            scan_assigns(e, c);
            b.iter().for_each(|s| scan_assigns_stmt(s, c));
        }
        TStmt::If(e, a, b) => {
            scan_assigns(e, c);
            a.iter().chain(b).for_each(|s| scan_assigns_stmt(s, c));
        }
        TStmt::Break(Some(e), _) | TStmt::Return(Some(e), _) | TStmt::Defer(e) => scan_assigns(e, c),
        _ => {}
    }
}

fn is_incr(l: LocalId, e: &TExpr) -> bool {
    matches!(&e.kind, TK::Bin(BinOp::Add, a, b) if matches!(a.kind, TK::Local(x) if x == l) && matches!(b.kind, TK::Int(1)))
}

fn scan_assigns(e: &TExpr, c: &mut HashSet<LocalId>) {
    if let TK::Assign(l, v) = &e.kind {
        if !matches!(v.kind, TK::Int(_)) && !is_incr(*l, v) {
            c.remove(l);
        }
    }
    each_child(e, &mut |x| scan_assigns(x, c));
}

pub(crate) fn each_child(e: &TExpr, f: &mut dyn FnMut(&TExpr)) {
    match &e.kind {
        TK::Assign(_, v) | TK::Neg(v) | TK::Not(v) | TK::Try(v) | TK::Puts(v) | TK::Some(v) => f(v),
        TK::IndexAssign(_, i, v) => {
            f(i);
            f(v);
        }
        TK::Bin(_, a, b) | TK::Range(a, b, _) | TK::Index(a, b) => {
            f(a);
            f(b);
        }
        TK::Select(arms, d) => {
            for a in arms {
                match a {
                    TSelArm::Recv { ch, body, .. } => {
                        f(ch);
                        body.iter().for_each(|s| stmt_exprs(s, f));
                    }
                    TSelArm::Send { ch, val, body } => {
                        f(ch);
                        f(val);
                        body.iter().for_each(|s| stmt_exprs(s, f));
                    }
                }
            }
            d.iter().flatten().for_each(|s| stmt_exprs(s, f));
        }
        TK::Slice(a, lo, hi, _) => {
            f(a);
            lo.iter().chain(hi.iter()).for_each(|x| f(x));
        }
        TK::Ternary(c, a, b) => {
            f(c);
            f(a);
            f(b);
        }
        TK::Call(_, args) | TK::Array(args) | TK::Format(_, args) => args.iter().for_each(f),
        TK::Seq(ss) => {
            for s in ss {
                stmt_exprs(s, f);
            }
        }
        TK::PlaceAssign(_, steps, _, v) => {
            for st in steps {
                if let TStep::Index(i) = st {
                    f(i);
                }
            }
            f(v);
        }
        TK::M(_, r, args, blk) => {
            if let Some(r) = r {
                f(r);
            }
            args.iter().for_each(&mut *f);
            if let Some(b) = blk {
                for s in &b.body {
                    stmt_exprs(s, f);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn stmt_exprs(s: &TStmt, f: &mut dyn FnMut(&TExpr)) {
    match s {
        TStmt::Expr(e) => f(e),
        TStmt::MultiAssign(_, es) => es.iter().for_each(f),
        TStmt::While(e, b) => {
            f(e);
            b.iter().for_each(|s| stmt_exprs(s, f));
        }
        TStmt::If(e, a, b) => {
            f(e);
            a.iter().chain(b).for_each(|s| stmt_exprs(s, f));
        }
        TStmt::Break(Some(e), _) | TStmt::Return(Some(e), _) | TStmt::Defer(e) | TStmt::Fail(e, _) => f(e),
        _ => {}
    }
}

fn konst(e: &TExpr) -> Option<i64> {
    match &e.kind {
        TK::Int(v) => Some(*v),
        TK::Neg(x) => konst(x).and_then(i64::checked_neg),
        TK::Bin(op, a, b) => {
            let (a, b) = (konst(a)?, konst(b)?);
            match op {
                BinOp::Add => a.checked_add(b),
                BinOp::Sub => a.checked_sub(b),
                BinOp::Mul => a.checked_mul(b),
                BinOp::Div => a.checked_div(b),
                BinOp::Rem => a.checked_rem(b),
                _ => None,
            }
        }
        _ => None,
    }
}

struct Prover<'a> {
    f: &'a TFunc,
    sm: &'a SourceMap,
    counters: HashSet<LocalId>,
}

impl Prover<'_> {
    fn stmt(&mut self, s: &TStmt) -> Result<(), Diag> {
        let mut r = Ok(());
        stmt_exprs(s, &mut |e| {
            if r.is_ok() {
                r = self.expr(e, false);
            }
        });
        r
    }

    fn unproven(&self, e: &TExpr) -> Diag {
        Diag::new(e.span, format!("unproven arithmetic in `#[pure] def {}`: `{}` may overflow; use `~(...)` or prove a bound", self.f.src_name, self.sm.snippet(e.span).trim_matches(|c| c == '(' || c == ')')))
    }

    /// Is every arithmetic operation in this arithmetic tree proven?
    fn proven(&self, e: &TExpr, assigned_to: Option<LocalId>) -> bool {
        match &e.kind {
            TK::Bin(op, a, b) if op.is_arith() => {
                if konst(e).is_some() {
                    return true;
                }
                let own = match op {
                    BinOp::Div | BinOp::Rem => konst(b).is_some_and(|c| c != 0 && c != -1),
                    BinOp::Add => assigned_to.is_some_and(|l| self.counters.contains(&l) && is_incr(l, e)),
                    _ => false,
                };
                own && self.proven(a, None) && self.proven(b, None)
            }
            TK::Neg(_) => konst(e).is_some(),
            _ => true,
        }
    }

    fn expr(&mut self, e: &TExpr, under_try: bool) -> Result<(), Diag> {
        match &e.kind {
            TK::Try(x) => return self.expr(x, true),
            TK::Assign(l, v) if !under_try && is_arith(v) => {
                if !self.proven(v, Some(*l)) {
                    return Err(self.unproven(v));
                }
                return self.children_of_arith(v, under_try);
            }
            _ if is_arith(e) => {
                if !under_try && !self.proven(e, None) {
                    return Err(self.unproven(e));
                }
                return self.children_of_arith(e, under_try);
            }
            TK::PlaceAssign(_, steps, op, _) if !under_try && (steps.iter().any(|s| matches!(s, TStep::Index(_))) || matches!(op, Some(o) if o.is_arith() && e.ty == Ty::Int)) => {
                let what = if steps.iter().any(|s| matches!(s, TStep::Index(_))) { "index" } else { "arithmetic" };
                return Err(Diag::new(e.span, format!("unproven {what} in `#[pure] def {}`: `{}`; use `~(...)`", self.f.src_name, self.sm.snippet(e.span))));
            }
            TK::Bin(crate::ast::BinOp::Shl | crate::ast::BinOp::Shr, _, c) if !under_try && !matches!(c.kind, TK::Int(n) if n >= 0) && c.ty.int_kind().is_some_and(|k| k.signed()) => {
                return Err(Diag::new(c.span, format!("unproven shift in `#[pure] def {}`: a negative count panics; use a constant or an unsigned count", self.f.src_name)));
            }
            TK::Index(..) if !under_try => {
                return Err(Diag::new(e.span, format!("unproven index in `#[pure] def {}`: `{}` may be out of bounds; use `~(...)`", self.f.src_name, self.sm.snippet(e.span))));
            }
            _ => {}
        }
        let mut r = Ok(());
        each_child(e, &mut |x| {
            if r.is_ok() {
                r = self.expr(x, under_try);
            }
        });
        r
    }

    /// Recurse into the non-arithmetic leaves of an arithmetic tree.
    fn children_of_arith(&mut self, e: &TExpr, under_try: bool) -> Result<(), Diag> {
        let mut r = Ok(());
        each_child(e, &mut |x| {
            if r.is_ok() {
                r = if is_arith(x) { self.children_of_arith(x, under_try) } else { self.expr(x, under_try) };
            }
        });
        r
    }
}

/// Int arithmetic, which can overflow. (Float arithmetic can't fail.)
fn is_arith(e: &TExpr) -> bool {
    e.ty.int_kind().is_some() && (matches!(&e.kind, TK::Bin(op, ..) if op.is_arith()) || matches!(e.kind, TK::Neg(_)))
}

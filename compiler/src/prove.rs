//! Proven-or-fallible for `#[pure]` functions (DESIGN.md).
//!
//! Every maximal arithmetic expression outside `try` must be proven not to
//! overflow (or divide by zero). Rules, deliberately small for v0:
//! - constant operands that fold without overflow;
//! - `/` and `%` by a constant other than 0 and -1;
//! - the counter axiom: `v += 1` where every assignment to `v` is a constant
//!   or `+= 1` (a 64-bit counter can't overflow in any feasible run).
//! Indexing in a pure function must be under `try` (no interval facts here).
//!
//! Every other builtin that can panic (slices, `first`/`last`/`max`/...
//! of a possibly empty collection, `sum`, `unwrap`, checked conversions,
//! `digits`, `Int.sqrt`, `Array.new`, `step`, pool slots) is a
//! [`fault`]: unproven unless its operands are constants that rule the
//! failure out, so it must be under `~(...)`, which turns it into an error
//! (IndexError, ArithError) instead of a panic.

use crate::ast::{BinOp, IntKind};
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

pub(crate) fn each_child<'a>(e: &'a TExpr, f: &mut dyn FnMut(&'a TExpr)) {
    match &e.kind {
        TK::Assign(_, v) | TK::Neg(v) | TK::Not(v) | TK::Try(v) | TK::Puts(v) | TK::Panic(v) | TK::Some(v) => f(v),
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
        TK::PlaceAssign(_, steps, _, v) | TK::Bang(_, steps, _, v) => {
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

pub(crate) fn stmt_exprs<'a>(s: &'a TStmt, f: &mut dyn FnMut(&'a TExpr)) {
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

/// A builtin operation that can panic, as the pure-code rules see it.
pub(crate) struct Fault {
    /// The builtin error it fails with under `~` ("Error": the error it carries).
    pub err: &'static str,
    /// What it is, for the diagnostic ("index", "`first`", ...).
    pub what: String,
    /// Why it may fail ("may be out of bounds").
    pub why: &'static str,
}

fn fault_of(err: &'static str, what: impl Into<String>, why: &'static str) -> Option<Fault> {
    Some(Fault { err, what: what.into(), why })
}

/// A collection known to be non-empty from constants alone.
fn nonempty_const(e: &TExpr) -> bool {
    match &e.kind {
        TK::Range(a, b, excl) => matches!((konst(a), konst(b)), (Some(a), Some(b)) if a < b || (a == b && !excl)),
        TK::Array(xs) => !xs.is_empty(),
        _ => false,
    }
}

/// The sum of a constant range, if it fits an Int.
fn const_range_sum(e: &TExpr) -> Option<i64> {
    let TK::Range(a, b, excl) = &e.kind else { return None };
    let (a, b) = (konst(a)? as i128, konst(b)? as i128);
    let b = if *excl { b - 1 } else { b };
    if b < a {
        return Some(0);
    }
    i64::try_from((a + b) * (b - a + 1) / 2).ok()
}

/// Can this node (not counting its children, nor arithmetic operators,
/// which the arithmetic rules cover) panic at run time? Constant operands
/// that rule the failure out prove it can't.
pub(crate) fn fault(e: &TExpr) -> Option<Fault> {
    let nonneg = |x: &TExpr| konst(x).is_some_and(|c| c >= 0) || x.ty.int_kind().is_some_and(|k| !k.signed());
    match &e.kind {
        TK::Index(..) | TK::IndexAssign(..) => fault_of("IndexError", "index", "may be out of bounds"),
        TK::PlaceAssign(_, steps, ..) | TK::Bang(_, steps, ..) if steps.iter().any(|s| matches!(s, TStep::Index(_))) => fault_of("IndexError", "index", "may be out of bounds"),
        TK::Slice(_, lo, hi, _) => {
            let from_start = lo.as_ref().is_none_or(|x| konst(x) == Some(0));
            if from_start && hi.is_none() { None } else { fault_of("IndexError", "slice", "may be out of range") }
        }
        TK::M(m, recv, args, _) => {
            let name = |s: &str| format!("`{s}`");
            match m {
                M::First | M::Last | M::Max | M::Min | M::MaxBy | M::MinBy | M::Reduce => {
                    if *m == M::Reduce && !args.is_empty() {
                        return None;
                    }
                    if recv.as_deref().is_some_and(nonempty_const) {
                        return None;
                    }
                    let n = match m {
                        M::First => "first",
                        M::Last => "last",
                        M::Max => "max",
                        M::Min => "min",
                        M::MaxBy => "max_by",
                        M::MinBy => "min_by",
                        _ => "reduce",
                    };
                    fault_of("IndexError", name(n), "panics on an empty collection")
                }
                M::Sum if e.ty != Ty::Float => {
                    let blockless_const = matches!(&e.kind, TK::M(_, Some(r), _, None) if const_range_sum(r).is_some());
                    if blockless_const { None } else { fault_of("ArithError", name("sum"), "may overflow") }
                }
                M::Unwrap => fault_of("IndexError", name("unwrap"), "panics on none"),
                M::ResUnwrap => fault_of("Error", name("unwrap"), "panics on an error"),
                M::Conv(k, wrap) => {
                    let r = recv.as_deref()?;
                    if r.ty == Ty::Float {
                        return fault_of("ArithError", "conversion", "may be NaN or out of range");
                    }
                    if *wrap {
                        return None;
                    }
                    let sk = r.ty.int_kind().unwrap_or(IntKind::I64);
                    let widening = if sk == IntKind::U64 { *k == IntKind::U64 } else if *k == IntKind::U64 { false } else { *k == IntKind::I64 || (k.min() <= sk.min() && k.max() >= sk.max()) };
                    let fits = konst(r).is_some_and(|c| sk != IntKind::U64 && (c as i128) >= k.min() as i128 && (c as i128) <= k.max() as i128);
                    if widening || fits { None } else { fault_of("ArithError", "conversion", "may not fit") }
                }
                M::FloatToI => fault_of("ArithError", "conversion", "may be NaN or out of range"),
                M::Digits => if recv.as_deref().is_some_and(nonneg) { None } else { fault_of("ArithError", name("digits"), "panics on a negative number") },
                M::IntSqrt => if args.first().is_some_and(nonneg) { None } else { fault_of("ArithError", "`Int.sqrt`", "panics on a negative number") },
                M::ArrayNew => if args.first().is_some_and(nonneg) { None } else { fault_of("ArithError", "`Array.new`", "panics on a negative size") },
                M::Step => if args.get(1).and_then(konst).is_some_and(|c| c > 0) { None } else { fault_of("ArithError", name("step"), "panics unless the step is positive") },
                M::PoolGet | M::PoolSet => fault_of("IndexError", "pool slot", "panics on a removed handle"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Blocks lowered as functions of their own (`~` doesn't reach into them).
pub(crate) fn own_function_block(e: &TExpr) -> bool {
    matches!(e.kind, TK::M(M::Lambda | M::Spawn | M::Pmap | M::EnumNew, ..))
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
        let s = self.sm.snippet(e.span);
        let s = s.trim();
        // One pair of enclosing parentheses, if the span has them.
        let s = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(s);
        Diag::new(e.span, format!("unproven arithmetic in `#[pure] def {}`: `{}` may overflow; use `~(...)` or prove a bound", self.f.src_name, s))
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
            TK::Panic(_) => {
                return Err(Diag::new(e.span, format!("`#[pure] def {}` can't panic: `{}`; fail with an error instead (`fail` in a `-> ~T` function)", self.f.src_name, self.sm.snippet(e.span).trim())));
            }
            _ if own_function_block(e) && under_try => {
                // The block runs as its own function: `~` outside doesn't cover it.
                let mut r = Ok(());
                each_child(e, &mut |x| {
                    if r.is_ok() {
                        r = self.expr(x, false);
                    }
                });
                return r;
            }
            TK::Bin(crate::ast::BinOp::Shl | crate::ast::BinOp::Shr, _, c) if !under_try && !matches!(c.kind, TK::Int(n) if n >= 0) && c.ty.int_kind().is_some_and(|k| k.signed()) => {
                return Err(Diag::new(c.span, format!("unproven shift in `#[pure] def {}`: a negative count panics; use a constant or an unsigned count", self.f.src_name)));
            }
            _ if !under_try => {
                if let Some(f) = fault(e) {
                    return Err(Diag::new(e.span, format!("unproven {} in `#[pure] def {}`: `{}` {}; use `~(...)`", f.what, self.f.src_name, self.sm.snippet(e.span).trim(), f.why)));
                }
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

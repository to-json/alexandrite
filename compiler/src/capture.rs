//! Captures by reference (decision R5): a variable that a lambda captures
//! and that changes after it's captured (reassigned, pushed to, indexed
//! into) lives in a *cell*, a one-element slice. Slices share storage, so
//! the lambda and the enclosing function see one variable, as in Go.
//!
//! Runs on the checked program, before region analysis and lowering:
//! every use of the variable becomes a use of `cell[0]`, and the lambda
//! captures the cell itself. `spawn` bodies keep copying what they capture
//! (a task boundary), and so do generators; a variable captured by either
//! isn't converted.

use crate::tast::*;
use std::collections::{HashMap, HashSet};

pub fn convert(p: &mut TProgram) {
    for f in p.funcs.iter_mut().filter(|f| !f.external) {
        convert_fn(f);
    }
}

fn convert_fn(f: &mut TFunc) {
    // Captured by lambdas, and changed somewhere.
    let mut by_lambda: HashSet<LocalId> = f.lambdas.iter().flat_map(|(_, _, caps)| caps.iter().copied()).collect();
    let mut by_task = HashSet::new();
    let mut in_multi = HashSet::new();
    for s in &f.body {
        scan_stmt(s, &mut by_task, &mut in_multi);
    }
    by_lambda.retain(|l| {
        let loc = &f.locals[*l];
        (loc.reassigned > 0 || loc.mutated || loc.pushed) && !by_task.contains(l) && !in_multi.contains(l) && (loc.user || f.params.contains(l)) && zero(&loc.ty).is_some()
    });
    if by_lambda.is_empty() {
        return;
    }
    // A cell per variable.
    let mut cells: HashMap<LocalId, LocalId> = HashMap::new();
    let mut boxed: Vec<LocalId> = by_lambda.into_iter().collect();
    boxed.sort_unstable();
    for l in &boxed {
        let loc = f.locals[*l].clone();
        f.locals.push(Local { name: format!("_cell_{}", loc.name), ty: Ty::arr(loc.ty.clone()), reassigned: 0, mutated: true, pushed: false, user: false });
        cells.insert(*l, f.locals.len() - 1);
    }
    let mut r = Rewriter { cells: &cells, locals: &mut f.locals };
    for s in f.body.iter_mut() {
        r.stmt(s);
    }
    // The lambdas' capture lists name the cells now.
    for (_, _, caps) in f.lambdas.iter_mut() {
        for c in caps.iter_mut() {
            if let Some(cell) = cells.get(c) {
                *c = *cell;
            }
        }
    }
    // Make the cells first thing: parameters start with their argument,
    // other variables with their zero value (they're assigned before use).
    let mut init = vec![];
    for l in &boxed {
        let cell = cells[l];
        let t = f.locals[*l].ty.clone();
        let sp = f.span;
        let first = if f.params.contains(l) { TExpr { kind: TK::Local(*l), ty: t.clone(), span: sp } } else { zero(&t).unwrap() };
        let arr = TExpr { kind: TK::Array(vec![first]), ty: Ty::arr(t.clone()), span: sp };
        init.push(TStmt::Expr(TExpr { kind: TK::Assign(cell, Box::new(arr)), ty: Ty::arr(t), span: sp }));
    }
    init.append(&mut f.body);
    f.body = init;
}

/// A zero value of a type, if it's simple to build here.
fn zero(t: &Ty) -> Option<TExpr> {
    let sp = crate::diag::Span::default();
    let kind = match t {
        Ty::Int | Ty::IntK(_) => TK::Int(0),
        Ty::Float => TK::Float(0.0),
        Ty::Bool => TK::Bool(false),
        Ty::Str => TK::Str(String::new()),
        Ty::Array(_) => TK::Array(vec![]),
        Ty::Opt(_) => TK::None,
        Ty::Map(..) => TK::M(M::MapNew, None, vec![], None),
        Ty::Struct(_, fs) => TK::M(M::StructNew, None, fs.iter().map(|(_, ft)| zero(ft)).collect::<Option<Vec<_>>>()?, None),
        _ => return None,
    };
    Some(TExpr { kind, ty: t.clone(), span: sp })
}

/// Locals captured by `spawn` / generator bodies, and those assigned by
/// multiple assignment (both keep plain variables).
fn scan_stmt(s: &TStmt, by_task: &mut HashSet<LocalId>, multi: &mut HashSet<LocalId>) {
    if let TStmt::MultiAssign(ls, _) = s {
        multi.extend(ls.iter().copied());
    }
    crate::prove::stmt_exprs(s, &mut |e| scan_expr(e, by_task, multi));
}

fn scan_expr(e: &TExpr, by_task: &mut HashSet<LocalId>, multi: &mut HashSet<LocalId>) {
    if let TK::M(M::Spawn | M::EnumNew, _, args, blk) = &e.kind {
        for a in args {
            if let TK::Local(l) = a.kind {
                by_task.insert(l);
            }
        }
        if let Some(b) = blk {
            let mut ls = vec![];
            for s in &b.body {
                crate::lower::collect_locals_stmt(s, &mut ls);
            }
            by_task.extend(ls);
        }
    }
    if let TK::M(_, _, _, Some(b)) = &e.kind {
        for s in &b.body {
            scan_stmt(s, by_task, multi);
        }
    }
    if let TK::Seq(ss) = &e.kind {
        for s in ss {
            scan_stmt(s, by_task, multi);
        }
    }
    crate::prove::each_child(e, &mut |c| scan_expr(c, by_task, multi));
}

struct Rewriter<'a> {
    cells: &'a HashMap<LocalId, LocalId>,
    locals: &'a mut Vec<Local>,
}

impl Rewriter<'_> {
    fn cell_read(&self, cell: LocalId, t: &Ty, sp: crate::diag::Span) -> TExpr {
        let c = TExpr { kind: TK::Local(cell), ty: Ty::arr(t.clone()), span: sp };
        let z = TExpr { kind: TK::Int(0), ty: Ty::Int, span: sp };
        TExpr { kind: TK::Index(Box::new(c), Box::new(z)), ty: t.clone(), span: sp }
    }

    fn stmts(&mut self, ss: &mut [TStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &mut TStmt) {
        match s {
            TStmt::Expr(e) | TStmt::Fail(e, _) | TStmt::Defer(e) => self.expr(e),
            TStmt::MultiAssign(_, es) => es.iter_mut().for_each(|e| self.expr(e)),
            TStmt::While(c, b) => {
                self.expr(c);
                self.stmts(b);
            }
            TStmt::If(c, a, b) => {
                self.expr(c);
                self.stmts(a);
                self.stmts(b);
            }
            TStmt::Break(Some(e), _) | TStmt::Return(Some(e), _) => self.expr(e),
            _ => {}
        }
    }

    fn opt(&mut self, e: &mut Option<Box<TExpr>>) {
        if let Some(e) = e {
            self.expr(e);
        }
    }

    fn expr(&mut self, e: &mut TExpr) {
        let sp = e.span;
        // Children first.
        match &mut e.kind {
            TK::Assign(_, v) | TK::Neg(v) | TK::Not(v) | TK::Try(v) | TK::Puts(v) | TK::Panic(v) | TK::Some(v) => self.expr(v),
            TK::IndexAssign(_, i, v) => {
                self.expr(i);
                self.expr(v);
            }
            TK::Bin(_, a, b) | TK::Range(a, b, _) | TK::Index(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            TK::Ternary(a, b, c) => {
                self.expr(a);
                self.expr(b);
                self.expr(c);
            }
            TK::Slice(a, lo, hi, _) => {
                self.expr(a);
                self.opt(lo);
                self.opt(hi);
            }
            TK::Call(_, args) | TK::Array(args) | TK::Format(_, args) => args.iter_mut().for_each(|a| self.expr(a)),
            TK::M(m, recv, args, blk) => {
                let is_lambda = *m == M::Lambda;
                if !(*m == M::Push && matches!(recv.as_deref(), Some(TExpr { kind: TK::Local(l), .. }) if self.cells.contains_key(l))) {
                    self.opt(recv);
                }
                for a in args.iter_mut() {
                    // A lambda captures the cell, not the value.
                    if is_lambda {
                        if let TK::Local(l) = a.kind {
                            if let Some(c) = self.cells.get(&l) {
                                *a = TExpr { kind: TK::Local(*c), ty: Ty::arr(a.ty.clone()), span: a.span };
                                continue;
                            }
                        }
                    }
                    self.expr(a);
                }
                if let Some(b) = blk {
                    self.stmts(&mut b.body);
                }
            }
            TK::PlaceAssign(_, steps, _, v) | TK::Bang(_, steps, _, v) => {
                for st in steps.iter_mut() {
                    if let TStep::Index(i) = st {
                        self.expr(i);
                    }
                }
                self.expr(v);
            }
            TK::Seq(ss) => self.stmts(ss),
            TK::Select(arms, d) => {
                for a in arms {
                    match a {
                        TSelArm::Recv { ch, body, .. } => {
                            self.expr(ch);
                            self.stmts(body);
                        }
                        TSelArm::Send { ch, val, body } => {
                            self.expr(ch);
                            self.expr(val);
                            self.stmts(body);
                        }
                    }
                }
                if let Some(d) = d {
                    self.stmts(d);
                }
            }
            _ => {}
        }
        // Then this node.
        let new = match &e.kind {
            TK::Local(l) => self.cells.get(l).map(|c| self.cell_read(*c, &e.ty, sp)),
            TK::Assign(l, v) => self.cells.get(l).map(|c| {
                let z = TExpr { kind: TK::Int(0), ty: Ty::Int, span: sp };
                TExpr { kind: TK::IndexAssign(*c, Box::new(z), v.clone()), ty: e.ty.clone(), span: sp }
            }),
            TK::IndexAssign(l, i, v) => self.cells.get(l).map(|c| {
                let z = TExpr { kind: TK::Int(0), ty: Ty::Int, span: sp };
                TExpr { kind: TK::PlaceAssign(*c, vec![TStep::Index(z), TStep::Index((**i).clone())], None, v.clone()), ty: e.ty.clone(), span: sp }
            }),
            TK::PlaceAssign(l, steps, op, v) => self.cells.get(l).map(|c| {
                let z = TExpr { kind: TK::Int(0), ty: Ty::Int, span: sp };
                let mut st = vec![TStep::Index(z)];
                st.extend(steps.iter().cloned());
                TExpr { kind: TK::PlaceAssign(*c, st, *op, v.clone()), ty: e.ty.clone(), span: sp }
            }),
            TK::Bang(l, steps, view, call) => self.cells.get(l).map(|c| {
                let z = TExpr { kind: TK::Int(0), ty: Ty::Int, span: sp };
                let mut st = vec![TStep::Index(z)];
                st.extend(steps.iter().cloned());
                TExpr { kind: TK::Bang(*c, st, *view, call.clone()), ty: e.ty.clone(), span: sp }
            }),
            // `xs << v` on a cell variable: through a temporary, written back.
            TK::M(M::Push, Some(recv), args, None) => match recv.kind {
                TK::Local(l) if self.cells.contains_key(&l) => {
                    let c = self.cells[&l];
                    let t = recv.ty.clone();
                    self.locals.push(Local { name: "_push".into(), ty: t.clone(), reassigned: 1, mutated: true, pushed: true, user: false });
                    let tmp = self.locals.len() - 1;
                    let read = self.cell_read(c, &t, sp);
                    let take = TExpr { kind: TK::Assign(tmp, Box::new(read)), ty: t.clone(), span: sp };
                    let tl = TExpr { kind: TK::Local(tmp), ty: t.clone(), span: sp };
                    let push = TExpr { kind: TK::M(M::Push, Some(Box::new(tl.clone())), args.clone(), None), ty: Ty::Unit, span: sp };
                    let z = TExpr { kind: TK::Int(0), ty: Ty::Int, span: sp };
                    let back = TExpr { kind: TK::IndexAssign(c, Box::new(z), Box::new(tl)), ty: t, span: sp };
                    let unit = TExpr { kind: TK::Unit, ty: Ty::Unit, span: sp };
                    Some(TExpr { kind: TK::Seq(vec![TStmt::Expr(take), TStmt::Expr(push), TStmt::Expr(back), TStmt::Expr(unit)]), ty: Ty::Unit, span: sp })
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(n) = new {
            *e = n;
        }
    }
}

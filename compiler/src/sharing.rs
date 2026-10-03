//! Moves into tasks (decision R4, milestone R6): a value with shared,
//! mutable storage (a slice, a map, a pool, a closure, or anything holding
//! one) that goes into `spawn` or down a channel now belongs to the other
//! side. Using it — or anything sharing its storage — afterwards would be
//! unsynchronized sharing between tasks, so it's a compile error. The way
//! out is a copy (`.dup`), a channel, or (later) a `Mutex`.
//!
//! Runs on the checked program. Aliasing comes from the regions analysis;
//! "afterwards" is flow-sensitive: branches merge, loops run until nothing
//! changes, and assigning a variable again gives it fresh storage.

use crate::diag::{Diag, SourceMap, Span};
use crate::tast::*;
use std::collections::HashMap;

thread_local! {
    /// Per interface: whether any implementor carries shared storage. An
    /// interface not listed (before `check` runs) is assumed to.
    static IFACE_SHARES: std::cell::RefCell<HashMap<String, bool>> = std::cell::RefCell::new(HashMap::new());
}

/// Which interfaces' values carry shared storage: those with an implementor
/// that does (a fixpoint, since implementors may hold interface values).
fn learn_ifaces(p: &TProgram) {
    IFACE_SHARES.with(|m| {
        let mut m = m.borrow_mut();
        m.clear();
        for k in p.ifaces.keys() {
            m.insert(k.clone(), false);
        }
    });
    loop {
        let mut changed = false;
        for (k, imps) in &p.ifaces {
            if IFACE_SHARES.with(|m| m.borrow()[k]) {
                continue;
            }
            if imps.iter().any(|(t, _)| shares(t)) {
                IFACE_SHARES.with(|m| m.borrow_mut().insert(k.clone(), true));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Does a value of this type carry storage another task could mutate?
pub fn shares(t: &Ty) -> bool {
    match t {
        Ty::Iface(n) => IFACE_SHARES.with(|m| m.borrow().get(n).copied().unwrap_or(true)),
        Ty::Array(_) | Ty::Map(..) | Ty::Pool(_) | Ty::Fn(..) => true,
        Ty::Fixed(t, _) | Ty::Opt(t) | Ty::Result(t) => shares(t),
        Ty::Tuple(ts) => ts.iter().any(shares),
        Ty::Struct(_, fs) => fs.iter().any(|(_, t)| shares(t)),
        Ty::Enum(_, vs) => vs.iter().any(|(_, fs)| fs.iter().any(|(_, t)| shares(t))),
        _ => false,
    }
}

pub fn check(p: &TProgram, sm: &SourceMap) -> Result<(), Diag> {
    learn_ifaces(p);
    let al = crate::regions::aliases(p);
    for (f, al) in p.funcs.iter().zip(&al) {
        if f.external {
            continue;
        }
        let mut c = Ck { f, sm, al, moved: HashMap::new(), loops: vec![], err: None };
        c.stmts(&f.body);
        if let Some(e) = c.err {
            return Err(e);
        }
    }
    Ok(())
}

/// Where a variable went: the span of the `spawn` or send, and which.
#[derive(Clone, Copy, PartialEq)]
struct Move {
    at: Span,
    what: &'static str,
    /// The variable named at the move (for messages about its aliases).
    via: LocalId,
}

type State = HashMap<LocalId, Move>;

struct Ck<'a> {
    f: &'a TFunc,
    sm: &'a SourceMap,
    al: &'a HashMap<LocalId, (Vec<LocalId>, Vec<LocalId>)>,
    moved: State,
    /// Per enclosing loop: the states at `break` and at `next`.
    loops: Vec<(Vec<State>, Vec<State>)>,
    err: Option<Diag>,
}

fn merge(a: &mut State, b: &State) {
    for (k, v) in b {
        a.entry(*k).or_insert(*v);
    }
}

impl Ck<'_> {
    fn fail(&mut self, d: Diag) {
        if self.err.is_none() {
            self.err = Some(d);
        }
    }

    fn name(&self, l: LocalId) -> &str {
        self.f.locals[l].name.trim_start_matches("_cell_")
    }

    fn use_of(&mut self, l: LocalId, sp: Span) {
        if let Some(m) = self.moved.get(&l).copied() {
            let what = m.what;
            let msg = if m.at == sp {
                format!("`{}` goes into a task on every iteration of this loop, so each task would share it with the one before", self.name(l))
            } else if m.via == l {
                if what == "moved into a mutex" {
                format!("`{}` was {what} at {}, so it's reached only through the mutex's `lock` now", self.name(l), self.sm.loc(m.at))
            } else {
                format!("`{}` was {what} at {}, so the other task owns it now; using it here would share it without synchronization", self.name(l), self.sm.loc(m.at))
            }
            } else {
                format!("`{}` shares storage with `{}`, which was {what} at {}; using it here would share it without synchronization", self.name(l), self.name(m.via), self.sm.loc(m.at))
            };
            let mut d = Diag::new(sp, msg);
            d.notes.push(format!("give the task its own copy where it's handed over (`{}.dup`), or pass it back through a channel", self.name(m.via)));
            self.fail(d);
        }
    }

    /// `vals` go to another task at `at`.
    fn hand_over(&mut self, vals: &[&TExpr], at: Span, what: &'static str) {
        for v in vals {
            if !shares(&v.ty) {
                continue;
            }
            let mut ls = vec![];
            locals_read(v, &mut ls);
            for l in ls {
                if !shares(&self.f.locals[l].ty) {
                    continue;
                }
                let (comp, callers) = self.al.get(&l).cloned().unwrap_or_default();
                if let Some(p) = callers.first() {
                    let whose = if *p == l { format!("`{}` is the caller's", self.name(l)) } else { format!("`{}` shares storage with the caller's `{}`", self.name(l), self.name(*p)) };
                    let mut d = Diag::new(v.span, format!("{whose}, so handing it to another task would share it without synchronization"));
                    d.notes.push(format!("hand over a copy instead (`{}.dup`)", self.name(l)));
                    self.fail(d);
                    continue;
                }
                for x in comp {
                    if shares(&self.f.locals[x].ty) {
                        self.moved.insert(x, Move { at, what, via: l });
                    }
                }
            }
        }
    }

    fn stmts(&mut self, ss: &[TStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Expr(e) | TStmt::Defer(e) => self.expr(e),
            TStmt::Fail(e, _) => {
                self.expr(e);
                self.moved.clear();
            }
            TStmt::MultiAssign(ls, es) => {
                for e in es {
                    self.expr(e);
                }
                for l in ls {
                    self.moved.remove(l);
                }
            }
            TStmt::If(c, a, b) => {
                self.expr(c);
                let start = self.moved.clone();
                self.stmts(a);
                let after_a = std::mem::replace(&mut self.moved, start);
                self.stmts(b);
                merge(&mut self.moved, &after_a);
            }
            TStmt::While(c, b) => self.looped(|ck| {
                ck.expr(c);
                let exit = ck.moved.clone();
                ck.stmts(b);
                Some(exit)
            }),
            TStmt::Next(_) => {
                if let Some(l) = self.loops.last_mut() {
                    l.1.push(self.moved.clone());
                }
                self.moved.clear();
            }
            TStmt::Break(v, _) => {
                if let Some(v) = v {
                    self.expr(v);
                }
                if let Some(l) = self.loops.last_mut() {
                    l.0.push(self.moved.clone());
                }
                self.moved.clear();
            }
            TStmt::Return(v, _) => {
                if let Some(v) = v {
                    self.expr(v);
                }
                self.moved.clear();
            }
        }
    }

    /// A loop: run the body until the state at its head stops growing. The
    /// body returns the state at the loop's normal exit (None: same as the
    /// head's).
    fn looped(&mut self, mut body: impl FnMut(&mut Self) -> Option<State>) {
        let mut head = self.moved.clone();
        let mut exit;
        loop {
            self.moved = head.clone();
            self.loops.push((vec![], vec![]));
            let e = body(self);
            let (breaks, nexts) = self.loops.pop().unwrap();
            exit = e.unwrap_or_else(|| head.clone());
            for b in &breaks {
                merge(&mut exit, b);
            }
            let mut next_head = head.clone();
            merge(&mut next_head, &self.moved);
            for n in &nexts {
                merge(&mut next_head, n);
            }
            if next_head.len() == head.len() || self.err.is_some() {
                merge(&mut exit, &self.moved);
                break;
            }
            head = next_head;
        }
        self.moved = exit;
    }

    fn expr(&mut self, e: &TExpr) {
        if self.err.is_some() {
            return;
        }
        match &e.kind {
            TK::Local(l) => self.use_of(*l, e.span),
            TK::Assign(l, v) => {
                self.expr(v);
                self.moved.remove(l);
            }
            TK::IndexAssign(l, ..) | TK::PlaceAssign(l, ..) => {
                let mut kids = vec![];
                crate::prove::each_child(e, &mut |c| kids.push(c as *const TExpr));
                for k in kids {
                    self.expr(unsafe { &*k });
                }
                self.use_of(*l, e.span);
            }
            TK::M(M::Spawn, _, caps, blk) => {
                // Capturing is a use; then the captures belong to the task.
                for c in caps {
                    self.expr(c);
                }
                let cs: Vec<&TExpr> = caps.iter().collect();
                self.hand_over(&cs, e.span, "moved into a task");
                // The body is the task's own code.
                if let Some(b) = blk {
                    let saved = std::mem::take(&mut self.moved);
                    let saved_loops = std::mem::take(&mut self.loops);
                    self.stmts(&b.body);
                    self.loops = saved_loops;
                    self.moved = saved;
                }
            }
            TK::M(M::ChanSend, recv, args, _) => {
                if let Some(r) = recv {
                    self.expr(r);
                }
                for a in args {
                    self.expr(a);
                }
                let vs: Vec<&TExpr> = args.iter().collect();
                self.hand_over(&vs, e.span, "sent on a channel");
            }
            TK::M(M::MutexNew, _, args, _) => {
                for a in args {
                    self.expr(a);
                }
                let vs: Vec<&TExpr> = args.iter().collect();
                self.hand_over(&vs, e.span, "moved into a mutex");
            }
            TK::M(M::Lock, recv, _, Some(b)) => {
                if let Some(r) = recv {
                    self.expr(r);
                }
                // Nothing outside the block may keep hold of the guarded value.
                if let Some(v) = b.params.first() {
                    let (comp, _) = self.al.get(v).cloned().unwrap_or_default();
                    // Only a store inside the block can make an outer variable
                    // hold the guarded value.
                    let mut written = vec![];
                    for st in &b.body {
                        crate::prove::stmt_exprs(st, &mut |e| written_locals(e, &mut written));
                    }
                    for x in comp {
                        let inside = (b.own.0..b.own.1).contains(&x) || x == *v;
                        if !inside && written.contains(&x) && shares(&self.f.locals[x].ty) {
                            let mut d = Diag::new(b.span, format!("`{}` would keep a reference into the value `lock` guards, past the end of the block", self.name(x)));
                            d.notes.push(format!("store a copy (`{}.dup`), or return what you need from the block (its value is copied out)", self.name(*v)));
                            self.fail(d);
                        }
                    }
                }
                self.looped(|ck| {
                    ck.stmts(&b.body);
                    None
                });
            }
            TK::M(M::Lambda, _, caps, _) => {
                // Captures are uses; the body runs later, on its own.
                for c in caps {
                    self.expr(c);
                }
            }
            TK::M(_, recv, args, Some(b)) => {
                if let Some(r) = recv {
                    self.expr(r);
                }
                for a in args {
                    self.expr(a);
                }
                // A block runs any number of times, maybe none.
                self.looped(|ck| {
                    ck.stmts(&b.body);
                    None
                });
            }
            TK::Ternary(c, a, b) => {
                self.expr(c);
                let start = self.moved.clone();
                self.expr(a);
                let after_a = std::mem::replace(&mut self.moved, start);
                self.expr(b);
                merge(&mut self.moved, &after_a);
            }
            TK::Seq(ss) => self.stmts(ss),
            TK::Select(arms, d) => {
                let start = self.moved.clone();
                let mut out = State::new();
                for a in arms {
                    self.moved = start.clone();
                    match a {
                        TSelArm::Recv { ch, body, .. } => {
                            self.expr(ch);
                            self.stmts(body);
                        }
                        TSelArm::Send { ch, val, body } => {
                            self.expr(ch);
                            self.expr(val);
                            self.hand_over(&[val], val.span, "sent on a channel");
                            self.stmts(body);
                        }
                    }
                    merge(&mut out, &self.moved);
                }
                self.moved = start;
                if let Some(d) = d {
                    self.stmts(d);
                }
                merge(&mut self.moved, &out);
            }
            _ => {
                let mut kids = vec![];
                crate::prove::each_child(e, &mut |c| kids.push(c as *const TExpr));
                for k in kids {
                    // Children borrow from `e`, which outlives this call.
                    self.expr(unsafe { &*k });
                }
            }
        }
    }
}

/// The locals an expression reads (not inside blocks).
fn locals_read(e: &TExpr, out: &mut Vec<LocalId>) {
    if let TK::Local(l) = e.kind {
        out.push(l);
        return;
    }
    crate::prove::each_child(e, &mut |c| locals_read(c, out));
}

/// The locals an expression assigns or stores into (blocks included).
fn written_locals(e: &TExpr, out: &mut Vec<LocalId>) {
    match &e.kind {
        TK::Assign(l, _) | TK::IndexAssign(l, ..) | TK::PlaceAssign(l, ..) => out.push(*l),
        TK::M(M::Push | M::MapSet | M::CopyInto | M::PoolAdd | M::PoolSet, Some(r), ..) => {
            if let TK::Local(l) = r.kind {
                out.push(l);
            }
        }
        _ => {}
    }
    if let TK::M(_, _, _, Some(b)) = &e.kind {
        for s in &b.body {
            crate::prove::stmt_exprs(s, &mut |x| written_locals(x, out));
        }
    }
    if let TK::Seq(ss) = &e.kind {
        for s in ss {
            crate::prove::stmt_exprs(s, &mut |x| written_locals(x, out));
        }
    }
    crate::prove::each_child(e, &mut |c| written_locals(c, out));
}

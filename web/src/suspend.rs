//! Which functions can suspend a task, and the rewrite that puts every
//! suspension at statement level, where the backend can save a frame and
//! come back to it (see `sched.rs` for the protocol).
//!
//! A function *suspends* if it blocks (wait, send, recv, select, lock,
//! sleep) or calls one that does. In a suspending function, after `prepare`:
//!   - a call that can suspend is a statement of its own:
//!     `Set(v, Call(f, args))` or `Eval(Call(f, args))`, args free of such calls;
//!   - a sleep is `Eval(Ffi(sleep, [vars]))`;
//!   - a blocking statement's operands are variables or constants (a
//!     resumed task runs the operation again, so they must not be re-evaluated).
//! Whatever was evaluated before a hoisted call is hoisted with it, in
//! order, so evaluation order is unchanged; `&&`, `||` and conditional
//! expressions become `If` statements so short-circuiting is too.

use crate::wasmgen::{ty_of, Tys};
use alx::lir::*;
use std::collections::{HashMap, HashSet};

/// The externs the browser provides (anything else is refused).
pub const BROWSER_FFI: &[&str] = &["alx_wall_ns", "alx_mono_ns", "alx_sleep_ns", "alx_local_offset", "alx_local_zone"];

pub struct Info {
    /// Functions (by name) that can suspend.
    pub funcs: HashSet<String>,
    /// Spawned workers that can suspend.
    pub workers: HashSet<usize>,
    /// Workers started by `Spawn` (the entries `run_task` dispatches to).
    pub spawned: Vec<usize>,
    /// Workers `pmap` runs (reachable ones), sorted.
    pub pmapped: Vec<usize>,
    /// Each pmap worker's element result type and, for a fallible one, its Result type.
    pub pmap_tys: HashMap<usize, (LTy, Option<LTy>)>,
    /// The extern index of `alx_sleep_ns`.
    pub sleep: Option<usize>,
    /// The program runs under the scheduler: it spawns or blocks, or (with
    /// threads) it uses `pmap`, which the other threads help with.
    pub sched: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Node {
    F(String),
    W(usize),
    G(usize),
}

pub fn kids(e: &mut LE) -> Vec<&mut LE> {
    match e {
        LE::Tup(_, v) | LE::Call(_, v) | LE::Ffi(_, v) | LE::Rt(_, v) | LE::Prim(_, v) | LE::ArrLit(_, v) | LE::GenNew(_, v) => v.iter_mut().collect(),
        LE::Field(x, _) | LE::Neg(x, _) | LE::FNeg(x) | LE::Not(x) | LE::Len(x) | LE::RangeField(x, _) | LE::ToP(x) | LE::ChanNew(_, x) | LE::ChanLen(x) | LE::AtomicNew(x) | LE::AtomicLoad(x) | LE::RegionOf(x) | LE::RegionNew(x) | LE::RegionBytes(x) | LE::ArrWithCap(_, x) => vec![&mut **x],
        LE::Arith(_, a, b, _) | LE::PArith(_, a, b) | LE::Cmp(_, a, b, _) | LE::FArith(_, a, b) | LE::AtomicRmw(_, a, b) | LE::Range(a, b, _) | LE::ArrNew(_, a, b, _) => vec![&mut **a, &mut **b],
        LE::Index { arr, idx, .. } => vec![&mut **arr, &mut **idx],
        LE::Cond(a, b, c) | LE::Slice(_, a, b, c) | LE::AtomicCas(a, b, c) => vec![&mut **a, &mut **b, &mut **c],
        LE::Var(_) | LE::I(_) | LE::F(_) | LE::B(_) | LE::S(_) | LE::SB(_) | LE::Loc(_) | LE::Unit | LE::LockNew | LE::NullTask(_) | LE::RegionProgram | LE::Global(_) => vec![],
    }
}

/// A statement's own expressions, in evaluation order (not nested blocks').
fn exprs(s: &mut LS) -> Vec<&mut LE> {
    match s {
        LS::Set(_, e) | LS::Push(_, e) | LS::Eval(e) | LS::Return(Some(e)) | LS::Yield(e) | LS::Puts(e, _) | LS::Print(e) | LS::PanicStr(e) | LS::Exit(e) | LS::Die(e) | LS::SetGlobal(_, e) | LS::RegionFree(e) | LS::Lock(e) | LS::Unlock(e) => vec![e],
        LS::Wait { task: e, .. } | LS::ChanRecv { ch: e, .. } | LS::ChanClose { ch: e, .. } | LS::NextOrBreak { source: e, .. } | LS::Pmap { arr: e, .. } | LS::Spawn { env: e, .. } | LS::RegionUse { region: e, .. } => vec![e],
        LS::If(c, _, _) => vec![c],
        LS::SetIndex { idx, val, .. } => vec![idx, val],
        LS::SetPlace { steps, val, .. } => {
            let mut v = vec![val];
            for st in steps {
                if let Step::Index(i, _) = st {
                    v.push(i);
                }
            }
            v
        }
        LS::ChanSend { ch, val, .. } => vec![ch, val],
        LS::AtomicStore(a, b) => vec![a, b],
        LS::Select { cases, .. } => cases
            .iter_mut()
            .flat_map(|c| match c {
                SelCase::Send { ch, val } => vec![ch, val],
                SelCase::Recv { ch, .. } => vec![ch],
            })
            .collect(),
        LS::Return(None) | LS::Loop(..) | LS::Break(_) | LS::Continue(_) | LS::Panic(..) | LS::RegionEnter { .. } | LS::RegionExit { .. } | LS::RegionRestore(_) | LS::SortInPlace(..) => vec![],
    }
}

fn blocks(s: &mut LS) -> Vec<&mut Vec<LS>> {
    match s {
        LS::If(_, a, b) => vec![a, b],
        LS::Loop(_, b) => vec![b],
        _ => vec![],
    }
}

/// A statement that blocks the task (a suspension point of its own).
pub fn blocking(s: &LS) -> bool {
    matches!(s, LS::Wait { .. } | LS::ChanSend { .. } | LS::ChanRecv { .. } | LS::Select { .. } | LS::Lock(_))
}

fn each_expr(e: &mut LE, f: &mut impl FnMut(&LE)) {
    f(e);
    for k in kids(e) {
        each_expr(k, f);
    }
}

fn each_stmt(ss: &mut [LS], f: &mut impl FnMut(&mut LS)) {
    for s in ss {
        f(s);
        for b in blocks(s) {
            each_stmt(b, f);
        }
    }
}

/// What a body uses: callees and whether it blocks or needs C.
#[derive(Default)]
struct Uses {
    edges: Vec<Node>,
    blocks: bool,
    spawns: Vec<usize>,
    pmaps: Vec<usize>,
    /// Something only the C library has (named for the error).
    c_only: bool,
}

fn uses(body: &mut [LS], externs: &[FfiSig], sleep: Option<usize>) -> Uses {
    let mut u = Uses::default();
    each_stmt(body, &mut |s| {
        if blocking(s) {
            u.blocks = true;
        }
        match s {
            LS::Spawn { worker, .. } => {
                u.edges.push(Node::W(*worker));
                u.spawns.push(*worker);
            }
            LS::Pmap { worker, .. } => {
                u.edges.push(Node::W(*worker));
                u.pmaps.push(*worker);
            }
            _ => {}
        }
        for e in exprs(s) {
            each_expr(e, &mut |e| match e {
                LE::Call(f, _) => u.edges.push(Node::F(f.clone())),
                LE::GenNew(g, _) => u.edges.push(Node::G(*g)),
                LE::Ffi(i, _) => {
                    if Some(*i) == sleep {
                        u.blocks = true;
                    }
                    if !BROWSER_FFI.contains(&externs[*i].sym.as_str()) {
                        u.c_only = true;
                    }
                }
                LE::Rt(Rt::Errno | Rt::Strerror | Rt::StrFromPtr, _) => u.c_only = true,
                _ => {}
            });
        }
    });
    u
}

/// Analyze the program, refuse what the browser can't run, and rewrite
/// every suspending function (see the module comment).
pub fn prepare(p: &mut LProgram) -> Result<Info, String> {
    let sleep = p.externs.iter().position(|x| x.sym == "alx_sleep_ns");
    let externs = p.externs.clone();
    let mut graph: HashMap<Node, Uses> = HashMap::new();
    let mut main = None;
    for f in &mut p.funcs {
        if f.is_main {
            main = Some(Node::F(f.name.clone()));
        }
        graph.insert(Node::F(f.name.clone()), uses(&mut f.body, &externs, sleep));
    }
    for w in &mut p.workers {
        graph.insert(Node::W(w.id), uses(&mut w.func.body, &externs, sleep));
    }
    for g in &mut p.gens {
        graph.insert(Node::G(g.id), uses(&mut g.func.body, &externs, sleep));
    }
    let main = main.ok_or("no main")?;
    // Reachable from main.
    let mut seen: HashSet<Node> = HashSet::new();
    let mut todo = vec![main.clone()];
    while let Some(n) = todo.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Some(u) = graph.get(&n) {
            todo.extend(u.edges.iter().cloned());
        }
    }
    if seen.iter().any(|n| graph.get(n).is_some_and(|u| u.c_only)) {
        return Err("`extern def` and the C library aren't available in the browser".into());
    }
    // Suspends: blocks, or calls something that suspends (not through a
    // spawn: the new task suspends on its own).
    let mut susp: HashSet<Node> = graph.iter().filter(|(_, u)| u.blocks).map(|(n, _)| n.clone()).collect();
    loop {
        let more: Vec<Node> = graph
            .iter()
            .filter(|(n, u)| !susp.contains(*n) && u.edges.iter().any(|e| susp.contains(e) && !matches!(e, Node::W(w) if u.spawns.contains(w))))
            .map(|(n, _)| n.clone())
            .collect();
        if more.is_empty() {
            break;
        }
        susp.extend(more);
    }
    let pmapped: HashSet<usize> = seen.iter().filter_map(|n| graph.get(n)).flat_map(|u| u.pmaps.iter().copied()).collect();
    for n in &seen {
        match n {
            Node::G(_) if susp.contains(n) => return Err("blocking (channels, wait, lock, sleep) inside a generator isn't available in the browser yet".into()),
            Node::W(w) if susp.contains(n) && pmapped.contains(w) => return Err("blocking (channels, wait, lock, sleep) inside `pmap` isn't available in the browser".into()),
            _ => {}
        }
    }
    let mut spawned: Vec<usize> = seen.iter().filter_map(|n| graph.get(n)).flat_map(|u| u.spawns.iter().copied()).collect();
    spawned.sort();
    spawned.dedup();
    let funcs: HashSet<String> = susp.iter().filter_map(|n| if let Node::F(f) = n { Some(f.clone()) } else { None }).collect();
    let workers: HashSet<usize> = susp.iter().filter_map(|n| if let Node::W(w) = n { Some(*w) } else { None }).collect();
    let mut pmapped_v: Vec<usize> = pmapped.iter().copied().collect();
    pmapped_v.sort();
    let sched = !spawned.is_empty() || susp.contains(&main) || (cfg!(target_feature = "atomics") && !pmapped_v.is_empty());

    let mut pmap_tys = HashMap::new();
    let bodies = p.funcs.iter_mut().chain(p.workers.iter_mut().map(|w| &mut w.func)).chain(p.gens.iter_mut().map(|g| &mut g.func));
    for f in bodies {
        let vars = &f.vars;
        each_stmt(&mut f.body, &mut |s| {
            if let LS::Pmap { dst, worker, err, .. } = s {
                let LTy::Arr(out) = &vars[*dst].ty else { unreachable!() };
                pmap_tys.insert(*worker, ((**out).clone(), err.map(|e| vars[e].ty.clone())));
            }
        });
    }
    let tys = Tys::new(p);
    for f in &mut p.funcs {
        if funcs.contains(&f.name) {
            flatten(f, &funcs, sleep, &tys);
        }
    }
    for w in &mut p.workers {
        if workers.contains(&w.id) {
            flatten(&mut w.func, &funcs, sleep, &tys);
        }
    }
    Ok(Info { funcs, workers, spawned, pmapped: pmapped_v, pmap_tys, sleep, sched })
}

fn flatten(f: &mut LFunc, susp: &HashSet<String>, sleep: Option<usize>, tys: &Tys) {
    let body = std::mem::take(&mut f.body);
    let mut fl = Fl { susp, sleep, tys, vars: &mut f.vars };
    f.body = fl.block(body);
}

struct Fl<'a> {
    susp: &'a HashSet<String>,
    sleep: Option<usize>,
    tys: &'a Tys,
    vars: &'a mut Vec<LVar>,
}

fn trivial(e: &LE) -> bool {
    matches!(e, LE::Var(_) | LE::I(_) | LE::F(_) | LE::B(_) | LE::S(_) | LE::SB(_) | LE::Loc(_) | LE::Unit | LE::Global(_))
}

impl Fl<'_> {
    fn fresh(&mut self, t: LTy) -> V {
        self.vars.push(LVar { name: format!("sus{}", self.vars.len()), ty: t });
        self.vars.len() - 1
    }

    fn ty(&self, e: &LE) -> LTy {
        ty_of(self.tys, self.vars, e)
    }

    /// A call that can suspend (the expression itself, not inside it).
    fn susp_call(&self, e: &LE) -> bool {
        match e {
            LE::Call(f, _) => self.susp.contains(f),
            LE::Ffi(i, _) => Some(*i) == self.sleep,
            _ => false,
        }
    }

    fn has(&self, e: &mut LE) -> bool {
        self.susp_call(e) || kids(e).into_iter().any(|k| self.has(k))
    }

    /// Evaluate `e` into a fresh variable first (unless it's trivial).
    fn spill(&mut self, e: &mut LE, out: &mut Vec<LS>) {
        if !trivial(e) {
            let v = self.fresh(self.ty(e));
            out.push(LS::Set(v, std::mem::replace(e, LE::Var(v))));
        }
    }

    /// Move every suspending call out of `e` into statements on `out`.
    fn hoist(&mut self, e: &mut LE, out: &mut Vec<LS>) {
        if !self.has(e) {
            return;
        }
        if self.susp_call(e) {
            let is_ffi = matches!(e, LE::Ffi(..));
            let (LE::Call(_, args) | LE::Ffi(_, args)) = e else { unreachable!() };
            self.hoist_list(args.iter_mut().collect(), out);
            if is_ffi {
                for a in args.iter_mut() {
                    self.spill(a, out);
                }
            }
            let t = self.ty(e);
            let call = std::mem::replace(e, LE::Unit);
            if t == LTy::Unit {
                out.push(LS::Eval(call));
            } else {
                let v = self.fresh(t);
                out.push(LS::Set(v, call));
                *e = LE::Var(v);
            }
            return;
        }
        match e {
            LE::Cond(c, a, b) => {
                self.hoist(c, out);
                if self.has(a) || self.has(b) {
                    let v = self.fresh(self.ty(a));
                    let (mut ta, mut tb) = (vec![], vec![]);
                    self.hoist(a, &mut ta);
                    ta.push(LS::Set(v, std::mem::replace(&mut **a, LE::Unit)));
                    self.hoist(b, &mut tb);
                    tb.push(LS::Set(v, std::mem::replace(&mut **b, LE::Unit)));
                    out.push(LS::If(std::mem::replace(&mut **c, LE::Unit), ta, tb));
                    *e = LE::Var(v);
                }
            }
            LE::Cmp(op @ (Op::And | Op::Or), a, b, _) => {
                let and = *op == Op::And;
                self.hoist(a, out);
                if self.has(b) {
                    let v = self.fresh(LTy::Bool);
                    let mut tb = vec![];
                    self.hoist(b, &mut tb);
                    tb.push(LS::Set(v, std::mem::replace(&mut **b, LE::Unit)));
                    let short = vec![LS::Set(v, LE::B(!and))];
                    let c = std::mem::replace(&mut **a, LE::Unit);
                    out.push(if and { LS::If(c, tb, short) } else { LS::If(c, short, tb) });
                    *e = LE::Var(v);
                }
            }
            _ => self.hoist_list(kids(e), out),
        }
    }

    /// Hoist out of operands evaluated left to right: everything before the
    /// last operand that suspends is evaluated first, in order.
    fn hoist_list(&mut self, mut ks: Vec<&mut LE>, out: &mut Vec<LS>) {
        let Some(last) = ks.iter_mut().rposition(|k| self.has(k)) else { return };
        for (i, k) in ks.into_iter().enumerate().take(last + 1) {
            self.hoist(k, out);
            if i < last {
                self.spill(k, out);
            }
        }
    }

    fn block(&mut self, ss: Vec<LS>) -> Vec<LS> {
        let mut out = vec![];
        for mut s in ss {
            for b in blocks(&mut s) {
                let inner = std::mem::take(b);
                *b = self.block(inner);
            }
            let call_stmt = matches!(&s, LS::Set(_, e) | LS::Eval(e) if self.susp_call(e));
            if call_stmt {
                let (LS::Set(_, e) | LS::Eval(e)) = &mut s else { unreachable!() };
                let is_ffi = matches!(e, LE::Ffi(..));
                let (LE::Call(_, args) | LE::Ffi(_, args)) = e else { unreachable!() };
                self.hoist_list(args.iter_mut().collect(), &mut out);
                if is_ffi {
                    for a in args.iter_mut() {
                        self.spill(a, &mut out);
                    }
                }
            } else {
                let es = exprs(&mut s);
                self.hoist_list(es, &mut out);
                if blocking(&s) {
                    for e in exprs(&mut s) {
                        self.spill(e, &mut out);
                    }
                }
            }
            out.push(s);
        }
        out
    }
}

//! Region placement (R1): where each allocation lives.
//!
//! Every function call runs with a *frame region* of its own, freed when
//! it returns. Its result is allocated in the region that was current when
//! it was called (the caller chooses that region), and anything stored
//! where it can outlive the call goes to the thread's program region.
//!
//! This module decides, for every expression that may allocate, which of
//! the three it must use:
//!
//! - `Local`: nothing allocated there can outlive the call;
//! - `Ret`: it may flow to the function's result;
//! - `Global`: it may be stored somewhere longer-lived: into a parameter's
//!   storage, a channel, a task, a generator, an unknown callee.
//!
//! The analysis is a flow graph per function: nodes are locals, allocation
//! sites (expressions), the result (`RET`) and everything longer-lived
//! (`GLOBAL`). An edge `a -> b` means "what `a` refers to must live as long
//! as `b`". A site's class is the strongest sink it reaches. Calls use the
//! callee's summary (does parameter i reach the result? anything longer-
//! lived?), solved to a fixpoint over the whole program. Anything not
//! understood is treated as escaping, so the analysis errs toward keeping
//! memory, never toward freeing it early.

use crate::tast::*;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    Local,
    Ret,
    Global,
}

/// Where one allocation goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    /// The region of one iteration of the loop with this key (a `while`
    /// statement or an `each`/`loop` call, by address).
    Iter(usize),
    Frame,
    Ret,
    /// Stored into the storage behind parameter p (and nothing longer-lived):
    /// allocated in the region of the container it's stored into (R2).
    Into(LocalId),
    Global,
}

/// Placement for one function: allocation sites (by address) to places,
/// and the loops that have iteration regions.
#[derive(Default, Debug)]
pub struct FnPlacement {
    pub sites: HashMap<usize, Place>,
    pub loops: std::collections::HashSet<usize>,
    /// Some site is stored into a parameter's storage.
    pub into: bool,
}

/// Per function: does parameter i flow to the result, or escape?
#[derive(Clone, Default, PartialEq, Debug)]
struct Summary {
    to_ret: Vec<bool>,
    to_global: Vec<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Node {
    Local(LocalId),
    Site(usize),
    Ret,
    Global,
    /// The caller's storage behind parameter `l` (writing into it outlives the call).
    Caller(LocalId),
}

fn site(e: &TExpr) -> Node {
    Node::Site(e as *const TExpr as usize)
}

/// Could evaluating this expression allocate (directly, not counting its
/// children)?
pub fn allocates(e: &TExpr, promote: bool) -> bool {
    match &e.kind {
        TK::Array(_) | TK::Format(..) | TK::Call(..) => true,
        // Strings concatenate; Ints are bignums in promote mode.
        TK::Bin(..) | TK::Neg(_) => e.ty == Ty::Str || (promote && e.ty == Ty::Int),
        TK::M(m, ..) => !matches!(
            m,
            // Scalars, reads, and terminals that produce an element or a number
            // (their blocks' allocations are sites of their own).
            M::TupleGet(_) | M::OptPresent | M::OptGet | M::Unwrap | M::EnumTag | M::Size | M::MapSize | M::MapHas | M::ChanLen | M::ResIsOk | M::ErrIs(_) | M::ErrAs(_)
                | M::Even | M::Odd | M::IntSqrt | M::ToF | M::FloatToI | M::Conv(..) | M::FloatAbs | M::Sqrt | M::NowNs
                | M::Sum | M::Max | M::Min | M::MaxBy | M::MinBy | M::Count | M::All | M::Any | M::Include | M::First | M::Last | M::Find | M::Each | M::Loop | M::Step
                | M::ChanClose | M::CapBegin | M::Exit
        ) || (promote && e.ty == Ty::Int),
        _ => false,
    }
}

struct Graph<'a> {
    f: &'a TFunc,
    sums: &'a [Summary],
    edges: HashMap<Node, Vec<Node>>,
    sites: Vec<usize>,
    /// The loops (innermost last) around the walk's current point, and
    /// each site's.
    loops: Vec<usize>,
    site_loops: HashMap<usize, Vec<usize>>,
    /// Each loop's body, for deciding which locals are fresh per iteration.
    loop_bodies: HashMap<usize, Vec<&'a TStmt>>,
}

impl<'a> Graph<'a> {
    fn edge(&mut self, a: Node, b: Node) {
        if a != b {
            self.edges.entry(a).or_default().push(b);
        }
    }
    /// `to` now refers to the same storage as `from` (an alias): what is
    /// stored through either must live as long as both.
    fn alias(&mut self, from: &[Node], to: &[Node]) {
        self.flow(from, to);
        self.flow(to, from);
    }
    fn flow(&mut self, from: &[Node], to: &[Node]) {
        for a in from {
            for b in to {
                self.edge(*a, *b);
            }
        }
    }

    /// The nodes an expression's value may refer to (walking it, adding
    /// the edges its effects create). A value with no storage (an Int, a
    /// Bool) refers to nothing.
    fn expr(&mut self, e: &TExpr) -> Vec<Node> {
        let v = self.expr_nodes(e);
        let promote = self.f.overflow == crate::ast::Overflow::Promote;
        if has_storage(&e.ty) || (promote && contains_int(&e.ty)) { v } else { vec![] }
    }

    fn expr_nodes(&mut self, e: &TExpr) -> Vec<Node> {
        let mut v = vec![];
        if allocates(e, self.f.overflow == crate::ast::Overflow::Promote) {
            let s = site(e);
            self.sites.push(e as *const TExpr as usize);
            self.site_loops.insert(e as *const TExpr as usize, self.loops.clone());
            v.push(s);
        }
        match &e.kind {
            TK::Local(l) => v.push(Node::Local(*l)),
            TK::Assign(l, x) => {
                let xv = self.expr(x);
                self.alias(&xv, &[Node::Local(*l)]);
                v.extend(xv);
            }
            TK::IndexAssign(l, i, x) => {
                self.expr(i);
                let xv = self.expr(x);
                self.flow(&xv, &[Node::Local(*l)]);
            }
            TK::PlaceAssign(l, steps, _, x) => {
                for st in steps {
                    if let TStep::Index(i) = st {
                        self.expr(i);
                    }
                }
                let xv = self.expr(x);
                self.flow(&xv, &[Node::Local(*l)]);
            }
            TK::Call(fid, args) => {
                let sum = self.sums.get(*fid).cloned().unwrap_or_default();
                for (i, a) in args.iter().enumerate() {
                    let av = self.expr(a);
                    let to_global = sum.to_global.get(i).copied().unwrap_or(true);
                    let to_ret = sum.to_ret.get(i).copied().unwrap_or(true);
                    if to_global {
                        self.flow(&av, &[Node::Global]);
                    }
                    if to_ret {
                        v.extend(av);
                    }
                }
            }
            TK::M(m, recv, args, blk) => {
                let rv = recv.as_ref().map(|r| self.expr(r)).unwrap_or_default();
                let per_arg: Vec<Vec<Node>> = args.iter().map(|a| self.expr(a)).collect();
                let avs: Vec<Node> = per_arg.iter().flatten().copied().collect();
                match m {
                    // Stored where it outlives the call.
                    M::ChanSend | M::Yield | M::Spawn | M::EnumNew | M::FnCall | M::IfaceCall(_) | M::Pmap => {
                        self.flow(&rv, &[Node::Global]);
                        self.flow(&avs, &[Node::Global]);
                    }
                    // Mutations of the receiver: what's stored (and any growth)
                    // lives as long as the receiver.
                    M::Push | M::MapSet | M::MapDel | M::CopyInto => {
                        let mut stored = avs.clone();
                        stored.push(site(e));
                        self.flow(&stored, &rv);
                        if *m == M::CopyInto && per_arg.len() >= 2 {
                            // copy(dst, src): src's elements into dst.
                            let (d, s) = (per_arg[0].clone(), per_arg[1].clone());
                            self.flow(&s, &d);
                        }
                    }
                    _ => {}
                }
                if let Some(b) = blk {
                    // Block parameters refer to the receiver's elements (and
                    // to the accumulator, for reduce-like methods).
                    let params: Vec<Node> = b.params.iter().map(|p| Node::Local(*p)).collect();
                    let mut src = rv.clone();
                    src.extend(avs.iter().copied());
                    self.alias(&src, &params);
                    if matches!(m, M::Spawn | M::EnumNew) {
                        // Everything the body touches outlives the caller.
                        let mut inner = vec![];
                        for s in &b.body {
                            inner.extend(self.stmt(s));
                        }
                        let mut locals = vec![];
                        for s in &b.body {
                            crate::lower::collect_locals_stmt(s, &mut locals);
                        }
                        let ls: Vec<Node> = locals.into_iter().map(Node::Local).collect();
                        self.flow(&ls, &[Node::Global]);
                        self.flow(&inner, &[Node::Global]);
                    } else {
                        // `each` / `loop` / `for`: a loop whose iterations may get regions.
                        let is_loop = matches!(m, M::Each | M::Loop);
                        if is_loop {
                            self.loops.push(e as *const TExpr as usize);
                            self.loop_bodies.insert(e as *const TExpr as usize, b.body.iter().map(|s| unsafe_stmt(s)).collect());
                        }
                        for s in &b.body {
                            let sv = self.stmt(s);
                            v.extend(sv);
                        }
                        if is_loop {
                            self.loops.pop();
                        }
                    }
                }
                v.extend(rv);
                v.extend(avs);
            }
            TK::Seq(ss) => {
                for s in ss {
                    v.extend(self.stmt(s));
                }
            }
            TK::Select(arms, d) => {
                for a in arms {
                    match a {
                        TSelArm::Recv { ch, bind, body } => {
                            let cv = self.expr(ch);
                            if let Some(l) = bind {
                                self.alias(&cv, &[Node::Local(*l)]);
                            }
                            for s in body {
                                self.stmt(s);
                            }
                        }
                        TSelArm::Send { ch, val, body } => {
                            self.expr(ch);
                            let vv = self.expr(val);
                            self.flow(&vv, &[Node::Global]);
                            for s in body {
                                self.stmt(s);
                            }
                        }
                    }
                }
                for s in d.iter().flatten() {
                    self.stmt(s);
                }
            }
            _ => {
                let mut kids = vec![];
                crate::prove::each_child(e, &mut |c| kids.push(c as *const TExpr));
                for c in kids {
                    // SAFETY-free: `c` points into `e`, which outlives this call.
                    let c = unsafe_ref(c);
                    v.extend(self.expr(c));
                }
            }
        }
        v
    }

    /// Walk a statement; returns the nodes of its value (an expression
    /// statement's), for block results.
    fn stmt(&mut self, s: &TStmt) -> Vec<Node> {
        match s {
            TStmt::Expr(e) => self.expr(e),
            TStmt::MultiAssign(ls, es) => {
                let mut all = vec![];
                for e in es {
                    all.extend(self.expr(e));
                }
                let targets: Vec<Node> = ls.iter().map(|l| Node::Local(*l)).collect();
                self.alias(&all, &targets);
                vec![]
            }
            TStmt::While(c, b) => {
                self.expr(c);
                let key = s as *const TStmt as usize;
                self.loops.push(key);
                self.loop_bodies.insert(key, b.iter().map(|s| unsafe_stmt(s)).collect());
                for s in b {
                    self.stmt(s);
                }
                self.loops.pop();
                vec![]
            }
            TStmt::If(c, a, b) => {
                self.expr(c);
                let mut v = vec![];
                for s in a.iter().chain(b) {
                    v.extend(self.stmt(s));
                }
                v
            }
            TStmt::Return(Some(e), _) => {
                let v = self.expr(e);
                self.flow(&v, &[Node::Ret]);
                vec![]
            }
            // Errors are copied out when they leave the function.
            TStmt::Fail(e, _) | TStmt::Break(Some(e), _) | TStmt::Defer(e) => {
                self.expr(e);
                vec![]
            }
            _ => vec![],
        }
    }

    /// The strongest sink each node reaches.
    fn classes(&self) -> HashMap<Node, Class> {
        // Reverse reachability from the sinks.
        let mut rev: HashMap<Node, Vec<Node>> = HashMap::new();
        for (a, bs) in &self.edges {
            for b in bs {
                rev.entry(*b).or_default().push(*a);
            }
        }
        let mut out: HashMap<Node, Class> = HashMap::new();
        let mut sinks = vec![(Node::Global, Class::Global), (Node::Ret, Class::Ret)];
        sinks.extend(self.f.params.iter().map(|p| (Node::Caller(*p), Class::Global)));
        sinks.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        for (sink, class) in sinks {
            let mut stack = vec![sink];
            while let Some(n) = stack.pop() {
                let cur = out.get(&n).copied().unwrap_or(Class::Local);
                if cur >= class && n != sink {
                    continue;
                }
                out.insert(n, cur.max(class));
                if let Some(ps) = rev.get(&n) {
                    stack.extend(ps.iter().copied());
                }
            }
        }
        out
    }
}

/// Does the type contain an Int (a bignum, in promote mode)?
fn contains_int(t: &Ty) -> bool {
    match t {
        Ty::Int => true,
        Ty::Opt(t) => contains_int(t),
        Ty::Tuple(ts) => ts.iter().any(contains_int),
        Ty::Struct(_, fs) => fs.iter().any(|(_, t)| contains_int(t)),
        _ => false,
    }
}

/// Can a value of this type refer to heap storage?
pub fn has_storage(t: &Ty) -> bool {
    match t {
        Ty::Int | Ty::IntK(_) | Ty::Float | Ty::Bool | Ty::Unit | Ty::Range | Ty::Never => false,
        Ty::Opt(t) => has_storage(t),
        Ty::Tuple(ts) => ts.iter().any(has_storage),
        Ty::Struct(_, fs) => fs.iter().any(|(_, t)| has_storage(t)),
        Ty::Enum(_, vs) => vs.iter().any(|(_, fs)| fs.iter().any(|(_, t)| has_storage(t))),
        _ => true,
    }
}

fn unsafe_stmt<'b>(s: &TStmt) -> &'b TStmt {
    // Statements of the function being analyzed, which outlives the graph.
    unsafe { &*(s as *const TStmt) }
}

fn unsafe_ref<'b>(p: *const TExpr) -> &'b TExpr {
    // The children collected by `each_child` borrow from the tree being
    // walked, which outlives the walk.
    unsafe { &*p }
}

fn graph<'a>(f: &'a TFunc, sums: &'a [Summary]) -> Graph<'a> {
    let mut g = Graph { f, sums, edges: HashMap::new(), sites: vec![], loops: vec![], site_loops: HashMap::new(), loop_bodies: HashMap::new() };
    // Writing into a parameter's storage writes into the caller's objects.
    for p in &f.params {
        g.edge(Node::Local(*p), Node::Caller(*p));
    }
    let n = f.body.len();
    for (i, s) in f.body.iter().enumerate() {
        let v = g.stmt(s);
        // The last expression is the result.
        if i + 1 == n && !f.is_main && f.ret != Ty::Unit {
            g.flow(&v, &[Node::Ret]);
        }
    }
    let _ = g.f;
    g
}

/// Placement for every function of the program.
pub fn analyze(p: &TProgram) -> Vec<FnPlacement> {
    // Summaries to a fixpoint, starting from "nothing flows anywhere".
    let mut sums: Vec<Summary> = p
        .funcs
        .iter()
        .map(|f| {
            let n = f.params.len();
            if f.external {
                Summary { to_ret: vec![true; n], to_global: vec![true; n] }
            } else {
                Summary { to_ret: vec![false; n], to_global: vec![false; n] }
            }
        })
        .collect();
    for _round in 0..50 {
        let mut changed = false;
        for (fi, f) in p.funcs.iter().enumerate() {
            if f.external {
                continue;
            }
            let g = graph(f, &sums);
            // What does each parameter's (caller-owned) storage reach,
            // other than its own caller?
            let mut s = Summary::default();
            for pl in &f.params {
                let reach = reaches_from(&g, Node::Local(*pl));
                s.to_ret.push(reach.contains(&Node::Ret));
                s.to_global.push(reach.iter().any(|n| *n == Node::Global || matches!(n, Node::Caller(q) if q != pl)));
            }
            if s != sums[fi] {
                sums[fi] = s;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    p.funcs
        .iter()
        .map(|f| {
            if f.external {
                return FnPlacement::default();
            }
            let g = graph(f, &sums);
            let fresh = fresh_locals(f, &g);
            let mut out = FnPlacement::default();
            for s in &g.sites {
                let reach = reaches_from(&g, Node::Site(*s));
                let callers: Vec<LocalId> = reach.iter().filter_map(|n| if let Node::Caller(l) = n { Some(*l) } else { None }).collect();
                let ret = reach.contains(&Node::Ret);
                let place = if reach.contains(&Node::Global) || callers.len() > 1 || (ret && !callers.is_empty()) {
                    Place::Global
                } else if let [p] = callers.as_slice() {
                    out.into = true;
                    Place::Into(*p)
                } else if ret {
                    Place::Ret
                } else {
                    // The innermost loop whose iteration nothing it reaches outlives.
                    let locals: Vec<LocalId> = reach.iter().filter_map(|n| if let Node::Local(l) = n { Some(*l) } else { None }).collect();
                    let loops = g.site_loops.get(s).cloned().unwrap_or_default();
                    loops.iter().rev().find(|l| fresh.get(l).is_some_and(|ok| locals.iter().all(|x| ok.contains(x)))).map_or(Place::Frame, |l| Place::Iter(*l))
                };
                if let Place::Iter(l) = place {
                    out.loops.insert(l);
                }
                out.sites.insert(*s, place);
            }
            out
        })
        .collect()
}

/// Nodes reachable from `start` (following "must live as long as" edges).
fn reaches_from(g: &Graph, start: Node) -> Vec<Node> {
    let mut seen = vec![start];
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        for m in g.edges.get(&n).into_iter().flatten() {
            if !seen.contains(m) {
                seen.push(*m);
                stack.push(*m);
            }
        }
    }
    seen
}

/// Per loop: the locals that hold nothing from one iteration to the next:
/// compiler temporaries and block parameters used only inside it, and
/// variables the body assigns first thing, before any use, and that
/// nothing outside the loop uses.
fn fresh_locals(f: &TFunc, g: &Graph) -> HashMap<usize, std::collections::HashSet<LocalId>> {
    let mut everywhere: HashMap<LocalId, usize> = HashMap::new();
    let mut all = vec![];
    for s in &f.body {
        crate::lower::collect_locals_stmt(s, &mut all);
    }
    for l in all {
        *everywhere.entry(l).or_default() += 1;
    }
    let mut out = HashMap::new();
    for (key, body) in &g.loop_bodies {
        let mut inside = vec![];
        for s in body {
            crate::lower::collect_locals_stmt(s, &mut inside);
        }
        let mut count: HashMap<LocalId, usize> = HashMap::new();
        for l in &inside {
            *count.entry(*l).or_default() += 1;
        }
        // Used only inside this loop?
        let only_inside = |l: &LocalId| everywhere.get(l) == count.get(l);
        let mut ok = std::collections::HashSet::new();
        let mut seen = std::collections::HashSet::new();
        for s in body {
            // `x = v` at the top of the body, before any other mention of x.
            if let TStmt::Expr(TExpr { kind: TK::Assign(l, v), .. }) = s {
                let mut in_v = vec![];
                crate::lower::collect_locals(v, &mut in_v);
                if !seen.contains(l) && !in_v.contains(l) && only_inside(l) {
                    ok.insert(*l);
                }
            }
            let mut here = vec![];
            crate::lower::collect_locals_stmt(s, &mut here);
            seen.extend(here);
        }
        for l in &inside {
            if !f.locals[*l].user && only_inside(l) {
                ok.insert(*l);
            }
        }
        out.insert(*key, ok);
    }
    out
}

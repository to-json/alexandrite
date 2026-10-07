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
//!
//! Lambdas: a lambda's body is part of its function's graph, but it runs
//! later, from anywhere, with no frame of its own (port-issues #164). Its
//! parameters are its caller's storage (`Caller` nodes, as a def's), its
//! captures are not aliased to them, and its result doesn't flow into the
//! lambda value. Each site in its body also gets an `LPlace`
//! (`lambda_place`): the current region, the storage of one parameter or
//! capture, or the program region.

use crate::tast::*;
use std::fmt::Write;
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
    /// R3: containers (Map or Pool locals) that own a child region for their
    /// contents, compacted at the end of each iteration of `loop_key`; and
    /// which locals refer to each one's header.
    pub owners: HashMap<LocalId, Owner>,
    pub owner_alias: HashMap<LocalId, LocalId>,
    /// Sites inside a lambda's body (by address): where they go when the
    /// lambda runs (port-issues #164). A lambda has no frame of its own: the
    /// region current when it runs is its caller's choice, which outlives
    /// only the call. What the body stores into storage it doesn't own (a
    /// parameter's, a capture's) must go where that storage lives.
    pub lambda_sites: HashMap<usize, LPlace>,
}

/// Where an allocation site in a lambda's body goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LPlace {
    /// The current region (the lambda's result, or temporaries).
    Cur,
    /// Stored into the storage behind this parameter or capture of the
    /// lambda (and nothing else outside it): the region that storage lives
    /// in, found at run time (the program region when it can't be).
    Storage(LocalId),
    /// Anything else: the program region.
    Program,
}

#[derive(Clone, Copy, Debug)]
pub struct Owner {
    pub loop_key: usize,
    pub def_site: usize,
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
        // Setting an optional field that may be boxed (R12: it leads back to
        // its own type) makes the box.
        TK::PlaceAssign(_, steps, _, x) => matches!(steps.last(), Some(TStep::Field(_))) && matches!(&x.ty, Ty::Opt(p) if mentions_rec(p)),
        TK::Array(_) | TK::Format(..) | TK::Call(..) => true,
        // Strings concatenate; Ints are bignums in promote mode.
        TK::Bin(..) | TK::Neg(_) => matches!(e.ty, Ty::Str | Ty::Array(_)) || (promote && e.ty == Ty::Int),
        TK::M(m, ..) => !matches!(
            m,
            // Scalars, reads, and terminals that produce an element or a number
            // (their blocks' allocations are sites of their own).
            M::TupleGet(_) | M::OptPresent | M::OptGet | M::Unwrap | M::EnumTag | M::Size | M::MapSize | M::MapHas | M::ChanLen | M::ResIsOk | M::ErrIs(_) | M::ErrAs(_) | M::IfaceIs(_) | M::IfaceAs(_) | M::IfaceEq | M::ErrEq
                | M::Even | M::Odd | M::IntSqrt | M::ToF | M::FloatToI | M::Conv(..) | M::FloatAbs | M::Sqrt | M::Math(_) | M::FloatBits | M::FloatFromBits | M::UMulHi | M::NowNs | M::PtrNull | M::CErrno
                | M::Sum | M::Max | M::Min | M::MaxBy | M::MinBy | M::Count | M::All | M::Any | M::Include | M::First | M::Last | M::Find | M::Each | M::Loop | M::Step
                | M::ChanClose | M::CapBegin | M::Exit | M::Global(_) | M::SetGlobal(_)
        ) || (promote && e.ty == Ty::Int),
        _ => false,
    }
}

type Ifaces = HashMap<String, Vec<(Ty, Vec<usize>)>>;

struct Graph<'a> {
    f: &'a TFunc,
    sums: &'a [Summary],
    /// Each interface's implementors, with their method instances.
    ifaces: &'a Ifaces,
    edges: HashMap<Node, Vec<Node>>,
    sites: Vec<usize>,
    /// Each site's expression (sites are keyed by address).
    site_exprs: HashMap<usize, &'a TExpr>,
    /// The loops (innermost last) around the walk's current point, and
    /// each site's.
    loops: Vec<usize>,
    site_loops: HashMap<usize, Vec<usize>>,
    /// Each loop's body, for deciding which locals are fresh per iteration.
    loop_bodies: HashMap<usize, Vec<&'a TStmt>>,
    /// Mutations of containers: the receiver's nodes and the loops around.
    mutations: Vec<(Vec<Node>, Vec<usize>)>,
    /// `c = Map/Pool literal`: the local and the site.
    defs: Vec<(LocalId, usize)>,
    /// The expression being walked, and for each node sent to Global the
    /// expression that sent it (for `alx explain mem`).
    at: crate::diag::Span,
    why_global: HashMap<Node, crate::diag::Span>,
    /// Where each loop is (its condition, or the iterating call).
    loop_at: HashMap<usize, crate::diag::Span>,
    /// The lambdas (by key, innermost last) around the walk's current
    /// point, and the innermost one of each site inside one.
    lambdas: Vec<u64>,
    site_lambda: HashMap<usize, u64>,
}

impl<'a> Graph<'a> {
    fn add_site(&mut self, e: &'a TExpr) {
        let k = e as *const TExpr as usize;
        self.sites.push(k);
        self.site_exprs.insert(k, e);
        self.site_loops.insert(k, self.loops.clone());
        if let Some(l) = self.lambdas.last() {
            self.site_lambda.insert(k, *l);
        }
    }
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
        if to.contains(&Node::Global) {
            for a in from {
                self.why_global.entry(*a).or_insert(self.at);
            }
        }
        for a in from {
            for b in to {
                self.edge(*a, *b);
            }
        }
    }

    /// The nodes an expression's value may refer to (walking it, adding
    /// the edges its effects create). A value with no storage (an Int, a
    /// Bool) refers to nothing.
    fn expr(&mut self, e: &'a TExpr) -> Vec<Node> {
        let saved = std::mem::replace(&mut self.at, e.span);
        let v = self.expr_nodes(e);
        self.at = saved;
        let promote = self.f.overflow == crate::ast::Overflow::Promote;
        if has_storage(&e.ty) || (promote && contains_int(&e.ty)) { v } else { vec![] }
    }

    fn expr_nodes(&mut self, e: &'a TExpr) -> Vec<Node> {
        let mut v = vec![];
        let promote = self.f.overflow == crate::ast::Overflow::Promote;
        // A call whose result has no storage (an Int, a Bool, ...) puts nothing in the
        // caller's region: not an allocation site, so a loop of such calls needs no
        // iteration region (a scanner calling `skip_ws(s, p)` per byte).
        // The same goes for building a tuple, struct or enum value without
        // storage (`(hi, lo)` or a struct of U64s returned per call): it
        // allocates nothing, so it needs no region of its own.
        let ctor = matches!(e.kind, TK::Call(..) | TK::M(M::StructNew | M::TupleNew | M::VariantNew(_) | M::EnumNew, ..));
        let scalar_call = ctor && !has_storage(&e.ty) && !(promote && contains_int(&e.ty));
        if allocates(e, promote) && !scalar_call {
            self.add_site(e);
            v.push(site(e));
        }
        match &e.kind {
            TK::Local(l) => v.push(Node::Local(*l)),
            TK::Assign(l, x) => {
                if matches!(x.kind, TK::M(M::MapNew | M::PoolNew, None, _, None)) {
                    self.defs.push((*l, &**x as *const TExpr as usize));
                }
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
                // A boxed optional field's box (R12) lives with the variable.
                let boxes = v.clone();
                self.flow(&boxes, &[Node::Local(*l)]);
            }
            // `place.m!(..)`: the view refers to the place (an alias). A
            // place in a local's own variables (no index step) gets its
            // region from this site: what the callee stores into it lives
            // there (see docs/notes/bang-calls.md).
            TK::Bang(l, steps, view, call) => {
                for st in steps {
                    if let TStep::Index(i) = st {
                        self.expr(i);
                    }
                }
                let vw = [Node::Local(*view)];
                self.alias(&[Node::Local(*l)], &vw);
                if !steps.iter().any(|s| matches!(s, TStep::Index(_))) {
                    self.add_site(e);
                    self.flow(&[site(e)], &vw);
                }
                self.mutations.push((vw.to_vec(), self.loops.clone()));
                v.extend(self.expr(call));
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
                    // A call through a function value whose lambdas all keep
                    // their parameters to themselves: the arguments stay put
                    // (the result may still be one of them, below).
                    M::FnCall if recv.as_ref().is_some_and(|r| clean_fn(&r.ty)) => {}
                    // A global (an array constant, or a package-level Atomic /
                    // Mutex, R11) outlives every task: what's stored in it,
                    // or under its lock, lives in the program region.
                    M::Global(_) => v.push(Node::Global),
                    M::SetGlobal(_) => self.flow(&avs, &[Node::Global]),
                    M::ChanSend | M::Yield | M::Spawn | M::EnumNew | M::FnCall | M::Pmap => {
                        self.flow(&rv, &[Node::Global]);
                        self.flow(&avs, &[Node::Global]);
                    }
                    // An interface call runs one of the implementors' methods:
                    // what escapes is what any of their summaries says escapes
                    // (parameter 0 is the receiver).
                    M::IfaceCall(mi) => {
                        let iname = match recv.as_ref().map(|r| &r.ty) {
                            Some(Ty::Iface(n)) => Some(n.clone()),
                            Some(Ty::Array(t)) => match &**t {
                                Ty::Iface(n) => Some(n.clone()),
                                _ => None,
                            },
                            _ => None,
                        };
                        let impls: Vec<usize> = iname.and_then(|n| self.ifaces.get(&n)).map(|v| v.iter().filter_map(|(_, fids)| fids.get(*mi).copied()).collect()).unwrap_or_default();
                        if impls.is_empty() {
                            self.flow(&rv, &[Node::Global]);
                            self.flow(&avs, &[Node::Global]);
                        }
                        for fid in impls {
                            let sum = self.sums.get(fid).cloned().unwrap_or_default();
                            if sum.to_global.first().copied().unwrap_or(true) {
                                self.flow(&rv, &[Node::Global]);
                            }
                            for (i, av) in per_arg.iter().enumerate() {
                                if sum.to_global.get(i + 1).copied().unwrap_or(true) {
                                    self.flow(av, &[Node::Global]);
                                }
                            }
                        }
                    }
                    // Mutations of the receiver: what's stored (and any growth)
                    // lives as long as the receiver.
                    M::Push | M::MapSet | M::MapDel | M::CopyInto | M::PoolAdd | M::PoolSet | M::PoolRemove => {
                        self.mutations.push((rv.clone(), self.loops.clone()));
                        // A Str map key is copied when it's inserted, and a
                        // deleted key isn't kept: only the value is stored.
                        let key_copied = match m {
                            M::MapDel => true,
                            M::MapSet => args.first().is_some_and(|a| a.ty == Ty::Str),
                            _ => false,
                        };
                        let mut stored: Vec<Node> = if key_copied { per_arg.iter().skip(1).flatten().copied().collect() } else { avs.clone() };
                        stored.push(site(e));
                        self.flow(&stored, &rv);
                        // copy(dst, src): src's elements into dst. Elements
                        // without storage (a [U64], a [Byte]) are copied by
                        // value: src needn't outlive dst (math/big copies
                        // words between scratch and result buffers).
                        let promote = self.f.overflow == crate::ast::Overflow::Promote;
                        let flat = args.first().is_some_and(|a| match &a.ty {
                            Ty::Array(t) | Ty::Fixed(t, _) => !has_storage(t) && !(promote && contains_int(t)),
                            _ => false,
                        });
                        if *m == M::CopyInto && per_arg.len() >= 2 && !flat {
                            let (d, s) = (per_arg[0].clone(), per_arg[1].clone());
                            self.flow(&s, &d);
                        }
                    }
                    _ => {}
                }
                if let Some(b) = blk {
                    // Block parameters refer to the receiver's elements (and
                    // to the accumulator, for reduce-like methods).
                    // A lambda's parameters are its caller's arguments, not
                    // its captures (graph() gives them `Caller` nodes).
                    let params: Vec<Node> = b.params.iter().map(|p| Node::Local(*p)).collect();
                    let mut src = rv.clone();
                    src.extend(avs.iter().copied());
                    if *m != M::Lambda {
                        self.alias(&src, &params);
                    }
                    if *m == M::Lambda {
                        // The body runs when the lambda is called, with the
                        // caller's region current; its result goes to that
                        // caller, not into the lambda value.
                        self.lambdas.push(((b.id as u64) << 32) | b.span.lo as u64);
                        for s in &b.body {
                            self.stmt(s);
                        }
                        self.lambdas.pop();
                    } else if matches!(m, M::Spawn | M::EnumNew) {
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
                            self.loop_at.insert(e as *const TExpr as usize, e.span);
                            self.loop_bodies.insert(e as *const TExpr as usize, b.body.iter().collect());
                        }
                        for s in &b.body {
                            let sv = self.stmt(s);
                            // A `lock` block's value is copied out.
                            if *m != M::Lock {
                                v.extend(sv);
                            }
                        }
                        if is_loop {
                            self.loops.pop();
                        }
                    }
                }
                // A task handle doesn't share its captures' storage (they
                // went to the task, through Global).
                // A copy of a flat slice or map is fresh storage, and so is a
                // function value's (its captures are copied deeply).
                let flat_copy = *m == M::Dup
                    && match &e.ty {
                        Ty::Array(t) => !has_storage(t),
                        Ty::Map(k, t) => !has_storage(k) && !has_storage(t),
                        Ty::Fn(..) | Ty::Struct(..) => true,
                        _ => false,
                    };
                // A map lookup's value is one of the map's values (or the
                // default given), never the key it was looked up by: the
                // view aliases the receiver only (port-issues #177; else a
                // helper `def get(m, k) { m[k] }` ties the map to the key's
                // storage, and the caller's map goes to the program region).
                let key_arg = matches!(m, M::MapGet | M::MapGetOr | M::MapDel | M::MapHas);
                if !matches!(m, M::Spawn | M::Lock) && !flat_copy {
                    v.extend(rv);
                    v.extend(per_arg.iter().skip(key_arg as usize).flatten().copied());
                }
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
                crate::prove::each_child(e, &mut |c| kids.push(c));
                for c in kids {
                    v.extend(self.expr(c));
                }
            }
        }
        v
    }

    /// Walk a statement; returns the nodes of its value (an expression
    /// statement's), for block results.
    fn stmt(&mut self, s: &'a TStmt) -> Vec<Node> {
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
                self.loop_at.insert(key, c.span);
                self.loops.push(key);
                self.loop_bodies.insert(key, b.iter().collect());
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
}

/// Does the type contain an Int (a bignum, in promote mode)?
pub fn contains_int(t: &Ty) -> bool {
    match t {
        Ty::Rec(n) => with_rec(n, false, contains_int),
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
        // A type that contains itself does so through storage (R12).
        Ty::Rec(_) => true,
        // An Int or Bool atomic is a runtime cell outside every region.
        Ty::Int | Ty::IntK(_) | Ty::Float | Ty::Bool | Ty::Unit | Ty::Range | Ty::Never | Ty::Handle(_) | Ty::Ptr => false,
        Ty::Atomic(t) => t.atomic_boxed(), // boxed: a lock and the value, like a Mutex
        Ty::Opt(t) => has_storage(t),
        Ty::Tuple(ts) => ts.iter().any(has_storage),
        Ty::Struct(_, fs) => fs.iter().any(|(_, t)| has_storage(t)),
        Ty::Enum(_, vs) => vs.iter().any(|(_, fs)| fs.iter().any(|(_, t)| has_storage(t))),
        _ => true,
    }
}

fn graph<'a>(f: &'a TFunc, sums: &'a [Summary], ifaces: &'a Ifaces) -> Graph<'a> {
    let mut g = Graph { f, sums, ifaces, edges: HashMap::new(), sites: vec![], site_exprs: HashMap::new(), loops: vec![], site_loops: HashMap::new(), loop_bodies: HashMap::new(), mutations: vec![], defs: vec![], at: f.span, why_global: HashMap::new(), loop_at: HashMap::new(), lambdas: vec![], site_lambda: HashMap::new() };
    // Writing into a parameter's storage writes into the caller's objects
    // (a lambda's parameters too: its caller's).
    for p in f.params.iter().chain(f.lambda_info.iter().flat_map(|(_, ps, _)| ps)) {
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
    let sums = summaries(p);
    analyze_with(p, &sums)
}

/// For every local of every function: the locals sharing storage with it
/// (itself included), and the parameters whose caller-owned storage it
/// shares. Aliasing in either direction counts; the paths stop at returns,
/// globals and callers.
pub fn aliases(p: &TProgram) -> Vec<HashMap<LocalId, (Vec<LocalId>, Vec<LocalId>)>> {
    let sums = summaries(p);
    p.funcs
        .iter()
        .map(|f| {
            let mut out = HashMap::new();
            if f.external {
                return out;
            }
            let g = graph(f, &sums, &p.ifaces);
            let mut adj: HashMap<Node, Vec<Node>> = HashMap::new();
            for (a, bs) in &g.edges {
                for b in bs {
                    adj.entry(*a).or_default().push(*b);
                    adj.entry(*b).or_default().push(*a);
                }
            }
            for l in 0..f.locals.len() {
                let (mut locals, mut callers) = (vec![l], vec![]);
                let mut seen = vec![Node::Local(l)];
                let mut stack = vec![Node::Local(l)];
                while let Some(n) = stack.pop() {
                    for m in adj.get(&n).into_iter().flatten() {
                        if seen.contains(m) {
                            continue;
                        }
                        seen.push(*m);
                        match m {
                            Node::Caller(q) if f.params.contains(q) => callers.push(*q),
                            Node::Caller(_) | Node::Ret | Node::Global => {}
                            // A mutex is a wall: what it guards is reached
                            // only under its lock.
                            Node::Local(x) if matches!(f.locals[*x].ty, Ty::Mutex(_) | Ty::Atomic(_)) => {}
                            // Nor does a value with no shared storage (a struct of
                            // scalars, strings and handles) carry an alias on.
                            Node::Local(x) if !crate::sharing::shares(&f.locals[*x].ty) && *x != l => {}
                            Node::Local(x) => {
                                locals.push(*x);
                                stack.push(*m);
                            }
                            _ => stack.push(*m),
                        }
                    }
                }
                out.insert(l, (locals, callers));
            }
            out
        })
        .collect()
}

thread_local! {
    /// Function types (by Debug text) whose every lambda keeps its
    /// parameters to itself: nothing they're given escapes the call.
    static CLEAN_FNS: std::cell::RefCell<std::collections::HashSet<String>> = std::cell::RefCell::new(Default::default());
}

fn clean_fn(t: &Ty) -> bool {
    CLEAN_FNS.with(|c| c.borrow().contains(&format!("{t:?}")))
}

/// The function types all of whose lambdas' parameters reach only the
/// lambda's own locals (not the program region, a return, a caller or a
/// captured variable).
fn clean_fn_types(p: &TProgram, sums: &[Summary]) -> std::collections::HashSet<String> {
    let mut dirty: std::collections::HashSet<String> = Default::default();
    let mut all: std::collections::HashSet<String> = Default::default();
    for f in p.funcs.iter().filter(|f| !f.external) {
        if f.lambdas.is_empty() {
            continue;
        }
        let g = graph(f, sums, &p.ifaces);
        for (lo, ty, _) in &f.lambdas {
            let key = format!("{ty:?}");
            all.insert(key.clone());
            let Some((_, params, (own_lo, own_hi))) = f.lambda_info.iter().find(|(l, _, _)| l == lo) else {
                dirty.insert(key);
                continue;
            };
            let escapes = params.iter().any(|pl| {
                reaches_from(&g, Node::Local(*pl)).iter().any(|n| match n {
                    Node::Local(x) => *x < *own_lo || *x >= *own_hi,
                    // Into its own argument: placed where that lives.
                    Node::Caller(q) => q != pl,
                    Node::Global | Node::Ret => true,
                    _ => false,
                })
            });
            if escapes {
                dirty.insert(key);
            }
        }
    }
    all.retain(|k| !dirty.contains(k));
    all
}

fn summaries(p: &TProgram) -> Vec<Summary> {
    CLEAN_FNS.with(|c| c.borrow_mut().clear());
    let first = summaries_with(p);
    let clean = clean_fn_types(p, &first);
    if clean.is_empty() {
        return first;
    }
    CLEAN_FNS.with(|c| *c.borrow_mut() = clean);
    summaries_with(p)
}

fn summaries_with(p: &TProgram) -> Vec<Summary> {
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
            let g = graph(f, &sums, &p.ifaces);
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
    sums
}

fn analyze_with(p: &TProgram, sums: &[Summary]) -> Vec<FnPlacement> {
    p.funcs
        .iter()
        .map(|f| {
            if f.external {
                return FnPlacement::default();
            }
            let g = graph(f, sums, &p.ifaces);
            let fresh = fresh_locals(f, &g);
            let mut out = FnPlacement::default();
            for s in &g.sites {
                let reach = reaches_from(&g, Node::Site(*s));
                let callers: Vec<LocalId> = reach.iter().filter_map(|n| if let Node::Caller(l) = n { Some(*l) } else { None }).collect();
                let ret = reach.contains(&Node::Ret);
                // Into a lambda's argument (through a capture the lambda
                // stores there): storage of unknown lifetime.
                let lambda_arg = callers.iter().any(|c| !f.params.contains(c));
                let place = if reach.contains(&Node::Global) || lambda_arg || callers.len() > 1 || (ret && !callers.is_empty()) {
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
                if let Some(lk) = g.site_lambda.get(s) {
                    out.lambda_sites.insert(*s, lambda_place(f, &g, *s, *lk));
                }
            }
            owners(f, &g, &fresh, &mut out);
            out
        })
        .collect()
}

/// Where site `s`, in the body of lambda `lk`, goes when the lambda runs:
/// follow what it must outlive through the lambda's own locals; stop at its
/// parameters and captures (storage it doesn't own, which lives wherever
/// its caller or creator put it).
fn lambda_place(f: &TFunc, g: &Graph, s: usize, lk: u64) -> LPlace {
    let Some((_, params, (lo, hi))) = f.lambda_info.iter().find(|(l, _, _)| *l == lk) else {
        return LPlace::Program;
    };
    let caps: &[LocalId] = f.lambdas.iter().find(|(l, _, _)| *l == lk).map_or(&[], |(_, _, c)| c);
    let mut into: Vec<LocalId> = vec![];
    let start = Node::Site(s);
    let mut seen = vec![start];
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        for m in g.edges.get(&n).into_iter().flatten() {
            if seen.contains(m) {
                continue;
            }
            seen.push(*m);
            match m {
                Node::Global => return LPlace::Program,
                // The def's own result (a `return` in the body has none of
                // the lambda's storage) or a parameter's caller (reached
                // from the parameter, below).
                Node::Ret | Node::Caller(_) => {}
                // What the argument's or capture's storage must outlive,
                // it outlives already.
                Node::Local(x) if params.contains(x) || caps.contains(x) => {
                    if !into.contains(x) {
                        into.push(*x);
                    }
                }
                // A variable of the enclosing function the body can't name
                // (never guess). Compiler temporaries made after checking
                // (capture.rs's `_push`) sit outside the lambda's range too.
                Node::Local(x) if (*x < *lo || *x >= *hi) && (f.locals[*x].user || f.params.contains(x)) => return LPlace::Program,
                _ => stack.push(*m),
            }
        }
    }
    match into.as_slice() {
        [] => LPlace::Cur,
        [x] => LPlace::Storage(*x),
        _ => LPlace::Program,
    }
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
        // Variables every iteration assigns before it reads them (port-issues
        // #177): `x = v` anywhere in the body's statements, and in an `if`'s
        // both branches (or one, when the other leaves the iteration), counts
        // from there on. A variable read (or mutated in place) anywhere it
        // may still hold the previous iteration's value is not fresh.
        let mut da = DefAssign::default();
        for s in body {
            da.stmt(s);
        }
        for l in &inside {
            if only_inside(l) && !da.stale.contains(l) {
                ok.insert(*l);
            }
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

/// Definite assignment over one iteration of a loop body: `assigned` holds
/// the variables assigned on every path to the current point; `stale` the
/// ones mentioned (read, or changed in place) at a point where they may not
/// be. Conservative: an assignment nested in an expression, a block or an
/// inner loop doesn't count after it; a `defer` runs later, so what it
/// mentions is stale.
#[derive(Default)]
struct DefAssign {
    assigned: std::collections::HashSet<LocalId>,
    stale: std::collections::HashSet<LocalId>,
}

impl DefAssign {
    fn read(&mut self, l: LocalId) {
        if !self.assigned.contains(&l) {
            self.stale.insert(l);
        }
    }
    /// Walk an expression in evaluation order. What it assigns counts only
    /// inside a statement sequence of its own (a `case` arm, a block), not
    /// after the expression: it may be conditional.
    fn uses(&mut self, e: &TExpr) {
        match &e.kind {
            TK::Local(l) => self.read(*l),
            TK::Assign(_, v) => self.uses(v),
            TK::IndexAssign(l, ..) | TK::PlaceAssign(l, ..) | TK::Bang(l, ..) => {
                self.read(*l);
                crate::prove::each_child(e, &mut |c| self.uses(c));
            }
            TK::Seq(ss) => {
                let saved = self.assigned.clone();
                for s in ss {
                    self.stmt(s);
                }
                self.assigned = saved;
            }
            TK::Ternary(c, a, b) => {
                self.uses(c);
                for x in [a, b] {
                    let saved = self.assigned.clone();
                    self.uses(x);
                    self.assigned = saved;
                }
            }
            TK::M(_, r, args, Some(b)) => {
                if let Some(r) = r {
                    self.uses(r);
                }
                for a in args {
                    self.uses(a);
                }
                // The body runs zero or more times, now or later; its
                // parameters are set each time.
                let saved = self.assigned.clone();
                self.assigned.extend(b.params.iter().copied());
                for s in &b.body {
                    self.stmt(s);
                }
                self.assigned = saved;
            }
            _ => crate::prove::each_child(e, &mut |c| self.uses(c)),
        }
    }
    /// Does this statement list always leave the iteration (or the loop)?
    fn leaves(ss: &[TStmt]) -> bool {
        matches!(ss.last(), Some(TStmt::Next(_) | TStmt::Break(..) | TStmt::Return(..) | TStmt::Fail(..)))
    }
    fn stmts(&mut self, ss: &[TStmt]) {
        for s in ss {
            self.stmt(s);
        }
    }
    fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Expr(TExpr { kind: TK::Assign(l, v), .. }) => {
                self.uses(v);
                self.assigned.insert(*l);
            }
            TStmt::Expr(e) | TStmt::Break(Some(e), _) | TStmt::Return(Some(e), _) | TStmt::Fail(e, _) => self.uses(e),
            TStmt::MultiAssign(ls, es) => {
                for e in es {
                    self.uses(e);
                }
                self.assigned.extend(ls.iter().copied());
            }
            TStmt::If(c, a, b) => {
                self.uses(c);
                let before = self.assigned.clone();
                self.stmts(a);
                let after_a = std::mem::replace(&mut self.assigned, before);
                self.stmts(b);
                let after_b = std::mem::take(&mut self.assigned);
                self.assigned = match (Self::leaves(a), Self::leaves(b)) {
                    (true, _) => after_b,
                    (false, true) => after_a,
                    _ => after_a.intersection(&after_b).copied().collect(),
                };
            }
            TStmt::While(c, b) => {
                self.uses(c);
                let before = self.assigned.clone();
                self.stmts(b);
                self.assigned = before;
            }
            TStmt::Defer(e) => {
                let mut ls = vec![];
                crate::lower::collect_locals(e, &mut ls);
                self.stale.extend(ls);
            }
            TStmt::Next(_) | TStmt::Break(None, _) | TStmt::Return(None, _) => {}
        }
    }
}

/// R3: which Map/Pool locals own their contents' region. A container
/// qualifies when it's made here (`c = {}` / `Pool[T].new`, once), stays in
/// this call's frame, is mutated inside a loop, and nothing that refers to
/// it or to its contents outlives that loop's iteration or the call: then,
/// at the end of each iteration, its live contents can be copied to a fresh
/// region and the old one freed without anything noticing.
fn owners(f: &TFunc, g: &Graph, fresh: &HashMap<usize, std::collections::HashSet<LocalId>>, out: &mut FnPlacement) {
    // Undirected neighbours (aliasing in either direction).
    let mut adj: HashMap<Node, Vec<Node>> = HashMap::new();
    for (a, bs) in &g.edges {
        for b in bs {
            adj.entry(*a).or_default().push(*b);
            adj.entry(*b).or_default().push(*a);
        }
    }
    for (c, def_site) in &g.defs {
        let loc = &f.locals[*c];
        if loc.reassigned > 0 || f.params.contains(c) || !matches!(loc.ty, Ty::Map(..) | Ty::Pool(_)) {
            continue;
        }
        if g.defs.iter().filter(|(l, _)| l == c).count() != 1 || out.sites.get(def_site) != Some(&Place::Frame) {
            continue;
        }
        // Everything connected to the container (stopping at the sinks).
        let mut comp = vec![Node::Local(*c)];
        let mut stack = vec![Node::Local(*c)];
        let mut escapes = false;
        while let Some(n) = stack.pop() {
            for m in adj.get(&n).into_iter().flatten() {
                match m {
                    Node::Ret | Node::Global | Node::Caller(_) => escapes = true,
                    _ if !comp.contains(m) => {
                        comp.push(*m);
                        stack.push(*m);
                    }
                    _ => {}
                }
            }
        }
        if escapes {
            continue;
        }
        // The loop: the innermost one around every mutation of it.
        let muts: Vec<&Vec<usize>> = g.mutations.iter().filter(|(rv, _)| rv.iter().any(|n| comp.contains(n))).map(|(_, l)| l).collect();
        if muts.is_empty() {
            continue;
        }
        let mut common: Vec<usize> = muts[0].clone();
        for l in &muts[1..] {
            let n = common.iter().zip(l.iter()).take_while(|(a, b)| a == b).count();
            common.truncate(n);
        }
        let Some(lk) = common.last().copied() else { continue };
        // The def must be outside that loop (the container lives across it).
        if g.site_loops.get(def_site).is_some_and(|ls| ls.contains(&lk)) {
            continue;
        }
        // Every other local tied to it is gone at the end of an iteration,
        // or only comes into use after the loop is over.
        let ok = fresh.get(&lk).cloned().unwrap_or_default();
        let locals: Vec<LocalId> = comp.iter().filter_map(|n| if let Node::Local(l) = n { Some(*l) } else { None }).collect();
        let (lo, hi) = loop_extent(g, lk);
        // Not when an enclosing loop comes back around to this one.
        let nested = g.loop_bodies.keys().any(|k| *k != lk && {
            let (a, b) = loop_extent(g, *k);
            a <= lo && hi <= b
        });
        let after_only = |l: &LocalId| {
            if nested {
                return false;
            }
            let mut spans = vec![];
            for st in &f.body {
                local_spans(st, *l, &mut spans);
            }
            !spans.is_empty() && spans.iter().all(|sp| sp.lo > hi)
        };
        if !locals.iter().all(|l| l == c || ok.contains(l) || after_only(l)) {
            continue;
        }
        out.owners.insert(*c, Owner { loop_key: lk, def_site: *def_site });
        for l in &locals {
            if *l != *c && f.locals[*l].ty == loc.ty {
                out.owner_alias.insert(*l, *c);
            }
        }
        out.owner_alias.insert(*c, *c);
        // What's stored into it is allocated in its region.
        for n in &comp {
            if let Node::Site(s) = n {
                if s != def_site && reaches_from(g, *n).contains(&Node::Local(*c)) {
                    out.sites.insert(*s, Place::Into(*c));
                    out.into = true;
                }
            }
        }
    }
}

/// The source extent of a loop's body.
fn loop_extent(g: &Graph, key: usize) -> (u32, u32) {
    let (mut lo, mut hi) = (u32::MAX, 0);
    for st in g.loop_bodies.get(&key).into_iter().flatten() {
        crate::prove::stmt_exprs(st, &mut |e| span_extent(e, &mut lo, &mut hi));
    }
    (lo, hi)
}

fn span_extent(e: &TExpr, lo: &mut u32, hi: &mut u32) {
    *lo = (*lo).min(e.span.lo);
    *hi = (*hi).max(e.span.hi);
    // (each_child visits blocks' statements too.)
    crate::prove::each_child(e, &mut |c| span_extent(c, lo, hi));
}

/// Where local `l` is used or assigned.
fn local_spans(s: &TStmt, l: LocalId, out: &mut Vec<crate::diag::Span>) {
    if let TStmt::MultiAssign(ls, _) = s {
        if ls.contains(&l) {
            out.push(crate::diag::Span::default());
        }
    }
    crate::prove::stmt_exprs(s, &mut |e| expr_spans(e, l, out));
}

fn expr_spans(e: &TExpr, l: LocalId, out: &mut Vec<crate::diag::Span>) {
    match &e.kind {
        TK::Local(x) | TK::Assign(x, _) | TK::IndexAssign(x, ..) | TK::PlaceAssign(x, ..) | TK::Bang(x, ..) if *x == l => out.push(e.span),
        _ => {}
    }
    // A block's or a Seq's statements go through local_spans (multiple
    // assignment); `each_child` would visit them again, doubling the work
    // at every level of nesting.
    match &e.kind {
        TK::M(_, r, args, Some(b)) => {
            r.iter().map(|r| &**r).chain(args).for_each(|c| expr_spans(c, l, out));
            if b.params.contains(&l) {
                out.push(b.span);
            }
            for s in &b.body {
                local_spans(s, l, out);
            }
        }
        TK::Seq(ss) => {
            for s in ss {
                local_spans(s, l, out);
            }
        }
        _ => crate::prove::each_child(e, &mut |c| expr_spans(c, l, out)),
    }
}

/// `alx explain mem`: every allocation site of the program's own code,
/// the region it goes in, and why.
pub fn explain(p: &TProgram, sm: &crate::diag::SourceMap, files: &[u32]) -> String {
    let sums = summaries(p);
    let placement = analyze_with(p, &sums);
    let mut out = String::new();
    let (mut total, mut hot) = (0, 0);
    let line_col = |sp: crate::diag::Span| {
        let (l, c) = sm.files[sp.file as usize].line_col(sp.lo);
        format!("{l}:{c}")
    };
    for (f, pl) in p.funcs.iter().zip(&placement) {
        if f.external || !files.contains(&f.span.file) {
            continue;
        }
        let g = graph(f, &sums, &p.ifaces);
        if g.sites.is_empty() {
            continue;
        }
        let name = if f.is_main { "(top level)".to_string() } else { f.src_name.clone() };
        let mut lines = String::new();
        let mut sites: Vec<usize> = g.sites.clone();
        sites.sort_by_key(|s| g.site_exprs[s].span.lo);
        sites.dedup();
        let mut shown = std::collections::HashSet::new();
        for s in sites {
            let e = g.site_exprs[&s];
            let snip: String = sm.snippet(e.span).lines().next().unwrap_or("").chars().take(32).collect();
            let in_loop = g.site_loops.get(&s).is_some_and(|l| !l.is_empty());
            let loop_at = |key: usize| match g.loop_at.get(&key) {
                Some(sp) => line_col(*sp),
                None => "?".into(),
            };
            // Values without storage allocate nothing, except a store's growth.
            let stores = matches!(e.kind, TK::IndexAssign(..) | TK::PlaceAssign(..) | TK::Bang(..) | TK::M(M::Push | M::MapSet | M::CopyInto | M::PoolAdd | M::PoolSet, ..));
            if (!has_storage(&e.ty) && !stores) || !shown.insert((e.span.lo, e.span.hi)) {
                continue;
            }
            let place = pl.sites.get(&s).copied().unwrap_or(Place::Frame);
            let in_lambda = match pl.lambda_sites.get(&s) {
                Some(LPlace::Cur) => Some("in a lambda: the region current when it runs (its caller's)".to_string()),
                Some(LPlace::Storage(x)) => Some(format!("in a lambda: the region of `{}` (stored into it)", f.locals[*x].name)),
                Some(LPlace::Program) => {
                    total += 1;
                    Some("in a lambda: program region, never freed".to_string())
                }
                None => None,
            };
            if let Some(why) = in_lambda {
                let _ = writeln!(lines, "  {:<7} {:<34} {why}", line_col(e.span), snip);
                continue;
            }
            let why = match place {
                Place::Frame => {
                    let when = if f.is_main { "the program ends".to_string() } else { format!("`{}` returns", f.src_name) };
                    if in_loop {
                        hot += 1;
                        format!("frame: freed when {when}  << every iteration: piles up until then")
                    } else {
                        format!("frame: freed when {when}")
                    }
                }
                Place::Iter(k) => format!("iteration of the loop at {}: freed each time round", loop_at(k)),
                Place::Ret => "the caller's region: it's the result".to_string(),
                Place::Into(c) if pl.owners.contains_key(&c) => {
                    format!("`{}`'s own region: compacted at the loop at {}", f.locals[c].name, loop_at(pl.owners[&c].loop_key))
                }
                Place::Into(c) => format!("the region of `{}` (stored into it)", f.locals[c].name),
                Place::Global => {
                    total += 1;
                    if in_loop {
                        hot += 1;
                    }

                    // Find what sends it to the program region.
                    let mut seen = vec![Node::Site(s)];
                    let mut i = 0;
                    let mut reason = None;
                    while i < seen.len() && reason.is_none() {
                        let n = seen[i];
                        i += 1;
                        if let Some(sp) = g.why_global.get(&n) {
                            reason = Some(*sp);
                        }
                        for m in g.edges.get(&n).into_iter().flatten() {
                            if !seen.contains(m) {
                                seen.push(*m);
                            }
                        }
                    }
                    let via = match reason {
                        Some(sp) => {
                            let w: String = sm.snippet(sp).lines().next().unwrap_or("").chars().take(40).collect();
                            format!("it reaches `{w}` at {}", line_col(sp))
                        }
                        None => "it's stored into more than one of the caller's objects".to_string(),
                    };
                    format!("program region, never freed: {via}{}", if in_loop { "  << every iteration: grows without bound" } else { "" })
                }
            };
            let _ = writeln!(lines, "  {:<7} {:<34} {why}", line_col(e.span), snip);
        }
        if !lines.is_empty() {
            let _ = writeln!(out, "{name}  {}\n{lines}", sm.files[f.span.file as usize].name);
        }
    }
    let _ = writeln!(out, "{total} allocation site(s) in the program region; {hot} site(s) inside loops that pile up");
    out
}

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

thread_local! {
    /// Each interface's implementor types (in tag order), for `lty`.
    static IFACES: RefCell<HashMap<String, Vec<Ty>>> = RefCell::new(HashMap::new());
    /// Every error type (enums), in tag order.
    static ERRORS: RefCell<Vec<Ty>> = const { RefCell::new(Vec::new()) };
    /// Every lambda literal: (enclosing function, block start, fn type, capture types).
    static LAMBDAS: RefCell<Vec<(String, u64, Ty, Vec<Ty>)>> = const { RefCell::new(Vec::new()) };
    /// The LIR global of each array constant read in place, by (`M::Global`
    /// index, type): an Int is a bignum in promote mode, so one constant
    /// can need two.
    static GLOBALS: RefCell<Vec<(usize, LTy)>> = const { RefCell::new(Vec::new()) };
    /// The generated helper functions made so far (`Lw::helper`).
    static HELPERS: RefCell<std::collections::HashSet<String>> = RefCell::new(Default::default());
}

/// The LIR global for array constant `k` as an `t`.
fn global_of(k: usize, t: LTy) -> usize {
    GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        g.iter().position(|(j, u)| *j == k && *u == t).unwrap_or_else(|| {
            g.push((k, t));
            g.len() - 1
        })
    })
}

/// A constant's literal value (`const_lit`) as an `t`; nothing to evaluate.
fn lit_le(e: &TExpr, t: &LTy) -> LE {
    match (&e.kind, t) {
        (TK::Int(v), LTy::PInt) => LE::ToP(Box::new(LE::I(*v))),
        (TK::Int(v), _) => LE::I(*v),
        (TK::Float(v), _) => LE::F(*v),
        (TK::Str(s), _) => LE::S(s.clone()),
        (TK::Bool(b), _) => LE::B(*b),
        (TK::Array(items), LTy::Arr(el)) => LE::ArrLit((**el).clone(), items.iter().map(|x| lit_le(x, el)).collect()),
        _ => unreachable!("not a constant literal: {e:?}"),
    }
}

/// Whether a function's frame can be a mark in its caller's region: nothing
/// it allocates outlives the call. Its parameters and result hold no storage
/// (so no callee can store into the caller's objects through them, and
/// nothing is returned), nothing is placed in the caller's or a parameter's
/// region, and it can't fail (an error is copied out to the caller).
fn light_frame(f: &TFunc, pl: &crate::regions::FnPlacement) -> bool {
    use crate::regions::{contains_int, has_storage, Place};
    let promote = f.overflow == Overflow::Promote;
    let storage = |t: &Ty| has_storage(t) || (promote && contains_int(t));
    !f.is_main
        && !f.fallible
        && !pl.into
        && pl.owners.is_empty()
        && !storage(&f.ret)
        && f.params.iter().all(|p| !storage(&f.locals[*p].ty))
        && pl.sites.values().all(|p| !matches!(p, Place::Ret | Place::Into(_)))
}

/// What an Error points to: (tag, where, context, each error type's value).
fn error_body(mode: Overflow) -> LTy {
    LTy::Tup([LTy::I64, LTy::Str, LTy::Str].into_iter().chain(ERRORS.with(|e| e.borrow().clone()).iter().map(|t| lty(t, mode))).collect())
}

/// An Error value's body (it's a one-element array).
fn err_body(e: LE) -> LE {
    LE::Index { arr: Box::new(e), idx: Box::new(LE::I(0)), check: None }
}

/// The sites (global index) of lambdas of function type `t`, in tag order.
fn lambda_sites(t: &Ty) -> Vec<(usize, Vec<Ty>)> {
    LAMBDAS.with(|l| l.borrow().iter().enumerate().filter(|(_, s)| s.2 == *t).map(|(g, s)| (g, s.3.clone())).collect())
}

pub fn lower(p: &TProgram, sm: &SourceMap, opts: &Opts) -> LProgram {
    IFACES.with(|m| *m.borrow_mut() = p.ifaces.iter().map(|(k, v)| (k.clone(), v.iter().map(|(t, _)| t.clone()).collect())).collect());
    ERRORS.with(|e| *e.borrow_mut() = p.errors.clone());
    LAMBDAS.with(|l| {
        *l.borrow_mut() = p.funcs.iter().flat_map(|f| f.lambdas.iter().map(move |(lo, t, caps)| (f.cname.clone(), *lo, t.clone(), caps.iter().map(|c| f.locals[*c].ty.clone()).collect()))).collect();
    });
    GLOBALS.with(|g| g.borrow_mut().clear());
    reset_recs();
    let prog = RefCell::new(LProgram::default());

    // R1: where each allocation lives (unless turned off, for comparison).
    let placement = if std::env::var_os("ALX_NO_REGIONS").is_some() { vec![] } else { crate::regions::analyze(p) };
    for (fi, f) in p.funcs.iter().enumerate() {
        let lf = if let Some(sym) = &f.ffi {
            ffi_wrapper(p, sm, opts, f, sym, &prog)
        } else if f.external {
            let mut lw = Lw::new(p, sm, opts, f, f.overflow, ErrPath::Return(vec![]), &prog);
            let params = f.params.iter().map(|l| lw.var_of(*l)).collect();
            LFunc { name: f.cname.clone(), params, vars: lw.vars, ret: fn_ret(f), body: vec![], external: true, is_main: false, labels: 0 }
        } else {
            let path = if f.is_main { ErrPath::Die(vec![]) } else { ErrPath::Return(vec![]) };
            let mut lw = Lw::new(p, sm, opts, f, f.overflow, path, &prog);
            if f.fallible && !f.is_main {
                lw.res = Some(ok_lty(lty(&f.ret, f.overflow)));
            }
            let params: Vec<V> = f.params.iter().map(|l| lw.var_of(*l)).collect();
            lw.analyze_facts(&f.body);
            // The call's own region: entered here, exited on every way out.
            let mut prologue = vec![];
            let mut frame_var: Option<(V, V)> = None;
            if let Some(pl) = placement.get(fi).filter(|pl| !pl.sites.is_empty()) {
                if light_frame(f, pl) {
                    // Allocations of the frame go in the region current at the
                    // call, above a mark; the way out rolls it back.
                    let (mark, larges, dest) = (lw.new_var("mark", LTy::Region), lw.new_var("larges", LTy::Region), lw.new_var("dest", LTy::Region));
                    // The region itself is named from inside loops that have
                    // iteration regions of their own, from an argument made in
                    // another region (`f(g([]))` with g's result in the program
                    // region and `[]` in the frame), and from an allocation in
                    // the program region (a map set under a package-level
                    // Mutex's lock with a key built for it), so it is always set.
                    prologue.push(LS::Set(dest, LE::Rt(Rt::RegionCur, vec![])));
                    prologue.push(LS::Set(mark, LE::Rt(Rt::RegionMark, vec![])));
                    prologue.push(LS::Set(larges, LE::Rt(Rt::RegionMarkLarges, vec![])));
                    lw.light = Some((mark, larges));
                    lw.frame = Some((dest, dest));
                } else {
                    let (frame, dest) = (lw.new_var("frame", LTy::Region), lw.new_var("dest", LTy::Region));
                    prologue.push(LS::RegionEnter { region: frame, saved: dest });
                    lw.frame = Some((frame, dest));
                }
                frame_var = lw.frame.filter(|_| lw.light.is_none()).map(|(f, d)| (f, d));
                lw.place = Some(pl);
            }
            let mut body = lw.body_with_return(&f.body, !f.is_main && f.ret != Ty::Unit);
            prologue.append(&mut body);
            let mut body = prologue;
            let _ = frame_var;
            if lw.res.is_some() && f.ret == Ty::Unit {
                // Falling off the end of a fallible def that returns nothing.
                body.push(LS::Return(Some(lw.ok_result(LE::B(false)))));
            }
            LFunc { name: f.cname.clone(), params, vars: lw.vars, ret: fn_ret(f), body, external: false, is_main: f.is_main, labels: lw.labels }
        };
        prog.borrow_mut().funcs.push(lf);
    }
    let mut prog = prog.into_inner();
    prog.uses_pint = p.funcs.iter().any(|f| f.overflow == Overflow::Promote);
    // Array constants' globals are set before anything else runs, in the
    // program region (current at that point), and only read afterwards.
    let globals = GLOBALS.with(|g| g.take());
    if let Some(main) = prog.funcs.iter_mut().find(|f| f.is_main) {
        // Package-level values (R11) are set by main's first statements.
        let init = globals.iter().enumerate().filter(|(_, (k, _))| !p.vars.contains(k)).map(|(id, (k, t))| LS::SetGlobal(id, lit_le(&p.globals[*k], t)));
        main.body.splice(0..0, init);
    }
    prog.globals = globals.into_iter().map(|(_, t)| t).collect();
    prog.recs = crate::lir::recs();
    drop_idle_regions(&mut prog);
    prog
}

/// Regions nothing is allocated in while they're current (a call's frame,
/// a loop's iteration region) aren't made, and region switches around
/// nothing are dropped. Functions whose results carry storage may allocate
/// in their caller's current region; unknown callees count as allocating.
fn drop_idle_regions(prog: &mut LProgram) {
    let known: std::collections::HashSet<String> = prog.funcs.iter().map(|f| f.name.clone()).collect();
    // Calls that may allocate in the caller's current region: results with
    // storage, and the generated map helpers that grow the map (they rely on
    // the caller making the map's region current around them).
    let storage: std::collections::HashSet<String> = prog
        .funcs
        .iter()
        .filter(|f| lty_has_storage(&f.ret) || f.name.starts_with("__map_set_") || f.name.starts_with("__map_new_"))
        .map(|f| f.name.clone())
        .collect();
    let rets = (storage, known);
    // Callees taking something with storage could store into it (in its
    // region, which may be the one an iteration's mark is on).
    let storage_params: std::collections::HashSet<String> = prog.funcs.iter().filter(|f| f.params.iter().any(|p| lty_has_storage(&f.vars[*p].ty))).map(|f| f.name.clone()).collect();
    // Task bodies (workers) have frames too (port-issues #277).
    for f in prog.funcs.iter_mut().filter(|f| !f.external).chain(prog.workers.iter_mut().map(|w| &mut w.func)) {
        let mut regions = vec![];
        collect_enters(&f.body, &mut regions);
        for (region, saved) in regions {
            // main's frame is where the program's region use starts.
            if f.is_main && f.vars.get(region).is_some_and(|v| v.name == "frame") {
                continue;
            }
            if !frame_allocates(&f.body, region, &rets) {
                drop_frame(&mut f.body, region, saved);
            } else if f.vars.get(region).is_some_and(|v| v.name == "iter") && iter_light_ok(&f.body, region, &rets, &storage_params) {
                let mark = f.vars.len();
                f.vars.push(LVar { name: "imark".into(), ty: LTy::Region });
                f.vars.push(LVar { name: "ilarges".into(), ty: LTy::Region });
                light_iter(&mut f.body, region, mark, mark + 1);
            }
        }
        drop_idle_switches(&mut f.body, &rets);
    }
}

fn collect_enters(ss: &[LS], out: &mut Vec<(V, V)>) {
    for s in ss {
        match s {
            LS::RegionEnter { region, saved } => {
                if !out.iter().any(|(r, _)| r == region) {
                    out.push((*region, *saved));
                }
            }
            LS::If(_, a, b) => {
                collect_enters(a, out);
                collect_enters(b, out);
            }
            LS::Loop(_, b) => collect_enters(b, out),
            _ => {}
        }
    }
}

/// An `extern def` as a function: its body is one C call, with Ints
/// converted to machine words (and back) in a `#![overflow(promote)]` file.
fn ffi_wrapper(p: &TProgram, sm: &SourceMap, opts: &Opts, f: &TFunc, sym: &str, prog: &RefCell<LProgram>) -> LFunc {
    let (lib, sym) = crate::ast::ffi_split(sym);
    let mut lw = Lw::new(p, sm, opts, f, f.overflow, ErrPath::Return(vec![]), prog);
    let params: Vec<V> = f.params.iter().map(|l| lw.var_of(*l)).collect();
    let ffi_ty = |t: &Ty| match t {
        Ty::Int => FfiTy::Int(IntKind::I64),
        Ty::IntK(k) => FfiTy::Int(*k),
        Ty::Float => FfiTy::F64,
        Ty::Bool => FfiTy::Bool,
        Ty::Str => FfiTy::Str,
        Ty::Array(_) => FfiTy::Bytes,
        Ty::Ptr => FfiTy::Ptr,
        _ => FfiTy::Unit,
    };
    let promote = f.overflow == Overflow::Promote;
    let (mut tys, mut args) = (vec![], vec![]);
    for (l, v) in f.params.iter().zip(&params) {
        let t = &f.locals[*l].ty;
        tys.push(ffi_ty(t));
        let a = LE::Var(*v);
        args.push(if promote && *t == Ty::Int { LE::Rt(Rt::PToI64, vec![a, LE::Loc(sym.to_string())]) } else { a });
    }
    let sig = FfiSig { sym: sym.to_string(), lib: lib.map(str::to_string), params: tys, ret: ffi_ty(&f.ret) };
    let idx = {
        let mut pr = prog.borrow_mut();
        match pr.externs.iter().position(|x| *x == sig) {
            Some(i) => i,
            None => {
                pr.externs.push(sig);
                pr.externs.len() - 1
            }
        }
    };
    let call = LE::Ffi(idx, args);
    let body = match &f.ret {
        Ty::Unit => vec![LS::Eval(call)],
        Ty::Int if promote => vec![LS::Return(Some(LE::ToP(Box::new(call))))],
        _ => vec![LS::Return(Some(call))],
    };
    LFunc { name: f.cname.clone(), params, vars: lw.vars, ret: fn_ret(f), body, external: false, is_main: false, labels: 0 }
}

/// A pool's header: slots (generation, value), live count, the contents'
/// region and its size after the last compaction (R3), and the free slots
/// (an array and how many of it are in use). A slot is live while its
/// generation is odd; a handle is its slot index | generation << 32, so a
/// handle to a removed (or since reused) slot is caught.
fn pool_header(t: LTy) -> LTy {
    LTy::Tup(vec![LTy::Arr(Box::new(LTy::Tup(vec![LTy::I64, t]))), LTy::I64, LTy::Region, LTy::I64, LTy::Arr(Box::new(LTy::I64)), LTy::I64])
}
const POOL_REGION: usize = 2;
const POOL_LIVE_BYTES: usize = 3;
const POOL_FREE: usize = 4;
const POOL_FREE_N: usize = 5;

/// A handle's slot index and generation.
fn handle_index(h: LE) -> LE {
    LE::Prim(Prim::And, vec![h, LE::I(0xffff_ffff)])
}
fn handle_gen(h: LE) -> LE {
    LE::Prim(Prim::ShrU, vec![h, LE::I(32)])
}
fn make_handle(i: LE, generation: LE) -> LE {
    LE::Prim(Prim::Or, vec![i, LE::Prim(Prim::Shl, vec![generation, LE::I(32)])])
}
fn gen_live(generation: LE) -> LE {
    LE::Cmp(Op::Eq, Box::new(LE::Prim(Prim::And, vec![generation, LE::I(1)])), Box::new(LE::I(1)), LTy::I64)
}

/// A fallible function returns (ok, value, error).
fn result_lty(ok: LTy, mode: Overflow) -> LTy {
    LTy::Tup(vec![LTy::Bool, ok_lty(ok), lty(&Ty::Error, mode)])
}

/// The value slot of a Result (nothing is a Bool placeholder).
fn ok_lty(t: LTy) -> LTy {
    if t == LTy::Unit { LTy::Bool } else { t }
}

fn fn_ret(f: &TFunc) -> LTy {
    let t = lty(&f.ret, f.overflow);
    if f.fallible && !f.is_main { result_lty(t, f.overflow) } else { t }
}

// ---------- types that contain themselves (R12, R13) ----------
//
// A struct, enum, interface or function type is *recursive* when its
// layout reaches itself: `struct Node { kids: [Node] }`, an interface
// with an implementor that holds the interface, a closure type with a
// lambda capturing a value that holds such closures. Its LIR type is an
// `LTy::Rec` (a named tuple), and its self-mentions are kept behind an
// array, so the layout is finite:
//
// - slices, maps, pools, mutexes, channels already are arrays;
// - an optional *field* (of a struct or an enum variant) whose value
//   leads back to its type is boxed: a zero- or one-element array
//   instead of (present, value);
// - a recursive interface value is (tag, [implementor 0], [implementor 1],
//   ...): each implementor's value in a one-element array (R13);
// - a recursive closure type holds each lambda's captures in a
//   one-element array.
//
// A box is never written through (an optional, an interface value and a
// closure's captures change only by being replaced), so copies share it.
// An empty box stands for the zero value: none, the first implementor's
// zero, the first lambda with zero captures.

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Node {
    Named(String),
    Iface(String),
    /// A function type, by `Ty::show`.
    Fn(String),
    /// `Error`: every error type's value.
    Err,
}

thread_local! {
    /// Struct and enum definitions met so far, by name.
    static NAMED: RefCell<HashMap<String, Ty>> = RefCell::new(HashMap::new());
    /// Whether each node's layout reaches itself.
    static RECURSIVE: RefCell<HashMap<Node, bool>> = RefCell::new(HashMap::new());
    /// The `LTy::Rec` of each recursive node, per overflow mode.
    static REC_LTY: RefCell<HashMap<(Node, bool), usize>> = RefCell::new(HashMap::new());
    /// Whether an optional field (container, payload by `Ty::show`) is boxed.
    static BOXED: RefCell<HashMap<(String, String), bool>> = RefCell::new(HashMap::new());
}

fn reset_recs() {
    HELPERS.with(|h| h.borrow_mut().clear());
    NAMED.with(|m| m.borrow_mut().clear());
    RECURSIVE.with(|m| m.borrow_mut().clear());
    REC_LTY.with(|m| m.borrow_mut().clear());
    BOXED.with(|m| m.borrow_mut().clear());
    crate::lir::set_recs(&[]);
}

fn named_def(n: &str) -> Option<Ty> {
    NAMED.with(|m| m.borrow().get(n).cloned()).or_else(|| rec_def(n))
}

fn node_of(t: &Ty) -> Option<Node> {
    match t {
        Ty::Struct(n, _) | Ty::Enum(n, _) => {
            NAMED.with(|m| {
                if !m.borrow().contains_key(n) {
                    m.borrow_mut().insert(n.clone(), t.clone());
                }
            });
            Some(Node::Named(n.clone()))
        }
        Ty::Rec(n) => Some(Node::Named(n.clone())),
        Ty::Iface(n) => Some(Node::Iface(n.clone())),
        Ty::Fn(..) => Some(Node::Fn(t.show())),
        Ty::Error => Some(Node::Err),
        _ => None,
    }
}

/// The nodes `t`'s layout reaches first (not looking inside them).
fn nodes_in(t: &Ty, out: &mut Vec<Node>) {
    if let Some(n) = node_of(t) {
        if !out.contains(&n) {
            out.push(n);
        }
        return;
    }
    match t {
        Ty::Array(x) | Ty::Fixed(x, _) | Ty::Seq(x, _) | Ty::Gen(x) | Ty::Opt(x) | Ty::Task(x) | Ty::Chan(x) | Ty::Pool(x) | Ty::Mutex(x) | Ty::Atomic(x) => nodes_in(x, out),
        Ty::Result(x) => {
            nodes_in(x, out);
            nodes_in(&Ty::Error, out);
        }
        Ty::Tuple(ts) => ts.iter().for_each(|x| nodes_in(x, out)),
        Ty::Map(k, v) => {
            nodes_in(k, out);
            nodes_in(v, out);
        }
        _ => {}
    }
}

fn fields_of_def(def: &Ty) -> Vec<Ty> {
    match def {
        Ty::Struct(_, fs) => fs.iter().map(|(_, t)| t.clone()).collect(),
        Ty::Enum(_, vs) => vs.iter().flat_map(|(_, fs)| fs.iter().map(|(_, t)| t.clone())).collect(),
        _ => vec![],
    }
}

/// What a node's layout is made of.
fn node_parts(n: &Node) -> Vec<Ty> {
    match n {
        Node::Named(name) => named_def(name).map(|d| fields_of_def(&d)).unwrap_or_default(),
        Node::Iface(name) => IFACES.with(|m| m.borrow().get(name).cloned().unwrap_or_default()),
        Node::Fn(key) => LAMBDAS.with(|l| l.borrow().iter().filter(|s| s.2.show() == *key).flat_map(|s| s.3.clone()).collect()),
        Node::Err => ERRORS.with(|e| e.borrow().clone()),
    }
}

fn succ(n: &Node) -> Vec<Node> {
    let mut out = vec![];
    for t in node_parts(n) {
        nodes_in(&t, &mut out);
    }
    out
}

/// Does the node's layout reach itself?
fn recursive(n: &Node) -> bool {
    if let Some(r) = RECURSIVE.with(|m| m.borrow().get(n).copied()) {
        return r;
    }
    let mut seen: Vec<Node> = vec![];
    let mut stack = succ(n);
    let mut found = false;
    while let Some(x) = stack.pop() {
        if x == *n {
            found = true;
            break;
        }
        if seen.contains(&x) {
            continue;
        }
        let next = succ(&x);
        seen.push(x);
        stack.extend(next);
    }
    RECURSIVE.with(|m| m.borrow_mut().insert(n.clone(), found));
    found
}

fn ty_recursive(t: &Ty) -> bool {
    match t {
        Ty::Struct(..) | Ty::Enum(..) | Ty::Rec(_) | Ty::Iface(_) | Ty::Fn(..) => node_of(t).is_some_and(|n| recursive(&n)),
        _ => false,
    }
}

/// Is an optional field of `container` holding a `payload` boxed? When the
/// payload leads back to the container without passing an array (or a
/// boxed interface value or closure).
fn boxed_opt(container: &str, payload: &Ty) -> bool {
    let key = (container.to_string(), payload.show());
    if let Some(b) = BOXED.with(|m| m.borrow().get(&key).copied()) {
        return b;
    }
    fn reaches(t: &Ty, target: &str, seen: &mut Vec<Node>) -> bool {
        match t {
            Ty::Struct(..) | Ty::Enum(..) | Ty::Rec(_) | Ty::Iface(_) | Ty::Fn(..) => {
                let n = node_of(t).unwrap();
                if n == Node::Named(target.to_string()) {
                    return true;
                }
                if seen.contains(&n) || (matches!(n, Node::Iface(_) | Node::Fn(_)) && recursive(&n)) {
                    return false;
                }
                seen.push(n.clone());
                node_parts(&n).iter().any(|x| reaches(x, target, seen))
            }
            Ty::Opt(x) | Ty::Fixed(x, _) | Ty::Result(x) => reaches(x, target, seen),
            Ty::Tuple(ts) => ts.iter().any(|x| reaches(x, target, seen)),
            _ => false,
        }
    }
    let b = reaches(payload, container, &mut vec![]);
    BOXED.with(|m| m.borrow_mut().insert(key, b));
    b
}

/// The payload of slot `k` of a struct or enum value (an enum's slot 0 is
/// its tag) when that slot is a boxed optional.
fn boxed_slot(container: &Ty, k: usize) -> Option<Ty> {
    let def = container.unrec();
    let (name, ft) = match &def {
        Ty::Struct(n, fs) => (n, fs.get(k)?.1.clone()),
        Ty::Enum(n, vs) => (n, vs.iter().flat_map(|(_, fs)| fs.iter().map(|(_, t)| t.clone())).nth(k.checked_sub(1)?)?),
        _ => return None,
    };
    let Ty::Opt(x) = ft else { return None };
    (recursive(&Node::Named(name.clone())) && boxed_opt(name, &x)).then(|| *x)
}

/// The LIR type of field `ft` of recursive struct or enum `container`.
fn slot_lty(container: &str, ft: &Ty, mode: Overflow) -> LTy {
    match ft {
        Ty::Opt(x) if boxed_opt(container, x) => LTy::Arr(Box::new(lty(x, mode))),
        t => lty(t, mode),
    }
}

/// The `LTy::Rec` of a recursive node (its body made on first use).
fn rec_lty(n: Node, mode: Overflow, body: impl FnOnce() -> LTy) -> LTy {
    let key = (n, mode == Overflow::Promote);
    if let Some(i) = REC_LTY.with(|m| m.borrow().get(&key).copied()) {
        return LTy::Rec(i);
    }
    let i = crate::lir::new_rec();
    REC_LTY.with(|m| m.borrow_mut().insert(key, i));
    let b = body();
    crate::lir::define_rec(i, b);
    LTy::Rec(i)
}

pub fn lty(t: &Ty, mode: Overflow) -> LTy {
    match t {
        Ty::Rec(_) => lty(&t.unrec(), mode),
        Ty::Struct(n, fs) if ty_recursive(t) => rec_lty(Node::Named(n.clone()), mode, || LTy::Tup(fs.iter().map(|(_, ft)| slot_lty(n, ft, mode)).collect())),
        Ty::Enum(n, vs) if ty_recursive(t) => rec_lty(Node::Named(n.clone()), mode, || LTy::Tup(std::iter::once(LTy::I64).chain(vs.iter().flat_map(|(_, fs)| fs.iter().map(|(_, ft)| slot_lty(n, ft, mode)))).collect())),
        Ty::Iface(n) if ty_recursive(t) => rec_lty(Node::Iface(n.clone()), mode, || {
            let impls = IFACES.with(|m| m.borrow().get(n).cloned().unwrap_or_default());
            LTy::Tup(std::iter::once(LTy::I64).chain(impls.iter().map(|t| LTy::Arr(Box::new(lty(t, mode))))).collect())
        }),
        Ty::Fn(..) if ty_recursive(t) => rec_lty(Node::Fn(t.show()), mode, || {
            LTy::Tup(std::iter::once(LTy::I64).chain(lambda_sites(t).iter().map(|(_, caps)| LTy::Arr(Box::new(LTy::Tup(caps.iter().map(|c| lty(c, mode)).collect()))))).collect())
        }),
        _ => lty_plain(t, mode),
    }
}

fn lty_plain(t: &Ty, mode: Overflow) -> LTy {
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
        // Error: (type tag, location, wrap context, then each error type's value).
        // Boxed: a one-element array of (tag, where, context, each error
        // type's value). A Result carries one pointer, and a success none.
        Ty::Error => LTy::Arr(Box::new(error_body(mode))),
        Ty::Result(t) => result_lty(lty(t, mode), mode),
        Ty::Task(t) => LTy::Task(Box::new(ok_lty(lty(t, mode)))),
        // A pool: a one-element array (shared, like a map) holding the slots
        // (present?, value) and the live count. A handle is a slot index.
        Ty::Pool(t) => LTy::Arr(Box::new(pool_header(lty(t, mode)))),
        // A one-element array (shared, like a map) of (lock, value).
        // So is an atomic of anything but an Int or a Bool (R11).
        Ty::Mutex(t) => LTy::Arr(Box::new(LTy::Tup(vec![LTy::Lock, lty(t, mode)]))),
        Ty::Atomic(t) if t.atomic_boxed() => LTy::Arr(Box::new(LTy::Tup(vec![LTy::Lock, lty(t, mode)]))),
        Ty::Atomic(_) => LTy::Atomic,
        Ty::Handle(_) | Ty::Ptr => LTy::I64,
        Ty::Chan(t) => LTy::Chan(Box::new(lty(t, mode))),
        Ty::Fn(..) => LTy::Tup(std::iter::once(LTy::I64).chain(lambda_sites(t).iter().map(|(_, caps)| LTy::Tup(caps.iter().map(|c| lty(c, mode)).collect()))).collect()),
        Ty::Iface(n) => {
            let impls = IFACES.with(|m| m.borrow().get(n).cloned().unwrap_or_default());
            LTy::Tup(std::iter::once(LTy::I64).chain(impls.iter().map(|t| lty(t, mode))).collect())
        }
        Ty::Enum(_, vs) => LTy::Tup(std::iter::once(LTy::I64).chain(vs.iter().flat_map(|(_, fs)| fs.iter().map(|(_, t)| lty(t, mode)))).collect()),
        Ty::Tuple(ts) => LTy::Tup(ts.iter().map(|t| lty(t, mode)).collect()),
        Ty::Range => LTy::Range,
        Ty::Gen(t) => LTy::Gen(Box::new(lty(t, mode))),
        Ty::Rec(_) => unreachable!(),
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
    try_mode: bool,
    consts: HashMap<LocalId, i64>,
    fixed_len: HashMap<LocalId, i64>,
    facts: HashMap<LocalId, Fact>,
    prog: &'a RefCell<LProgram>,
    /// In a fallible function: the ok value's type (returns are wrapped).
    res: Option<LTy>,
    /// R1 placement for this function's allocation sites, its frame region
    /// and the caller's (`dest`), and the class whose region is current.
    place: Option<&'a crate::regions::FnPlacement>,
    /// In a lambda's body: the placement of the function the lambda is
    /// written in (its sites are placed there; the body has no frame).
    lambda_place: Option<&'a crate::regions::FnPlacement>,
    /// In a task's, generator's or pmap worker's body: the enclosing
    /// function's placement, for the lambdas made there (port-issues #164).
    nested_place: Option<&'a crate::regions::FnPlacement>,
    frame: Option<(V, V)>,
    /// A light frame: a mark in the caller's region instead of a region
    /// of its own (see `light_frame`).
    light: Option<(V, V)>,
    ambient: crate::regions::Place,
    /// Each loop's iteration region (and the region saved when entering it).
    iter_vars: HashMap<usize, (V, V)>,
    /// Iteration regions open now, with the defer depth they belong to.
    iter_open: Vec<(usize, V, V)>,
    /// For the next inlined block: the defer depth its `next` unwinds to.
    next_depth: Option<usize>,
    /// Inside a store into a container: the container's region (R2).
    into: Vec<V>,
}

/// A stage of a pipeline with its pre-loop state.
struct Stage<'t> {
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
            try_mode: false,
            consts: HashMap::new(),
            fixed_len: HashMap::new(),
            facts: HashMap::new(),
            prog,
            res: None,
            place: None,
            lambda_place: None,
            nested_place: None,
            frame: None,
            light: None,
            ambient: crate::regions::Place::Frame,
            iter_vars: HashMap::new(),
            iter_open: vec![],
            next_depth: None,
            into: vec![],
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
            _ if self.f.pure && self.opts.release && !self.try_mode => Ovf::Unchecked,
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

    /// `then` is `x / 2^s` (s >= 1) and `cond` is `x.even?`, for an I64 local x
    /// (s == 1 only: evenness says nothing about higher bits).
    fn even_halving<'e>(&self, cond: &TExpr, then: &'e TExpr) -> Option<(&'e TExpr, i64)> {
        if self.promote() {
            return None;
        }
        let TK::M(M::Even, Some(r), _, _) = &cond.kind else { return None };
        let TK::Local(l) = r.kind else { return None };
        let TK::Bin(BinOp::Div, x, k) = &then.kind else { return None };
        match (&x.kind, &k.kind) {
            (TK::Local(m), TK::Int(2)) if *m == l && x.ty == Ty::Int => Some((x, 1)),
            _ => None,
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
                _ => ty_range(&e.ty),
            },
            TK::Bin(op, a, b) => {
                let (Some(a), Some(b)) = (self.interval(a), self.interval(b)) else { return ty_range(&e.ty) };
                let combos = |f: fn(i64, i64) -> Option<i64>| -> Option<(i64, i64)> {
                    let vs = [f(a.0, b.0)?, f(a.0, b.1)?, f(a.1, b.0)?, f(a.1, b.1)?];
                    Some((*vs.iter().min()?, *vs.iter().max()?))
                };
                let r = match op {
                    BinOp::Add => combos(i64::checked_add),
                    BinOp::Sub => combos(i64::checked_sub),
                    BinOp::Mul => combos(i64::checked_mul),
                    // x & y with a non-negative side: within [0, that side's max].
                    BinOp::BitAnd if a.0 >= 0 && b.0 >= 0 => Some((0, a.1.min(b.1))),
                    BinOp::BitAnd if a.0 >= 0 => Some((0, a.1)),
                    BinOp::BitAnd if b.0 >= 0 => Some((0, b.1)),
                    // A non-negative value shifted right by a constant count.
                    BinOp::Shr if a.0 >= 0 && b.0 == b.1 && (0..64).contains(&b.0) => Some((a.0 >> b.0, a.1 >> b.0)),
                    _ => None,
                };
                // Wrapping or narrow arithmetic: the result type's range bounds it.
                match (r, ty_range(&e.ty)) {
                    (Some((lo, hi)), Some((tlo, thi))) if lo < tlo || hi > thi => Some((tlo, thi)),
                    (None, t) => t,
                    (r, _) => r,
                }
            }
            // A conversion keeps a value that fits the target type; otherwise
            // the result is somewhere in the target's range.
            TK::M(M::Conv(..), Some(r), _, _) => {
                let t = ty_range(&e.ty);
                match (self.interval(r), t) {
                    (Some((lo, hi)), Some((tlo, thi))) if lo >= tlo && hi <= thi => Some((lo, hi)),
                    (Some((lo, hi)), None) if e.ty == Ty::Int && r.ty != Ty::IntK(crate::ast::IntKind::U64) => Some((lo, hi)),
                    _ => t,
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
            _ => ty_range(&e.ty),
        }
    }

    /// Bounds check needed? `None` = proven in range (release only).
    fn index_check(&self, arr: &TExpr, idx: &TExpr) -> Option<String> {
        let loc = Some(self.loc(idx.span));
        if !self.opts.release {
            return loc;
        }
        // A `[T; N]` value has N elements, whatever expression made it.
        if let Ty::Fixed(_, n) = &arr.ty {
            if let Some((lo, hi)) = self.interval(idx) {
                if lo >= 0 && (hi as u64) < *n {
                    return None;
                }
            }
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
                    // A last expression that never finishes (every branch
                    // returns) has no value to return.
                    if let TStmt::Expr(e) = s {
                        if e.ty == Ty::Never {
                            lw.stmt(s);
                            continue;
                        }
                        let mut v = lw.expr(e);
                        if lw.has_defers(0) {
                            let t = lw.lty(&e.ty);
                            v = lw.bind(v, t);
                            lw.emit_defers(0);
                        }
                        let v = if lw.res.is_some() { lw.ok_result(v) } else { v };
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
        (from == 0 && self.frame.is_some()) || self.iter_open.iter().any(|(d, _, _)| *d >= from) || self.defers[from.min(self.defers.len())..].iter().any(|d| !d.is_empty())
    }

    /// The iteration region of loop `key`, if its body allocates per iteration.
    fn iter_region(&mut self, key: usize) -> Option<(V, V)> {
        if !self.place.is_some_and(|pl| pl.loops.contains(&key)) || self.frame.is_none() {
            return None;
        }
        if let Some(r) = self.iter_vars.get(&key) {
            return Some(*r);
        }
        let r = (self.new_var("iter", LTy::Region), self.new_var("outer", LTy::Region));
        self.iter_vars.insert(key, r);
        Some(r)
    }

    /// Open loop `key`'s iteration region (as a defer level, so every way
    /// out of the loop's body frees it). Returns what `close_iter` needs.
    fn open_iter(&mut self, key: usize) -> Option<(V, V, crate::regions::Place)> {
        let (r, sv) = self.iter_region(key)?;
        self.defers.push(vec![]);
        let depth = self.defers.len() - 1;
        self.emit(LS::RegionEnter { region: r, saved: sv });
        self.iter_open.push((depth, r, sv));
        let prev = std::mem::replace(&mut self.ambient, crate::regions::Place::Iter(key));
        Some((r, sv, prev))
    }

    fn close_iter(&mut self, open: Option<(V, V, crate::regions::Place)>) {
        if let Some((r, sv, prev)) = open {
            self.ambient = prev;
            self.emit(LS::RegionExit { region: r, saved: sv });
            self.iter_open.pop();
            self.defers.pop();
        }
    }

    /// Run the deferred code of blocks `from..` (innermost first), as when
    /// leaving them. Errors inside deferred code clean up only outer blocks.
    fn emit_defers(&mut self, from: usize) {
        if !self.has_defers(from) {
            return;
        }
        let saved = self.defers.clone();
        let saved_iters = self.iter_open.clone();
        for depth in (from..saved.len()).rev() {
            // Errors in deferred code clean up only the outer blocks; a scope
            // inside it sits below an empty level (so its own cleanup never
            // looks like leaving the function, which frees the frame).
            self.defers.truncate(depth);
            self.defers.push(vec![]);
            // The iteration regions of the levels being left are freed once,
            // below: the deferred code's own scopes must not free them too.
            self.iter_open.retain(|(d, _, _)| *d < depth);
            for e in saved[depth].iter().rev() {
                self.stmt(&TStmt::Expr(e.clone()));
            }
        }
        self.defers = saved;
        self.iter_open = saved_iters;
        // Leaving loops: their iteration regions go (innermost first).
        for (d, r, sv) in self.iter_open.clone().into_iter().rev() {
            if d >= from {
                self.emit(LS::RegionExit { region: r, saved: sv });
            }
        }
        // Leaving the function: its region goes too.
        if from == 0 {
            if let Some((mark, larges)) = self.light {
                self.emit(LS::Eval(LE::Rt(Rt::RegionReset, vec![LE::Var(mark), LE::Var(larges)])));
            } else if let Some((frame, dest)) = self.frame {
                self.emit(LS::RegionExit { region: frame, saved: dest });
            }
        }
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
            if matches!(ty, Ty::Unit | Ty::Never) {
                // No value to keep (a call returning nothing has none to bind).
                if !matches!(val, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                    self.emit(LS::Eval(val));
                }
                val = LE::Unit;
            } else {
                let t = self.lty(ty);
                val = self.bind(val, t);
            }
            self.emit_defers(depth);
        }
        self.defers.pop();
        val
    }

    fn stmt(&mut self, s: &TStmt) {
        match s {
            TStmt::Expr(e) => {
                let v = self.expr(e);
                if !matches!(v, LE::Var(_) | LE::I(_) | LE::B(_) | LE::S(_) | LE::SB(_) | LE::Unit | LE::Field(..) | LE::Cmp(..)) {
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
                // Each iteration gets a fresh region: entered before the loop,
                // renewed at the top of every iteration, left after it.
                let key = s as *const TStmt as usize;
                let open = self.open_iter(key);
                let inner = self.sub(|lw| {
                    lw.compact_owners(key);
                    if let Some((r, sv, _)) = open {
                        lw.emit(LS::RegionExit { region: r, saved: sv });
                        lw.emit(LS::RegionEnter { region: r, saved: sv });
                    }
                    let cv = lw.expr(c);
                    lw.emit(LS::If(LE::Not(Box::new(cv)), vec![LS::Break(l)], vec![]));
                    lw.next_target.push((Some(l), lw.defers.len()));
                    lw.break_target.push((l, lw.defers.len()));
                    lw.stmts(body);
                    lw.break_target.pop();
                    lw.next_target.pop();
                });
                self.emit(LS::Loop(l, inner));
                self.close_iter(open);
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
                // `return nil` (or any Unit value) from a def returning
                // nothing: evaluate it, return no value (C's void).
                let v = match v {
                    Some((x, LTy::Unit)) => {
                        if !matches!(x, LE::Unit) {
                            self.emit(LS::Eval(x));
                        }
                        None
                    }
                    v => v.map(|(x, _)| x),
                };
                let v = match (&self.res, v) {
                    (Some(_), v) => Some(self.ok_result(v.unwrap_or(LE::B(false)))),
                    (None, v) => v,
                };
                self.emit(LS::Return(v));
            }
            TStmt::Fail(e, _) => {
                let v = self.expr(e);
                let v = self.bind(v, self.lty(&Ty::Error));
                self.fail(v);
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
        // R2: storing into a container whose storage is the caller's: what's
        // stored (and any growth) goes in the region the container lives in.
        if self.place.is_some_and(|pl| pl.into) && self.frame.is_some() {
            let root = match &e.kind {
                TK::M(M::Push | M::MapSet | M::MapDel | M::CopyInto | M::PoolAdd | M::PoolSet, Some(r), ..) => match &r.kind {
                    TK::Local(l) => Some(*l),
                    _ => None,
                },
                TK::M(M::CopyInto, None, args, _) => match args.first().map(|a| &a.kind) {
                    Some(TK::Local(l)) => Some(*l),
                    _ => None,
                },
                TK::IndexAssign(l, ..) | TK::PlaceAssign(l, ..) => Some(*l),
                _ => None,
            };
            // Storing a value without storage (a byte, a word) into an
            // existing element, or copying such elements, allocates nothing:
            // no region to look up (a hot loop's `s[i] = digit`).
            let promote = self.promote();
            let flat_store = match &e.kind {
                TK::IndexAssign(_, _, x) | TK::PlaceAssign(_, _, _, x) => !crate::regions::has_storage(&x.ty) && !(promote && crate::regions::contains_int(&x.ty)),
                TK::M(M::CopyInto, None, args, _) => args.first().is_some_and(|a| matches!(&a.ty, Ty::Array(t) | Ty::Fixed(t, _) if !crate::regions::has_storage(t) && !(promote && crate::regions::contains_int(t)))),
                _ => false,
            };
            let root = if flat_store { None } else { root };
            if let Some(l) = root.filter(|l| matches!(self.lty(&self.f.locals[*l].ty), LTy::Arr(_) | LTy::Str)) {
                let r = self.tmp(LTy::Region);
                let v = self.var_of(l);
                let region = match self.owner_region(l) {
                    Some(reg) => reg,
                    None if matches!(self.lty(&self.f.locals[l].ty), LTy::Arr(_)) => LE::ViewRegion(Box::new(LE::Var(v))),
                    None => LE::RegionOf(Box::new(LE::Var(v))),
                };
                self.emit(LS::Set(r, region));
                self.into.push(r);
                let out = self.expr_placed(e);
                self.into.pop();
                return out;
            }
        }
        self.expr_placed(e)
    }

    fn expr_placed(&mut self, e: &TExpr) -> LE {
        // R3: an owning container: its contents in a child region of this
        // call's, its header (what variables point to) here.
        if let (Some(pl), Some((frame, _))) = (self.place, self.frame) {
            let me = e as *const TExpr as usize;
            if pl.owners.values().any(|o| o.def_site == me) {
                let r = self.tmp(LTy::Region);
                self.emit(LS::Set(r, LE::RegionNew(Box::new(LE::Var(frame)))));
                let saved = self.tmp(LTy::Region);
                self.emit(LS::RegionUse { region: LE::Var(r), saved });
                let lt = self.lty(&e.ty);
                let inner = self.expr_in(e);
                let inner = self.bind(inner, lt.clone());
                self.emit(LS::RegionRestore(saved));
                let LTy::Arr(h) = &lt else { unreachable!() };
                let hdr = self.tmp(lt.clone());
                let header = LE::Index { arr: Box::new(inner), idx: Box::new(LE::I(0)), check: None };
                // The header is allocated here (in the call's region), not in r.
                let fsaved = self.tmp(LTy::Region);
                self.emit(LS::RegionUse { region: LE::Var(frame), saved: fsaved });
                self.emit(LS::Set(hdr, LE::ArrLit((**h).clone(), vec![header])));
                self.emit(LS::RegionRestore(fsaved));
                let (rf, bf) = self.region_fields(&e.ty);
                self.emit(LS::SetPlace { var: hdr, steps: vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(rf)], val: LE::Var(r) });
                self.emit(LS::SetPlace { var: hdr, steps: vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(bf)], val: LE::RegionBytes(Box::new(LE::Var(r))) });
                return LE::Var(hdr);
            }
        }
        // R1: an allocation that must live elsewhere than the current
        // region is made with that region current (and forced to a value
        // before switching back: LIR expressions run where they're used).
        if let Some((region, class)) = self.site_region(e) {
            let saved = self.tmp(LTy::Region);
            self.emit(LS::RegionUse { region, saved });
            let prev = std::mem::replace(&mut self.ambient, class);
            let v = self.expr_in(e);
            let lt = self.lty(&e.ty);
            let v = if lt == LTy::Unit {
                if !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                    self.emit(LS::Eval(v));
                }
                LE::Unit
            } else {
                self.bind(v, lt)
            };
            self.ambient = prev;
            self.emit(LS::RegionRestore(saved));
            return v;
        }
        self.expr_in(e)
    }

    /// The region an allocating expression must be made in, when that isn't
    /// the current one (and its placement class).
    fn site_region(&mut self, e: &TExpr) -> Option<(LE, crate::regions::Place)> {
        // A call whose result holds no storage: where it's made doesn't matter
        // (the callee places its own allocations).
        let ctor = matches!(e.kind, TK::Call(..) | TK::M(M::StructNew | M::TupleNew | M::VariantNew(_) | M::EnumNew, ..));
        if ctor && !crate::regions::has_storage(&e.ty) && !(self.promote() && crate::regions::contains_int(&e.ty)) {
            return None;
        }
        // In a lambda's body, what the analysis sends to the program region
        // (stored into a captured variable that outlives the call, say) must
        // go there: the region current when the lambda runs may be the
        // frame of whoever called it, freed when that returns.
        if !crate::regions::allocates(e, self.promote()) {
            return None;
        }
        self.placed(e as *const TExpr as usize)
    }

    /// Where allocation site `key` (by address) is placed, when that isn't
    /// the current region (and its placement class).
    fn placed(&mut self, key: usize) -> Option<(LE, crate::regions::Place)> {
        if let (Some(pl), None) = (self.lambda_place, self.frame) {
            use crate::regions::{LPlace, Place};
            // Stored into a parameter's or a capture's storage: the region
            // that storage lives in (port-issues #164).
            match pl.lambda_sites.get(&key).copied() {
                Some(LPlace::Cur) => return None,
                Some(LPlace::Program) => {
                    return if self.ambient != Place::Global { Some((LE::RegionProgram, Place::Global)) } else { None };
                }
                Some(LPlace::Storage(x)) => {
                    let v = self.var_of(x);
                    let t = self.f.locals[x].ty.clone();
                    let r = self.storage_region(LE::Var(v), &t).unwrap_or(LE::RegionProgram);
                    return Some((r, Place::Into(x)));
                }
                None => {}
            }
            {
                let class = pl.sites.get(&key).copied().unwrap_or(Place::Global);
                if matches!(class, Place::Global | Place::Into(_)) && self.ambient != Place::Global {
                    return Some((LE::RegionProgram, Place::Global));
                }
            }
            return None;
        }
        if let (Some(pl), Some((frame, dest))) = (self.place, self.frame) {
            {
                use crate::regions::Place;
                let mut class = pl.sites.get(&key).copied().unwrap_or(Place::Global);
                // A loop not open here (can't happen, but never guess): the frame.
                if let Place::Iter(k) = class {
                    if !self.iter_open.iter().any(|(_, r, _)| Some(r) == self.iter_vars.get(&k).map(|x| &x.0)) {
                        class = Place::Frame;
                    }
                }
                if class != self.ambient {
                    let region = match class {
                        Place::Iter(k) => LE::Var(self.iter_vars[&k].0),
                        Place::Frame => LE::Var(frame),
                        Place::Ret => LE::Var(dest),
                        Place::Into(p) => match self.into.last().copied() {
                            Some(r) => LE::Var(r),
                            None if self.place.is_some_and(|pl| pl.owner_alias.contains_key(&p)) => self.owner_region(p).unwrap(),
                            // Where the parameter's own storage lives (a
                            // `!` method's receiver: the view's region).
                            None => {
                                let v = self.var_of(p);
                                let t = self.f.locals[p].ty.clone();
                                self.storage_region(LE::Var(v), &t).unwrap_or(LE::RegionProgram)
                            }
                        },
                        Place::Global => LE::RegionProgram,
                    };
                    return Some((region, class));
                }
            }
        }
        None
    }

    /// The region the storage of value `v` (of type `t`) lives in, when it
    /// has a single pointer to find it by: an array, map or string, or a
    /// struct or tuple whose only field with storage is one (port-issues
    /// #164: `w.header.set!` on a parameter `w`). Through a null pointer
    /// (an empty slice, a zero map) it's the program region. None when the
    /// value has several pointers (they may live in different regions).
    fn storage_region(&mut self, v: LE, t: &Ty) -> Option<LE> {
        use crate::regions::{contains_int, has_storage};
        let promote = self.promote();
        let storage = |t: &Ty| has_storage(t) || (promote && contains_int(t));
        let fields: Vec<Ty> = match t {
            Ty::Struct(_, fs) => fs.iter().map(|(_, t)| t.clone()).collect(),
            Ty::Tuple(ts) => ts.clone(),
            Ty::Rec(_) | Ty::Opt(_) => return None,
            _ => {
                return match self.lty(t) {
                    LTy::Arr(_) => Some(LE::ViewRegion(Box::new(v))),
                    LTy::Str => Some(LE::RegionOf(Box::new(v))),
                    _ => None,
                };
            }
        };
        let with: Vec<usize> = (0..fields.len()).filter(|k| storage(&fields[*k])).collect();
        match with.as_slice() {
            [k] => self.storage_region(LE::Field(Box::new(v), *k), &fields[*k]),
            _ => None,
        }
    }

    fn expr_in(&mut self, e: &TExpr) -> LE {
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
            TK::Bang(l, steps, view, call) => self.bang(*l, steps, *view, call, e),
            TK::Format(pieces, args) => self.format(pieces, args),
            TK::Seq(ss) => self.scoped_value(ss, &e.ty),
            TK::Zero => zero_le(&self.lty(&e.ty)),
            TK::None => {
                let t = self.lty(&e.ty);
                let ts = t.tup_fields();
                let z = zero_le(&ts[1]);
                LE::Tup(t, vec![LE::B(false), z])
            }
            TK::Some(x) => {
                let t = self.lty(&e.ty);
                let v = self.arg(x);
                LE::Tup(t, vec![LE::B(true), v])
            }
            TK::Select(arms, default) => {
                let mut cases = vec![];
                let mut recv_vars = vec![];
                for a in arms {
                    match a {
                        TSelArm::Recv { ch, .. } => {
                            let c = self.expr(ch);
                            let lt = self.lty(&ch.ty);
                            let c = self.bind(c, lt.clone());
                            let LTy::Chan(el) = lt else { unreachable!() };
                            let (ok, val) = (self.tmp(LTy::Bool), self.tmp((*el).clone()));
                            self.emit(LS::Set(val, zero_le(&el)));
                            cases.push(SelCase::Recv { ch: c, ok, val });
                            recv_vars.push(Some((ok, val)));
                        }
                        TSelArm::Send { ch, val, .. } => {
                            let c = self.expr(ch);
                            let c = self.bind(c, self.lty(&ch.ty));
                            let v = self.arg(val);
                            let v = self.bind(v, self.lty(&val.ty));
                            cases.push(SelCase::Send { ch: c, val: v });
                            recv_vars.push(None);
                        }
                    }
                }
                let dst = self.tmp(LTy::I64);
                let n = cases.len();
                self.emit(LS::Select { cases, default: default.is_some(), dst });
                for (i, a) in arms.iter().enumerate() {
                    let body = self.sub(|lw| {
                        if let (TSelArm::Recv { bind: Some(l), ch, .. }, Some((ok, val))) = (a, recv_vars[i]) {
                            let Ty::Chan(t) = &ch.ty else { unreachable!() };
                            let ot = lw.lty(&Ty::Opt(t.clone()));
                            let v = lw.var_of(*l);
                            lw.emit(LS::Set(v, LE::Tup(ot, vec![LE::Var(ok), LE::Var(val)])));
                        }
                        let b = match a {
                            TSelArm::Recv { body, .. } | TSelArm::Send { body, .. } => body,
                        };
                        lw.stmts(b);
                    });
                    self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(LE::Var(dst)), Box::new(LE::I(i as i64)), LTy::I64), body, vec![]));
                }
                if let Some(d) = default {
                    let body = self.sub(|lw| lw.stmts(d));
                    self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(LE::Var(dst)), Box::new(LE::I(n as i64)), LTy::I64), body, vec![]));
                }
                LE::Unit
            }
            TK::Str(s) => LE::S(s.clone()),
            TK::Bytes(b) => LE::SB(b.clone()),
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
                let mut check = self.index_check(&arr_t, i);
                let iv = self.expr(i);
                let mut iv = self.int_in_t(iv, &i.ty, i.span);
                if self.try_mode {
                    iv = self.bind(iv, LTy::I64);
                }
                let vv = self.expr(v);
                let vt = self.lty(&v.ty);
                let vv = self.bind(vv, vt);
                let var = self.var_of(*l);
                if self.try_mode {
                    self.guard(Self::out_of_bounds(&LE::Var(var), &iv), "index out of bounds", i.span, "IndexError", 0);
                    check = None;
                }
                self.emit(LS::SetIndex { arr: var, idx: iv, val: vv.clone(), check });
                vv
            }
            TK::Bin(op @ (BinOp::Eq | BinOp::Ne), a, b) if matches!(a.ty, Ty::Handle(_) | Ty::Ptr) => {
                let (x, y) = (self.expr(a), self.expr(b));
                LE::Cmp(if *op == BinOp::Eq { Op::Eq } else { Op::Ne }, Box::new(x), Box::new(y), LTy::I64)
            }
            TK::Bin(BinOp::Add, a, b) if e.ty == Ty::Str => {
                let (x, y) = (self.expr(a), self.expr(b));
                LE::Rt(Rt::StrCat, vec![x, y])
            }
            TK::Bin(BinOp::Add, a, b) if matches!(e.ty, Ty::Array(_)) => {
                let at = self.lty(&e.ty);
                let et = at.clone().arr_elem_lty();
                let x = self.expr(a);
                let x = self.bind(x, at.clone());
                let y = self.expr(b);
                let y = self.bind(y, at.clone());
                let len = |v: &LE| LE::Len(Box::new(v.clone()));
                let n = LE::Arith(Op::Add, Box::new(len(&x)), Box::new(len(&y)), Ovf::Unchecked);
                let c = self.tmp(at);
                self.emit(LS::Set(c, LE::ArrWithCap(et, Box::new(n))));
                for src in [x, y] {
                    let i = self.tmp(LTy::I64);
                    self.emit(LS::Set(i, LE::I(0)));
                    let l = self.label();
                    let body = self.sub(|lw| {
                        lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(len(&src)), LTy::I64), vec![LS::Break(l)], vec![]));
                        lw.emit(LS::Push(c, LE::Index { arr: Box::new(src.clone()), idx: Box::new(LE::Var(i)), check: None }));
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    });
                    self.emit(LS::Loop(l, body));
                }
                LE::Var(c)
            }
            TK::Bin(op, a, b) => self.binary(*op, a, b, e),
            TK::Neg(x) => {
                let v = self.expr(x);
                if e.ty == Ty::Float {
                    return LE::FNeg(Box::new(v));
                }
                if self.promote() {
                    LE::PArith(Op::Sub, Box::new(LE::ToP(Box::new(LE::I(0)))), Box::new(v))
                } else if self.try_mode {
                    self.checked_arith(Op::Sub, LE::I(0), v, e.span)
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
                let (sa, va) = match self.even_halving(c, a) {
                    // `x.even? ? x / 2^s : ..`: x is even there, so the
                    // division is a plain shift (no rounding fix-up).
                    Some((x, s)) => {
                        let xv = self.expr(x);
                        (vec![], LE::Prim(Prim::ShrS, vec![xv, LE::I(s)]))
                    }
                    None => self.sub_val(|lw| lw.expr(a)),
                };
                let (sb, vb) = self.sub_val(|lw| lw.expr(b));
                // A value-less `if` (branches end in calls returning nothing):
                // run the branch expressions as statements.
                if matches!(e.ty, Ty::Unit | Ty::Never) {
                    let mut sa = sa;
                    let mut sb = sb;
                    for (ss, v) in [(&mut sa, va), (&mut sb, vb)] {
                        if !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                            ss.push(LS::Eval(v));
                        }
                    }
                    self.emit(LS::If(cv, sa, sb));
                    return LE::Unit;
                }
                if sa.is_empty() && sb.is_empty() {
                    return LE::Cond(Box::new(cv), Box::new(va), Box::new(vb));
                }
                let t = self.lty(&e.ty);
                let t = self.tmp(t);
                let mut sa = sa;
                if a.ty != Ty::Never {
                    sa.push(LS::Set(t, va));
                }
                let mut sb = sb;
                if b.ty != Ty::Never {
                    sb.push(LS::Set(t, vb));
                }
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
                self.guard(bad, "index out of bounds", i.span, "IndexError", 0);
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
                self.guard(bad, "slice bounds out of range", e.span, "IndexError", 1);
                let len = self.tmp(LTy::I64);
                self.emit(LS::Set(len, LE::Arith(Op::Sub, Box::new(end), Box::new(lo_v.clone()), Ovf::Unchecked)));
                if is_str {
                    // A non-literal length: always a substring, even when empty.
                    LE::Rt(Rt::StrByte, vec![av, lo_v, LE::Var(len)])
                } else {
                    LE::Slice(at, Box::new(av), Box::new(lo_v), Box::new(LE::Var(len)))
                }
            }
            // The element reads of a slice `==` (check.rs arr_eq) are always in
            // range: no IndexError under `~`.
            TK::Index(a, i) if self.try_mode && !matches!(&i.kind, TK::Local(l) if self.f.locals[*l].name.starts_with("_eqi")) => {
                let av = self.expr(a);
                let av = self.bind_arr(av, self.lty(&a.ty));
                let iv = self.expr(i);
                let iv = self.int_in_t(iv, &i.ty, i.span);
                let iv = self.bind(iv, LTy::I64);
                self.guard(Self::out_of_bounds(&av, &iv), "index out of bounds", i.span, "IndexError", 0);
                LE::Index { arr: Box::new(av), idx: Box::new(iv), check: None }
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
            TK::Panic(x) => {
                let m = self.expr(x);
                self.emit(LS::PanicStr(m));
                LE::Unit
            }
            TK::Puts(x) => {
                let v = self.expr(x);
                if matches!(x.ty, Ty::Opt(_) | Ty::Map(..) | Ty::Array(_) | Ty::Fixed(..) | Ty::Struct(..) | Ty::Tuple(_) | Ty::Enum(..) | Ty::Iface(_) | Ty::Error | Ty::Handle(_)) {
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
    /// Call the generated function `name` (making it on first use): its
    /// parameters are `params`, its body what `body` makes of them. For
    /// work on types that contain themselves (R12), which can't be done
    /// inline. It has no region of its own: it allocates in its caller's.
    fn helper(&mut self, name: String, params: Vec<LTy>, ret: LTy, body: impl FnOnce(&mut Lw<'a>, Vec<LE>) -> LE) -> String {
        if HELPERS.with(|h| h.borrow_mut().insert(name.clone())) {
            let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Return(vec![]), self.prog);
            let pvs: Vec<V> = params.into_iter().map(|t| w.new_var("p", t)).collect();
            let args: Vec<LE> = pvs.iter().map(|v| LE::Var(*v)).collect();
            let (mut stmts, v) = w.sub_val(|w| body(w, args));
            stmts.push(LS::Return(Some(v)));
            let func = LFunc { name: name.clone(), params: pvs, vars: w.vars, ret, body: stmts, external: false, is_main: false, labels: w.labels };
            self.prog.borrow_mut().funcs.push(func);
        }
        name
    }

    /// Slot `k` of a struct or enum value `v` of type `container` (a
    /// boxed optional field read as an optional).
    fn slot_read(&mut self, container: &Ty, v: LE, k: usize) -> LE {
        let f = LE::Field(Box::new(v), k);
        match boxed_slot(container, k) {
            Some(x) => self.opt_unbox(f, &x),
            None => f,
        }
    }

    /// A boxed optional (a zero- or one-element array) as a `x?`.
    fn opt_unbox(&mut self, b: LE, x: &Ty) -> LE {
        let xt = self.lty(x);
        let ot = self.lty(&Ty::Opt(Box::new(x.clone())));
        let b = self.bind(b, LTy::Arr(Box::new(xt.clone())));
        let o = self.tmp(ot.clone());
        self.emit(LS::Set(o, LE::Tup(ot.clone(), vec![LE::B(false), zero_le(&xt)])));
        let got = LE::Tup(ot, vec![LE::B(true), LE::Index { arr: Box::new(b.clone()), idx: Box::new(LE::I(0)), check: None }]);
        self.emit(LS::If(LE::Cmp(Op::Gt, Box::new(LE::Len(Box::new(b))), Box::new(LE::I(0)), LTy::I64), vec![LS::Set(o, got)], vec![]));
        LE::Var(o)
    }

    /// A `x?` boxed, for an optional field that leads back to its type.
    fn opt_box(&mut self, o: LE, x: &Ty) -> LE {
        let xt = self.lty(x);
        let ot = self.lty(&Ty::Opt(Box::new(x.clone())));
        let at = LTy::Arr(Box::new(xt.clone()));
        let o = self.bind(o, ot);
        let b = self.tmp(at);
        self.emit(LS::Set(b, LE::ArrWithCap(xt.clone(), Box::new(LE::I(0)))));
        let set = LS::Set(b, LE::ArrLit(xt, vec![LE::Field(Box::new(o.clone()), 1)]));
        self.emit(LS::If(LE::Field(Box::new(o), 0), vec![set], vec![]));
        LE::Var(b)
    }

    /// The value of a box (a one-element array), or the zero value of `t`
    /// when it is empty (a zero interface value or closure, R13).
    fn unbox(&mut self, b: LE, t: &LTy) -> LE {
        let b = self.bind(b, LTy::Arr(Box::new(t.clone())));
        let x = self.tmp(t.clone());
        self.emit(LS::Set(x, zero_le(t)));
        let got = LE::Index { arr: Box::new(b.clone()), idx: Box::new(LE::I(0)), check: None };
        self.emit(LS::If(LE::Cmp(Op::Gt, Box::new(LE::Len(Box::new(b))), Box::new(LE::I(0)), LTy::I64), vec![LS::Set(x, got)], vec![]));
        LE::Var(x)
    }

    /// Implementor `k`'s value of interface value `v` (bound) of type `it`.
    fn iface_get(&mut self, v: LE, it: &Ty, k: usize) -> LE {
        let f = LE::Field(Box::new(v), k + 1);
        if !ty_recursive(it) {
            return f;
        }
        let Ty::Iface(n) = it else { unreachable!() };
        let impl_t = self.p.ifaces[n][k].0.clone();
        let lt = self.lty(&impl_t);
        self.unbox(f, &lt)
    }

    /// The interface value of type `it` holding `v` as implementor `k`.
    fn make_iface(&mut self, it: &Ty, k: usize, v: LE) -> LE {
        let lt = self.lty(it);
        let ts = lt.tup_fields();
        let boxed = ty_recursive(it);
        let mut vals = vec![LE::I(k as i64)];
        for (j, t) in ts[1..].iter().enumerate() {
            vals.push(if j != k {
                zero_le(t)
            } else if boxed {
                LE::ArrLit(t.clone().arr_elem_lty(), vec![v.clone()])
            } else {
                v.clone()
            });
        }
        LE::Tup(lt, vals)
    }

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

    /// `call` (an implementor's method) as the interface method's result: an
    /// infallible method satisfying a fallible interface method always succeeds.
    fn iface_result(&mut self, e: &TExpr, fid: FuncId, call: LE) -> (Vec<LS>, LE) {
        let f = &self.p.funcs[fid];
        // A covariant result (`def clone -> Hash` returning a Digest): wrap it.
        if let (Ty::Iface(n), Ty::Struct(..) | Ty::Enum(..)) = (&e.ty, &f.ret) {
            let impls = self.p.ifaces.get(n).cloned().unwrap_or_default();
            let k = impls.iter().position(|(t, _)| *t == f.ret).expect("covariant result implements the interface");
            return (vec![], self.make_iface(&e.ty, k, call));
        }
        let Ty::Result(t) = &e.ty else { return (vec![], call) };
        if f.fallible {
            return (vec![], call);
        }
        let rt = result_lty(ok_lty(self.lty(t)), self.mode);
        let et = self.lty(&Ty::Error);
        if **t == Ty::Unit {
            (vec![LS::Eval(call)], LE::Tup(rt, vec![LE::B(true), LE::B(false), zero_le(&et)]))
        } else {
            (vec![], LE::Tup(rt, vec![LE::B(true), call, zero_le(&et)]))
        }
    }

    /// A successful Result of this function's type.
    fn ok_result(&mut self, v: LE) -> LE {
        let ok = self.res.clone().expect("in a fallible function");
        let rt = result_lty(ok.clone(), self.mode);
        let et = self.lty(&Ty::Error);
        LE::Tup(rt, vec![LE::B(true), v, zero_le(&et)])
    }

    /// Leave with error `err` (an Error value): return it from a fallible
    /// function, or print it and exit 1 at the top level.
    fn fail(&mut self, err: LE) {
        // The error outlives this call's region: copy it to the caller's first.
        let err = match (self.frame, &self.path, &self.res) {
            (Some((_, dest)), ErrPath::Return(_), Some(_)) => {
                let et = self.lty(&Ty::Error);
                let saved = self.tmp(LTy::Region);
                self.emit(LS::RegionUse { region: LE::Var(dest), saved });
                let c = self.error_copy(err);
                let c = self.bind(c, et);
                self.emit(LS::RegionRestore(saved));
                c
            }
            _ => err,
        };
        let path = self.err_path();
        match (&path, self.res.clone()) {
            (ErrPath::Return(_), Some(ok)) => {
                self.block(path.cleanup());
                let rt = result_lty(ok.clone(), self.mode);
                self.emit(LS::Return(Some(LE::Tup(rt, vec![LE::B(false), zero_le(&ok), err]))));
            }
            _ => {
                // The message first: cleanup may free what the error refers to.
                let msg = self.error_message(err.clone());
                let loc = LE::Field(Box::new(err_body(err)), 1);
                let text = LE::Rt(Rt::StrCat, vec![LE::S("error: ".into()), msg, LE::S(" (".into()), loc, LE::S(")".into())]);
                let text = self.bind(text, LTy::Str);
                // Printed from the program region (the frame is gone by then).
                let keep = self.tmp(LTy::Str);
                let saved = self.tmp(LTy::Region);
                self.emit(LS::RegionUse { region: LE::RegionProgram, saved });
                self.emit(LS::Set(keep, LE::Rt(Rt::StrCat, vec![text])));
                self.emit(LS::RegionRestore(saved));
                self.block(path.cleanup());
                self.emit(LS::Die(LE::Var(keep)));
            }
        }
    }

    /// A copy of `v` whose storage (strings, arrays) is all fresh, in the
    /// current region.
    fn deep_copy(&mut self, v: LE, t: &LTy) -> LE {
        fn has_storage(t: &LTy) -> bool {
            match t {
                // A Mutex (a one-element array of lock and value) is a handle:
                // copies share it, like channels and atomics.
                LTy::Arr(el) if matches!(&**el, LTy::Tup(ts) if ts.first() == Some(&LTy::Lock)) => false,
                LTy::Str | LTy::Arr(_) | LTy::PInt | LTy::Rec(_) => true,
                LTy::Tup(ts) => ts.iter().any(has_storage),
                _ => false,
            }
        }
        if !has_storage(t) {
            return v;
        }
        match t {
            // A type that contains itself: a function (copying inline would never end).
            LTy::Rec(i) => {
                let body = t.unrec();
                let rt = t.clone();
                let name = self.helper(format!("__dup_rec{i}"), vec![t.clone()], t.clone(), move |w, ps| {
                    let fields = body.tup_fields().iter().enumerate().map(|(k, ft)| w.deep_copy(LE::Field(Box::new(ps[0].clone()), k), ft)).collect();
                    LE::Tup(rt, fields)
                });
                LE::Call(name, vec![v])
            }
            // (A one-part concatenation always copies.)
            LTy::Str => LE::Rt(Rt::StrCat, vec![v]),
            LTy::PInt => v,
            LTy::Tup(ts) => {
                let x = self.bind(v, t.clone());
                let fields = ts.iter().enumerate().map(|(k, ft)| self.deep_copy(LE::Field(Box::new(x.clone()), k), ft)).collect();
                LE::Tup(t.clone(), fields)
            }
            LTy::Arr(el) => {
                let c = self.tmp(t.clone());
                self.emit(LS::Set(c, LE::Rt(Rt::ArrCopy, vec![v])));
                if has_storage(el) {
                    let (i, n) = (self.tmp(LTy::I64), self.tmp(LTy::I64));
                    self.emit(LS::Set(i, LE::I(0)));
                    self.emit(LS::Set(n, LE::Len(Box::new(LE::Var(c)))));
                    let l = self.label();
                    let el = (**el).clone();
                    let body = self.sub(|lw| {
                        lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Var(n)), LTy::I64), vec![LS::Break(l)], vec![]));
                        let x = LE::Index { arr: Box::new(LE::Var(c)), idx: Box::new(LE::Var(i)), check: None };
                        let x = lw.deep_copy(x, &el);
                        lw.emit(LS::SetIndex { arr: c, idx: LE::Var(i), val: x, check: None });
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    });
                    self.emit(LS::Loop(l, body));
                }
                LE::Var(c)
            }
            _ => v,
        }
    }

    fn block(&mut self, ss: &[LS]) {
        for s in ss {
            self.emit(s.clone());
        }
    }

    fn fail_if(&mut self, cond: LE, err: LE) {
        let body = self.sub(|lw| {
            let e = lw.bind(err, lw.lty(&Ty::Error));
            lw.fail(e);
        });
        self.emit(LS::If(cond, body, vec![]));
    }

    /// A runtime check: when `bad` holds, panic with `msg`, or under `~`
    /// fail with builtin error `ty`'s variant `j` (variant 2 of ArithError
    /// and IndexError carries `msg` as its message).
    fn guard(&mut self, bad: LE, msg: &str, sp: Span, ty: &str, j: usize) {
        if self.try_mode {
            let vals = if j == 2 { vec![LE::S(msg.into())] } else { vec![] };
            let err = self.make_error(ty, j, vals, sp);
            self.fail_if(bad, err);
        } else {
            self.emit(LS::If(bad, vec![LS::Panic(msg.into(), self.loc(sp))], vec![]));
        }
    }

    /// `idx` outside `0...len(arr)`, as one unsigned comparison.
    fn out_of_bounds(arr: &LE, idx: &LE) -> LE {
        LE::Prim(Prim::ULe, vec![LE::Len(Box::new(arr.clone())), idx.clone()])
    }

    /// An array value usable more than once (array constants are read in place).
    fn bind_arr(&mut self, a: LE, t: LTy) -> LE {
        if matches!(a, LE::Global(_)) { a } else { self.bind(a, t) }
    }

    /// `a ** b` under `~`: a negative exponent or overflow fails (squaring
    /// with checked multiplications).
    fn checked_pow(&mut self, a: LE, b: LE, sp: Span) -> LE {
        let a = self.bind(a, LTy::I64);
        let b = self.bind(b, LTy::I64);
        self.guard(LE::Cmp(Op::Lt, Box::new(b.clone()), Box::new(LE::I(0)), LTy::I64), "negative exponent", sp, "ArithError", 2);
        let (r, base, n) = (self.tmp(LTy::I64), self.tmp(LTy::I64), self.tmp(LTy::I64));
        self.emit(LS::Set(r, LE::I(1)));
        self.emit(LS::Set(base, a));
        self.emit(LS::Set(n, b));
        let l = self.label();
        let body = self.sub(|lw| {
            lw.emit(LS::If(LE::Cmp(Op::Eq, Box::new(LE::Var(n)), Box::new(LE::I(0)), LTy::I64), vec![LS::Break(l)], vec![]));
            let odd = LE::Cmp(Op::Ne, Box::new(LE::Prim(Prim::And, vec![LE::Var(n), LE::I(1)])), Box::new(LE::I(0)), LTy::I64);
            let mul = lw.sub(|lw| {
                let v = lw.checked_arith(Op::Mul, LE::Var(r), LE::Var(base), sp);
                lw.emit(LS::Set(r, v));
            });
            lw.emit(LS::If(odd, mul, vec![]));
            lw.emit(LS::Set(n, LE::Prim(Prim::ShrS, vec![LE::Var(n), LE::I(1)])));
            let sq = lw.sub(|lw| {
                let v = lw.checked_arith(Op::Mul, LE::Var(base), LE::Var(base), sp);
                lw.emit(LS::Set(base, v));
            });
            lw.emit(LS::If(LE::Cmp(Op::Ne, Box::new(LE::Var(n)), Box::new(LE::I(0)), LTy::I64), sq, vec![]));
        });
        self.emit(LS::Loop(l, body));
        LE::Var(r)
    }

    /// Under `~`: a Float that `to_i` (or `to_u64` when `u64`) can't
    /// convert fails instead of reaching the runtime's panic.
    fn float_to_int_guard(&mut self, v: LE, u64: bool, sp: Span) -> LE {
        if !self.try_mode {
            return v;
        }
        let v = self.bind(v, LTy::F64);
        let nan = LE::Cmp(Op::Ne, Box::new(v.clone()), Box::new(v.clone()), LTy::F64);
        self.guard(nan, "Float#to_i of NaN", sp, "ArithError", 2);
        let (lo, hi) = if u64 { (0.0, 18446744073709551616.0) } else { (-9223372036854775808.0, 9223372036854775808.0) };
        let ge = LE::Cmp(Op::Ge, Box::new(v.clone()), Box::new(LE::F(lo)), LTy::F64);
        let lt = LE::Cmp(Op::Lt, Box::new(v.clone()), Box::new(LE::F(hi)), LTy::F64);
        let inside = LE::Cond(Box::new(ge), Box::new(lt), Box::new(LE::B(false)));
        self.guard(LE::Not(Box::new(inside)), "conversion overflow", sp, "ArithError", 0);
        v
    }

    /// If Result `r` failed, propagate its error.
    fn unwrap_result(&mut self, r: LE) {
        let err = LE::Field(Box::new(r.clone()), 2);
        let body = self.sub(|lw| lw.fail(err));
        self.emit(LS::If(LE::Not(Box::new(LE::Field(Box::new(r), 0))), body, vec![]));
    }

    /// An Error holding `v`, a value of error type `k`, raised at `sp`.
    fn error_value(&mut self, k: usize, v: LE, sp: Span) -> LE {
        let bt = error_body(self.mode);
        let LTy::Tup(ts) = &bt else { unreachable!() };
        let mut fields = vec![LE::I(k as i64), LE::S(self.loc(sp)), LE::S(String::new())];
        for (j, t) in ts[3..].iter().enumerate() {
            fields.push(if j == k { v.clone() } else { zero_le(t) });
        }
        LE::ArrLit(bt.clone(), vec![LE::Tup(bt, fields)])
    }

    /// An Error of builtin type `ty`, variant `j`.
    fn make_error(&mut self, ty: &str, j: usize, vals: Vec<LE>, sp: Span) -> LE {
        let errors = ERRORS.with(|e| e.borrow().clone());
        let k = errors.iter().position(|t| t.type_name() == Some(ty)).expect("builtin error");
        let Ty::Enum(_, vs) = &errors[k] else { unreachable!() };
        let et = self.lty(&errors[k]);
        let mut slots = vec![LE::I(j as i64)];
        let mut vals = vals.into_iter();
        for (vk, (_, fs)) in vs.iter().enumerate() {
            for (_, ft) in fs {
                slots.push(if vk == j { vals.next().unwrap() } else { zero_le(&self.lty(ft)) });
            }
        }
        let v = LE::Tup(et, slots);
        self.error_value(k, v, sp)
    }

    /// File.read's error for status `st` (1 = not found, else failed).
    fn io_error(&mut self, st: LE, path: LE, sp: Span) -> LE {
        let nf = self.make_error("IoError", 0, vec![path.clone()], sp);
        let failed = self.make_error("IoError", 1, vec![path], sp);
        LE::Cond(Box::new(LE::Cmp(Op::Eq, Box::new(st), Box::new(LE::I(1)), LTy::I64)), Box::new(nf), Box::new(failed))
    }

    /// `a op b` under `~`: overflow becomes an ArithError.
    fn checked_arith(&mut self, op: Op, a: LE, b: LE, sp: Span) -> LE {
        let a = self.bind(a, LTy::I64);
        let b = self.bind(b, LTy::I64);
        let r = self.tmp(LTy::I64);
        self.emit(LS::Set(r, LE::Arith(op, Box::new(a.clone()), Box::new(b.clone()), Ovf::Wrap)));
        let neg = |x: LE| LE::Cmp(Op::Lt, Box::new(x), Box::new(LE::I(0)), LTy::I64);
        let xor = |x: LE, y: LE| LE::Prim(Prim::Xor, vec![x, y]);
        let and = |x: LE, y: LE| LE::Prim(Prim::And, vec![x, y]);
        let rv = LE::Var(r);
        let cmp = |c: Op, x: LE, k: i64| LE::Cmp(c, Box::new(x), Box::new(LE::I(k)), LTy::I64);
        let cond = match (op, &b) {
            // A constant operand: one compare against the bound.
            (Op::Add, LE::I(k)) if *k > 0 => cmp(Op::Gt, a.clone(), i64::MAX - k),
            (Op::Add, LE::I(k)) if *k < 0 => cmp(Op::Lt, a.clone(), i64::MIN - k),
            (Op::Sub, LE::I(k)) if *k > 0 => cmp(Op::Lt, a.clone(), i64::MIN + k),
            (Op::Sub, LE::I(k)) if *k < 0 && *k != i64::MIN => cmp(Op::Gt, a.clone(), i64::MAX + k),
            (Op::Add | Op::Sub, LE::I(0)) => LE::B(false),
            (Op::Mul, _) if const_factor(&a, &b).is_some_and(|k| k > 0) => {
                // x * k fits iff MIN / k <= x <= MAX / k: one unsigned compare.
                let k = const_factor(&a, &b).unwrap();
                let x = if matches!(a, LE::I(_)) { b.clone() } else { a.clone() };
                let (lo, hi) = (i64::MIN / k, i64::MAX / k);
                let off = LE::Arith(Op::Sub, Box::new(x), Box::new(LE::I(lo)), Ovf::Wrap);
                LE::Not(Box::new(LE::Prim(Prim::ULe, vec![off, LE::I(hi.wrapping_sub(lo))])))
            }
            (Op::Add, _) => neg(and(xor(a.clone(), rv.clone()), xor(b.clone(), rv.clone()))),
            (Op::Sub, _) => neg(and(xor(a.clone(), b.clone()), xor(a.clone(), rv.clone()))),
            _ => LE::Prim(Prim::MulOvf, vec![a.clone(), b.clone()]),
        };
        let err = self.make_error("ArithError", 0, vec![], sp);
        self.fail_if(cond, err);
        rv
    }

    /// An Error's message (wrap context first), via a generated function.
    fn error_message(&mut self, e: LE) -> LE {
        let name = "__error_message".to_string();
        let exists = self.prog.borrow().funcs.iter().any(|f| f.name == name);
        if !exists {
            let et = self.lty(&Ty::Error);
            // Reserve the name first (to_s of an error may need it).
            self.prog.borrow_mut().funcs.push(LFunc { name: name.clone(), params: vec![], vars: vec![], ret: LTy::Str, body: vec![], external: false, is_main: false, labels: 0 });
            let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Die(vec![]), self.prog);
            let pe = w.new_var("e", et);
            let s = w.new_var("s", LTy::Str);
            let errors = ERRORS.with(|x| x.borrow().clone());
            let mut body = vec![LS::Set(s, LE::S(String::new()))];
            for (k, t) in errors.iter().enumerate() {
                let v = LE::Field(Box::new(err_body(LE::Var(pe))), 3 + k);
                let arm = w.sub(|w| {
                    let m = match (w.p.messages.get(&k), t.type_name().unwrap_or("")) {
                        (Some(&fid), _) => LE::Call(w.p.funcs[fid].cname.clone(), vec![v.clone()]),
                        (None, "IoError") => {
                            let p = LE::Field(Box::new(v.clone()), 1);
                            let p2 = LE::Field(Box::new(v.clone()), 2);
                            let tag = LE::Field(Box::new(v.clone()), 0);
                            LE::Cond(
                                Box::new(LE::Cmp(Op::Eq, Box::new(tag), Box::new(LE::I(0)), LTy::I64)),
                                Box::new(LE::Rt(Rt::StrCat, vec![LE::S("File.read: no such file `".into()), p, LE::S("`".into())])),
                                Box::new(LE::Rt(Rt::StrCat, vec![LE::S("File.read: cannot read `".into()), p2, LE::S("`".into())])),
                            )
                        }
                        (None, t @ ("ArithError" | "IndexError")) => {
                            // Two payload-free variants, then one carrying its message.
                            let (m0, m1) = if t == "ArithError" { ("overflow", "division by zero") } else { ("index out of bounds", "slice bounds out of range") };
                            let tag = |j: i64| LE::Cmp(Op::Eq, Box::new(LE::Field(Box::new(v.clone()), 0)), Box::new(LE::I(j)), LTy::I64);
                            let rest = LE::Cond(Box::new(tag(1)), Box::new(LE::S(m1.into())), Box::new(LE::Field(Box::new(v.clone()), 1)));
                            LE::Cond(Box::new(tag(0)), Box::new(LE::S(m0.into())), Box::new(rest))
                        }
                        (None, "Failure" | "TaskError") => LE::Field(Box::new(v.clone()), 1),
                        (None, _) => w.to_s(v.clone(), t),
                    };
                    w.emit(LS::Set(s, m));
                });
                body.push(LS::If(LE::Cmp(Op::Eq, Box::new(LE::Field(Box::new(err_body(LE::Var(pe))), 0)), Box::new(LE::I(k as i64)), LTy::I64), arm, vec![]));
            }
            body.push(LS::Return(Some(LE::Rt(Rt::StrCat, vec![LE::Field(Box::new(err_body(LE::Var(pe))), 2), LE::Var(s)]))));
            let func = LFunc { name: name.clone(), params: vec![pe], vars: std::mem::take(&mut w.vars), ret: LTy::Str, body, external: false, is_main: false, labels: w.labels };
            let mut prog = self.prog.borrow_mut();
            let slot = prog.funcs.iter().position(|f| f.name == name).unwrap();
            prog.funcs[slot] = func;
        }
        LE::Call(name, vec![e])
    }

    /// A deep copy of an Error value into the current region: one shared
    /// function (`__error_copy`), not inlined at every `fail`. The Error
    /// type spans every error type in the program, so an inline copy grew
    /// each fail site by kilobytes (and the Rust oracle past rustc's patience).
    fn error_copy(&mut self, e: LE) -> LE {
        let name = "__error_copy".to_string();
        let exists = self.prog.borrow().funcs.iter().any(|f| f.name == name);
        if !exists {
            let et = self.lty(&Ty::Error);
            self.prog.borrow_mut().funcs.push(LFunc { name: name.clone(), params: vec![], vars: vec![], ret: et.clone(), body: vec![], external: false, is_main: false, labels: 0 });
            let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Die(vec![]), self.prog);
            let pe = w.new_var("e", et.clone());
            let c = w.deep_copy(LE::Var(pe), &et);
            let mut body = w.out.pop().unwrap_or_default();
            body.push(LS::Return(Some(c)));
            let func = LFunc { name: name.clone(), params: vec![pe], vars: std::mem::take(&mut w.vars), ret: et, body, external: false, is_main: false, labels: w.labels };
            let mut prog = self.prog.borrow_mut();
            let slot = prog.funcs.iter().position(|f| f.name == name).unwrap();
            prog.funcs[slot] = func;
        }
        LE::Call(name, vec![e])
    }

    /// (region field, live-bytes field) of a Map's or Pool's header.
    fn region_fields(&self, t: &Ty) -> (usize, usize) {
        match t {
            Ty::Pool(_) => (POOL_REGION, POOL_LIVE_BYTES),
            _ => (crate::mapgen::REGION, crate::mapgen::LIVE_BYTES),
        }
    }

    /// The child region of the owning container local `l` refers to (R3).
    fn owner_region(&mut self, l: LocalId) -> Option<LE> {
        let c = *self.place?.owner_alias.get(&l)?;
        let t = self.f.locals[c].ty.clone();
        let (rf, _) = self.region_fields(&t);
        let v = self.var_of(l);
        Some(LE::Field(Box::new(LE::Index { arr: Box::new(LE::Var(v)), idx: Box::new(LE::I(0)), check: None }), rf))
    }

    /// At the end of an iteration of loop `key`: compact the containers that
    /// own a region there, if their garbage outweighs their live contents.
    fn compact_owners(&mut self, key: usize) {
        let Some(pl) = self.place else { return };
        let mut cs: Vec<LocalId> = pl.owners.iter().filter(|(_, o)| o.loop_key == key).map(|(c, _)| *c).collect();
        cs.sort_unstable();
        for c in cs {
            let t = self.f.locals[c].ty.clone();
            let (rf, bf) = self.region_fields(&t);
            let cv = self.var_of(c);
            let hdr = |f: usize| LE::Field(Box::new(LE::Index { arr: Box::new(LE::Var(cv)), idx: Box::new(LE::I(0)), check: None }), f);
            let set = |f: usize, v: LE| LS::SetPlace { var: cv, steps: vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(f)], val: v };
            let used = LE::RegionBytes(Box::new(hdr(rf)));
            let limit = LE::Arith(Op::Add, Box::new(LE::Arith(Op::Mul, Box::new(hdr(bf)), Box::new(LE::I(3)), Ovf::Unchecked)), Box::new(LE::I(65536)), Ovf::Unchecked);
            let body = self.sub(|lw| {
                let old = lw.tmp(LTy::Region);
                lw.emit(LS::Set(old, hdr(rf)));
                let fresh = lw.tmp(LTy::Region);
                lw.emit(LS::Set(fresh, LE::RegionNew(Box::new(LE::RegionOf(Box::new(LE::Var(cv)))))));
                let saved = lw.tmp(LTy::Region);
                lw.emit(LS::RegionUse { region: LE::Var(fresh), saved });
                match &t {
                    Ty::Map(kt, vt) => {
                        // A fresh map with copies of the live entries.
                        let (fns, klt, vlt) = lw.map_fns(&t);
                        let m2 = lw.tmp(lw.lty(&t));
                        lw.emit(LS::Set(m2, LE::Call(fns.new.clone(), vec![hdr(crate::mapgen::COUNT)])));
                        let set_fn = fns.set.clone();
                        lw.map_each(&LE::Var(cv), kt, vt, |lw, k, v| {
                            let k = lw.deep_copy(k, &klt);
                            let v = lw.deep_copy(v, &vlt);
                            lw.emit(LS::Eval(LE::Call(set_fn.clone(), vec![LE::Var(m2), k, v])));
                        });
                        lw.emit(LS::RegionRestore(saved));
                        for f in 0..=crate::mapgen::COUNT {
                            let nf = LE::Field(Box::new(LE::Index { arr: Box::new(LE::Var(m2)), idx: Box::new(LE::I(0)), check: None }), f);
                            lw.emit(set(f, nf));
                        }
                    }
                    _ => {
                        // A pool keeps its slots (handles index them): copy them.
                        let slots_t = match lw.lty(&t) {
                            LTy::Arr(h) => match *h {
                                LTy::Tup(hs) => hs[0].clone(),
                                _ => unreachable!(),
                            },
                            _ => unreachable!(),
                        };
                        let copied = lw.deep_copy(hdr(0), &slots_t);
                        let copied = lw.bind(copied, slots_t);
                        let free_t = LTy::Arr(Box::new(LTy::I64));
                        let free = lw.deep_copy(hdr(POOL_FREE), &free_t);
                        let free = lw.bind(free, free_t);
                        lw.emit(LS::RegionRestore(saved));
                        lw.emit(set(0, copied));
                        lw.emit(set(POOL_FREE, free));
                    }
                }
                lw.emit(set(rf, LE::Var(fresh)));
                lw.emit(LS::RegionFree(LE::Var(old)));
                lw.emit(set(bf, LE::RegionBytes(Box::new(LE::Var(fresh)))));
            });
            self.emit(LS::If(LE::Cmp(Op::Gt, Box::new(used), Box::new(limit), LTy::I64), body, vec![]));
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
        let unfolded;
        let ty = if let Ty::Rec(_) = ty {
            unfolded = ty.unrec();
            &unfolded
        } else {
            ty
        };
        match ty {
            Ty::Fixed(el, n) => {
                let lt = self.lty(ty);
                let c = self.tmp(lt.clone());
                // The copy goes into the region of the array it copies: the
                // region analysis placed that one where the value must live
                // (`v = ys; xs[i] = v` stores v somewhere longer-lived than
                // the current region, which a loop iteration frees; port-issues #88).
                let src = self.bind(v, lt.clone());
                let saved = (*n > 0).then(|| {
                    let saved = self.tmp(LTy::Region);
                    self.emit(LS::RegionUse { region: LE::RegionOf(Box::new(src.clone())), saved });
                    saved
                });
                self.emit(LS::Set(c, LE::Rt(Rt::ArrCopy, vec![src])));
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
                if let Some(saved) = saved {
                    self.emit(LS::RegionRestore(saved));
                }
                LE::Var(c)
            }
            Ty::Struct(_, fs) => {
                let lt = self.lty(ty);
                let t = self.bind(v, lt.clone());
                // A boxed optional field is never written through: copies share it.
                let vals = fs.iter().enumerate().map(|(k, (_, ft))| if boxed_slot(ty, k).is_some() { LE::Field(Box::new(t.clone()), k) } else { self.copy_value(LE::Field(Box::new(t.clone()), k), ft) }).collect();
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
        // Everything under `~` fails instead of panicking (call arguments too).
        let saved = std::mem::replace(&mut self.try_mode, true);
        let v = self.try_expr_in(inner);
        self.try_mode = saved;
        v
    }

    fn try_expr_in(&mut self, inner: &TExpr) -> LE {
        let path = self.err_path();
        match &inner.kind {
            TK::Call(fid, args) => {
                let _ = path;
                let f = &self.p.funcs[*fid];
                let name = f.cname.clone();
                let rt = fn_ret(f);
                let unit = f.ret == Ty::Unit;
                // The result lives where the call's site is placed (as for
                // any call: `~f()` returned from here goes to the caller).
                let placed = self.site_region(inner);
                let restore = placed.map(|(region, class)| {
                    let saved = self.tmp(LTy::Region);
                    self.emit(LS::RegionUse { region, saved });
                    (saved, std::mem::replace(&mut self.ambient, class))
                });
                let args = args.iter().map(|a| self.arg(a)).collect();
                let r = self.tmp(rt);
                self.emit(LS::Set(r, LE::Call(name, args)));
                if let Some((saved, prev)) = restore {
                    self.ambient = prev;
                    self.emit(LS::RegionRestore(saved));
                }
                self.unwrap_result(LE::Var(r));
                if unit { LE::Unit } else { LE::Field(Box::new(LE::Var(r)), 1) }
            }
            TK::M(M::FileRead, None, args, _) => {
                let _ = path;
                let p = self.expr(&args[0]);
                let p = self.bind(p, LTy::Str);
                let st = self.tmp(LTy::I64);
                self.emit(LS::Set(st, LE::Rt(Rt::FileStatus, vec![p.clone()])));
                let err = self.io_error(LE::Var(st), p.clone(), inner.span);
                let body = self.sub(|lw| {
                    let e = lw.bind(err, lw.lty(&Ty::Error));
                    lw.fail(e);
                });
                self.emit(LS::If(LE::Cmp(Op::Ne, Box::new(LE::Var(st)), Box::new(LE::I(0)), LTy::I64), body, vec![]));
                LE::Rt(Rt::FileRead, vec![p])
            }
            _ if matches!(inner.ty, Ty::Result(_)) => {
                let _ = path;
                let Ty::Result(t) = &inner.ty else { unreachable!() };
                let unit = **t == Ty::Unit;
                let v = self.expr(inner);
                let v = self.bind(v, self.lty(&inner.ty));
                self.unwrap_result(v.clone());
                if unit { LE::Unit } else { LE::Field(Box::new(v), 1) }
            }
            _ => self.expr(inner),
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
                return self.parith(op, av, bv, e.span);
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
            BinOp::Pow if self.try_mode => self.checked_pow(a, b, sp),
            // Under `~`, Div and Rem take the checked path below (zero
            // divisor, MIN / -1); checked_arith only knows Add/Sub/Mul.
            _ if k == IntKind::I64 && !(self.try_mode && matches!(op, BinOp::Div | BinOp::Rem)) => {
                if self.try_mode {
                    return self.checked_arith(lop, a, b, sp);
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
                if self.try_mode {
                    let err = self.make_error("ArithError", 1, vec![], sp);
                    self.fail_if(zero, err);
                } else {
                    self.emit(LS::If(zero, vec![LS::Panic("division by zero".into(), self.loc(sp))], vec![]));
                }
                if k == IntKind::U64 {
                    return LE::Prim(if op == BinOp::Div { Prim::UDiv } else { Prim::URem }, vec![a, b]);
                }
                if k == IntKind::I64 && op == BinOp::Div {
                    // Only reached under `~`. MIN / -1 doesn't fit in 64 bits:
                    // an ArithError, or MIN in a wrap file (as Go).
                    let eq = |x: LE, y: i64| LE::Cmp(Op::Eq, Box::new(x), Box::new(LE::I(y)), LTy::I64);
                    let ovf = LE::Cmp(Op::And, Box::new(eq(a.clone(), i64::MIN)), Box::new(eq(b.clone(), -1)), LTy::Bool);
                    if wrap_mode {
                        return LE::Cond(Box::new(ovf), Box::new(LE::I(i64::MIN)), Box::new(LE::Arith(lop, Box::new(a), Box::new(b), Ovf::Unchecked)));
                    }
                    self.overflow_if(ovf, k, sp);
                    return LE::Arith(lop, Box::new(a), Box::new(b), Ovf::Unchecked);
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
        // A constant count (`x >> 7`, the common case in hash and crypto
        // code) needs no checks or clamping: fold them here, so no backend
        // sees a branch per shift (the JIT made a block per Cond, which made
        // fully unrolled compression functions 40x slower than the C build).
        if let LE::I(n) = c {
            let w = k.bits() as i64;
            if n >= 0 {
                return match op {
                    BinOp::Shl if n >= w => LE::I(0),
                    BinOp::Shl => wrap_to(k, LE::Prim(Prim::Shl, vec![a, LE::I(n)])),
                    _ if k == IntKind::U64 && n >= 64 => LE::I(0),
                    _ if k == IntKind::U64 => LE::Prim(Prim::ShrU, vec![a, LE::I(n)]),
                    // A narrow unsigned value: saying so (a no-op wrap) lets
                    // clang see `(x << k) | (x >> (w - k))` as a rotate.
                    _ if !k.signed() => LE::Prim(Prim::ShrS, vec![wrap_to(k, a), LE::I(n.min(63))]),
                    _ => LE::Prim(Prim::ShrS, vec![a, LE::I(n.min(63))]),
                };
            }
        }
        if ck.signed() {
            let neg = LE::Cmp(Op::Lt, Box::new(c.clone()), Box::new(LE::I(0)), LTy::I64);
            self.guard(neg, "negative shift amount", sp, "ArithError", 2);
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
        if self.try_mode {
            let _ = loc;
            let err = self.make_error("ArithError", 0, vec![], sp);
            self.fail_if(cond, err);
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
        let msg = format!("conversion overflow: the value doesn't fit {}", k.name());
        if x.ty == Ty::Float {
            let v = self.float_to_int_guard(v, k == IntKind::U64, sp);
            if k == IntKind::U64 {
                return LE::Rt(Rt::FToU64, vec![v, LE::Loc(loc.clone())]);
            }
            let i = self.tmp(LTy::I64);
            self.emit(LS::Set(i, LE::Rt(Rt::FToI, vec![v, LE::Loc(loc.clone())])));
            if k != IntKind::I64 {
                let out = out_of_range(k, LE::Var(i));
                self.guard(out, &msg, sp, "ArithError", 0);
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
                self.guard(out, &msg, sp, "ArithError", 0);
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
        if self.try_mode {
            // Each index checked against the place it indexes, in order.
            let mut cur = LE::Var(var);
            for (st, ts) in lsteps.iter_mut().zip(steps) {
                cur = match st {
                    Step::Index(i, check) => {
                        let TStep::Index(ie) = ts else { unreachable!() };
                        self.guard(Self::out_of_bounds(&cur, i), "index out of bounds", ie.span, "IndexError", 0);
                        *check = None;
                        LE::Index { arr: Box::new(cur), idx: Box::new(i.clone()), check: None }
                    }
                    Step::Field(f) => LE::Field(Box::new(cur), *f),
                };
            }
        }
        // The type at each step: the last one may be a boxed optional field.
        let mut cur_t = self.f.locals[l].ty.clone();
        let mut last_box = None;
        for (k, st) in steps.iter().enumerate() {
            let u = cur_t.unrec();
            if let (TStep::Field(f), true) = (st, k + 1 == steps.len()) {
                last_box = boxed_slot(&u, *f);
            }
            cur_t = match (st, &u) {
                (TStep::Index(_), _) => u.arr_elem().unwrap_or(Ty::Unit),
                (TStep::Field(f), Ty::Struct(_, fs)) => fs.get(*f).map(|x| x.1.clone()).unwrap_or(Ty::Unit),
                (TStep::Field(f), Ty::Tuple(ts)) => ts.get(*f).cloned().unwrap_or(Ty::Unit),
                _ => Ty::Unit,
            };
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
        } else if let Some(x) = last_box {
            let b = self.opt_box(val.clone(), &x);
            self.emit(LS::SetPlace { var, steps: lsteps, val: b });
        } else {
            self.emit(LS::SetPlace { var, steps: lsteps, val: val.clone() });
        }
        val
    }

    /// `place.m!(args)` (`TK::Bang`): `view` refers to the place for the
    /// call (docs/notes/bang-calls.md). A copy instead (made in the region
    /// the view would carry, written back after) where the callee might
    /// keep its receiver past the call, and for a bare narrow-integer local
    /// (its C variable is wider than the element a view points at).
    fn bang(&mut self, l: LocalId, steps: &[TStep], view: LocalId, call: &TExpr, e: &TExpr) -> LE {
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
        if self.try_mode {
            // Each index checked against the place it indexes, in order.
            let mut cur = LE::Var(var);
            for (st, ts) in lsteps.iter_mut().zip(steps) {
                cur = match st {
                    Step::Index(i, check) => {
                        let TStep::Index(ie) = ts else { unreachable!() };
                        self.guard(Self::out_of_bounds(&cur, i), "index out of bounds", ie.span, "IndexError", 0);
                        *check = None;
                        LE::Index { arr: Box::new(cur), idx: Box::new(i.clone()), check: None }
                    }
                    Step::Field(f) => LE::Field(Box::new(cur), *f),
                };
            }
        }
        let vv = self.var_of(view);
        let LTy::Arr(et) = self.lty(&self.f.locals[view].ty) else { unreachable!("a view is an Arr") };
        let et = *et;
        // The region what the callee stores into the place must live in:
        // the region of the array holding it (the innermost index), else
        // (a local's own variables) where the analysis placed this site.
        let last = lsteps.iter().rposition(|s| matches!(s, Step::Index(..)));
        let region = match last {
            Some(j) => LE::ViewRegion(Box::new(crate::lir::place_le(var, &lsteps[..j]))),
            None => match self.placed(e as *const TExpr as usize) {
                // Into the storage behind the root (a parameter, a lambda's
                // capture): the region of the place itself when it has one
                // pointer to find it by (`w.header` holding one map, though
                // `w` holds more), else the root's.
                Some((r, crate::regions::Place::Into(x))) if x == l && !lsteps.is_empty() => {
                    let pt = match &self.f.locals[view].ty {
                        Ty::Array(t) => (**t).clone(),
                        _ => unreachable!("a view is an Array"),
                    };
                    self.storage_region(crate::lir::place_le(var, &lsteps), &pt).unwrap_or(r)
                }
                Some((r, _)) => r,
                // The current region: by its variable where there is one
                // (a JIT call per `!` call otherwise).
                None => {
                    use crate::regions::Place;
                    match (self.ambient, self.frame) {
                        (Place::Frame, Some((frame, _))) if self.lambda_place.is_none() => LE::Var(frame),
                        (Place::Iter(k), Some(_)) if self.iter_vars.contains_key(&k) => LE::Var(self.iter_vars[&k].0),
                        _ => LE::Rt(Rt::RegionCur, vec![]),
                    }
                }
            },
        };
        let narrow = last.is_none() && matches!(et, LTy::IntK(_));
        let escapes = self.bang_callees(call).into_iter().any(|f| self_escapes(&self.p.funcs[f]));
        let unchecked: Vec<Step> = lsteps.iter().map(|s| match s {
            Step::Index(i, _) => Step::Index(i.clone(), None),
            Step::Field(f) => Step::Field(*f),
        }).collect();
        if escapes || narrow {
            let r = self.bind(region, LTy::Region);
            let saved = self.tmp(LTy::Region);
            let cur = crate::lir::place_le(var, &lsteps);
            self.emit(LS::RegionUse { region: r, saved });
            self.emit(LS::Set(vv, LE::ArrLit(et, vec![cur])));
            self.emit(LS::RegionRestore(saved));
        } else {
            let r = self.bind(region, LTy::Region);
            self.emit(LS::View { dst: vv, var, steps: lsteps, ty: et, region: r });
        }
        let rt = self.lty(&e.ty);
        let v = self.expr(call);
        let out = if rt == LTy::Unit {
            if !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                self.emit(LS::Eval(v));
            }
            LE::Unit
        } else {
            let o = self.tmp(rt.clone());
            self.emit(LS::Set(o, v));
            LE::Var(o)
        };
        if escapes || narrow {
            let back = LE::Index { arr: Box::new(LE::Var(vv)), idx: Box::new(LE::I(0)), check: None };
            if unchecked.is_empty() {
                self.emit(LS::Set(var, back));
            } else {
                self.emit(LS::SetPlace { var, steps: unchecked, val: back });
            }
        } else {
            self.emit(LS::Unview { dst: vv, var, steps: unchecked });
        }
        out
    }

    /// The functions a `!` call (`TK::Bang`'s call) may run.
    fn bang_callees(&self, call: &TExpr) -> Vec<FuncId> {
        match &call.kind {
            TK::Call(f, _) => vec![*f],
            TK::M(M::IfaceCall(mi), Some(r), ..) => {
                let Ty::Array(it) = &r.ty else { return vec![] };
                let Ty::Iface(iname) = &**it else { return vec![] };
                self.p.ifaces.get(iname).into_iter().flatten().filter_map(|(_, fids)| fids.get(*mi).copied()).collect()
            }
            _ => vec![],
        }
    }

    /// `a op b` on promoted Ints. Under `~`, a zero divisor and a negative
    /// or oversized exponent are ArithErrors (there is no overflow).
    fn parith(&mut self, op: BinOp, a: LE, b: LE, sp: Span) -> LE {
        let lop = op_of(op);
        if self.try_mode && matches!(op, BinOp::Div | BinOp::Rem | BinOp::Pow) {
            let a = self.bind(a, LTy::PInt);
            let b = self.bind(b, LTy::PInt);
            let p = |x: i64| Box::new(LE::ToP(Box::new(LE::I(x))));
            if op == BinOp::Pow {
                self.guard(LE::PArith(Op::Lt, Box::new(b.clone()), p(0)), "negative exponent", sp, "ArithError", 2);
                self.guard(LE::PArith(Op::Gt, Box::new(b.clone()), p(i32::MAX as i64)), "exponent out of range", sp, "ArithError", 0);
            } else {
                self.guard(LE::PArith(Op::Eq, Box::new(b.clone()), p(0)), "division by zero", sp, "ArithError", 1);
            }
            return LE::PArith(lop, Box::new(a), Box::new(b));
        }
        LE::PArith(lop, Box::new(a), Box::new(b))
    }

    /// `a op b` on already-lowered operands of type `t`.
    fn arith_le(&mut self, op: crate::ast::BinOp, a: LE, b: LE, t: &LTy, sp: Span) -> LE {
        use crate::ast::BinOp as B;
        let lop = op_of(op);
        let _ = B::Add;
        match t {
            LTy::F64 => LE::FArith(lop, Box::new(a), Box::new(b)),
            LTy::PInt if op.is_arith() => self.parith(op, a, b, sp),
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
            let part = self.fmt_piece(p, &vals, args);
            parts.push(part);
        }
        match parts.len() {
            0 => LE::S(String::new()),
            1 if matches!(parts[0], LE::S(_)) => parts.pop().unwrap(),
            _ => LE::Rt(Rt::StrCat, parts),
        }
    }

    fn fmt_piece(&mut self, p: &FmtPiece, vals: &[LE], args: &[TExpr]) -> LE {
        match p {
            FmtPiece::Lit(s) => LE::S(s.clone()),
            FmtPiece::Int(k) => self.to_s(vals[*k].clone(), &args[*k].ty),
            FmtPiece::Str(k) => self.to_s(vals[*k].clone(), &args[*k].ty),
            FmtPiece::Fixed(k, d) => LE::Rt(Rt::FFmt, vec![vals[*k].clone(), LE::I(*d as i64)]),
            FmtPiece::Exp(k, d, upper) => LE::Rt(Rt::FFmtE, vec![vals[*k].clone(), LE::I(*d as i64), LE::B(*upper)]),
            FmtPiece::Quote(k) => LE::Rt(Rt::StrQuote, vec![vals[*k].clone()]),
            FmtPiece::Base(k, base, upper) => {
                let t = &args[*k].ty;
                let v = self.int_in_t(vals[*k].clone(), t, args[*k].span);
                LE::Rt(Rt::IntFmt, vec![v, LE::I(*base as i64), LE::B(*upper), LE::B(*t == Ty::IntK(IntKind::U64))])
            }
            FmtPiece::Char(k) => {
                let v = self.int_in_t(vals[*k].clone(), &args[*k].ty, args[*k].span);
                LE::Rt(Rt::RuneToS, vec![v])
            }
            FmtPiece::Digits { inner, n } => {
                let s = self.fmt_piece(inner, vals, args);
                let s = self.bind(s, LTy::Str);
                let first = LE::Rt(Rt::StrByte, vec![s.clone(), LE::I(0), LE::I(0)]);
                let neg = LE::Cmp(Op::Eq, Box::new(first), Box::new(LE::I(b'-' as i64)), LTy::I64);
                let w = LE::Cond(Box::new(neg), Box::new(LE::I(*n as i64 + 1)), Box::new(LE::I(*n as i64)));
                let padded = LE::Rt(Rt::StrPad, vec![s.clone(), w, LE::I(2)]);
                if *n > 0 {
                    return padded;
                }
                let zero = LE::Cmp(Op::Eq, Box::new(s), Box::new(LE::S("0".into())), LTy::Str);
                LE::Cond(Box::new(zero), Box::new(LE::S(String::new())), Box::new(padded))
            }
            FmtPiece::Padded { inner, width, left, zero, plus, space } => {
                let s = self.fmt_piece(inner, vals, args);
                let mut s = self.bind(s, LTy::Str);
                // `+` / ` `: a sign on non-negative numbers.
                if *plus || *space {
                    let first = LE::Rt(Rt::StrByte, vec![s.clone(), LE::I(0), LE::I(0)]);
                    let neg = LE::Cmp(Op::Eq, Box::new(first), Box::new(LE::I(b'-' as i64)), LTy::I64);
                    let signed = LE::Rt(Rt::StrCat, vec![LE::S(if *plus { "+" } else { " " }.into()), s.clone()]);
                    s = self.bind(LE::Cond(Box::new(neg), Box::new(s.clone()), Box::new(signed)), LTy::Str);
                }
                if *width == 0 {
                    return s;
                }
                let flags = (*left as i64) | ((*zero as i64) << 1);
                LE::Rt(Rt::StrPad, vec![s, LE::I(*width as i64), LE::I(flags)])
            }
        }
    }

    fn to_s(&mut self, v: LE, t: &Ty) -> LE {
        if let Some(&fid) = self.p.stringers.get(&t.show()) {
            return LE::Call(self.p.funcs[fid].cname.clone(), vec![v]);
        }
        if *t == Ty::Error {
            return self.error_message(v);
        }
        let unfolded;
        let t = if let Ty::Rec(_) = t {
            unfolded = t.unrec();
            &unfolded
        } else {
            t
        };
        // A type that contains itself: a function (printing inline would never end).
        if matches!(t, Ty::Struct(..) | Ty::Enum(..) | Ty::Iface(_)) && ty_recursive(t) {
            let lt = self.lty(t);
            let LTy::Rec(i) = lt else { unreachable!() };
            let t2 = t.clone();
            let name = self.helper(format!("__to_s_rec{i}"), vec![lt], LTy::Str, move |w, ps| w.to_s_parts(ps[0].clone(), &t2));
            return LE::Call(name, vec![v]);
        }
        self.to_s_parts(v, t)
    }

    fn to_s_parts(&mut self, v: LE, t: &Ty) -> LE {
        match t {
            Ty::Opt(inner) => {
                let lt = self.lty(t);
                let v = self.bind(v, lt);
                let s = self.to_s(LE::Field(Box::new(v.clone()), 1), inner);
                LE::Cond(Box::new(LE::Field(Box::new(v), 0)), Box::new(s), Box::new(LE::S("none".into())))
            }
            Ty::Str => v,
            Ty::Handle(_) => LE::Rt(Rt::StrCat, vec![LE::S("@".into()), LE::Rt(Rt::IntToS, vec![handle_index(v)])]),
            Ty::Array(el) | Ty::Fixed(el, _) => {
                // Go: `[1 2 3]`.
                let lt = self.lty(t);
                let a = self.bind(v, lt);
                let s = self.tmp(LTy::Str);
                let i = self.tmp(LTy::I64);
                self.emit(LS::Set(s, LE::S("[".into())));
                self.emit(LS::Set(i, LE::I(0)));
                let l = self.label();
                let body = self.sub(|lw| {
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Len(Box::new(a.clone()))), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = LE::Index { arr: Box::new(a.clone()), idx: Box::new(LE::Var(i)), check: None };
                    let xs = lw.to_s(x, el);
                    let sep = LE::Cond(Box::new(LE::Cmp(Op::Eq, Box::new(LE::Var(i)), Box::new(LE::I(0)), LTy::I64)), Box::new(LE::S(String::new())), Box::new(LE::S(" ".into())));
                    lw.emit(LS::Set(s, LE::Rt(Rt::StrCat, vec![LE::Var(s), sep, xs])));
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                });
                self.emit(LS::Loop(l, body));
                LE::Rt(Rt::StrCat, vec![LE::Var(s), LE::S("]".into())])
            }
            Ty::Iface(n) => {
                let impls = self.p.ifaces.get(n).cloned().unwrap_or_default();
                let lt = self.lty(t);
                let x = self.bind(v, lt);
                let s = self.tmp(LTy::Str);
                self.emit(LS::Set(s, LE::S(String::new())));
                for (k, (it, _)) in impls.iter().enumerate() {
                    let body = self.sub(|lw| {
                        let v = lw.iface_get(x.clone(), t, k);
                        let v = lw.to_s(v, it);
                        lw.emit(LS::Set(s, v));
                    });
                    self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(LE::Field(Box::new(x.clone()), 0)), Box::new(LE::I(k as i64)), LTy::I64), body, vec![]));
                }
                LE::Var(s)
            }
            Ty::Enum(_, vs) => {
                // `Red`, `Circle(2)`.
                let lt = self.lty(t);
                let x = self.bind(v, lt);
                let tag = LE::Field(Box::new(x.clone()), 0);
                let mut slot = 1;
                let mut shown = vec![];
                for (name, fs) in vs.iter() {
                    let s = if fs.is_empty() {
                        LE::S(name.clone())
                    } else {
                        let mut parts = vec![LE::S(format!("{name}("))];
                        for (j, (_, ft)) in fs.iter().enumerate() {
                            if j > 0 {
                                parts.push(LE::S(", ".into()));
                            }
                            let f = self.slot_read(t, x.clone(), slot + j);
                            let f = self.to_s(f, ft);
                            let fv = self.tmp(LTy::Str);
                            self.emit(LS::Set(fv, f));
                            parts.push(LE::Var(fv));
                        }
                        parts.push(LE::S(")".into()));
                        LE::Rt(Rt::StrCat, parts)
                    };
                    slot += fs.len();
                    shown.push(s);
                }
                let mut acc = shown.pop().unwrap_or(LE::S(String::new()));
                for (k, s) in shown.into_iter().enumerate().rev() {
                    acc = LE::Cond(Box::new(LE::Cmp(Op::Eq, Box::new(tag.clone()), Box::new(LE::I(k as i64)), LTy::I64)), Box::new(s), Box::new(acc));
                }
                acc
            }
            Ty::Struct(_, _) | Ty::Tuple(_) => {
                // Go: `{1 2}`.
                let fts: Vec<Ty> = match t {
                    Ty::Struct(_, fs) => fs.iter().map(|(_, t)| t.clone()).collect(),
                    Ty::Tuple(ts) => ts.clone(),
                    _ => unreachable!(),
                };
                let lt = self.lty(t);
                let x = self.bind(v, lt);
                let mut parts = vec![LE::S("{".into())];
                for (k, ft) in fts.iter().enumerate() {
                    if k > 0 {
                        parts.push(LE::S(" ".into()));
                    }
                    let fs = self.slot_read(t, x.clone(), k);
                    let fs = self.to_s(fs, ft);
                    let fv = self.tmp(LTy::Str);
                    self.emit(LS::Set(fv, fs));
                    parts.push(LE::Var(fv));
                }
                parts.push(LE::S("}".into()));
                LE::Rt(Rt::StrCat, parts)
            }
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
        let nd = self.next_depth.take().unwrap_or(self.defers.len());
        self.next_target.push((next_label, nd));
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
            Global(k) => LE::Global(global_of(k, self.lty(&e.ty))),
            SetGlobal(k) => {
                let lt = self.lty(&args[0].ty);
                let v = self.expr(&args[0]);
                self.emit(LS::SetGlobal(global_of(k, lt), v));
                LE::Unit
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
                let check = if self.try_mode {
                    let empty = LE::Cmp(Op::Eq, Box::new(LE::Len(Box::new(av.clone()))), Box::new(LE::I(0)), LTy::I64);
                    self.guard(empty, "`last` of an empty collection", sp, "IndexError", 2);
                    None
                } else {
                    Some(self.loc(sp))
                };
                LE::Index { arr: Box::new(av), idx: Box::new(idx), check }
            }
            StrHelper(code) => {
                use crate::strgen::StrFn;
                let f = [StrFn::Strip, StrFn::Lstrip, StrFn::Rstrip, StrFn::StartWith, StrFn::EndWith, StrFn::Include, StrFn::Lines, StrFn::Repeat][code as usize];
                let name = crate::strgen::instantiate(&mut self.prog.borrow_mut(), f);
                let mut av = vec![self.expr(recv.unwrap())];
                for a in args {
                    let v = self.expr(a);
                    av.push(if f == StrFn::Repeat { self.int_in(v, a.span) } else { v });
                }
                LE::Call(name.into(), av)
            }
            Reverse if matches!(recv.unwrap().ty, Ty::Array(_)) => {
                let r = recv.unwrap();
                let at = self.lty(&r.ty);
                let et = at.clone().arr_elem_lty();
                let v = self.expr(r);
                let v = self.bind(v, at.clone());
                let out = self.tmp(at);
                self.emit(LS::Set(out, LE::ArrWithCap(et, Box::new(LE::Len(Box::new(v.clone()))))));
                let i = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::Len(Box::new(v.clone()))));
                let l = self.label();
                let body = self.sub(|lw| {
                    lw.emit(LS::If(LE::Cmp(Op::Le, Box::new(LE::Var(i)), Box::new(LE::I(0)), LTy::I64), vec![LS::Break(l)], vec![]));
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(-1)), Ovf::Unchecked)));
                    lw.emit(LS::Push(out, LE::Index { arr: Box::new(v.clone()), idx: Box::new(LE::Var(i)), check: None }));
                });
                self.emit(LS::Loop(l, body));
                LE::Var(out)
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
                if self.promote() && e.ty == Ty::Int {
                    LE::Rt(Rt::PFromStr, vec![v])
                } else {
                    self.int_out(LE::Rt(Rt::StrToI, vec![v]))
                }
            }
            ByteIndex => {
                let v = self.expr(recv.unwrap());
                let a = self.expr(&args[0]);
                let from = if args.len() > 1 { self.expr(&args[1]) } else { LE::I(0) };
                LE::Rt(Rt::StrIndex, vec![v, a, from])
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
                let v = if self.try_mode {
                    let p = self.promote();
                    let v = self.bind(v, if p { LTy::PInt } else { LTy::I64 });
                    let neg = if p { LE::PArith(Op::Lt, Box::new(v.clone()), Box::new(LE::ToP(Box::new(LE::I(0))))) } else { LE::Cmp(Op::Lt, Box::new(v.clone()), Box::new(LE::I(0)), LTy::I64) };
                    self.guard(neg, "`digits` of a negative number", sp, "ArithError", 2);
                    v
                } else {
                    v
                };
                if self.promote() { LE::Rt(Rt::PDigits, vec![v, LE::Loc(self.loc(sp))]) } else { LE::Rt(Rt::Digits, vec![v, LE::Loc(self.loc(sp))]) }
            }
            IntSqrt => {
                let v = self.expr(&args[0]);
                let v = self.int_in(v, sp);
                let v = if self.try_mode {
                    let v = self.bind(v, LTy::I64);
                    self.guard(LE::Cmp(Op::Lt, Box::new(v.clone()), Box::new(LE::I(0)), LTy::I64), "Int.sqrt of a negative number", sp, "ArithError", 2);
                    v
                } else {
                    v
                };
                self.int_out(LE::Rt(Rt::Isqrt, vec![v, LE::Loc(self.loc(sp))]))
            }
            MapNew | MapGet | MapGetOr | MapSet | MapDel | MapHas | MapSize | MapKeys | MapValues => self.map_op(m, e, recv, args),
            FromBytes => {
                let v = self.expr(&args[0]);
                LE::Rt(Rt::StrFromBytes, vec![v])
            }
            Dup if matches!(recv.unwrap().ty, Ty::Fn(..) | Ty::Struct(..) | Ty::Iface(_)) => {
                let r = recv.unwrap();
                let v = self.expr(r);
                let t = self.lty(&r.ty);
                self.deep_copy(v, &t)
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
                self.guard(LE::Cmp(Op::Lt, Box::new(n.clone()), Box::new(LE::I(0)), LTy::I64), "negative array size", sp, "ArithError", 2);
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
                let n = if self.try_mode {
                    let n = self.bind(n, LTy::I64);
                    self.guard(LE::Cmp(Op::Lt, Box::new(n.clone()), Box::new(LE::I(0)), LTy::I64), "negative array size", sp, "ArithError", 2);
                    n
                } else {
                    n
                };
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
                let v = self.float_to_int_guard(v, false, sp);
                self.int_out(LE::Rt(Rt::FToI, vec![v, LE::Loc(self.loc(sp))]))
            }
            FloatAbs => LE::Rt(Rt::FAbs, vec![self.expr(recv.unwrap())]),
            FloatToS => LE::Rt(Rt::FToS, vec![self.expr(recv.unwrap())]),
            Sqrt => LE::Rt(Rt::FSqrt, vec![self.expr(&args[0])]),
            // math block: libm calls and bit casts (receiver first, then args)
            Math(f) => {
                let mut v: Vec<LE> = recv.into_iter().map(|r| self.expr(r)).collect();
                v.extend(args.iter().map(|a| self.expr(a)));
                LE::Rt(Rt::Math(f), v)
            }
            FloatBits => LE::Rt(Rt::FBits, vec![self.expr(recv.unwrap())]),
            UMulHi => LE::Prim(Prim::UMulHi, vec![self.expr(recv.unwrap()), self.expr(&args[0])]),
            FloatFromBits => LE::Rt(Rt::FFromBits, vec![self.expr(recv.unwrap())]),
            FloatFmtF => {
                let x = self.expr(recv.unwrap());
                let n = self.expr(&args[0]);
                LE::Rt(Rt::FFmt, vec![x, n])
            }
            FloatFmtE => {
                let x = self.expr(recv.unwrap());
                let n = self.expr(&args[0]);
                LE::Rt(Rt::FFmtE, vec![x, n, LE::B(false)])
            }
            StructNew => {
                let t = self.lty(&e.ty);
                let mut vs = vec![];
                for (k, a) in args.iter().enumerate() {
                    let v = self.arg(a);
                    vs.push(match boxed_slot(&e.ty, k) {
                        Some(x) => self.opt_box(v, &x),
                        None => v,
                    });
                }
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
                self.guard(absent, "unwrap of none", sp, "IndexError", 2);
                LE::Field(Box::new(v), 1)
            }
            Lambda => {
                let b = blk.unwrap();
                let lo = ((b.id as u64) << 32) | b.span.lo as u64;
                let g = LAMBDAS.with(|l| l.borrow().iter().position(|s| s.0 == self.f.cname && s.1 == lo)).expect("lambda registered");
                let sites = lambda_sites(&e.ty);
                let tag = sites.iter().position(|(x, _)| *x == g).unwrap();
                // The body becomes its own function: captures first, then params.
                let Ty::Fn(pts, rt) = &e.ty else { unreachable!() };
                let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Return(vec![]), self.prog);
                w.lambda_place = self.place.or(self.lambda_place).or(self.nested_place);
                // `(..) -> ~T`: a fallible body, like a fallible def's.
                let ok_t = match &**rt {
                    Ty::Result(t) => Some(w.lty(t)),
                    _ => None,
                };
                if let Some(t) = &ok_t {
                    w.res = Some(ok_lty(t.clone()));
                }
                let mut params = vec![];
                for a in args {
                    let TK::Local(l) = a.kind else { unreachable!() };
                    params.push(w.var_of(l));
                }
                let mut pvs = vec![];
                for pt in pts {
                    let v = w.new_var("p", w.lty(pt));
                    params.push(v);
                    pvs.push(LE::Var(v));
                }
                let (mut body, v) = w.sub_val(|w| w.inline_block(b, &pvs, &[], None));
                let ret = w.lty(rt);
                // A body whose last expression never finishes (`panic(..)`
                // after early returns) has no value to return: like a def's,
                // it ends without a return.
                let never = matches!(v, LE::Unit) && !matches!(ok_t, Some(LTy::Unit)) && ret != LTy::Unit && !matches!(&**rt, Ty::Result(t) if **t == Ty::Unit);
                if never {
                } else if ok_t.is_some() {
                    let v = if ok_t == Some(LTy::Unit) {
                        if !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                            body.push(LS::Eval(v));
                        }
                        LE::B(false) // ok_lty's placeholder for Unit
                    } else {
                        v
                    };
                    let r = w.ok_result(v);
                    body.push(LS::Return(Some(r)));
                } else {
                    // A Unit body's last expression still runs (it's a call, say).
                    if ret == LTy::Unit && !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                        body.push(LS::Eval(v.clone()));
                    }
                    body.push(LS::Return(if ret == LTy::Unit { None } else { Some(v) }));
                }
                let func = LFunc { name: format!("__lambda_{g}"), params, vars: std::mem::take(&mut w.vars), ret, body, external: false, is_main: false, labels: w.labels };
                self.prog.borrow_mut().funcs.push(func);
                let lt = self.lty(&e.ty);
                let ts = lt.tup_fields();
                let caps: Vec<LE> = args.iter().map(|a| self.arg(a)).collect();
                // A closure type that contains itself (R12) holds each
                // lambda's captures in a box.
                let boxed = ty_recursive(&e.ty);
                let mut vals = vec![LE::I(tag as i64)];
                for (j, t) in ts[1..].iter().enumerate() {
                    vals.push(if j != tag {
                        zero_le(t)
                    } else if boxed {
                        let ct = t.clone().arr_elem_lty();
                        LE::ArrLit(ct.clone(), vec![LE::Tup(ct, caps.clone())])
                    } else {
                        LE::Tup(t.clone(), caps.clone())
                    });
                }
                LE::Tup(lt, vals)
            }
            FnCall => {
                let f = recv.unwrap();
                let fv = self.expr(f);
                let fv = self.bind(fv, self.lty(&f.ty));
                let mut avs = vec![];
                for a in args {
                    let v = self.arg(a);
                    avs.push(self.bind(v, self.lty(&a.ty)));
                }
                let rt = self.lty(&e.ty);
                let out = if rt == LTy::Unit { None } else { Some(self.tmp(rt.clone())) };
                if let Some(o) = out {
                    self.emit(LS::Set(o, zero_le(&rt)));
                }
                let tag = LE::Field(Box::new(fv.clone()), 0);
                let boxed = ty_recursive(&f.ty);
                let fts = self.lty(&f.ty).tup_fields();
                for (k, (g, caps)) in lambda_sites(&f.ty).iter().enumerate() {
                    let env = LE::Field(Box::new(fv.clone()), k + 1);
                    let env = if boxed {
                        let ct = fts[k + 1].clone().arr_elem_lty();
                        let (pre, env) = self.sub_val(|lw| lw.unbox(env, &ct));
                        let env_v = self.tmp(ct.clone());
                        let mut pre = pre;
                        pre.push(LS::Set(env_v, env));
                        self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(tag.clone()), Box::new(LE::I(k as i64)), LTy::I64), pre, vec![]));
                        LE::Var(env_v)
                    } else {
                        env
                    };
                    let cargs = (0..caps.len()).map(|j| LE::Field(Box::new(env.clone()), j)).chain(avs.iter().cloned()).collect();
                    let call = LE::Call(format!("__lambda_{g}"), cargs);
                    let st = match out {
                        Some(o) => LS::Set(o, call),
                        None => LS::Eval(call),
                    };
                    self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(tag.clone()), Box::new(LE::I(k as i64)), LTy::I64), vec![st], vec![]));
                }
                match out {
                    Some(o) => LE::Var(o),
                    None => LE::Unit,
                }
            }
            Spawn => {
                let b = blk.unwrap();
                let Ty::Task(produced) = &e.ty else { unreachable!() };
                let fallible = matches!(**produced, Ty::Result(_));
                let ok_ty = match &**produced {
                    Ty::Result(t) => (**t).clone(),
                    t => t.clone(),
                };
                let cap_tys: Vec<LTy> = args.iter().map(|a| self.lty(&a.ty)).collect();
                let env_t = LTy::Tup(cap_tys.clone());
                // The body becomes a worker: unpack the captured copies, run, return.
                let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Return(vec![]), self.prog);
                w.nested_place = self.place.or(self.lambda_place).or(self.nested_place);
                if fallible {
                    w.res = Some(ok_lty(w.lty(&ok_ty)));
                }
                let env = w.new_var("env", env_t.clone());
                let mut body = vec![];
                // The task's own frame (port-issues #277): its body's sites
                // are placed in the enclosing function's placement (regions.rs
                // walks a spawn body outside a lambda as a frame of its own).
                // Freed when the body is done; what outlives the task was
                // placed in the program region.
                if let Some(pl) = self.place.filter(|_| self.lambda_place.is_none()) {
                    let (frame, dest) = (w.new_var("frame", LTy::Region), w.new_var("dest", LTy::Region));
                    body.push(LS::RegionEnter { region: frame, saved: dest });
                    w.frame = Some((frame, dest));
                    w.place = Some(pl);
                }
                for (j, a) in args.iter().enumerate() {
                    let TK::Local(l) = a.kind else { unreachable!() };
                    let v = w.var_of(l);
                    body.push(LS::Set(v, LE::Field(Box::new(LE::Var(env)), j)));
                }
                let (stmts, v) = w.sub_val(|w| w.scoped_value(&b.body, &ok_ty));
                body.extend(stmts);
                // Nothing to return: the last expression still runs.
                let v = if matches!(ok_ty, Ty::Unit | Ty::Never) {
                    if !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                        body.push(LS::Eval(v));
                    }
                    LE::B(false)
                } else {
                    v
                };
                let out_t = if fallible { result_lty(w.lty(&ok_ty), self.mode) } else { ok_lty(w.lty(&ok_ty)) };
                let v = if fallible { w.ok_result(if matches!(ok_ty, Ty::Unit | Ty::Never) { LE::B(false) } else { v }) } else if matches!(ok_ty, Ty::Unit | Ty::Never) { LE::B(false) } else { v };
                body.push(LS::Return(Some(v)));
                let mut prog = self.prog.borrow_mut();
                let id = prog.workers.len();
                let func = LFunc { name: format!("worker{id}"), params: vec![env], vars: std::mem::take(&mut w.vars), ret: out_t, body, external: false, is_main: false, labels: w.labels };
                prog.workers.push(LWorker { id, input: env_t.clone(), func });
                drop(prog);
                let caps: Vec<LE> = args.iter().map(|a| self.arg(a)).collect();
                let lt = self.lty(&e.ty);
                let dst = self.tmp(lt);
                self.emit(LS::Spawn { dst, worker: id, env: LE::Tup(env_t, caps) });
                LE::Var(dst)
            }
            TaskWait => {
                let r = recv.unwrap();
                let Ty::Task(produced) = &r.ty else { unreachable!() };
                let tv = self.expr(r);
                let tv = self.bind(tv, self.lty(&r.ty));
                let LTy::Task(inner) = self.lty(&r.ty) else { unreachable!() };
                let (ok, val, msg) = (self.tmp(LTy::Bool), self.tmp((*inner).clone()), self.tmp(LTy::Str));
                self.emit(LS::Set(val, zero_le(&inner)));
                self.emit(LS::Wait { task: tv, ok, val, msg });
                let rt = self.lty(&e.ty);
                let out = self.tmp(rt.clone());
                let good = if matches!(**produced, Ty::Result(_)) {
                    LE::Var(val)
                } else {
                    let et = self.lty(&Ty::Error);
                    LE::Tup(rt.clone(), vec![LE::B(true), LE::Var(val), zero_le(&et)])
                };
                let panicked = self.make_error("TaskError", 0, vec![LE::Var(msg)], sp);
                let LTy::Tup(ts) = &rt else { unreachable!() };
                let bad = LE::Tup(rt.clone(), vec![LE::B(false), zero_le(&ts[1]), panicked]);
                self.emit(LS::Set(out, LE::Cond(Box::new(LE::Var(ok)), Box::new(good), Box::new(bad))));
                LE::Var(out)
            }
            PoolNew => {
                let lt = self.lty(&e.ty);
                let LTy::Arr(h) = &lt else { unreachable!() };
                let LTy::Tup(hs) = &**h else { unreachable!() };
                let LTy::Arr(slot) = &hs[0] else { unreachable!() };
                LE::ArrLit((**h).clone(), vec![LE::Tup((**h).clone(), vec![LE::ArrWithCap((**slot).clone(), Box::new(LE::I(0))), LE::I(0), LE::RegionProgram, LE::I(0), LE::ArrWithCap(LTy::I64, Box::new(LE::I(0))), LE::I(0)])])
            }
            PoolAdd | PoolGet | PoolLookup | PoolSet | PoolRemove | PoolSize => {
                let r = recv.unwrap();
                let lt = self.lty(&r.ty);
                let pv = self.expr(r);
                let p = self.tmp_of(pv, lt.clone());
                let LTy::Arr(h) = &lt else { unreachable!() };
                let LTy::Tup(hs) = &**h else { unreachable!() };
                let slots_t = hs[0].clone();
                let LTy::Arr(slot_t) = &slots_t else { unreachable!() };
                let LTy::Tup(st) = &**slot_t else { unreachable!() };
                let val_t = st[1].clone();
                let hdr = |f: usize| LE::Field(Box::new(LE::Index { arr: Box::new(LE::Var(p)), idx: Box::new(LE::I(0)), check: None }), f);
                let set_hdr = |f: usize, v: LE| LS::SetPlace { var: p, steps: vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(f)], val: v };
                let loc = self.loc(sp);
                match m {
                    PoolSize => self.int_out(hdr(1)),
                    PoolLookup => {
                        // No panics: a handle past the end or of a removed
                        // slot gives none.
                        let h = self.expr(&args[0]);
                        let hv = self.bind(h, LTy::I64);
                        let idx = self.bind(handle_index(hv.clone()), LTy::I64);
                        let rt = self.lty(&e.ty);
                        let res = self.tmp(rt.clone());
                        self.emit(LS::Set(res, zero_le(&rt)));
                        let inb = LE::Cmp(Op::Lt, Box::new(idx.clone()), Box::new(LE::Len(Box::new(hdr(0)))), LTy::I64);
                        let body = self.sub(|lw| {
                            let slot = lw.tmp((**slot_t).clone());
                            lw.emit(LS::Set(slot, LE::Index { arr: Box::new(hdr(0)), idx: Box::new(idx.clone()), check: None }));
                            let live = LE::Cmp(Op::Eq, Box::new(LE::Field(Box::new(LE::Var(slot)), 0)), Box::new(handle_gen(hv.clone())), LTy::I64);
                            let live = lw.bind(live, LTy::Bool);
                            lw.emit(LS::If(live.clone(), vec![LS::Set(res, LE::Tup(rt.clone(), vec![live, LE::Field(Box::new(LE::Var(slot)), 1)]))], vec![]));
                        });
                        self.emit(LS::If(inb, body, vec![]));
                        LE::Var(res)
                    }
                    PoolAdd => {
                        // The value first: making it may add to this pool too.
                        let v = self.arg(&args[0]);
                        let v = self.bind(v, val_t.clone());
                        let (idx, generation) = (self.tmp(LTy::I64), self.tmp(LTy::I64));
                        // A free slot if there is one (its generation goes back to odd).
                        let n = LE::Arith(Op::Sub, Box::new(hdr(POOL_FREE_N)), Box::new(LE::I(1)), Ovf::Unchecked);
                        let reuse = vec![
                            set_hdr(POOL_FREE_N, n),
                            LS::Set(idx, LE::Index { arr: Box::new(hdr(POOL_FREE)), idx: Box::new(hdr(POOL_FREE_N)), check: None }),
                            LS::Set(generation, LE::Arith(Op::Add, Box::new(LE::Field(Box::new(LE::Index { arr: Box::new(hdr(0)), idx: Box::new(LE::Var(idx)), check: None }), 0)), Box::new(LE::I(1)), Ovf::Unchecked)),
                            LS::SetPlace { var: p, steps: vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(0), crate::lir::Step::Index(LE::Var(idx), None)], val: LE::Tup((**slot_t).clone(), vec![LE::Var(generation), v.clone()]) },
                        ];
                        let fresh = self.sub(|lw| {
                            let s = lw.tmp(slots_t.clone());
                            lw.emit(LS::Set(s, hdr(0)));
                            lw.emit(LS::Push(s, LE::Tup((**slot_t).clone(), vec![LE::I(1), v.clone()])));
                            lw.emit(set_hdr(0, LE::Var(s)));
                            lw.emit(LS::Set(idx, LE::Arith(Op::Sub, Box::new(LE::Len(Box::new(LE::Var(s)))), Box::new(LE::I(1)), Ovf::Unchecked)));
                            lw.emit(LS::Set(generation, LE::I(1)));
                        });
                        self.emit(LS::If(LE::Cmp(Op::Gt, Box::new(hdr(POOL_FREE_N)), Box::new(LE::I(0)), LTy::I64), reuse, fresh));
                        self.emit(set_hdr(1, LE::Arith(Op::Add, Box::new(hdr(1)), Box::new(LE::I(1)), Ovf::Unchecked)));
                        make_handle(LE::Var(idx), LE::Var(generation))
                    }
                    _ => {
                        // The slot, checked: in range and not removed (the new
                        // value first: making it may add to this pool).
                        let h = self.expr(&args[0]);
                        let hv = self.bind(h, LTy::I64);
                        let h = self.bind(handle_index(hv.clone()), LTy::I64);
                        let newv = if m == PoolSet {
                            let v = self.arg(&args[1]);
                            Some(self.bind(v, val_t.clone()))
                        } else {
                            None
                        };
                        let slot = self.tmp((**slot_t).clone());
                        let check = if self.try_mode {
                            self.guard(Self::out_of_bounds(&hdr(0), &h), "index out of bounds", sp, "IndexError", 0);
                            None
                        } else {
                            Some(loc.clone())
                        };
                        self.emit(LS::Set(slot, LE::Index { arr: Box::new(hdr(0)), idx: Box::new(h.clone()), check }));
                        let live = LE::Cmp(Op::Eq, Box::new(LE::Field(Box::new(LE::Var(slot)), 0)), Box::new(handle_gen(hv.clone())), LTy::I64);
                        if m != PoolRemove {
                            self.guard(LE::Not(Box::new(live.clone())), "use of a removed pool handle", sp, "IndexError", 2);
                        }
                        let slot_step = vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(0), crate::lir::Step::Index(h.clone(), None)];
                        match m {
                            PoolGet => LE::Field(Box::new(LE::Var(slot)), 1),
                            PoolSet => {
                                let v = newv.unwrap();
                                self.emit(LS::SetPlace { var: p, steps: slot_step, val: LE::Tup((**slot_t).clone(), vec![LE::Field(Box::new(LE::Var(slot)), 0), v]) });
                                LE::Unit
                            }
                            _ => {
                                // remove: the slot's generation goes even and it joins
                                // the free list; the value comes back if it was there.
                                let was = self.bind(live, LTy::Bool);
                                let free_step = |k: LE| vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(POOL_FREE), crate::lir::Step::Index(k, None)];
                                let push_free = self.sub(|lw| {
                                    let fr = lw.tmp(LTy::Arr(Box::new(LTy::I64)));
                                    lw.emit(LS::Set(fr, hdr(POOL_FREE)));
                                    lw.emit(LS::Push(fr, h.clone()));
                                    lw.emit(set_hdr(POOL_FREE, LE::Var(fr)));
                                });
                                let body = vec![
                                    LS::SetPlace { var: p, steps: slot_step, val: LE::Tup((**slot_t).clone(), vec![LE::Arith(Op::Add, Box::new(LE::Field(Box::new(LE::Var(slot)), 0)), Box::new(LE::I(1)), Ovf::Unchecked), zero_le(&val_t)]) },
                                    set_hdr(1, LE::Arith(Op::Sub, Box::new(hdr(1)), Box::new(LE::I(1)), Ovf::Unchecked)),
                                    LS::If(
                                        LE::Cmp(Op::Lt, Box::new(hdr(POOL_FREE_N)), Box::new(LE::Len(Box::new(hdr(POOL_FREE)))), LTy::I64),
                                        vec![LS::SetPlace { var: p, steps: free_step(hdr(POOL_FREE_N)), val: h.clone() }],
                                        push_free,
                                    ),
                                    set_hdr(POOL_FREE_N, LE::Arith(Op::Add, Box::new(hdr(POOL_FREE_N)), Box::new(LE::I(1)), Ovf::Unchecked)),
                                ];
                                self.emit(LS::If(was.clone(), body, vec![]));
                                let rt = self.lty(&e.ty);
                                self.bind(LE::Tup(rt.clone(), vec![was, LE::Field(Box::new(LE::Var(slot)), 1)]), rt)
                            }
                        }
                    }
                }
            }
            Join => {
                let r = recv.unwrap();
                let Ty::Array(el) = &r.ty else { unreachable!() };
                let av = self.expr(r);
                let sep = self.expr(&args[0]);
                if **el == Ty::Str {
                    return LE::Rt(Rt::StrJoin, vec![av, sep]);
                }
                // Other elements: shown one by one into a [Str] first.
                let at = self.lty(&r.ty);
                let a = self.tmp_of(av, at);
                let strs = self.tmp(LTy::Arr(Box::new(LTy::Str)));
                self.emit(LS::Set(strs, LE::ArrWithCap(LTy::Str, Box::new(LE::Len(Box::new(LE::Var(a)))))));
                let i = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                let l = self.label();
                let el_t = (**el).clone();
                let body = self.sub(|lw| {
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Len(Box::new(LE::Var(a)))), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = LE::Index { arr: Box::new(LE::Var(a)), idx: Box::new(LE::Var(i)), check: None };
                    let s = lw.to_s(x, &el_t);
                    lw.emit(LS::Push(strs, s));
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                });
                self.emit(LS::Loop(l, body));
                LE::Rt(Rt::StrJoin, vec![LE::Var(strs), sep])
            }
            TupleNew => {
                let lt = self.lty(&e.ty);
                let vs = args.iter().map(|a| self.arg(a)).collect();
                LE::Tup(lt, vs)
            }
            PrintStr => {
                let s = self.expr(&args[0]);
                self.emit(LS::Print(s));
                LE::Unit
            }
            MutexNew => {
                let lt = self.lty(&e.ty);
                let LTy::Arr(h) = &lt else { unreachable!() };
                let v = self.arg(&args[0]);
                LE::ArrLit((**h).clone(), vec![LE::Tup((**h).clone(), vec![LE::LockNew, v])])
            }
            Lock => {
                // Holding the lock: the block gets the value, changes to it
                // are written back, and the result is copied out (it mustn't
                // point into what the lock guards).
                let r = recv.unwrap();
                let lt = self.lty(&r.ty);
                let pv = self.expr(r);
                let p = self.tmp_of(pv, lt);
                let cell = || LE::Index { arr: Box::new(LE::Var(p)), idx: Box::new(LE::I(0)), check: None };
                self.emit(LS::Lock(LE::Field(Box::new(cell()), 0), self.loc(sp)));
                let b = blk.unwrap();
                let res = self.inline_block(b, &[LE::Field(Box::new(cell()), 1)], &[], None);
                let rt = self.lty(&e.ty);
                let res = if rt == LTy::Unit {
                    // a block ending in a Unit call: evaluate it, there's no value
                    if !matches!(res, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                        self.emit(LS::Eval(res));
                    }
                    LE::Unit
                } else {
                    let res = self.bind(res, rt.clone());
                    let res = self.deep_copy(res, &rt);
                    self.bind(res, rt)
                };
                let pvar = self.var_of(b.params[0]);
                self.emit(LS::SetPlace { var: p, steps: vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(1)], val: LE::Var(pvar) });
                self.emit(LS::Unlock(LE::Field(Box::new(cell()), 0)));
                res
            }
            AtomicNew | AtomicLoad | AtomicStore | AtomicAdd | AtomicSwap | AtomicCas => {
                // Bools are 0/1 in the cell.
                let Ty::Atomic(vt) = (if m == AtomicNew { e.ty.clone() } else { recv.unwrap().ty.clone() }) else { unreachable!() };
                let is_bool = *vt == Ty::Bool;
                let val_in = |lw: &mut Self, a: &TExpr| {
                    let v = lw.expr(a);
                    if is_bool { LE::Cond(Box::new(v), Box::new(LE::I(1)), Box::new(LE::I(0))) } else { lw.int_in(v, a.span) }
                };
                let val_out = |lw: &mut Self, x: LE| {
                    let x = lw.bind(x, LTy::I64);
                    if is_bool { LE::Cmp(Op::Ne, Box::new(x), Box::new(LE::I(0)), LTy::I64) } else { lw.int_out(x) }
                };
                if m == AtomicNew {
                    let v = val_in(self, &args[0]);
                    return LE::AtomicNew(Box::new(v));
                }
                let av = self.expr(recv.unwrap());
                let a = self.tmp_of(av, LTy::Atomic);
                let a = || Box::new(LE::Var(a));
                match m {
                    AtomicLoad => val_out(self, LE::AtomicLoad(a())),
                    AtomicStore => {
                        let v = val_in(self, &args[0]);
                        self.emit(LS::AtomicStore(*a(), v));
                        LE::Unit
                    }
                    AtomicAdd => {
                        let v = val_in(self, &args[0]);
                        val_out(self, LE::AtomicRmw(crate::lir::AtomicOp::Add, a(), Box::new(v)))
                    }
                    AtomicSwap => {
                        let v = val_in(self, &args[0]);
                        val_out(self, LE::AtomicRmw(crate::lir::AtomicOp::Swap, a(), Box::new(v)))
                    }
                    _ => {
                        let o = val_in(self, &args[0]);
                        let o = self.bind(o, LTy::I64);
                        let n = val_in(self, &args[1]);
                        self.bind(LE::AtomicCas(a(), Box::new(o), Box::new(n)), LTy::Bool)
                    }
                }
            }
            ChanNew => {
                let Ty::Chan(t) = &e.ty else { unreachable!() };
                let el = self.lty(t);
                let cap = self.expr(&args[0]);
                let cap = self.int_in(cap, args[0].span);
                LE::ChanNew(el, Box::new(cap))
            }
            ChanSend => {
                let c = self.expr(recv.unwrap());
                let v = self.arg(&args[0]);
                self.emit(LS::ChanSend { ch: c, val: v, loc: self.loc(sp) });
                LE::Unit
            }
            ChanRecv => {
                let r = recv.unwrap();
                let c = self.expr(r);
                let LTy::Chan(el) = self.lty(&r.ty) else { unreachable!() };
                let (ok, val) = (self.tmp(LTy::Bool), self.tmp((*el).clone()));
                self.emit(LS::Set(val, zero_le(&el)));
                self.emit(LS::ChanRecv { ch: c, ok, val });
                LE::Tup(self.lty(&e.ty), vec![LE::Var(ok), LE::Var(val)])
            }
            ChanClose => {
                let c = self.expr(recv.unwrap());
                self.emit(LS::ChanClose { ch: c, loc: self.loc(sp) });
                LE::Unit
            }
            ChanLen => {
                let c = self.expr(recv.unwrap());
                self.int_out(LE::ChanLen(Box::new(c)))
            }
            MutexPoisoned | MutexClearPoison => {
                let r = recv.unwrap();
                let lt = self.lty(&r.ty);
                let pv = self.expr(r);
                let p = self.tmp_of(pv, lt);
                let cell = LE::Index { arr: Box::new(LE::Var(p)), idx: Box::new(LE::I(0)), check: None };
                let l = LE::Field(Box::new(cell), 0);
                if m == MutexClearPoison {
                    self.emit(LS::LockClearPoison(l));
                    LE::Unit
                } else {
                    let b = LE::LockPoisoned(Box::new(l));
                    self.bind(b, LTy::Bool)
                }
            }
            ToIface(k) => {
                let v = self.arg(recv.unwrap());
                self.make_iface(&e.ty, k, v)
            }
            IfaceEq => {
                // The same implementor, and its `__eq` of the two values (#108).
                let r = recv.unwrap();
                let Ty::Iface(n) = &r.ty else { unreachable!() };
                let lt = self.lty(&r.ty);
                let a = self.expr(r);
                let a = self.bind(a, lt.clone());
                let b = self.expr(&args[0]);
                let b = self.bind(b, lt);
                let out = self.tmp(LTy::Bool);
                self.emit(LS::Set(out, LE::B(false)));
                let eqs = self.p.iface_eqs.get(n).cloned().unwrap_or_default();
                let tag_a = LE::Field(Box::new(a.clone()), 0);
                let same = LE::Cmp(Op::Eq, Box::new(tag_a.clone()), Box::new(LE::Field(Box::new(b.clone()), 0)), LTy::I64);
                let body = self.sub(|lw| {
                    for (k, fid) in eqs.iter().enumerate() {
                        let arm = lw.sub(|lw| {
                            let (x, y) = (lw.iface_get(a.clone(), &r.ty, k), lw.iface_get(b.clone(), &r.ty, k));
                            lw.emit(LS::Set(out, LE::Call(lw.p.funcs[*fid].cname.clone(), vec![x, y])));
                        });
                        lw.emit(LS::If(LE::Cmp(Op::Eq, Box::new(tag_a.clone()), Box::new(LE::I(k as i64)), LTy::I64), arm, vec![]));
                    }
                });
                self.emit(LS::If(same, body, vec![]));
                LE::Var(out)
            }
            ErrEq => {
                // The same error type, and its `__eq` of the two values.
                let r = recv.unwrap();
                let lt = self.lty(&Ty::Error);
                let a = self.expr(r);
                let a = self.bind(a, lt.clone());
                let b = self.expr(&args[0]);
                let b = self.bind(b, lt);
                let out = self.tmp(LTy::Bool);
                self.emit(LS::Set(out, LE::B(false)));
                let eqs = self.p.iface_eqs.get(ERR_EQ).cloned().unwrap_or_default();
                let tag_a = LE::Field(Box::new(err_body(a.clone())), 0);
                let same = LE::Cmp(Op::Eq, Box::new(tag_a.clone()), Box::new(LE::Field(Box::new(err_body(b.clone())), 0)), LTy::I64);
                let body = self.sub(|lw| {
                    for (k, fid) in eqs.iter().enumerate() {
                        if *fid == usize::MAX {
                            continue;
                        }
                        let arm = lw.sub(|lw| {
                            let (x, y) = (LE::Field(Box::new(err_body(a.clone())), 3 + k), LE::Field(Box::new(err_body(b.clone())), 3 + k));
                            lw.emit(LS::Set(out, LE::Call(lw.p.funcs[*fid].cname.clone(), vec![x, y])));
                        });
                        lw.emit(LS::If(LE::Cmp(Op::Eq, Box::new(tag_a.clone()), Box::new(LE::I(k as i64)), LTy::I64), arm, vec![]));
                    }
                });
                self.emit(LS::If(same, body, vec![]));
                LE::Var(out)
            }
            IfaceCall(mi) if matches!(recv.unwrap().ty, Ty::Array(_)) => {
                // A `!` method: the receiver is a view of the interface value;
                // each implementor's method gets a view of the value inside it.
                let r = recv.unwrap();
                let Ty::Array(it) = &r.ty else { unreachable!() };
                let Ty::Iface(iname) = &**it else { unreachable!() };
                let impls = self.p.ifaces.get(iname).cloned().unwrap_or_default();
                let TK::Local(tl) = r.kind else { unreachable!("the receiver is a temporary") };
                let tv = self.var_of(tl);
                let mut avs = vec![];
                for a in args {
                    let v = self.arg(a);
                    avs.push(self.bind(v, self.lty(&a.ty)));
                }
                let rt = self.lty(&e.ty);
                let out = if rt == LTy::Unit { None } else { Some(self.tmp(rt.clone())) };
                if let Some(o) = out {
                    self.emit(LS::Set(o, zero_le(&rt)));
                }
                let held = LE::Index { arr: Box::new(LE::Var(tv)), idx: Box::new(LE::I(0)), check: None };
                let tag = LE::Field(Box::new(held.clone()), 0);
                let boxed = ty_recursive(it);
                for (k, (ty, fids)) in impls.iter().enumerate() {
                    let ct = self.lty(ty);
                    let cell = self.tmp(LTy::Arr(Box::new(ct.clone())));
                    let f = self.p.funcs[fids[mi]].cname.clone();
                    let escapes = self_escapes(&self.p.funcs[fids[mi]]);
                    let body = self.sub(|lw| {
                        let steps = vec![crate::lir::Step::Index(LE::I(0), None), crate::lir::Step::Field(k + 1)];
                        let region = LE::ViewRegion(Box::new(LE::Var(tv)));
                        if escapes || boxed {
                            // The method may keep its receiver (or the value sits in a
                            // box that may be empty, R13): a copy, written back.
                            let saved = lw.tmp(LTy::Region);
                            lw.emit(LS::RegionUse { region, saved });
                            let cur = lw.iface_get(held.clone(), it, k);
                            lw.emit(LS::Set(cell, LE::ArrLit(ct.clone(), vec![cur])));
                            lw.emit(LS::RegionRestore(saved));
                        } else {
                            let r = lw.bind(region, LTy::Region);
                            lw.emit(LS::View { dst: cell, var: tv, steps: steps.clone(), ty: ct.clone(), region: r });
                        }
                        let call = LE::Call(f.clone(), std::iter::once(LE::Var(cell)).chain(avs.iter().cloned()).collect());
                        let (pre, call) = lw.iface_result(e, fids[mi], call);
                        for st in pre {
                            lw.emit(st);
                        }
                        match out {
                            Some(o) => lw.emit(LS::Set(o, call)),
                            None => lw.emit(LS::Eval(call)),
                        }
                        if boxed {
                            // A boxed implementor (R13): the cell is its new box.
                            lw.emit(LS::SetPlace { var: tv, steps, val: LE::Var(cell) });
                        } else if escapes {
                            let back = LE::Index { arr: Box::new(LE::Var(cell)), idx: Box::new(LE::I(0)), check: None };
                            lw.emit(LS::SetPlace { var: tv, steps, val: back });
                        } else {
                            lw.emit(LS::Unview { dst: cell, var: tv, steps });
                        }
                    });
                    self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(tag.clone()), Box::new(LE::I(k as i64)), LTy::I64), body, vec![]));
                }
                match out {
                    Some(o) => LE::Var(o),
                    None => LE::Unit,
                }
            }
            IfaceOpt(k) => {
                let r = recv.unwrap();
                let rv = self.expr(r);
                let rv = self.bind(rv, self.lty(&r.ty));
                let t = self.lty(&e.ty);
                let ts = t.tup_fields();
                let out = self.tmp(t.clone());
                self.emit(LS::Set(out, LE::Tup(t.clone(), vec![LE::B(false), zero_le(&ts[1])])));
                let tag = LE::Field(Box::new(rv.clone()), 0);
                let arm = self.sub(|lw| {
                    let v = lw.iface_get(rv.clone(), &r.ty, k);
                    lw.emit(LS::Set(out, LE::Tup(t.clone(), vec![LE::B(true), v])));
                });
                self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(tag), Box::new(LE::I(k as i64)), LTy::I64), arm, vec![]));
                LE::Var(out)
            }
            IfaceCall(mi) => {
                let r = recv.unwrap();
                let Ty::Iface(iname) = &r.ty else { unreachable!() };
                let impls = self.p.ifaces.get(iname).cloned().unwrap_or_default();
                let rv = self.expr(r);
                let rv = self.bind(rv, self.lty(&r.ty));
                let mut avs = vec![];
                for a in args {
                    let v = self.arg(a);
                    avs.push(self.bind(v, self.lty(&a.ty)));
                }
                let rt = self.lty(&e.ty);
                let out = if rt == LTy::Unit { None } else { Some(self.tmp(rt.clone())) };
                if let Some(o) = out {
                    self.emit(LS::Set(o, zero_le(&rt)));
                }
                let tag = LE::Field(Box::new(rv.clone()), 0);
                for (k, (_, fids)) in impls.iter().enumerate() {
                    let fname = self.p.funcs[fids[mi]].cname.clone();
                    let arm = self.sub(|lw| {
                        let me = lw.iface_get(rv.clone(), &r.ty, k);
                        let call = LE::Call(fname, std::iter::once(me).chain(avs.iter().cloned()).collect());
                        let (sts, call) = lw.iface_result(e, fids[mi], call);
                        for st in sts {
                            lw.emit(st);
                        }
                        lw.emit(match out {
                            Some(o) => LS::Set(o, call),
                            None => LS::Eval(call),
                        });
                    });
                    self.emit(LS::If(LE::Cmp(Op::Eq, Box::new(tag.clone()), Box::new(LE::I(k as i64)), LTy::I64), arm, vec![]));
                }
                match out {
                    Some(o) => LE::Var(o),
                    None => LE::Unit,
                }
            }
            VariantNew(k) => {
                let lt = self.lty(&e.ty);
                let mut vals = vec![LE::I(k as i64)];
                for (j, a) in args.iter().enumerate() {
                    let v = match (boxed_slot(&e.ty, j + 1), &a.kind) {
                        // Another variant's slot: an empty box.
                        (Some(x), TK::None | TK::Zero) => LE::ArrWithCap(self.lty(&x), Box::new(LE::I(0))),
                        (Some(x), _) => {
                            let v = self.arg(a);
                            self.opt_box(v, &x)
                        }
                        (None, _) => self.arg(a),
                    };
                    vals.push(v);
                }
                LE::Tup(lt, vals)
            }
            EnumTag => {
                let v = self.expr(recv.unwrap());
                self.int_out(LE::Field(Box::new(v), 0))
            }
            TupleGet(k) => {
                let r = recv.unwrap();
                let v = self.expr(r);
                if boxed_slot(&r.ty, k).is_some() {
                    let lt = self.lty(&r.ty);
                    let v = self.bind(v, lt);
                    return self.slot_read(&r.ty, v, k);
                }
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
                let key = e as *const TExpr as usize;
                let open = self.open_iter(key);
                let inner = self.sub(|lw| {
                    lw.compact_owners(key);
                    if let Some((r, sv, _)) = open {
                        lw.emit(LS::RegionExit { region: r, saved: sv });
                        lw.emit(LS::RegionEnter { region: r, saved: sv });
                    }
                    lw.next_target.push((Some(l), lw.defers.len()));
                    lw.break_target.push((l, lw.defers.len()));
                    lw.stmts(&b.body);
                    lw.break_target.pop();
                    lw.next_target.pop();
                });
                self.emit(LS::Loop(l, inner));
                self.close_iter(open);
                LE::Unit
            }
            EnumNew => self.generator(e, blk.unwrap()),
            Exit => {
                let c = self.expr(&args[0]);
                self.emit(LS::Exit(c));
                LE::Unit
            }
            PtrNull => LE::I(0),
            CErrno | CStrerror | StrFromCstr | StrFromPtr => {
                let (rt, ty, n) = match m {
                    CErrno => (Rt::Errno, LTy::I64, 0),
                    CStrerror => (Rt::Strerror, LTy::Str, 1),
                    StrFromCstr => (Rt::StrFromCstr, LTy::Str, 1),
                    _ => (Rt::StrFromPtr, LTy::Str, 2),
                };
                let mut a = vec![];
                for (i, x) in args.iter().enumerate().take(n) {
                    let v = self.expr(x);
                    a.push(if x.ty == Ty::Int { self.int_in(v, sp) } else { v });
                    let _ = i;
                }
                let t = self.tmp(ty.clone());
                self.emit(LS::Set(t, LE::Rt(rt, a)));
                if ty == LTy::I64 && self.lty(&e.ty) == LTy::PInt { LE::ToP(Box::new(LE::Var(t))) } else { LE::Var(t) }
            }
            NowNs | CapBegin | CapEnd => {
                let (rt, ty) = match m {
                    NowNs => (Rt::NowNs, LTy::I64),
                    CapBegin => (Rt::CapBegin, LTy::I64),
                    _ => (Rt::CapEnd, LTy::Str),
                };
                let t = self.tmp(ty);
                self.emit(LS::Set(t, LE::Rt(rt, vec![])));
                if m == CapBegin { LE::Unit } else { LE::Var(t) }
            }
            FileRead => {
                // Without `~`: a `~Str` value.
                let p = self.expr(&args[0]);
                let p = self.bind(p, LTy::Str);
                let st = self.tmp(LTy::I64);
                self.emit(LS::Set(st, LE::Rt(Rt::FileStatus, vec![p.clone()])));
                let ok = LE::Cmp(Op::Eq, Box::new(LE::Var(st)), Box::new(LE::I(0)), LTy::I64);
                let err = self.io_error(LE::Var(st), p.clone(), sp);
                let et = self.lty(&Ty::Error);
                let rt = self.lty(&e.ty);
                LE::Tup(rt, vec![ok.clone(), LE::Cond(Box::new(ok.clone()), Box::new(LE::Rt(Rt::FileRead, vec![p])), Box::new(LE::S(String::new()))), LE::Cond(Box::new(ok), Box::new(zero_le(&et)), Box::new(err))])
            }
            ToError(k) => {
                let v = self.arg(recv.unwrap());
                self.error_value(k, v, sp)
            }
            ErrMessage => {
                let v = self.expr(recv.unwrap());
                self.error_message(v)
            }
            ErrWrap => {
                let et = self.lty(&Ty::Error);
                let bt = error_body(self.mode);
                let v = self.expr(recv.unwrap());
                let v = self.bind(v, et);
                let c = self.expr(&args[0]);
                let LTy::Tup(ts) = &bt else { unreachable!() };
                let b = err_body(v);
                let fields = (0..ts.len()).map(|i| if i == 2 { LE::Rt(Rt::StrCat, vec![c.clone(), LE::S(": ".into()), LE::Field(Box::new(b.clone()), 2)]) } else { LE::Field(Box::new(b.clone()), i) }).collect();
                LE::ArrLit(bt.clone(), vec![LE::Tup(bt, fields)])
            }
            ErrIs(k) => {
                let v = self.expr(recv.unwrap());
                LE::Cmp(Op::Eq, Box::new(LE::Field(Box::new(err_body(v)), 0)), Box::new(LE::I(k as i64)), LTy::I64)
            }
            ErrAs(k) => {
                let v = self.expr(recv.unwrap());
                LE::Field(Box::new(err_body(v)), 3 + k)
            }
            // An interface value is (tag, implementor 0, implementor 1, ...).
            IfaceIs(k) => {
                let v = self.expr(recv.unwrap());
                LE::Cmp(Op::Eq, Box::new(LE::Field(Box::new(v), 0)), Box::new(LE::I(k as i64)), LTy::I64)
            }
            IfaceAs(k) => {
                let r = recv.unwrap();
                let v = self.expr(r);
                let v = self.bind(v, self.lty(&r.ty));
                self.iface_get(v, &r.ty, k)
            }
            ResOk | ResErr | ResIsOk | ResUnwrap | ResUnwrapOr | ResRescue => {
                let r = recv.unwrap();
                let rv = self.expr(r);
                let rv = self.bind(rv, self.lty(&r.ty));
                let ok = LE::Field(Box::new(rv.clone()), 0);
                let val = LE::Field(Box::new(rv.clone()), 1);
                let err = LE::Field(Box::new(rv.clone()), 2);
                match m {
                    ResIsOk => ok,
                    ResOk => LE::Tup(self.lty(&e.ty), vec![ok, val]),
                    ResErr => LE::Tup(self.lty(&e.ty), vec![LE::Not(Box::new(ok)), err]),
                    ResUnwrapOr => {
                        let d = self.expr(&args[0]);
                        LE::Cond(Box::new(ok), Box::new(val), Box::new(d))
                    }
                    ResUnwrap if self.try_mode => {
                        // Under `~`: the error propagates as it is.
                        self.unwrap_result(rv.clone());
                        val
                    }
                    ResUnwrap => {
                        let msg = self.error_message(err);
                        let die = LS::Die(LE::Rt(Rt::StrCat, vec![LE::S("unwrap of an error: ".into()), msg, LE::S(format!(" ({})", self.loc(sp)))]));
                        self.emit(LS::If(LE::Not(Box::new(ok)), vec![die], vec![]));
                        val
                    }
                    _ => {
                        let out = self.tmp(self.lty(&e.ty));
                        self.emit(LS::Set(out, val));
                        let b = blk.unwrap();
                        let mut body = self.sub(|lw| {
                            let v = lw.inline_block(b, &[err], &[], None);
                            lw.emit(LS::Set(out, v));
                        });
                        // A block that ends in `fail` / `return` / a panic has
                        // no value: nothing to store after the jump (the C and
                        // Rust backends reject a Unit stored into the result).
                        let n = body.len();
                        if n >= 2 && matches!(body[n - 2], LS::Return(_) | LS::Die(_) | LS::Panic(..) | LS::Break(_) | LS::Continue(_)) {
                            body.pop();
                        }
                        self.emit(LS::If(LE::Not(Box::new(ok)), body, vec![]));
                        LE::Var(out)
                    }
                }
            }
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
        self.guard(LE::Cmp(Op::Le, Box::new(LE::Var(byv)), Box::new(LE::I(0)), LTy::I64), "`step` needs a positive step", args[1].span, "ArithError", 2);
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
                    } else if lw.try_mode {
                        lw.checked_arith(Op::Add, LE::Var(a), v, sp)
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
                    // One region per element (freed on every way out, `next` too).
                    let open = lw.open_iter(e as *const TExpr as usize);
                    if open.is_some() {
                        lw.next_depth = Some(lw.defers.len() - 1);
                    }
                    let v = lw.inline_block(blk.unwrap(), &[x], &[fact], Some(inner));
                    if !matches!(v, LE::Unit | LE::Var(_) | LE::I(_) | LE::B(_)) {
                        lw.emit(LS::Eval(v));
                    }
                    lw.close_iter(open);
                    lw.compact_owners(e as *const TExpr as usize);
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
                    if args.is_empty() {
                        lw.emit(LS::If(LE::Var(h), s, vec![LS::Set(a, x), LS::Set(h, LE::B(true))]));
                    } else {
                        // A start value: the accumulator may be another type.
                        lw.block(&s);
                    }
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
                let _ = loc;
                self.guard(LE::Not(Box::new(LE::Var(have.unwrap()))), &format!("`{what}` of an empty collection"), sp, "IndexError", 2);
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
            let counter = if matches!(m, M::Drop | M::Take | M::StepBy | M::EachWithIndex) {
                let c = self.tmp(LTy::I64);
                self.emit(LS::Set(c, LE::I(0)));
                Some(c)
            } else {
                None
            };
            let limit = if matches!(m, M::Drop | M::Take | M::StepBy) {
                let n = self.expr(&args[0]);
                let n = self.int_in(n, args[0].span);
                let nv = self.tmp(LTy::I64);
                self.emit(LS::Set(nv, n));
                if *m == M::StepBy {
                    self.guard(LE::Cmp(Op::Le, Box::new(LE::Var(nv)), Box::new(LE::I(0)), LTy::I64), "`step` must be positive", s.span, "ArithError", 0);
                }
                Some(nv)
            } else {
                None
            };
            st.push(Stage { node: s, counter, limit });
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
                // A first `step(n)` stage: the loop counts by n instead.
                let by = st.first().filter(|s| matches!(s.node.kind, TK::M(M::StepBy, ..))).and_then(|s| s.limit);
                let first = if by.is_some() { 1 } else { 0 };
                let fact = if by.is_some() { None } else { fact };
                let bounded = by.is_none() && matches!(fact, Some(Fact::Interval(_, b)) if b < i64::MAX);
                let body = self.sub(|lw| {
                    let stop = if excl { Op::Ge } else { Op::Gt };
                    lw.emit(LS::If(LE::Cmp(stop, Box::new(LE::Var(i)), Box::new(LE::Var(h)), LTy::I64), vec![LS::Break(l)], vec![]));
                    let x = lw.tmp(LTy::I64);
                    lw.emit(LS::Set(x, LE::Var(i)));
                    if let Some(n) = by {
                        // Counting by n: the last element is the one less than n
                        // from the end (an unsigned distance, so nothing
                        // overflows); after it the loop stops.
                        let gap = LE::Arith(Op::Sub, Box::new(LE::Var(h)), Box::new(LE::Var(i)), Ovf::Wrap);
                        let last = LE::Prim(Prim::ULt, vec![gap, LE::Var(n)]);
                        let step = LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::Var(n)), Ovf::Wrap);
                        lw.emit(LS::If(last, vec![LS::Set(h, LE::I(i64::MIN))], vec![LS::Set(i, step)]));
                    } else if bounded {
                        lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    } else {
                        lw.emit(LS::Set(i, LE::Rt(Rt::SatAdd, vec![LE::Var(i), LE::I(1)])));
                    }
                    // Inclusive range ending at i64::MAX: stop after it.
                    if !excl && !bounded && by.is_none() {
                        lw.emit(LS::If(LE::Cmp(Op::Eq, Box::new(LE::Var(x)), Box::new(LE::I(i64::MAX)), LTy::I64), vec![LS::Set(h, LE::I(i64::MIN))], vec![]));
                    }
                    let xv = if promote { LE::ToP(Box::new(LE::Var(x))) } else { LE::Var(x) };
                    lw.apply(&st, first, xv, fact, l, outer, k);
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
            (_, Ty::Pool(t)) => {
                // Every live slot, as (handle, value).
                let pv = self.expr(base);
                let pv = self.bind(pv, base_lty.clone());
                let tl = LTy::Tup(vec![LTy::I64, self.lty(t)]);
                let l = own_label.unwrap_or_else(|| self.label());
                let i = self.tmp(LTy::I64);
                self.emit(LS::Set(i, LE::I(0)));
                let body = self.sub(|lw| {
                    let slots = LE::Field(Box::new(LE::Index { arr: Box::new(pv.clone()), idx: Box::new(LE::I(0)), check: None }), 0);
                    lw.emit(LS::If(LE::Cmp(Op::Ge, Box::new(LE::Var(i)), Box::new(LE::Len(Box::new(slots.clone()))), LTy::I64), vec![LS::Break(l)], vec![]));
                    let slot_lt = match &base_lty {
                        LTy::Arr(h) => match &**h {
                            LTy::Tup(hs) => match &hs[0] {
                                LTy::Arr(st) => (**st).clone(),
                                _ => unreachable!(),
                            },
                            _ => unreachable!(),
                        },
                        _ => unreachable!(),
                    };
                    let slot = lw.tmp(slot_lt);
                    lw.emit(LS::Set(slot, LE::Index { arr: Box::new(slots), idx: Box::new(LE::Var(i)), check: None }));
                    let slot = LE::Var(slot);
                    let x = lw.tmp(tl.clone());
                    lw.emit(LS::Set(i, LE::Arith(Op::Add, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked)));
                    lw.emit(LS::If(LE::Not(Box::new(gen_live(LE::Field(Box::new(slot.clone()), 0)))), vec![LS::Continue(l)], vec![]));
                    let h = make_handle(LE::Arith(Op::Sub, Box::new(LE::Var(i)), Box::new(LE::I(1)), Ovf::Unchecked), LE::Field(Box::new(slot.clone()), 0));
                    lw.emit(LS::Set(x, LE::Tup(tl.clone(), vec![h, LE::Field(Box::new(slot), 1)])));
                    lw.apply(&st, 0, LE::Var(x), None, l, outer, k);
                });
                self.emit(LS::Loop(l, body));
            }
            (_, Ty::Chan(t)) => {
                // Receive until the channel is closed and drained.
                let cv = self.expr(base);
                let cv = self.bind(cv, base_lty.clone());
                let el = self.lty(t);
                let l = own_label.unwrap_or_else(|| self.label());
                let body = self.sub(|lw| {
                    let ok = lw.tmp(LTy::Bool);
                    let x = lw.tmp(el.clone());
                    lw.emit(LS::Set(x, zero_le(&el)));
                    lw.emit(LS::ChanRecv { ch: cv.clone(), ok, val: x });
                    lw.emit(LS::If(LE::Not(Box::new(LE::Var(ok))), vec![LS::Break(l)], vec![]));
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
            M::StepBy => {
                // Every n-th: the counter counts down from n - 1 to 0.
                let c = s.counter.unwrap();
                let n = s.limit.unwrap();
                let skip = LE::Cmp(Op::Gt, Box::new(LE::Var(c)), Box::new(LE::I(0)), LTy::I64);
                let dec = LS::Set(c, LE::Arith(Op::Add, Box::new(LE::Var(c)), Box::new(LE::I(-1)), Ovf::Unchecked));
                self.emit(LS::If(skip, vec![dec, LS::Continue(inner)], vec![]));
                self.emit(LS::Set(c, LE::Arith(Op::Add, Box::new(LE::Var(n)), Box::new(LE::I(-1)), Ovf::Unchecked)));
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
        g.nested_place = self.place.or(self.lambda_place).or(self.nested_place);
        g.consts = self.consts.clone();
        g.fixed_len = self.fixed_len.clone();
        let cap_vars: Vec<V> = caps.iter().map(|l| g.var_of(*l)).collect();
        for p in &b.params {
            g.var_of(*p);
        }
        let body = g.sub(|g| g.stmts(&b.body));
        let mut prog = self.prog.borrow_mut();
        let id = prog.gens.len();
        let func = LFunc { name: format!("gen{id}"), params: vec![], vars: g.vars, ret: LTy::Unit, body, external: false, is_main: false, labels: g.labels };
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
        // A block using `~` returns a Result per element; the first error
        // propagates after all workers finish.
        let fallible = format!("{:?}", b.body).contains("Try(");
        let mut w = Lw::new(self.p, self.sm, self.opts, self.f, self.mode, ErrPath::Return(vec![]), self.prog);
        w.nested_place = self.place.or(self.lambda_place).or(self.nested_place);
        if fallible {
            w.res = Some(ok_lty(out_ty.clone()));
        }
        let param = w.new_var("x", in_ty.clone());
        let (body_stmts, v) = w.sub_val(|w| w.inline_block(b, &[LE::Var(param)], &[], None));
        let mut body = body_stmts;
        let v = if fallible { w.ok_result(v) } else { v };
        body.push(LS::Return(Some(v)));
        let wret = if fallible { result_lty(out_ty.clone(), self.mode) } else { out_ty.clone() };
        let mut prog = self.prog.borrow_mut();
        let id = prog.workers.len();
        let func = LFunc { name: format!("worker{id}"), params: vec![param], vars: std::mem::take(&mut w.vars), ret: wret.clone(), body, external: false, is_main: false, labels: w.labels };
        prog.workers.push(LWorker { id, input: in_ty, func });
        drop(prog);
        let dst = self.tmp(LTy::Arr(Box::new(out_ty)));
        let err = fallible.then(|| self.tmp(wret));
        self.emit(LS::Pmap { dst, arr, worker: id, err });
        if let Some(err) = err {
            self.unwrap_result(LE::Var(err));
        }
        LE::Var(dst)
    }
}

/// The constant operand of a multiplication, if one is.
fn const_factor(a: &LE, b: &LE) -> Option<i64> {
    match (a, b) {
        (LE::I(k), _) | (_, LE::I(k)) => Some(*k),
        _ => None,
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

#[derive(Clone, Debug)]
enum ErrPath {
    /// In a fallible function: return the error. The statements are the
    /// deferred code of every enclosing block, run first.
    Return(Vec<LS>),
    /// At the top level: print the error and exit 1 (after the deferred code).
    Die(Vec<LS>),
}

impl ErrPath {
    pub fn cleanup(&self) -> &[LS] {
        match self {
            ErrPath::Return(c) | ErrPath::Die(c) => c,
        }
    }
}

pub(crate) fn collect_locals_stmt(s: &TStmt, out: &mut Vec<LocalId>) {
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

pub(crate) fn collect_locals(e: &TExpr, out: &mut Vec<LocalId>) {
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
        TK::Neg(x) | TK::Not(x) | TK::Try(x) | TK::Puts(x) | TK::Panic(x) | TK::Some(x) => collect_locals(x, out),
        TK::Select(arms, d) => {
            for a in arms {
                match a {
                    TSelArm::Recv { ch, bind, body } => {
                        collect_locals(ch, out);
                        out.extend(bind.iter().copied());
                        body.iter().for_each(|s| collect_locals_stmt(s, out));
                    }
                    TSelArm::Send { ch, val, body } => {
                        collect_locals(ch, out);
                        collect_locals(val, out);
                        body.iter().for_each(|s| collect_locals_stmt(s, out));
                    }
                }
            }
            d.iter().flatten().for_each(|s| collect_locals_stmt(s, out));
        }
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
        TK::PlaceAssign(l, steps, _, v) | TK::Bang(l, steps, _, v) => {
            out.push(*l);
            if let TK::Bang(_, _, view, _) = &e.kind {
                out.push(*view);
            }
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
        LTy::Region => LE::RegionProgram,
        // A fresh closed-over channel stands in for "no channel" (never observed).
        LTy::Chan(e) => LE::ChanNew((**e).clone(), Box::new(LE::I(0))),
        LTy::Task(_) => LE::NullTask(t.clone()),
        LTy::Lock => LE::LockNew,
        LTy::Atomic => LE::AtomicNew(Box::new(LE::I(0))),
        LTy::I64 | LTy::IntK(_) => LE::I(0),
        LTy::F64 => LE::F(0.0),
        LTy::Bool => LE::B(false),
        LTy::Unit => LE::Unit,
        LTy::Str => LE::S(String::new()),
        LTy::PInt => LE::ToP(Box::new(LE::I(0))),
        LTy::Arr(e) => LE::ArrWithCap((**e).clone(), Box::new(LE::I(0))),
        LTy::Tup(ts) => LE::Tup(t.clone(), ts.iter().map(zero_le).collect()),
        LTy::Rec(_) => LE::Tup(t.clone(), t.tup_fields().iter().map(zero_le).collect()),
        LTy::Range => LE::Range(Box::new(LE::I(0)), Box::new(LE::I(0)), false),
        LTy::Gen(_) => panic!("an optional generator has no zero value yet"),
    }
}

fn lty_has_storage(t: &LTy) -> bool {
    match t {
        LTy::Str | LTy::Arr(_) | LTy::PInt | LTy::Gen(_) | LTy::Rec(_) => true,
        LTy::Tup(ts) => ts.iter().any(lty_has_storage),
        _ => false,
    }
}

/// Can evaluating `e` allocate in the current region? (Conservative: only
/// forms known not to are cleared.)
fn le_allocates(e: &LE, rets: &(std::collections::HashSet<String>, std::collections::HashSet<String>)) -> bool {
    let any = |xs: &[LE]| xs.iter().any(|x| le_allocates(x, rets));
    let one = |x: &LE| le_allocates(x, rets);
    match e {
        LE::Var(_) | LE::I(_) | LE::F(_) | LE::B(_) | LE::S(_) | LE::SB(_) | LE::Loc(_) | LE::Unit | LE::RegionProgram | LE::Global(_) => false,
        LE::Tup(_, xs) | LE::Prim(_, xs) => any(xs),
        LE::Field(x, _) | LE::Neg(x, _) | LE::FNeg(x) | LE::Not(x) | LE::Len(x) | LE::RangeField(x, _) | LE::ChanLen(x) | LE::LockPoisoned(x) | LE::AtomicLoad(x) | LE::RegionOf(x) | LE::ViewRegion(x) | LE::RegionBytes(x) => one(x),
        LE::Arith(_, a, b, _) | LE::Cmp(_, a, b, _) | LE::FArith(_, a, b) | LE::Range(a, b, _) => one(a) || one(b),
        LE::Cond(a, b, c) => one(a) || one(b) || one(c),
        LE::Index { arr, idx, .. } => one(arr) || one(idx),
        LE::Call(name, args) => !rets.1.contains(name) || rets.0.contains(name) || any(args),
        LE::Rt(r, args) => !matches!(r, Rt::RegionCur | Rt::Even | Rt::Isqrt | Rt::SatAdd) || any(args),
        _ => true,
    }
}

/// Does anything allocate while `frame` is the current region?
fn frame_allocates(body: &[LS], frame: V, rets: &(std::collections::HashSet<String>, std::collections::HashSet<String>)) -> bool {
    // saved var -> was the frame current when it was saved
    fn walk(ss: &[LS], cur: &mut bool, saved: &mut HashMap<V, bool>, frame: V, rets: &(std::collections::HashSet<String>, std::collections::HashSet<String>)) -> bool {
        for s in ss {
            let alloc = |e: &LE| le_allocates(e, rets);
            let hit = match s {
                LS::RegionEnter { region, saved: sv } => {
                    saved.insert(*sv, *cur);
                    *cur = *region == frame;
                    false
                }
                LS::RegionExit { saved: sv, .. } => {
                    *cur = saved.get(sv).copied().unwrap_or(true);
                    false
                }
                LS::RegionUse { region, saved: sv } => {
                    saved.insert(*sv, *cur);
                    *cur = matches!(region, LE::Var(v) if *v == frame);
                    alloc(region) && *cur
                }
                LS::RegionRestore(sv) => {
                    *cur = saved.get(sv).copied().unwrap_or(true);
                    false
                }
                LS::Set(_, e) | LS::Eval(e) | LS::Puts(e, _) | LS::Print(e) | LS::PanicStr(e) | LS::Exit(e) | LS::Die(e) | LS::Lock(e, _) | LS::Unlock(e) | LS::LockClearPoison(e) => *cur && alloc(e),
                LS::Return(e) => *cur && e.as_ref().is_some_and(alloc),
                LS::SetIndex { idx, val, .. } => *cur && (alloc(idx) || alloc(val)),
                LS::AtomicStore(a, b) => *cur && (alloc(a) || alloc(b)),
                LS::If(c, a, b) => {
                    if *cur && alloc(c) {
                        return true;
                    }
                    let (mut ca, mut cb) = (*cur, *cur);
                    if walk(a, &mut ca, saved, frame, rets) || walk(b, &mut cb, saved, frame, rets) {
                        return true;
                    }
                    *cur = ca || cb;
                    false
                }
                LS::Loop(_, b) => {
                    // Twice: the state at the end of the body flows back to its start.
                    let mut c = *cur;
                    if walk(b, &mut c, saved, frame, rets) {
                        return true;
                    }
                    let mut c2 = *cur || c;
                    if walk(b, &mut c2, saved, frame, rets) {
                        return true;
                    }
                    *cur = *cur || c2;
                    false
                }
                LS::Break(_) | LS::Continue(_) | LS::Panic(..) | LS::RegionFree(_) | LS::Unview { .. } => false,
                LS::View { region, .. } => *cur && alloc(region),
                _ => *cur,
            };
            if hit {
                return true;
            }
        }
        false
    }
    let mut cur = false;
    let mut saved = HashMap::new();
    walk(body, &mut cur, &mut saved, frame, rets)
}

/// Replaces the frame's enter with "the frame is the caller's current
/// region" and removes its exits (nothing was allocated in it).
fn drop_frame(body: &mut Vec<LS>, frame: V, dest: V) {
    fn walk(ss: &mut Vec<LS>, frame: V, dest: V) {
        let mut out = Vec::with_capacity(ss.len());
        for s in ss.drain(..) {
            match s {
                LS::RegionEnter { region, saved } if region == frame => {
                    let _ = dest;
                    out.push(LS::Set(saved, LE::Rt(Rt::RegionCur, vec![])));
                    out.push(LS::Set(frame, LE::Var(saved)));
                }
                LS::RegionExit { region, .. } if region == frame => {}
                LS::If(c, mut a, mut b) => {
                    walk(&mut a, frame, dest);
                    walk(&mut b, frame, dest);
                    out.push(LS::If(c, a, b));
                }
                LS::Loop(l, mut b) => {
                    walk(&mut b, frame, dest);
                    out.push(LS::Loop(l, b));
                }
                o => out.push(o),
            }
        }
        *ss = out;
    }
    walk(body, frame, dest);
}

/// Does anything in `ss` allocate (whatever region is current)?
fn stmts_allocate(ss: &[LS], rets: &(std::collections::HashSet<String>, std::collections::HashSet<String>)) -> bool {
    let alloc = |e: &LE| le_allocates(e, rets);
    ss.iter().any(|s| match s {
        LS::Set(_, e) | LS::Eval(e) | LS::Puts(e, _) | LS::Print(e) | LS::Lock(e, _) | LS::Unlock(e) | LS::LockClearPoison(e) => alloc(e),
        LS::SetIndex { idx, val, .. } => alloc(idx) || alloc(val),
        LS::If(c, a, b) => alloc(c) || stmts_allocate(a, rets) || stmts_allocate(b, rets),
        LS::Loop(_, b) => stmts_allocate(b, rets),
        LS::Break(_) | LS::Continue(_) | LS::Unview { .. } => false,
        LS::View { region, .. } => alloc(region),
        _ => true,
    })
}

/// `RegionUse` ... `RegionRestore` with nothing allocated in between (a
/// value built in place, say) switches regions for nothing: dropped.
fn drop_idle_switches(ss: &mut Vec<LS>, rets: &(std::collections::HashSet<String>, std::collections::HashSet<String>)) {
    for s in ss.iter_mut() {
        match s {
            LS::If(_, a, b) => {
                drop_idle_switches(a, rets);
                drop_idle_switches(b, rets);
            }
            LS::Loop(_, b) => drop_idle_switches(b, rets),
            _ => {}
        }
    }
    let mut i = 0;
    while i < ss.len() {
        if let LS::RegionUse { region, saved } = &ss[i] {
            let saved = *saved;
            let pure_region = !matches!(region, LE::Call(..) | LE::Rt(..));
            if let Some(j) = ss[i + 1..].iter().position(|t| matches!(t, LS::RegionRestore(v) if *v == saved)).map(|k| i + 1 + k) {
                let rest = format!("{:?}", &ss[j + 1..]);
                let used_later = rest.contains(&format!("RegionRestore({saved})")) || rest.contains(&format!("Var({saved})"));
                if pure_region && !used_later && !stmts_allocate(&ss[i + 1..j], rets) {
                    ss.remove(j);
                    ss.remove(i);
                    continue;
                }
            }
        }
        i += 1;
    }
}

/// Can loop iteration region `r` be a mark/reset on the region current at
/// the loop instead of a region of its own? Only if, while it's open, every
/// allocation goes to it (none while some other existing region is made
/// current) and no call could store into a container through its arguments.
fn iter_light_ok(body: &[LS], r: V, rets: &(std::collections::HashSet<String>, std::collections::HashSet<String>), storage_params: &std::collections::HashSet<String>) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        Closed,
        Mine,
        Fresh,
        Other,
    }
    fn calls_storing(e: &LE, sp: &std::collections::HashSet<String>) -> bool {
        let mut hit = false;
        visit_le(e, &mut |x| {
            if let LE::Call(n, _) = x {
                if sp.contains(n) {
                    hit = true;
                }
            }
            if matches!(x, LE::Ffi(..)) {
                hit = true;
            }
        });
        hit
    }
    fn walk(ss: &[LS], st: &mut St, saved: &mut HashMap<V, St>, r: V, rets: &(std::collections::HashSet<String>, std::collections::HashSet<String>), sp: &std::collections::HashSet<String>) -> bool {
        for s in ss {
            let alloc = |e: &LE| le_allocates(e, rets);
            let open = *st != St::Closed;
            let bad = match s {
                LS::RegionEnter { region, saved: sv } => {
                    saved.insert(*sv, *st);
                    *st = if *region == r { St::Mine } else if open { St::Fresh } else { St::Closed };
                    false
                }
                LS::RegionExit { region, saved: sv } => {
                    *st = if *region == r { St::Closed } else { saved.get(sv).copied().unwrap_or(St::Other) };
                    false
                }
                LS::RegionUse { region, saved: sv } => {
                    saved.insert(*sv, *st);
                    if open {
                        *st = if matches!(region, LE::Var(v) if *v == r) { St::Mine } else { St::Other };
                    }
                    false
                }
                LS::RegionRestore(sv) => {
                    if open {
                        *st = saved.get(sv).copied().unwrap_or(St::Other);
                    }
                    false
                }
                LS::If(c, a, b) => {
                    if open && ((*st == St::Other && alloc(c)) || calls_storing(c, sp)) {
                        return false;
                    }
                    let (mut sa, mut sb) = (*st, *st);
                    if !walk(a, &mut sa, saved, r, rets, sp) || !walk(b, &mut sb, saved, r, rets, sp) {
                        return false;
                    }
                    *st = if sa == sb { sa } else if sa == St::Closed || sb == St::Closed { St::Other } else { St::Other };
                    false
                }
                LS::Loop(_, b) => {
                    let mut c = *st;
                    if !walk(b, &mut c, saved, r, rets, sp) {
                        return false;
                    }
                    let mut c2 = if c == *st { c } else { St::Other };
                    if *st != St::Closed && c2 == St::Other && *st == St::Closed {
                        c2 = St::Other;
                    }
                    if !walk(b, &mut c2, saved, r, rets, sp) {
                        return false;
                    }
                    *st = c2;
                    false
                }
                _ if !open => false,
                LS::Break(_) | LS::Continue(_) | LS::Panic(..) => false,
                LS::Set(_, e) | LS::Eval(e) | LS::Return(Some(e)) | LS::Puts(e, _) | LS::Print(e) | LS::Die(e) | LS::PanicStr(e) | LS::Exit(e) => calls_storing(e, sp) || (*st == St::Other && alloc(e)),
                LS::Return(None) | LS::Unview { .. } => false,
                LS::View { region, .. } => calls_storing(region, sp) || (*st == St::Other && alloc(region)),
                LS::SetIndex { idx, val, .. } => calls_storing(idx, sp) || calls_storing(val, sp) || (*st == St::Other && (alloc(idx) || alloc(val))),
                LS::Push(_, e) => calls_storing(e, sp) || *st == St::Other,
                _ => true,
            };
            if bad {
                return false;
            }
        }
        true
    }
    let mut st = St::Closed;
    walk(body, &mut st, &mut HashMap::new(), r, rets, storage_params)
}

fn visit_le(e: &LE, f: &mut dyn FnMut(&LE)) {
    f(e);
    match e {
        LE::Tup(_, xs) | LE::Prim(_, xs) | LE::Call(_, xs) | LE::Ffi(_, xs) | LE::Rt(_, xs) | LE::ArrLit(_, xs) | LE::GenNew(_, xs) => xs.iter().for_each(|x| visit_le(x, f)),
        LE::Field(x, _) | LE::Neg(x, _) | LE::FNeg(x) | LE::Not(x) | LE::Len(x) | LE::RangeField(x, _) | LE::ChanLen(x) | LE::LockPoisoned(x) | LE::AtomicLoad(x) | LE::RegionOf(x) | LE::ViewRegion(x) | LE::RegionBytes(x) | LE::RegionNew(x) | LE::ToP(x) | LE::AtomicNew(x) | LE::ArrWithCap(_, x) | LE::ChanNew(_, x) => visit_le(x, f),
        LE::Arith(_, a, b, _) | LE::Cmp(_, a, b, _) | LE::FArith(_, a, b) | LE::PArith(_, a, b) | LE::Range(a, b, _) | LE::AtomicRmw(_, a, b) | LE::ArrNew(_, a, b, _) => {
            visit_le(a, f);
            visit_le(b, f);
        }
        LE::Cond(a, b, c) | LE::Slice(_, a, b, c) | LE::AtomicCas(a, b, c) => {
            visit_le(a, f);
            visit_le(b, f);
            visit_le(c, f);
        }
        LE::Index { arr, idx, .. } => {
            visit_le(arr, f);
            visit_le(idx, f);
        }
        _ => {}
    }
}

/// Iteration region `r` becomes a mark (enter) and a reset (exit) on the
/// region current at the loop; `r` itself names that region.
fn light_iter(ss: &mut Vec<LS>, r: V, mark: V, larges: V) {
    let mut out = Vec::with_capacity(ss.len());
    for s in ss.drain(..) {
        match s {
            LS::RegionEnter { region, saved } if region == r => {
                out.push(LS::Set(saved, LE::Rt(Rt::RegionCur, vec![])));
                out.push(LS::Set(mark, LE::Rt(Rt::RegionMark, vec![])));
                out.push(LS::Set(larges, LE::Rt(Rt::RegionMarkLarges, vec![])));
                // (After the mark: on the program region it opens a region.)
                out.push(LS::Set(r, LE::Rt(Rt::RegionCur, vec![])));
            }
            LS::RegionExit { region, .. } if region == r => {
                out.push(LS::Eval(LE::Rt(Rt::RegionReset, vec![LE::Var(mark), LE::Var(larges)])));
            }
            LS::If(c, mut a, mut b) => {
                light_iter(&mut a, r, mark, larges);
                light_iter(&mut b, r, mark, larges);
                out.push(LS::If(c, a, b));
            }
            LS::Loop(l, mut b) => {
                light_iter(&mut b, r, mark, larges);
                out.push(LS::Loop(l, b));
            }
            o => out.push(o),
        }
    }
    *ss = out;
}

/// Can a `!` method keep its receiver (the view of its caller's place)
/// past the call? Only through `self` itself, the one-element slice: a
/// lambda, task or generator capturing it, or a copy of it stored
/// somewhere. Its other uses (`self[0]`, places under it, receivers of
/// further `!` calls) end with the call.
pub(crate) fn self_escapes(f: &TFunc) -> bool {
    let Some(&me) = f.params.first() else { return false };
    if !f.src_name.ends_with('!') || f.locals[me].name != "self" || !matches!(f.locals[me].ty, Ty::Array(_)) {
        return false;
    }
    fn walk(e: &TExpr, me: LocalId, hit: &mut bool) {
        if *hit {
            return;
        }
        match &e.kind {
            TK::Local(l) if *l == me => {
                *hit = true;
                return;
            }
            // `self[0]`.
            TK::Index(a, i) if matches!(a.kind, TK::Local(l) if l == me) => {
                walk(i, me, hit);
                return;
            }
            // The receiver of a `!` call: a view of `self[0]`'s place, or
            // `self` passed through (its first argument).
            TK::Call(_, args) if matches!(args.first().map(|a| &a.kind), Some(TK::Local(l)) if *l == me) => {
                args[1..].iter().for_each(|a| walk(a, me, hit));
                return;
            }
            _ => {}
        }
        crate::prove::each_child(e, &mut |c| walk(c, me, hit));
    }
    let mut hit = false;
    for s in &f.body {
        crate::prove::stmt_exprs(s, &mut |e| walk(e, me, &mut hit));
    }
    hit
}

/// The range of an integer type narrower than 64 bits (I64, U64: none).
fn ty_range(t: &Ty) -> Option<(i64, i64)> {
    match t {
        Ty::IntK(k) if k.bits() < 64 => Some((k.min() as i64, k.max() as i64)),
        _ => None,
    }
}

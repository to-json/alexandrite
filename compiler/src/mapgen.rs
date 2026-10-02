//! `Map[K, V]` as LIR: every backend gets ordinary functions over arrays.
//!
//! A map is a one-element array (so copies share it, as Go maps do) whose
//! element is the header:
//!
//!   (keys: [K], vals: [V], live: [Bool], slots: [I64], count: I64)
//!
//! Entries are kept in insertion order; `slots` is an open-addressing
//! table of entry indices (-1 = empty). Deleting clears `live` and leaves
//! the slot in place as a tombstone; growing rebuilds both, dropping the
//! dead entries.

use crate::lir::*;

pub const KEYS: usize = 0;
pub const VALS: usize = 1;
pub const LIVE: usize = 2;
pub const SLOTS: usize = 3;
pub const COUNT: usize = 4;
/// The region holding the contents (R3: an owning map's child region;
/// otherwise the program region, unused), and its size after the last
/// compaction.
pub const REGION: usize = 5;
pub const LIVE_BYTES: usize = 6;
/// The entry the last successful lookup found (-1: none): `m[k] = f(m[k])`
/// and repeated lookups of one key skip the hash. Checked against the key,
/// so a stale index is harmless.
pub const LAST: usize = 7;

pub fn header(k: &LTy, v: &LTy) -> LTy {
    LTy::Tup(vec![LTy::Arr(Box::new(k.clone())), LTy::Arr(Box::new(v.clone())), LTy::Arr(Box::new(LTy::Bool)), LTy::Arr(Box::new(LTy::I64)), LTy::I64, LTy::Region, LTy::I64, LTy::I64])
}

pub fn map_ty(k: &LTy, v: &LTy) -> LTy {
    LTy::Arr(Box::new(header(k, v)))
}

/// Names of one instantiation's functions.
pub struct MapFns {
    pub new: String,
    pub find: String,
    pub set: String,
    pub del: String,
}

/// Generate (once per K, V) the functions and return their names.
pub fn instantiate(prog: &mut LProgram, k: &LTy, v: &LTy) -> MapFns {
    let key = format!("{k:?}/{v:?}");
    let id = match prog.maps.iter().position(|m| *m == key) {
        Some(i) => i,
        None => {
            prog.maps.push(key);
            let id = prog.maps.len() - 1;
            let names = names(id);
            prog.funcs.push(gen_new(&names, k, v));
            prog.funcs.push(gen_find(&names, k, v));
            prog.funcs.push(gen_set(&names, k, v));
            prog.funcs.push(gen_del(&names, k, v));
            return names;
        }
    };
    names(id)
}

fn names(id: usize) -> MapFns {
    MapFns { new: format!("__map_new_{id}"), find: format!("__map_find_{id}"), set: format!("__map_set_{id}"), del: format!("__map_del_{id}") }
}

struct Fb {
    vars: Vec<LVar>,
    labels: usize,
}

impl Fb {
    fn new() -> Self {
        Fb { vars: vec![], labels: 0 }
    }
    fn var(&mut self, name: &str, ty: LTy) -> V {
        self.vars.push(LVar { name: name.into(), ty });
        self.vars.len() - 1
    }
    fn label(&mut self) -> Label {
        self.labels += 1;
        self.labels - 1
    }
    fn func(self, name: &str, params: Vec<V>, ret: LTy, body: Vec<LS>) -> LFunc {
        LFunc { name: name.into(), params, vars: self.vars, ret, body, external: false, is_main: false, labels: self.labels }
    }
}

fn var(v: V) -> LE {
    LE::Var(v)
}
fn b<T>(x: T) -> Box<T> {
    Box::new(x)
}
fn cmp(op: Op, x: LE, y: LE) -> LE {
    LE::Cmp(op, b(x), b(y), LTy::I64)
}
fn add(x: LE, y: LE) -> LE {
    LE::Arith(Op::Add, b(x), b(y), Ovf::Unchecked)
}
fn and(x: LE, y: LE) -> LE {
    LE::Prim(Prim::And, vec![x, y])
}
fn idx(a: LE, i: LE) -> LE {
    LE::Index { arr: b(a), idx: b(i), check: None }
}
/// Field `f` of the header of map `m`.
fn hdr(m: V, f: usize) -> LE {
    LE::Field(b(idx(var(m), LE::I(0))), f)
}
fn set_field(m: V, f: usize, val: LE) -> LS {
    LS::SetPlace { var: m, steps: vec![Step::Index(LE::I(0), None), Step::Field(f)], val }
}
fn inc(i: V) -> LS {
    LS::Set(i, add(var(i), LE::I(1)))
}
/// `loop { if i >= n break; body; i += 1 }`
fn count_loop(fb: &mut Fb, i: V, n: LE, body: Vec<LS>) -> Vec<LS> {
    let l = fb.label();
    let mut inner = vec![LS::If(cmp(Op::Ge, var(i), n), vec![LS::Break(l)], vec![])];
    inner.extend(body);
    inner.push(inc(i));
    vec![LS::Set(i, LE::I(0)), LS::Loop(l, inner)]
}

fn keq(k: &LTy, x: LE, y: LE) -> LE {
    if *k == LTy::PInt {
        return LE::PArith(Op::Eq, b(x), b(y));
    }
    let t = match k {
        LTy::IntK(_) => LTy::I64,
        t => t.clone(),
    };
    LE::Cmp(Op::Eq, b(x), b(y), t)
}

/// A hash of key `x` into `h` (statements), then mixed.
fn hash(fb: &mut Fb, k: &LTy, x: LE, h: V) -> Vec<LS> {
    let mut out = vec![];
    match k {
        LTy::Str | LTy::PInt => {
            // FNV-1a over the bytes (a bignum: over its decimal digits).
            let s = fb.var("hs", LTy::Str);
            let i = fb.var("hi", LTy::I64);
            let n = fb.var("hn", LTy::I64);
            out.push(LS::Set(s, if *k == LTy::PInt { LE::Rt(Rt::PIntToS, vec![x]) } else { x }));
            out.push(LS::Set(n, LE::Rt(Rt::StrLen, vec![var(s)])));
            out.push(LS::Set(h, LE::I(0xcbf29ce484222325u64 as i64)));
            let byte = LE::Rt(Rt::StrByte, vec![var(s), var(i), LE::I(0)]);
            let step = LS::Set(h, LE::Arith(Op::Mul, b(LE::Prim(Prim::Xor, vec![var(h), byte])), b(LE::I(0x100000001b3)), Ovf::Wrap));
            out.extend(count_loop(fb, i, var(n), vec![step]));
        }
        LTy::Bool => out.push(LS::Set(h, LE::Cond(b(x), b(LE::I(1)), b(LE::I(2))))),
        _ => out.push(LS::Set(h, x)),
    }
    // A murmur3 finalizer: spreads every input bit over the low bits.
    let shr = |v: V, n: i64| LE::Prim(Prim::ShrU, vec![var(v), LE::I(n)]);
    let xs = |v: V, n: i64| LS::Set(v, LE::Prim(Prim::Xor, vec![var(v), shr(v, n)]));
    out.push(xs(h, 33));
    out.push(LS::Set(h, LE::Arith(Op::Mul, b(var(h)), b(LE::I(0xff51afd7ed558ccdu64 as i64)), Ovf::Wrap)));
    out.push(xs(h, 33));
    out.push(LS::Set(h, LE::Arith(Op::Mul, b(var(h)), b(LE::I(0xc4ceb9fe1a85ec53u64 as i64)), Ovf::Wrap)));
    out.push(xs(h, 33));
    out
}

/// `__map_new(cap) -> Map`: room for `cap` entries before growing.
fn gen_new(n: &MapFns, k: &LTy, v: &LTy) -> LFunc {
    let mut fb = Fb::new();
    let cap = fb.var("cap", LTy::I64);
    let size = fb.var("size", LTy::I64);
    let m = fb.var("m", map_ty(k, v));
    let l = fb.label();
    let mut body = vec![LS::Set(size, LE::I(8))];
    // Slots stay at most half full.
    body.push(LS::Loop(l, vec![LS::If(cmp(Op::Ge, var(size), add(var(cap), var(cap))), vec![LS::Break(l)], vec![]), LS::Set(size, add(var(size), var(size)))]));
    let h = LE::Tup(
        header(k, v),
        vec![
            LE::ArrWithCap(k.clone(), b(var(cap))),
            LE::ArrWithCap(v.clone(), b(var(cap))),
            LE::ArrWithCap(LTy::Bool, b(var(cap))),
            LE::ArrNew(LTy::I64, b(var(size)), b(LE::I(-1)), String::new()),
            LE::I(0),
            LE::RegionProgram,
            LE::I(0),
            LE::I(-1),
        ],
    );
    body.push(LS::Set(m, LE::ArrLit(header(k, v), vec![h])));
    body.push(LS::Return(Some(var(m))));
    fb.func(&n.new, vec![cap], map_ty(k, v), body)
}

/// `__map_find(m, key) -> I64`: the entry index, or -1.
fn gen_find(n: &MapFns, k: &LTy, v: &LTy) -> LFunc {
    let mut fb = Fb::new();
    let m = fb.var("m", map_ty(k, v));
    let key = fb.var("key", k.clone());
    let h = fb.var("h", LTy::I64);
    let mask = fb.var("mask", LTy::I64);
    let s = fb.var("s", LTy::I64);
    let slots = fb.var("slots", LTy::Arr(b(LTy::I64)));
    // The last entry found, if it's this key.
    let last = fb.var("last", LTy::I64);
    let mut body = vec![LS::Set(last, hdr(m, LAST))];
    let hit = LE::Cond(
        b(cmp(Op::Ge, var(last), LE::I(0))),
        b(LE::Cond(
            b(cmp(Op::Lt, var(last), LE::Len(b(hdr(m, KEYS))))),
            b(LE::Cond(b(idx(hdr(m, LIVE), var(last))), b(keq(k, idx(hdr(m, KEYS), var(last)), var(key))), b(LE::B(false)))),
            b(LE::B(false)),
        )),
        b(LE::B(false)),
    );
    body.push(LS::If(hit, vec![LS::Return(Some(var(last)))], vec![]));
    body.extend(hash(&mut fb, k, var(key), h));
    body.push(LS::Set(slots, hdr(m, SLOTS)));
    body.push(LS::Set(mask, add(LE::Len(b(var(slots))), LE::I(-1))));
    body.push(LS::Set(h, and(var(h), var(mask))));
    let l = fb.label();
    // Slots: an entry index, -1 empty (the chain ends), -2 a deleted entry's
    // tombstone (the chain goes on). Live entries are exactly those slots
    // point at, so no `live` load here.
    let found = LE::Cond(b(cmp(Op::Ge, var(s), LE::I(0))), b(keq(k, idx(hdr(m, KEYS), var(s)), var(key))), b(LE::B(false)));
    body.push(LS::Loop(
        l,
        vec![
            LS::Set(s, idx(var(slots), var(h))),
            LS::If(cmp(Op::Eq, var(s), LE::I(-1)), vec![LS::Return(Some(LE::I(-1)))], vec![]),
            LS::If(found, vec![set_field(m, LAST, var(s)), LS::Return(Some(var(s)))], vec![]),
            LS::Set(h, and(add(var(h), LE::I(1)), var(mask))),
        ],
    ));
    body.push(LS::Return(Some(LE::I(-1))));
    fb.func(&n.find, vec![m, key], LTy::I64, body)
}

/// Put entry `e` (whose key is `key`) into the first empty slot of its chain.
fn place_slot(fb: &mut Fb, k: &LTy, slots: V, key: LE, e: LE) -> Vec<LS> {
    let h = fb.var("ph", LTy::I64);
    let mask = fb.var("pmask", LTy::I64);
    let mut out = hash(fb, k, key, h);
    out.push(LS::Set(mask, add(LE::Len(b(var(slots))), LE::I(-1))));
    out.push(LS::Set(h, and(var(h), var(mask))));
    let l = fb.label();
    out.push(LS::Loop(
        l,
        vec![
            LS::If(cmp(Op::Lt, idx(var(slots), var(h)), LE::I(0)), vec![LS::Break(l)], vec![]),
            LS::Set(h, and(add(var(h), LE::I(1)), var(mask))),
        ],
    ));
    out.push(LS::SetIndex { arr: slots, idx: var(h), val: e, check: None });
    out
}

/// `__map_set(m, key, val)`.
fn gen_set(n: &MapFns, k: &LTy, v: &LTy) -> LFunc {
    let mut fb = Fb::new();
    let m = fb.var("m", map_ty(k, v));
    let key = fb.var("key", k.clone());
    let val = fb.var("val", v.clone());
    let e = fb.var("e", LTy::I64);
    let keys = fb.var("keys", LTy::Arr(b(k.clone())));
    let vals = fb.var("vals", LTy::Arr(b(v.clone())));
    let live = fb.var("live", LTy::Arr(b(LTy::Bool)));
    let slots = fb.var("slots", LTy::Arr(b(LTy::I64)));
    let mut body = vec![
        LS::Set(e, LE::Call(n.find.clone(), vec![var(m), var(key)])),
        LS::If(
            cmp(Op::Ge, var(e), LE::I(0)),
            vec![LS::SetPlace { var: m, steps: vec![Step::Index(LE::I(0), None), Step::Field(VALS), Step::Index(var(e), None)], val: var(val) }, LS::Return(None)],
            vec![],
        ),
    ];
    // Grow when the entries (dead ones too) would fill half the slots:
    // rebuild with only the live entries, in order.
    let grow = {
        let nk = fb.var("nk", LTy::Arr(b(k.clone())));
        let nv = fb.var("nv", LTy::Arr(b(v.clone())));
        let nl = fb.var("nl", LTy::Arr(b(LTy::Bool)));
        let ns = fb.var("ns", LTy::Arr(b(LTy::I64)));
        let size = fb.var("size", LTy::I64);
        let i = fb.var("i", LTy::I64);
        let cnt = hdr(m, COUNT);
        let mut g = vec![
            LS::Set(keys, hdr(m, KEYS)),
            LS::Set(vals, hdr(m, VALS)),
            LS::Set(live, hdr(m, LIVE)),
            LS::Set(size, LE::Len(b(hdr(m, SLOTS)))),
        ];
        // Double unless deletions alone freed enough room.
        g.push(LS::If(cmp(Op::Ge, add(add(cnt.clone(), cnt.clone()), add(cnt.clone(), cnt)), var(size)), vec![LS::Set(size, add(var(size), var(size)))], vec![]));
        g.push(LS::Set(nk, LE::ArrWithCap(k.clone(), b(var(size)))));
        g.push(LS::Set(nv, LE::ArrWithCap(v.clone(), b(var(size)))));
        g.push(LS::Set(nl, LE::ArrWithCap(LTy::Bool, b(var(size)))));
        g.push(LS::Set(ns, LE::ArrNew(LTy::I64, b(var(size)), b(LE::I(-1)), String::new())));
        let mut copy = vec![LS::Push(nk, idx(var(keys), var(i))), LS::Push(nv, idx(var(vals), var(i))), LS::Push(nl, LE::B(true))];
        copy.extend(place_slot(&mut fb, k, ns, idx(var(keys), var(i)), add(LE::Len(b(var(nk))), LE::I(-1))));
        let each = vec![LS::If(idx(var(live), var(i)), copy, vec![])];
        g.extend(count_loop(&mut fb, i, LE::Len(b(var(keys))), each));
        g.push(set_field(m, KEYS, var(nk)));
        g.push(set_field(m, VALS, var(nv)));
        g.push(set_field(m, LIVE, var(nl)));
        g.push(set_field(m, SLOTS, var(ns)));
        g
    };
    let used = LE::Len(b(hdr(m, KEYS)));
    body.push(LS::If(cmp(Op::Ge, add(add(used.clone(), used), LE::I(2)), LE::Len(b(hdr(m, SLOTS)))), grow, vec![]));
    body.push(LS::Set(keys, hdr(m, KEYS)));
    body.push(LS::Set(vals, hdr(m, VALS)));
    body.push(LS::Set(live, hdr(m, LIVE)));
    body.push(LS::Set(slots, hdr(m, SLOTS)));
    // A new Str key is copied into the map's storage: what the caller passed
    // can then die with the caller's iteration (strings are immutable, so
    // the copy can't be told apart).
    let stored_key = if *k == LTy::Str { LE::Rt(Rt::StrCat, vec![var(key)]) } else { var(key) };
    body.push(LS::Push(keys, stored_key));
    body.push(LS::Push(vals, var(val)));
    body.push(LS::Push(live, LE::B(true)));
    body.extend(place_slot(&mut fb, k, slots, var(key), add(LE::Len(b(var(keys))), LE::I(-1))));
    body.push(set_field(m, KEYS, var(keys)));
    body.push(set_field(m, VALS, var(vals)));
    body.push(set_field(m, LIVE, var(live)));
    body.push(set_field(m, COUNT, add(hdr(m, COUNT), LE::I(1))));
    body.push(LS::Return(None));
    fb.func(&n.set, vec![m, key, val], LTy::Unit, body)
}

/// `__map_del(m, key) -> I64`: the removed entry's index (its value stays
/// readable in `vals`), or -1.
fn gen_del(n: &MapFns, k: &LTy, v: &LTy) -> LFunc {
    let mut fb = Fb::new();
    let m = fb.var("m", map_ty(k, v));
    let key = fb.var("key", k.clone());
    let e = fb.var("e", LTy::I64);
    let h = fb.var("h", LTy::I64);
    let mask = fb.var("mask", LTy::I64);
    let slots = fb.var("slots", LTy::Arr(b(LTy::I64)));
    // The slot pointing at entry e becomes a tombstone (-2).
    let mut tomb = hash(&mut fb, k, var(key), h);
    tomb.push(LS::Set(slots, hdr(m, SLOTS)));
    tomb.push(LS::Set(mask, add(LE::Len(b(var(slots))), LE::I(-1))));
    tomb.push(LS::Set(h, and(var(h), var(mask))));
    let l = fb.label();
    tomb.push(LS::Loop(
        l,
        vec![
            LS::If(cmp(Op::Eq, idx(var(slots), var(h)), var(e)), vec![LS::SetIndex { arr: slots, idx: var(h), val: LE::I(-2), check: None }, LS::Break(l)], vec![]),
            LS::Set(h, and(add(var(h), LE::I(1)), var(mask))),
        ],
    ));
    let mut hit = vec![
        LS::SetPlace { var: m, steps: vec![Step::Index(LE::I(0), None), Step::Field(LIVE), Step::Index(var(e), None)], val: LE::B(false) },
        set_field(m, COUNT, add(hdr(m, COUNT), LE::I(-1))),
    ];
    hit.extend(tomb);
    let body = vec![
        LS::Set(e, LE::Call(n.find.clone(), vec![var(m), var(key)])),
        LS::If(cmp(Op::Ge, var(e), LE::I(0)), hit, vec![]),
        LS::Return(Some(var(e))),
    ];
    fb.func(&n.del, vec![m, key], LTy::I64, body)
}

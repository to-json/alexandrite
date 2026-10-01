//! Probe 01, hand-emitted. Two strategies for a value that escapes its scope.
//! Expected: both print sum = 2 * 2^DEPTH = 2048.

use alx_rt::{Id, Pool, Relocate, Rooted};

const DEPTH: u32 = 10;
/// Compile-time-known node count of `build(DEPTH)`: 2^(DEPTH+1) - 1.
const NODES: usize = (1 << (DEPTH + 1)) - 1;

enum Tree {
    Leaf(i64),
    Node(Id<Tree>, Id<Tree>),
}

impl Relocate for Tree {
    fn relocate(&self, f: &mut dyn FnMut(Id<Self>) -> Id<Self>) -> Self {
        match *self {
            Tree::Leaf(n) => Tree::Leaf(n),
            Tree::Node(l, r) => Tree::Node(f(l), f(r)),
        }
    }
}

fn sum(p: &Pool<Tree>, t: Id<Tree>) -> i64 {
    match *p.get(t) {
        Tree::Leaf(n) => n,
        Tree::Node(l, r) => sum(p, l) + sum(p, r),
    }
}

/// (a) Region inference, MLKit-style: every function that returns a fresh
/// value takes the caller's pool as an extra parameter. Pool parameters are
/// lifetimes with the names filed off.
mod caller_pool {
    use super::*;

    pub fn build(out: &mut Pool<Tree>, depth: u32) -> Id<Tree> {
        if depth == 0 {
            return out.put(Tree::Leaf(1));
        }
        let l = build(out, depth - 1);
        let r = build(out, depth - 1);
        out.put(Tree::Node(l, r))
    }

    // Reads from one region, writes into another: two region params.
    pub fn doubled(src: &Pool<Tree>, t: Id<Tree>, out: &mut Pool<Tree>) -> Id<Tree> {
        match *src.get(t) {
            Tree::Leaf(n) => out.put(Tree::Leaf(n * 2)),
            Tree::Node(l, r) => {
                let l = doubled(src, l, out);
                let r = doubled(src, r, out);
                out.put(Tree::Node(l, r))
            }
        }
    }

    pub fn main() -> i64 {
        let mut p_t = Pool::with_headroom(NODES);
        let t = build(&mut p_t, DEPTH);
        let mut p_d = Pool::with_headroom(NODES);
        let d = doubled(&p_t, t, &mut p_d);
        drop(p_t); // region of `t` ends at its last use
        sum(&p_d, d)
    }
}

/// (b) Copy-out at return: each function owns a private pool; the value
/// escapes by compacting into a fresh pool that moves to the caller.
/// Signatures carry no region params.
mod copy_out {
    use super::*;

    fn build_into(p: &mut Pool<Tree>, depth: u32) -> Id<Tree> {
        if depth == 0 {
            return p.put(Tree::Leaf(1));
        }
        let l = build_into(p, depth - 1);
        let r = build_into(p, depth - 1);
        p.put(Tree::Node(l, r))
    }

    pub fn build(depth: u32) -> Rooted<Tree> {
        let mut p = Pool::with_headroom(NODES);
        let root = build_into(&mut p, depth);
        Rooted { pool: p, root }.compact()
    }

    fn doubled_into(src: &Pool<Tree>, t: Id<Tree>, p: &mut Pool<Tree>) -> Id<Tree> {
        match *src.get(t) {
            Tree::Leaf(n) => p.put(Tree::Leaf(n * 2)),
            Tree::Node(l, r) => {
                let l = doubled_into(src, l, p);
                let r = doubled_into(src, r, p);
                p.put(Tree::Node(l, r))
            }
        }
    }

    pub fn doubled(t: &Rooted<Tree>) -> Rooted<Tree> {
        let mut p = Pool::with_headroom(NODES);
        let root = doubled_into(&t.pool, t.root, &mut p);
        Rooted { pool: p, root }.compact()
    }

    pub fn main() -> i64 {
        let t = build(DEPTH);
        let d = doubled(&t);
        drop(t);
        sum(&d.pool, d.root)
    }
}

fn main() {
    alx_rt::reset_counters();
    println!("(a) caller-pool  sum = {}", caller_pool::main());
    alx_rt::report("a");

    alx_rt::reset_counters();
    println!("(b) copy-out     sum = {}", copy_out::main());
    alx_rt::report("b");

    println!("nodes per tree = {NODES}");
}
